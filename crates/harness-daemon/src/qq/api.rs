//! Authenticated local-owner endpoints only. HTTP disconnect never replays or
//! aborts a durably claimed OS mutation; GET recovers its original operation.
use super::{
    configuration,
    credentials::{Outcome, WorkKind},
};
use crate::{
    capabilities::vault::{
        CredentialRef, FreshCredential, SecretBytes, VaultCancellation, VaultCertainty,
    },
    ApiError, ApiResult, AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
fn invalid(_: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "QQ 操作未确认，请刷新原记录核对；不会回显输入或底层错误".into(),
    )
}
pub(crate) async fn snapshot(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    state
        .store
        .lock()
        .unwrap()
        .qq_snapshot(super::daemon::fixture_origin().is_some())
        .map(Json)
        .map_err(invalid)
}
pub(crate) async fn command(
    State(state): State<Arc<AppState>>,
    Json(envelope): Json<Value>,
) -> ApiResult<Json<Value>> {
    let settings = state.settings.read().unwrap().clone();
    let receipt = state
        .store
        .lock()
        .unwrap()
        .qq_admin_command(
            &envelope,
            &settings,
            super::daemon::fixture_origin().is_some(),
        )
        .map_err(invalid)?;
    state.wake.notify_waiters();
    Ok(Json(receipt))
}
pub(crate) async fn receipt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Option<Value>>> {
    state
        .store
        .lock()
        .unwrap()
        .qq_admin_receipt(&id)
        .map(Json)
        .map_err(invalid)
}
pub(crate) async fn operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Option<harness_core::CredentialOperationView>>> {
    state
        .store
        .lock()
        .unwrap()
        .qq_credential_operation(&id)
        .map(Json)
        .map_err(invalid)
}
#[derive(Deserialize)]
struct Metadata {
    id: String,
    expected_revision: u64,
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
pub(crate) struct DeleteRequest {
    id: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupRequest {
    id: String,
}
pub(crate) async fn set_credential(
    State(state): State<Arc<AppState>>,
    Path(config_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    if body.len() > 8192 {
        return Err(invalid("bounded"));
    }
    let meta: Metadata = serde_json::from_slice(&body).map_err(invalid)?;
    // A replay does not validate, hash or apply this attempt's new secret.
    if let Some(receipt) = state
        .store
        .lock()
        .unwrap()
        .qq_existing_set(&meta.id, &config_id, meta.expected_revision)
        .map_err(invalid)?
    {
        return Ok(Json(receipt));
    }
    let request: SetRequest = serde_json::from_slice(&body).map_err(invalid)?;
    let secret = SecretBytes::new(request.secret.into_bytes()).map_err(invalid)?;
    if !secret.expose_secret().iter().all(|b| b.is_ascii_graphic()) {
        return Err(invalid("credential format"));
    }
    let config = state
        .store
        .lock()
        .unwrap()
        .qq_config()
        .map_err(invalid)?
        .ok_or_else(|| invalid("config missing"))?;
    if config.id != config_id || config.revision != request.expected_revision {
        return Err(invalid("config revision"));
    }
    let value = std::str::from_utf8(secret.expose_secret()).map_err(invalid)?;
    if config.app_id.contains(value) {
        return Err(invalid("secret in metadata"));
    }
    let binding = super::credential_binding(
        &config_id,
        &configuration::credential_binding_digest(&config.app_id).map_err(invalid)?,
    )
    .map_err(invalid)?;
    let fresh = FreshCredential::new(&binding, secret);
    let receipt = state
        .store
        .lock()
        .unwrap()
        .qq_begin_set(
            &request.id,
            &config_id,
            request.expected_revision,
            fresh.reference(),
        )
        .map_err(invalid)?;
    if receipt["replayed"] != true {
        tokio::spawn(mutate(state, request.id, Some(fresh)));
    }
    Ok(Json(receipt))
}
pub(crate) async fn delete_credential(
    State(state): State<Arc<AppState>>,
    Path(config_id): Path<String>,
    Json(request): Json<DeleteRequest>,
) -> ApiResult<Json<Value>> {
    let receipt = state
        .store
        .lock()
        .unwrap()
        .qq_begin_delete(&request.id, &config_id, request.expected_revision)
        .map_err(invalid)?;
    if receipt["replayed"] != true {
        tokio::spawn(mutate(state, request.id, None));
    }
    Ok(Json(receipt))
}
pub(crate) async fn cleanup(
    State(state): State<Arc<AppState>>,
    Path(target): Path<String>,
    Json(request): Json<CleanupRequest>,
) -> ApiResult<Json<Value>> {
    let receipt = state
        .store
        .lock()
        .unwrap()
        .qq_begin_cleanup(&request.id, &target)
        .map_err(invalid)?;
    if receipt["replayed"] != true {
        tokio::spawn(mutate(state, request.id, None));
    }
    Ok(Json(receipt))
}
async fn mutate(state: Arc<AppState>, id: String, fresh: Option<FreshCredential>) {
    let work = match state.store.lock().unwrap().qq_claim_credential(&id) {
        Ok(Some(work)) => work,
        _ => return,
    };
    let prepared = (|| -> anyhow::Result<_> {
        let binding = super::credential_binding(&work.config_id, &work.binding_digest)?;
        let reference = CredentialRef::parse(&work.reference, &binding)?;
        Ok((binding, reference))
    })();
    let (binding, reference) = match prepared {
        Ok(value) => value,
        Err(_) => {
            let _ = state
                .store
                .lock()
                .unwrap()
                .qq_finish_credential(&id, Outcome::NotApplied);
            return;
        }
    };
    let cancel = VaultCancellation::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    let result = match (work.kind, fresh) {
        (WorkKind::Set, Some(fresh)) if fresh.reference() == &reference => {
            state.vault.set(fresh, &binding, deadline, &cancel).await
        }
        (WorkKind::Delete, None) => {
            state
                .vault
                .delete(&reference, &binding, deadline, &cancel)
                .await
        }
        _ => {
            let _ = state
                .store
                .lock()
                .unwrap()
                .qq_finish_credential(&id, Outcome::NotApplied);
            return;
        }
    };
    let outcome = match result {
        Ok(()) => Outcome::Succeeded,
        Err(error) if error.certainty == VaultCertainty::Unknown => Outcome::Unknown,
        Err(_) => Outcome::NotApplied,
    };
    let _ = state
        .store
        .lock()
        .unwrap()
        .qq_finish_credential(&id, outcome);
    state.wake.notify_waiters();
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
