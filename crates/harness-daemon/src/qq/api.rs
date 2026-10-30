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
    reliability::{
        lifecycle::{WorkerExit, WorkerKind},
        lifecycle_runtime::WorkAdmission,
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
    state.store_only(|store| {
        store
            .qq_snapshot(super::daemon::fixture_origin().is_some())
            .map(Json)
            .map_err(invalid)
    })
}
pub(crate) async fn command(
    State(state): State<Arc<AppState>>,
    Json(envelope): Json<Value>,
) -> ApiResult<Json<Value>> {
    let settings = state.settings.read().unwrap().clone();
    let (receipt, admission) = begin(&state, |store| {
        store.qq_admin_command(
            &envelope,
            &settings,
            super::daemon::fixture_origin().is_some(),
        )
    })?;
    admission.settle();
    state.wake.notify_waiters();
    Ok(Json(receipt))
}
pub(crate) async fn receipt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Option<Value>>> {
    state.store_only(|store| store.qq_admin_receipt(&id).map(Json).map_err(invalid))
}
pub(crate) async fn operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Option<harness_core::CredentialOperationView>>> {
    state.store_only(|store| {
        store
            .qq_credential_operation(&id)
            .map(Json)
            .map_err(invalid)
    })
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
    if let Some(receipt) = state.store_only(|store| {
        store
            .qq_existing_set(&meta.id, &config_id, meta.expected_revision)
            .map_err(invalid)
    })? {
        return Ok(Json(receipt));
    }
    let request: SetRequest = serde_json::from_slice(&body).map_err(invalid)?;
    let secret = SecretBytes::new(request.secret.into_bytes()).map_err(invalid)?;
    if !secret.expose_secret().iter().all(|b| b.is_ascii_graphic()) {
        return Err(invalid("credential format"));
    }
    let config = state
        .store_only(|store| store.qq_config().map_err(invalid))?
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
    let (receipt, admission) = begin(&state, |store| {
        store.qq_begin_set(
            &request.id,
            &config_id,
            request.expected_revision,
            fresh.reference(),
        )
    })?;
    launch(state, request.id, Some(fresh), &receipt, admission)?;
    Ok(Json(receipt))
}
pub(crate) async fn delete_credential(
    State(state): State<Arc<AppState>>,
    Path(config_id): Path<String>,
    Json(request): Json<DeleteRequest>,
) -> ApiResult<Json<Value>> {
    let (receipt, admission) = begin(&state, |store| {
        store.qq_begin_delete(&request.id, &config_id, request.expected_revision)
    })?;
    launch(state, request.id, None, &receipt, admission)?;
    Ok(Json(receipt))
}
pub(crate) async fn cleanup(
    State(state): State<Arc<AppState>>,
    Path(target): Path<String>,
    Json(request): Json<CleanupRequest>,
) -> ApiResult<Json<Value>> {
    let (receipt, admission) = begin(&state, |store| store.qq_begin_cleanup(&request.id, &target))?;
    launch(state, request.id, None, &receipt, admission)?;
    Ok(Json(receipt))
}
// Admission is reserved before intent; its gate and commit share the same Store
// mutex. Synchronous rollback is settled, not an invented physical Unknown.
fn begin(
    state: &AppState,
    operation: impl FnOnce(&mut crate::store::Store) -> anyhow::Result<Value>,
) -> ApiResult<(Value, WorkAdmission)> {
    let mut admission = state.lifecycle.admit().map_err(unavailable)?;
    let mut store = state.store.lock().unwrap();
    admission.begin_intent(&store).map_err(unavailable)?;
    match operation(&mut store) {
        Ok(value) => Ok((value, admission)),
        Err(error) => {
            // Preserve an armed uncertain SQL acknowledgement. Only known
            // validation/rollback may release the admission as settled.
            if !super::runtime::storage_uncertain(&error) {
                admission.settle();
            }
            Err(invalid(error))
        }
    }
}
fn unavailable(_: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "后台正在停止或在途工作已满，未接收新操作".into(),
    )
}
fn launch(
    state: Arc<AppState>,
    id: String,
    fresh: Option<FreshCredential>,
    receipt: &Value,
    admission: WorkAdmission,
) -> ApiResult<()> {
    if receipt["replayed"] == true {
        admission.settle();
        return Ok(());
    }
    // No await between durable intent and this ownership transfer. Observer
    // disconnect only discards its receipt; the registered worker remains.
    admission
        .spawn(WorkerKind::Credential, async move {
            let exit = mutate(state, id, fresh).await;
            (exit, ())
        })
        .map_err(unavailable)?;
    Ok(())
}
async fn mutate(state: Arc<AppState>, id: String, fresh: Option<FreshCredential>) -> WorkerExit {
    let work = match state.store.lock().unwrap().qq_claim_credential(&id) {
        Ok(Some(work)) => work,
        Ok(None) => return WorkerExit::Settled,
        Err(_) => return WorkerExit::Unknown,
    };
    let prepared = (|| -> anyhow::Result<_> {
        let binding = super::credential_binding(&work.config_id, &work.binding_digest)?;
        let reference = CredentialRef::parse(&work.reference, &binding)?;
        Ok((binding, reference))
    })();
    let (binding, reference) = match prepared {
        Ok(value) => value,
        Err(_) => {
            return finish_credential(&state, &id, Outcome::NotApplied);
        }
    };
    let cancel = VaultCancellation::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    let signal = state.lifecycle.signal();
    let apply = async {
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
            _ => return Outcome::NotApplied,
        };
        match result {
            Ok(()) => Outcome::Succeeded,
            Err(error) if error.certainty == VaultCertainty::Unknown => Outcome::Unknown,
            Err(_) => Outcome::NotApplied,
        }
    };
    tokio::pin!(apply);
    let outcome = if signal.is_cancelled() {
        Outcome::NotApplied
    } else {
        tokio::select! {biased; _=signal.cancelled()=>{cancel.cancel();apply.await}, result=&mut apply=>result}
    };
    let exit = finish_credential(&state, &id, outcome);
    state.wake.notify_waiters();
    exit
}
fn finish_credential(state: &AppState, id: &str, outcome: Outcome) -> WorkerExit {
    if state
        .store
        .lock()
        .unwrap()
        .qq_finish_credential(id, outcome)
        .is_ok()
    {
        // Historical Unknown is a durable external-outcome fact. The shared
        // Vault custodian, not this receipt, proves actual OS thread shutdown.
        WorkerExit::Settled
    } else {
        WorkerExit::Unknown
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
