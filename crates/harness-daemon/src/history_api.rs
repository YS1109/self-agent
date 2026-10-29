//! Owner-only bounded reads. Historical pages never confer command authority.
use crate::{store::history::HistoryError, AppState};
use axum::{
    extract::{rejection::QueryRejection, OriginalUri, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/tasks/{id}", get(task))
        .route("/api/v1/history/{kind}", get(page))
        .route("/api/v1/history/sessions/{id}/messages", get(messages))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    session_id: Option<String>,
    cursor: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageQuery {
    cursor: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotQuery {
    window: Option<u8>,
}
enum Read {
    Snapshot,
    LegacySnapshot,
    Task(String),
    Page(String, PageQuery),
    Messages(String, MessageQuery),
}
fn failure(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"code":code,"error":message}))).into_response()
}
fn invalid() -> Response {
    failure(
        StatusCode::BAD_REQUEST,
        "invalid_history_query",
        "历史查询参数无效",
    )
}
fn safe_uri(uri: &axum::http::Uri, no_query: bool) -> bool {
    // All supported identifiers/cursors are ASCII and need no percent encoding.
    // Refuse alternate path/query spellings before the decoded extractor is used.
    let raw = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("");
    raw.len() <= 4096 && !raw.contains('%') && (!no_query || uri.query().is_none())
}
async fn read(state: Arc<AppState>, operation: Read) -> Response {
    // HTTP cancellation must not accumulate unbounded queued blocking jobs.
    // The owned slot remains in the closure until its actual SQLite read ends.
    let Ok(slot) = state.history_gate.clone().try_acquire_owned() else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "history_busy",
            "历史读取繁忙，请稍后只读重试",
        );
    };
    let result = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let store = state
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store unavailable"))?;
        match operation {
            Read::Snapshot => store.windowed_snapshot(&state.history_epoch),
            Read::LegacySnapshot => store.legacy_bounded_snapshot(),
            Read::Task(id) => store.bounded_task_detail(&id),
            Read::Page(kind, query) => store.history_page(
                &state.history_epoch,
                &kind,
                query.session_id.as_deref(),
                query.cursor.as_deref(),
            ),
            Read::Messages(id, query) => {
                store.history_messages(&state.history_epoch, &id, query.cursor.as_deref())
            }
        }
    })
    .await;
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => {
            if let Some(error) = error.downcast_ref::<HistoryError>() {
                let status = match error {
                    HistoryError::Reset => StatusCode::CONFLICT,
                    HistoryError::InvalidQuery => StatusCode::BAD_REQUEST,
                    HistoryError::NotFound => StatusCode::NOT_FOUND,
                    HistoryError::RecordTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                    HistoryError::Capacity => StatusCode::SERVICE_UNAVAILABLE,
                    HistoryError::InvalidRecord => StatusCode::INTERNAL_SERVER_ERROR,
                };
                return failure(status, error.code(), &error.to_string());
            }
            failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_unavailable",
                "历史读取未确认；未返回底层错误或私有记录",
            )
        }
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "history_unavailable",
            "历史读取未确认；请稍后只读查询",
        ),
    }
}
async fn snapshot(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    query: Result<Query<SnapshotQuery>, QueryRejection>,
) -> Response {
    if !safe_uri(&uri, false) {
        return invalid();
    }
    let Ok(Query(query)) = query else {
        return invalid();
    };
    match query.window {
        Some(1) => read(state, Read::Snapshot).await,
        None => read(state, Read::LegacySnapshot).await,
        _ => invalid(),
    }
}
async fn task(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> Response {
    if !safe_uri(&uri, true) || !crate::store::history::safe_history_id(&id) {
        return invalid();
    }
    read(state, Read::Task(id)).await
}
async fn page(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    Path(kind): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    if !safe_uri(&uri, false) {
        return invalid();
    }
    let Ok(Query(query)) = query else {
        return invalid();
    };
    read(state, Read::Page(kind, query)).await
}
async fn messages(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    query: Result<Query<MessageQuery>, QueryRejection>,
) -> Response {
    if !safe_uri(&uri, false) {
        return invalid();
    }
    let Ok(Query(query)) = query else {
        return invalid();
    };
    read(state, Read::Messages(id, query)).await
}

#[cfg(test)]
#[path = "history_api_tests.rs"]
mod tests;
