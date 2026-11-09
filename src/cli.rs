//! Client commands: `ats run`, `wait`, `status`, `logs`, `cancel`, `detach`
//! and the return contract of design 5.4.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::client::{self, Answer};
use crate::domain::job::{Created, Entry, JobState, WaitUntil};
use crate::domain::resource::{Reason, Verdict};
use crate::executor;
use crate::paths::Paths;
use crate::procinfo;
use crate::protocol::{
    ExecutorRequest, FinishedRequest, Job, Submission, SubmitRequest, WaitRequest, WaitResponse,
    WhyResponse,
};
use crate::runspec::RunSpec;
use crate::timing::Deadline;

/// The command was not finished within the wait; it continues in the
/// background.
pub const EXIT_DETACHED: u8 = 75;
/// The command cannot run because a predecessor failed.
pub const EXIT_BLOCKED: u8 = 76;
/// The submission was rejected.
pub const EXIT_REJECTED: u8 = 77;

/// Lines of log attached when a failed terminal state is retrieved.
const TAIL_LINES: usize = 50;
/// Longest single wait request, so a request-file answer stays bounded.
const WAIT_CHUNK: Duration = Duration::from_secs(30);
const FOLLOW_POLL: Duration = Duration::from_millis(500);
const RECONNECT_PAUSE: Duration = Duration::from_millis(500);

/// Scheduling options of `ats run` (design 5.2).
#[derive(clap::Args, Debug, Default)]
pub struct RunOptions {
    /// Run only after these jobs or submissions succeed (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub after: Vec<String>,
    /// Run at once, skipping the queue. Only when explicitly asked to run
    /// now; refused when a dependency or lock conflicts.
    #[arg(long)]
    pub now: bool,
    /// Also lock this path or named resource, such as a device or port.
    #[arg(long, value_delimiter = ',')]
    pub lock: Vec<String>,
    /// Expected memory, such as mem=6G; used where higher than predicted.
    #[arg(long)]
    pub hint: Option<String>,
}

fn expect_ok(answer: Answer) -> Result<Value> {
    if !(200..300).contains(&answer.status) {
        bail!("{}", error_text(&answer));
    }
    Ok(answer.body)
}

fn error_text(answer: &Answer) -> String {
    match answer.body["error"].as_str() {
        Some(e) => e.to_string(),
        None => format!("daemon answered {}: {}", answer.status, answer.body),
    }
}

async fn get<T: serde::de::DeserializeOwned>(paths: &Paths, path: &str) -> Result<T> {
    Ok(serde_json::from_value(expect_ok(
        client::call(paths, "GET", path, None).await?.0,
    )?)?)
}

async fn post(paths: &Paths, path: &str, body: impl serde::Serialize) -> Result<()> {
    expect_ok(
        client::call(paths, "POST", path, Some(serde_json::to_value(body)?))
            .await?
            .0,
    )?;
    Ok(())
}

/// Waits for a submission to reach `until` ("main" or "terminal"), in
/// bounded requests. Returns whether it was reached, and the submission.
async fn wait_for(
    paths: &Paths,
    submission: &str,
    until: WaitUntil,
    wait: Option<Duration>,
) -> Result<(bool, Submission)> {
    let deadline = Deadline::after(wait);
    let mut last: Option<Submission> = None;
    loop {
        let chunk = deadline.remaining().unwrap_or(WAIT_CHUNK).min(WAIT_CHUNK);
        let called = client::call_with_timeout(
            paths,
            "POST",
            &format!("/v1/submissions/{submission}/wait"),
            Some(serde_json::to_value(WaitRequest::new(
                until,
                Some(chunk.as_secs_f64()),
            ))?),
            chunk + Duration::from_secs(10),
        )
        .await;
        let answer = match (called, &last) {
            (Ok((answer, _)), _) => answer,
            // Once the daemon has answered, an unreachable daemon is
            // restarting; keep waiting within the time limit.
            (Err(_), Some(last)) if deadline.expired() => return Ok((false, last.clone())),
            (Err(_), Some(_)) => {
                tokio::time::sleep(RECONNECT_PAUSE).await;
                continue;
            }
            (Err(e), None) => return Err(e),
        };
        let body: WaitResponse = serde_json::from_value(expect_ok(answer)?)?;
        let reached = body.reached;
        if reached || deadline.expired() {
            return Ok((reached, body.submission));
        }
        last = Some(body.submission);
    }
}

fn exit_code(code: i64) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// The one-line status printed instead of command output (design 5.4).
fn status_line(sub: &Submission) -> String {
    let log = sub.log_path.as_deref().unwrap_or("not started yet");
    format!(
        "[ats] submission {} job {} is {}; log: {log}; check with: ats wait {}",
        sub.id, sub.job, sub.job_state, sub.id,
    )
}

fn read_log(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap_or_default()
}

/// Writes the command's own output unchanged (design 5.4).
fn print_output(sub: &Submission) -> Result<()> {
    if let Some(dir) = sub.log_path.as_deref() {
        let dir = Path::new(dir);
        std::io::stdout().write_all(&read_log(dir, "stdout.log"))?;
        std::io::stdout().flush()?;
        std::io::stderr().write_all(&read_log(dir, "stderr.log"))?;
    }
    Ok(())
}

/// Prints the last lines of each non-empty stream, for failed jobs.
fn print_tails(log_path: Option<&str>) {
    let Some(dir) = log_path else { return };
    for name in ["stdout.log", "stderr.log"] {
        let data = read_log(Path::new(dir), name);
        if data.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(TAIL_LINES);
        eprintln!("[ats] last {} lines of {name}:", lines.len() - start);
        for line in &lines[start..] {
            eprintln!("{line}");
        }
    }
}

fn failed(sub: &Submission) -> bool {
    sub.job_state != JobState::Completed || sub.exit_code != Some(0)
}

/// `ats run`: submits, starts the executor, and returns the main execution
/// process result or a status line.
pub async fn run(
    paths: &Paths,
    spec: RunSpec,
    wait: Option<Duration>,
    session: Option<String>,
    options: RunOptions,
) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let (entry, argv, source) = match &spec {
        RunSpec::Argv { argv } => (Entry::Argv, argv.clone(), None),
        RunSpec::Sh { source, .. } => (Entry::Sh, Vec::new(), Some(source.clone())),
    };
    let body = serde_json::to_value(SubmitRequest {
        entry,
        argv,
        source,
        cwd: cwd.to_string_lossy().into_owned(),
        session,
        launcher_pid: Some(i64::from(std::process::id())),
        launcher_start: procinfo::start_time(std::process::id() as i32),
        after: options.after,
        now: options.now,
        lock: options.lock,
        hint: options.hint,
    })?;
    let (answer, _) = client::call(paths, "POST", "/v1/jobs", Some(body)).await?;
    if matches!(answer.status, 400 | 409) {
        eprintln!("ats: submission rejected: {}", error_text(&answer));
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    let created: Created = serde_json::from_value(expect_ok(answer)?)?;
    let (submission, job) = (&created.submission, &created.job);

    match executor::spawn_detached(paths, job, &spec, &cwd) {
        // Register the executor before this process can exit, so the job
        // always has a known starter (the executor registers too).
        Ok(pid) => {
            if let Some(start) = procinfo::start_time(pid) {
                let body = serde_json::to_value(ExecutorRequest {
                    pid: i64::from(pid),
                    start,
                })?;
                client::call(
                    paths,
                    "POST",
                    &format!("/v1/jobs/{job}/executor"),
                    Some(body),
                )
                .await?;
            }
        }
        Err(e) => {
            post(
                paths,
                &format!("/v1/jobs/{job}/finished"),
                FinishedRequest::new(JobState::StartFailed, None, None),
            )
            .await?;
            return Err(e);
        }
    }

    let (reached, sub) = wait_for(paths, submission, WaitUntil::Main, wait).await?;
    if sub.job_state == JobState::Blocked {
        return blocked(paths, &sub).await;
    }
    if reached {
        if let Some(code) = sub.main_exit_code {
            print_output(&sub)?;
            delivered(paths, &sub).await;
            return Ok(exit_code(code));
        }
        if sub.job_state == JobState::StartFailed {
            print_output(&sub)?;
            delivered(paths, &sub).await;
            return Ok(exit_code(sub.exit_code.unwrap_or(127)));
        }
    }
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::from(EXIT_DETACHED))
}

/// The submission's result has reached the agent: reminders leave it out
/// (design 6.2). Should the daemon miss this, a reminder names it once.
async fn delivered(paths: &Paths, sub: &Submission) {
    let id = &sub.id;
    let _ = client::call(paths, "POST", &format!("/v1/submissions/{id}/quiet"), None).await;
}

fn terminal_line(sub: &Submission) -> String {
    let code = sub
        .exit_code
        .map_or("unknown".to_string(), |c| c.to_string());
    format!(
        "[ats] submission {} job {} is {}, exit code {code}",
        sub.id, sub.job, sub.job_state,
    )
}

/// Reports a submission's terminal state and attaches log tails on
/// failure (design 5.4).
fn report_terminal(sub: &Submission) -> ExitCode {
    eprintln!("{}", terminal_line(sub));
    if failed(sub) {
        print_tails(sub.log_path.as_deref());
    }
    match sub.exit_code {
        Some(code) => exit_code(code),
        None => ExitCode::from(1),
    }
}

/// Reports a job held back by a failed predecessor (design 5.4: 76).
async fn blocked(paths: &Paths, sub: &Submission) -> Result<ExitCode> {
    let job = &sub.job;
    let why: WhyResponse = get(paths, &format!("/v1/jobs/{job}/why")).await?;
    let predecessor = match &why.verdict {
        Some(Verdict::Blocked { job }) => job.as_str(),
        _ => "a predecessor",
    };
    eprintln!(
        "[ats] submission {} job {job} did not run: {predecessor} failed. Run `ats unblock {job}` to run it anyway, or `ats cancel {}`.",
        sub.id, sub.id,
    );
    delivered(paths, sub).await;
    Ok(ExitCode::from(EXIT_BLOCKED))
}

/// `ats wait`: waits for the job's terminal state and returns the
/// command's output and exit code (design 5.3, 5.4).
pub async fn wait(paths: &Paths, submission: &str, wait: Option<Duration>) -> Result<ExitCode> {
    let (reached, sub) = wait_for(paths, submission, WaitUntil::Terminal, wait).await?;
    if sub.job_state == JobState::Blocked {
        return blocked(paths, &sub).await;
    }
    if reached {
        // The command's own output, as `ats run` would have returned it,
        // then the job's end state.
        print_output(&sub)?;
        eprintln!("{}", terminal_line(&sub));
        delivered(paths, &sub).await;
        return Ok(match sub.exit_code {
            Some(code) => exit_code(code),
            None => ExitCode::from(1),
        });
    }
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::from(EXIT_DETACHED))
}

/// `ats status <id>`: a submission or a job.
pub async fn status_one(paths: &Paths, id: &str, json_out: bool) -> Result<ExitCode> {
    if id.starts_with("j_") {
        let job: Job = get(paths, &format!("/v1/jobs/{id}")).await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&job)?);
        } else {
            println!("{} {} {}", job.id, job.state, job.command);
        }
    } else {
        let sub: Submission = get(paths, &format!("/v1/submissions/{id}")).await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&sub)?);
        } else if sub.job_state.terminal() {
            delivered(paths, &sub).await;
            report_terminal(&sub);
        } else {
            println!("{}", status_line(&sub));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The log directory and state of a submission or job.
async fn log_target(paths: &Paths, id: &str) -> Result<(Option<PathBuf>, JobState, String)> {
    let job: Job = if id.starts_with("j_") {
        get(paths, &format!("/v1/jobs/{id}")).await?
    } else {
        let sub: Submission = get(paths, &format!("/v1/submissions/{id}")).await?;
        get(paths, &format!("/v1/jobs/{}", sub.job)).await?
    };
    Ok((job.log_path.map(PathBuf::from), job.state, job.id))
}

/// `ats logs <id> [-f]`: prints the logs, stdout to stdout and stderr to
/// stderr; with `follow`, keeps printing until the job ends.
pub async fn logs(paths: &Paths, id: &str, follow: bool) -> Result<ExitCode> {
    let mut offsets = [0usize; 2];
    loop {
        let (dir, state, job) = log_target(paths, id).await?;
        let done = state.terminal();
        if let Some(dir) = &dir {
            for (i, name) in ["stdout.log", "stderr.log"].into_iter().enumerate() {
                let data = read_log(dir, name);
                if data.len() > offsets[i] {
                    let new = &data[offsets[i]..];
                    if i == 0 {
                        std::io::stdout().write_all(new)?;
                        std::io::stdout().flush()?;
                    } else {
                        std::io::stderr().write_all(new)?;
                    }
                    offsets[i] = data.len();
                }
            }
        } else if !follow {
            eprintln!("[ats] job {job} is {state} and has no log yet");
        }
        if !follow || done {
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(FOLLOW_POLL).await;
    }
}

/// `ats cancel` / `ats detach`.
pub async fn mark(paths: &Paths, submission: &str, action: &str) -> Result<ExitCode> {
    post(
        paths,
        &format!("/v1/submissions/{submission}/{action}"),
        json!({}),
    )
    .await?;
    let sub: Submission = get(paths, &format!("/v1/submissions/{submission}")).await?;
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::SUCCESS)
}

/// A submission or job id to its job id.
async fn job_of(paths: &Paths, id: &str) -> Result<String> {
    if id.starts_with("j_") {
        return Ok(id.to_string());
    }
    let sub: Submission = get(paths, &format!("/v1/submissions/{id}")).await?;
    Ok(sub.job)
}

fn gib(bytes: f64) -> String {
    format!("{:.1}G", bytes / (1u64 << 30) as f64)
}

/// One sentence for a scheduler verdict.
fn explain(v: &Verdict) -> String {
    match v {
        Verdict::Admit => "it is being admitted now".into(),
        Verdict::Blocked { job } => {
            format!("predecessor {job} failed; `ats unblock` runs it anyway")
        }
        Verdict::CapacityInsufficient { need, budget } => format!(
            "it is predicted to need {}, more than the memory budget of {}; cancel it, give a lower --hint, or run it with --now",
            gib(*need as f64),
            gib(*budget as f64)
        ),
        Verdict::Wait { reason } => match reason {
            Reason::Recovering => {
                "the daemon restarted and is waiting for running jobs to reconnect".into()
            }
            Reason::Pressure => "memory pressure is rising, so nothing new is admitted".into(),
            Reason::NoExecutor => "its executor has not registered yet".into(),
            Reason::Dependency { job } => format!("waiting for {job} to succeed"),
            Reason::Lock { resource, holder } => format!("{resource} is locked by {holder}"),
            Reason::Memory { need, free } => format!(
                "it needs {} of memory and {} is free",
                gib(*need as f64),
                gib((*free).max(0) as f64)
            ),
            Reason::Behind { head } => format!("{head} is first in line and this would delay it"),
        },
    }
}

/// `ats why`: why a job is still waiting.
pub async fn why(paths: &Paths, id: &str) -> Result<ExitCode> {
    let job = job_of(paths, id).await?;
    let why: WhyResponse = get(paths, &format!("/v1/jobs/{job}/why")).await?;
    match &why.verdict {
        Some(v) => println!("job {job} is {}: {}", why.state, explain(v)),
        None => println!("job {job} is {}", why.state),
    }
    Ok(ExitCode::SUCCESS)
}

/// `ats unblock`: runs a job whose predecessor failed (design 5.3).
pub async fn unblock(paths: &Paths, id: &str) -> Result<ExitCode> {
    let job = job_of(paths, id).await?;
    post(paths, &format!("/v1/jobs/{job}/unblock"), json!({})).await?;
    println!("job {job} is queued again");
    Ok(ExitCode::SUCCESS)
}
