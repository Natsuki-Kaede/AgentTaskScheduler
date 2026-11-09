//! Lifecycle guards and payload checks share the transaction with each update.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use super::{Conflict, Store, no_job, now};
use crate::domain::job::{ActiveJob, FinishReport, JobEvent, JobState};
use crate::domain::process::ProcessIdentity;

struct Current {
    state: JobState,
    executor: Option<(i64, String)>,
    pgid: Option<i64>,
    log_path: Option<String>,
    main: Option<i64>,
    exit: Option<i64>,
    finish: Option<FinishReport>,
}

pub(super) fn finish_row(r: &rusqlite::Row, at: usize) -> rusqlite::Result<Option<FinishReport>> {
    let state: Option<JobState> = r.get(at)?;
    match state {
        Some(state) => Ok(Some(FinishReport {
            state,
            exit_code: r.get(at + 1)?,
            max_rss: r.get(at + 2)?,
        })),
        None => Ok(None),
    }
}

fn current(conn: &rusqlite::Connection, job: &str) -> Result<Current> {
    conn.query_row(
        "SELECT state, executor_pid, executor_start, pgid, log_path, main_exit_code, exit_code,
         finish_state, finish_exit_code, finish_max_rss FROM jobs WHERE id = ?1",
        params![job],
        |r| {
            Ok(Current {
                state: r.get(0)?,
                executor: super::process(r.get(1)?, r.get(2)?),
                pgid: r.get(3)?,
                log_path: r.get(4)?,
                main: r.get(5)?,
                exit: r.get(6)?,
                finish: finish_row(r, 7)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| no_job(job))
}

enum Report<'a> {
    Executor(i64, &'a str),
    Started(&'a str, Option<i64>),
    Main(i64),
    Finished(JobState, Option<i64>, Option<i64>),
    Lost,
}

impl Store {
    fn apply_report(
        &mut self,
        job: &str,
        report: Report<'_>,
        expected: Option<(&ActiveJob, &[ProcessIdentity])>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        if let Some((expected, members)) = expected {
            let actual = tx
                .query_row(
                    &format!("SELECT {} FROM jobs WHERE id = ?1", super::RECOVERY_COLUMNS),
                    params![job],
                    super::recovery_row,
                )
                .optional()?;
            if actual.as_ref() != Some(expected) {
                return Err(Conflict(format!("job {job}: recovery facts changed")).into());
            }
            let actual: std::collections::BTreeSet<ProcessIdentity> = tx
                .prepare("SELECT pid, start FROM job_processes WHERE job_id = ?1")?
                .query_map(params![job], |r| {
                    Ok(ProcessIdentity {
                        pid: r.get(0)?,
                        start: r.get(1)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            if actual != members.iter().cloned().collect() {
                return Err(Conflict(format!("job {job}: recovery membership changed")).into());
            }
        }
        let current = current(&tx, job)?;
        let conflict = |message: &str| Conflict(format!("job {job}: {message}"));
        let event = match report {
            Report::Executor(pid, start) => {
                if current.state.terminal() {
                    return Err(conflict("has already ended").into());
                }
                if let Some((p, s)) = &current.executor {
                    return if *p == pid && s == start {
                        Ok(())
                    } else {
                        Err(conflict("already has another executor").into())
                    };
                }
                if current.finish.is_some() {
                    return Err(conflict("completion has already been reported").into());
                }
                JobEvent::RegisterExecutor
            }
            Report::Started(log, pgid) => {
                if current.state == JobState::Running {
                    return if current.log_path.as_deref() == Some(log) && current.pgid == pgid {
                        Ok(())
                    } else {
                        Err(conflict("conflicting start report").into())
                    };
                }
                if current.finish.is_some() {
                    return Err(conflict("completion has already been reported").into());
                }
                JobEvent::Started
            }
            Report::Main(code) => {
                if let Some(previous) = current.main {
                    return if previous == code {
                        Ok(())
                    } else {
                        Err(conflict("conflicting main result").into())
                    };
                }
                JobEvent::MainResult
            }
            Report::Finished(state, code, _) => {
                if current.state.terminal() {
                    return if current.state == state && current.exit == code {
                        Ok(())
                    } else {
                        Err(conflict("conflicting terminal report").into())
                    };
                }
                JobEvent::Finish(state)
            }
            Report::Lost if current.state == JobState::AwaitingTakeover => return Ok(()),
            Report::Lost => JobEvent::ExecutorLost,
        };
        let next = current
            .state
            .transition(event)
            .map_err(|e| Conflict(format!("job {job}: {e}")))?;
        let at = now();
        let (kind, detail) = match report {
            Report::Executor(pid, start) => {
                tx.execute(
                    "UPDATE jobs SET executor_pid = ?2, executor_start = ?3 WHERE id = ?1",
                    params![job, pid, start],
                )?;
                ("executor", Some(pid.to_string()))
            }
            Report::Started(log, pgid) => {
                tx.execute("UPDATE jobs SET state = ?2, started_at = ?3, log_path = ?4, pgid = ?5 WHERE id = ?1", params![job, next, at, log, pgid])?;
                ("started", None)
            }
            Report::Main(code) => {
                tx.execute(
                    "UPDATE jobs SET main_exit_code = ?2 WHERE id = ?1",
                    params![job, code],
                )?;
                ("main_exited", Some(code.to_string()))
            }
            Report::Finished(state, code, max_rss) => {
                tx.execute("UPDATE jobs SET state = ?2, exit_code = ?3, ended_at = ?4, peak_private = COALESCE(peak_private, ?5) WHERE id = ?1", params![job, state, code, at, max_rss])?;
                tx.execute("DELETE FROM job_processes WHERE job_id = ?1", params![job])?;
                (state.as_str(), code.map(|c| c.to_string()))
            }
            Report::Lost => {
                tx.execute(
                    "UPDATE jobs SET state = ?2 WHERE id = ?1",
                    params![job, next],
                )?;
                ("executor_lost", None)
            }
        };
        Self::event(&tx, job, kind, detail.as_deref())?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_executor(&mut self, job: &str, pid: i64, start: &str) -> Result<()> {
        self.apply_report(job, Report::Executor(pid, start), None)
    }
    pub fn set_started(&mut self, job: &str, log_path: &str, pgid: Option<i64>) -> Result<()> {
        self.apply_report(job, Report::Started(log_path, pgid), None)
    }
    pub fn set_main_result(&mut self, job: &str, code: i64) -> Result<()> {
        self.apply_report(job, Report::Main(code), None)
    }
    #[cfg(test)]
    pub fn finish(&mut self, job: &str, state: JobState, exit_code: Option<i64>) -> Result<()> {
        self.finish_with_usage(job, state, exit_code, None)
    }
    #[cfg(test)]
    pub fn finish_with_usage(
        &mut self,
        job: &str,
        state: JobState,
        exit_code: Option<i64>,
        max_rss: Option<i64>,
    ) -> Result<()> {
        self.apply_report(job, Report::Finished(state, exit_code, max_rss), None)
    }

    /// Accept a replay-safe report without claiming that all owned work has exited.
    pub fn report_finish(&mut self, job: &str, report: &FinishReport) -> Result<()> {
        let tx = self.conn.transaction()?;
        let current = current(&tx, job)?;
        if let Some(previous) = current.finish {
            return if previous == *report {
                Ok(())
            } else {
                Err(Conflict(format!("job {job}: conflicting completion report")).into())
            };
        }
        if current.state.terminal() {
            return if current.state == report.state && current.exit == report.exit_code {
                Ok(())
            } else {
                Err(Conflict(format!("job {job}: conflicting terminal report")).into())
            };
        }
        current
            .state
            .transition(JobEvent::Finish(report.state))
            .map_err(|e| Conflict(format!("job {job}: {e}")))?;
        tx.execute("UPDATE jobs SET finish_state = ?2, finish_exit_code = ?3, finish_max_rss = ?4 WHERE id = ?1",
            params![job,report.state,report.exit_code,report.max_rss])?;
        Self::event(&tx, job, "finish_reported", Some(report.state.as_str()))?;
        tx.commit()?;
        Ok(())
    }

    /// Reject a stale recovery decision before changing state, members or events.
    pub fn recover(
        &mut self,
        expected: &ActiveJob,
        members: &[ProcessIdentity],
        event: JobEvent,
        exit_code: Option<i64>,
    ) -> Result<()> {
        let report = match event {
            JobEvent::ExecutorLost => Report::Lost,
            JobEvent::Finish(state) => Report::Finished(
                state,
                exit_code,
                expected.finish.as_ref().and_then(|r| r.max_rss),
            ),
            _ => bail!("unsupported recovery event {event:?}"),
        };
        self.apply_report(&expected.id, report, Some((expected, members)))
    }
}

// The persistence adapter owns SQL conversion; the domain has no SQLite dependency.
impl rusqlite::types::FromSql for JobState {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
    }
}

impl rusqlite::ToSql for JobState {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(
            rusqlite::types::ValueRef::Text(self.as_str().as_bytes()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::sample;

    #[test]
    fn reported_spawn_failure_can_finalize_after_takeover() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store
            .create_submission(&sample("missing-command"))
            .unwrap()
            .job;
        store.set_executor(&job, 10, "executor").unwrap();
        store.admit(&job, 100, 1).unwrap();
        store.set_started(&job, "/logs", Some(20)).unwrap();
        store
            .commit_process_members(
                &[(
                    job.clone(),
                    vec![ProcessIdentity {
                        pid: 20,
                        start: "helper".into(),
                    }],
                )]
                .into(),
            )
            .unwrap();
        store
            .report_finish(
                &job,
                &FinishReport {
                    state: JobState::StartFailed,
                    exit_code: Some(127),
                    max_rss: None,
                },
            )
            .unwrap();
        let expected = store.recovery_facts(&job).unwrap();
        let members = store.members_of(&job).unwrap();
        store
            .recover(&expected, &members, JobEvent::ExecutorLost, None)
            .unwrap();
        store.commit_process_members(&Default::default()).unwrap();
        let expected = store.recovery_facts(&job).unwrap();
        store
            .recover(
                &expected,
                &[],
                JobEvent::Finish(JobState::StartFailed),
                Some(127),
            )
            .unwrap();
        assert_eq!(
            store.job(&job).unwrap().unwrap().state,
            JobState::StartFailed
        );
    }

    #[test]
    fn completion_reports_survive_restart_and_reject_conflicting_replays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let mut store = Store::open(&path).unwrap();
        let job = store.create_submission(&sample("make")).unwrap().job;
        store.set_executor(&job, 10, "executor").unwrap();
        store.admit(&job, 100, 1).unwrap();
        store.set_started(&job, "/logs", Some(20)).unwrap();
        let before = store.recovery_facts(&job).unwrap();
        let report = FinishReport {
            state: JobState::Completed,
            exit_code: Some(7),
            max_rss: Some(900),
        };
        store.report_finish(&job, &report).unwrap();
        assert!(
            store
                .recover(&before, &[], JobEvent::Finish(JobState::EndedUnknown), None)
                .is_err()
        );
        let count = |s: &Store| {
            s.conn
                .query_row("SELECT COUNT(*) FROM job_events", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
        };
        let events = count(&store);
        store.report_finish(&job, &report).unwrap();
        for conflicting in [
            FinishReport {
                state: JobState::Canceled,
                ..report.clone()
            },
            FinishReport {
                exit_code: Some(8),
                ..report.clone()
            },
            FinishReport {
                max_rss: Some(901),
                ..report.clone()
            },
        ] {
            assert!(store.report_finish(&job, &conflicting).is_err());
        }
        assert_eq!(count(&store), events);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let expected = store.recovery_facts(&job).unwrap();
        assert_eq!(expected.finish, Some(report.clone()));
        assert_eq!(expected.state, JobState::Running);
        store
            .recover(
                &expected,
                &[],
                JobEvent::Finish(JobState::Completed),
                Some(7),
            )
            .unwrap();
        let row = store.job(&job).unwrap().unwrap();
        assert_eq!(
            (row.state, row.exit_code, row.peak_private),
            (JobState::Completed, Some(7), Some(900))
        );
        store.report_finish(&job, &report).unwrap();
    }

    #[test]
    fn completion_reports_prevent_new_execution() {
        for admitted in [false, true] {
            let mut store = Store::open_in_memory().unwrap();
            let job = store.create_submission(&sample("make")).unwrap().job;
            if admitted {
                store.set_executor(&job, 10, "executor").unwrap();
                store.admit(&job, 100, 1).unwrap();
            }
            store
                .report_finish(
                    &job,
                    &FinishReport {
                        state: JobState::StartFailed,
                        exit_code: Some(127),
                        max_rss: None,
                    },
                )
                .unwrap();
            assert!(store.queued_jobs().unwrap().is_empty());
            assert!(store.admit(&job, 100, 1).is_err());
            assert!(store.set_started(&job, "/logs", Some(20)).is_err());
            if admitted {
                assert!(store.lease_terms(&job).is_err());
            } else {
                assert!(store.set_executor(&job, 10, "executor").is_err());
            }
        }
    }

    #[test]
    fn recovery_rejects_changed_facts_without_partial_writes() {
        for change in [
            "registration",
            "admission",
            "started",
            "main_result",
            "cancellation",
            "members_added",
            "members_removed",
            "member_identity",
            "completed",
        ] {
            let mut store = Store::open_in_memory().unwrap();
            let created = store.create_submission(&sample("make")).unwrap();
            let job = &created.job;
            if change != "registration" {
                store.set_executor(job, 10, "executor").unwrap();
            }
            if !matches!(change, "registration" | "admission") {
                store.admit(job, 100, 1).unwrap();
                if change != "started" {
                    store.set_started(job, "/logs", Some(20)).unwrap();
                }
            }
            let members = |start: &str| {
                [(
                    job.clone(),
                    vec![ProcessIdentity {
                        pid: 20,
                        start: start.into(),
                    }],
                )]
                .into()
            };
            if matches!(change, "members_removed" | "member_identity") {
                store.commit_process_members(&members("old")).unwrap();
            }
            let expected = store.active_jobs().unwrap().remove(0);
            let observed = store
                .process_members()
                .unwrap()
                .remove(job)
                .unwrap_or_default();
            match change {
                "registration" => store.set_executor(job, 10, "executor").unwrap(),
                "admission" => store.admit(job, 100, 1).unwrap(),
                "started" => store.set_started(job, "/logs", Some(20)).unwrap(),
                "main_result" => store.set_main_result(job, 0).unwrap(),
                "cancellation" => {
                    store.cancel_submission(&created.submission).unwrap();
                }
                "members_added" | "member_identity" => {
                    store.commit_process_members(&members("new")).unwrap();
                }
                "members_removed" => store.commit_process_members(&Default::default()).unwrap(),
                "completed" => store.finish(job, JobState::Completed, Some(0)).unwrap(),
                _ => unreachable!(),
            }
            let facts = store.active_jobs().unwrap();
            let saved_members = store.process_members().unwrap();
            let row = store.job(job).unwrap().unwrap();
            let events = store
                .conn
                .query_row("SELECT COUNT(*) FROM job_events", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap();
            let event = match change {
                "registration" | "admission" => JobEvent::Finish(JobState::StartFailed),
                "members_removed" | "member_identity" => JobEvent::ExecutorLost,
                _ => JobEvent::Finish(JobState::EndedUnknown),
            };
            let error = store
                .recover(&expected, &observed, event, None)
                .unwrap_err();
            assert!(
                error.downcast_ref::<Conflict>().is_some(),
                "{change}: {error:#}"
            );
            assert_eq!(store.active_jobs().unwrap(), facts, "{change}");
            assert_eq!(store.process_members().unwrap(), saved_members, "{change}");
            let after = store.job(job).unwrap().unwrap();
            assert_eq!(
                (after.exit_code, after.ended_at),
                (row.exit_code, row.ended_at),
                "{change}"
            );
            assert_eq!(
                store
                    .conn
                    .query_row("SELECT COUNT(*) FROM job_events", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                events,
                "{change} must not append a recovery event"
            );
        }
    }

    #[test]
    fn recovery_applies_current_facts_and_compares_members_as_a_set() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store.create_submission(&sample("make")).unwrap().job;
        store.set_executor(&job, 10, "executor").unwrap();
        store.admit(&job, 100, 1).unwrap();
        store.set_started(&job, "/logs", Some(20)).unwrap();
        let mut members = vec![
            ProcessIdentity {
                pid: 20,
                start: "a".into(),
            },
            ProcessIdentity {
                pid: 21,
                start: "b".into(),
            },
        ];
        store
            .commit_process_members(&[(job.clone(), members.clone())].into())
            .unwrap();
        members.reverse();
        let expected = store.active_jobs().unwrap().remove(0);
        store
            .recover(&expected, &members, JobEvent::ExecutorLost, None)
            .unwrap();
        assert_eq!(
            store.job(&job).unwrap().unwrap().state,
            JobState::AwaitingTakeover
        );
        assert_eq!(store.process_members().unwrap()[&job].len(), 2);
        assert_eq!(store.held_jobs().unwrap()[0].reserved_private, 100);

        store.set_main_result(&job, 0).unwrap();
        store.commit_process_members(&Default::default()).unwrap();
        let expected = store.active_jobs().unwrap().remove(0);
        store
            .recover(
                &expected,
                &[],
                JobEvent::Finish(JobState::Completed),
                Some(0),
            )
            .unwrap();
        assert_eq!(store.job(&job).unwrap().unwrap().exit_code, Some(0));
        assert!(store.held_jobs().unwrap().is_empty());
        assert!(store.process_members().unwrap().is_empty());
    }

    #[test]
    fn invalid_transitions_and_conflicting_replays_leave_no_partial_writes() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store.create_submission(&sample("make")).unwrap().job;
        assert!(store.set_started(&job, "/logs", Some(20)).is_err());
        assert!(store.set_main_result(&job, 0).is_err());
        assert!(
            store
                .finish_with_usage(&job, JobState::Completed, Some(0), Some(900))
                .is_err()
        );
        let row = store.job(&job).unwrap().unwrap();
        assert_eq!(row.state, JobState::Queued);
        assert!(row.peak_private.is_none());
        store.set_executor(&job, 10, "executor").unwrap();
        store.admit(&job, 100, 1).unwrap();
        store.set_started(&job, "/logs", Some(20)).unwrap();
        let event_count = |s: &Store| {
            s.conn
                .query_row(
                    "SELECT COUNT(*) FROM job_events WHERE job_id = ?1",
                    params![job],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        let started = event_count(&store);
        store.set_started(&job, "/logs", Some(20)).unwrap();
        assert!(store.set_started(&job, "/other", Some(20)).is_err());
        assert_eq!(event_count(&store), started);
        store.set_main_result(&job, 0).unwrap();
        let main = event_count(&store);
        store.set_main_result(&job, 0).unwrap();
        assert!(store.set_main_result(&job, 1).is_err());
        assert_eq!(event_count(&store), main);
        store
            .finish_with_usage(&job, JobState::Completed, Some(0), Some(500))
            .unwrap();
        let finished = event_count(&store);
        store
            .finish_with_usage(&job, JobState::Completed, Some(0), Some(500))
            .unwrap();
        assert!(
            store
                .finish_with_usage(&job, JobState::Completed, Some(1), Some(900))
                .is_err()
        );
        assert_eq!(event_count(&store), finished);
        let row = store.job(&job).unwrap().unwrap();
        assert_eq!((row.exit_code, row.peak_private), (Some(0), Some(500)));
    }
}
