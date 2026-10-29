//! Owner-only named service metadata and one-shot vault mutations. No secret is
//! logged, serialized into a receipt or passed to SQLite. A disconnected HTTP
//! client cannot abort a claimed mutation; it recovers the outcome via GET.
use super::{
    providers,
    vault::{FreshCredential, SecretBytes, VaultCancellation, VaultCertainty},
};
use crate::{
    reliability::lifecycle::{WorkerExit, WorkerKind},
    store::providers::{CredentialOutcome, CredentialWorkKind},
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

fn provider_error(error: providers::ProviderError) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        format!("{}（{}）", error.message(), error.code()),
    )
}

fn invalid(_: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "模型服务或凭据操作无效，请刷新配置后重试；未回显输入内容".into(),
    )
}

pub(crate) async fn registry(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<harness_core::NamedProviders>> {
    state
        .store
        .lock()
        .unwrap()
        .named_providers()
        .map(Json)
        .map_err(invalid)
}

pub(crate) async fn operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<harness_core::CredentialOperationView>> {
    state
        .store
        .lock()
        .unwrap()
        .credential_operation(&id)
        .map(Json)
        .map_err(invalid)
}

pub(crate) async fn command(
    State(state): State<Arc<AppState>>,
    Json(envelope): Json<Value>,
) -> ApiResult<Json<Value>> {
    command_with_resolver(state, envelope, |settings| {
        crate::configuration::resolve(&settings).unwrap_or(settings)
    })
    .await
}

async fn command_with_resolver(
    state: Arc<AppState>,
    envelope: Value,
    resolve: impl FnOnce(crate::Settings) -> crate::Settings + Send + 'static,
) -> ApiResult<Json<Value>> {
    state
        .ensure_cleanup_owner()
        .map_err(crate::lifecycle_error)?;
    let admission = state.lifecycle.admit().map_err(crate::lifecycle_error)?;
    let observer = admission
        .spawn(WorkerKind::BlockingCleanup, async move {
            let result = tokio::task::spawn_blocking(move || -> ApiResult<Value> {
                let settings = state.settings.read().unwrap().clone();
                // Local Codex configuration is real filesystem work, not harmless
                // metadata prevalidation. This owner remains registered through it.
                if state.lifecycle.signal().is_cancelled() {
                    return Err(crate::lifecycle_error(
                        crate::reliability::lifecycle::AdmissionError::Draining,
                    ));
                }
                let settings = resolve(settings);
                let secrets = crate::continuation::known_secrets(&settings);
                let mut store = state.store.lock().unwrap();
                state
                    .lifecycle
                    .check_locked(&store)
                    .map_err(crate::lifecycle_error)?;
                store.provider_command(&envelope, &secrets).map_err(invalid)
            })
            .await;
            match result {
                Ok(result) => (WorkerExit::Settled, result),
                Err(_) => (WorkerExit::Unknown, Err(invalid("worker"))),
            }
        })
        .map_err(crate::lifecycle_error)?;
    observer.await.map_err(|_| invalid("worker"))?.map(Json)
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
    Path(provider_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    // A generic error avoids a serde diagnostic retaining credential content.
    let request: SetRequest = serde_json::from_slice(&body).map_err(invalid)?;
    let secret = SecretBytes::new(request.secret.into_bytes()).map_err(|error| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("{}（{}）", error.reason.message(), error.reason.code()),
        )
    })?;
    let existing = state
        .store
        .lock()
        .unwrap()
        .existing_provider_credential_set(&request.id, &provider_id, request.expected_revision)
        .map_err(invalid)?;
    if let Some(receipt) = existing {
        return Ok(Json(receipt));
    }
    providers::validate_api_key(&secret).map_err(provider_error)?;
    let record = state
        .store
        .lock()
        .unwrap()
        .provider_record(&provider_id)
        .map_err(invalid)?;
    // Do not enroll a value which already appears in persisted public metadata.
    let key = std::str::from_utf8(secret.expose_secret())
        .map_err(invalid)?
        .to_owned();
    let checked = providers::validate_definition(record.definition.definition().clone(), &[key])
        .map_err(invalid)?;
    let binding = providers::credential_binding(&provider_id, &checked).map_err(invalid)?;
    let fresh = FreshCredential::new(&binding, secret);
    let (admission, receipt) = state.admit_store(|store| {
        store
            .begin_provider_credential_set(
                &request.id,
                &provider_id,
                request.expected_revision,
                fresh.reference(),
            )
            .map_err(invalid)
    })?;
    if receipt["replayed"] != true {
        let state = state.clone();
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

pub(crate) async fn delete_credential(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    Json(request): Json<DeleteRequest>,
) -> ApiResult<Json<Value>> {
    let (admission, receipt) = state.admit_store(|store| {
        store
            .begin_provider_credential_delete(&request.id, &provider_id, request.expected_revision)
            .map_err(invalid)
    })?;
    if receipt["replayed"] != true {
        let state = state.clone();
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

pub(crate) async fn cleanup(
    State(state): State<Arc<AppState>>,
    Path(target): Path<String>,
    Json(request): Json<CleanupRequest>,
) -> ApiResult<Json<Value>> {
    let (admission, receipt) = state.admit_store(|store| {
        store
            .begin_credential_cleanup(&request.id, &target)
            .map_err(invalid)
    })?;
    if receipt["replayed"] != true {
        let state = state.clone();
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
        let work = match store.claim_credential_operation(&id) {
            Ok(Some(work)) => work,
            Ok(None) => return WorkerExit::Settled,
            Err(_) => return WorkerExit::Unknown,
        };
        if stopping {
            // This is settlement of the original accepted intent, not a new
            // external admission: no vault request was attempted by this worker.
            return settled(store.finish_credential_operation(&id, CredentialOutcome::Rejected));
        }
        work
    };
    if work.operation_id != id || uuid::Uuid::parse_str(&work.owner_id).is_err() {
        return settled(
            state
                .store
                .lock()
                .unwrap()
                .finish_credential_operation(&id, CredentialOutcome::Rejected),
        );
    }
    let cancel = VaultCancellation::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    let result = match (work.kind, fresh) {
        (CredentialWorkKind::Set, Some(fresh)) if fresh.reference() == &work.reference => {
            state
                .vault
                .set(fresh, &work.binding, deadline, &cancel)
                .await
        }
        (CredentialWorkKind::Delete, None) => {
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
                    .finish_credential_operation(&id, CredentialOutcome::Rejected),
            );
        }
    };
    let outcome = match result {
        Ok(()) => CredentialOutcome::Succeeded,
        Err(error) if error.certainty == VaultCertainty::Unknown => CredentialOutcome::Unknown,
        Err(_) => CredentialOutcome::Rejected,
    };
    // Vault certainty is an audited result, not physical-thread cleanup. The
    // separately tracked vault shutdown custodian waits for its real OS worker.
    settled(
        state
            .store
            .lock()
            .unwrap()
            .finish_credential_operation(&id, outcome),
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
#[path = "provider_api/tests.rs"]
mod tests;
