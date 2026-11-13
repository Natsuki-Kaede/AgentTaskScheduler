//! Lifecycle operations and bounded waits, independent of HTTP.

use super::App;
use crate::domain::error::{Conflict, InvalidInput};
use crate::domain::job::{FinishReport, JobEvent, JobState, SubmissionRecord, WaitUntil};
use crate::domain::resource::LeaseTerms;
use crate::timing::Deadline;
use anyhow::Result;
use std::time::Duration;

const LEASE_POLL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(3)
};

impl App {
    pub fn register_executor(&self, id: &str, pid: i64, start: &str) -> Result<()> {
        self.store().set_executor(id, pid, start)?;
        self.notify();
        Ok(())
    }
    pub fn started(&self, id: &str, log_path: &str, pgid: Option<i64>) -> Result<()> {
        let executor = self.store().recovery_facts(id)?.executor;
        let leader = pgid
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .and_then(|pid| crate::procinfo::probe(pid).ok().flatten())
            .filter(|leader| {
                leader.pid == leader.pgid
                    && executor.as_ref().is_some_and(|(pid, start)| {
                        i64::from(leader.ppid) == *pid && crate::procinfo::alive(leader.ppid, start)
                    })
            });
        self.store().record_start(
            id,
            log_path,
            pgid,
            leader.as_ref().map(|p| p.start.as_str()),
        )?;
        self.notify();
        Ok(())
    }
    pub fn launch(&self, id: &str, pid: i64, start: &str) -> Result<()> {
        let facts = self.store().recovery_facts(id)?;
        let identity = crate::domain::process::ProcessIdentity {
            pid: i32::try_from(pid)
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or_else(|| InvalidInput("invalid launch PID".into()))?,
            start: start.to_owned(),
        };
        let replay = facts
            .launched
            .as_ref()
            .is_some_and(|(prior, prior_start)| *prior == pid && prior_start == start);
        if !replay {
            let verified = crate::procinfo::probe(identity.pid)?.is_some_and(|child| {
                child.start == start
                    && facts.pgid == Some(i64::from(child.pgid))
                    && facts.executor.as_ref().is_some_and(|(parent, start)| {
                        *parent == i64::from(child.ppid)
                            && crate::procinfo::alive(child.ppid, start)
                    })
            });
            if !verified {
                return Err(Conflict(format!("job {id}: cannot confirm launch process")).into());
            }
        }
        self.store().record_launch(&facts, &identity)?;
        self.notify();
        Ok(())
    }
    pub fn main_result(&self, id: &str, code: i64) -> Result<()> {
        self.store().set_main_result(id, code)?;
        self.notify();
        Ok(())
    }
    pub fn finished(
        &self,
        id: &str,
        state: JobState,
        code: Option<i64>,
        max_rss: Option<i64>,
    ) -> Result<()> {
        if !state.terminal() {
            return Err(InvalidInput(format!("{state} is not a terminal state")).into());
        }
        if max_rss.is_some_and(|rss| rss < 0) {
            return Err(InvalidInput("max_rss must be nonnegative".into()).into());
        }
        self.store().report_finish(
            id,
            &FinishReport {
                state,
                exit_code: code,
                max_rss,
            },
        )?;
        self.settle_finished(id, crate::procinfo::probe)?;
        self.notify();
        Ok(())
    }

    fn settle_finished(
        &self,
        id: &str,
        mut probe: impl FnMut(i32) -> std::io::Result<Option<crate::domain::process::Process>>,
    ) -> Result<()> {
        let (job, members) = {
            let store = self.store();
            (store.recovery_facts(id)?, store.members_of(id)?)
        };
        let mut gone =
            |pid, start: &str| probe(pid).is_ok_and(|p| p.is_none_or(|p| p.start != start));
        if !job.state.terminal()
            && job
                .launched
                .as_ref()
                .is_none_or(|(pid, start)| gone(*pid as i32, start))
            && members
                .iter()
                .all(|identity| gone(identity.pid, &identity.start))
        {
            let report = job.finish.as_ref().expect("completion just recorded");
            let state = if job.canceled {
                JobState::Canceled
            } else {
                report.state
            };
            match self
                .store()
                .recover(&job, &members, JobEvent::Finish(state), report.exit_code)
            {
                Ok(()) => {}
                Err(e) if e.downcast_ref::<Conflict>().is_some() => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    pub fn cancel(&self, id: &str) -> Result<()> {
        self.store().cancel_submission(id)?;
        self.notify();
        Ok(())
    }
    pub fn detach(&self, id: &str) -> Result<()> {
        self.store().detach_submission(id)?;
        self.notify();
        Ok(())
    }
    pub fn quiet(&self, id: &str) -> Result<()> {
        self.store().quiet_submission(id)?;
        self.notify();
        Ok(())
    }
    pub fn unblock(&self, id: &str) -> Result<()> {
        self.store().unblock(id)?;
        self.notify();
        Ok(())
    }

    pub fn control(&self, id: &str) -> Result<bool> {
        self.contact(id);
        self.store().cancel_requested(id)
    }

    pub async fn lease(&self, id: &str, pid: i64) -> Result<Option<LeaseTerms>> {
        self.contact(id);
        let deadline = Deadline::after(Some(LEASE_POLL));
        let mut changes = self.changes.subscribe();
        loop {
            let row = self.job(id)?;
            if row.executor_pid != Some(pid) {
                return Err(
                    Conflict(format!("process {pid} is not the executor of job {id}")).into(),
                );
            }
            if row.state == JobState::Admitted {
                return Ok(Some(self.store().lease_terms(id)?));
            }
            if !row.state.pre_lease() {
                return Err(Conflict(format!("job {id} is {}", row.state)).into());
            }
            if deadline.expired() || self.stopping() {
                return Ok(None);
            }
            let pause = deadline
                .remaining()
                .unwrap_or_default()
                .min(Duration::from_secs(1));
            let _ = tokio::time::timeout(pause, changes.changed()).await;
        }
    }

    pub async fn wait(
        &self,
        id: &str,
        seconds: Option<f64>,
        until: WaitUntil,
    ) -> Result<(bool, SubmissionRecord)> {
        let duration = seconds
            .map(Duration::try_from_secs_f64)
            .transpose()
            .map_err(|_| {
                InvalidInput("wait_seconds must be a finite nonnegative duration".into())
            })?;
        let deadline = Deadline::after(duration);
        let mut changes = self.changes.subscribe();
        let job = self.submission(id)?.job;
        let _awaiting = self.awaiting(&job);
        loop {
            let row = self.submission(id)?;
            let reached = row.job_state.terminal()
                || matches!(
                    row.job_state,
                    JobState::Blocked | JobState::CapacityInsufficient
                )
                || (until == WaitUntil::Main && row.main_exit_code.is_some());
            if reached || deadline.expired() || self.stopping() {
                return Ok((reached, row));
            }
            let pause = deadline
                .remaining()
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(1));
            let _ = tokio::time::timeout(pause, changes.changed()).await;
        }
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use crate::domain::job::{Entry, NewSubmission, Plan};
    use crate::domain::process::{Process, ProcessIdentity};
    use crate::store::Store;

    #[test]
    fn completion_retains_unknown_or_concurrently_changed_members() {
        for reading in ["alive", "unknown", "gone", "reused", "new_member"] {
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
            app.started(&job, "/logs", Some(20)).unwrap();
            let original = ProcessIdentity {
                pid: 20,
                start: "worker".into(),
            };
            app.store()
                .commit_process_members(&[(job.clone(), vec![original.clone()])].into())
                .unwrap();
            app.store()
                .report_finish(
                    &job,
                    &FinishReport {
                        state: JobState::Completed,
                        exit_code: Some(7),
                        max_rss: Some(900),
                    },
                )
                .unwrap();
            app.settle_finished(&job, |_| match reading {
                "unknown" => Err(std::io::Error::other("unreadable worker")),
                "alive" | "reused" => Ok(Some(Process {
                    pid: 20,
                    ppid: 1,
                    pgid: 20,
                    session: None,
                    start: if reading == "alive" {
                        "worker"
                    } else {
                        "replacement"
                    }
                    .into(),
                })),
                "new_member" => {
                    app.store()
                        .commit_process_members(
                            &[(
                                job.clone(),
                                vec![
                                    original.clone(),
                                    ProcessIdentity {
                                        pid: 21,
                                        start: "new child".into(),
                                    },
                                ],
                            )]
                            .into(),
                        )
                        .unwrap();
                    Ok(None)
                }
                _ => Ok(None),
            })
            .unwrap();
            let row = app.job(&job).unwrap();
            if matches!(reading, "gone" | "reused") {
                assert_eq!(
                    (row.state, row.exit_code, row.peak_private),
                    (JobState::Completed, Some(7), Some(900)),
                    "{reading}"
                );
                assert!(app.store().members_of(&job).unwrap().is_empty());
            } else {
                assert_eq!(row.state, JobState::Running, "{reading}");
                assert!(app.store().recovery_facts(&job).unwrap().finish.is_some());
                assert!(!app.store().members_of(&job).unwrap().is_empty());
                app.settle_finished(&job, |_| Ok(None)).unwrap();
                assert_eq!(app.job(&job).unwrap().state, JobState::Completed);
            }
        }
    }
}
