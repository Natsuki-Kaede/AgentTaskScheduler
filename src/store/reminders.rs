//! Session notification queries and their transactional delivery receipts.

use anyhow::Result;
use rusqlite::{Connection, params};

use super::{Store, sched};
use crate::domain::job::{Reminder, TERMINAL};

fn reminders(conn: &Connection, session: &str, filter: &str) -> Result<Vec<Reminder>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT s.id, s.job_id, j.state, j.exit_code, j.command
         FROM submissions s JOIN jobs j ON j.id = s.job_id
         WHERE s.session_id = ?1 AND s.canceled = 0 AND {filter}
         ORDER BY s.created_at, s.rowid"
    ))?;
    let rows = stmt.query_map(params![session], |r| {
        Ok(Reminder {
            submission: r.get(0)?,
            job: r.get(1)?,
            state: r.get(2)?,
            exit_code: r.get(3)?,
            command: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

impl Store {
    pub fn running_reminders(&self, session: &str) -> Result<Vec<Reminder>> {
        reminders(
            &self.conn,
            session,
            &format!(
                "s.detached = 0 AND j.state NOT IN ({})",
                sched::in_list(TERMINAL)
            ),
        )
    }

    pub fn take_ended_reminders(&mut self, session: &str) -> Result<Vec<Reminder>> {
        let tx = self.conn.transaction()?;
        let ended = reminders(
            &tx,
            session,
            &format!("s.quiet = 0 AND j.state IN ({})", sched::in_list(TERMINAL)),
        )?;
        for r in &ended {
            tx.execute(
                "UPDATE submissions SET quiet = 1 WHERE id = ?1",
                params![r.submission],
            )?;
        }
        tx.commit()?;
        Ok(ended)
    }

    pub fn take_attention_reminders(&mut self, session: &str) -> Result<Vec<Reminder>> {
        let tx = self.conn.transaction()?;
        let attention = reminders(
            &tx,
            session,
            "j.state IN ('blocked', 'capacity_insufficient') AND s.attention_state IS NOT j.state",
        )?;
        for r in &attention {
            tx.execute(
                "UPDATE submissions SET attention_state = ?2 WHERE id = ?1",
                params![r.submission, r.state],
            )?;
        }
        tx.commit()?;
        Ok(attention)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::job::JobState;
    use crate::store::tests::sample;

    #[test]
    fn attention_is_per_submission_and_state_without_consuming_completion() {
        for state in [JobState::Blocked, JobState::CapacityInsufficient] {
            for detached in [false, true] {
                let mut store = Store::open_in_memory().unwrap();
                let created = store.create_submission(&sample("make")).unwrap();
                if detached {
                    store.detach_submission(&created.submission).unwrap();
                }
                store.set_queue_state(&created.job, state).unwrap();
                store.quiet_submission(&created.submission).unwrap();
                assert!(
                    !store
                        .submission(&created.submission)
                        .unwrap()
                        .unwrap()
                        .quiet
                );
                assert!(store.take_attention_reminders("other").unwrap().is_empty());
                assert!(store.take_ended_reminders("sess").unwrap().is_empty());
                let notice = store.take_attention_reminders("sess").unwrap();
                assert_eq!(notice.len(), 1);
                assert_eq!(notice[0].submission, created.submission);
                assert_eq!(notice[0].state, state);
                store.set_queue_state(&created.job, state).unwrap();
                assert!(store.take_attention_reminders("sess").unwrap().is_empty());
                if state == JobState::Blocked {
                    store.unblock(&created.job).unwrap();
                } else {
                    store
                        .set_queue_state(&created.job, JobState::Queued)
                        .unwrap();
                }
                store.set_queue_state(&created.job, state).unwrap();
                assert_eq!(store.take_attention_reminders("sess").unwrap().len(), 1);
                let other = if state == JobState::Blocked {
                    JobState::CapacityInsufficient
                } else {
                    JobState::Blocked
                };
                store.set_queue_state(&created.job, other).unwrap();
                assert_eq!(
                    store.take_attention_reminders("sess").unwrap()[0].state,
                    other
                );
                store.set_executor(&created.job, 1, "executor").unwrap();
                store.admit(&created.job, 0, 1).unwrap();
                store
                    .finish(&created.job, JobState::Completed, Some(0))
                    .unwrap();
                assert!(store.take_attention_reminders("sess").unwrap().is_empty());
                assert_eq!(store.take_ended_reminders("sess").unwrap().len(), 1);
                assert!(store.take_ended_reminders("sess").unwrap().is_empty());
            }
        }
    }

    #[test]
    fn attention_receipts_survive_restart_and_canceled_work_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let mut store = Store::open(&path).unwrap();
        let first = store.create_submission(&sample("make")).unwrap();
        store
            .set_queue_state(&first.job, JobState::Blocked)
            .unwrap();
        assert_eq!(store.take_attention_reminders("sess").unwrap().len(), 1);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert!(store.take_attention_reminders("sess").unwrap().is_empty());
        let second = store.create_submission(&sample("other")).unwrap();
        store
            .set_queue_state(&second.job, JobState::Blocked)
            .unwrap();
        store.cancel_submission(&second.submission).unwrap();
        assert!(store.take_attention_reminders("sess").unwrap().is_empty());
    }

    #[test]
    fn delivered_main_results_remain_quiet_while_descendants_drain() {
        let mut store = Store::open_in_memory().unwrap();
        let created = store.create_submission(&sample("make")).unwrap();
        store.set_executor(&created.job, 1, "executor").unwrap();
        store.admit(&created.job, 0, 1).unwrap();
        store.set_started(&created.job, "/logs", None).unwrap();
        store.set_main_result(&created.job, 0).unwrap();
        store.quiet_submission(&created.submission).unwrap();
        assert!(
            store
                .submission(&created.submission)
                .unwrap()
                .unwrap()
                .quiet
        );
        store
            .finish(&created.job, JobState::Completed, Some(0))
            .unwrap();
        assert!(store.take_ended_reminders("sess").unwrap().is_empty());
    }

    fn version_five(path: &std::path::Path) -> (String, String) {
        let conn = Connection::open(path).unwrap();
        for schema in [
            super::super::SCHEMA,
            super::super::BACKGROUND_SCHEMA,
            super::super::processes::SCHEMA,
            super::super::SCHEMA_V5,
        ] {
            conn.execute_batch(schema).unwrap();
        }
        let mut legacy = Store { conn };
        let blocked = legacy.create_submission(&sample("blocked")).unwrap();
        let ended = legacy.create_submission(&sample("ended")).unwrap();
        let draining = legacy.create_submission(&sample("main-result")).unwrap();
        legacy.create_submission(&sample("resumed-queued")).unwrap();
        let resumed = legacy
            .create_submission(&sample("resumed-running"))
            .unwrap();
        legacy
            .conn
            .execute(
                "UPDATE jobs SET state='running' WHERE id=?1",
                params![resumed.job],
            )
            .unwrap();
        legacy
            .conn
            .execute(
                "UPDATE jobs SET state='running', main_exit_code=0 WHERE id=?1",
                params![draining.job],
            )
            .unwrap();
        legacy
            .conn
            .execute(
                "UPDATE jobs SET state='blocked' WHERE id=?1",
                params![blocked.job],
            )
            .unwrap();
        legacy
            .conn
            .execute(
                "UPDATE jobs SET state='completed', exit_code=7 WHERE id=?1",
                params![ended.job],
            )
            .unwrap();
        legacy
            .conn
            .execute("UPDATE submissions SET quiet=1", [])
            .unwrap();
        legacy.conn.pragma_update(None, "user_version", 5).unwrap();
        (blocked.submission, ended.submission)
    }

    #[test]
    fn version_six_repairs_legacy_blocked_delivery_without_repeating_final_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let (blocked, ended) = version_five(&path);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            6
        );
        let row = store.submission(&blocked).unwrap().unwrap();
        assert_eq!(row.job_state, JobState::Blocked);
        assert!(!row.quiet);
        let row = store.submission(&ended).unwrap().unwrap();
        assert!(row.quiet);
        assert_eq!(row.exit_code, Some(7));
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM submissions s JOIN jobs j ON s.job_id=j.id
             WHERE j.command IN ('resumed-queued','resumed-running') AND s.quiet=0",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2,
            "repair also covers work unblocked before upgrade"
        );
        assert_eq!(store.conn.query_row(
            "SELECT s.quiet FROM submissions s JOIN jobs j ON s.job_id=j.id WHERE j.command='main-result'",
            [], |r| r.get::<_, i64>(0)
        ).unwrap(), 1, "migration preserves delivered main results");
        assert_eq!(
            store.take_attention_reminders("sess").unwrap()[0].submission,
            blocked
        );
        assert!(store.take_ended_reminders("sess").unwrap().is_empty());
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert!(store.take_attention_reminders("sess").unwrap().is_empty());
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn failed_version_six_repair_rolls_back_the_column_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let (blocked, _) = version_five(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER refuse_repair BEFORE UPDATE OF quiet ON submissions
             BEGIN SELECT RAISE(ABORT, 'injected repair failure'); END;",
        )
        .unwrap();
        drop(conn);
        assert!(Store::open(&path).is_err());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert!(
            conn.prepare("SELECT attention_state FROM submissions")
                .is_err()
        );
        assert_eq!(
            conn.query_row(
                "SELECT quiet FROM submissions WHERE id=?1",
                params![blocked],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}
