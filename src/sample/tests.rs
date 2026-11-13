use super::*;
use crate::domain::process::{ProcessRoot, ProcessTable};
use crate::runtime::registry::Registry;

#[test]
fn samples_a_running_child() {
    let mut sampler = Sampler::default();
    let mut child = std::process::Command::new("perl")
        .args(["-e", "$x = 'a' x (64 << 20); sleep 5"])
        .spawn()
        .unwrap();
    let mut registry = Registry::restore(Default::default());
    let mut snapshot = Snapshot::default();
    for _ in 0..20 {
        let executor = ProcessIdentity {
            pid: std::process::id() as i32,
            start: procinfo::start_time(std::process::id() as i32).unwrap(),
        };
        let observation = registry.observe(
            vec![ProcessRoot {
                id: "j".into(),
                executor: Some(executor),
                pgid: None,
                leader: None,
                launched: None,
            }],
            procinfo::snapshot().unwrap(),
        );
        registry.accept(&observation);
        snapshot = sampler.sample(&observation);
        if snapshot.jobs["j"].peak_private_total > 64 << 20 {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let usage = &snapshot.jobs["j"];
    assert!(usage.processes.contains(&(child.id() as i32)));
    assert!(usage.peak_private_total > 64 << 20, "{usage:?}");
    assert!(snapshot.system.unwrap().mem_total > 0);
}

#[test]
fn measurement_uses_retained_members_and_preserves_unknown_readings() {
    let identity = ProcessIdentity {
        pid: 20,
        start: "worker".into(),
    };
    let mut observation = ProcessSnapshot {
        members: [("j".into(), vec![identity.clone()])].into(),
        table: ProcessTable::default(),
        ..Default::default()
    };
    let mut sampler = Sampler::default();
    let first = sampler.sample_with(&observation, None, |_| {
        Some(Memory {
            private_resident: 100,
            private_swapped: 0,
        })
    });
    assert_eq!(first.jobs["j"].private_resident, 100);
    sampler.ticks = MEMORY_EVERY;
    let grown = sampler.sample_with(&observation, None, |_| {
        Some(Memory {
            private_resident: 400,
            private_swapped: 0,
        })
    });
    assert_eq!(grown.jobs["j"].peak_private_total, 400);
    observation.table.uncertain.insert(identity.pid);
    sampler.ticks = MEMORY_EVERY;
    let unknown = sampler.sample_with(&observation, None, |_| None);
    assert_eq!(unknown.jobs["j"].private_resident, 400);
    assert!(!unknown.jobs["j"].complete);
}
