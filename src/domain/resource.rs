//! Resource values shared by planning, policy, persistence and the protocol.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// A policy restoration obligation; legacy records may not name their job.
pub struct QosObligation {
    pub job: Option<String>,
    pub identity: crate::domain::process::ProcessIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "resource", rename_all = "lowercase")]
pub enum Lock {
    /// A normalized absolute path.
    Path(String),
    /// A device, port or other named resource.
    Named(String),
}

impl fmt::Display for Lock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Lock::Path(p) | Lock::Named(p) => f.write_str(p),
        }
    }
}

impl Lock {
    pub fn conflicts(&self, other: &Lock) -> bool {
        match (self, other) {
            (Lock::Path(a), Lock::Path(b)) => {
                let (a, b) = (Path::new(a), Path::new(b));
                a.starts_with(b) || b.starts_with(a)
            }
            (Lock::Named(a), Lock::Named(b)) => a == b,
            _ => false,
        }
    }
}
/// What a job is expected to use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    /// Peak memory (R).
    pub mem: u64,
    /// Run time, when every part of the line has history.
    pub seconds: Option<f64>,
}

/// A finite memory budget shared by processes in one execution cgroup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryScope {
    pub id: String,
    pub memory: u64,
    pub used: u64,
}

/// What an executor is given with its lease (design 10.5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LeaseTerms {
    pub parallelism: Option<u32>,
    /// Tokens for the jobserver pool, when the line runs jobserver clients.
    pub jobserver_tokens: Option<u32>,
    /// Variables to set to `parallelism`.
    pub env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    Recovering,
    ResourceUnavailable,
    Pressure,
    NoExecutor,
    Dependency {
        job: String,
    },
    Lock {
        resource: String,
        holder: String,
    },
    Memory {
        need: u64,
        free: i64,
    },
    /// Would delay the job at the head of the queue.
    Behind {
        head: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Admit,
    Wait {
        reason: Reason,
    },
    /// A predecessor failed; `ats unblock` or cancel decides.
    Blocked {
        job: String,
    },
    /// Its predicted peak exceeds the whole budget (design 5.1).
    CapacityInsufficient {
        need: u64,
        budget: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PressureStatus {
    pub pressured: bool,
    /// Swap-out bytes per second, averaged over the last 10 seconds.
    pub swap_out_rate: f64,
    /// The system's own level: 1 normal, 4 critical (macOS also reports 2,
    /// warning).
    pub system_level: u8,
    /// Memory resident in managed jobs and everything else.
    pub managed: u64,
    pub unmanaged: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistorySample {
    pub peak: Option<u64>,
    pub seconds: Option<f64>,
}
