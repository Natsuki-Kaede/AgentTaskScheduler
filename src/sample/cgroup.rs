//! Resolve the execution process's cgroup v2 memory limits, including ancestors.
//! Paths in /proc/PID/cgroup are relative to the reader's cgroup namespace.

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::domain::resource::MemoryScope;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn mount_path(word: &str) -> PathBuf {
    // mountinfo escapes spaces, tabs, newlines and backslashes as octal bytes.
    PathBuf::from(
        word.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\"),
    )
}

fn location(membership: &str, mounts: &str) -> io::Result<(PathBuf, PathBuf)> {
    let group = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| invalid("cgroup v2 membership is unavailable"))?;
    let group = Path::new(group);
    if !group.is_absolute()
        || group
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(invalid("execution cgroup is outside the visible namespace"));
    }
    let mut candidates = Vec::new();
    for line in mounts.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        if after.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let fields: Vec<_> = before.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let root = mount_path(fields[3]);
        let mount = mount_path(fields[4]);
        if let Ok(relative) = group.strip_prefix(&root) {
            candidates.push((root.components().count(), mount.join(relative), mount));
        }
    }
    // Prefer the broadest mount so ancestor restrictions remain visible.
    candidates.sort_by_key(|c| c.0);
    candidates
        .into_iter()
        .next()
        .map(|(_, group, mount)| (group, mount))
        .ok_or_else(|| invalid("execution cgroup has no visible cgroup2 mount"))
}

fn limits(group: &Path, mount: &Path) -> io::Result<Vec<MemoryScope>> {
    let mut scopes = Vec::new();
    for directory in group.ancestors() {
        // Missing controllers can be inherited from an ancestor. Missing groups
        // themselves or unreadable limit files must not imply unlimited capacity.
        if !directory.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "execution cgroup disappeared",
            ));
        }
        match std::fs::read_to_string(directory.join("memory.max")) {
            Ok(limit) if limit.trim() == "max" => {}
            Ok(limit) => {
                let memory = limit
                    .trim()
                    .parse()
                    .map_err(|_| invalid("invalid memory.max"))?;
                let used = std::fs::read_to_string(directory.join("memory.current"))?
                    .trim()
                    .parse()
                    .map_err(|_| invalid("invalid memory.current"))?;
                scopes.push(MemoryScope {
                    id: directory.to_string_lossy().into_owned(),
                    memory,
                    used,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if directory == mount {
            return Ok(scopes);
        }
    }
    Err(invalid("execution cgroup is outside its mount"))
}

pub fn memory_scopes(pid: i32) -> io::Result<Vec<MemoryScope>> {
    let membership = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    let (group, mount) = location(&membership, &mounts)?;
    let scopes = limits(&group, &mount)?;
    if membership != std::fs::read_to_string(format!("/proc/{pid}/cgroup"))? {
        return Err(invalid("execution cgroup changed during observation"));
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_membership_and_mount_roots_without_assuming_a_mountpoint() {
        for (member, mount, expected) in [
            (
                "0::/user.slice/job\n",
                "1 0 0:1 / /sys/fs/cgroup rw - cgroup2 cgroup rw",
                "/sys/fs/cgroup/user.slice/job",
            ),
            (
                "0::/container/job\n",
                "1 0 0:1 /container /cg\\040mount rw - cgroup2 cgroup rw",
                "/cg mount/job",
            ),
            (
                "0::/\n",
                "1 0 0:1 / /sys/fs/cgroup rw - cgroup2 cgroup rw",
                "/sys/fs/cgroup/",
            ),
        ] {
            assert_eq!(location(member, mount).unwrap().0, Path::new(expected));
        }
        for member in ["0::/../outside", "1:memory:/legacy", "0::relative"] {
            assert!(location(member, "1 0 0:1 / /cg rw - cgroup2 cgroup rw").is_err());
        }
    }

    #[test]
    fn reads_every_ancestor_and_rejects_incomplete_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        for (path, limit, used) in [(&parent, "1000", "800"), (&child, "600", "100")] {
            std::fs::write(path.join("memory.max"), limit).unwrap();
            std::fs::write(path.join("memory.current"), used).unwrap();
        }
        let found = limits(&child, dir.path()).unwrap();
        assert_eq!(
            found.iter().map(|s| (s.memory, s.used)).collect::<Vec<_>>(),
            [(600, 100), (1000, 800)]
        );
        std::fs::write(child.join("memory.max"), "max").unwrap();
        assert_eq!(limits(&child, dir.path()).unwrap().len(), 1);
        std::fs::remove_file(parent.join("memory.current")).unwrap();
        assert!(limits(&child, dir.path()).is_err());
        assert!(limits(&child.join("missing"), dir.path()).is_err());
    }
}
