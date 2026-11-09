//! Watches executors and settles jobs whose executor is gone (design 7.1).
//! A command runs only after its lease is committed, so a job that never
//! got a lease is known not to have run; after the lease, a job keeps it
//! while its process group lives, and its exit code is unknown unless the
//! executor reported the main result.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;

use super::App;
use crate::domain::error::Conflict;
use crate::domain::job::{ActiveJob, JobEvent, JobState};
use crate::domain::process::{ProcessIdentity, ProcessSnapshot};
use crate::procinfo;
use crate::timing;

/// Process checks, replaceable in tests.
pub trait Processes {
    fn alive(&self, pid: i64, start: &str) -> bool;
    fn members(&self, job: &ActiveJob) -> &[ProcessIdentity];
    fn ready(&self, _job: &ActiveJob) -> bool {
        true
    }
    /// Asks the job's process group to end, or kills it when `force`.
    fn terminate(&self, job: &ActiveJob, force: bool);
}

/// One recovery pass uses a fixed committed observation, even if monitoring fails meanwhile.
pub struct System {
    observation: Option<ProcessSnapshot>,
}

impl System {
    pub fn new(app: &App) -> Self {
        Self {
            observation: app.observations.lock().expect("observations lock").clone(),
        }
    }
}

impl Processes for System {
    fn ready(&self, job: &ActiveJob) -> bool {
        let Some(observation) = self.observation.as_ref() else {
            return false;
        };
        job.state.pre_lease()
            || observation
                .roots
                .get(&job.id)
                .is_some_and(|root| root.pgid.map(i64::from) == job.pgid)
    }

    fn alive(&self, pid: i64, start: &str) -> bool {
        // A process may have registered since the last shared observation.
        procinfo::probe(pid as i32).map_or(true, |p| p.is_some_and(|p| p.start == start))
    }

    fn members(&self, job: &ActiveJob) -> &[ProcessIdentity] {
        self.observation
            .as_ref()
            .and_then(|o| o.members.get(&job.id))
            .map_or(&[], Vec::as_slice)
    }

    fn terminate(&self, job: &ActiveJob, force: bool) {
        procinfo::signal(
            &crate::runtime::family::Family::from_members(self.members(job).iter().cloned()),
            if force { libc::SIGKILL } else { libc::SIGTERM },
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    AwaitTakeover,
    /// Canceled while its executor is gone: the daemon stops it.
    Terminate,
    Finish(JobState, Option<i64>),
}

fn alive(p: &Option<(i64, String)>, procs: &impl Processes) -> bool {
    p.as_ref()
        .is_some_and(|(pid, start)| procs.alive(*pid, start))
}

/// Decides what to do with one job that has not ended.
pub fn check(job: &ActiveJob, procs: &impl Processes) -> Option<Action> {
    if !procs.ready(job) {
        return None;
    }
    if let Some(report) = &job.finish
        && procs.members(job).is_empty()
    {
        return Some(Action::Finish(
            if job.canceled {
                JobState::Canceled
            } else {
                report.state
            },
            report.exit_code,
        ));
    }
    if alive(&job.executor, procs) {
        return None;
    }
    if job.state.pre_lease() {
        // Without a lease the command never ran. Wait for an executor while
        // the process that starts it is alive.
        let starter_gone =
            job.executor.is_some() || (job.launcher.is_some() && !alive(&job.launcher, procs));
        return starter_gone.then_some(Action::Finish(JobState::StartFailed, None));
    }
    if !procs.members(job).is_empty() {
        return Some(
            if job.state != crate::domain::job::JobState::AwaitingTakeover {
                Action::AwaitTakeover
            } else if job.canceled {
                Action::Terminate
            } else {
                return None;
            },
        );
    }
    Some(match (job.canceled, job.main_exit_code) {
        (true, code) => Action::Finish(JobState::Canceled, code),
        (false, Some(code)) => Action::Finish(JobState::Completed, Some(code)),
        (false, None) => Action::Finish(JobState::EndedUnknown, None),
    })
}

/// Jobs whose executors are alive now; after a restart they are given time
/// to reconnect before new leases are granted.
pub fn live_executors(state: &App, procs: &impl Processes) -> Result<Vec<String>> {
    Ok(state
        .store()
        .active_jobs()?
        .into_iter()
        .filter(|j| alive(&j.executor, procs))
        .map(|j| j.id)
        .collect())
}

/// How long processes get to end after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Applies [`check`] to every job that has not ended. `terminating` holds
/// when each canceled job was asked to end.
pub fn sweep(
    state: &App,
    procs: &impl Processes,
    terminating: &mut HashMap<String, Duration>,
) -> Result<()> {
    let jobs = state.store().active_jobs()?;
    terminating.retain(|id, _| jobs.iter().any(|j| &j.id == id));
    let mut changed = false;
    for job in jobs {
        let Some(action) = check(&job, procs) else {
            continue;
        };
        let applied = match &action {
            Action::Terminate => {
                let now = timing::elapsed();
                match terminating.get(&job.id) {
                    None => {
                        procs.terminate(&job, false);
                        terminating.insert(job.id.clone(), now);
                    }
                    Some(&asked) if now.saturating_sub(asked) >= TERM_GRACE => {
                        procs.terminate(&job, true)
                    }
                    Some(_) => {}
                }
                continue;
            }
            Action::AwaitTakeover => {
                state
                    .store()
                    .recover(&job, procs.members(&job), JobEvent::ExecutorLost, None)
            }
            Action::Finish(st, code) => {
                state
                    .store()
                    .recover(&job, procs.members(&job), JobEvent::Finish(*st), *code)
            }
        };
        match applied {
            Ok(()) => {
                tracing::warn!(job = %job.id, ?action, "executor gone");
                changed = true;
            }
            // Job facts or committed membership changed after the recovery read.
            Err(e) if e.downcast_ref::<Conflict>().is_some() => {}
            Err(e) => return Err(e),
        }
    }
    if changed {
        state.notify();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[derive(Default)]
    struct Fake {
        alive: HashSet<i64>,
        groups: HashMap<i64, Vec<ProcessIdentity>>,
    }

    impl Processes for Fake {
        fn alive(&self, pid: i64, _: &str) -> bool {
            self.alive.contains(&pid)
        }
        fn members(&self, job: &ActiveJob) -> &[ProcessIdentity] {
            job.pgid
                .and_then(|g| self.groups.get(&g))
                .map_or(&[], Vec::as_slice)
        }
        fn terminate(&self, _: &ActiveJob, _: bool) {}
    }

    fn job(state: &str) -> ActiveJob {
        ActiveJob {
            id: "j".into(),
            state: state.parse().unwrap(),
            executor: Some((10, "s".into())),
            pgid: None,
            launcher: Some((20, "s".into())),
            main_exit_code: None,
            canceled: false,
            finish: None,
        }
    }

    fn fake(alive: &[i64], groups: &[i64]) -> Fake {
        Fake {
            alive: alive.iter().copied().collect(),
            groups: groups
                .iter()
                .map(|&g| {
                    (
                        g,
                        vec![ProcessIdentity {
                            pid: g as i32,
                            start: "worker".into(),
                        }],
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn completion_waits_for_members_and_honors_late_cancellation() {
        let mut j = job("running");
        j.pgid = Some(30);
        j.finish = Some(crate::domain::job::FinishReport {
            state: JobState::Completed,
            exit_code: Some(7),
            max_rss: Some(900),
        });
        assert_eq!(
            check(&j, &fake(&[10], &[])),
            Some(Action::Finish(JobState::Completed, Some(7)))
        );
        assert_eq!(check(&j, &fake(&[10], &[30])), None);
        assert_eq!(check(&j, &fake(&[], &[30])), Some(Action::AwaitTakeover));
        j.state = JobState::AwaitingTakeover;
        j.canceled = true;
        assert_eq!(check(&j, &fake(&[], &[30])), Some(Action::Terminate));
        assert_eq!(
            check(&j, &fake(&[], &[])),
            Some(Action::Finish(JobState::Canceled, Some(7)))
        );
    }

    #[test]
    fn queued_jobs_never_ran_so_a_lost_starter_means_start_failed() {
        let start_failed = Some(Action::Finish(JobState::StartFailed, None));
        assert_eq!(check(&job("queued"), &fake(&[10], &[])), None);
        assert_eq!(check(&job("queued"), &fake(&[20], &[])), start_failed);

        let mut waiting = job("queued");
        waiting.executor = None;
        assert_eq!(
            check(&waiting, &fake(&[20], &[])),
            None,
            "launcher still starting the executor"
        );
        assert_eq!(check(&waiting, &fake(&[], &[])), start_failed);
        waiting.launcher = None;
        assert_eq!(check(&waiting, &fake(&[], &[])), None, "no launcher known");
    }

    #[test]
    fn a_lost_executor_holds_the_job_while_its_processes_run() {
        let mut j = job("running");
        j.pgid = Some(30);
        assert_eq!(check(&j, &fake(&[10], &[30])), None);
        assert_eq!(check(&j, &fake(&[], &[30])), Some(Action::AwaitTakeover));
        assert_eq!(
            check(&j, &fake(&[], &[])),
            Some(Action::Finish(JobState::EndedUnknown, None))
        );

        j.state = crate::domain::job::JobState::AwaitingTakeover;
        assert_eq!(check(&j, &fake(&[], &[30])), None);
        j.main_exit_code = Some(4);
        assert_eq!(
            check(&j, &fake(&[], &[])),
            Some(Action::Finish(JobState::Completed, Some(4)))
        );
    }

    #[test]
    fn a_lost_start_confirmation_is_never_reported_as_not_started() {
        // Leased, but the executor died before reporting the start: the
        // command may have run.
        let j = job("admitted");
        assert_eq!(
            check(&j, &fake(&[], &[])),
            Some(Action::Finish(JobState::EndedUnknown, None))
        );
    }

    #[test]
    fn a_canceled_takeover_job_is_stopped_then_ends_canceled() {
        let mut j = job("awaiting_takeover");
        j.pgid = Some(30);
        j.canceled = true;
        assert_eq!(check(&j, &fake(&[], &[30])), Some(Action::Terminate));
        assert_eq!(
            check(&j, &fake(&[], &[])),
            Some(Action::Finish(JobState::Canceled, None))
        );
        j.state = crate::domain::job::JobState::Running;
        assert_eq!(
            check(&j, &fake(&[], &[30])),
            Some(Action::AwaitTakeover),
            "first marked as taken over"
        );
    }
}
