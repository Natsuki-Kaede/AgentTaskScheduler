//! Implements the daemon client commands.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{self, Answer};
use crate::domain::job::{Created, Entry, JobState, WaitUntil};
use crate::domain::resource::{CargoEnv, Reason, Verdict};
use crate::executor;
use crate::paths::Paths;
use crate::procinfo;
use crate::protocol::{
    ExecutorRequest, FinishedRequest, Job, Submission, SubmitRequest, WaitRequest, WaitResponse,
    WhyResponse,
};
use crate::runspec::RunSpec;
use crate::timing::Deadline;

pub const EXIT_DETACHED: u8 = 75;
pub const EXIT_BLOCKED: u8 = 76;
pub const EXIT_REJECTED: u8 = 77;

const TAIL_LINES: usize = 50;
const WAIT_CHUNK: Duration = Duration::from_secs(30);
const FOLLOW_POLL: Duration = Duration::from_millis(500);
const RECONNECT_PAUSE: Duration = Duration::from_millis(500);

/// Scheduling options of `ats run`.
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

fn print_output(sub: &Submission) -> Result<()> {
    if let Some(dir) = sub.log_path.as_deref() {
        let dir = Path::new(dir);
        std::io::stdout().write_all(&read_log(dir, "stdout.log"))?;
        std::io::stdout().flush()?;
        std::io::stderr().write_all(&read_log(dir, "stderr.log"))?;
        print_capture_errors(dir);
    }
    Ok(())
}

fn print_capture_errors(dir: &Path) {
    if let Ok(errors) = std::fs::read_to_string(dir.join("capture-errors.log")) {
        eprintln!("[ats] output may be incomplete: {errors}");
    }
}

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
        cargo_env: CargoEnv {
            target_dir: std::env::var("CARGO_TARGET_DIR").ok(),
            build_target_dir: std::env::var("CARGO_BUILD_TARGET_DIR").ok(),
            home: std::env::var("CARGO_HOME").ok(),
        },
    })?;
    let (answer, _) = client::call(paths, "POST", "/v1/jobs", Some(body)).await?;
    if matches!(answer.status, 400 | 409) {
        eprintln!("ats: submission rejected: {}", error_text(&answer));
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    let created: Created = serde_json::from_value(expect_ok(answer)?)?;
    let (submission, job) = (&created.submission, &created.job);
    run_accepted(paths, &spec, &cwd, wait, &created)
        .await
        .with_context(|| {
            format!("[ats] submission {submission} job {job} was accepted; check with: ats wait {submission}")
        })
}

async fn run_accepted(
    paths: &Paths,
    spec: &RunSpec,
    cwd: &Path,
    wait: Option<Duration>,
    created: &Created,
) -> Result<ExitCode> {
    let (submission, job) = (&created.submission, &created.job);
    match executor::spawn_detached(paths, job, spec, cwd) {
        Ok(pid) => {
            if let Some(start) = procinfo::start_time(pid) {
                let body = serde_json::to_value(ExecutorRequest {
                    pid: i64::from(pid),
                    start,
                })?;
                let _ = client::call(
                    paths,
                    "POST",
                    &format!("/v1/jobs/{job}/executor"),
                    Some(body),
                )
                .await;
            }
        }
        Err(e) => {
            post(
                paths,
                &format!("/v1/jobs/{job}/finished"),
                FinishedRequest::new(JobState::StartFailed, None, None),
            )
            .await
            .with_context(|| format!("could not start the executor: {e:#}"))?;
            return Err(e);
        }
    }

    let (reached, sub) = wait_for(paths, submission, WaitUntil::Main, wait).await?;
    if sub.job_state == JobState::Blocked {
        return blocked(paths, &sub).await;
    }
    if sub.job_state == JobState::CapacityInsufficient {
        return Ok(report_capacity(&sub));
    }
    if reached {
        if sub.job_state.terminal() {
            print_output(&sub)?;
            delivered(paths, &sub).await;
            return Ok(terminal_exit_code(&sub));
        }
        if let Some(code) = sub.main_exit_code {
            print_output(&sub)?;
            delivered(paths, &sub).await;
            return Ok(exit_code(code));
        }
    }
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::from(EXIT_DETACHED))
}

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

fn terminal_exit_code(sub: &Submission) -> ExitCode {
    match sub.job_state {
        JobState::Completed => exit_code(sub.exit_code.unwrap_or(1)),
        JobState::StartFailed => exit_code(sub.exit_code.unwrap_or(127)),
        _ => exit_code(sub.exit_code.filter(|code| *code != 0).unwrap_or(1)),
    }
}

fn report_terminal(sub: &Submission) {
    eprintln!("{}", terminal_line(sub));
    if failed(sub) {
        print_tails(sub.log_path.as_deref());
    }
}

async fn blocked(paths: &Paths, sub: &Submission) -> Result<ExitCode> {
    let job = &sub.job;
    let why: Option<WhyResponse> = get(paths, &format!("/v1/jobs/{job}/why")).await.ok();
    let predecessor = match why.as_ref().and_then(|why| why.verdict.as_ref()) {
        Some(Verdict::Blocked { job }) => job.as_str(),
        _ => "a predecessor",
    };
    eprintln!(
        "[ats] submission {} job {job} did not run: {predecessor} failed. Run `ats unblock {job}` to run it anyway, or `ats cancel {}`.",
        sub.id, sub.id,
    );
    Ok(ExitCode::from(EXIT_BLOCKED))
}

fn report_capacity(sub: &Submission) -> ExitCode {
    eprintln!(
        "[ats] submission {} job {} is capacity_insufficient. Use `ats why {}` for the memory estimate. Cancel with `ats cancel {}` and resubmit with work that fits the budget.",
        sub.id, sub.job, sub.id, sub.id,
    );
    ExitCode::from(EXIT_DETACHED)
}

pub async fn wait(paths: &Paths, submission: &str, wait: Option<Duration>) -> Result<ExitCode> {
    let (reached, sub) = wait_for(paths, submission, WaitUntil::Terminal, wait).await?;
    if sub.job_state == JobState::Blocked {
        return blocked(paths, &sub).await;
    }
    if sub.job_state == JobState::CapacityInsufficient {
        return Ok(report_capacity(&sub));
    }
    if reached {
        print_output(&sub)?;
        eprintln!("{}", terminal_line(&sub));
        delivered(paths, &sub).await;
        return Ok(terminal_exit_code(&sub));
    }
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::from(EXIT_DETACHED))
}

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

async fn log_target(paths: &Paths, id: &str) -> Result<(Option<PathBuf>, JobState, String)> {
    let job: Job = if id.starts_with("j_") {
        get(paths, &format!("/v1/jobs/{id}")).await?
    } else {
        let sub: Submission = get(paths, &format!("/v1/submissions/{id}")).await?;
        get(paths, &format!("/v1/jobs/{}", sub.job)).await?
    };
    Ok((job.log_path.map(PathBuf::from), job.state, job.id))
}

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
            if let Some(dir) = &dir {
                print_capture_errors(dir);
            }
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(FOLLOW_POLL).await;
    }
}

pub async fn mark(paths: &Paths, submission: &str, action: &str) -> Result<ExitCode> {
    post(
        paths,
        &format!("/v1/submissions/{submission}/{action}"),
        json!({}),
    )
    .await?;
    let sub: Submission = get(paths, &format!("/v1/submissions/{submission}")).await?;
    eprintln!("{}", status_line(&sub));
    if action == "detach" && !sub.job_state.terminal() {
        eprintln!(
            "[ats] detached: reminders arrive at a later agent turn; this does not wake a stopped agent. Keep using `ats wait {submission}` if the current goal needs its result."
        );
    }
    Ok(ExitCode::SUCCESS)
}

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

fn explain(v: &Verdict) -> String {
    match v {
        Verdict::Admit => "it is being admitted now".into(),
        Verdict::Blocked { job } => {
            format!("predecessor {job} failed; `ats unblock` runs it anyway")
        }
        Verdict::CapacityInsufficient { need, budget } => format!(
            "it is predicted to need {}, more than the memory budget of {}; cancel it and resubmit with work that fits the budget",
            gib(*need as f64),
            gib(*budget as f64)
        ),
        Verdict::Wait { reason } => match reason {
            Reason::Recovering => {
                "the daemon restarted and is waiting for running jobs to reconnect".into()
            }
            Reason::ResourceUnavailable => {
                "its execution memory limits could not be read; Linux requires visible cgroup v2 membership and mounts"
                    .into()
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

pub async fn why(paths: &Paths, id: &str) -> Result<ExitCode> {
    let job = job_of(paths, id).await?;
    let why: WhyResponse = get(paths, &format!("/v1/jobs/{job}/why")).await?;
    match &why.verdict {
        Some(v) => println!("job {job} is {}: {}", why.state, explain(v)),
        None => println!("job {job} is {}", why.state),
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn unblock(paths: &Paths, id: &str) -> Result<ExitCode> {
    let job = job_of(paths, id).await?;
    post(paths, &format!("/v1/jobs/{job}/unblock"), json!({})).await?;
    println!("job {job} is queued again");
    Ok(ExitCode::SUCCESS)
}
