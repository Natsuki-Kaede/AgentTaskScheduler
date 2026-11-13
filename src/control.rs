//! Applies platform-specific background QoS to job processes.

#[cfg(target_os = "linux")]
pub fn set_background(_pid: i32, _on: bool) -> bool {
    false
}

#[cfg(target_os = "macos")]
pub fn set_background(pid: i32, on: bool) -> bool {
    const PRIO_DARWIN_PROCESS: i32 = 4;
    const PRIO_DARWIN_BG: i32 = 0x1000;
    // SAFETY: changes the scheduling policy of a process of this user.
    unsafe {
        libc::setpriority(
            PRIO_DARWIN_PROCESS,
            pid as u32,
            if on { PRIO_DARWIN_BG } else { 0 },
        ) == 0
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn test_priority(pid: i32) -> i32 {
    const PROC_PIDLISTTHREADS: i32 = 6;
    let mut thread = 0u64;
    // SAFETY: the output buffer holds exactly one thread identifier.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDLISTTHREADS,
            0,
            (&mut thread as *mut u64).cast(),
            8,
        )
    };
    assert_eq!(read, 8, "reading test thread for {pid}");
    let mut info = std::mem::MaybeUninit::<libc::proc_threadinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_threadinfo>() as i32;
    // SAFETY: the buffer has the size and alignment required by PROC_PIDTHREADINFO.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTHREADINFO,
            thread,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    assert_eq!(
        read,
        size,
        "reading thread priority for {pid}: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: proc_pidinfo initialized the complete structure.
    unsafe { info.assume_init() }.pth_priority
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn background_qos_lowers_priority_and_reverts() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id();
        let normal = test_priority(pid as i32);
        assert!(set_background(pid as i32, true));
        assert_eq!(test_priority(pid as i32), 4);
        assert!(set_background(pid as i32, false));
        assert_eq!(test_priority(pid as i32), normal);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
