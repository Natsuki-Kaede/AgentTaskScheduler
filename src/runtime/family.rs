use std::collections::{BTreeMap, HashSet};

use crate::domain::process::{ProcessIdentity, ProcessTable};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Family {
    members: BTreeMap<i32, String>,
}

impl Family {
    pub fn from_members(members: impl IntoIterator<Item = ProcessIdentity>) -> Self {
        Self {
            members: members.into_iter().map(|p| (p.pid, p.start)).collect(),
        }
    }

    pub fn members(&self) -> Vec<ProcessIdentity> {
        self.members
            .iter()
            .map(|(&pid, start)| ProcessIdentity {
                pid,
                start: start.clone(),
            })
            .collect()
    }

    pub fn observe(
        &mut self,
        pgid: Option<i32>,
        executor: Option<&ProcessIdentity>,
        table: &ProcessTable,
    ) {
        self.members.retain(|pid, start| {
            table.alive(&ProcessIdentity {
                pid: *pid,
                start: start.clone(),
            })
        });
        let executor = executor.filter(|e| {
            table
                .processes
                .iter()
                .any(|p| p.pid == e.pid && p.start == e.start)
        });
        for p in &table.processes {
            if pgid == Some(p.pgid) || executor.is_some_and(|e| p.ppid == e.pid) {
                let identity = p.identity();
                self.members.insert(identity.pid, identity.start);
            }
        }
        let mut confirmed: HashSet<i32> = table
            .processes
            .iter()
            .filter(|p| self.members.get(&p.pid) == Some(&p.start))
            .map(|p| p.pid)
            .collect();
        loop {
            let mut added = false;
            for p in &table.processes {
                if !self.members.contains_key(&p.pid) && confirmed.contains(&p.ppid) {
                    let identity = p.identity();
                    self.members.insert(identity.pid, identity.start);
                    confirmed.insert(p.pid);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::process::Process;

    #[test]
    fn descendant_discovery_requires_a_confirmed_parent_identity() {
        let child = Process {
            pid: 11,
            ppid: 10,
            pgid: 99,
            start: "child".into(),
        };
        let grandchild = Process {
            pid: 12,
            ppid: 11,
            pgid: 99,
            start: "grandchild".into(),
        };
        for (name, parent, uncertain, expected) in [
            ("unreadable", None, true, vec![10]),
            ("confirmed", Some("old"), false, vec![10, 11, 12]),
            ("reused", Some("new"), false, vec![]),
        ] {
            let mut family = Family::from_members([ProcessIdentity {
                pid: 10,
                start: "old".into(),
            }]);
            let mut table = ProcessTable {
                processes: vec![grandchild.clone(), child.clone()],
                ..Default::default()
            };
            if uncertain {
                table.uncertain.insert(10);
            }
            if let Some(start) = parent {
                table.processes.push(Process {
                    pid: 10,
                    ppid: 1,
                    pgid: 99,
                    start: start.into(),
                });
            }
            family.observe(None, None, &table);
            assert_eq!(
                family.members().iter().map(|p| p.pid).collect::<Vec<_>>(),
                expected,
                "{name} parent"
            );
        }
    }

    #[test]
    fn a_confirmed_child_can_discover_work_while_its_parent_is_unreadable() {
        let mut family = Family::from_members([
            ProcessIdentity {
                pid: 10,
                start: "parent".into(),
            },
            ProcessIdentity {
                pid: 11,
                start: "child".into(),
            },
        ]);
        let table = ProcessTable {
            uncertain: [10].into(),
            processes: vec![
                Process {
                    pid: 11,
                    ppid: 10,
                    pgid: 99,
                    start: "child".into(),
                },
                Process {
                    pid: 12,
                    ppid: 11,
                    pgid: 99,
                    start: "grandchild".into(),
                },
            ],
        };
        family.observe(None, None, &table);
        assert_eq!(
            family.members().iter().map(|p| p.pid).collect::<Vec<_>>(),
            [10, 11, 12]
        );
    }

    #[test]
    fn retains_reparented_members_and_restores_identities_without_reused_pids() {
        let p = |pid, ppid, pgid, start: &str| Process {
            pid,
            ppid,
            pgid,
            start: start.into(),
        };
        let table = |processes| ProcessTable {
            processes,
            ..ProcessTable::default()
        };
        let mut family = Family::default();
        family.observe(
            Some(10),
            None,
            &table(vec![p(10, 1, 10, "a"), p(11, 10, 11, "b")]),
        );
        let saved = family.members();
        let mut restored = Family::from_members(saved);
        restored.observe(
            Some(10),
            None,
            &table(vec![p(11, 1, 11, "b"), p(12, 11, 12, "c")]),
        );
        assert_eq!(
            restored.members().iter().map(|p| p.pid).collect::<Vec<_>>(),
            [11, 12]
        );
        restored.observe(
            Some(10),
            None,
            &table(vec![p(11, 1, 11, "new"), p(12, 1, 12, "c")]),
        );
        assert_eq!(
            restored.members().iter().map(|p| p.pid).collect::<Vec<_>>(),
            [12]
        );
        let mut unreadable = ProcessTable::default();
        unreadable.uncertain.insert(12);
        restored.observe(Some(10), None, &unreadable);
        assert!(
            !restored.is_empty(),
            "an unreadable member is not proof of exit"
        );
        restored.observe(Some(10), None, &ProcessTable::default());
        assert!(restored.is_empty());
    }
}
