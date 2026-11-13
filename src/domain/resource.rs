//! Resource values shared by planning, policy, persistence and the protocol.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

pub struct QosObligation {
    pub job: Option<String>,
    pub identity: crate::domain::process::ProcessIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "resource", rename_all = "lowercase")]
pub enum Lock {
    Path(String),
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    pub mem: u64,
    pub seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryScope {
    pub id: String,
    pub memory: u64,
    pub used: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LeaseTerms {
    pub parallelism: Option<u32>,
    pub jobserver_tokens: Option<u32>,
    pub env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    Recovering,
    ResourceUnavailable,
    Pressure,
    NoExecutor,
    Dependency { job: String },
    Lock { resource: String, holder: String },
    Memory { need: u64, free: i64 },
    Behind { head: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Admit,
    Wait { reason: Reason },
    Blocked { job: String },
    CapacityInsufficient { need: u64, budget: u64 },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PressureStatus {
    pub pressured: bool,
    pub swap_out_rate: f64,
    pub system_level: u8,
    pub managed: u64,
    pub unmanaged: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistorySample {
    pub peak: Option<u64>,
    pub executor_max_rss: Option<u64>,
    pub seconds: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CargoEnv {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_target_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
}

impl CargoEnv {
    pub fn is_empty(&self) -> bool {
        self.target_dir.is_none() && self.build_target_dir.is_none() && self.home.is_none()
    }
}
