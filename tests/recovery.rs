//! Fault injection against the real binary (design 7.1): the daemon or an
//! executor is killed while a job runs.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Home {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Home {
    fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        use std::io::{Read, Write};
        let mut socket =
            std::os::unix::net::UnixStream::connect(self.path.join("ats.sock")).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let payload = body.map(|value| value.to_string()).unwrap_or_default();
        write!(socket, "{method} {path} HTTP/1.1\r\nHost: ats\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len()).unwrap();
        socket.write_all(payload.as_bytes()).unwrap();
        let mut bytes = String::new();
        socket.read_to_string(&mut bytes).unwrap();
        let (header, body) = bytes.split_once("\r\n\r\n").unwrap();
        let status = header.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            status,
            if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(body).unwrap()
            },
        )
    }

    fn get(&self, path: &str) -> Value {
        let (status, body) = self.request("GET", path, None);
        assert_eq!(status, 200, "{body}");
        body
    }

    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home");
        Home { _dir: dir, path }
    }

    fn ats(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ats"))
            .args(args)
            .env("ATS_HOME", &self.path)
            // Tests may themselves run inside an ats job.
            .env_remove("ATS_JOB_ID")
            .output()
            .unwrap()
    }

    fn daemon(&self) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_ats"))
            .arg("daemon")
            .env("ATS_HOME", &self.path)
            .env_remove("ATS_JOB_ID")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let socket = self.path.join("ats.sock");
        wait_until("daemon socket", Duration::from_secs(15), || {
            socket.exists() && self.ats(&["status"]).status.success()
        });
        Daemon(child)
    }

    fn job(&self, id: &str) -> Value {
        let out = self.ats(&["status", id, "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

struct Daemon(Child);

impl Daemon {
    fn kill(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Submits without waiting and returns the submission and job ids.
fn submit(home: &Home, source: &str) -> (String, String) {
    submit_with_options(home, source, &[])
}

fn submit_with_options(home: &Home, source: &str, options: &[&str]) -> (String, String) {
    let mut args = vec!["run", "--no-wait", "--sh", source];
    args.extend_from_slice(options);
    let out = home.ats(&args);
    assert_eq!(out.status.code(), Some(75));
    let line = String::from_utf8(out.stderr).unwrap();
    let id = |prefix: &str| {
        line.split_whitespace()
            .find(|w| w.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} id in {line:?}"))
            .to_string()
    };
    (id("s_"), id("j_"))
}

#[test]
fn light_commands_honor_explicit_dependencies_and_locks() {
    let home = Home::new();
    let _daemon = home.daemon();
    let (failed, _) = submit(&home, "perl -e 'exit 9'");
    assert_eq!(
        home.ats(&["wait", &failed, "--wait", "10s"]).status.code(),
        Some(9)
    );
    let out = home.ats(&["run", "--after", &failed, "--", "echo", "must not run"]);
    assert_eq!(out.status.code(), Some(76), "{out:?}");
    assert!(out.stdout.is_empty());
    let report = String::from_utf8(out.stderr).unwrap();
    let id = |prefix: &str| {
        report
            .split_whitespace()
            .find(|word| word.starts_with(prefix))
            .unwrap()
    };
    let executor = home.job(id("j_"))["executor_pid"].as_i64().unwrap();
    assert!(home.ats(&["cancel", id("s_")]).status.success());
    wait_until("blocked executor exit", Duration::from_secs(5), || {
        let output = Command::new("ps")
            .args(["-o", "state=", "-p", &executor.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8(output.stdout).unwrap();
        state.trim().is_empty() || state.trim_start().starts_with('Z')
    });

    let (holding, job) = submit_with_options(&home, "perl -e 'sleep 30'", &["--lock", "shared"]);
    wait_until("lock holder", Duration::from_secs(5), || {
        home.job(&job)["state"] == "running"
    });
    let (waiting, waiter) = submit_with_options(&home, "echo ready", &["--lock", "shared"]);
    assert_eq!(home.job(&waiter)["state"], "queued");
    assert!(home.ats(&["cancel", &holding]).status.success());
    assert_eq!(
        home.ats(&["wait", &holding, "--wait", "10s"]).status.code(),
        Some(1)
    );
    let out = home.ats(&["wait", &waiting, "--wait", "10s"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"ready\n");
}

fn log(home: &Home, job: &str) -> String {
    let dir = home.job(job)["log_path"].as_str().unwrap().to_string();
    std::fs::read_to_string(Path::new(&dir).join("stdout.log")).unwrap()
}

#[test]
fn a_job_survives_a_daemon_crash_and_reports_after_the_restart() {
    let home = Home::new();
    let daemon = home.daemon();
    let (sub, job) = submit(&home, "perl -e 'sleep 2'; echo done");
    wait_until("job start", Duration::from_secs(5), || {
        home.job(&job)["state"] == "running"
    });

    daemon.kill();
    // The job ends while the daemon is down; its executor keeps the result
    // until the daemon is back.
    std::thread::sleep(Duration::from_secs(3));
    let _daemon = home.daemon();

    let out = home.ats(&["wait", &sub, "--wait", "20s"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = home.job(&job);
    assert_eq!(
        (row["state"].as_str(), row["exit_code"].as_i64()),
        (Some("completed"), Some(0))
    );
    assert_eq!(log(&home, &job), "done\n");
}

#[test]
fn a_lost_executor_holds_the_job_until_its_processes_exit() {
    let home = Home::new();
    let _daemon = home.daemon();
    let marker = home.path.join("command-started");
    let (_, job) = submit(
        &home,
        &format!("echo started > {}; perl -e 'sleep 3'", marker.display()),
    );
    wait_until("job start", Duration::from_secs(5), || {
        home.job(&job)["state"] == "running" && marker.exists()
    });

    let executor = home.job(&job)["executor_pid"].as_i64().unwrap() as i32;
    // SAFETY: kills the executor started for this test.
    assert_eq!(unsafe { libc::kill(executor, libc::SIGKILL) }, 0);

    wait_until("awaiting_takeover", Duration::from_secs(5), || {
        home.job(&job)["state"] == "awaiting_takeover"
    });
    // The command was not reported, so its exit code is unknown.
    wait_until("ended_unknown", Duration::from_secs(10), || {
        home.job(&job)["state"] == "ended_unknown"
    });
    assert!(home.job(&job)["exit_code"].is_null());
    // The killed executor could not remove its error log.
    let _ = std::fs::remove_file(home.path.join("executors").join(format!("{job}.log")));
}

struct Worker {
    pid: Option<i32>,
    home: PathBuf,
    submission: String,
}

impl Worker {
    fn kill(&mut self) {
        if let Some(pid) = self.pid.take() {
            // SAFETY: the worker sleeps for a minute and belongs to this test.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
        if std::thread::panicking() {
            // Keep failure cleanup inside this test's daemon and data directory.
            let _ = std::fs::write(self.home.join("release-parent"), "release");
            for args in [
                vec!["cancel", &self.submission],
                vec!["wait", &self.submission, "--wait", "5s"],
            ] {
                let _ = Command::new(env!("CARGO_BIN_EXE_ats"))
                    .args(args)
                    .env("ATS_HOME", &self.home)
                    .env_remove("ATS_JOB_ID")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    }
}

fn escaped_worker(home: &Home, worker_source: &str) -> (String, String, Worker) {
    let marker = home.path.join("escaped-worker.pid");
    let release = home.path.join("release-parent");
    let source = format!(
        "import pathlib,subprocess,sys,time; child=subprocess.Popen([sys.executable,'-c',{}],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); pathlib.Path({}).write_text(str(child.pid))\nwhile not pathlib.Path({}).exists(): time.sleep(.05)",
        serde_json::to_string(worker_source).unwrap(),
        serde_json::to_string(&marker.to_string_lossy()).unwrap(),
        serde_json::to_string(&release.to_string_lossy()).unwrap(),
    );
    let quoted = format!("'{}'", source.replace('\'', "'\\''"));
    let (sub, job) = submit_with_options(
        home,
        &format!("exec python3 -c {quoted}"),
        &["--lock", "escaped"],
    );
    wait_until("worker pid", Duration::from_secs(5), || marker.exists());
    let worker = Worker {
        pid: Some(std::fs::read_to_string(marker).unwrap().parse().unwrap()),
        home: home.path.clone(),
        submission: sub.clone(),
    };
    let pid = worker.pid.unwrap();
    wait_until("confirmed worker", Duration::from_secs(5), || {
        home.get("/v1/measurements")["jobs"][&job]["processes"]
            .as_array()
            .is_some_and(|pids| pids.iter().any(|p| p.as_i64() == Some(i64::from(pid))))
    });
    std::fs::write(release, "release").unwrap();
    wait_until("parent result", Duration::from_secs(5), || {
        home.job(&job)["main_exit_code"] == 0
    });
    (sub, job, worker)
}

#[test]
fn restart_then_executor_loss_retains_an_escaped_worker_and_its_lock() {
    let home = Home::new();
    let daemon = home.daemon();
    let (sub, job, mut worker) = escaped_worker(&home, "import time; time.sleep(60)");
    let executor = home.job(&job)["executor_pid"].as_i64().unwrap() as i32;
    daemon.kill();
    let _daemon = home.daemon();
    // SAFETY: this is the still-running executor created for this job.
    assert_eq!(unsafe { libc::kill(executor, libc::SIGKILL) }, 0);
    wait_until("restored worker ownership", Duration::from_secs(5), || {
        home.job(&job)["state"] == "awaiting_takeover"
    });
    let (waiting, waiter) = submit_with_options(&home, "echo released", &["--lock", "escaped"]);
    assert_eq!(home.job(&waiter)["state"], "queued");
    assert!(home.job(&waiter)["main_exit_code"].is_null());
    let probe = serde_json::json!({"entry": "sh", "source": "echo probe", "cwd": "/", "now": true, "lock": ["escaped"]});
    let (status, rejected) = home.request("POST", "/v1/jobs", Some(probe.clone()));
    assert_eq!(status, 409, "{rejected}");
    assert!(rejected["error"].as_str().unwrap().contains(&job));
    worker.kill();
    assert!(home.ats(&["wait", &sub, "--wait", "10s"]).status.success());
    // Clean up the waiting task; admission may still be held by recovery or pressure.
    assert!(home.ats(&["cancel", &waiting]).status.success());
    assert_ne!(
        home.ats(&["wait", &waiting, "--wait", "10s"]).status.code(),
        Some(75)
    );
    let (status, created) = home.request("POST", "/v1/jobs", Some(probe));
    assert_eq!(status, 201, "{created}");
    assert!(
        home.ats(&["cancel", created["submission"].as_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn a_reparented_worker_remains_measured_and_records_later_growth() {
    let home = Home::new();
    let _daemon = home.daemon();
    let grow = home.path.join("grow-worker");
    let worker_source = format!(
        "import pathlib,time; data=bytearray(8*1024*1024)\nwhile not pathlib.Path({}).exists(): time.sleep(.05)\ndata.extend(bytearray(64*1024*1024)); time.sleep(60)",
        serde_json::to_string(&grow.to_string_lossy()).unwrap()
    );
    let (sub, job, mut worker) = escaped_worker(&home, &worker_source);
    let pid = worker.pid.unwrap();
    let before = home.job(&job)["peak_private"].as_u64().unwrap();
    std::fs::write(grow, "grow").unwrap();
    let started = Instant::now();
    loop {
        let measurements = home.get("/v1/measurements");
        let usage = &measurements["jobs"][&job];
        let row = home.job(&job);
        if usage["processes"]
            .as_array()
            .is_some_and(|pids| pids.iter().any(|p| p.as_i64() == Some(i64::from(pid))))
            && usage["private_total"]
                .as_u64()
                .is_some_and(|m| m > 48 << 20)
            && row["peak_private"].as_u64().is_some_and(|m| m > before)
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "before={before}; usage={usage}; job={row}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    worker.kill();
    assert!(home.ats(&["wait", &sub, "--wait", "10s"]).status.success());
}

#[test]
fn executor_finish_retains_daemon_members_until_the_worker_exits() {
    retained_finish(false);
}

#[test]
fn pending_finish_can_be_canceled_after_a_daemon_restart() {
    retained_finish(true);
}

fn retained_finish(cancel: bool) {
    struct Paused(Option<i32>);
    impl Paused {
        fn resume(&mut self) {
            if let Some(pid) = self.0.take() {
                // SAFETY: this isolated job's executor remains stopped until resumed.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
            }
        }
    }
    impl Drop for Paused {
        fn drop(&mut self) {
            if let Some(pid) = self.0.take() {
                // SAFETY: failure cleanup resumes only the executor owned by this guard.
                unsafe { libc::kill(pid, libc::SIGCONT) };
            }
        }
    }
    let home = Home::new();
    let mut daemon = home.daemon();
    let ready = home.path.join("review-parent-ready");
    let spawn = home.path.join("review-spawn-worker");
    let marker = home.path.join("review-worker.pid");
    let release = home.path.join("release-parent");
    let source = format!(
        "import pathlib,subprocess,sys,time; pathlib.Path({}).write_text(str(__import__('os').getpid()))\nwhile not pathlib.Path({}).exists(): time.sleep(.02)\nchild=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); pathlib.Path({}).write_text(str(child.pid))\nwhile not pathlib.Path({}).exists(): time.sleep(.02)",
        serde_json::to_string(&ready.to_string_lossy()).unwrap(),
        serde_json::to_string(&spawn.to_string_lossy()).unwrap(),
        serde_json::to_string(&marker.to_string_lossy()).unwrap(),
        serde_json::to_string(&release.to_string_lossy()).unwrap(),
    );
    let quoted = format!("'{}'", source.replace('\'', "'\\''"));
    let (sub, job) = submit_with_options(
        &home,
        &format!("exec python3 -c {quoted}"),
        &["--lock", "review-finish"],
    );
    wait_until("waiting parent", Duration::from_secs(15), || {
        std::fs::read_to_string(&ready).is_ok_and(|s| s.parse::<i64>().is_ok())
    });
    let parent: i64 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    let executor = home.job(&job)["executor_pid"].as_i64().unwrap() as i32;
    let mut worker = Worker {
        pid: None,
        home: home.path.clone(),
        submission: sub.clone(),
    };
    let mut paused = Paused(Some(executor));
    // SAFETY: this executor was registered by the job in this temporary home.
    assert_eq!(unsafe { libc::kill(executor, libc::SIGSTOP) }, 0);
    wait_until("stopped executor", Duration::from_secs(5), || {
        Command::new("ps")
            .args(["-o", "state=", "-p", &executor.to_string()])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains('T'))
    });
    std::fs::write(&spawn, "spawn").unwrap();
    wait_until("worker pid", Duration::from_secs(5), || {
        std::fs::read_to_string(&marker).is_ok_and(|s| s.parse::<i32>().is_ok())
    });
    let pid: i32 = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
    worker.pid = Some(pid);
    let members = || {
        let db = rusqlite::Connection::open_with_flags(
            home.path.join("ats.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut q = db
            .prepare("SELECT pid FROM job_processes WHERE job_id=?1")
            .unwrap();
        q.query_map([&job], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    wait_until("daemon persisted worker", Duration::from_secs(10), || {
        members().contains(&i64::from(pid))
    });
    std::fs::write(&release, "release").unwrap();
    wait_until(
        "orphaned worker observation",
        Duration::from_secs(10),
        || {
            let measured = home.get("/v1/measurements");
            measured["jobs"][&job]["processes"]
                .as_array()
                .is_some_and(|pids| {
                    pids.iter().any(|p| p.as_i64() == Some(i64::from(pid)))
                        && !pids.iter().any(|p| p.as_i64() == Some(parent))
                })
        },
    );
    let before = members();
    paused.resume();
    wait_until("main result", Duration::from_secs(5), || {
        home.job(&job)["main_exit_code"] == 0
    });
    wait_until("durable completion report", Duration::from_secs(5), || {
        let db = rusqlite::Connection::open_with_flags(
            home.path.join("ats.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.query_row("SELECT finish_state FROM jobs WHERE id=?1", [&job], |r| {
            r.get::<_, Option<String>>(0)
        })
        .unwrap()
        .as_deref()
            == Some("completed")
    });
    let row = home.job(&job);
    // SAFETY: signal zero only checks the sleeping worker owned by this test.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    let after = members();
    let (status, created) = home.request(
        "POST",
        "/v1/jobs",
        Some(serde_json::json!({
            "entry":"sh","source":"echo probe","cwd":"/","now":true,"lock":["review-finish"]
        })),
    );
    if status == 201 {
        assert!(
            home.ats(&["cancel", created["submission"].as_str().unwrap()])
                .status
                .success()
        );
    }
    eprintln!(
        "finish report regression: worker_alive={alive}; state={}; members_before={before:?}; members_after={after:?}; conflicting_now_status={status}",
        row["state"]
    );
    assert!(
        alive,
        "the worker must still have been running at the observation point"
    );
    assert!(
        !matches!(
            row["state"].as_str(),
            Some("completed" | "canceled" | "ended_unknown" | "start_failed")
        ),
        "an executor's incomplete local family must not override the daemon's confirmed worker"
    );
    assert_eq!(status, 409, "the live worker must retain its resource lock");
    assert_eq!(after, before);

    daemon.kill();
    daemon = home.daemon();
    assert!(!matches!(
        home.job(&job)["state"].as_str(),
        Some("completed" | "canceled" | "ended_unknown" | "start_failed")
    ));
    assert!(members().contains(&i64::from(pid)));
    if cancel {
        assert!(home.ats(&["cancel", &sub]).status.success());
    } else {
        worker.kill();
    }
    let output = home.ats(&["wait", &sub, "--wait", "15s"]);
    assert_ne!(output.status.code(), Some(75), "{output:?}");
    let final_job = home.job(&job);
    assert_eq!(
        final_job["state"],
        if cancel { "canceled" } else { "completed" }
    );
    assert_eq!(final_job["exit_code"], 0);
    assert!(members().is_empty());
    if cancel {
        worker.pid = None;
    }
    drop(daemon);
}
