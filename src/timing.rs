//! The elapsed clock: time since boot including system sleep, unaffected
//! by wall-clock changes. Bounded waits count sleep (design 5.2).

use std::time::Duration;

// macOS CLOCK_MONOTONIC counts sleep; Linux needs CLOCK_BOOTTIME for that.
#[cfg(target_os = "macos")]
const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
#[cfg(target_os = "linux")]
const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;

fn read(clock: libc::clockid_t) -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the provided timespec.
    let r = unsafe { libc::clock_gettime(clock, &mut ts) };
    assert_eq!(r, 0, "clock_gettime({clock}) failed");
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Time since boot, including sleep.
pub fn elapsed() -> Duration {
    read(CLOCK)
}

/// A point in time measured on the elapsed clock.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Option<Duration>);

impl Deadline {
    /// A deadline `after` from now; `None` never expires.
    pub fn after(after: Option<Duration>) -> Self {
        Deadline(after.map(|d| elapsed() + d))
    }

    /// Time left, or `None` for a deadline that never expires.
    pub fn remaining(&self) -> Option<Duration> {
        self.0.map(|at| at.saturating_sub(elapsed()))
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_some_and(|r| r.is_zero())
    }

    /// Bounds a future with the same sleep-aware deadline as bounded waits.
    /// Periodic checks also notice system sleep while an I/O future is idle.
    pub async fn run<F: std::future::Future>(&self, future: F) -> std::io::Result<F::Output> {
        tokio::pin!(future);
        loop {
            if self.expired() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "request deadline expired",
                ));
            }
            let pause = self
                .remaining()
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(1));
            tokio::select! {
                result = &mut future => return Ok(result),
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }
}

/// Current time as an RFC 3339 string with milliseconds.
pub fn now() -> String {
    humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_advances() {
        let e1 = elapsed();
        std::thread::sleep(Duration::from_millis(20));
        assert!(elapsed() > e1);
    }

    #[test]
    fn deadlines() {
        assert!(Deadline::after(Some(Duration::ZERO)).expired());
        assert!(!Deadline::after(Some(Duration::from_secs(60))).expired());
        assert!(Deadline::after(None).remaining().is_none());
    }
}
