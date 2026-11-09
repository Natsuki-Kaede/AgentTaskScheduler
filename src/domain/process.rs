//! Stable process identities and observations supplied by the OS adapter.

use std::collections::{BTreeMap, HashSet};

pub type Memberships = BTreeMap<String, Vec<ProcessIdentity>>;

#[derive(Debug, Clone)]
pub struct ProcessRoot {
    pub id: String,
    pub pgid: Option<i32>,
    pub executor: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, Default)]
pub struct ProcessSnapshot {
    pub members: Memberships,
    pub roots: BTreeMap<String, ProcessRoot>,
    pub table: ProcessTable,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub start: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub start: String,
}

impl Process {
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            start: self.start.clone(),
        }
    }
}

/// An enumeration may succeed while individual processes cannot be read.
/// Such identities remain owned until a later observation resolves them.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    pub processes: Vec<Process>,
    pub uncertain: HashSet<i32>,
}

impl ProcessTable {
    pub fn alive(&self, identity: &ProcessIdentity) -> bool {
        self.uncertain.contains(&identity.pid)
            || self
                .processes
                .iter()
                .any(|p| p.pid == identity.pid && p.start == identity.start)
    }
}
