//! Linux readings (design 9) from `/proc` and the cgroup file system. None
//! of them needs root.

use std::path::Path;

use super::{Memory, System};

/// Values of `Key: value` lines, as in `/proc/meminfo`, in their unit.
fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// Values of `key value` lines, as in `/proc/vmstat` and `memory.stat`.
fn stat(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok())?
    })
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// 4 (critical) while tasks stall on memory, all at once, for 10% of the
/// last 10 seconds; 1 otherwise or without pressure stall information.
fn pressure() -> u8 {
    let full = read("/proc/pressure/memory").and_then(|t| {
        let line = t.lines().find(|l| l.starts_with("full "))?;
        line.split_whitespace()
            .find_map(|w| w.strip_prefix("avg10="))?
            .parse::<f64>()
            .ok()
    });
    if full.is_some_and(|f| f >= 10.0) {
        4
    } else {
        1
    }
}

fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64
}

pub fn system() -> Option<System> {
    let meminfo = read("/proc/meminfo")?;
    let total = field(&meminfo, "MemTotal")? * 1024;
    let used = total.saturating_sub(field(&meminfo, "MemAvailable")? * 1024);
    let swapped = read("/proc/vmstat")
        .and_then(|t| stat(&t, "pswpout"))
        .unwrap_or(0);
    Some(System {
        // Execution cgroups are checked separately per executor at admission.
        // The daemon's own cgroup is not necessarily the command's resource domain.
        mem_total: total,
        mem_used: used,
        pressure: pressure(),
        swap_out_bytes: swapped * page_size(),
        cores: super::cores(),
    })
}

/// Resident and swapped anonymous memory, without shared memory and files
/// (design 10.1).
pub fn memory(pid: i32) -> Option<Memory> {
    let status = read(format!("/proc/{pid}/status"))?;
    Some(Memory {
        private_resident: field(&status, "RssAnon")? * 1024,
        private_swapped: field(&status, "VmSwap").unwrap_or(0) * 1024,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_files() {
        let meminfo = "MemTotal:       16318352 kB\nMemAvailable:    9876543 kB\n";
        assert_eq!(field(meminfo, "MemTotal"), Some(16318352));
        assert_eq!(field(meminfo, "Mem"), None);
        assert_eq!(stat("pswpin 3\npswpout 42\n", "pswpout"), Some(42));
    }

    #[test]
    fn reads_this_machine_and_this_process() {
        let s = system().unwrap();
        assert!(s.mem_total > 0 && s.mem_used > 0 && s.cores > 0);
        assert!(
            [1, 4].contains(&s.pressure),
            "pressure level {}",
            s.pressure
        );

        // 64 MB of touched anonymous memory shows up.
        let me = std::process::id() as i32;
        let base = memory(me).unwrap();
        let block = vec![1u8; 64 << 20];
        let grown = memory(me).unwrap();
        std::hint::black_box(&block);
        let delta = (grown.private_resident + grown.private_swapped) as i64
            - (base.private_resident + base.private_swapped) as i64;
        assert!(delta > 48 << 20, "private grew by {delta}");
    }
}
