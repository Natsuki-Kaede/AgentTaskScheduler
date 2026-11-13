//! Tests agent reminders and submissions through the CLI.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn ats(home: &std::path::Path, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ats"))
        .args(args)
        .env("ATS_HOME", home)
        .env_remove("ATS_JOB_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("DSH_SESSION_ID")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn runs_light_commands_directly() {
    let dir = tempfile::tempdir().unwrap();
    let out = ats(dir.path(), &["run", "--sh", "echo light; exit 4"], "", &[]);
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "light\n");
    let out = ats(dir.path(), &["run", "--", "ls", "/"], "", &[]);
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn runs_directly_inside_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let out = ats(
        dir.path(),
        &["run", "--sh", "echo nested; exit 7"],
        "",
        &[("ATS_JOB_ID", "j_parent")],
    );
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "nested\n");
}

#[test]
fn a_connected_unresponsive_daemon_does_not_hold_the_stop_hook() {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let listener = std::os::unix::net::UnixListener::bind(dir.path().join("ats.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut bytes = [0; 4096];
        assert!(socket.read(&mut bytes).unwrap() > 0);
        while socket.read(&mut bytes).unwrap() != 0 {}
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_ats"))
        .args(["hook", "codex", "stop"])
        .env("ATS_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"session_id\":\"timeout-test\",\"stop_hook_active\":false}")
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let finished = child.try_wait().unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(finished, "hook exceeded its request budget");
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "a failed reminder must let the agent continue"
    );
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "timed out: {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn reminds_the_session_of_its_submissions() {
    for agent in ["claude", "codex", "dsh"] {
        reminds_agent(agent);
    }
}

fn reminds_agent(agent: &str) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let hook = |event: &str, input: serde_json::Value| -> serde_json::Value {
        let out = ats(&home, &["hook", agent, event], &input.to_string(), &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_default()
    };
    let stop = |active: bool| {
        hook(
            "stop",
            serde_json::json!({"session_id": "s1", "stop_hook_active": active}),
        )
    };
    let prompt = || {
        hook(
            "user-prompt-submit",
            serde_json::json!({"session_id": "s1"}),
        )
    };

    assert!(stop(false).is_null() && prompt().is_null());

    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_ats"))
            .arg("daemon")
            .env("ATS_HOME", &home)
            .env_remove("ATS_JOB_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("daemon", || {
        ats(&home, &["status"], "", &[]).status.success()
    });
    let run = |args: &[&str]| {
        let mut all = vec!["run"];
        all.extend_from_slice(args);
        let variable = match agent {
            "dsh" => "DSH_SESSION_ID",
            "codex" => "CODEX_SESSION_ID",
            _ => "CLAUDE_CODE_SESSION_ID",
        };
        let mut env = vec![(variable, "s1")];
        if agent == "dsh" {
            env.push(("CODEX_SESSION_ID", "outer-session"));
        }
        ats(&home, &all, "", &env)
    };
    assert!(run(&["--sh", "perl -e 1; echo done"]).status.success());
    let out = run(&["--no-wait", "--sh", "perl -e 'sleep 2'"]);
    assert_eq!(out.status.code(), Some(75));
    let line = String::from_utf8_lossy(&out.stderr).to_string();
    let submission = line.split_whitespace().nth(2).unwrap().to_string();

    let blocked = stop(false);
    assert_eq!(blocked["decision"], "block");
    let reason = blocked["reason"].as_str().unwrap();
    assert!(
        reason.contains(&submission) && reason.contains("sleep 2") && !reason.contains("echo done"),
        "{reason}"
    );
    assert!(stop(true).is_null(), "a second stop goes through");
    assert!(prompt().is_null(), "nothing has ended yet");

    wait_until("job end", || stop(false).is_null());
    let context = prompt()["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        context.contains(&submission) && context.contains("exit code 0"),
        "{context}"
    );
    assert!(prompt().is_null(), "reported once");
}

#[test]
fn wait_returns_the_commands_output_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_ats"))
            .arg("daemon")
            .env("ATS_HOME", &home)
            .env_remove("ATS_JOB_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("daemon", || {
        ats(&home, &["status"], "", &[]).status.success()
    });
    let out = ats(
        &home,
        &[
            "run",
            "--no-wait",
            "--sh",
            "perl -e 'sleep 1'; echo out; echo err >&2; exit 3",
        ],
        "",
        &[],
    );
    assert_eq!(out.status.code(), Some(75));
    let line = String::from_utf8_lossy(&out.stderr).to_string();
    let submission = line.split_whitespace().nth(2).unwrap().to_string();
    let out = ats(&home, &["wait", &submission], "", &[]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("err\n") && stderr.contains("is completed, exit code 3"),
        "{stderr}"
    );
}

struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
