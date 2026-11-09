//! Measurement (design 9): system memory and swap-out rate every second,
//! and each job's private anonymous memory. Per-process readings cost up to
//! a few milliseconds (a region walk on macOS) and run every five seconds
//! and for new processes.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::domain::process::{ProcessIdentity, ProcessSnapshot};
use crate::procinfo;
use crate::timing;

pub use os::system;

pub use crate::domain::measurement::{JobUsage, Memory, Snapshot, System};

/// Cores this process may use: the CPU affinity mask and, on Linux, the
/// cgroup CPU quota count.
fn cores() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

/// Per-process memory is read this often; new processes are read at once.
const MEMORY_EVERY: u64 = 5;

#[derive(Default)]
pub struct Sampler {
    memory: HashMap<ProcessIdentity, Memory>,
    /// The peak of each job so far.
    peaks: HashMap<String, u64>,
    /// The previous swap-out count and when it was read.
    last: Option<(u64, Duration)>,
    ticks: u64,
}

impl Sampler {
    pub fn sample(&mut self, processes: &ProcessSnapshot) -> Snapshot {
        self.sample_with(processes, system(), |identity| {
            procinfo::alive(identity.pid, &identity.start)
                .then(|| os::memory(identity.pid))
                .flatten()
        })
    }

    fn sample_with(
        &mut self,
        processes: &ProcessSnapshot,
        system: Option<System>,
        mut memory: impl FnMut(&ProcessIdentity) -> Option<Memory>,
    ) -> Snapshot {
        let now = timing::elapsed();
        let mut swap_out_rate = 0.0;
        if let Some(s) = &system {
            if let Some((before, at)) = self.last {
                let secs = now.saturating_sub(at).as_secs_f64();
                if secs > 0.0 {
                    swap_out_rate = s.swap_out_bytes.saturating_sub(before) as f64 / secs;
                }
            }
            self.last = Some((s.swap_out_bytes, now));
        }

        self.memory.retain(|identity, _| {
            processes
                .members
                .values()
                .any(|members| members.contains(identity))
        });
        let walk_all = self.ticks.is_multiple_of(MEMORY_EVERY);
        self.ticks += 1;

        let mut snapshot = Snapshot {
            at: crate::timing::now(),
            system,
            swap_out_rate,
            jobs: BTreeMap::new(),
        };
        for (job, members) in &processes.members {
            let mut u = JobUsage {
                processes: members.iter().map(|p| p.pid).collect(),
                complete: true,
                ..JobUsage::default()
            };
            for key in members {
                if processes.table.uncertain.contains(&key.pid) {
                    u.complete = false;
                }
                if walk_all || !self.memory.contains_key(key) {
                    match memory(key) {
                        Some(m) => {
                            self.memory.insert(key.clone(), m);
                        }
                        None => u.complete = false,
                    }
                }
                if let Some(m) = self.memory.get(key) {
                    u.private_resident += m.private_resident;
                    u.private_total += m.private_resident + m.private_swapped;
                }
            }
            let peak = self.peaks.entry(job.clone()).or_default();
            *peak = (*peak).max(u.private_total);
            u.peak_private_total = *peak;
            snapshot.jobs.insert(job.clone(), u);
        }
        self.peaks.retain(|job, _| snapshot.jobs.contains_key(job));
        snapshot
    }
}

#[cfg(test)]
mod tests;
