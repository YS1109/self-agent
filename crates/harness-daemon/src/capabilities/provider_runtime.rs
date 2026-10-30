//! A selected named provider never falls back to manual/environment credentials.
//! Resolve one private vault value, then fence the immutable target and revision
//! before network sends and while awaiting their response.
use super::{
    providers,
    vault::{CredentialRef, SecretBytes, VaultBinding, VaultCancellation},
};
use crate::{execution_control::RunControl, store::providers::ProviderRecord, AppState, Settings};
use anyhow::{bail, Result};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn metadata(settings: &Settings, record: &ProviderRecord, model: &str) -> Settings {
    Settings {
        provider_source: "manual".into(),
        provider_id: None,
        base_url: record.definition.base_url().into(),
        models_endpoint: record.definition.models_url().into(),
        model: model.into(),
        api_key: String::new(),
        provider_headers: Default::default(),
        ..settings.clone()
    }
}

async fn read_key(
    state: &Arc<AppState>,
    reference: &CredentialRef,
    binding: &VaultBinding,
    deadline: Instant,
    stopped: impl Fn() -> bool,
) -> Result<SecretBytes> {
    let cancellation = VaultCancellation::default();
    let operation = state.vault.get(
        reference,
        binding,
        deadline.min(Instant::now() + Duration::from_secs(15)),
        &cancellation,
    );
    tokio::pin!(operation);
    loop {
        if stopped() {
            cancellation.cancel();
        }
        tokio::select! {
            result = &mut operation => return result.map_err(anyhow::Error::from),
            _ = tokio::time::sleep(Duration::from_millis(25)) => {},
        }
    }
}

pub(crate) async fn task_settings(
    state: &Arc<AppState>,
    task_id: &str,
    settings: &Settings,
    executor: &str,
    control: &RunControl,
) -> Result<Settings> {
    control.check()?;
    let pinned = state.store.lock().unwrap().task_provider(task_id)?;
    let Some(pinned) = pinned else {
        if executor == "codex" {
            // Codex owns its credentials. Never inject a global named key.
            let mut own = settings.clone();
            if own.provider_source == "named" {
                own.api_key.clear();
                own.provider_headers.clear();
            }
            return Ok(crate::configuration::resolve(&own).unwrap_or(own));
        }
        if settings.provider_source == "named" {
            bail!("任务没有固定模型服务快照，未发送请求");
        }
        return crate::configuration::resolve(settings);
    };
    if executor != "native" {
        bail!("此执行器不支持应用托管模型服务");
    }
    let record = state.store.lock().unwrap().provider_record(pinned.id())?;
    pinned.check_current(
        &record.id,
        record.revision,
        record.enabled,
        &record.definition,
        record.credential_ref.as_ref(),
    )?;
    let mut resolved = metadata(settings, &record, pinned.model());
    if let Some(reference) = pinned.credential_ref() {
        let secret = read_key(
            state,
            reference,
            &pinned.binding()?,
            control.deadline().into_std(),
            || control.stopped(),
        )
        .await?;
        providers::validate_api_key(&secret)?;
        resolved.api_key = std::str::from_utf8(secret.expose_secret())?.to_owned();
        providers::validate_definition(
            record.definition.definition().clone(),
            &[resolved.api_key.clone()],
        )?;
    }
    control.check()?;
    crate::configuration::resolve(&resolved)
}

pub(crate) async fn coordinator_settings(
    state: &Arc<AppState>,
    job_id: &str,
    settings: &Settings,
) -> Result<Settings> {
    let pinned = state.store.lock().unwrap().coordinator_provider(job_id)?;
    let Some(pinned) = pinned else {
        if settings.provider_source == "named" {
            bail!("主智能体作业没有固定模型服务快照，未发送请求");
        }
        return crate::configuration::resolve(settings);
    };
    state
        .store
        .lock()
        .unwrap()
        .check_coordinator_provider(job_id)?;
    let record = state.store.lock().unwrap().provider_record(pinned.id())?;
    let mut resolved = metadata(settings, &record, pinned.model());
    if let Some(reference) = pinned.credential_ref() {
        let secret = read_key(
            state,
            reference,
            &pinned.binding()?,
            Instant::now() + Duration::from_secs(15),
            || {
                let store = state.store.lock().unwrap();
                !store.coordinator_job_active(job_id).unwrap_or(false)
                    || store.check_coordinator_provider(job_id).is_err()
            },
        )
        .await?;
        providers::validate_api_key(&secret)?;
        resolved.api_key = std::str::from_utf8(secret.expose_secret())?.into();
        providers::validate_definition(
            record.definition.definition().clone(),
            &[resolved.api_key.clone()],
        )?;
    }
    state
        .store
        .lock()
        .unwrap()
        .check_coordinator_provider(job_id)?;
    crate::configuration::resolve(&resolved)
}

pub(crate) struct DiscoveryProvider {
    pub settings: Settings,
    record: ProviderRecord,
}
impl DiscoveryProvider {
    pub(crate) fn check(&self, state: &AppState) -> Result<()> {
        let current = state
            .store
            .lock()
            .unwrap()
            .provider_record(&self.record.id)?;
        if current.revision != self.record.revision
            || current.enabled != self.record.enabled
            || current.definition != self.record.definition
            || current.credential_ref != self.record.credential_ref
        {
            bail!("模型服务或凭据已改变，未继续请求且没有自动切换服务");
        }
        Ok(())
    }
}

pub(crate) async fn discovery(
    state: &Arc<AppState>,
    id: &str,
    base: &Settings,
) -> Result<DiscoveryProvider> {
    let record = state.store.lock().unwrap().provider_record(id)?;
    let settings = metadata(base, &record, &record.definition.definition().model);
    let mut selected = DiscoveryProvider { settings, record };
    if let Some(reference) = &selected.record.credential_ref {
        let binding = providers::credential_binding(id, &selected.record.definition)?;
        let secret = read_key(
            state,
            reference,
            &binding,
            Instant::now() + Duration::from_secs(15),
            || selected.check(state).is_err(),
        )
        .await?;
        providers::validate_api_key(&secret)?;
        selected.settings.api_key = std::str::from_utf8(secret.expose_secret())?.to_owned();
        providers::validate_definition(
            selected.record.definition.definition().clone(),
            &[selected.settings.api_key.clone()],
        )?;
    }
    selected.check(state)?;
    Ok(selected)
}

/// Metadata readiness is not an OS-vault unlock or a model health check.
pub(crate) fn public_settings(state: &AppState, settings: &Settings) -> serde_json::Value {
    let mut public = settings.public();
    if settings.provider_source != "named" {
        return public;
    }
    public["api_key_configured"] = false.into();
    public["configured"] = false.into();
    public["base_url"] = "".into();
    public["configuration_error"] = "请选择已启用的命名模型服务；凭据存储在执行时检查".into();
    if let Some(record) = settings
        .provider_id
        .as_deref()
        .and_then(|id| state.store.lock().unwrap().provider_record(id).ok())
    {
        public["base_url"] = record.definition.base_url().into();
        public["models_endpoint"] = record.definition.models_url().into();
        if settings.main_executor == "native" {
            public["model"] = record.definition.definition().model.clone().into();
        }
        public["api_key_configured"] = record.credential_ref.is_some().into();
        public["credential_status"] = if record.credential_ref.is_some() {
            "bound_not_checked"
        } else {
            "not_configured"
        }
        .into();
        let ready = record.enabled && !record.definition.definition().model.is_empty();
        public["configured"] = ready.into();
        if ready {
            public["configuration_error"] = serde_json::Value::Null;
        }
    }
    public
}

#[cfg(test)]
#[path = "provider_runtime/tests.rs"]
mod tests;
