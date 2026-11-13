//! System and task measurement values shared with read-only API views.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct System {
    pub mem_total: u64,
    pub mem_used: u64,
    pub pressure: u8,
    pub swap_out_bytes: u64,
    pub cores: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub private_resident: u64,
    pub private_swapped: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobUsage {
    #[serde(skip)]
    pub complete: bool,
    pub processes: Vec<i32>,
    pub private_resident: u64,
    pub private_total: u64,
    pub peak_private_total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub at: String,
    pub system: Option<System>,
    pub swap_out_rate: f64,
    pub jobs: BTreeMap<String, JobUsage>,
}
