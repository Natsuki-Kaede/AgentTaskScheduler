use super::family::Family;
use crate::domain::process::{Memberships, ProcessRoot, ProcessSnapshot, ProcessTable};

pub struct Registry {
    members: Memberships,
}

impl Registry {
    pub fn restore(members: Memberships) -> Self {
        Self { members }
    }

    pub fn observe(&self, roots: Vec<ProcessRoot>, table: ProcessTable) -> ProcessSnapshot {
        let mut members = Memberships::new();
        for root in &roots {
            let mut family =
                Family::from_members(self.members.get(&root.id).cloned().unwrap_or_default());
            family.observe(root.pgid, root.executor.as_ref(), &table);
            members.insert(root.id.clone(), family.members());
        }
        ProcessSnapshot {
            members,
            roots: roots.into_iter().map(|r| (r.id.clone(), r)).collect(),
            table,
        }
    }

    pub fn accept(&mut self, observation: &ProcessSnapshot) {
        self.members = observation.members.clone();
    }
}
