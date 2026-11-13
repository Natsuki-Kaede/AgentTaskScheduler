//! QoS restoration ownership survives process creation, job completion and restart.

use anyhow::Result;
use rusqlite::params;

use super::Store;
use crate::domain::process::ProcessIdentity;
use crate::domain::resource::QosObligation;

impl Store {
    pub fn remember_background_for_job(&self, job: &str, pid: i32, start: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO background_jobs (job_id) VALUES (?1)",
            params![job],
        )?;
        tx.execute(
            "INSERT INTO background_processes (pid, start, job_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(pid, start) DO UPDATE SET job_id = COALESCE(background_processes.job_id, excluded.job_id)",
            params![pid,start,job],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn background_obligations(&self) -> Result<Vec<QosObligation>> {
        Ok(self
            .conn
            .prepare("SELECT job_id, pid, start FROM background_processes ORDER BY pid, start")?
            .query_map([], |r| {
                Ok(QosObligation {
                    job: r.get(0)?,
                    identity: ProcessIdentity {
                        pid: r.get(1)?,
                        start: r.get(2)?,
                    },
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn background_jobs(&self) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare("SELECT job_id FROM background_jobs ORDER BY job_id")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    pub fn forget_background_job(&self, job: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM background_jobs WHERE job_id = ?1",
            params![job],
        )?;
        Ok(())
    }

    pub fn forget_background(&self, pid: i32, start: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM background_processes WHERE pid = ?1 AND start = ?2",
            params![pid, start],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn remember_background(&self, pid: i32, start: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO background_processes (pid, start) VALUES (?1, ?2)",
            params![pid, start],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn background_processes(&self) -> Result<Vec<(i32, String)>> {
        Ok(self
            .background_obligations()?
            .into_iter()
            .map(|o| (o.identity.pid, o.identity.start))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::job::{FinishReport, JobState};

    fn version_four(path: &std::path::Path, conflict: bool) -> String {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.execute_batch(super::super::BACKGROUND_SCHEMA).unwrap();
        conn.execute_batch(super::super::processes::SCHEMA).unwrap();
        let mut legacy = Store { conn };
        let job = legacy
            .create_submission(&crate::store::tests::sample("make"))
            .unwrap()
            .job;
        legacy.conn.execute("UPDATE jobs SET state='running', executor_pid=10, executor_start='executor', pgid=20, reserved_private=100 WHERE id=?1",params![job]).unwrap();
        legacy
            .conn
            .execute(
                "INSERT INTO job_processes (job_id,pid,start) VALUES (?1,20,'worker')",
                params![job],
            )
            .unwrap();
        legacy
            .conn
            .execute(
                "INSERT INTO background_processes (pid,start) VALUES (20,'worker')",
                [],
            )
            .unwrap();
        legacy.conn.pragma_update(None, "user_version", 4).unwrap();
        if conflict {
            legacy
                .conn
                .execute("CREATE TABLE background_jobs (job_id TEXT PRIMARY KEY)", [])
                .unwrap();
        }
        job
    }

    #[test]
    fn version_five_preserves_legacy_ownership_and_persists_new_obligations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let job = version_four(&path, false);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            super::super::SCHEMA_VERSION
        );
        assert_eq!(store.recovery_facts(&job).unwrap().state, JobState::Running);
        assert!(store.recovery_facts(&job).unwrap().finish.is_none());
        assert_eq!(store.members_of(&job).unwrap()[0].pid, 20);
        assert_eq!(
            store.background_processes().unwrap(),
            [(20, "worker".into())]
        );
        assert!(store.background_obligations().unwrap()[0].job.is_none());
        let report = FinishReport {
            state: JobState::Completed,
            exit_code: Some(7),
            max_rss: Some(900),
        };
        store.report_finish(&job, &report).unwrap();
        store
            .remember_background_for_job(&job, 20, "worker")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.recovery_facts(&job).unwrap().finish, Some(report));
        assert_eq!(store.background_jobs().unwrap(), std::slice::from_ref(&job));
        assert_eq!(
            store.background_obligations().unwrap()[0].job.as_deref(),
            Some(job.as_str())
        );
    }

    #[test]
    fn a_failed_version_five_migration_leaves_version_four_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let job = version_four(&path, true);
        assert!(Store::open(&path).is_err());
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            4
        );
        assert!(conn.prepare("SELECT finish_state FROM jobs").is_err());
        assert!(
            conn.prepare("SELECT job_id FROM background_processes")
                .is_err()
        );
        assert_eq!(
            conn.query_row("SELECT state FROM jobs WHERE id=?1", params![job], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "running"
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM job_processes", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM background_processes", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
