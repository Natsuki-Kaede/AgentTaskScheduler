use anyhow::Result;

use super::App;
use crate::domain::error::NotFound;
use crate::domain::job::{JobRecord, JobState, Reminder, SubmissionRecord};
use crate::domain::measurement::Snapshot;
use crate::domain::resource::{PressureStatus, Verdict};

impl App {
    pub fn identity(&self) -> (&str, &str) {
        (&self.started_at, &self.instance)
    }

    pub fn job(&self, id: &str) -> Result<JobRecord> {
        self.store()
            .job(id)?
            .ok_or_else(|| NotFound(format!("no job {id}")).into())
    }

    pub fn submission(&self, id: &str) -> Result<SubmissionRecord> {
        self.store()
            .submission(id)?
            .ok_or_else(|| NotFound(format!("no submission {id}")).into())
    }

    pub fn list_jobs(&self, states: &[String], limit: u32) -> Result<Vec<JobRecord>> {
        self.store().list_jobs(states, limit.min(1000))
    }

    pub fn measurements(&self) -> Snapshot {
        self.measurements.lock().expect("measurements lock").clone()
    }
    pub fn pressure(&self) -> PressureStatus {
        self.pressure.lock().expect("pressure lock").clone()
    }

    pub fn why(&self, id: &str) -> Result<(JobState, Option<Verdict>)> {
        let row = self.job(id)?;
        Ok((
            row.state,
            self.verdicts
                .lock()
                .expect("verdicts lock")
                .get(id)
                .filter(|verdict| {
                    !matches!(verdict, Verdict::Blocked { .. }) || row.state == JobState::Blocked
                })
                .cloned(),
        ))
    }

    pub fn running_reminders(&self, session: &str) -> Result<Vec<Reminder>> {
        self.store().running_reminders(session)
    }
    pub fn take_ended_reminders(&self, session: &str) -> Result<Vec<Reminder>> {
        self.store().take_ended_reminders(session)
    }
    pub fn take_attention_reminders(&self, session: &str) -> Result<Vec<Reminder>> {
        self.store().take_attention_reminders(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_hooks_share_one_attention_receipt() {
        let mut store = crate::store::Store::open_in_memory().unwrap();
        let created = store
            .create_submission(&crate::store::tests::sample("make"))
            .unwrap();
        store
            .set_queue_state(&created.job, JobState::Blocked)
            .unwrap();
        let app = App::new(store, "start".into(), "instance".into());
        let barrier = std::sync::Barrier::new(2);
        let delivered: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        app.take_attention_reminders("sess").unwrap().len()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(delivered, 1);
        assert!(!app.submission(&created.submission).unwrap().quiet);
    }
}
