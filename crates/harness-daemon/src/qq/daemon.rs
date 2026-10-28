//! One active configuration generation, with a shared token cache for outbound
//! gateway and passive delivery workers. Explicit fixture mode never loads OS
//! credentials or permits a user-configured remote token endpoint.
use super::{
    configuration::{self, Config},
    persistence::{DeliveryOutcome, OutboxClaim},
    runtime::{self, Backend, ConnectionState},
    transport::{Checkpoint, Credentials, Transport},
};
use crate::{
    capabilities::vault::{CredentialRef, VaultCancellation},
    AppState,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::watch;
use zeroize::Zeroizing;

pub(crate) fn fixture_origin() -> Option<String> {
    if std::env::var("HARNESS_QQ_FIXTURE_MODE").as_deref() != Ok("1") {
        return None;
    }
    std::env::var("HARNESS_QQ_FIXTURE_URL").ok()
}
pub(crate) fn spawn(state: Arc<AppState>) {
    let owned = state.clone();
    let handle = tokio::spawn(async move {
        supervise(owned).await;
    });
    *state.qq_worker.lock().unwrap() = Some(handle);
}
async fn wait(state: &AppState, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {_=state.wake.notified()=>{},_=tokio::time::sleep(Duration::from_millis(500))=>{},changed=stop.changed()=>{if changed.is_err(){return false}}}
    !*stop.borrow()
}
async fn supervise(state: Arc<AppState>) {
    let mut stop = state.qq_stop.subscribe();
    while !*stop.borrow() {
        let config = state
            .store
            .lock()
            .unwrap()
            .qq_config()
            .ok()
            .flatten()
            .filter(|c| c.enabled);
        let Some(config) = config else {
            if !wait(&state, &mut stop).await {
                break;
            }
            continue;
        };
        let transport =
            tokio::select! {_=stop.changed()=>break,result=transport(&state,&config)=>result};
        let backend = Arc::new(DaemonBackend {
            state: state.clone(),
            id: config.id.clone(),
            revision: config.revision,
        });
        match transport {
            Ok(receive) if backend.current() => {
                let delivery = receive.fork();
                runtime::run(backend, receive, delivery, stop.clone()).await;
            }
            Ok(_) => {}
            Err(_) => {
                backend.connection(ConnectionState::CredentialUnavailable);
                // A locked/denied OS vault is not repeatedly prompted in a retry loop.
                // Explicitly save/rotate/enable a new config revision to attempt again.
                while backend.current() {
                    if !wait(&state, &mut stop).await {
                        return;
                    }
                }
            }
        }
    }
}
async fn transport(state: &AppState, config: &Config) -> Result<Transport> {
    if let Some(origin) = fixture_origin() {
        return Transport::fixture(
            Credentials::new(
                config.app_id.clone(),
                Zeroizing::new(b"self-agent-qq-fixture-secret".to_vec()),
            )?,
            &origin,
        );
    }
    let binding = super::credential_binding(
        &config.id,
        &configuration::credential_binding_digest(&config.app_id)?,
    )?;
    let reference = CredentialRef::parse(
        config.credential_ref().context("QQ 独立凭据未配置")?,
        &binding,
    )?;
    let secret = state
        .vault
        .get(
            &reference,
            &binding,
            Instant::now() + Duration::from_secs(20),
            &VaultCancellation::default(),
        )
        .await?;
    Transport::production(Credentials::new(
        config.app_id.clone(),
        Zeroizing::new(secret.expose_secret().to_vec()),
    )?)
}
struct DaemonBackend {
    state: Arc<AppState>,
    id: String,
    revision: u64,
}
impl Backend for DaemonBackend {
    fn current(&self) -> bool {
        !*self.state.qq_stop.borrow()
            && self
                .state
                .store
                .lock()
                .unwrap()
                .qq_config()
                .ok()
                .flatten()
                .is_some_and(|c| c.enabled && c.id == self.id && c.revision == self.revision)
    }
    fn checkpoint(&self) -> Result<Option<Checkpoint>> {
        self.state
            .store
            .lock()
            .unwrap()
            .qq_checkpoint(&self.id, self.revision)
    }
    fn ingest(&self, payload: &Value, checkpoint: &Checkpoint) -> Result<()> {
        self.state
            .store
            .lock()
            .unwrap()
            .qq_ingest(&self.id, self.revision, payload, checkpoint)?;
        self.state.wake.notify_waiters();
        Ok(())
    }
    fn clear_checkpoint(&self) -> Result<()> {
        self.state
            .store
            .lock()
            .unwrap()
            .qq_clear_checkpoint(&self.id, self.revision)
    }
    fn connection(&self, state: ConnectionState) {
        let _ = self
            .state
            .store
            .lock()
            .unwrap()
            .qq_connection(&self.id, self.revision, state);
    }
    fn process_input(&self) -> Result<bool> {
        let settings = self.state.settings.read().unwrap().clone();
        let (progress, event) = self.state.store.lock().unwrap().qq_process_input(
            &self.id,
            self.revision,
            &settings,
        )?;
        if let Some(event) = event {
            self.state.publish(event)
        }
        if progress {
            self.state.wake.notify_waiters()
        }
        Ok(progress)
    }
    fn claim(&self) -> Result<Option<OutboxClaim>> {
        self.state
            .store
            .lock()
            .unwrap()
            .qq_claim(&self.id, self.revision)
    }
    fn allowed(&self, claim: &OutboxClaim) -> bool {
        self.state
            .store
            .lock()
            .unwrap()
            .qq_allowed(&self.id, self.revision, claim)
            .unwrap_or(false)
    }
    fn finish(&self, claim: &OutboxClaim, outcome: DeliveryOutcome) -> Result<()> {
        self.state.store.lock().unwrap().qq_finish(claim, outcome)
    }
}
