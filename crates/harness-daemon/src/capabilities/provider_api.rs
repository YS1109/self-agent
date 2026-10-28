//! Owner-only named service metadata and one-shot vault mutations. No secret is
//! logged, serialized into a receipt or passed to SQLite. A disconnected HTTP
//! client cannot abort a claimed mutation; it recovers the outcome via GET.
use super::{
    providers,
    vault::{FreshCredential, SecretBytes, VaultCancellation, VaultCertainty},
};
use crate::{
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
    let settings = state.settings.read().unwrap().clone();
    let settings = crate::configuration::resolve(&settings).unwrap_or(settings);
    let secrets = crate::continuation::known_secrets(&settings);
    state
        .store
        .lock()
        .unwrap()
        .provider_command(&envelope, &secrets)
        .map(Json)
        .map_err(invalid)
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
    let receipt = state
        .store
        .lock()
        .unwrap()
        .begin_provider_credential_set(
            &request.id,
            &provider_id,
            request.expected_revision,
            fresh.reference(),
        )
        .map_err(invalid)?;
    if receipt["replayed"] != true {
        let state = state.clone();
        tokio::spawn(async move {
            mutate(state, request.id, Some(fresh)).await;
        });
    }
    Ok(Json(receipt))
}

pub(crate) async fn delete_credential(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    Json(request): Json<DeleteRequest>,
) -> ApiResult<Json<Value>> {
    let receipt = state
        .store
        .lock()
        .unwrap()
        .begin_provider_credential_delete(&request.id, &provider_id, request.expected_revision)
        .map_err(invalid)?;
    if receipt["replayed"] != true {
        let state = state.clone();
        tokio::spawn(async move {
            mutate(state, request.id, None).await;
        });
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
        .begin_credential_cleanup(&request.id, &target)
        .map_err(invalid)?;
    if receipt["replayed"] != true {
        let state = state.clone();
        tokio::spawn(async move {
            mutate(state, request.id, None).await;
        });
    }
    Ok(Json(receipt))
}

async fn mutate(state: Arc<AppState>, id: String, fresh: Option<FreshCredential>) {
    let work = match state.store.lock().unwrap().claim_credential_operation(&id) {
        Ok(Some(work)) => work,
        _ => return,
    };
    if work.operation_id != id || uuid::Uuid::parse_str(&work.owner_id).is_err() {
        return;
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
            let _ = state
                .store
                .lock()
                .unwrap()
                .finish_credential_operation(&id, CredentialOutcome::Rejected);
            return;
        }
    };
    let outcome = match result {
        Ok(()) => CredentialOutcome::Succeeded,
        Err(error) if error.certainty == VaultCertainty::Unknown => CredentialOutcome::Unknown,
        Err(_) => CredentialOutcome::Rejected,
    };
    let _ = state
        .store
        .lock()
        .unwrap()
        .finish_credential_operation(&id, outcome);
}

#[cfg(test)]
#[path = "provider_api/tests.rs"]
mod tests;
