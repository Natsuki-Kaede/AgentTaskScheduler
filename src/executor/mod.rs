//! Runs leased jobs, captures output and reports results.

mod jobserver;
mod logwriter;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;

use crate::client;
use crate::domain::job::JobState;
use crate::domain::resource::LeaseTerms;
use crate::paths::Paths;
use crate::procinfo;
use crate::protocol::{
    ControlResponse, ExecutorRequest, FinishedRequest, LeaseRequest, LeaseResponse,
    MainResultRequest, StartedRequest,
};
use crate::runspec::RunSpec;
use logwriter::CappedLog;

const LOG_HEAD: usize = 24 << 20;
const LOG_TAIL: usize = 1 << 20;
const CONTROL_POLL: Duration = Duration::from_millis(500);
const GROUP_POLL: Duration = Duration::from_millis(250);
const TERM_GRACE: Duration = Duration::from_secs(5);

pub const JOB_ENV: &str = "ATS_JOB_ID";

const RETRY: Duration = Duration::from_millis(500);

async fn report(paths: &Paths, path: &str, body: impl serde::Serialize) -> Result<client::Answer> {
    let body = serde_json::to_value(body)?;
    loop {
        match client::call(paths, "POST", path, Some(body.clone())).await {
            Ok((answer, _)) if answer.status < 500 => return Ok(answer),
            Ok((answer, _)) => eprintln!("ats executor: {path}: daemon answered {}", answer.status),
            Err(e) => eprintln!("ats executor: {path}: {e:#}"),
        }
        tokio::time::sleep(RETRY).await;
    }
}

async fn post(paths: &Paths, path: &str, body: impl serde::Serialize) -> Result<serde_json::Value> {
    let answer = report(paths, path, body).await?;
    if !(200..300).contains(&answer.status) {
        bail!("{path}: daemon answered {}: {}", answer.status, answer.body);
    }
    Ok(answer.body)
}

async fn acquire_lease(paths: &Paths, job: &str) -> Result<Option<LeaseTerms>> {
    let pid = std::process::id() as i32;
    let start = procinfo::start_time(pid).context("reading this process's start time")?;
    let answer = report(
        paths,
        &format!("/v1/jobs/{job}/executor"),
        ExecutorRequest {
            pid: i64::from(pid),
            start,
        },
    )
    .await?;
    match answer.status {
        204 => {}
        409 => return Ok(None),
        s => bail!(
            "registering the executor: daemon answered {s}: {}",
            answer.body
        ),
    }
    loop {
        let answer = report(
            paths,
            &format!("/v1/jobs/{job}/lease"),
            LeaseRequest {
                pid: i64::from(pid),
            },
        )
        .await?;
        match answer.status {
            200 => {
                let lease: LeaseResponse = serde_json::from_value(answer.body)?;
                if lease.granted {
                    return Ok(Some(lease.terms.context("granted lease has no terms")?));
                }
            }
            409 => return Ok(None),
            s => bail!("asking for the lease: daemon answered {s}: {}", answer.body),
        }
    }
}

fn log_dir(paths: &Paths, job: &str) -> Result<PathBuf> {
    let preferred = paths.logs.join(job);
    if std::fs::create_dir_all(&preferred).is_ok() {
        return Ok(preferred);
    }
    let fallback = paths.temporary.join("logs").join(job);
    crate::paths::create_private_dir(&fallback)?;
    Ok(fallback)
}

async fn pump(mut src: impl tokio::io::AsyncRead + Unpin, path: PathBuf) -> Result<()> {
    let mut log = CappedLog::create(&path, LOG_HEAD, LOG_TAIL)?;
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        log.write(&buf[..n])?;
    }
    log.finish()?;
    Ok(())
}

fn exit_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .map(i64::from)
        .or_else(|| status.signal().map(|s| 128 + i64::from(s)))
        .unwrap_or(-1)
}

pub async fn run(paths: Paths, job: String, spec: RunSpec, cwd: PathBuf) -> Result<()> {
    let Some(terms) = acquire_lease(&paths, &job).await? else {
        return Ok(());
    };

    let dir = log_dir(&paths, &job)?;
    let mut cmd = tokio::process::Command::from(spec.command());
    if let Some(n) = terms.parallelism {
        for var in &terms.env {
            if std::env::var_os(var).is_none() {
                cmd.env(var, n.to_string());
            }
        }
    }
    let own_jobserver = ["MAKEFLAGS", "CARGO_MAKEFLAGS"]
        .iter()
        .any(|v| std::env::var_os(v).is_some());
    let pool = match terms.jobserver_tokens {
        Some(tokens) if !own_jobserver => {
            let pool = jobserver::Pool::create(&paths.temporary.join("jobserver"), &job, tokens)?;
            cmd.env("MAKEFLAGS", pool.makeflags(tokens));
            Some(pool)
        }
        _ => None,
    };
    let result = run_command(&paths, &job, cmd, &cwd, &dir).await;
    drop(pool);
    result
}

async fn run_command(
    paths: &Paths,
    job: &str,
    mut cmd: tokio::process::Command,
    cwd: &Path,
    dir: &Path,
) -> Result<()> {
    let (paths, job) = (paths.clone(), job.to_string());
    let mut leader = tokio::process::Command::new("/bin/sh")
        .args(["-c", "read -r _"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("starting the job's process group")?;
    let pgid = leader.id().context("group leader has no pid")? as i32;
    post(
        &paths,
        &format!("/v1/jobs/{job}/started"),
        StartedRequest {
            log_path: dir.to_string_lossy().into_owned(),
            pgid: Some(i64::from(pgid)),
        },
    )
    .await?;
    cmd.current_dir(cwd)
        .env(JOB_ENV, &job)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(pgid);
    let spawned = cmd.spawn();
    drop(leader.stdin.take());
    leader.wait().await?;
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            std::fs::write(
                dir.join("stderr.log"),
                format!("ats: could not start the command: {e}\n"),
            )?;
            post(
                &paths,
                &format!("/v1/jobs/{job}/finished"),
                FinishedRequest::new(JobState::StartFailed, Some(127), None),
            )
            .await?;
            return Ok(());
        }
    };
    let out = tokio::spawn(pump(
        child.stdout.take().expect("piped stdout"),
        dir.join("stdout.log"),
    ));
    let err = tokio::spawn(pump(
        child.stderr.take().expect("piped stderr"),
        dir.join("stderr.log"),
    ));

    let mut canceled = false;
    let mut family = crate::runtime::family::Family::default();
    procinfo::refresh(&mut family, pgid);
    let mut poll = tokio::time::interval(GROUP_POLL);
    let cancel = wait_for_cancel(&paths, &job);
    tokio::pin!(cancel);
    let main = loop {
        tokio::select! {
            status = child.wait() => break Some(exit_code(status?)),
            () = &mut cancel => { canceled = true; break None; }
            _ = poll.tick() => procinfo::refresh(&mut family, pgid),
        }
    };
    if canceled {
        stop_family(pgid, &mut family).await;
        let _ = child.wait().await;
        let _ = out.await;
        let _ = err.await;
        post(
            &paths,
            &format!("/v1/jobs/{job}/finished"),
            ended(JobState::Canceled, None),
        )
        .await?;
        return Ok(());
    }
    let main = main.expect("main result");

    let streams = async {
        out.await??;
        err.await??;
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(streams);
    loop {
        tokio::select! {
            result = &mut streams => { result?; break; }
            () = &mut cancel => {
                stop_family(pgid, &mut family).await;
                streams.await?;
                post(&paths, &format!("/v1/jobs/{job}/finished"), ended(JobState::Canceled, Some(main))).await?;
                return Ok(());
            }
            _ = poll.tick() => procinfo::refresh(&mut family, pgid),
        }
    }
    post(
        &paths,
        &format!("/v1/jobs/{job}/main-result"),
        MainResultRequest { exit_code: main },
    )
    .await?;

    procinfo::refresh(&mut family, pgid);
    while !family.is_empty() {
        tokio::select! {
            _ = poll.tick() => procinfo::refresh(&mut family, pgid),
            () = &mut cancel => {
                stop_family(pgid, &mut family).await;
                post(&paths, &format!("/v1/jobs/{job}/finished"), ended(JobState::Canceled, Some(main))).await?;
                return Ok(());
            }
        }
    }
    post(
        &paths,
        &format!("/v1/jobs/{job}/finished"),
        ended(JobState::Completed, Some(main)),
    )
    .await?;
    Ok(())
}

fn ended(state: JobState, exit_code: Option<i64>) -> FinishedRequest {
    // SAFETY: getrusage fills the struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    let unit = if cfg!(target_os = "linux") { 1024 } else { 1 };
    FinishedRequest::new(state, exit_code, Some(ru.ru_maxrss * unit))
}

async fn wait_for_cancel(paths: &Paths, job: &str) {
    loop {
        tokio::time::sleep(CONTROL_POLL).await;
        let Ok((answer, _)) =
            client::call(paths, "GET", &format!("/v1/jobs/{job}/control"), None).await
        else {
            continue;
        };
        if answer.status == 200
            && serde_json::from_value::<ControlResponse>(answer.body)
                .is_ok_and(|control| control.cancel)
        {
            return;
        }
    }
}

async fn stop_family(pgid: i32, family: &mut crate::runtime::family::Family) {
    let deadline = tokio::time::Instant::now() + TERM_GRACE;
    loop {
        procinfo::refresh(family, pgid);
        if family.is_empty() {
            return;
        }
        procinfo::signal(
            family,
            if tokio::time::Instant::now() < deadline {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            },
        );
        tokio::time::sleep(GROUP_POLL).await;
    }
}

pub fn error_log(paths: &Paths, job: &str) -> PathBuf {
    paths.temporary.join("executors").join(format!("{job}.log"))
}

pub fn spawn_detached(paths: &Paths, job: &str, spec: &RunSpec, cwd: &Path) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().context("locating the ats binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__executor")
        .arg("--job")
        .arg(job)
        .arg("--spec")
        .arg(serde_json::to_string(spec)?)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr({
            crate::paths::create_private_dir(&paths.temporary)?;
            let path = error_log(paths, job);
            crate::paths::create_private_dir(path.parent().expect("has a parent"))?;
            std::fs::File::create(path)?
        });
    // SAFETY: setsid is async-signal-safe and runs in the child before exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("starting the executor")?;
    Ok(child.id() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon;
    use crate::daemon::tests::{test_paths, wait_for_socket};
    use serde_json::json;

    async fn submit(paths: &Paths, source: &str) -> (String, String) {
        let body = json!({"entry": "sh", "source": source, "cwd": "/"});
        let created = post(paths, "/v1/jobs", body).await.unwrap();
        (
            created["submission"].as_str().unwrap().into(),
            created["job"].as_str().unwrap().into(),
        )
    }

    async fn job(paths: &Paths, job: &str) -> serde_json::Value {
        client::call(paths, "GET", &format!("/v1/jobs/{job}"), None)
            .await
            .unwrap()
            .0
            .body
    }

    async fn terminal_job(paths: &Paths, id: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row = job(paths, id).await;
                if row["state"]
                    .as_str()
                    .and_then(|s| s.parse::<JobState>().ok())
                    .is_some_and(JobState::terminal)
                {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon did not confirm completion")
    }

    fn sh(source: &str) -> RunSpec {
        RunSpec::Sh {
            source: source.into(),
            shell: "/bin/sh".into(),
        }
    }

    #[tokio::test]
    async fn runs_jobs_to_their_terminal_states() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let d = tokio::spawn(daemon::run(paths.clone(), async {
            let _ = stop_rx.await;
        }));
        wait_for_socket(&paths).await;

        let (_, j) = submit(&paths, "x").await;
        let spec = sh("echo out; echo err >&2; exit 3");
        run(paths.clone(), j.clone(), spec, "/".into())
            .await
            .unwrap();
        let row = terminal_job(&paths, &j).await;
        assert_eq!(
            (row["state"].as_str(), row["exit_code"].as_i64()),
            (Some("completed"), Some(3))
        );
        assert_eq!(row["main_exit_code"], 3);
        let logs = PathBuf::from(row["log_path"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(logs.join("stdout.log")).unwrap(),
            "out\n"
        );
        assert_eq!(
            std::fs::read_to_string(logs.join("stderr.log")).unwrap(),
            "err\n"
        );

        let (_, j) = submit(&paths, "x").await;
        let spec = RunSpec::Argv {
            argv: vec![
                dir.path()
                    .join("missing-command")
                    .to_string_lossy()
                    .into_owned(),
            ],
        };
        run(paths.clone(), j.clone(), spec, "/".into())
            .await
            .unwrap();
        let row = terminal_job(&paths, &j).await;
        assert_eq!(row["state"], "start_failed");
        assert_eq!(row["exit_code"], 127);
        let logs = PathBuf::from(row["log_path"].as_str().unwrap());
        assert!(
            std::fs::read_to_string(logs.join("stderr.log"))
                .unwrap()
                .contains("could not start the command")
        );

        let (_, j) = submit(&paths, "x").await;
        let spec = sh("sleep 1 >/dev/null 2>&1 & echo started");
        let task = tokio::spawn(run(paths.clone(), j.clone(), spec, "/".into()));
        let mut saw_main_while_running = false;
        while !task.is_finished() {
            let row = job(&paths, &j).await;
            if row["main_exit_code"] == 0 && row["state"] == "running" {
                saw_main_while_running = true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        task.await.unwrap().unwrap();
        assert!(saw_main_while_running);
        assert_eq!(terminal_job(&paths, &j).await["state"], "completed");

        let (s, j) = submit(&paths, "x").await;
        let marker = dir.path().join("child.pid");
        let spec = sh(&format!("sleep 30 & echo $! > {}; wait", marker.display()));
        let task = tokio::spawn(run(paths.clone(), j.clone(), spec, "/".into()));
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        post(&paths, &format!("/v1/submissions/{s}/cancel"), json!({}))
            .await
            .unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(terminal_job(&paths, &j).await["state"], "canceled");
        let child: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only checks for existence.
        assert_ne!(
            unsafe { libc::kill(child, 0) },
            0,
            "background child survived"
        );

        for redirect in ["", ">/dev/null 2>&1"] {
            let (s, j) = submit(&paths, "x").await;
            let marker = dir.path().join("background.pids");
            let _ = std::fs::remove_file(&marker);
            let spec = sh(&format!(
                "sleep 30 {redirect} & echo $$ $! > {}; exit 0",
                crate::hook::quote(&marker.to_string_lossy())
            ));
            let mut task = tokio::spawn(run(paths.clone(), j.clone(), spec, "/".into()));
            let pids = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&marker) {
                        let pids: Vec<i32> = text
                            .split_whitespace()
                            .filter_map(|p| p.parse().ok())
                            .collect();
                        if pids.len() == 2 && procinfo::start_time(pids[0]).is_none() {
                            break pids;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            post(&paths, &format!("/v1/submissions/{s}/cancel"), json!({}))
                .await
                .unwrap();
            let finished = tokio::time::timeout(Duration::from_secs(8), &mut task).await;
            if finished.is_err() {
                // SAFETY: stops the child this test started before failing.
                unsafe { libc::kill(pids[1], libc::SIGKILL) };
                task.abort();
            }
            finished.unwrap().unwrap().unwrap();
            assert_eq!(
                terminal_job(&paths, &j).await["state"],
                "canceled",
                "{redirect}"
            );
            assert!(
                procinfo::start_time(pids[1]).is_none(),
                "background child survived: {redirect}"
            );
        }

        for (parent_seconds, quiet) in [(30, false), (1, false), (1, true)] {
            let (s, j) = submit(&paths, "x").await;
            let marker = dir.path().join("escaped.pids");
            let _ = std::fs::remove_file(&marker);
            let output = if quiet { "subprocess.DEVNULL" } else { "None" };
            let source = format!(
                "import os,pathlib,subprocess,time; child=subprocess.Popen(['sleep','30'],start_new_session=True,stdout={output},stderr={output}); pathlib.Path({}).write_text(str(os.getpid())+' '+str(child.pid)); time.sleep({parent_seconds})",
                serde_json::to_string(&marker.to_string_lossy()).unwrap()
            );
            let spec = sh(&format!("exec python3 -c {}", crate::hook::quote(&source)));
            let mut task = tokio::spawn(run(paths.clone(), j.clone(), spec, "/".into()));
            let pids = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&marker) {
                        let pids: Vec<i32> = text
                            .split_whitespace()
                            .filter_map(|p| p.parse().ok())
                            .collect();
                        if pids.len() == 2 {
                            break pids;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            tokio::time::sleep(GROUP_POLL * 2).await;
            if parent_seconds == 1 {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while procinfo::start_time(pids[0]).is_some() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                assert!(
                    !task.is_finished(),
                    "escaped child must keep the job running"
                );
            }
            post(&paths, &format!("/v1/submissions/{s}/cancel"), json!({}))
                .await
                .unwrap();
            let finished = tokio::time::timeout(Duration::from_secs(8), &mut task).await;
            if finished.is_err() {
                // SAFETY: these are the two processes owned by this test.
                unsafe {
                    libc::kill(pids[0], libc::SIGKILL);
                    libc::kill(pids[1], libc::SIGKILL);
                }
                task.abort();
            }
            finished.unwrap().unwrap().unwrap();
            assert_eq!(terminal_job(&paths, &j).await["state"], "canceled");
            assert!(
                procinfo::start_time(pids[1]).is_none(),
                "escaped child survived"
            );
        }

        stop_tx.send(()).unwrap();
        d.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn executor_loss_before_registration_never_starts_the_command() {
        use axum::{Json, Router, http::StatusCode, routing::post};

        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let app = Router::new().route(
            "/v1/jobs/j_gate/started",
            post(move |Json(body): Json<serde_json::Value>| {
                let tx = tx.clone();
                async move {
                    tx.send(body["pgid"].as_i64().unwrap() as i32)
                        .await
                        .unwrap();
                    std::future::pending::<StatusCode>().await
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let marker = dir.path().join("command-started");
        let cmd = tokio::process::Command::from(
            sh(&format!(
                "echo started > {}",
                crate::hook::quote(&marker.to_string_lossy())
            ))
            .command(),
        );
        let task = tokio::spawn(async move {
            run_command(&paths, "j_gate", cmd, Path::new("/"), &paths.home).await
        });
        let pgid = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !marker.exists(),
            "command ran before registration was committed"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while procinfo::group_alive(pgid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(!marker.exists());
        server.abort();
    }
}
