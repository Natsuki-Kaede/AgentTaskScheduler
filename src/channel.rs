//! Exchanges daemon requests and responses through files.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tower::ServiceExt;

use crate::timing::Deadline;

const POLL: Duration = Duration::from_millis(100);
const RETRY: Duration = Duration::from_secs(1);
const STALE_AFTER: Duration = Duration::from_secs(120);
const TRACKED_PREFIX: &str = "r2_";

#[derive(Debug, Serialize, Deserialize)]
pub struct FileRequest {
    pub method: String,
    pub path: String,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

use crate::paths::create_private_dir;

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub async fn serve(dir: PathBuf, router: Router) {
    let mut pending = JoinSet::new();
    loop {
        while let Some(result) = pending.try_join_next() {
            if let Err(error) = result {
                tracing::warn!("request file handler: {error}");
            }
        }
        if let Err(error) = scan(&dir, &router, &mut pending) {
            tracing::warn!("scanning request directory {}: {error:#}", dir.display());
            tokio::time::sleep(RETRY).await;
        } else {
            tokio::time::sleep(POLL).await;
        }
    }
}

fn scan(dir: &Path, router: &Router, pending: &mut JoinSet<()>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            create_private_dir(dir)?;
            std::fs::read_dir(dir)?
        }
        result => result?,
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!("reading entry in {}: {error}", dir.display());
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        let tracked = name.starts_with(TRACKED_PREFIX.as_bytes());
        if !(tracked || name.starts_with(b"r_")) {
            continue;
        }
        let stale = || {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|at| at.elapsed().is_ok_and(|age| age >= STALE_AFTER))
        };
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("resp" | "tmp") if stale() => {
                let _ = std::fs::remove_file(&path);
            }
            Some("req") if !tracked && stale() => {
                let _ = std::fs::remove_file(&path);
            }
            Some("req") => match claim(&path, tracked) {
                Ok(Some((file, raw))) => {
                    let router = router.clone();
                    pending.spawn(async move {
                        if let Err(error) =
                            answer(&path, tracked.then_some(file), raw, &router).await
                        {
                            tracing::warn!("request file {}: {error:#}", path.display());
                        }
                    });
                }
                Ok(None) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => tracing::warn!("request file {}: {error}", path.display()),
            },
            _ => {}
        }
    }
    Ok(())
}

fn client_waiting(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn claim(path: &Path, tracked: bool) -> io::Result<Option<(File, Vec<u8>)>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    if tracked && !client_waiting(&file)? {
        std::fs::remove_file(path)?;
        return Ok(None);
    }
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    std::fs::remove_file(path)?;
    Ok(Some((file, raw)))
}

async fn answer(path: &Path, client: Option<File>, raw: Vec<u8>, router: &Router) -> Result<()> {
    let resp = match serde_json::from_slice::<FileRequest>(&raw) {
        Ok(req) => dispatch(router, req).await?,
        Err(e) => FileResponse {
            status: 400,
            body: serde_json::json!({ "error": e.to_string() }),
        },
    };
    let response = path.with_extension("resp");
    write_atomically(&response, &serde_json::to_vec(&resp)?)?;
    if let Some(client) = client
        && !client_waiting(&client)?
    {
        let _ = std::fs::remove_file(response);
    }
    Ok(())
}

async fn dispatch(router: &Router, req: FileRequest) -> Result<FileResponse> {
    let body = req
        .body
        .map(|b| Body::from(b.to_string()))
        .unwrap_or_else(Body::empty);
    let http = axum::http::Request::builder()
        .method(req.method.as_str())
        .uri(req.path)
        .header("content-type", "application/json")
        .body(body)?;
    let resp = router.clone().oneshot(http).await?;
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Ok(FileResponse { status, body })
}

pub async fn request(dir: &Path, req: &FileRequest, timeout: Duration) -> Result<FileResponse> {
    let id = crate::ids::new("r2");
    let req_path = dir.join(format!("{id}.req"));
    let files = RequestFiles {
        response: req_path.with_extension("resp"),
        request: req_path,
    };
    let _client = files
        .publish(req)
        .with_context(|| format!("writing to {}", dir.display()))?;
    let deadline = Deadline::after(Some(timeout));
    while !deadline.expired() {
        if let Ok(raw) = std::fs::read(&files.response) {
            return Ok(serde_json::from_slice(&raw)?);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("no answer through {} within {timeout:?}", dir.display())
}

struct RequestFiles {
    request: PathBuf,
    response: PathBuf,
}

impl RequestFiles {
    fn publish(&self, req: &FileRequest) -> Result<File> {
        let mut pending = tempfile::Builder::new()
            .prefix(TRACKED_PREFIX)
            .suffix(".tmp")
            .tempfile_in(self.request.parent().expect("request directory"))?;
        pending.as_file().try_lock()?;
        serde_json::to_writer(&mut pending, req)?;
        Ok(pending.persist(&self.request)?)
    }
}

impl Drop for RequestFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.request);
        let _ = std::fs::remove_file(&self.response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new().route(
            "/v1/ping",
            get(|| async { axum::Json(serde_json::json!({"pong": true})) }),
        );
        let server = tokio::spawn(serve(dir.path().to_path_buf(), router));
        let req = FileRequest {
            method: "GET".into(),
            path: "/v1/ping".into(),
            body: None,
        };
        let resp = request(dir.path(), &req, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["pong"], true);
        server.abort();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "channel files are cleaned up"
        );
    }

    #[tokio::test]
    async fn a_slow_request_does_not_hold_up_others() {
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    "slow"
                }),
            )
            .route("/fast", get(|| async { "fast" }));
        let server = tokio::spawn(serve(dir.path().to_path_buf(), router));
        let get = |path: &str| FileRequest {
            method: "GET".into(),
            path: path.into(),
            body: None,
        };
        let slow_dir = dir.path().to_path_buf();
        let slow = tokio::spawn(async move {
            request(&slow_dir, &get("/slow"), Duration::from_secs(10)).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let fast = request(dir.path(), &get("/fast"), Duration::from_secs(1)).await;
        assert_eq!(fast.unwrap().status, 200);
        assert_eq!(slow.await.unwrap().unwrap().status, 200);
        server.abort();
    }

    #[tokio::test]
    async fn times_out_and_withdraws_without_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let req = FileRequest {
            method: "GET".into(),
            path: "/v1/ping".into(),
            body: None,
        };
        assert!(
            request(dir.path(), &req, Duration::from_millis(200))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn canceled_client_withdraws_unclaimed_and_claimed_requests() {
        let dir = tempfile::tempdir().unwrap();
        let req = FileRequest {
            method: "GET".into(),
            path: "/wait".into(),
            body: None,
        };
        let start = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let finished = std::sync::Arc::new(AtomicUsize::new(0));
        let router = Router::new().route("/wait", {
            let (start, release, finished) = (start.clone(), release.clone(), finished.clone());
            get(move || {
                let (start, release, finished) = (start.clone(), release.clone(), finished.clone());
                async move {
                    start.notify_one();
                    release.notified().await;
                    finished.fetch_add(1, Ordering::SeqCst);
                }
            })
        });
        let mut unclaimed = Box::pin(request(dir.path(), &req, Duration::from_secs(5)));
        tokio::select! {
            _ = &mut unclaimed => panic!("request finished before it was claimed"),
            _ = tokio::time::sleep(POLL) => {}
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        drop(unclaimed);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let orphan = dir.path().join("r2_orphan.req");
        std::fs::write(&orphan, serde_json::to_vec(&req).unwrap()).unwrap();
        let server = tokio::spawn(serve(dir.path().to_path_buf(), router));
        let mut claimed = Box::pin(request(dir.path(), &req, Duration::from_secs(5)));
        tokio::select! {
            _ = &mut claimed => panic!("request finished before its handler was released"),
            result = tokio::time::timeout(Duration::from_secs(2), start.notified()) => result.unwrap(),
        }
        assert!(!orphan.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        drop(claimed);
        release.notify_one();
        tokio::time::sleep(POLL * 3).await;
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        server.abort();
    }

    #[tokio::test]
    async fn recovers_from_a_directory_scan_failure() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("requests");
        std::fs::write(&dir, "temporary obstruction").unwrap();
        let router = Router::new().route("/ping", get(|| async { axum::Json("pong") }));
        let server = tokio::spawn(serve(dir.clone(), router));
        tokio::time::sleep(POLL * 2).await;
        std::fs::remove_file(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        let req = FileRequest {
            method: "GET".into(),
            path: "/ping".into(),
            body: None,
        };
        assert_eq!(
            request(&dir, &req, Duration::from_secs(3))
                .await
                .unwrap()
                .body,
            "pong"
        );
        server.abort();
    }
}
