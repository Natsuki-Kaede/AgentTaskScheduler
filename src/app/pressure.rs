//! The pressure response (design 10.2), run after every sample: while
//! pressure is up the scheduler admits nothing, and the processes of
//! running jobs are on background QoS; 30 seconds after it subsides both
//! are undone. ats never freezes, pauses or terminates a job, and never
//! touches processes it does not manage.

use anyhow::Result;
use std::collections::HashSet;

use super::App;
use crate::control;
use crate::domain::measurement::Snapshot;
use crate::domain::process::{Memberships, ProcessSnapshot};
use crate::domain::resource::PressureStatus;
use crate::procinfo;
use crate::sched::pressure::Tracker;
use crate::store::Store;
use crate::timing;

trait Qos {
    fn supported(&self) -> bool {
        true
    }
    fn start(&self, pid: i32) -> std::io::Result<Option<String>>;
    fn set(&self, pid: i32, on: bool) -> bool;
}

struct SystemQos;

impl Qos for SystemQos {
    fn supported(&self) -> bool {
        cfg!(target_os = "macos")
    }
    fn start(&self, pid: i32) -> std::io::Result<Option<String>> {
        procinfo::probe(pid).map(|process| process.map(|p| p.start))
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
    /// Identities already restored in this pressure episode. Rebuilt after restart.
    settled: HashSet<(i32, String)>,
}

impl Response {
    pub fn update(
        &mut self,
        state: &App,
        snapshot: &Snapshot,
        processes: &ProcessSnapshot,
    ) -> Result<()> {
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
        self.apply(&state.store(), &processes.members, pressured, &SystemQos)
    }

    fn apply(
        &mut self,
        store: &Store,
        members: &Memberships,
        pressured: bool,
        qos: &impl Qos,
    ) -> Result<()> {
        if !qos.supported() {
            return Ok(());
        }
        // Associate legacy intents when the committed registry still identifies their job.
        for obligation in store.background_obligations()? {
            if obligation.job.is_none()
                && let Some((job, _)) = members
                    .iter()
                    .find(|(_, ids)| ids.contains(&obligation.identity))
            {
                store.remember_background_for_job(
                    job,
                    obligation.identity.pid,
                    &obligation.identity.start,
                )?;
            }
        }
        let jobs = store.background_jobs()?;
        let mut changed_jobs = HashSet::new();
        if !pressured {
            // Children may inherit policy before their first observation, including the
            // observation where pressure ends. Persist their restoration before acting.
            for job in &jobs {
                for identity in members.get(job).into_iter().flatten() {
                    if !self
                        .settled
                        .contains(&(identity.pid, identity.start.clone()))
                    {
                        store.remember_background_for_job(job, identity.pid, &identity.start)?;
                        changed_jobs.insert(job.clone());
                    }
                }
            }
        }
        for obligation in store.background_obligations()? {
            let (pid, start) = (obligation.identity.pid, obligation.identity.start);
            let actual = match qos.start(pid) {
                Ok(actual) => actual,
                Err(e) => {
                    tracing::warn!(pid, "reading identity for QoS restoration: {e}");
                    continue;
                }
            };
            let alive = actual.as_deref() == Some(start.as_str());
            if !alive || (!pressured && qos.set(pid, false)) {
                store.forget_background(pid, &start)?;
                self.background.remove(&(pid, start.clone()));
                self.settled.insert((pid, start));
                if let Some(job) = obligation.job {
                    changed_jobs.insert(job);
                }
            }
        }
        if pressured {
            self.settled.clear();
            for (job, identities) in members {
                for identity in identities {
                    let (pid, start) = (identity.pid, &identity.start);
                    if qos
                        .start(pid)
                        .is_ok_and(|actual| actual.as_deref() == Some(start.as_str()))
                        && !self.background.contains(&(pid, start.clone()))
                    {
                        store.remember_background_for_job(job, pid, start)?;
                        if qos
                            .start(pid)
                            .is_ok_and(|actual| actual.as_deref() == Some(start.as_str()))
                            && qos.set(pid, true)
                        {
                            self.background.insert((pid, start.clone()));
                        }
                    }
                }
            }
        } else {
            let pending: HashSet<String> = store
                .background_obligations()?
                .into_iter()
                .filter_map(|o| o.job)
                .collect();
            for job in jobs {
                // A later unchanged observation closes the fork-before-restoration gap.
                if !changed_jobs.contains(&job)
                    && !pending.contains(&job)
                    && members
                        .get(&job)
                        .into_iter()
                        .flatten()
                        .all(|id| self.settled.contains(&(id.pid, id.start.clone())))
                {
                    store.forget_background_job(&job)?;
                }
            }
            self.settled.retain(|(pid, start)| {
                members
                    .values()
                    .flatten()
                    .any(|id| id.pid == *pid && id.start == *start)
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::process::ProcessIdentity;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        starts: HashMap<i32, String>,
        unreadable: HashSet<i32>,
        changes: RefCell<Vec<(i32, bool)>>,
        fail_restore: bool,
    }

    impl Qos for Fake {
        fn start(&self, pid: i32) -> std::io::Result<Option<String>> {
            if self.unreadable.contains(&pid) {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "unreadable identity",
                ))
            } else {
                Ok(self.starts.get(&pid).cloned())
            }
        }
        fn set(&self, pid: i32, on: bool) -> bool {
            self.changes.borrow_mut().push((pid, on));
            on || !self.fail_restore
        }
    }

    #[test]
    fn inheritance_restoration_survives_parent_exit_restart_and_new_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let store = Store::open(&path).unwrap();
        let mut fake = Fake {
            starts: [
                (10, "parent".into()),
                (11, "child".into()),
                (12, "grandchild".into()),
                (13, "later".into()),
            ]
            .into(),
            ..Fake::default()
        };
        let ids = |pairs: &[(i32, &str)]| {
            pairs
                .iter()
                .map(|(pid, start)| crate::domain::process::ProcessIdentity {
                    pid: *pid,
                    start: (*start).into(),
                })
                .collect::<Vec<_>>()
        };
        let mut response = Response::default();
        response
            .apply(
                &store,
                &[("j".into(), ids(&[(10, "parent")]))].into(),
                true,
                &fake,
            )
            .unwrap();
        fake.starts.remove(&10);
        fake.changes.borrow_mut().clear();
        response
            .apply(
                &store,
                &[("j".into(), ids(&[(11, "child")]))].into(),
                false,
                &fake,
            )
            .unwrap();
        assert_eq!(*fake.changes.borrow(), [(11, false)]);
        assert_eq!(store.background_jobs().unwrap(), ["j"]);
        drop(response);
        drop(store);

        let store = Store::open(&path).unwrap();
        let mut response = Response::default();
        let current = [("j".into(), ids(&[(11, "child"), (12, "grandchild")]))].into();
        response.apply(&store, &current, false, &fake).unwrap();
        assert!(fake.changes.borrow().contains(&(12, false)));
        assert_eq!(store.background_jobs().unwrap(), ["j"]);
        response.apply(&store, &current, false, &fake).unwrap();
        assert!(store.background_jobs().unwrap().is_empty());
        let changes = fake.changes.borrow().len();
        response
            .apply(
                &store,
                &[(
                    "j".into(),
                    ids(&[(11, "child"), (12, "grandchild"), (13, "later")]),
                )]
                .into(),
                false,
                &fake,
            )
            .unwrap();
        assert_eq!(
            fake.changes.borrow().len(),
            changes,
            "a completed restoration episode must not keep changing new work"
        );
    }

    #[test]
    fn confirmed_identity_loss_discards_obligations_without_changing_qos() {
        for start in [None, Some("reused")] {
            let store = Store::open_in_memory().unwrap();
            store.remember_background(10, "original").unwrap();
            let fake = Fake {
                starts: start.map(|s| (10, s.into())).into_iter().collect(),
                ..Fake::default()
            };
            Response::default()
                .apply(&store, &Memberships::new(), false, &fake)
                .unwrap();
            assert!(store.background_processes().unwrap().is_empty());
            assert!(fake.changes.borrow().is_empty());
        }
    }

    #[test]
    fn identity_read_errors_never_apply_qos_and_preserve_committed_intent() {
        use std::cell::Cell;

        struct FailAt {
            reads: Cell<usize>,
            fail_at: usize,
            changes: RefCell<Vec<bool>>,
        }
        impl Qos for FailAt {
            fn start(&self, _: i32) -> std::io::Result<Option<String>> {
                let read = self.reads.get() + 1;
                self.reads.set(read);
                if read == self.fail_at {
                    Err(std::io::Error::other("identity read failed"))
                } else {
                    Ok(Some("worker".into()))
                }
            }
            fn set(&self, _: i32, on: bool) -> bool {
                self.changes.borrow_mut().push(on);
                true
            }
        }
        for fail_at in [1, 2] {
            let store = Store::open_in_memory().unwrap();
            let qos = FailAt {
                reads: Cell::new(0),
                fail_at,
                changes: RefCell::new(vec![]),
            };
            let members = [(
                "j".into(),
                vec![ProcessIdentity {
                    pid: 10,
                    start: "worker".into(),
                }],
            )]
            .into();
            let mut response = Response::default();
            response.apply(&store, &members, true, &qos).unwrap();
            assert!(qos.changes.borrow().is_empty());
            let expected = if fail_at == 2 {
                vec![(10, "worker".into())]
            } else {
                vec![]
            };
            assert_eq!(store.background_processes().unwrap(), expected);
            response
                .apply(&store, &Memberships::new(), false, &qos)
                .unwrap();
            assert!(store.background_processes().unwrap().is_empty());
            assert_eq!(
                *qos.changes.borrow(),
                if fail_at == 2 { vec![false] } else { vec![] }
            );
        }
    }

    #[test]
    fn unreadable_identities_keep_restoration_pending_without_blocking_other_processes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let store = Store::open(&path).unwrap();
        let mut fake = Fake {
            starts: [(10, "a".into()), (11, "b".into())].into(),
            ..Fake::default()
        };
        let members = [(
            "j".into(),
            vec![
                ProcessIdentity {
                    pid: 10,
                    start: "a".into(),
                },
                ProcessIdentity {
                    pid: 11,
                    start: "b".into(),
                },
            ],
        )]
        .into();
        let mut response = Response::default();
        response.apply(&store, &members, true, &fake).unwrap();
        fake.unreadable.insert(10);
        fake.changes.borrow_mut().clear();
        response
            .apply(&store, &Memberships::new(), false, &fake)
            .unwrap();
        assert_eq!(*fake.changes.borrow(), [(11, false)]);
        assert_eq!(store.background_processes().unwrap(), [(10, "a".into())]);
        drop(response);
        drop(store);

        let store = Store::open(&path).unwrap();
        fake.unreadable.clear();
        Response::default()
            .apply(&store, &Memberships::new(), false, &fake)
            .unwrap();
        assert_eq!(*fake.changes.borrow(), [(11, false), (10, false)]);
        assert!(store.background_processes().unwrap().is_empty());
    }

    #[test]
    fn a_reused_member_pid_is_not_put_on_background_qos() {
        let store = Store::open_in_memory().unwrap();
        let fake = Fake {
            starts: [(10, "new-process".into())].into(),
            ..Fake::default()
        };
        let members: Memberships = [(
            "j".into(),
            vec![ProcessIdentity {
                pid: 10,
                start: "original-worker".into(),
            }],
        )]
        .into();
        Response::default()
            .apply(&store, &members, true, &fake)
            .unwrap();
        assert!(fake.changes.borrow().is_empty());
        assert!(store.background_processes().unwrap().is_empty());
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
        let members: Memberships = [(
            "j".into(),
            vec![
                ProcessIdentity {
                    pid: 10,
                    start: "a".into(),
                },
                ProcessIdentity {
                    pid: 11,
                    start: "b".into(),
                },
            ],
        )]
        .into();
        let mut response = Response::default();
        response.apply(&store, &members, true, &fake).unwrap();
        assert_eq!(*fake.changes.borrow(), [(10, true), (11, true)]);
        assert_eq!(store.background_processes().unwrap().len(), 2);
        response.apply(&store, &members, true, &fake).unwrap();
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
            .apply(&store, &Memberships::new(), false, &fake)
            .unwrap();
        assert_eq!(
            *fake.changes.borrow(),
            [(10, false)],
            "no reused or unrelated process changed"
        );
        assert_eq!(store.background_processes().unwrap(), [(10, "a".into())]);
        fake.fail_restore = false;
        restarted
            .apply(&store, &Memberships::new(), false, &fake)
            .unwrap();
        assert!(store.background_processes().unwrap().is_empty());
        assert_eq!(*fake.changes.borrow(), [(10, false), (10, false)]);
    }

    #[test]
    fn intent_is_committed_before_changing_qos() {
        struct Check<'a>(&'a Store);
        impl Qos for Check<'_> {
            fn start(&self, _: i32) -> std::io::Result<Option<String>> {
                Ok(Some("a".into()))
            }
            fn set(&self, pid: i32, on: bool) -> bool {
                assert!(on);
                assert_eq!(self.0.background_processes().unwrap(), [(pid, "a".into())]);
                true
            }
        }
        let store = Store::open_in_memory().unwrap();
        let members: Memberships = [(
            "j".into(),
            vec![ProcessIdentity {
                pid: 10,
                start: "a".into(),
            }],
        )]
        .into();
        Response::default()
            .apply(&store, &members, true, &Check(&store))
            .unwrap();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod inherited_tests {
    use super::*;
    use crate::domain::process::ProcessIdentity;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};
    struct Owned {
        parent: Child,
        child: Option<i32>,
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            if let Some(pid) = self.child.take() {
                // SAFETY: this is the sleeping child created by the test's parent.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = self.parent.kill();
            let _ = self.parent.wait();
        }
    }
    use crate::control::test_priority as priority;

    #[test]
    fn background_batch_restores_all_processes_and_records_latency() {
        struct Batch(Vec<Child>);
        impl Drop for Batch {
            fn drop(&mut self) {
                for child in &mut self.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut owned = Batch(Vec::new());
        let mut identities = Vec::new();
        for _ in 0..16 {
            owned
                .0
                .push(Command::new("sleep").arg("30").spawn().unwrap());
            let pid = owned.0.last().unwrap().id() as i32;
            assert!(crate::control::set_background(pid, false));
            identities.push(ProcessIdentity {
                pid,
                start: procinfo::probe(pid).unwrap().unwrap().start,
            });
        }
        let members: Memberships = [("batch".into(), identities.clone())].into();
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("ats.db")).unwrap();
        let mut response = Response::default();
        let started = Instant::now();
        response.apply(&store, &members, true, &SystemQos).unwrap();
        let background = started.elapsed();
        for identity in &identities {
            assert_eq!(priority(identity.pid), 4);
        }
        let started = Instant::now();
        response.apply(&store, &members, false, &SystemQos).unwrap();
        let restored = started.elapsed();
        for identity in &identities {
            assert_ne!(priority(identity.pid), 4);
        }
        assert!(store.background_obligations().unwrap().is_empty());
        response.apply(&store, &members, false, &SystemQos).unwrap();
        assert!(store.background_jobs().unwrap().is_empty());
        eprintln!(
            "QoS batch: processes={}; background_ms={:.3}; restore_ms={:.3}",
            identities.len(),
            background.as_secs_f64() * 1000.0,
            restored.as_secs_f64() * 1000.0,
        );
    }

    #[test]
    fn inherited_background_qos_is_restored_for_newly_observed_children() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("fork");
        let marker = dir.path().join("child.pid");
        let ready = dir.path().join("ready");
        let script = "import os,pathlib,sys,time\nflag=pathlib.Path(sys.argv[1]); marker=pathlib.Path(sys.argv[2]); pathlib.Path(sys.argv[3]).touch()\nwhile not flag.exists(): time.sleep(.02)\npid=os.fork()\nif pid==0:\n time.sleep(60); os._exit(0)\nmarker.write_text(str(pid))\ntime.sleep(60)";
        let parent = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&flag)
            .arg(&marker)
            .arg(&ready)
            .spawn()
            .unwrap();
        let mut owned = Owned {
            parent,
            child: None,
        };
        // Runtime startup can reset background policy. Apply it only after
        // Python has finished starting and is waiting for the fork signal.
        let started = Instant::now();
        while !ready.exists() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = owned.parent.id() as i32;
        assert!(crate::control::set_background(pid, false));
        let baseline = priority(pid);
        let parent_identity = ProcessIdentity {
            pid,
            start: procinfo::probe(pid).unwrap().unwrap().start,
        };
        let db = dir.path().join("ats.db");
        let store = Store::open(&db).unwrap();
        let mut response = Response::default();
        response
            .apply(
                &store,
                &[("j".into(), vec![parent_identity.clone()])].into(),
                true,
                &SystemQos,
            )
            .unwrap();
        assert_eq!(priority(pid), 4);
        std::fs::write(&flag, "fork").unwrap();
        let started = Instant::now();
        let child = loop {
            if let Ok(raw) = std::fs::read_to_string(&marker)
                && let Ok(pid) = raw.parse::<i32>()
            {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        };
        owned.child = Some(child);
        let inherited = priority(child);
        assert_eq!(
            inherited, 4,
            "the test child must inherit its parent's background policy"
        );
        let child_identity = ProcessIdentity {
            pid: child,
            start: procinfo::probe(child).unwrap().unwrap().start,
        };
        drop(response);
        drop(store);
        let store = Store::open(&db).unwrap();
        let mut response = Response::default();
        let members = [("j".into(), vec![parent_identity, child_identity])].into();
        response.apply(&store, &members, false, &SystemQos).unwrap();
        let after = priority(child);
        eprintln!(
            "inherited QoS: parent_baseline={baseline}; child_before_restore={inherited}; parent_after_restore={}; child_after_restore={after}; obligations={:?}",
            priority(pid),
            store.background_processes().unwrap()
        );
        assert_ne!(priority(pid), 4);
        assert_ne!(
            after, 4,
            "a newly confirmed child must not retain inherited background QoS after pressure subsides"
        );
        assert_eq!(store.background_jobs().unwrap(), ["j"]);
        response.apply(&store, &members, false, &SystemQos).unwrap();
        assert!(store.background_jobs().unwrap().is_empty());
    }
}
