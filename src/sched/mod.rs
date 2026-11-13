//! Decides admission and queue order from scheduling inputs.

pub mod pressure;

use crate::domain::resource::{Lock, MemoryScope};

#[derive(Debug, Clone, PartialEq)]
pub struct Capacity {
    pub memory: u64,
    pub used: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub id: String,
    pub reserved: u64,
    pub resident: u64,
    pub locks: Vec<Lock>,
    pub ends_in: Option<f64>,
    pub scopes: Vec<(String, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    pub id: String,
    pub age: f64,
    pub reserved: u64,
    pub seconds: Option<f64>,
    pub locks: Vec<Lock>,
    pub deps: Vec<(String, Dep)>,
    pub now: bool,
    pub ready: bool,
    pub awaited: bool,
    pub scopes: Option<Vec<MemoryScope>>,
}

pub struct Round<'a> {
    pub capacity: &'a Capacity,
    pub held: &'a [Held],
    pub waiting: &'a [Waiting],
    pub pressure: bool,
    pub admitting: bool,
}

use crate::domain::resource::{Reason, Verdict};

pub fn order(waiting: &[Waiting]) -> Vec<&Waiting> {
    let mut v: Vec<&Waiting> = waiting.iter().collect();
    v.sort_by(|a, b| b.awaited.cmp(&a.awaited).then(b.age.total_cmp(&a.age)));
    v
}

pub fn headroom(capacity: &Capacity, held: &[Held]) -> i64 {
    let pending: u64 = held
        .iter()
        .map(|h| h.reserved.saturating_sub(h.resident))
        .sum();
    capacity.memory as i64 - capacity.used as i64 - pending as i64
}

fn head_eta(held: &[Held], admitted: &[(f64, u64)], deficit: i64) -> Option<f64> {
    let mut ends: Vec<(f64, u64)> = held
        .iter()
        .map(|h| {
            (
                h.ends_in.unwrap_or(f64::INFINITY),
                h.reserved.max(h.resident),
            )
        })
        .chain(admitted.iter().copied())
        .collect();
    ends.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut freed = 0i64;
    for (t, m) in ends {
        freed += m as i64;
        if freed >= deficit {
            return t.is_finite().then_some(t);
        }
    }
    None
}

pub fn decide(r: &Round) -> Vec<(String, Verdict)> {
    let c = r.capacity;
    let mut free = headroom(c, r.held);
    let mut locks: Vec<(Lock, String)> = r
        .held
        .iter()
        .flat_map(|h| h.locks.iter().map(|l| (l.clone(), h.id.clone())))
        .collect();
    let mut admitted_ends: Vec<(f64, u64)> = Vec::new();
    let mut admitted_scopes: Vec<(Vec<String>, u64)> = Vec::new();
    let mut head: Option<(String, Option<f64>, Vec<Lock>)> = None;

    let mut out = Vec::new();
    for w in order(r.waiting) {
        let wait = |reason| Verdict::Wait { reason };
        let verdict = (|| {
            if let Some((job, _)) = w.deps.iter().find(|(_, d)| *d == Dep::Failed) {
                return Verdict::Blocked { job: job.clone() };
            }
            if let Some((job, _)) = w.deps.iter().find(|(_, d)| *d == Dep::Pending) {
                return wait(Reason::Dependency { job: job.clone() });
            }
            let budget = w
                .scopes
                .iter()
                .flatten()
                .map(|s| s.memory)
                .fold(c.memory, u64::min);
            if w.reserved > budget && !w.now {
                return Verdict::CapacityInsufficient {
                    need: w.reserved,
                    budget,
                };
            }
            if !r.admitting {
                return wait(Reason::Recovering);
            }
            if !w.ready {
                return wait(Reason::NoExecutor);
            }
            if let Some((lock, holder)) = locks
                .iter()
                .find(|(l, _)| w.locks.iter().any(|m| m.conflicts(l)))
            {
                return wait(Reason::Lock {
                    resource: lock.to_string(),
                    holder: holder.clone(),
                });
            }
            if w.now {
                return Verdict::Admit;
            }
            let Some(scopes) = &w.scopes else {
                return wait(Reason::ResourceUnavailable);
            };
            if r.pressure {
                return wait(Reason::Pressure);
            }
            if let Some((head_id, eta, head_locks)) = &head {
                let ends_first = w.seconds.zip(*eta).is_some_and(|(s, e)| s <= e);
                let takes_its_lock = w
                    .locks
                    .iter()
                    .any(|l| head_locks.iter().any(|h| h.conflicts(l)));
                if takes_its_lock || !ends_first {
                    return wait(Reason::Behind {
                        head: head_id.clone(),
                    });
                }
            }
            let scope_free = scopes
                .iter()
                .map(|scope| {
                    let pending: u64 = r
                        .held
                        .iter()
                        .filter_map(|held| {
                            held.scopes
                                .iter()
                                .find(|(id, _)| id == &scope.id)
                                .map(|(_, resident)| held.reserved.saturating_sub(*resident))
                        })
                        .sum();
                    let admitted: u64 = admitted_scopes
                        .iter()
                        .filter(|(ids, _)| ids.contains(&scope.id))
                        .map(|(_, memory)| memory)
                        .sum();
                    scope.memory as i64 - scope.used as i64 - pending as i64 - admitted as i64
                })
                .min()
                .unwrap_or(i64::MAX);
            let available = free.min(scope_free);
            let idle = r.held.is_empty() && admitted_ends.is_empty();
            if w.reserved as i64 <= available || (idle && w.reserved as i64 <= scope_free) {
                return Verdict::Admit;
            }
            if head.is_none() {
                let eta = head_eta(r.held, &admitted_ends, w.reserved as i64 - available);
                head = Some((w.id.clone(), eta, w.locks.clone()));
            }
            wait(Reason::Memory {
                need: w.reserved,
                free: available,
            })
        })();
        if verdict == Verdict::Admit {
            free -= w.reserved as i64;
            locks.extend(w.locks.iter().map(|l| (l.clone(), w.id.clone())));
            admitted_ends.push((w.seconds.unwrap_or(f64::INFINITY), w.reserved));
            admitted_scopes.push((
                w.scopes.iter().flatten().map(|s| s.id.clone()).collect(),
                w.reserved,
            ));
        }
        out.push((w.id.clone(), verdict));
    }
    out
}

#[cfg(test)]
mod tests;
