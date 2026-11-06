//! The pressure response (design 10.2), run after every sample: while
//! pressure is up the scheduler admits nothing, and the processes of
//! running jobs are on background QoS; 30 seconds after it subsides both
//! are undone. ats never freezes, pauses or terminates a job, and never
//! touches processes it does not manage.

use anyhow::Result;
use std::collections::HashSet;

use crate::api::AppState;
use crate::control;
use crate::procinfo;
use crate::sample::Snapshot;
use crate::sched::pressure::{PressureStatus, Tracker};
use crate::store::Store;
use crate::timing;

trait Qos {
    fn supported(&self) -> bool {
        true
    }
    fn start(&self, pid: i32) -> Option<String>;
    fn set(&self, pid: i32, on: bool) -> bool;
}

struct SystemQos;

impl Qos for SystemQos {
    fn supported(&self) -> bool {
        cfg!(target_os = "macos")
    }
    fn start(&self, pid: i32) -> Option<String> {
        procinfo::start_time(pid)
    }
    fn set(&self, pid: i32, on: bool) -> bool {
        control::set_background(pid, on)
    }
}

#[derive(Default)]
pub struct Response {
    tracker: Tracker,
    /// Processes put on background QoS, as (pid, start time).
    background: HashSet<(i32, String)>,
}

impl Response {
    pub fn update(&mut self, state: &AppState, snapshot: &Snapshot) -> Result<()> {
        let Some(system) = &snapshot.system else {
            return Ok(());
        };
        let now = timing::elapsed().as_secs_f64();
        let (swap_out_rate, pressured) =
            self.tracker
                .update(snapshot.swap_out_rate, system.pressure, now);
        let was = state.pressure.lock().expect("pressure lock").pressured;
        if pressured != was {
            tracing::warn!(
                pressured,
                swap_out_rate,
                system_level = system.pressure,
                "memory pressure"
            );
        }
        let managed: u64 = snapshot.jobs.values().map(|u| u.private_resident).sum();
        *state.pressure.lock().expect("pressure lock") = PressureStatus {
            pressured,
            swap_out_rate,
            system_level: system.pressure,
            managed,
            unmanaged: system.mem_used.saturating_sub(managed),
        };
        if pressured != was {
            state.notify();
        }
        self.apply(&state.store(), snapshot, pressured, &SystemQos)
    }

    fn apply(
        &mut self,
        store: &Store,
        snapshot: &Snapshot,
        pressured: bool,
        qos: &impl Qos,
    ) -> Result<()> {
        if !qos.supported() {
            return Ok(());
        }
        for (pid, start) in store.background_processes()? {
            let alive = qos.start(pid).as_deref() == Some(start.as_str());
            if !alive || (!pressured && qos.set(pid, false)) {
                store.forget_background(pid, &start)?;
                self.background.remove(&(pid, start));
            }
        }
        if pressured {
            for pid in snapshot.jobs.values().flat_map(|u| &u.processes) {
                if let Some(start) = qos.start(*pid)
                    && !self.background.contains(&(*pid, start.clone()))
                {
                    store.remember_background(*pid, &start)?;
                    if qos.start(*pid).as_deref() == Some(start.as_str()) && qos.set(*pid, true) {
                        self.background.insert((*pid, start));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::JobUsage;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        starts: HashMap<i32, String>,
        changes: RefCell<Vec<(i32, bool)>>,
        fail_restore: bool,
    }

    impl Qos for Fake {
        fn start(&self, pid: i32) -> Option<String> {
            self.starts.get(&pid).cloned()
        }
        fn set(&self, pid: i32, on: bool) -> bool {
            self.changes.borrow_mut().push((pid, on));
            on || !self.fail_restore
        }
    }

    #[test]
    fn restores_owned_identities_after_restart_and_retries_failures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let store = Store::open(&path).unwrap();
        let mut fake = Fake {
            starts: [(10, "a".into()), (11, "b".into()), (12, "c".into())].into(),
            ..Fake::default()
        };
        let snapshot = Snapshot {
            jobs: [(
                "j".into(),
                JobUsage {
                    processes: vec![10, 11],
                    ..JobUsage::default()
                },
            )]
            .into(),
            ..Snapshot::default()
        };
        let mut response = Response::default();
        response.apply(&store, &snapshot, true, &fake).unwrap();
        assert_eq!(*fake.changes.borrow(), [(10, true), (11, true)]);
        assert_eq!(store.background_processes().unwrap().len(), 2);
        response.apply(&store, &snapshot, true, &fake).unwrap();
        assert_eq!(
            fake.changes.borrow().len(),
            2,
            "already applied in this daemon"
        );
        drop(response);
        drop(store);

        let store = Store::open(&path).unwrap();
        let mut restarted = Response::default();
        fake.starts.insert(11, "reused".into());
        fake.changes.borrow_mut().clear();
        fake.fail_restore = true;
        restarted
            .apply(&store, &Snapshot::default(), false, &fake)
            .unwrap();
        assert_eq!(
            *fake.changes.borrow(),
            [(10, false)],
            "no reused or unrelated process changed"
        );
        assert_eq!(store.background_processes().unwrap(), [(10, "a".into())]);
        fake.fail_restore = false;
        restarted
            .apply(&store, &Snapshot::default(), false, &fake)
            .unwrap();
        assert!(store.background_processes().unwrap().is_empty());
        assert_eq!(*fake.changes.borrow(), [(10, false), (10, false)]);
    }

    #[test]
    fn intent_is_committed_before_changing_qos() {
        struct Check<'a>(&'a Store);
        impl Qos for Check<'_> {
            fn start(&self, _: i32) -> Option<String> {
                Some("a".into())
            }
            fn set(&self, pid: i32, on: bool) -> bool {
                assert!(on);
                assert_eq!(self.0.background_processes().unwrap(), [(pid, "a".into())]);
                true
            }
        }
        let store = Store::open_in_memory().unwrap();
        let snapshot = Snapshot {
            jobs: [(
                "j".into(),
                JobUsage {
                    processes: vec![10],
                    ..JobUsage::default()
                },
            )]
            .into(),
            ..Snapshot::default()
        };
        Response::default()
            .apply(&store, &snapshot, true, &Check(&store))
            .unwrap();
    }
}
