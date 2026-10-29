//! Owner-only MCP credential enrollment. An operation ID fixes metadata, never
//! secret bytes. Claimed mutations survive caller disconnect; recovery is GET.
use super::{
    runtime,
    vault::{FreshCredential, SecretBytes, VaultCancellation, VaultCertainty},
};
use crate::{
    reliability::lifecycle::{WorkerExit, WorkerKind},
    store::capability_credentials::{self, CredentialOutcome, CredentialWorkKind},
    ApiError, ApiResult, AppState,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/capabilities/mcp/{server}/credentials/{slot}",
            post(set_credential),
        )
        .route(
            "/api/v1/capabilities/mcp/{server}/credentials/{slot}/delete",
            post(delete_credential),
        )
        .route(
            "/api/v1/capability-credential-operations/{operation}",
            get(operation),
        )
        .route(
            "/api/v1/capability-credential-operations/{operation}/cleanup",
            post(cleanup),
        )
        .layer(DefaultBodyLimit::max(8 * 1024))
}
fn invalid(_: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "MCP 凭据操作无效，请刷新配置并核对槽名；未回显输入内容".into(),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    id: String,
    expected_revision: u64,
    secret: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRequest {
    id: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupRequest {
    id: String,
}

async fn operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<harness_core::McpCredentialOperationView>> {
    if !uuid::Uuid::parse_str(&id).is_ok_and(|v| v.hyphenated().to_string() == id) {
        return Err(invalid("id"));
    }
    state
        .store
        .lock()
        .unwrap()
        .mcp_credential_operation(&id)
        .map(Json)
        .map_err(|_| {
            ApiError(
                StatusCode::NOT_FOUND,
                "未找到此 MCP 凭据操作；没有重新发送".into(),
            )
        })
}
async fn set_credential(
    State(state): State<Arc<AppState>>,
    Path((server, slot)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    // Never return a serde error, as its diagnostic could contain a credential.
    let request: SetRequest = serde_json::from_slice(&body).map_err(invalid)?;
    let secret = SecretBytes::new(request.secret.into_bytes()).map_err(invalid)?;
    if let Some(receipt) = state
        .store
        .lock()
        .unwrap()
        .existing_mcp_credential_set(&request.id, &server, &slot, request.expected_revision)
        .map_err(invalid)?
    {
        return Ok(Json(receipt));
    }
    let value = std::str::from_utf8(secret.expose_secret()).map_err(invalid)?;
    if value.contains('\0') {
        return Err(invalid("secret"));
    }
    let (record, metadata) = {
        let store = state.store.lock().unwrap();
        (
            store.mcp_credential_record(&server).map_err(invalid)?,
            store
                .metadata_for_mcp_credential_check(&server)
                .map_err(invalid)?,
        )
    };
    if !record.declared_slots.contains(&slot) || runtime::has_secret(&metadata, &[value.to_owned()])
    {
        return Err(invalid("metadata"));
    }
    let binding = capability_credentials::mcp_binding(&server, &slot, &record.config_digest)
        .map_err(invalid)?;
    let fresh = FreshCredential::new(&binding, secret);
    let (admission, receipt) = state.admit_store(|store| {
        store
            .begin_mcp_credential_set(
                &request.id,
                &server,
                &slot,
                request.expected_revision,
                fresh.reference(),
            )
            .map_err(invalid)
    })?;
    if receipt["replayed"] != true {
        admission
            .spawn(WorkerKind::Credential, async move {
                (mutate(state, request.id, Some(fresh)).await, ())
            })
            .map_err(crate::lifecycle_error)?;
    } else {
        admission.settle();
    }
    Ok(Json(receipt))
}
async fn delete_credential(
    State(state): State<Arc<AppState>>,
    Path((server, slot)): Path<(String, String)>,
    Json(request): Json<DeleteRequest>,
) -> ApiResult<Json<Value>> {
    let (admission, receipt) = state.admit_store(|store| {
        store
            .begin_mcp_credential_delete(&request.id, &server, &slot, request.expected_revision)
            .map_err(invalid)
    })?;
    if receipt["replayed"] != true {
        admission
            .spawn(WorkerKind::Credential, async move {
                (mutate(state, request.id, None).await, ())
            })
            .map_err(crate::lifecycle_error)?;
    } else {
        admission.settle();
    }
    Ok(Json(receipt))
}
async fn cleanup(
    State(state): State<Arc<AppState>>,
    Path(target): Path<String>,
    Json(request): Json<CleanupRequest>,
) -> ApiResult<Json<Value>> {
    let (admission, receipt) = state.admit_store(|store| {
        store
            .begin_mcp_credential_cleanup(&request.id, &target)
            .map_err(invalid)
    })?;
    if receipt["replayed"] != true {
        admission
            .spawn(WorkerKind::Credential, async move {
                (mutate(state, request.id, None).await, ())
            })
            .map_err(crate::lifecycle_error)?;
    } else {
        admission.settle();
    }
    Ok(Json(receipt))
}
async fn mutate(state: Arc<AppState>, id: String, fresh: Option<FreshCredential>) -> WorkerExit {
    let work = {
        let mut store = state.store.lock().unwrap();
        let stopping = state.lifecycle.check_locked(&store).is_err();
        let work = match store.claim_mcp_credential_operation(&id) {
            Ok(Some(work)) => work,
            Ok(None) => return WorkerExit::Settled,
            Err(_) => return WorkerExit::Unknown,
        };
        if stopping {
            // This is settlement of the original accepted intent, not a new
            // external admission: no vault request was attempted by this worker.
            return settled(
                store.finish_mcp_credential_operation(&id, CredentialOutcome::Rejected),
            );
        }
        work
    };
    if work.operation_id != id
        || uuid::Uuid::parse_str(&work.server_id).is_err()
        || capability_credentials::validate_slot(&work.slot).is_err()
    {
        return settled(
            state
                .store
                .lock()
                .unwrap()
                .finish_mcp_credential_operation(&id, CredentialOutcome::Rejected),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let cancel = VaultCancellation::default();
    let result = match (work.kind, fresh) {
        (CredentialWorkKind::Set, Some(fresh))
            if work.operation_id == id && fresh.reference() == &work.reference =>
        {
            state
                .vault
                .set(fresh, &work.binding, deadline, &cancel)
                .await
        }
        (CredentialWorkKind::Delete, None) if work.operation_id == id => {
            state
                .vault
                .delete(&work.reference, &work.binding, deadline, &cancel)
                .await
        }
        _ => {
            return settled(
                state
                    .store
                    .lock()
                    .unwrap()
                    .finish_mcp_credential_operation(&id, CredentialOutcome::Rejected),
            );
        }
    };
    let outcome = match result {
        Ok(()) => CredentialOutcome::Succeeded,
        Err(e) if e.certainty == VaultCertainty::Unknown => CredentialOutcome::Unknown,
        Err(_) => CredentialOutcome::Rejected,
    };
    // Vault certainty is an audited result, not physical-thread cleanup. The
    // separately tracked vault shutdown custodian waits for its real OS worker.
    settled(
        state
            .store
            .lock()
            .unwrap()
            .finish_mcp_credential_operation(&id, outcome),
    )
}

fn settled<T>(result: anyhow::Result<T>) -> WorkerExit {
    if result.is_ok() {
        WorkerExit::Settled
    } else {
        WorkerExit::Unknown
    }
}

#[cfg(test)]
#[path = "credential_api/tests.rs"]
mod tests;
