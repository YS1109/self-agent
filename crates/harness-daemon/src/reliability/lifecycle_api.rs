//! Local-owner shutdown control, mounted INSIDE bearer/protocol middleware.
//!
//! These observers are deliberately outside the execution worker set: a
//! shutdown response must not await a registry that includes itself. Only the
//! trusted backend starts retained cleanup. Disconnecting an observer cannot
//! cancel that cleanup. No caller can supply a backend, runtime or stop proof.
use super::lifecycle_runtime::{LifecycleRuntime, ShutdownBackend};
use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        DefaultBodyLimit, OriginalUri, Query, State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

struct Control<B> {
    runtime: Arc<LifecycleRuntime>,
    backend: Arc<B>,
}

/// The caller supplies both the same process-wide registry and its trusted
/// authoritative Store adapter. Mounting this router is not authentication.
pub(crate) fn routes<S, B>(runtime: Arc<LifecycleRuntime>, backend: Arc<B>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    B: ShutdownBackend,
{
    Router::new()
        .route("/api/v1/lifecycle", get(status::<B>))
        .route("/api/v1/lifecycle/shutdown", post(shutdown::<B>))
        .layer(DefaultBodyLimit::max(128))
        .with_state(Arc::new(Control { runtime, backend }))
}

async fn status<B: ShutdownBackend>(State(control): State<Arc<Control<B>>>) -> Response {
    no_store(Json(control.runtime.snapshot()).into_response())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitQuery {
    wait_ms: Option<u32>,
}

async fn shutdown<B: ShutdownBackend>(
    State(control): State<Arc<Control<B>>>,
    OriginalUri(uri): OriginalUri,
    query: Result<Query<WaitQuery>, QueryRejection>,
    request: Result<Json<Value>, JsonRejection>,
) -> Response {
    let wait_ms = match query {
        Ok(Query(query))
            if uri.query().is_none_or(|value| value.len() <= 64)
                && query.wait_ms.unwrap_or(0) <= 30_000 =>
        {
            query.wait_ms.unwrap_or(0)
        }
        _ => {
            return no_store(
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"关闭观察参数无效；未改变后台状态。"})),
                )
                    .into_response(),
            )
        }
    };
    let expected = match request {
        Ok(Json(Value::Object(fields))) if fields.len() == 1 => fields
            .get("expected_instance")
            .and_then(Value::as_str)
            .filter(|value| uuid::Uuid::parse_str(value).is_ok())
            .map(str::to_owned),
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return no_store(
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({"error":"关闭请求超过限制；未改变后台状态。"})),
                )
                    .into_response(),
            );
        }
        _ => None,
    };
    let Some(expected) = expected else {
        return no_store(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"关闭请求必须包含目标后台实例标识；未改变后台状态。"})),
            )
                .into_response(),
        );
    };
    if expected != control.runtime.instance_id() {
        // Proxies may rediscover a new daemon between GET and POST. Never let
        // an observation of the old process authorize stopping the new one.
        return no_store(
            (
                StatusCode::CONFLICT,
                Json(json!({"error":"后台实例已变化；未关闭当前实例，旧实例清理结果仍未确认。"})),
            )
                .into_response(),
        );
    }
    match control.runtime.request_shutdown(control.backend.clone()) {
        Ok(snapshot) => {
            if wait_ms == 0 {
                no_store((StatusCode::ACCEPTED, Json(snapshot)).into_response())
            } else {
                let observed = control
                    .runtime
                    .observe(Instant::now() + Duration::from_millis(wait_ms.into()))
                    .await;
                let status = if observed.snapshot.outcome
                    == Some(super::lifecycle_runtime::ShutdownOutcome::Stopped)
                {
                    StatusCode::OK
                } else {
                    StatusCode::ACCEPTED
                };
                no_store((status, Json(observed.snapshot)).into_response())
            }
        }
        Err(_) => no_store(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"关闭控制暂不可用；未确认后台停止，请查询原状态。"})),
            )
                .into_response(),
        ),
    }
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

#[cfg(test)]
#[path = "lifecycle_api_tests.rs"]
mod tests;
