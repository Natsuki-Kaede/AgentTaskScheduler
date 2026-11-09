//! The task lifecycle and its allowed transitions. Persistence checks these
//! rules against the current state inside the update transaction.

use crate::domain::resource::{Lock, Prediction};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Blocked,
    CapacityInsufficient,
    Admitted,
    Running,
    AwaitingTakeover,
    Completed,
    StartFailed,
    Canceled,
    EndedUnknown,
}

impl JobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Blocked => "blocked",
            Self::CapacityInsufficient => "capacity_insufficient",
            Self::Admitted => "admitted",
            Self::Running => "running",
            Self::AwaitingTakeover => "awaiting_takeover",
            Self::Completed => "completed",
            Self::StartFailed => "start_failed",
            Self::Canceled => "canceled",
            Self::EndedUnknown => "ended_unknown",
        }
    }

    pub const fn pre_lease(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Blocked | Self::CapacityInsufficient
        )
    }

    pub const fn held(self) -> bool {
        matches!(
            self,
            Self::Admitted | Self::Running | Self::AwaitingTakeover
        )
    }

    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::StartFailed | Self::Canceled | Self::EndedUnknown
        )
    }

    pub fn transition(self, event: JobEvent) -> Result<Self, TransitionError> {
        let next = match event {
            JobEvent::RegisterExecutor if !self.terminal() => self,
            JobEvent::Admit if self.pre_lease() => Self::Admitted,
            JobEvent::Queue(next) if self.pre_lease() && next.pre_lease() => next,
            JobEvent::Started if self == Self::Admitted => Self::Running,
            JobEvent::MainResult if matches!(self, Self::Running | Self::AwaitingTakeover) => self,
            JobEvent::ExecutorLost if self.held() => Self::AwaitingTakeover,
            JobEvent::Finish(next)
                if !self.terminal()
                    && match next {
                        Self::Completed | Self::EndedUnknown => self.held(),
                        // A spawn failure may still await confirmation after
                        // recovery takes over the executor's helper processes.
                        Self::StartFailed => self.pre_lease() || self.held(),
                        Self::Canceled => true,
                        _ => false,
                    } =>
            {
                next
            }
            _ => return Err(TransitionError { state: self, event }),
        };
        Ok(next)
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

impl std::str::FromStr for JobState {
    type Err = UnknownState;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "queued" => Self::Queued,
            "blocked" => Self::Blocked,
            "capacity_insufficient" => Self::CapacityInsufficient,
            "admitted" => Self::Admitted,
            "running" => Self::Running,
            "awaiting_takeover" => Self::AwaitingTakeover,
            "completed" => Self::Completed,
            "start_failed" => Self::StartFailed,
            "canceled" => Self::Canceled,
            "ended_unknown" => Self::EndedUnknown,
            _ => return Err(UnknownState(s.into())),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEvent {
    RegisterExecutor,
    Admit,
    Queue(JobState),
    Started,
    MainResult,
    ExecutorLost,
    Finish(JobState),
}

#[derive(Debug, thiserror::Error)]
#[error("unknown job state {0:?}")]
pub struct UnknownState(pub String);

#[derive(Debug, thiserror::Error)]
#[error("job in {state} cannot accept {event:?}")]
pub struct TransitionError {
    state: JobState,
    event: JobEvent,
}

pub const PRE_LEASE: &[&str] = &["queued", "blocked", "capacity_insufficient"];
pub const HELD: &[&str] = &["admitted", "running", "awaiting_takeover"];
pub const TERMINAL: &[&str] = &["completed", "start_failed", "canceled", "ended_unknown"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitUntil {
    Main,
    Terminal,
}

impl WaitUntil {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Terminal => "terminal",
        }
    }
}

impl std::str::FromStr for WaitUntil {
    type Err = crate::domain::error::InvalidInput;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "main" => Ok(Self::Main),
            "terminal" => Ok(Self::Terminal),
            _ => Err(crate::domain::error::InvalidInput(
                "until must be main or terminal".into(),
            )),
        }
    }
}

/// Data needed to register a submission and its job.
#[derive(Debug, Clone)]
pub struct NewSubmission {
    pub entry: Entry,
    /// Redacted display form of the command.
    pub command: String,
    pub cwd: String,
    pub project_root: String,
    pub session_id: Option<String>,
    /// The process that will start the executor, as (pid, start time).
    pub launcher: Option<(i64, String)>,
    pub plan: Plan,
}

/// What admission needs to know about a new job (design 10).
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub profiles: Vec<String>,
    pub prediction: Prediction,
    pub locks: Vec<Lock>,
    /// Predecessor job ids from `--after`.
    pub after: Vec<String>,
    pub run_now: bool,
    /// Tool and subcommand of a one-command line, for history lookups.
    pub history_key: Option<String>,
    /// Environment variables set to the leased parallelism (design 10.5).
    pub parallel_env: Vec<String>,
    /// The line runs jobserver clients, so the executor offers a pool.
    pub jobserver: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Entry {
    Argv,
    Sh,
}

impl Entry {
    pub fn as_str(self) -> &'static str {
        match self {
            Entry::Argv => "argv",
            Entry::Sh => "sh",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Created {
    pub submission: String,
    pub job: String,
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: String,
    pub state: JobState,
    pub entry: String,
    pub command: String,
    pub cwd: String,
    pub project_root: String,
    pub exit_code: Option<i64>,
    pub main_exit_code: Option<i64>,
    pub log_path: Option<String>,
    pub executor_pid: Option<i64>,
    /// Peak private memory measured so far, in bytes.
    pub peak_private: Option<i64>,
    pub submissions: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubmissionRecord {
    pub id: String,
    pub job: String,
    pub job_state: JobState,
    pub session_id: Option<String>,
    pub exit_code: Option<i64>,
    pub main_exit_code: Option<i64>,
    pub log_path: Option<String>,
    pub canceled: bool,
    /// `ats detach`: the `Stop` reminder does not hold the agent for it.
    pub detached: bool,
    /// Its end is not reported: the result already reached the agent, or
    /// it was reported once (design 6.2).
    pub quiet: bool,
}

/// A submission a reminder names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reminder {
    pub submission: String,
    pub job: String,
    pub state: JobState,
    pub exit_code: Option<i64>,
    pub command: String,
}

/// An executor's completion claim, retained until owned processes have exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishReport {
    pub state: JobState,
    pub exit_code: Option<i64>,
    pub max_rss: Option<i64>,
}

/// A job's durable facts used by conditional lifecycle updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveJob {
    pub id: String,
    pub state: JobState,
    /// The executor process, as (pid, start time).
    pub executor: Option<(i64, String)>,
    pub pgid: Option<i64>,
    /// The process that starts the executor, as (pid, start time).
    pub launcher: Option<(i64, String)>,
    pub main_exit_code: Option<i64>,
    /// Every submission of the job is canceled.
    pub canceled: bool,
    pub finish: Option<FinishReport>,
}

#[derive(Debug, Clone)]
pub struct SubmitCommand {
    pub entry: Entry,
    pub argv: Vec<String>,
    pub source: Option<String>,
    pub cwd: String,
    pub session_id: Option<String>,
    pub launcher: Option<(i64, String)>,
    pub after: Vec<String>,
    pub now: bool,
    pub locks: Vec<String>,
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_rules_reject_out_of_order_events() {
        use JobState::*;
        let cases = [
            (Queued, JobEvent::Admit, Some(Admitted)),
            (Blocked, JobEvent::Admit, Some(Admitted)),
            (Queued, JobEvent::Started, None),
            (Admitted, JobEvent::Started, Some(Running)),
            (Queued, JobEvent::MainResult, None),
            (Running, JobEvent::MainResult, Some(Running)),
            (Running, JobEvent::ExecutorLost, Some(AwaitingTakeover)),
            (Queued, JobEvent::Finish(Completed), None),
            (Running, JobEvent::Finish(Completed), Some(Completed)),
            (Queued, JobEvent::Finish(StartFailed), Some(StartFailed)),
            (
                AwaitingTakeover,
                JobEvent::Finish(StartFailed),
                Some(StartFailed),
            ),
            (Completed, JobEvent::Admit, None),
            (Completed, JobEvent::Finish(Canceled), None),
            (Running, JobEvent::Queue(Queued), None),
        ];
        for (state, event, expected) in cases {
            assert_eq!(
                state.transition(event).ok(),
                expected,
                "{state:?}: {event:?}"
            );
        }
    }
}
