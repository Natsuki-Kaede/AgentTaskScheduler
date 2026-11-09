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
                .cloned(),
        ))
    }

    pub fn running_reminders(&self, session: &str) -> Result<Vec<Reminder>> {
        self.store().running_reminders(session)
    }
    pub fn take_ended_reminders(&self, session: &str) -> Result<Vec<Reminder>> {
        self.store().take_ended_reminders(session)
    }
}
