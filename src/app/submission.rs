use super::App;
use crate::domain::error::{Conflict, InvalidInput};
use crate::domain::job::{Created, Entry, NewSubmission, Plan, SubmitCommand};
use crate::planner::predict::{Lookup, Stats};
use crate::planner::{self, locks};
use crate::recognize::Recognizer;
use crate::redact;
use crate::store::Store;
use anyhow::Result;
use std::path::Path;

impl App {
    pub fn submit(&self, req: SubmitCommand) -> Result<Created> {
        let command = match req.entry {
            Entry::Argv if req.argv.is_empty() => {
                return Err(InvalidInput("argv entry needs argv".into()).into());
            }
            Entry::Argv => redact::command_words(&req.argv).join(" "),
            Entry::Sh => redact::shell_source(
                req.source
                    .as_deref()
                    .ok_or_else(|| InvalidInput("sh entry needs source".into()))?,
            ),
        };
        let cwd = Path::new(&req.cwd);
        let project = locks::project_root(cwd).to_string_lossy().into_owned();
        let recognizer = Recognizer::new(&self.profiles, &self.wrappers);
        let analysis = match req.entry {
            Entry::Argv => recognizer.argv(&req.argv, cwd),
            Entry::Sh => recognizer.shell(req.source.as_deref().unwrap_or_default(), cwd),
        };
        let history = |lookup: Lookup| {
            let found = match lookup {
                Lookup::Line => self.store().history(Some(&project), Some(&command), None),
                Lookup::ProjectKey(key) => self.store().history(Some(&project), None, Some(key)),
                Lookup::GlobalKey(key) => self.store().history(None, None, Some(key)),
            };
            found
                .map(|samples| Stats::from_history(&samples))
                .unwrap_or_else(|e| {
                    tracing::warn!("reading history: {e:#}");
                    Stats::default()
                })
        };
        let mut plan = planner::plan(
            &analysis,
            &self.profiles,
            cwd,
            &req.locks,
            req.hint.as_deref(),
            &req.cargo_env,
            &history,
        )?;
        let mut store = self.store();
        for id in req.after {
            plan.after.push(
                store.resolve_job(&id)?.ok_or_else(|| {
                    InvalidInput(format!("--after {id}: no such job or submission"))
                })?,
            );
        }
        plan.run_now = req.now;
        if req.now {
            refuse_now_conflicts(&store, &plan)?;
        }
        let created = store.create_submission(&NewSubmission {
            entry: req.entry,
            command,
            cwd: req.cwd,
            project_root: project,
            session_id: req.session_id,
            launcher: req.launcher,
            plan,
        })?;
        drop(store);
        self.notify();
        Ok(created)
    }
}

fn refuse_now_conflicts(store: &Store, plan: &Plan) -> anyhow::Result<()> {
    let mut conflicts = Vec::new();
    for job in &plan.after {
        let row = store.job(job)?.expect("resolved above");
        if !(row.state == crate::domain::job::JobState::Completed && row.exit_code == Some(0)) {
            conflicts.push(format!("predecessor {job} is {}", row.state));
        }
    }
    for held in store.held_jobs()? {
        for lock in &plan.locks {
            if let Some(h) = held.locks.iter().find(|h| h.conflicts(lock)) {
                conflicts.push(format!("lock {h} is held by {}", held.id));
            }
        }
    }
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(Conflict(format!(
        "--now cannot run it: {}. Submit without --now to queue it, or retry later.",
        conflicts.join("; ")
    ))
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(cwd: &str) -> SubmitCommand {
        SubmitCommand {
            entry: Entry::Sh,
            argv: vec![],
            source: Some("make".into()),
            cwd: cwd.into(),
            session_id: None,
            launcher: None,
            after: vec![],
            now: false,
            locks: vec!["shared".into()],
            hint: None,
            cargo_env: Default::default(),
        }
    }

    #[test]
    fn submission_and_conflict_rules_work_without_http() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let app = App::new(
            Store::open_in_memory().unwrap(),
            "test".into(),
            "instance".into(),
        );
        let mut missing = command(cwd);
        missing.after.push("j_missing".into());
        assert!(
            app.submit(missing)
                .unwrap_err()
                .downcast_ref::<InvalidInput>()
                .is_some()
        );
        assert!(app.list_jobs(&[], 10).unwrap().is_empty());
        let first = app.submit(command(cwd)).unwrap();
        app.register_executor(&first.job, 10, "executor").unwrap();
        app.test_admit(&first.job, 100, 1);
        let mut now = command(cwd);
        now.now = true;
        assert!(
            app.submit(now)
                .unwrap_err()
                .downcast_ref::<Conflict>()
                .is_some()
        );
        assert_eq!(app.list_jobs(&[], 10).unwrap().len(), 1);
    }

    #[test]
    fn configured_cargo_outputs_are_mutually_exclusive() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        let app = App::new(
            Store::open_in_memory().unwrap(),
            "test".into(),
            "instance".into(),
        );
        let make_request = |name: &str| {
            let cwd = root.path().join(name);
            std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
            std::fs::write(cwd.join("Cargo.toml"), "").unwrap();
            std::fs::write(
                cwd.join(".cargo/config.toml"),
                format!("[build]\ntarget-dir = {:?}\n", shared.to_str().unwrap()),
            )
            .unwrap();
            let mut req = command(cwd.to_str().unwrap());
            req.source = Some("cargo build".into());
            req.locks.clear();
            req
        };
        let first = app.submit(make_request("first")).unwrap();
        app.register_executor(&first.job, 10, "executor").unwrap();
        app.test_admit(&first.job, 100, 1);
        let mut second = make_request("second");
        second.now = true;
        let error = app.submit(second).unwrap_err();
        assert!(error.downcast_ref::<Conflict>().is_some());
        assert!(error.to_string().contains(&locks::normalize(&shared)));
    }
}
