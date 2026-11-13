//! Store operations for admission.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Conflict, Store, no_job, now, process};
use crate::domain::resource::Lock;

use crate::domain::job::{HELD, JobEvent, JobState, PRE_LEASE};

pub(super) fn in_list(states: &[&str]) -> String {
    states
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::sample;

    #[test]
    fn unblocking_invalidates_a_pending_blocked_decision() {
        let mut store = Store::open_in_memory().unwrap();
        let failed = store.create_submission(&sample("make")).unwrap().job;
        store.finish(&failed, JobState::StartFailed, None).unwrap();
        let mut submission = sample("make test");
        submission.plan.after.push(failed.clone());
        let job = store.create_submission(&submission).unwrap().job;
        assert_eq!(store.block_for(&job, &failed).unwrap(), Some(true));
        assert_eq!(store.take_attention_reminders("sess").unwrap().len(), 1);
        assert_eq!(store.block_for(&job, &failed).unwrap(), Some(false));
        store.unblock(&job).unwrap();
        assert_eq!(store.block_for(&job, &failed).unwrap(), None);
        assert_eq!(store.job(&job).unwrap().unwrap().state, JobState::Queued);
        assert!(store.take_attention_reminders("sess").unwrap().is_empty());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Predecessor {
    pub job: String,
    pub state: JobState,
    pub exit_code: Option<i64>,
}

impl Predecessor {
    pub fn succeeded(&self) -> bool {
        self.state == crate::domain::job::JobState::Completed && self.exit_code == Some(0)
    }

    pub fn failed(&self) -> bool {
        self.state.terminal() && !self.succeeded()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueuedJob {
    pub id: String,
    pub state: JobState,
    pub created_at: String,
    pub predicted_private: u64,
    pub predicted_seconds: Option<f64>,
    pub run_now: bool,
    pub executor: Option<(i64, String)>,
    pub locks: Vec<Lock>,
    pub deps: Vec<Predecessor>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeldJob {
    pub id: String,
    pub reserved_private: u64,
    pub predicted_seconds: Option<f64>,
    pub started_at: Option<String>,
    pub locks: Vec<Lock>,
}

use crate::domain::resource::LeaseTerms;

fn lock_from(kind: String, resource: String) -> Lock {
    if kind == "path" {
        Lock::Path(resource)
    } else {
        Lock::Named(resource)
    }
}

impl Store {
    fn locks_of(&self, job: &str) -> Result<Vec<Lock>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT kind, resource FROM locks WHERE job_id = ?1")?;
        let rows = stmt.query_map(params![job], |r| Ok(lock_from(r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn predecessors(&self, job: &str) -> Result<Vec<Predecessor>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.after_job_id, j.state, j.exit_code FROM dependencies d JOIN jobs j ON j.id = d.after_job_id
             WHERE d.job_id = ?1",
        )?;
        let rows = stmt.query_map(params![job], |r| {
            Ok(Predecessor {
                job: r.get(0)?,
                state: r.get(1)?,
                exit_code: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn queued_jobs(&self) -> Result<Vec<QueuedJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, state, created_at, predicted_private, predicted_seconds, run_now, executor_pid, executor_start
             FROM jobs WHERE state IN ({}) AND finish_state IS NULL ORDER BY created_at, rowid",
            in_list(PRE_LEASE)
        ))?;
        let rows: Vec<QueuedJob> = stmt
            .query_map([], |r| {
                Ok(QueuedJob {
                    id: r.get(0)?,
                    state: r.get(1)?,
                    created_at: r.get(2)?,
                    predicted_private: r.get::<_, i64>(3)? as u64,
                    predicted_seconds: r.get(4)?,
                    run_now: r.get(5)?,
                    executor: process(r.get(6)?, r.get(7)?),
                    locks: Vec::new(),
                    deps: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|mut j| {
                j.locks = self.locks_of(&j.id)?;
                j.deps = self.predecessors(&j.id)?;
                Ok(j)
            })
            .collect()
    }

    pub fn held_jobs(&self) -> Result<Vec<HeldJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, reserved_private, predicted_seconds, started_at FROM jobs WHERE state IN ({})",
            in_list(HELD)
        ))?;
        let rows: Vec<HeldJob> = stmt
            .query_map([], |r| {
                Ok(HeldJob {
                    id: r.get(0)?,
                    reserved_private: r.get::<_, i64>(1)? as u64,
                    predicted_seconds: r.get(2)?,
                    started_at: r.get(3)?,
                    locks: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|mut h| {
                h.locks = self.locks_of(&h.id)?;
                Ok(h)
            })
            .collect()
    }

    pub fn admit(&mut self, job: &str, reserved: u64, parallelism: u32) -> Result<()> {
        let tx = self.conn.transaction()?;
        let (state, executor, finish): (JobState, Option<i64>, Option<JobState>) = tx
            .query_row(
                "SELECT state, executor_pid, finish_state FROM jobs WHERE id = ?1",
                params![job],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| no_job(job))?;
        if !state.pre_lease() || executor.is_none() || finish.is_some() {
            return Err(Conflict(format!("job {job} is {state} and cannot be admitted")).into());
        }
        state
            .transition(JobEvent::Admit)
            .map_err(|e| Conflict(e.to_string()))?;
        tx.execute(
            "INSERT INTO leases (job_id, granted_at, parallelism, jobserver_tokens)
             SELECT ?1, ?2, ?3, CASE WHEN jobserver THEN ?3 - 1 END FROM jobs WHERE id = ?1",
            params![job, now(), parallelism],
        )?;
        tx.execute(
            "UPDATE jobs SET state = 'admitted', reserved_private = ?2 WHERE id = ?1",
            params![job, reserved as i64],
        )?;
        Self::event(&tx, job, "admitted", Some(&reserved.to_string()))?;
        tx.commit()?;
        Ok(())
    }

    pub fn lease_terms(&self, job: &str) -> Result<LeaseTerms> {
        let (terms, finish) = self
            .conn
            .query_row(
                "SELECT l.parallelism, l.jobserver_tokens, j.parallel_env, j.finish_state FROM jobs j
                 LEFT JOIN leases l ON l.job_id = j.id WHERE j.id = ?1",
                params![job],
                |r| {
                    Ok((
                        LeaseTerms {
                            parallelism: r.get(0)?,
                            jobserver_tokens: r.get(1)?,
                            env: r
                                .get::<_, Option<String>>(2)?
                                .and_then(|e| serde_json::from_str(&e).ok())
                                .unwrap_or_default(),
                        },
                        r.get::<_, Option<JobState>>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| no_job(job))?;
        if finish.is_some() {
            return Err(
                Conflict(format!("job {job}: completion has already been reported")).into(),
            );
        }
        Ok(terms)
    }

    pub fn set_queue_state(&mut self, job: &str, next: JobState) -> Result<()> {
        self.update_queue_state(job, next, None).map(|_| ())
    }

    pub fn block_for(&mut self, job: &str, predecessor: &str) -> Result<Option<bool>> {
        self.update_queue_state(job, JobState::Blocked, Some(predecessor))
    }

    fn update_queue_state(
        &mut self,
        job: &str,
        next: JobState,
        failed_dependency: Option<&str>,
    ) -> Result<Option<bool>> {
        let tx = self.conn.transaction()?;
        let state: JobState = tx
            .query_row("SELECT state FROM jobs WHERE id = ?1", params![job], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| no_job(job))?;
        if !state.pre_lease() {
            return Ok(None);
        }
        if let Some(predecessor) = failed_dependency {
            let failed: bool = tx.query_row(
                &format!(
                    "SELECT EXISTS (
                        SELECT 1 FROM dependencies d JOIN jobs p ON p.id = d.after_job_id
                        WHERE d.job_id = ?1 AND p.id = ?2 AND p.state IN ({})
                          AND (p.state != 'completed' OR p.exit_code IS NOT 0)
                    )",
                    in_list(crate::domain::job::TERMINAL)
                ),
                params![job, predecessor],
                |r| r.get(0),
            )?;
            if !failed {
                return Ok(None);
            }
        }
        if state == next {
            return Ok(Some(false));
        }
        state
            .transition(JobEvent::Queue(next))
            .map_err(|e| Conflict(e.to_string()))?;
        tx.execute(
            "UPDATE jobs SET state = ?2 WHERE id = ?1",
            params![job, next],
        )?;
        tx.execute(
            "UPDATE submissions SET attention_state = NULL WHERE job_id = ?1",
            params![job],
        )?;
        Self::event(&tx, job, next.as_str(), None)?;
        tx.commit()?;
        Ok(Some(true))
    }

    pub fn resolve_job(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM jobs WHERE id = ?1 UNION ALL SELECT job_id FROM submissions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn unblock(&mut self, job: &str) -> Result<()> {
        let failed: Vec<String> = self
            .predecessors(job)?
            .into_iter()
            .filter(|p| p.failed())
            .map(|p| p.job)
            .collect();
        let tx = self.conn.transaction()?;
        for p in &failed {
            tx.execute(
                "DELETE FROM dependencies WHERE job_id = ?1 AND after_job_id = ?2",
                params![job, p],
            )?;
        }
        let changed = tx.execute(
            "UPDATE jobs SET state = 'queued' WHERE id = ?1 AND state = 'blocked'",
            params![job],
        )?;
        if changed != 0 {
            tx.execute(
                "UPDATE submissions SET attention_state = NULL WHERE job_id = ?1",
                params![job],
            )?;
        }
        Self::event(&tx, job, "unblocked", Some(&failed.join(",")))?;
        tx.commit()?;
        Ok(())
    }
}
