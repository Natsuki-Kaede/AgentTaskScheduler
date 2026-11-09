//! Shared wire messages. Database records are converted explicitly at the API boundary.
use crate::domain::job::{Entry, JobState};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
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

impl From<crate::domain::job::JobRecord> for Job {
    fn from(value: crate::domain::job::JobRecord) -> Self {
        Self {
            id: value.id,
            state: value.state,
            entry: value.entry,
            command: value.command,
            cwd: value.cwd,
            project_root: value.project_root,
            exit_code: value.exit_code,
            main_exit_code: value.main_exit_code,
            log_path: value.log_path,
            executor_pid: value.executor_pid,
            peak_private: value.peak_private,
            submissions: value.submissions,
            created_at: value.created_at,
            started_at: value.started_at,
            ended_at: value.ended_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
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

impl From<crate::domain::job::SubmissionRecord> for Submission {
    fn from(value: crate::domain::job::SubmissionRecord) -> Self {
        Self {
            id: value.id,
            job: value.job,
            job_state: value.job_state,
            session_id: value.session_id,
            exit_code: value.exit_code,
            main_exit_code: value.main_exit_code,
            log_path: value.log_path,
            canceled: value.canceled,
            detached: value.detached,
            quiet: value.quiet,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    pub submission: String,
    pub job: String,
    pub state: JobState,
    pub exit_code: Option<i64>,
    pub command: String,
}

impl From<crate::domain::job::Reminder> for Reminder {
    fn from(value: crate::domain::job::Reminder) -> Self {
        Self {
            submission: value.submission,
            job: value.job,
            state: value.state,
            exit_code: value.exit_code,
            command: value.command,
        }
    }
}

/// Body of `POST /v1/jobs`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub entry: Entry,
    #[serde(default)]
    pub argv: Vec<String>,
    pub source: Option<String>,
    pub cwd: String,
    pub session: Option<String>,
    /// The submitting CLI, which starts the executor.
    pub launcher_pid: Option<i64>,
    pub launcher_start: Option<String>,
    /// `--after`: job or submission ids that must succeed first.
    #[serde(default)]
    pub after: Vec<String>,
    /// `--now`: run at once unless a dependency or lock conflicts.
    #[serde(default)]
    pub now: bool,
    /// `--lock`: extra paths or named resources.
    #[serde(default)]
    pub lock: Vec<String>,
    /// `--hint mem=6G`.
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorRequest {
    pub pid: i64,
    pub start: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub pid: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartedRequest {
    pub log_path: String,
    pub pgid: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainResultRequest {
    pub exit_code: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishedRequest {
    pub state: String,
    pub exit_code: Option<i64>,
    /// The largest resident size of any process the command waited for,
    /// for commands too short to sample (design 8.3).
    pub max_rss: Option<i64>,
}

impl FinishedRequest {
    pub fn new(state: JobState, exit_code: Option<i64>, max_rss: Option<i64>) -> Self {
        Self {
            state: state.to_string(),
            exit_code,
            max_rss,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitRequest {
    /// Seconds to wait; absent waits without limit.
    pub wait_seconds: Option<f64>,
    /// "main": until the main execution process has a result or the job
    /// ends. "terminal": until the job ends.
    pub until: String,
}

impl WaitRequest {
    pub fn new(until: crate::domain::job::WaitUntil, wait_seconds: Option<f64>) -> Self {
        Self {
            until: until.as_str().into(),
            wait_seconds,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseResponse {
    pub granted: bool,
    #[serde(flatten)]
    pub terms: Option<crate::domain::resource::LeaseTerms>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitResponse {
    pub reached: bool,
    pub submission: Submission,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub cancel: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhyResponse {
    pub job: String,
    pub state: JobState,
    pub verdict: Option<crate::domain::resource::Verdict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
    pub pid: u32,
    pub started_at: String,
    pub instance: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::resource::LeaseTerms;
    use serde_json::json;

    #[test]
    fn lease_messages_preserve_the_flat_v1_wire_contract() {
        assert_eq!(
            serde_json::to_value(LeaseResponse {
                granted: false,
                terms: None
            })
            .unwrap(),
            json!({"granted": false})
        );
        let granted = json!({"granted": true, "parallelism": 4, "jobserver_tokens": 3, "env": ["CARGO_BUILD_JOBS"]});
        let lease: LeaseResponse = serde_json::from_value(granted.clone()).unwrap();
        assert_eq!(
            lease.terms.unwrap(),
            LeaseTerms {
                parallelism: Some(4),
                jobserver_tokens: Some(3),
                env: vec!["CARGO_BUILD_JOBS".into()]
            }
        );
        let round_trip: LeaseResponse = serde_json::from_value(granted.clone()).unwrap();
        assert_eq!(serde_json::to_value(round_trip).unwrap(), granted);
    }
}
