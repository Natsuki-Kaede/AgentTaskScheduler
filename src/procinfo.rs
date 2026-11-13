//! Reads process identities and observations from the operating system.

pub fn start_time(pid: i32) -> Option<String> {
    imp::proc(pid).ok().flatten().map(|p| p.start)
}

pub fn probe(pid: i32) -> std::io::Result<Option<Proc>> {
    imp::proc(pid)
}

pub fn alive(pid: i32, start: &str) -> bool {
    start_time(pid).is_some_and(|s| s == start)
}

#[cfg(test)]
pub fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

pub use crate::domain::process::Process as Proc;
use crate::domain::process::ProcessTable;
use crate::runtime::family::Family;

pub fn refresh(family: &mut Family, pgid: i32) {
    match snapshot() {
        Ok(table) => family.observe(Some(pgid), None, &table),
        Err(e) => tracing::warn!("enumerating job processes: {e}"),
    }
}

pub fn signal(family: &Family, signal: i32) {
    for identity in family.members() {
        if alive(identity.pid, &identity.start) {
            // SAFETY: rechecks an identity confirmed as part of this job.
            unsafe {
                libc::kill(identity.pid, signal);
            }
        }
    }
}

pub fn snapshot() -> std::io::Result<ProcessTable> {
    let mut table = ProcessTable::default();
    for pid in imp::all_pids()? {
        match imp::proc(pid) {
            Ok(Some(process)) => table.processes.push(process),
            Ok(None) => {}
            Err(_) => {
                table.uncertain.insert(pid);
            }
        }
    }
    Ok(table)
}

#[cfg(test)]
pub fn list() -> Vec<Proc> {
    snapshot().map(|t| t.processes).unwrap_or_default()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Proc;

    pub fn proc(pid: i32) -> std::io::Result<Option<Proc>> {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        parse(pid, &stat).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid process stat")
        })
    }

    fn parse(pid: i32, stat: &str) -> Option<Option<Proc>> {
        let fields: Vec<&str> = stat
            .get(stat.rfind(')')? + 1..)?
            .split_whitespace()
            .collect();
        if matches!(fields.first(), Some(&"Z" | &"X")) {
            return Some(None);
        }
        Some(Some(Proc {
            pid,
            ppid: fields.get(1)?.parse().ok()?,
            pgid: fields.get(2)?.parse().ok()?,
            start: fields.get(19)?.to_string(),
        }))
    }

    pub fn all_pids() -> std::io::Result<Vec<i32>> {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            if let Some(pid) = entry?.file_name().to_str().and_then(|s| s.parse().ok()) {
                pids.push(pid);
            }
        }
        Ok(pids)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::mem::{MaybeUninit, size_of};

    use super::Proc;

    pub fn proc(pid: i32) -> std::io::Result<Option<Proc>> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if n != size {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: the call filled the whole struct.
        let i = unsafe { info.assume_init() };
        if i.pbi_status == 5 {
            return Ok(None);
        }
        Ok(Some(Proc {
            pid,
            ppid: i.pbi_ppid as i32,
            pgid: i.pbi_pgid as i32,
            start: format!("{}.{:06}", i.pbi_start_tvsec, i.pbi_start_tvusec),
        }))
    }

    pub fn all_pids() -> std::io::Result<Vec<i32>> {
        // SAFETY: a null buffer asks for the number of processes.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut pids = vec![0i32; n as usize + 64];
        let bytes = (pids.len() * size_of::<i32>()) as i32;
        // SAFETY: the buffer holds `bytes` bytes.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n as usize >= pids.len() {
            return Err(std::io::Error::other(
                "process enumeration exceeded its buffer",
            ));
        }
        pids.truncate(n as usize);
        Ok(pids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_processes_by_start_time() {
        let me = std::process::id() as i32;
        let start = start_time(me).unwrap();
        assert!(alive(me, &start));
        assert!(!alive(me, "0"));

        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let listed = list().into_iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(listed.ppid, me);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(start_time(pid).is_none());
    }
}
