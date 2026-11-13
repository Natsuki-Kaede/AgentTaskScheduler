//! Durable ownership of confirmed job processes, separate from QoS obligations.

use std::collections::BTreeSet;

use anyhow::Result;
use rusqlite::params;

use super::Store;
use crate::domain::process::{Memberships, ProcessIdentity};

pub(super) const SCHEMA: &str = "CREATE TABLE job_processes (
    job_id TEXT NOT NULL REFERENCES jobs(id),
    pid INTEGER NOT NULL,
    start TEXT NOT NULL,
    PRIMARY KEY (job_id, pid, start)
);";

impl Store {
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
    use crate::store::tests::sample;

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
