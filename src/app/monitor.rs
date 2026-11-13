//! Owns the daemon's process registry and publishes only committed observations.

use anyhow::Result;

use super::App;
use crate::domain::process::{ProcessIdentity, ProcessRoot};
use crate::procinfo;
use crate::runtime::registry::Registry;
use crate::sample::Sampler;
use crate::store;
use std::collections::HashMap;
use std::time::Duration;
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const SETTLE_AFTER: Duration = Duration::from_secs(30);
const SETTLE_EVERY: u64 = 5;

pub fn initialize(state: &App) -> Result<Registry> {
    #[cfg(target_os = "linux")]
    {
        let boot_id = procinfo::boot_id()?;
        state.store().register_boot(&boot_id)?;
    }
    let mut registry = Registry::restore(state.store().process_members()?);
    observe(state, &mut registry)?;
    Ok(registry)
}

pub fn observe(state: &App, registry: &mut Registry) -> Result<()> {
    observe_with(state, registry, procinfo::snapshot)
}

fn observe_with(
    state: &App,
    registry: &mut Registry,
    read: impl FnOnce() -> std::io::Result<crate::domain::process::ProcessTable>,
) -> Result<()> {
    let result = (|| {
        let roots = state
            .store()
            .active_jobs()?
            .into_iter()
            .filter(|job| job.state.held())
            .map(|job| ProcessRoot {
                id: job.id,
                pgid: job.pgid.map(|g| g as i32),
                leader: job
                    .pgid
                    .zip(job.pgid_start)
                    .map(|(pid, start)| ProcessIdentity {
                        pid: pid as i32,
                        start,
                    }),
                launched: job.launched.map(|(pid, start)| ProcessIdentity {
                    pid: pid as i32,
                    start,
                }),
                executor: job.executor.map(|(pid, start)| ProcessIdentity {
                    pid: pid as i32,
                    start,
                }),
            })
            .collect();
        let observation = registry.observe(roots, read()?);
        state.store().commit_process_members(&observation.members)?;
        registry.accept(&observation);
        *state.observations.lock().expect("observations lock") = Some(observation);
        Ok(())
    })();
    if result.is_err() {
        *state.observations.lock().expect("observations lock") = None;
    }
    result
}

pub(crate) fn sample_loop(state: &App, mut registry: crate::runtime::registry::Registry) {
    let mut sampler = Sampler::default();
    let mut response = super::pressure::Response::default();
    let mut recorded: HashMap<String, u64> = HashMap::new();
    let mut ticks = 0u64;
    while !state.stopping() {
        if let Err(e) = observe(state, &mut registry) {
            tracing::error!("observing job processes: {e:#}");
            std::thread::sleep(SAMPLE_EVERY);
            continue;
        }
        let observation = state
            .observations
            .lock()
            .expect("observations lock")
            .clone()
            .expect("observation just committed");
        let snapshot = sampler.sample(&observation);
        let grown: Vec<(String, u64)> = snapshot
            .jobs
            .iter()
            .filter(|(job, u)| u.peak_private_total > recorded.get(*job).copied().unwrap_or(0))
            .map(|(job, u)| (job.clone(), u.peak_private_total))
            .collect();
        if !grown.is_empty() {
            match state.store().record_peaks(&grown) {
                Ok(()) => recorded.extend(grown),
                Err(e) => tracing::error!("recording peaks: {e:#}"),
            }
        }
        recorded.retain(|job, _| snapshot.jobs.contains_key(job));
        if ticks.is_multiple_of(SETTLE_EVERY) && snapshot.jobs.values().all(|u| u.complete) {
            let before =
                humantime::format_rfc3339_millis(std::time::SystemTime::now() - SETTLE_AFTER)
                    .to_string();
            match state.store().settle_reservations(&before) {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!(jobs = n, "reservations lowered to measured use");
                    state.notify();
                }
                Err(e) => tracing::error!("settling reservations: {e:#}"),
            }
        }
        ticks += 1;
        if let Err(e) = response.update(state, &snapshot, &observation) {
            tracing::error!("updating pressure response: {e:#}");
        }
        *state.measurements.lock().expect("measurements lock") = snapshot;
        std::thread::sleep(SAMPLE_EVERY);
    }
}

pub(crate) fn prune(state: &App) -> Result<()> {
    let before =
        humantime::format_rfc3339_millis(std::time::SystemTime::now() - store::KEEP).to_string();
    let pruned = state.store().prune(&before)?;
    for (job, log) in &pruned {
        if let Some(dir) = log
            .as_deref()
            .map(std::path::Path::new)
            .filter(|d| d.file_name().is_some_and(|n| n == job.as_str()))
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    if !pruned.is_empty() {
        tracing::info!(jobs = pruned.len(), "pruned old history");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::job::JobState;
    use crate::domain::job::{Entry, NewSubmission, Plan};
    use crate::domain::process::{Process, ProcessTable};
    use crate::store::Store;

    #[test]
    fn recovery_preserves_a_concurrent_start_and_its_committed_members() {
        use super::super::recovery::{self, Processes};
        use crate::domain::job::ActiveJob;
        use std::cell::{Cell, RefCell};

        struct StartBeforeProbe<'a> {
            app: &'a App,
            job: String,
            pinned: recovery::System,
            registry: RefCell<Registry>,
            injected: Cell<bool>,
        }
        impl Processes for StartBeforeProbe<'_> {
            fn ready(&self, job: &ActiveJob) -> bool {
                self.pinned.ready(job)
            }
            fn alive(&self, _: i64, _: &str) -> bool {
                assert!(!self.injected.replace(true));
                self.app
                    .store()
                    .record_start(&self.job, "/logs", Some(20), Some("worker"))
                    .unwrap();
                observe_with(self.app, &mut self.registry.borrow_mut(), || {
                    Ok(ProcessTable {
                        processes: vec![Process {
                            pid: 20,
                            ppid: 1,
                            pgid: 20,
                            session: None,
                            start: "worker".into(),
                        }],
                        ..Default::default()
                    })
                })
                .unwrap();
                assert_eq!(
                    self.app.store().process_members().unwrap()[&self.job].len(),
                    1
                );
                false
            }
            fn members(&self, job: &ActiveJob) -> &[ProcessIdentity] {
                self.pinned.members(job)
            }
            fn terminate(&self, _: &ActiveJob, _: bool) {
                panic!("the job was not canceled");
            }
        }

        let app = App::new(
            Store::open_in_memory().unwrap(),
            "test".into(),
            "instance".into(),
        );
        let job = app
            .store()
            .create_submission(&NewSubmission {
                entry: Entry::Sh,
                command: "make".into(),
                cwd: "/work".into(),
                project_root: "/work".into(),
                session_id: None,
                launcher: None,
                plan: Plan::default(),
            })
            .unwrap()
            .job;
        app.register_executor(&job, i64::from(std::process::id()), "old-executor")
            .unwrap();
        app.test_admit(&job, 100, 1);
        let mut registry = Registry::restore(Default::default());
        observe_with(&app, &mut registry, || Ok(ProcessTable::default())).unwrap();
        let procs = StartBeforeProbe {
            app: &app,
            job: job.clone(),
            pinned: recovery::System::new(&app),
            registry: RefCell::new(registry),
            injected: Cell::new(false),
        };
        let mut terminating = HashMap::new();
        recovery::sweep(&app, &procs, &mut terminating).unwrap();
        assert_eq!(app.job(&job).unwrap().state, JobState::Running);
        assert_eq!(app.store().process_members().unwrap()[&job].len(), 1);

        recovery::sweep(&app, &recovery::System::new(&app), &mut terminating).unwrap();
        assert_eq!(app.job(&job).unwrap().state, JobState::AwaitingTakeover);
        observe_with(&app, &mut procs.registry.borrow_mut(), || {
            Ok(ProcessTable::default())
        })
        .unwrap();
        recovery::sweep(&app, &recovery::System::new(&app), &mut terminating).unwrap();
        assert_eq!(app.job(&job).unwrap().state, JobState::EndedUnknown);
        assert!(app.store().process_members().unwrap().is_empty());
    }

    #[test]
    fn observation_failure_during_recovery_cannot_release_confirmed_members() {
        use super::super::recovery::{self, Processes};

        struct FailAfterReady<'a> {
            app: &'a App,
            system: recovery::System,
            registry: std::cell::RefCell<Registry>,
        }
        impl Processes for FailAfterReady<'_> {
            fn ready(&self, job: &crate::domain::job::ActiveJob) -> bool {
                let ready = self.system.ready(job);
                assert!(ready);
                assert!(
                    observe_with(self.app, &mut self.registry.borrow_mut(), || {
                        Err(std::io::Error::other("enumeration failed"))
                    })
                    .is_err()
                );
                ready
            }
            fn alive(&self, pid: i64, start: &str) -> bool {
                self.system.alive(pid, start)
            }
            fn members(&self, job: &crate::domain::job::ActiveJob) -> &[ProcessIdentity] {
                self.system.members(job)
            }
            fn terminate(&self, _: &crate::domain::job::ActiveJob, _: bool) {
                panic!("the job was not canceled");
            }
        }

        let app = App::new(
            Store::open_in_memory().unwrap(),
            "test".into(),
            "instance".into(),
        );
        let job = app
            .store()
            .create_submission(&NewSubmission {
                entry: Entry::Sh,
                command: "make".into(),
                cwd: "/work".into(),
                project_root: "/work".into(),
                session_id: None,
                launcher: None,
                plan: Plan::default(),
            })
            .unwrap()
            .job;
        app.register_executor(&job, i64::from(std::process::id()), "old-executor")
            .unwrap();
        app.test_admit(&job, 100, 1);
        app.store()
            .record_start(&job, "/logs", Some(20), Some("worker"))
            .unwrap();
        app.main_result(&job, 0).unwrap();
        let mut registry = Registry::restore(Default::default());
        observe_with(&app, &mut registry, || {
            Ok(ProcessTable {
                processes: vec![Process {
                    pid: 20,
                    ppid: 1,
                    pgid: 20,
                    session: None,
                    start: "worker".into(),
                }],
                ..Default::default()
            })
        })
        .unwrap();
        let saved = app.store().process_members().unwrap();
        let procs = FailAfterReady {
            app: &app,
            system: recovery::System::new(&app),
            registry: std::cell::RefCell::new(registry),
        };
        let mut terminating = HashMap::new();
        recovery::sweep(&app, &procs, &mut terminating).unwrap();
        assert_eq!(app.job(&job).unwrap().state, JobState::AwaitingTakeover);
        assert_eq!(app.store().process_members().unwrap(), saved);

        recovery::sweep(&app, &recovery::System::new(&app), &mut terminating).unwrap();
        assert_eq!(app.job(&job).unwrap().state, JobState::AwaitingTakeover);
        assert_eq!(app.store().process_members().unwrap(), saved);

        observe_with(&app, &mut procs.registry.borrow_mut(), || {
            Ok(ProcessTable::default())
        })
        .unwrap();
        recovery::sweep(&app, &recovery::System::new(&app), &mut terminating).unwrap();
        assert_eq!(app.job(&job).unwrap().state, JobState::Completed);
        assert!(app.store().process_members().unwrap().is_empty());
    }

    #[test]
    fn enumeration_failure_preserves_durable_members_and_blocks_recovery() {
        let app = App::new(
            Store::open_in_memory().unwrap(),
            "test".into(),
            "instance".into(),
        );
        let job = app
            .store()
            .create_submission(&NewSubmission {
                entry: Entry::Sh,
                command: "make".into(),
                cwd: "/work".into(),
                project_root: "/work".into(),
                session_id: None,
                launcher: None,
                plan: Plan::default(),
            })
            .unwrap()
            .job;
        app.register_executor(&job, 10, "executor").unwrap();
        app.test_admit(&job, 100, 1);
        app.store()
            .record_start(&job, "/logs", Some(20), Some("worker"))
            .unwrap();
        let mut registry = Registry::restore(Default::default());
        let table = |pgid| ProcessTable {
            processes: vec![Process {
                pid: 20,
                ppid: 1,
                pgid,
                session: None,
                start: "worker".into(),
            }],
            ..Default::default()
        };
        observe_with(&app, &mut registry, || Ok(table(20))).unwrap();
        let saved = app.store().process_members().unwrap();
        assert_eq!(saved[&job].len(), 1);
        assert!(
            observe_with(&app, &mut registry, || Err(std::io::Error::other(
                "enumeration failed"
            )))
            .is_err()
        );
        assert!(app.observations.lock().unwrap().is_none());
        assert!(!app.admitting());
        assert_eq!(app.store().process_members().unwrap(), saved);
        let procs = super::super::recovery::System::new(&app);
        super::super::recovery::sweep(&app, &procs, &mut HashMap::new()).unwrap();
        assert_eq!(app.job(&job).unwrap().state, JobState::Running);
        observe_with(&app, &mut registry, || Ok(table(21))).unwrap();
        assert_eq!(app.store().process_members().unwrap(), saved);
    }
}
