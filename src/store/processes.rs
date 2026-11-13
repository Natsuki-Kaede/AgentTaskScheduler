//! Durable ownership of confirmed job processes, separate from QoS obligations.

use std::collections::BTreeSet;

use anyhow::Result;
use rusqlite::params;

#[cfg(any(target_os = "linux", test))]
use rusqlite::OptionalExtension;

use super::Store;
use crate::domain::process::{Memberships, ProcessIdentity};

pub(super) const SCHEMA: &str = "CREATE TABLE job_processes (
    job_id TEXT NOT NULL REFERENCES jobs(id),
    pid INTEGER NOT NULL,
    start TEXT NOT NULL,
    PRIMARY KEY (job_id, pid, start)
);";

impl Store {
    #[cfg(any(target_os = "linux", test))]
    pub fn register_boot(&mut self, boot_id: &str) -> Result<()> {
        use crate::domain::job::{JobEvent, JobState, TERMINAL};

        let tx = self.conn.transaction()?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT boot_id FROM process_boot WHERE singleton = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if previous.as_deref() == Some(boot_id) {
            return Ok(());
        }
        if previous.is_some() {
            let query = format!(
                "SELECT {} FROM jobs WHERE state NOT IN ({})",
                super::RECOVERY_COLUMNS,
                super::sched::in_list(TERMINAL),
            );
            let jobs = tx
                .prepare(&query)?
                .query_map([], super::recovery_row)?
                .collect::<Result<Vec<_>, _>>()?;
            for job in jobs {
                let (state, code) = if job.canceled {
                    (
                        JobState::Canceled,
                        job.finish.and_then(|f| f.exit_code).or(job.main_exit_code),
                    )
                } else if let Some(finish) = job.finish {
                    (finish.state, finish.exit_code)
                } else if job.state.pre_lease() || job.state == JobState::Admitted {
                    (JobState::StartFailed, None)
                } else if let Some(code) = job.main_exit_code {
                    (JobState::Completed, Some(code))
                } else {
                    (JobState::EndedUnknown, None)
                };
                let state = job.state.transition(JobEvent::Finish(state))?;
                tx.execute(
                    "UPDATE jobs SET state = ?2, exit_code = ?3, ended_at = ?4,
                     peak_private = COALESCE(peak_private, finish_max_rss) WHERE id = ?1",
                    params![job.id, state, code, super::now()],
                )?;
                Self::event(&tx, &job.id, state.as_str(), Some("system rebooted"))?;
            }
            tx.execute_batch(
                "DELETE FROM job_processes;
                 DELETE FROM background_processes;
                 DELETE FROM background_jobs;",
            )?;
        }
        tx.execute(
            "INSERT INTO process_boot (singleton, boot_id) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET boot_id = excluded.boot_id",
            [boot_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn members_of(&self, job: &str) -> Result<Vec<ProcessIdentity>> {
        Ok(self
            .conn
            .prepare("SELECT pid, start FROM job_processes WHERE job_id = ?1 ORDER BY pid, start")?
            .query_map(params![job], |r| {
                Ok(ProcessIdentity {
                    pid: r.get(0)?,
                    start: r.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn process_members(&self) -> Result<Memberships> {
        let mut stmt = self.conn.prepare(
            "SELECT p.job_id, p.pid, p.start FROM job_processes p JOIN jobs j ON j.id = p.job_id
             WHERE j.state IN ('admitted', 'running', 'awaiting_takeover') ORDER BY p.job_id, p.pid",
        )?;
        let mut members = Memberships::new();
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ProcessIdentity {
                    pid: r.get(1)?,
                    start: r.get(2)?,
                },
            ))
        })? {
            let (job, identity) = row?;
            members.entry(job).or_default().push(identity);
        }
        Ok(members)
    }

    pub fn commit_process_members(&mut self, members: &Memberships) -> Result<()> {
        let tx = self.conn.transaction()?;
        let held: BTreeSet<String> = tx
            .prepare(
                "SELECT id FROM jobs WHERE state IN ('admitted', 'running', 'awaiting_takeover')",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let wanted: BTreeSet<(String, i32, String)> = members
            .iter()
            .filter(|(job, _)| held.contains(*job))
            .flat_map(|(job, identities)| {
                identities
                    .iter()
                    .map(|p| (job.clone(), p.pid, p.start.clone()))
            })
            .collect();
        let current: BTreeSet<(String, i32, String)> = tx
            .prepare("SELECT job_id, pid, start FROM job_processes")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (job, pid, start) in current.difference(&wanted) {
            tx.execute(
                "DELETE FROM job_processes WHERE job_id = ?1 AND pid = ?2 AND start = ?3",
                params![job, pid, start],
            )?;
        }
        for (job, pid, start) in wanted.difference(&current) {
            tx.execute(
                "INSERT INTO job_processes (job_id, pid, start) VALUES (?1, ?2, ?3)",
                params![job, pid, start],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::job::{FinishReport, JobState};
    use crate::store::tests::sample;

    #[test]
    fn boot_change_retires_identities_and_preserves_known_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let mut store = Store::open(&path).unwrap();
        let create = |store: &mut Store, admitted, started| {
            let created = store.create_submission(&sample("make")).unwrap();
            store.set_executor(&created.job, 10, "executor").unwrap();
            if admitted {
                store.admit(&created.job, 100, 1).unwrap();
            }
            if started {
                store
                    .record_start(&created.job, "/logs", Some(20), Some("leader"))
                    .unwrap();
            }
            created
        };
        let queued = create(&mut store, false, false).job;
        let admitted = create(&mut store, true, false).job;
        let running = create(&mut store, true, true).job;
        let main = create(&mut store, true, true).job;
        let finish = create(&mut store, true, true).job;
        let canceled = create(&mut store, true, true);
        store.set_main_result(&main, 5).unwrap();
        store
            .report_finish(
                &finish,
                &FinishReport {
                    state: JobState::Completed,
                    exit_code: Some(7),
                    max_rss: Some(900),
                },
            )
            .unwrap();
        store.cancel_submission(&canceled.submission).unwrap();
        let members = [(
            running.clone(),
            vec![ProcessIdentity {
                pid: 20,
                start: "leader".into(),
            }],
        )]
        .into();
        store.commit_process_members(&members).unwrap();
        store
            .remember_background_for_job(&running, 20, "leader")
            .unwrap();
        store.register_boot("boot-a").unwrap();
        drop(store);

        let mut store = Store::open(&path).unwrap();
        store.register_boot("boot-a").unwrap();
        assert_eq!(store.process_members().unwrap(), members);
        assert_eq!(
            store
                .recovery_facts(&running)
                .unwrap()
                .pgid_start
                .as_deref(),
            Some("leader")
        );
        assert_eq!(store.background_obligations().unwrap().len(), 1);
        store.register_boot("boot-b").unwrap();
        assert!(store.active_jobs().unwrap().is_empty());
        assert!(store.held_jobs().unwrap().is_empty());
        assert!(store.process_members().unwrap().is_empty());
        assert!(store.background_obligations().unwrap().is_empty());
        assert!(store.background_jobs().unwrap().is_empty());
        for (id, state, code) in [
            (queued, JobState::StartFailed, None),
            (admitted, JobState::StartFailed, None),
            (running, JobState::EndedUnknown, None),
            (main, JobState::Completed, Some(5)),
            (finish, JobState::Completed, Some(7)),
            (canceled.job, JobState::Canceled, None),
        ] {
            let job = store.job(&id).unwrap().unwrap();
            assert_eq!((job.state, job.exit_code), (state, code));
        }
        let next = create(&mut store, false, false).job;
        store.register_boot("boot-b").unwrap();
        assert_eq!(store.job(&next).unwrap().unwrap().state, JobState::Queued);
    }

    #[test]
    fn migrates_version_three_and_restores_only_live_job_membership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.execute_batch(super::super::BACKGROUND_SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        drop(conn);
        let mut store = Store::open(&path).unwrap();
        let job = store.create_submission(&sample("make")).unwrap().job;
        store.set_executor(&job, 10, "executor").unwrap();
        store.admit(&job, 100, 1).unwrap();
        let members = [(
            job.clone(),
            vec![ProcessIdentity {
                pid: 20,
                start: "worker".into(),
            }],
        )]
        .into();
        store.commit_process_members(&members).unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.process_members().unwrap(), members);
        store
            .finish(&job, crate::domain::job::JobState::Completed, Some(0))
            .unwrap();
        store.commit_process_members(&members).unwrap();
        assert!(store.process_members().unwrap().is_empty());
    }
}
