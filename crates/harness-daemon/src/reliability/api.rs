//! Owner-only online backup intents and static diagnostics. Remote adapters do
//! not receive these routes or the loopback owner bearer credential.
use super::{jobs, storage};
use crate::{ApiError, ApiResult, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

fn invalid(_: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "备份操作无效、冲突或不可用；请查询原操作状态".into(),
    )
}
pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/reliability", get(status))
        .route("/api/v1/reliability/diagnostics", get(diagnostics))
        .route("/api/v1/reliability/backups", post(backup))
        .route("/api/v1/reliability/backups/{id}", get(receipt))
        .layer(axum::extract::DefaultBodyLimit::max(8192))
}
async fn status(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let jobs = state
        .store
        .lock()
        .unwrap()
        .backup_receipts()
        .map_err(invalid)?;
    Ok(Json(
        json!({"schema_version":storage::SCHEMA_VERSION,"backup_available":state.storage_lease.is_some(),
        "restore_mode":"offline_only","updates":"disabled_no_trusted_release_configuration","backup_jobs":jobs}),
    ))
}
async fn diagnostics(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let bytes = state
        .store
        .lock()
        .unwrap()
        .diagnostic_export()
        .map_err(invalid)?;
    Ok(Json(serde_json::from_slice(&bytes).map_err(invalid)?))
}
async fn receipt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<jobs::BackupReceipt>> {
    state
        .store
        .lock()
        .unwrap()
        .backup_receipt(&id)
        .map_err(invalid)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "备份操作不存在".into()))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupRequest {
    id: String,
    parent: PathBuf,
    name: String,
}
async fn backup(
    State(state): State<Arc<AppState>>,
    Json(request): Json<BackupRequest>,
) -> ApiResult<Json<jobs::BackupReceipt>> {
    let lease = state
        .storage_lease
        .clone()
        .ok_or_else(|| invalid("no lease"))?;
    {
        let mut store = state.store.lock().unwrap();
        if store
            .backup_receipt(&request.id)
            .map_err(invalid)?
            .is_some()
        {
            return store
                .accept_backup(&request.id, &request.parent, &request.name)
                .map(Json)
                .map_err(invalid);
        }
    }
    let permit = state.backup_gate.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "备份工作槽已满；未接受新操作".into(),
        )
    })?;
    let (receipt, work) = {
        let mut store = state.store.lock().unwrap();
        let replayed = store
            .backup_receipt(&request.id)
            .map_err(invalid)?
            .is_some();
        let receipt = store
            .accept_backup(&request.id, &request.parent, &request.name)
            .map_err(invalid)?;
        let work = if replayed {
            None
        } else {
            store.claim_backup(&request.id).map_err(invalid)?
        };
        (receipt, work)
    };
    if let Some(work) = work {
        // The accepted operation owns its worker. Dropping the HTTP request or
        // receiving the same UUID never schedules a second filesystem export.
        tokio::spawn(async move {
            let _permit = permit;
            let id = work.id.clone();
            let token = work.token.clone();
            let deadline = Instant::now() + Duration::from_secs(30);
            let result = tokio::task::spawn_blocking(move || {
                lease.export_backup(&work.parent, &work.name, deadline)
            })
            .await;
            let outcome = match result {
                Ok(Ok(manifest)) => jobs::BackupOutcome::Completed(manifest),
                Ok(Err(_)) => jobs::BackupOutcome::Failed(if Instant::now() >= deadline {
                    jobs::BackupReason::Deadline
                } else {
                    jobs::BackupReason::ExportFailed
                }),
                Err(_) => jobs::BackupOutcome::Unknown,
            };
            if state
                .store
                .lock()
                .unwrap()
                .finish_backup(&id, &token, outcome)
                .is_err()
            {
                tracing::error!("无法确认备份操作的最终记录；未自动重放，请检查原操作与目标目录");
            }
        });
    }
    Ok(Json(receipt))
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
