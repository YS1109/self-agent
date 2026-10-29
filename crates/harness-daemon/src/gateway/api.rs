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
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    use axum::{
        extract::DefaultBodyLimit,
        routing::{get, post},
        Router,
    };
    Router::new()
        .route("/api/v1/gateway", get(snapshot))
        .route(
            "/api/v1/gateway/commands",
            post(command).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/api/v1/gateway/commands/{id}", get(receipt))
        .route(
            "/api/v1/gateway/credentials/{id}",
            post(set_credential)
                .delete(delete_credential)
                .layer(DefaultBodyLimit::max(8192)),
        )
        .route("/api/v1/gateway/credentials/cleanup/{id}", post(cleanup))
        .route("/api/v1/gateway/credential-operations/{id}", get(operation))
}
fn invalid(_: impl std::fmt::Display) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "网关 操作未确认，请刷新原记录核对；不会回显输入或底层错误".into(),
    )
}
const MAX_ADMITTED: usize = 8;
fn admission() -> &'static Arc<tokio::sync::Semaphore> {
    static LIMIT: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    LIMIT.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_ADMITTED)))
}
fn reserve() -> ApiResult<tokio::sync::OwnedSemaphorePermit> {
    reserve_from(admission())
}
fn reserve_from(
    limit: &Arc<tokio::sync::Semaphore>,
) -> ApiResult<tokio::sync::OwnedSemaphorePermit> {
    limit.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "网关本机操作繁忙；未接收新的操作，请稍后核对原记录".into(),
        )
    })
}
/// No admission wait queue or unbounded blocking jobs. The permit lives inside
/// the blocking job so dropping the HTTP observer cannot free capacity early.
async fn database<T: Send + 'static>(
    state: Arc<AppState>,
    action: impl FnOnce(&mut crate::store::Store) -> anyhow::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let permit = reserve()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut store = state
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store unavailable"))?;
        action(&mut store)
    })
    .await
    .map_err(invalid)?
    .map_err(invalid)
}
/// Already-admitted credential work keeps its reservation through finalization;
/// it must not be rejected for overload after a possible OS side effect.
async fn in_flight<T: Send + 'static>(
    state: Arc<AppState>,
    action: impl FnOnce(&mut crate::store::Store) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(move || {
        let mut store = state
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store unavailable"))?;
        action(&mut store)
    })
    .await?
}
type CredentialStart = (Value, String, Option<FreshCredential>);
async fn credential_request(
    state: Arc<AppState>,
    begin: impl FnOnce(&mut crate::store::Store) -> anyhow::Result<CredentialStart> + Send + 'static,
) -> ApiResult<Json<Value>> {
    let permit = reserve()?;
    tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let (receipt, id, fresh) = {
            let mut store = state
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store unavailable"))?;
            begin(&mut store)?
        };
        if receipt["replayed"] != true {
            // Launch from the durable-admission closure, not the cancellable
            // HTTP continuation. The reserved slot is owned until settle.
            tokio::spawn(mutate(state, id, fresh, permit));
        }
        Ok(receipt)
    })
    .await
    .map_err(invalid)?
    .map(Json)
    .map_err(invalid)
}
pub(crate) async fn snapshot(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    database(state, |store| {
        store.gateway_snapshot(super::daemon::fixture_enabled())
    })
    .await
    .map(Json)
}
pub(crate) async fn command(
    State(state): State<Arc<AppState>>,
    Json(envelope): Json<Value>,
) -> ApiResult<Json<Value>> {
    let settings = state.settings.read().unwrap().clone();
    let wake = state.clone();
    database(state, move |store| {
        let receipt =
            store.gateway_admin_command(&envelope, &settings, super::daemon::fixture_enabled())?;
        wake.wake.notify_waiters();
        Ok(receipt)
    })
    .await
    .map(Json)
}
pub(crate) async fn receipt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Option<Value>>> {
    database(state, move |store| store.gateway_admin_receipt(&id))
        .await
        .map(Json)
}
pub(crate) async fn operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Option<harness_core::CredentialOperationView>>> {
    database(state, move |store| store.gateway_credential_operation(&id))
        .await
        .map(Json)
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
    credential_request(state, move |store| {
        let meta: Metadata = serde_json::from_slice(&body)?;
        // A replay never validates or applies the replacement secret.
        if let Some(receipt) =
            store.gateway_existing_set(&meta.id, &config_id, meta.expected_revision)?
        {
            return Ok((receipt, meta.id, None));
        }
        let request: SetRequest = serde_json::from_slice(&body)?;
        let secret = SecretBytes::new(request.secret.into_bytes())?;
        configuration::validate_device_token(secret.expose_secret())?;
        let config = store
            .gateway_config()?
            .ok_or_else(|| anyhow::anyhow!("config missing"))?;
        if config.id != config_id || config.revision != request.expected_revision {
            anyhow::bail!("config revision");
        }
        let value = std::str::from_utf8(secret.expose_secret())?;
        if config.origin.contains(value) || config.device_id.contains(value) {
            anyhow::bail!("secret in metadata");
        }
        let binding = super::credential_binding(&config_id, &config.scope_digest)?;
        let fresh = FreshCredential::new(&binding, secret);
        let receipt = store.gateway_begin_set(
            &request.id,
            &config_id,
            request.expected_revision,
            fresh.reference(),
        )?;
        Ok((receipt, request.id, Some(fresh)))
    })
    .await
}
pub(crate) async fn delete_credential(
    State(state): State<Arc<AppState>>,
    Path(config_id): Path<String>,
    Json(request): Json<DeleteRequest>,
) -> ApiResult<Json<Value>> {
    credential_request(state, move |store| {
        Ok((
            store.gateway_begin_delete(&request.id, &config_id, request.expected_revision)?,
            request.id,
            None,
        ))
    })
    .await
}
pub(crate) async fn cleanup(
    State(state): State<Arc<AppState>>,
    Path(target): Path<String>,
    Json(request): Json<CleanupRequest>,
) -> ApiResult<Json<Value>> {
    credential_request(state, move |store| {
        Ok((
            store.gateway_begin_cleanup(&request.id, &target)?,
            request.id,
            None,
        ))
    })
    .await
}
async fn mutate(
    state: Arc<AppState>,
    id: String,
    fresh: Option<FreshCredential>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let claimed_id = id.clone();
    let work = match in_flight(state.clone(), move |store| {
        store.gateway_claim_credential(&claimed_id)
    })
    .await
    {
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
            let _ = in_flight(state.clone(), move |store| {
                store.gateway_finish_credential(&id, Outcome::NotApplied)
            })
            .await;
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
            let _ = in_flight(state.clone(), move |store| {
                store.gateway_finish_credential(&id, Outcome::NotApplied)
            })
            .await;
            return;
        }
    };
    let outcome = match result {
        Ok(()) => Outcome::Succeeded,
        Err(error) if error.certainty == VaultCertainty::Unknown => Outcome::Unknown,
        Err(_) => Outcome::NotApplied,
    };
    let _ = in_flight(state.clone(), move |store| {
        store.gateway_finish_credential(&id, outcome)
    })
    .await;
    state.wake.notify_waiters();
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
