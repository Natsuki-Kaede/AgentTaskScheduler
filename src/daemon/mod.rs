//! The daemon: one instance per user, serving the API on a unix socket and
//! through the request-file channel.

mod lock;

use anyhow::{Context, Result};
use std::future::Future;
use std::time::Duration;

use crate::api;
use crate::app::{App, monitor, recovery, scheduling};
use crate::paths::{Paths, create_private_dir};
use crate::profile::Profiles;
use crate::store::Store;

/// How long leases wait after a restart for running executors to reconnect.
/// They poll every half second, so this covers slow starts.
const RECONNECT_GRACE: Duration = Duration::from_secs(10);
const SWEEP_EVERY: Duration = Duration::from_secs(1);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// Runs the daemon until `shutdown` completes.
pub async fn run(paths: Paths, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
    create_private_dir(&paths.home)?;
    create_private_dir(&paths.logs)?;
    create_private_dir(&paths.temporary)?;
    let _lock = lock::acquire(&paths.lock)?;

    let store = Store::open(&paths.db)?;
    let instance = crate::ids::new("d");
    let profiles = Profiles::load(Some(&paths.profiles))?;
    let config = crate::config::Config::load(&paths)?;
    let state = App::new(store, crate::timing::now(), instance)
        .with_recognition(profiles, config.recognize.wrappers);
    let registry = monitor::initialize(&state)?;
    let reconnecting = recovery::live_executors(&state, &recovery::System::new(&state))?;
    tracing::info!(
        count = reconnecting.len(),
        "waiting for running executors to reconnect"
    );
    state.expect_reconnects(reconnecting.into_iter().collect(), RECONNECT_GRACE);
    let router = api::router(state.clone());

    // Holding the lock means no other daemon owns the socket file, so a
    // leftover one is stale.
    let _ = std::fs::remove_file(&paths.socket);
    let listener = tokio::net::UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    set_mode(&paths.socket, 0o600)?;

    let channel = tokio::spawn(crate::channel::serve(
        paths.requests.clone(),
        router.clone(),
    ));
    let watcher = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut terminating = std::collections::HashMap::new();
            loop {
                let procs = recovery::System::new(&state);
                if let Err(e) = recovery::sweep(&state, &procs, &mut terminating) {
                    tracing::error!("checking executors: {e:#}");
                }
                tokio::time::sleep(SWEEP_EVERY).await;
            }
        })
    };
    let sampling = {
        let state = state.clone();
        std::thread::spawn(move || monitor::sample_loop(&state, registry))
    };
    let scheduling = tokio::spawn(scheduling::run(state.clone()));
    let pruning = {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = monitor::prune(&state) {
                    tracing::error!("pruning history: {e:#}");
                }
                tokio::time::sleep(PRUNE_EVERY).await;
            }
        })
    };
    tracing::info!(socket = %paths.socket.display(), requests = %paths.requests.display(), "ats daemon started");

    let stopping = state.clone();
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown.await;
            stopping.stop();
        })
        .await;
    watcher.abort();
    scheduling.abort();
    pruning.abort();
    channel.abort();
    let _ = sampling.join();
    let _ = std::fs::remove_file(&paths.socket);
    tracing::info!("ats daemon stopped");
    served.context("serving the API")
}

/// Resolves when the process receives SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn set_mode(path: &std::path::Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::client::{self, Transport};
    use std::time::Duration;

    pub(crate) fn test_paths(dir: &std::path::Path) -> Paths {
        Paths::with(dir.join("home"), dir.join("requests"))
    }

    pub(crate) async fn wait_for_socket(paths: &Paths) {
        for _ in 0..100 {
            if paths.socket.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon did not start");
    }

    #[tokio::test]
    async fn serves_socket_and_request_files_and_refuses_a_second_instance() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(run(paths.clone(), async {
            let _ = stop_rx.await;
        }));
        wait_for_socket(&paths).await;

        let (health, transport) = client::call(&paths, "GET", "/v1/health", None)
            .await
            .unwrap();
        assert_eq!((health.status, transport), (200, Transport::Socket));

        let body = serde_json::json!({"entry": "argv", "argv": ["cargo", "build"], "cwd": "/w"});
        let (created, _) = client::call(&paths, "POST", "/v1/jobs", Some(body))
            .await
            .unwrap();
        assert_eq!(created.status, 201);

        // A second daemon on the same home must refuse to start.
        let second = run(paths.clone(), async {}).await.unwrap_err();
        assert!(
            second.to_string().contains("another ats daemon is running"),
            "{second:#}"
        );

        // With the socket gone, the client falls back to request files.
        std::fs::remove_file(&paths.socket).unwrap();
        let (jobs, transport) = client::call(&paths, "GET", "/v1/jobs", None).await.unwrap();
        assert_eq!(transport, Transport::RequestFile);
        assert_eq!(jobs.body.as_array().unwrap().len(), 1);

        stop_tx.send(()).unwrap();
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn persisted_jobs_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        for round in 0..2 {
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            let daemon = tokio::spawn(run(paths.clone(), async {
                let _ = stop_rx.await;
            }));
            wait_for_socket(&paths).await;
            if round == 0 {
                let body = serde_json::json!({"entry": "sh", "source": "make", "cwd": "/w"});
                client::call(&paths, "POST", "/v1/jobs", Some(body))
                    .await
                    .unwrap();
            } else {
                let (jobs, _) = client::call(&paths, "GET", "/v1/jobs", None).await.unwrap();
                assert_eq!(jobs.body[0]["command"], "make");
            }
            stop_tx.send(()).unwrap();
            daemon.await.unwrap().unwrap();
        }
    }
}
