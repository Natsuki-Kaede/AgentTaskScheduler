//! Measured history for prediction: successful jobs from the
//! last 90 days, most recent first.

use std::time::SystemTime;

use anyhow::Result;
use rusqlite::params;

use super::Store;
use crate::domain::resource::HistorySample;

const LIMIT: i64 = 200;

fn seconds_between(start: &str, end: &str) -> Option<f64> {
    let (s, e) = (
        humantime::parse_rfc3339(start).ok()?,
        humantime::parse_rfc3339(end).ok()?,
    );
    Some(e.duration_since(s).ok()?.as_secs_f64())
}

impl Store {
    pub fn history(
        &self,
        project: Option<&str>,
        command: Option<&str>,
        key: Option<&str>,
    ) -> Result<Vec<HistorySample>> {
        let since = humantime::format_rfc3339_millis(SystemTime::now() - super::KEEP).to_string();
        let mut stmt = self.conn.prepare_cached(
            "SELECT peak_private, finish_max_rss, started_at, ended_at FROM jobs
             WHERE state = 'completed' AND exit_code = 0 AND ended_at >= ?1
               AND (?2 IS NULL OR project_root = ?2)
               AND (?3 IS NULL OR command = ?3)
               AND (?4 IS NULL OR history_key = ?4)
             ORDER BY ended_at DESC LIMIT ?5",
        )?;
        let rows = stmt.query_map(params![since, project, command, key, LIMIT], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut samples = Vec::new();
        for row in rows {
            let (peak, executor_max_rss, start, end) = row?;
            samples.push(HistorySample {
                peak: peak.map(|p| p as u64),
                executor_max_rss: executor_max_rss.map(|rss| rss as u64),
                seconds: start.zip(end).and_then(|(s, e)| seconds_between(&s, &e)),
            });
        }
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::predict::Stats;

    fn history(
        store: &Store,
        project: Option<&str>,
        command: Option<&str>,
        key: Option<&str>,
    ) -> Result<Stats> {
        Ok(Stats::from_history(&store.history(project, command, key)?))
    }

    use crate::domain::job::{Entry, NewSubmission, Plan};

    fn run(store: &mut Store, project: &str, command: &str, key: &str, peak: i64, state: &str) {
        let new = NewSubmission {
            entry: Entry::Sh,
            command: command.into(),
            cwd: project.into(),
            project_root: project.into(),
            session_id: None,
            launcher: None,
            plan: Plan {
                history_key: Some(key.into()),
                ..Plan::default()
            },
        };
        let job = store.create_submission(&new).unwrap().job;
        store.set_executor(&job, 1, "t").unwrap();
        store.admit(&job, 0, 1).unwrap();
        store.set_started(&job, "/l", None).unwrap();
        store
            .finish_with_usage(&job, state.parse().unwrap(), Some(0), Some(peak))
            .unwrap();
    }

    #[test]
    fn matches_project_command_and_key() {
        let mut store = Store::open_in_memory().unwrap();
        run(
            &mut store,
            "/a",
            "cargo test",
            "cargo test",
            100,
            "completed",
        );
        run(
            &mut store,
            "/a",
            "cargo test -p x",
            "cargo test",
            200,
            "completed",
        );
        run(
            &mut store,
            "/b",
            "cargo test",
            "cargo test",
            400,
            "completed",
        );
        run(
            &mut store,
            "/a",
            "cargo test",
            "cargo test",
            9999,
            "canceled",
        );

        let line = history(&store, Some("/a"), Some("cargo test"), None).unwrap();
        assert_eq!(
            (line.samples, line.mem),
            (1, Some(130)),
            "canceled runs are not history"
        );
        let project = history(&store, Some("/a"), None, Some("cargo test")).unwrap();
        assert_eq!((project.samples, project.mem), (2, Some(260)));
        let global = history(&store, None, None, Some("cargo test")).unwrap();
        assert_eq!((global.samples, global.mem), (3, Some(520)));
        assert!(global.seconds.is_some());
        assert_eq!(
            history(&store, None, None, Some("make")).unwrap(),
            Stats::default()
        );

        for peak in [10, 20, 30, 40, 50, 60] {
            run(&mut store, "/c", "make", "make", peak, "completed");
        }
        assert_eq!(
            history(&store, Some("/c"), None, Some("make")).unwrap().mem,
            Some(60)
        );
    }

    #[test]
    fn prediction_includes_rss_peaks_missed_by_private_memory_sampling() {
        use crate::domain::job::{FinishReport, JobEvent, JobState};

        let mut store = Store::open_in_memory().unwrap();
        let job = store
            .create_submission(&super::super::tests::sample("make"))
            .unwrap()
            .job;
        store.set_executor(&job, 1, "t").unwrap();
        store.admit(&job, 0, 1).unwrap();
        store.set_started(&job, "/logs", None).unwrap();
        store.record_peaks(&[(job.clone(), 10)]).unwrap();
        store
            .report_finish(
                &job,
                &FinishReport {
                    state: JobState::Completed,
                    exit_code: Some(0),
                    max_rss: Some(100),
                },
            )
            .unwrap();
        let facts = store.recovery_facts(&job).unwrap();
        store
            .recover(&facts, &[], JobEvent::Finish(JobState::Completed), Some(0))
            .unwrap();
        let samples = store.history(None, Some("make"), None).unwrap();
        assert_eq!(samples[0].peak, Some(10));
        assert_eq!(samples[0].executor_max_rss, Some(100));
        assert_eq!(Stats::from_history(&samples).mem, Some(130));
    }
    #[test]
    fn early_failures_do_not_replace_profile_or_successful_history() {
        let mut store = Store::open_in_memory().unwrap();
        run(
            &mut store,
            "/a",
            "cargo build",
            "cargo build",
            100,
            "completed",
        );
        store
            .conn
            .execute("UPDATE jobs SET exit_code = 2", [])
            .unwrap();
        for (project, command, key) in [
            (Some("/a"), Some("cargo build"), None),
            (Some("/a"), None, Some("cargo build")),
            (None, None, Some("cargo build")),
        ] {
            assert_eq!(
                history(&store, project, command, key).unwrap(),
                Stats::default()
            );
        }
        run(
            &mut store,
            "/a",
            "cargo build",
            "cargo build",
            4096,
            "completed",
        );
        let stats = history(&store, Some("/a"), Some("cargo build"), None).unwrap();
        assert_eq!(stats.samples, 1);
        assert_eq!(stats.mem, Some((4096.0 * 1.3) as u64));
    }
}
