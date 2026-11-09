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
    /// Bumped after every change to jobs or submissions, so that waiting
    /// requests re-check their condition.
    changes: Arc<tokio::sync::watch::Sender<u64>>,
    /// Executors expected to reconnect after a restart, and until when
    /// leases wait for them (design 7.1).
    reconnect: Arc<Mutex<(HashSet<String>, Deadline)>>,
    stopping: Arc<AtomicBool>,
    /// The latest resource measurements (design 9).
    measurements: Arc<Mutex<Snapshot>>,
    profiles: Arc<Profiles>,
    /// Wrapper programs from the configuration (design 8.1).
    wrappers: Arc<Vec<String>>,
    /// Jobs someone is waiting for, with how many waits (design 10.3).
    awaited: Arc<Mutex<HashMap<String, usize>>>,
    /// The scheduler's latest verdict on each waiting job, for `ats why`.
    verdicts: Arc<Mutex<HashMap<String, Verdict>>>,
    /// Memory pressure and what it is doing (design 10.2).
    pressure: Arc<Mutex<PressureStatus>>,
}

/// Counts a wait on a job while it lasts.
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

    /// Uses the user's profiles and wrapper list instead of the built-in
    /// profiles alone.
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

    /// Holds leases until each of these jobs' executors has contacted the
    /// daemon, or until `grace` has passed.
    pub fn expect_reconnects(&self, jobs: HashSet<String>, grace: Duration) {
        *self.reconnect.lock().expect("reconnect lock") = (jobs, Deadline::after(Some(grace)));
    }

    /// Records that a job's executor reached the daemon.
    fn contact(&self, job: &str) {
        self.reconnect.lock().expect("reconnect lock").0.remove(job);
    }

    /// False while executors are still reconnecting after a restart.
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

    /// Ends held requests so the daemon can stop.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.notify();
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Wakes requests waiting for a change.
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
