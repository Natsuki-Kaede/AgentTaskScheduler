//! Execution-domain observations for admission, separate from pure policy.

use crate::domain::process::ProcessIdentity;
use crate::domain::resource::MemoryScope;

pub(super) fn waiting(executor: Option<&(i64, String)>) -> Option<Vec<MemoryScope>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = executor;
        Some(Vec::new())
    }
    #[cfg(target_os = "linux")]
    {
        let Some((pid, start)) = executor else {
            return Some(Vec::new());
        };
        let identity = ProcessIdentity {
            pid: *pid as i32,
            start: start.clone(),
        };
        match observe(&identity) {
            Ok(scopes) => scopes,
            Err(error) => {
                tracing::warn!(pid, "reading execution memory limits: {error}");
                None
            }
        }
    }
}

pub(super) fn held(
    executor: Option<&(i64, String)>,
    members: &[ProcessIdentity],
) -> std::io::Result<Vec<(String, u64)>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (executor, members);
        Ok(Vec::new())
    }
    #[cfg(target_os = "linux")]
    {
        let mut identities = members.to_vec();
        if let Some((pid, start)) = executor {
            identities.push(ProcessIdentity {
                pid: *pid as i32,
                start: start.clone(),
            });
        }
        identities.sort();
        identities.dedup();
        let mut scopes = std::collections::BTreeMap::<String, u64>::new();
        for identity in identities {
            let Some(limits) = observe(&identity)? else {
                continue;
            };
            if limits.is_empty() {
                continue;
            }
            let resident = crate::sample::process_memory(identity.pid)
                .map_or(0, |memory| memory.private_resident);
            if !crate::procinfo::probe(identity.pid)?
                .is_some_and(|process| process.start == identity.start)
            {
                return Err(std::io::Error::other(
                    "process identity changed during capacity observation",
                ));
            }
            for limit in limits {
                *scopes.entry(limit.id).or_default() += resident;
            }
        }
        Ok(scopes.into_iter().collect())
    }
}

#[cfg(target_os = "linux")]
fn observe(identity: &ProcessIdentity) -> std::io::Result<Option<Vec<MemoryScope>>> {
    let matches = || {
        crate::procinfo::probe(identity.pid).map(|p| p.is_some_and(|p| p.start == identity.start))
    };
    if !matches()? {
        return Ok(None);
    }
    let scopes = crate::sample::memory_scopes(identity.pid)?;
    if !matches()? {
        return Err(std::io::Error::other(
            "process identity changed during capacity observation",
        ));
    }
    Ok(Some(scopes))
}
