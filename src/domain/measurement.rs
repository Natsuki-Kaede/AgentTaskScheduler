//! System and task measurement values shared with read-only API views.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Whole-machine readings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct System {
    pub mem_total: u64,
    /// Memory in use, as the system counts it (U in design 10.1).
    pub mem_used: u64,
    /// The system's own pressure level: 1 normal, 4 critical (macOS also
    /// reports 2, warning).
    pub pressure: u8,
    /// Bytes swapped out since boot.
    pub swap_out_bytes: u64,
    pub cores: u32,
}

/// One process's private anonymous memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub private_resident: u64,
    /// Compressed or swapped out, at uncompressed size.
    pub private_swapped: u64,
}

/// Memory of a job's processes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobUsage {
    /// Incomplete readings must not lower a running job's reservation.
    #[serde(skip)]
    pub complete: bool,
    pub processes: Vec<i32>,
    /// Private memory resident now (M in design 10.1).
    pub private_resident: u64,
    /// Private memory including compressed and swapped-out parts.
    pub private_total: u64,
    /// The largest `private_total` seen, which a reservation is raised to.
    pub peak_private_total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub at: String,
    pub system: Option<System>,
    /// Bytes swapped out per second since the previous sample.
    pub swap_out_rate: f64,
    pub jobs: BTreeMap<String, JobUsage>,
}
