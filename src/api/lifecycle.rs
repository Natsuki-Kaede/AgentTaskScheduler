//! HTTP lifecycle adapters. Application services own state and waiting rules.
use super::ApiResult;
use crate::app::App;
use crate::protocol::*;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

pub fn routes() -> Router<App> {
    Router::new()
        .route("/v1/jobs/{id}", get(job))
        .route("/v1/jobs/{id}/executor", post(executor))
        .route("/v1/jobs/{id}/lease", post(lease))
        .route("/v1/jobs/{id}/started", post(started))
        .route("/v1/jobs/{id}/main-result", post(main_result))
        .route("/v1/jobs/{id}/finished", post(finished))
        .route("/v1/jobs/{id}/control", get(control))
        .route("/v1/submissions/{id}/wait", post(wait))
        .route("/v1/submissions/{id}/cancel", post(cancel))
        .route("/v1/submissions/{id}/quiet", post(quiet))
        .route("/v1/submissions/{id}/detach", post(detach))
        .route("/v1/sessions/{id}/running", get(running))
        .route("/v1/sessions/{id}/ended", post(ended))
        .route("/v1/jobs/{id}/why", get(why))
        .route("/v1/jobs/{id}/unblock", post(unblock))
}

fn ok() -> ApiResult<Response> {
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn job(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(Job::from(s.job(&id)?)).into_response())
}
async fn executor(
    State(s): State<App>,
    Path(id): Path<String>,
    Json(b): Json<ExecutorRequest>,
) -> ApiResult<Response> {
    s.register_executor(&id, b.pid, &b.start)?;
    ok()
}
async fn lease(
    State(s): State<App>,
    Path(id): Path<String>,
    Json(b): Json<LeaseRequest>,
) -> ApiResult<Response> {
    let terms = s.lease(&id, b.pid).await?;
    Ok(Json(LeaseResponse {
        granted: terms.is_some(),
        terms,
    })
    .into_response())
}
async fn started(
    State(s): State<App>,
    Path(id): Path<String>,
    Json(b): Json<StartedRequest>,
) -> ApiResult<Response> {
    s.started(&id, &b.log_path, b.pgid)?;
    ok()
}
async fn main_result(
    State(s): State<App>,
    Path(id): Path<String>,
    Json(b): Json<MainResultRequest>,
) -> ApiResult<Response> {
    s.main_result(&id, b.exit_code)?;
    ok()
}
async fn finished(
    State(s): State<App>,
    Path(id): Path<String>,
    Json(b): Json<FinishedRequest>,
) -> ApiResult<Response> {
    let state = b.state.parse().map_err(|_| {
        crate::domain::error::InvalidInput(format!("{} is not a terminal state", b.state))
    })?;
    s.finished(&id, state, b.exit_code, b.max_rss)?;
    ok()
}
async fn control(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(ControlResponse {
        cancel: s.control(&id)?,
    })
    .into_response())
}
async fn wait(
    State(s): State<App>,
    Path(id): Path<String>,
    Json(b): Json<WaitRequest>,
) -> ApiResult<Response> {
    let (reached, submission) = s.wait(&id, b.wait_seconds, b.until.parse()?).await?;
    Ok(Json(WaitResponse {
        reached,
        submission: submission.into(),
    })
    .into_response())
}
async fn cancel(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    s.cancel(&id)?;
    ok()
}
async fn detach(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    s.detach(&id)?;
    ok()
}
async fn quiet(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    s.quiet(&id)?;
    ok()
}
async fn unblock(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    s.unblock(&id)?;
    ok()
}
async fn why(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    let (state, verdict) = s.why(&id)?;
    Ok(Json(WhyResponse {
        job: id,
        state,
        verdict,
    })
    .into_response())
}
async fn running(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(
        s.running_reminders(&id)?
            .into_iter()
            .map(Reminder::from)
            .collect::<Vec<_>>(),
    )
    .into_response())
}
async fn ended(State(s): State<App>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(
        s.take_ended_reminders(&id)?
            .into_iter()
            .map(Reminder::from)
            .collect::<Vec<_>>(),
    )
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{app, call};
    use axum::http::StatusCode;
    use serde_json::json;

    async fn submit(app: &axum::Router) -> (String, String) {
        let (st, created) = call(
            app,
            "POST",
            "/v1/jobs",
            Some(json!({"entry": "argv", "argv": ["make"], "cwd": "/w"})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        (
            created["submission"].as_str().unwrap().into(),
            created["job"].as_str().unwrap().into(),
        )
    }

    #[tokio::test]
    async fn invalid_wait_conditions_and_terminal_states_are_bad_requests() {
        let app = app();
        let (sub, job) = submit(&app).await;
        for (path, body) in [
            (
                format!("/v1/submissions/{sub}/wait"),
                json!({"until": "other", "wait_seconds": 0}),
            ),
            (
                format!("/v1/submissions/{sub}/wait"),
                json!({"until": "main", "wait_seconds": -1}),
            ),
            (
                format!("/v1/jobs/{job}/finished"),
                json!({"state": "unknown"}),
            ),
        ] {
            assert_eq!(
                call(&app, "POST", &path, Some(body)).await.0,
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[tokio::test]
    async fn job_runs_to_terminal() {
        let state = crate::app::App::new(
            crate::store::Store::open_in_memory().unwrap(),
            "t".into(),
            "i".into(),
        );
        let app = crate::api::router(state.clone());
        let (sub, job) = submit(&app).await;
        let post = |path: String, body| {
            let app = app.clone();
            async move { call(&app, "POST", &path, Some(body)).await.0 }
        };
        let wait = |until: &'static str| {
            let (app, sub) = (app.clone(), sub.clone());
            async move {
                call(
                    &app,
                    "POST",
                    &format!("/v1/submissions/{sub}/wait"),
                    Some(json!({"wait_seconds": 0, "until": until})),
                )
                .await
                .1
            }
        };

        // The CLI and the executor both register the same process; another
        // process is refused.
        for _ in 0..2 {
            let executor = json!({"pid": 1, "start": "t"});
            assert_eq!(
                post(format!("/v1/jobs/{job}/executor"), executor).await,
                StatusCode::NO_CONTENT
            );
        }
        let other = json!({"pid": 2, "start": "t"});
        assert_eq!(
            post(format!("/v1/jobs/{job}/executor"), other).await,
            StatusCode::CONFLICT
        );
        // The lease comes from the scheduler; until it decides, the answer
        // is "not yet".
        let lease_path = format!("/v1/jobs/{job}/lease");
        let lease = |pid: i64| call(&app, "POST", &lease_path, Some(json!({"pid": pid})));
        assert_eq!(lease(1).await.1["granted"], false);
        state.test_admit(&job, 1 << 30, 1);
        // Asking again after a lost answer gets the same lease; only the
        // executor gets it.
        for _ in 0..2 {
            assert_eq!(lease(1).await.1["granted"], true);
        }
        assert_eq!(
            post(format!("/v1/jobs/{job}/lease"), json!({"pid": 2})).await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            post(format!("/v1/jobs/{job}/started"), json!({"log_path": "/l"})).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(wait("main").await["reached"], false);

        post(
            format!("/v1/jobs/{job}/main-result"),
            json!({"exit_code": 2}),
        )
        .await;
        let w = wait("main").await;
        assert_eq!(w["reached"], true);
        assert_eq!(w["submission"]["main_exit_code"], 2);
        assert_eq!(w["submission"]["job_state"], "running");
        assert_eq!(wait("terminal").await["reached"], false);

        assert_eq!(
            post(
                format!("/v1/jobs/{job}/finished"),
                json!({"state": "running"})
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        post(
            format!("/v1/jobs/{job}/finished"),
            json!({"state": "completed", "exit_code": 2}),
        )
        .await;
        // A repeated report is accepted; a different end is refused.
        let again = json!({"state": "completed", "exit_code": 2});
        assert_eq!(
            post(format!("/v1/jobs/{job}/finished"), again).await,
            StatusCode::NO_CONTENT
        );
        let other = json!({"state": "canceled"});
        assert_eq!(
            post(format!("/v1/jobs/{job}/finished"), other).await,
            StatusCode::CONFLICT
        );
        let w = wait("terminal").await;
        assert_eq!(w["reached"], true);
        assert_eq!(w["submission"]["exit_code"], 2);
    }

    #[tokio::test]
    async fn canceling_a_queued_job_ends_it_and_refuses_the_lease() {
        let app = app();
        let (sub, job) = submit(&app).await;
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/submissions/{sub}/cancel"),
            Some(json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, row) = call(&app, "GET", &format!("/v1/jobs/{job}"), None).await;
        assert_eq!(row["state"], "canceled");
        let (_, control) = call(&app, "GET", &format!("/v1/jobs/{job}/control"), None).await;
        assert_eq!(control["cancel"], true);
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/jobs/{job}/lease"),
            Some(json!({"pid": 1})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
    }
}
