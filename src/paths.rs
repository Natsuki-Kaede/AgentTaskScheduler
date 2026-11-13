//! Locations of the daemon's files.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub const HOME_ENV: &str = "ATS_HOME";

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub db: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub logs: PathBuf,
    pub config: PathBuf,
    pub installed: PathBuf,
    pub profiles: PathBuf,
    pub temporary: PathBuf,
    pub requests: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        match std::env::var_os(HOME_ENV) {
            Some(home) => {
                let home = PathBuf::from(home);
                let requests = home.join("requests");
                Ok(Self::with(home, requests))
            }
            None => {
                let home = dirs::data_dir()
                    .context("no platform data directory")?
                    .join("ats");
                // SAFETY: geteuid has no preconditions.
                let temporary = temporary_root(&std::env::temp_dir(), unsafe { libc::geteuid() });
                let requests = temporary.join("requests");
                Ok(Self::with(home, requests))
            }
        }
    }

    pub fn with(home: PathBuf, requests: PathBuf) -> Self {
        Self {
            temporary: requests
                .parent()
                .expect("request directory has a parent")
                .to_path_buf(),
            db: home.join("ats.db"),
            socket: home.join("ats.sock"),
            lock: home.join("daemon.lock"),
            logs: home.join("logs"),
            config: home.join("config.toml"),
            installed: home.join("installed.json"),
            profiles: home.join("profiles.d"),
            home,
            requests,
        }
    }
}

fn temporary_root(base: &std::path::Path, uid: u32) -> PathBuf {
    base.join(format!("ats-{uid}"))
}

pub fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_directories_are_separate_for_users_and_instances() {
        let base = PathBuf::from("/tmp");
        let a = temporary_root(&base, 1000);
        let b = temporary_root(&base, 1001);
        assert!(!a.starts_with(&b) && !b.starts_with(&a));
        for home in [base.join("one"), base.join("two")] {
            let paths = Paths::with(home.clone(), home.join("requests"));
            assert_eq!(paths.temporary, home);
            assert!(paths.requests.starts_with(&paths.temporary));
        }
    }
}
