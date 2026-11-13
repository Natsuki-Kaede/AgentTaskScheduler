//! Exercises CLI handoff and wait behavior through the public entry points.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn ats(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ats"));
    cmd.env("ATS_HOME", home)
        .env_remove("ATS_JOB_ID")
        .env_remove("CARGO_HOME")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR");
    cmd
}

#[tokio::test]
async fn a_post_acceptance_disconnect_keeps_both_ids() {
    use axum::body::{Body, Bytes, HttpBody};
    use axum::http::{Response, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    struct Disconnect;

    impl HttpBody for Disconnect {
        type Data = Bytes;
        type Error = std::io::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(Some(Err(std::io::Error::other("simulated disconnect"))))
        }
    }

    fn disconnect() -> Response<Body> {
        Response::new(Body::new(Disconnect))
    }

    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::UnixListener::bind(dir.path().join("ats.sock")).unwrap();
    let waited = Arc::new(AtomicBool::new(false));
    let app = axum::Router::new()
        .route(
            "/v1/jobs",
            post(|| async {
                axum::Json(serde_json::json!({"job": "j_accepted", "submission": "s_accepted"}))
            }),
        )
        .route(
            "/v1/jobs/{id}/executor",
            post({
                let waited = waited.clone();
                move || {
                    let waited = waited.clone();
                    async move {
                        if waited.load(Ordering::SeqCst) {
                            StatusCode::CONFLICT.into_response()
                        } else {
                            disconnect()
                        }
                    }
                }
            }),
        )
        .route(
            "/v1/submissions/{id}/wait",
            post({
                let waited = waited.clone();
                move || {
                    let waited = waited.clone();
                    async move {
                        waited.store(true, Ordering::SeqCst);
                        disconnect()
                    }
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = dir.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        ats(&home)
            .args(["run", "--no-wait", "--lock", "handoff", "--", "true"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let saw_wait = waited.swap(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        while dir.path().join("executors/j_accepted.log").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("executor did not exit");
    assert_eq!(output.status.code(), Some(78), "{output:?}");
    assert!(
        saw_wait,
        "registration failure stopped the launcher before its first wait"
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("submission s_accepted job j_accepted was accepted"),
        "{error}"
    );
    assert!(error.contains("ats wait s_accepted"), "{error}");
    server.abort();
}

#[test]
fn explicit_memory_hints_and_capacity_waits_are_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = ats(dir.path())
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _daemon = Daemon(daemon);
    {
        let started = Instant::now();
        while !dir.path().join("ats.sock").exists() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "daemon did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let invalid = ats(dir.path())
            .args(["run", "--hint", "bad", "--", "echo", "unexpected"])
            .output()
            .unwrap();
        assert_eq!(invalid.status.code(), Some(77), "{invalid:?}");
        assert!(invalid.stdout.is_empty());

        let started = Instant::now();
        let held = ats(dir.path())
            .args([
                "run",
                "--wait",
                "30s",
                "--hint",
                "mem=10000000G",
                "--",
                "echo",
                "unexpected",
            ])
            .output()
            .unwrap();
        assert_eq!(held.status.code(), Some(75), "{held:?}");
        assert!(held.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&held.stderr);
        assert!(
            stderr.contains("capacity_insufficient") && stderr.contains("ats why"),
            "{stderr}"
        );
        let submission = stderr
            .split_whitespace()
            .find(|part| part.starts_with("s_"))
            .unwrap();
        let wait = ats(dir.path())
            .args(["wait", submission, "--wait", "30s"])
            .output()
            .unwrap();
        assert_eq!(wait.status.code(), Some(75), "{wait:?}");
        assert!(String::from_utf8_lossy(&wait.stderr).contains("resubmit with work that fits"));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "capacity wait used the full budget"
        );
        let why = ats(dir.path()).args(["why", submission]).output().unwrap();
        assert!(String::from_utf8_lossy(&why.stdout).contains("resubmit with work that fits"));

        assert!(
            ats(dir.path())
                .args(["cancel", submission])
                .output()
                .unwrap()
                .status
                .success()
        );
        let ended = ats(dir.path())
            .args(["wait", submission, "--wait", "5s"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&ended.stderr).contains("is canceled"));
    }
}

#[tokio::test]
async fn launch_waits_for_the_daemon_and_preserves_start_failure() {
    use axum::Json;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tokio::sync::{Notify, oneshot};

    for (decision, command_exists, unknown_job) in [
        (StatusCode::NO_CONTENT, true, false),
        (StatusCode::CONFLICT, true, false),
        (StatusCode::NO_CONTENT, false, false),
        (StatusCode::NOT_FOUND, true, false),
        (StatusCode::NOT_FOUND, true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::UnixListener::bind(dir.path().join("ats.sock")).unwrap();
        let (witness, authorized, finished) = (
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            Arc::new(std::sync::Mutex::new(None)),
        );
        let (tx, rx) = oneshot::channel::<Value>();
        *finished.lock().unwrap() = Some(tx);
        let app = axum::Router::new()
            .route(
                "/v1/jobs/j_gate/executor",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/jobs/j_gate/lease",
                post(|| async { Json(json!({"granted": true, "parallelism": 1, "env": []})) }),
            )
            .route(
                "/v1/jobs/j_gate/started",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/jobs/j_gate/main-result",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/jobs/j_gate/launch",
                post({
                    let (witness, authorized) = (witness.clone(), authorized.clone());
                    move |Json(body): Json<Value>| {
                        let (witness, authorized) = (witness.clone(), authorized.clone());
                        async move {
                            assert!(body["pid"].as_i64().unwrap() > 0);
                            assert!(!body["start"].as_str().unwrap().is_empty());
                            witness.notify_one();
                            authorized.notified().await;
                            if unknown_job {
                                (decision, Json(json!({"error": "no job j_gate"}))).into_response()
                            } else {
                                decision.into_response()
                            }
                        }
                    }
                }),
            )
            .route(
                "/v1/jobs/j_gate/finished",
                post(move |Json(body): Json<Value>| {
                    let finished = finished.clone();
                    async move {
                        finished.lock().unwrap().take().unwrap().send(body).unwrap();
                        StatusCode::NO_CONTENT
                    }
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let marker = dir.path().join("command-started");
        let spec = if command_exists {
            json!({"entry": "argv", "argv": ["/bin/sh", "-c", "touch command-started; exit 127"]})
        } else {
            json!({"entry": "argv", "argv": [dir.path().join("missing-command")]})
        };
        let mut executor = tokio::process::Command::from(ats(dir.path()));
        let mut executor = executor
            .args(["__executor", "--job", "j_gate", "--spec", &spec.to_string()])
            .current_dir(dir.path())
            .kill_on_drop(true)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), witness.notified())
            .await
            .expect("executor did not register the gated worker");
        assert!(!marker.exists());
        authorized.notify_one();
        if unknown_job {
            let status = tokio::time::timeout(Duration::from_secs(5), executor.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(status.code(), Some(78));
            assert!(!marker.exists());
            server.abort();
            continue;
        }
        let report = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("executor did not report completion")
            .unwrap();
        let status = executor.wait().await.unwrap();
        assert!(status.success(), "{status}");
        let canceled = decision == StatusCode::CONFLICT;
        let expected = if canceled {
            "canceled"
        } else if command_exists {
            "completed"
        } else {
            "start_failed"
        };
        assert_eq!(report["state"], expected);
        assert_eq!(report["exit_code"].as_i64(), (!canceled).then_some(127));
        assert_eq!(marker.exists(), command_exists && !canceled);
        server.abort();
    }
}
