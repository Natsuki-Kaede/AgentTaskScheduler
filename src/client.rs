//! Client side of the daemon API: the unix socket first, then the
//! request-file channel when the socket cannot be reached (design 4).

use std::io::ErrorKind;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;

use crate::channel::{self, FileRequest};
use crate::paths::Paths;
use crate::timing::Deadline;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    NotSent,
    Unknown,
}

impl std::fmt::Display for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotSent => "not sent",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("request deadline expired; delivery is {delivery}")]
pub struct RequestTimeout {
    pub delivery: Delivery,
    #[source]
    source: std::io::Error,
}

/// A daemon answer: HTTP status and JSON body.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: serde_json::Value,
}

/// Which transport carried a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Socket,
    RequestFile,
}

/// Sends a request to the daemon.
pub async fn call(
    paths: &Paths,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(Answer, Transport)> {
    call_with_timeout(paths, method, path, body, REQUEST_TIMEOUT).await
}

/// Like [`call`], for requests the daemon may hold open (such as a bounded
/// wait). Both transports share one budget, including response-body reads.
pub async fn call_with_timeout(
    paths: &Paths,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    timeout: Duration,
) -> Result<(Answer, Transport)> {
    let deadline = Deadline::after(Some(timeout));
    // Fallback is safe only before a connection is established: once a
    // request may have been delivered, replay could create another job.
    match deadline
        .run(tokio::net::UnixStream::connect(&paths.socket))
        .await
        .map_err(|source| RequestTimeout {
            delivery: Delivery::NotSent,
            source,
        })? {
        Ok(stream) => {
            let answer = deadline
                .run(via_socket(stream, method, path, body.as_ref()))
                .await
                .map_err(|source| RequestTimeout {
                    delivery: Delivery::Unknown,
                    source,
                })??;
            Ok((answer, Transport::Socket))
        }
        Err(e) if socket_unreachable(&e) => {
            let req = FileRequest {
                method: method.into(),
                path: path.into(),
                body,
            };
            let resp = channel::request(
                &paths.requests,
                &req,
                deadline.remaining().unwrap_or_default(),
            )
            .await
            .map_err(|ce| {
                anyhow!(
                    "daemon unreachable: socket {} ({e}); request-file channel: {ce:#}",
                    paths.socket.display()
                )
            })?;
            Ok((
                Answer {
                    status: resp.status,
                    body: resp.body,
                },
                Transport::RequestFile,
            ))
        }
        Err(e) => Err(e.into()),
    }
}

fn socket_unreachable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::PermissionDenied
    )
}

async fn via_socket(
    stream: tokio::net::UnixStream,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<Answer> {
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("HTTP handshake")?;
    let connection = ConnectionTask(tokio::spawn(conn));
    let payload = body.map(|b| Bytes::from(b.to_string())).unwrap_or_default();
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "ats")
        .header("content-type", "application/json")
        .body(Full::new(payload))?;
    let resp = sender.send_request(req).await.context("sending request")?;
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    drop(connection);
    Ok(Answer { status, body })
}

/// Dropping a timed-out request also stops its detached HTTP driver.
struct ConnectionTask(tokio::task::JoinHandle<Result<(), hyper::Error>>);

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn paths(dir: &std::path::Path) -> Paths {
        Paths::with(dir.to_path_buf(), dir.join("requests"))
    }

    #[tokio::test]
    async fn socket_timeout_covers_headers_and_body_without_replaying() {
        for partial in ["", "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = paths(dir.path());
            crate::paths::create_private_dir(&paths.requests).unwrap();
            let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 4096];
                assert!(stream.read(&mut buf).await.unwrap() > 0);
                stream.write_all(partial.as_bytes()).await.unwrap();
                // Cancellation must close the HTTP connection as well.
                while stream.read(&mut buf).await.unwrap() != 0 {}
            });
            let error = call_with_timeout(
                &paths,
                "POST",
                "/v1/jobs",
                Some(serde_json::json!({"entry": "sh", "source": "make"})),
                Duration::from_millis(150),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("delivery is unknown"),
                "{error:#}"
            );
            assert_eq!(
                error.downcast_ref::<RequestTimeout>().unwrap().delivery,
                Delivery::Unknown
            );
            assert_eq!(std::fs::read_dir(&paths.requests).unwrap().count(), 0);
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn socket_long_poll_uses_the_callers_budget() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            assert!(stream.read(&mut buf).await.unwrap() > 0);
            tokio::time::sleep(Duration::from_millis(250)).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}")
                .await
                .unwrap();
        });
        let (answer, transport) =
            call_with_timeout(&paths, "GET", "/wait", None, Duration::from_secs(2))
                .await
                .unwrap();
        assert_eq!(transport, Transport::Socket);
        assert_eq!(answer.body["ok"], true);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unreachable_socket_falls_back_and_file_timeout_withdraws() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        crate::paths::create_private_dir(&paths.requests).unwrap();
        let router = axum::Router::new().route(
            "/ping",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        );
        let server = tokio::spawn(channel::serve(paths.requests.clone(), router));
        let (_, transport) = call(&paths, "GET", "/ping", None).await.unwrap();
        assert_eq!(transport, Transport::RequestFile);
        server.abort();
        let _ = server.await;
        assert!(
            call_with_timeout(&paths, "GET", "/ping", None, Duration::from_millis(150))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&paths.requests).unwrap().count(), 0);
    }
}
