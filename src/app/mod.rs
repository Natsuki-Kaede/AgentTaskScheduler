//! Application services and private coordination state. HTTP is an adapter.
use crate::domain::measurement::Snapshot;
use crate::domain::resource::{PressureStatus, Verdict};
use crate::profile::Profiles;
use crate::store::Store;
use crate::timing::Deadline;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod capacity;
mod jobs;
pub(crate) mod monitor;
pub(crate) mod pressure;
mod queries;
pub(crate) mod recovery;
pub(crate) mod scheduling;
mod submission;

#[derive(Clone)]
pub struct App {
    observations: Arc<Mutex<Option<crate::domain::process::ProcessSnapshot>>>,
    store: Arc<Mutex<Store>>,
    started_at: String,
    instance: String,
    changes: Arc<tokio::sync::watch::Sender<u64>>,
    reconnect: Arc<Mutex<(HashSet<String>, Deadline)>>,
    stopping: Arc<AtomicBool>,
    measurements: Arc<Mutex<Snapshot>>,
    profiles: Arc<Profiles>,
    wrappers: Arc<Vec<String>>,
    awaited: Arc<Mutex<HashMap<String, usize>>>,
    verdicts: Arc<Mutex<HashMap<String, Verdict>>>,
    pressure: Arc<Mutex<PressureStatus>>,
}

pub struct Awaiting {
    awaited: Arc<Mutex<HashMap<String, usize>>>,
    job: String,
}

impl Drop for Awaiting {
    fn drop(&mut self) {
        let mut map = self.awaited.lock().expect("awaited lock");
        if let Some(n) = map.get_mut(&self.job) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.job);
            }
        }
    }
}

impl App {
    pub fn new(store: Store, started_at: String, instance: String) -> Self {
        Self {
            observations: Arc::new(Mutex::new(None)),
            store: Arc::new(Mutex::new(store)),
            started_at,
            instance,
            changes: Arc::new(tokio::sync::watch::channel(0).0),
            reconnect: Arc::new(Mutex::new((HashSet::new(), Deadline::after(None)))),
            stopping: Arc::new(AtomicBool::new(false)),
            measurements: Arc::new(Mutex::new(Snapshot::default())),
            profiles: Arc::new(Profiles::load(None).expect("built-in profiles parse")),
            wrappers: Arc::new(Vec::new()),
            awaited: Arc::new(Mutex::new(HashMap::new())),
            verdicts: Arc::new(Mutex::new(HashMap::new())),
            pressure: Arc::new(Mutex::new(PressureStatus::default())),
        }
    }

    pub fn with_recognition(mut self, profiles: Profiles, wrappers: Vec<String>) -> Self {
        self.profiles = Arc::new(profiles);
        self.wrappers = Arc::new(wrappers);
        self
    }

    fn awaiting(&self, job: &str) -> Awaiting {
        *self
            .awaited
            .lock()
            .expect("awaited lock")
            .entry(job.to_string())
            .or_default() += 1;
        Awaiting {
            awaited: self.awaited.clone(),
            job: job.to_string(),
        }
    }

    fn awaited_jobs(&self) -> HashSet<String> {
        self.awaited
            .lock()
            .expect("awaited lock")
            .keys()
            .cloned()
            .collect()
    }

    pub fn expect_reconnects(&self, jobs: HashSet<String>, grace: Duration) {
        *self.reconnect.lock().expect("reconnect lock") = (jobs, Deadline::after(Some(grace)));
    }

    fn contact(&self, job: &str) {
        self.reconnect.lock().expect("reconnect lock").0.remove(job);
    }

    fn admitting(&self) -> bool {
        if self
            .observations
            .lock()
            .expect("observations lock")
            .is_none()
        {
            return false;
        }
        let r = self.reconnect.lock().expect("reconnect lock");
        r.0.is_empty() || r.1.expired()
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.notify();
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn notify(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().expect("store lock")
    }
}

impl App {
    #[cfg(test)]
    pub(crate) fn test_admit(&self, job: &str, reserved: u64, parallelism: u32) {
        self.store().admit(job, reserved, parallelism).unwrap();
    }
}
