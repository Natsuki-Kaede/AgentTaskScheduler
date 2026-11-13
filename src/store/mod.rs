//! Owns the daemon's SQLite database.

use crate::domain::resource::Lock;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

mod background;
mod history;
mod lifecycle;
mod processes;
mod reminders;
mod sched;

use crate::domain::job::JobState;
use crate::domain::job::{HELD, TERMINAL};

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 6;
const SCHEMA_V5: &str = include_str!("schema_v5.sql");
const SCHEMA_V6: &str = include_str!("schema_v6.sql");
const BACKGROUND_SCHEMA: &str = "CREATE TABLE background_processes (
    pid INTEGER NOT NULL,
    start TEXT NOT NULL,
    PRIMARY KEY (pid, start)
);";

pub const KEEP: std::time::Duration = std::time::Duration::from_secs(90 * 24 * 3600);

use crate::timing::now;

pub struct Store {
    conn: Connection,
}

use crate::domain::job::{ActiveJob, Created, JobRecord, NewSubmission, SubmissionRecord};

fn process(pid: Option<i64>, start: Option<String>) -> Option<(i64, String)> {
    pid.zip(start)
}

const RECOVERY_COLUMNS: &str =
    "id, state, executor_pid, executor_start, pgid, launcher_pid, launcher_start, main_exit_code,
    NOT EXISTS (SELECT 1 FROM submissions s WHERE s.job_id = jobs.id AND s.canceled = 0),
    finish_state, finish_exit_code, finish_max_rss, pgid_start, launch_pid, launch_start";

fn recovery_row(r: &rusqlite::Row) -> rusqlite::Result<ActiveJob> {
    Ok(ActiveJob {
        id: r.get(0)?,
        state: r.get(1)?,
        executor: process(r.get(2)?, r.get(3)?),
        pgid: r.get(4)?,
        pgid_start: r.get(12)?,
        launched: process(r.get(13)?, r.get(14)?),
        launcher: process(r.get(5)?, r.get(6)?),
        main_exit_code: r.get(7)?,
        canceled: r.get(8)?,
        finish: lifecycle::finish_row(r, 9)?,
    })
}

use crate::domain::error::{Conflict, NotFound};

fn no_job(job: &str) -> anyhow::Error {
    NotFound(format!("no job {job}")).into()
}

fn no_submission(id: &str) -> anyhow::Error {
    NotFound(format!("no submission {id}")).into()
}

const JOB_COLUMNS: &str = "j.id, j.state, j.entry, j.command, j.cwd, j.project_root, j.exit_code,
    j.main_exit_code, j.log_path, j.executor_pid, j.peak_private,
    (SELECT COUNT(*) FROM submissions s WHERE s.job_id = j.id), j.created_at, j.started_at, j.ended_at";

fn job_row(r: &rusqlite::Row) -> rusqlite::Result<JobRecord> {
    Ok(JobRecord {
        id: r.get(0)?,
        state: r.get(1)?,
        entry: r.get(2)?,
        command: r.get(3)?,
        cwd: r.get(4)?,
        project_root: r.get(5)?,
        exit_code: r.get(6)?,
        main_exit_code: r.get(7)?,
        log_path: r.get(8)?,
        executor_pid: r.get(9)?,
        peak_private: r.get(10)?,
        submissions: r.get(11)?,
        created_at: r.get(12)?,
        started_at: r.get(13)?,
        ended_at: r.get(14)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("database schema version {version} is newer than this ats ({SCHEMA_VERSION})");
        }
        let tx = conn.unchecked_transaction()?;
        match version {
            0 => {
                tx.execute_batch(SCHEMA)?;
            }
            1 => {
                tx.execute_batch(
                    "ALTER TABLE submissions ADD COLUMN detached INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            _ => {}
        }
        if version < 3 {
            tx.execute_batch(BACKGROUND_SCHEMA)?;
        }
        if version < 4 {
            tx.execute_batch(processes::SCHEMA)?;
        }
        if version < 5 {
            tx.execute_batch(SCHEMA_V5)?;
        }
        if version < 6 {
            tx.execute_batch(SCHEMA_V6)?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(Self { conn })
    }

    pub fn create_submission(&mut self, new: &NewSubmission) -> Result<Created> {
        let tx = self.conn.transaction()?;
        let job = crate::ids::new("j");
        let submission = crate::ids::new("s");
        let at = now();
        let p = &new.plan;
        tx.execute(
            "INSERT INTO jobs (id, state, cwd, project_root, entry, command, profile, history_key,
                               predicted_private, predicted_seconds, run_now, parallel_env, jobserver,
                               launcher_pid, launcher_start, created_at)
             VALUES (?1, 'queued', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                job,
                new.cwd,
                new.project_root,
                new.entry.as_str(),
                new.command,
                serde_json::to_string(&p.profiles)?,
                p.history_key,
                p.prediction.mem as i64,
                p.prediction.seconds,
                p.run_now,
                serde_json::to_string(&p.parallel_env)?,
                p.jobserver,
                new.launcher.as_ref().map(|l| l.0),
                new.launcher.as_ref().map(|l| &l.1),
                at,
            ],
        )?;
        tx.execute(
            "INSERT INTO submissions (id, job_id, session_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![submission, job, new.session_id, at],
        )?;
        for lock in &p.locks {
            let (kind, resource) = match lock {
                Lock::Path(p) => ("path", p),
                Lock::Named(n) => ("named", n),
            };
            tx.execute(
                "INSERT OR IGNORE INTO locks (job_id, resource, kind) VALUES (?1, ?2, ?3)",
                params![job, resource, kind],
            )?;
        }
        for after in &p.after {
            tx.execute(
                "INSERT OR IGNORE INTO dependencies (job_id, after_job_id) VALUES (?1, ?2)",
                params![job, after],
            )?;
        }
        Self::event(&tx, &job, "submitted", None)?;
        tx.commit()?;
        Ok(Created { submission, job })
    }

    pub fn list_jobs(&self, states: &[String], limit: u32) -> Result<Vec<JobRecord>> {
        let mut sql = format!("SELECT {JOB_COLUMNS} FROM jobs j");
        if !states.is_empty() {
            let marks = vec!["?"; states.len()].join(",");
            sql.push_str(&format!(" WHERE j.state IN ({marks})"));
        }
        sql.push_str(" ORDER BY j.created_at DESC, j.rowid DESC LIMIT ?");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut values: Vec<&dyn rusqlite::ToSql> =
            states.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        values.push(&limit);
        let rows = stmt.query_map(values.as_slice(), job_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn job(&self, id: &str) -> Result<Option<JobRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs j WHERE j.id = ?1"),
                params![id],
                job_row,
            )
            .optional()?)
    }

    pub fn submission(&self, id: &str) -> Result<Option<SubmissionRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.id, s.job_id, j.state, s.session_id, j.exit_code, j.main_exit_code, j.log_path,
                        s.canceled, s.detached, s.quiet
                 FROM submissions s JOIN jobs j ON j.id = s.job_id WHERE s.id = ?1",
                params![id],
                |r| {
                    Ok(SubmissionRecord {
                        id: r.get(0)?,
                        job: r.get(1)?,
                        job_state: r.get(2)?,
                        session_id: r.get(3)?,
                        exit_code: r.get(4)?,
                        main_exit_code: r.get(5)?,
                        log_path: r.get(6)?,
                        canceled: r.get(7)?,
                        detached: r.get(8)?,
                        quiet: r.get(9)?,
                    })
                },
            )
            .optional()?)
    }

    fn event(
        tx: &rusqlite::Transaction,
        job: &str,
        kind: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO job_events (job_id, at, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            params![job, now(), kind, detail],
        )?;
        Ok(())
    }

    pub fn record_peaks(&mut self, peaks: &[(String, u64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (job, peak) in peaks {
            tx.execute(
                &format!(
                    "UPDATE jobs SET peak_private = MAX(COALESCE(peak_private, 0), ?2),
                     reserved_private = CASE WHEN state IN ({}) THEN MAX(reserved_private, ?2) ELSE reserved_private END
                     WHERE id = ?1",
                    sched::in_list(HELD)
                ),
                params![job, *peak as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn settle_reservations(&mut self, before: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE jobs SET reserved_private = CAST(peak_private * 1.3 AS INTEGER)
             WHERE state IN ('running', 'awaiting_takeover') AND started_at < ?1
               AND peak_private IS NOT NULL AND peak_private * 1.3 < reserved_private",
            params![before],
        )?)
    }

    pub fn active_jobs(&self) -> Result<Vec<ActiveJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RECOVERY_COLUMNS} FROM jobs WHERE state NOT IN ('completed', 'start_failed', 'canceled', 'ended_unknown')"
        ))?;
        let rows = stmt.query_map([], recovery_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn recovery_facts(&self, job: &str) -> Result<ActiveJob> {
        self.conn
            .query_row(
                &format!("SELECT {RECOVERY_COLUMNS} FROM jobs WHERE id = ?1"),
                params![job],
                recovery_row,
            )
            .optional()?
            .ok_or_else(|| no_job(job))
    }

    fn update_submission(&mut self, id: &str, set: &str) -> Result<String> {
        let job: String = self
            .conn
            .query_row(
                "SELECT job_id FROM submissions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| no_submission(id))?;
        self.conn.execute(
            &format!("UPDATE submissions SET {set} WHERE id = ?1"),
            params![id],
        )?;
        Ok(job)
    }

    pub fn cancel_submission(&mut self, id: &str) -> Result<String> {
        let tx = self.conn.transaction()?;
        let (job, state): (String, JobState) = tx.query_row(
            "SELECT s.job_id, j.state FROM submissions s JOIN jobs j ON j.id = s.job_id WHERE s.id = ?1",
            params![id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?.ok_or_else(|| no_submission(id))?;
        tx.execute(
            "UPDATE submissions SET canceled = 1 WHERE id = ?1",
            params![id],
        )?;
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM submissions WHERE job_id = ?1 AND canceled = 0",
            params![job],
            |r| r.get(0),
        )?;
        if open == 0 && (state.pre_lease() || state == JobState::Admitted) {
            state
                .transition(crate::domain::job::JobEvent::Finish(JobState::Canceled))
                .map_err(|e| Conflict(e.to_string()))?;
            tx.execute(
                "UPDATE jobs SET state = 'canceled', ended_at = ?2 WHERE id = ?1",
                params![job, now()],
            )?;
            Self::event(&tx, &job, "canceled", None)?;
        }
        tx.commit()?;
        Ok(job)
    }

    pub fn quiet_submission(&mut self, id: &str) -> Result<String> {
        self.update_submission(
            id,
            &format!(
                "quiet = CASE WHEN EXISTS (SELECT 1 FROM jobs j WHERE j.id = job_id AND (j.state IN ({}) OR j.main_exit_code IS NOT NULL)) THEN 1 ELSE quiet END",
                sched::in_list(TERMINAL)
            ),
        )
    }

    pub fn detach_submission(&mut self, id: &str) -> Result<String> {
        self.update_submission(id, "detached = 1")
    }

    pub fn prune(&mut self, before: &str) -> Result<Vec<(String, Option<String>)>> {
        let terminal = sched::in_list(TERMINAL);
        let tx = self.conn.transaction()?;
        let old: Vec<(String, Option<String>)> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT id, log_path FROM jobs j WHERE state IN ({terminal}) AND ended_at < ?1
                 AND NOT EXISTS (SELECT 1 FROM dependencies d JOIN jobs k ON k.id = d.job_id
                                 WHERE d.after_job_id = j.id AND k.state NOT IN ({terminal}))"
            ))?;
            let rows = stmt.query_map(params![before], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (job, _) in &old {
            for sql in [
                "DELETE FROM job_processes WHERE job_id = ?1",
                "DELETE FROM job_events WHERE job_id = ?1",
                "DELETE FROM locks WHERE job_id = ?1",
                "DELETE FROM leases WHERE job_id = ?1",
                "DELETE FROM dependencies WHERE job_id = ?1 OR after_job_id = ?1",
                "DELETE FROM submissions WHERE job_id = ?1",
                "DELETE FROM jobs WHERE id = ?1",
            ] {
                tx.execute(sql, params![job])?;
            }
        }
        tx.commit()?;
        Ok(old)
    }

    pub fn cancel_requested(&self, job: &str) -> Result<bool> {
        let open: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM submissions WHERE job_id = ?1 AND canceled = 0",
            params![job],
            |r| r.get(0),
        )?;
        Ok(open == 0)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::job::{Entry, Plan, Reminder};

    pub(crate) fn sample(command: &str) -> NewSubmission {
        NewSubmission {
            entry: Entry::Sh,
            command: command.into(),
            cwd: "/tmp/p".into(),
            project_root: "/tmp/p".into(),
            session_id: Some("sess".into()),
            launcher: None,
            plan: Plan::default(),
        }
    }

    #[test]
    fn creates_and_reads_back() {
        let mut store = Store::open_in_memory().unwrap();
        let c = store.create_submission(&sample("cargo build")).unwrap();
        let jobs = store.list_jobs(&[], 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (
                jobs[0].command.as_str(),
                jobs[0].state.as_str(),
                jobs[0].submissions
            ),
            ("cargo build", "queued", 1)
        );
        let s = store.submission(&c.submission).unwrap().unwrap();
        assert_eq!((s.job, s.job_state.as_str()), (c.job, "queued"));
        assert!(store.submission("s_missing").unwrap().is_none());
    }

    #[test]
    fn peaks_only_grow() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store.create_submission(&sample("make")).unwrap().job;
        store.set_executor(&job, 1, "t").unwrap();
        store.admit(&job, 400, 1).unwrap();
        store.record_peaks(&[(job.clone(), 500)]).unwrap();
        store.record_peaks(&[(job.clone(), 300)]).unwrap();
        assert_eq!(
            store.job(&job).unwrap().unwrap().peak_private,
            Some(500),
            "a sampled peak is kept"
        );
        assert_eq!(
            store.held_jobs().unwrap()[0].reserved_private,
            500,
            "the reservation follows the peak"
        );
        store
            .finish_with_usage(&job, JobState::Completed, Some(0), Some(900))
            .unwrap();
        assert_eq!(store.job(&job).unwrap().unwrap().peak_private, Some(500));
    }

    #[test]
    fn reminders_name_each_submission_until_it_is_quiet() {
        let mut store = Store::open_in_memory().unwrap();
        let mut submit = |session: Option<&str>| {
            let mut new = sample("make");
            new.session_id = session.map(str::to_string);
            store.create_submission(&new).unwrap()
        };
        let (running, ended, delivered, canceled, detached) = (
            submit(Some("s")),
            submit(Some("s")),
            submit(Some("s")),
            submit(Some("s")),
            submit(Some("s")),
        );
        submit(Some("another session"));
        store.detach_submission(&detached.submission).unwrap();
        store.set_executor(&ended.job, 1, "t").unwrap();
        store.admit(&ended.job, 0, 1).unwrap();
        store
            .finish(&ended.job, crate::domain::job::JobState::Completed, Some(0))
            .unwrap();
        store.set_executor(&delivered.job, 1, "t").unwrap();
        store.admit(&delivered.job, 0, 1).unwrap();
        store
            .finish(
                &delivered.job,
                crate::domain::job::JobState::Completed,
                Some(1),
            )
            .unwrap();
        store.quiet_submission(&delivered.submission).unwrap();
        store.cancel_submission(&canceled.submission).unwrap();
        let ids = |r: Vec<Reminder>| r.into_iter().map(|r| r.submission).collect::<Vec<_>>();
        assert_eq!(
            ids(store.running_reminders("s").unwrap()),
            std::slice::from_ref(&running.submission)
        );
        assert_eq!(
            ids(store.take_ended_reminders("s").unwrap()),
            std::slice::from_ref(&ended.submission)
        );
        assert!(
            store.take_ended_reminders("s").unwrap().is_empty(),
            "reported once"
        );
        store.set_executor(&detached.job, 1, "t").unwrap();
        store.admit(&detached.job, 0, 1).unwrap();
        store
            .finish(
                &detached.job,
                crate::domain::job::JobState::Completed,
                Some(0),
            )
            .unwrap();
        assert_eq!(
            ids(store.take_ended_reminders("s").unwrap()),
            std::slice::from_ref(&detached.submission)
        );
        assert!(store.submission(&ended.submission).unwrap().unwrap().quiet);
    }

    #[test]
    fn prunes_old_jobs_but_keeps_awaited_predecessors() {
        let mut store = Store::open_in_memory().unwrap();
        let mut job = |after: &[&str]| {
            let mut new = sample("make");
            new.plan.after = after.iter().map(|s| s.to_string()).collect();
            store.create_submission(&new).unwrap().job
        };
        let (old, recent, awaited) = (job(&[]), job(&[]), job(&[]));
        let waiting = job(&[&awaited]);
        for j in [&old, &recent, &awaited] {
            store.set_executor(j, 1, "t").unwrap();
            store.admit(j, 0, 1).unwrap();
            store
                .finish(j, crate::domain::job::JobState::Completed, Some(0))
                .unwrap();
        }
        for j in [&old, &awaited] {
            store
                .conn
                .execute(
                    "UPDATE jobs SET ended_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
                    params![j],
                )
                .unwrap();
        }
        let pruned: Vec<String> = store
            .prune("2026-07-01T00:00:00.000Z")
            .unwrap()
            .into_iter()
            .map(|(j, _)| j)
            .collect();
        assert_eq!(pruned, std::slice::from_ref(&old));
        assert!(store.job(&old).unwrap().is_none());
        for j in [&recent, &awaited, &waiting] {
            assert!(store.job(j).unwrap().is_some());
        }
    }

    #[test]
    fn reservations_fall_to_measurements_after_a_while() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store.create_submission(&sample("make")).unwrap().job;
        store.set_executor(&job, 1, "t").unwrap();
        store.admit(&job, 1000, 1).unwrap();
        store.set_started(&job, "/l", None).unwrap();
        store.record_peaks(&[(job.clone(), 100)]).unwrap();
        let reserved = |s: &Store| s.held_jobs().unwrap()[0].reserved_private;
        assert_eq!(
            store
                .settle_reservations("2000-01-01T00:00:00.000Z")
                .unwrap(),
            0,
            "not long enough"
        );
        assert_eq!(reserved(&store), 1000);
        store
            .settle_reservations("2999-01-01T00:00:00.000Z")
            .unwrap();
        assert_eq!(reserved(&store), 130);
        store.record_peaks(&[(job.clone(), 400)]).unwrap();
        assert_eq!(reserved(&store), 400, "growth raises it again");
    }

    #[test]
    fn filters_by_state() {
        let mut store = Store::open_in_memory().unwrap();
        store.create_submission(&sample("a")).unwrap();
        assert_eq!(store.list_jobs(&["queued".into()], 10).unwrap().len(), 1);
        assert!(store.list_jobs(&["running".into()], 10).unwrap().is_empty());
    }

    #[test]
    fn upgrades_a_version_1_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let old = SCHEMA.replace(
            "    detached            INTEGER NOT NULL DEFAULT 0, -- Stop does not wait for it\n",
            "",
        );
        assert_ne!(old, SCHEMA);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&old).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);
        let mut store = Store::open(&path).unwrap();
        let id = store.create_submission(&sample("make")).unwrap().submission;
        store.detach_submission(&id).unwrap();
        assert!(store.submission(&id).unwrap().unwrap().detached);
    }

    #[test]
    fn survives_reopen_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let id = Store::open(&path)
            .unwrap()
            .create_submission(&sample("a"))
            .unwrap()
            .submission;
        assert!(
            Store::open(&path)
                .unwrap()
                .submission(&id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn upgrades_version_2_and_persists_background_obligations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        drop(conn);
        let store = Store::open(&path).unwrap();
        store.remember_background(10, "start").unwrap();
        store.remember_background(10, "start").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.background_processes().unwrap(),
            [(10, "start".into())]
        );
        store.forget_background(10, "other").unwrap();
        assert_eq!(store.background_processes().unwrap().len(), 1);
        store.forget_background(10, "start").unwrap();
        assert!(store.background_processes().unwrap().is_empty());
    }
}
