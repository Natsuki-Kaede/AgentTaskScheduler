//! The daemon's HTTP/JSON API (api/openapi.yaml). The same router serves the
//! unix socket and the request-file channel.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

mod lifecycle;

use crate::app::App;
use crate::domain::error::{Conflict, InvalidInput, NotFound};
use crate::domain::measurement::Snapshot;
use crate::domain::resource::PressureStatus;

pub fn router(state: App) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/jobs", get(list_jobs).post(submit))
        .route("/v1/submissions/{id}", get(submission))
        .route("/v1/measurements", get(measurements))
        .route("/v1/pressure", get(pressure))
        .merge(lifecycle::routes())
        .with_state(state)
}

/// An error answered as JSON with a status code.
pub struct ApiError(pub StatusCode, pub String);

impl From<InvalidInput> for ApiError {
    fn from(error: InvalidInput) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(crate::protocol::ErrorBody { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if e.downcast_ref::<InvalidInput>().is_some() {
            return ApiError(StatusCode::BAD_REQUEST, e.to_string());
        }
        if e.downcast_ref::<Conflict>().is_some() {
            return ApiError(StatusCode::CONFLICT, e.to_string());
        }
        if e.downcast_ref::<NotFound>().is_some() {
            return ApiError(StatusCode::NOT_FOUND, e.to_string());
        }
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<T, ApiError>;

async fn health(State(s): State<App>) -> Json<crate::protocol::Health> {
    Json(crate::protocol::Health {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        started_at: s.identity().0.into(),
        instance: s.identity().1.into(),
    })
}

async fn submit(
    State(s): State<App>,
    Json(req): Json<crate::protocol::SubmitRequest>,
) -> ApiResult<Response> {
    let command = crate::domain::job::SubmitCommand {
        entry: req.entry,
        argv: req.argv,
        source: req.source,
        cwd: req.cwd,
        session_id: req.session,
        launcher: req.launcher_pid.zip(req.launcher_start),
        after: req.after,
        now: req.now,
        locks: req.lock,
        hint: req.hint,
    };
    Ok((StatusCode::CREATED, Json(s.submit(command)?)).into_response())
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    state: Option<String>,
    limit: Option<u32>,
}

async fn list_jobs(State(s): State<App>, Query(q): Query<ListQuery>) -> ApiResult<Response> {
    let states: Vec<String> = q
        .state
        .map(|v| {
            v.split(',')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let jobs = s.list_jobs(&states, q.limit.unwrap_or(100))?;
    Ok(Json(
        jobs.into_iter()
            .map(crate::protocol::Job::from)
            .collect::<Vec<_>>(),
    )
    .into_response())
}

async fn measurements(State(s): State<App>) -> Json<Snapshot> {
    Json(s.measurements())
}

async fn pressure(State(s): State<App>) -> Json<PressureStatus> {
    Json(s.pressure())
}

async fn submission(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(crate::protocol::Submission::from(s.submission(&id)?)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    pub(super) fn app() -> Router {
        router(App::new(
            Store::open_in_memory().unwrap(),
            "t".into(),
            "i".into(),
        ))
    }

    pub(super) async fn call(
        app: &Router,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn submit_list_and_get() {
        let app = app();
        let (st, created) = call(&app, "POST", "/v1/jobs", Some(json!({
            "entry": "sh", "source": "API_TOKEN=x make build", "cwd": "/w", "session": "s1", "now": false
        }))).await;
        assert_eq!(st, StatusCode::CREATED);
        let sid = created["submission"].as_str().unwrap().to_string();

        let (st, jobs) = call(&app, "GET", "/v1/jobs?state=queued", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(jobs[0]["command"], "API_TOKEN=[redacted] make build");

        let (st, sub) = call(&app, "GET", &format!("/v1/submissions/{sid}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(sub["job_state"], "queued");
        assert_eq!(sub["session_id"], "s1");
    }

    #[tokio::test]
    async fn rejects_bad_requests() {
        let app = app();
        let (st, _) = call(
            &app,
            "POST",
            "/v1/jobs",
            Some(json!({"entry": "argv", "cwd": "/w"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(
            &app,
            "POST",
            "/v1/jobs",
            Some(json!({"entry": "sh", "source": "make", "cwd": "/w", "urgency": "high"})),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown fields are rejected"
        );
        assert_eq!(
            call(&app, "GET", "/v1/submissions/s_nope", None).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
