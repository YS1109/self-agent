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
    reliability::lifecycle::{WorkerExit, WorkerKind},
    AppState,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
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
pub(crate) fn spawn(state: Arc<AppState>) -> Result<()> {
    let owned = state.clone();
    state.lifecycle.spawn(WorkerKind::Channel, async move {
        match supervise(owned).await {
            Ok(()) => WorkerExit::Settled,
            Err(_) => WorkerExit::Unknown,
        }
    })?;
    Ok(())
}
async fn wait(state: &AppState, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {_=state.wake.notified()=>{},_=tokio::time::sleep(Duration::from_millis(500))=>{},changed=stop.changed()=>{if changed.is_err(){return false}}}
    !*stop.borrow()
}
async fn supervise(state: Arc<AppState>) -> Result<()> {
    let mut stop = state.qq_stop.subscribe();
    while !*stop.borrow() {
        let config = {
            let store = state.store.lock().unwrap();
            if state.lifecycle.check_locked(&store).is_err() {
                break;
            }
            store.qq_config()?.filter(|c| c.enabled)
        };
        let Some(config) = config else {
            if !wait(&state, &mut stop).await {
                break;
            }
            continue;
        };
        // Vault owns its OS worker; keep the read observation until it settles.
        // Shutdown must not cancel/drop this admitted generation's await.
        let transport = transport(&state, &config).await;
        let backend = Arc::new(DaemonBackend {
            state: state.clone(),
            id: config.id.clone(),
            revision: config.revision,
            failed: AtomicBool::new(false),
        });
        match transport {
            Ok(receive) if backend.current() => {
                let delivery = receive.fork();
                runtime::run(backend.clone(), receive, delivery, stop.clone()).await?;
            }
            Ok(_) => {}
            Err(_) => {
                backend.connection(ConnectionState::CredentialUnavailable);
                // A locked/denied OS vault is not repeatedly prompted in a retry loop.
                // Explicitly save/rotate/enable a new config revision to attempt again.
                while backend.current() {
                    if !wait(&state, &mut stop).await {
                        return Ok(());
                    }
                }
            }
        }
        anyhow::ensure!(backend.worker_certain(), "QQ 持久确认未完成");
        // Transport runtime owns and reports its joined generation. Credential
        // failure itself is definite no-connect, not a cleanup failure.
    }
    Ok(())
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
    failed: AtomicBool,
}
impl Backend for DaemonBackend {
    fn failed_worker(&self) {
        self.failed.store(true, Ordering::Release);
    }
    fn worker_certain(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
    }

    fn current(&self) -> bool {
        self.worker_certain()
            && !self.state.lifecycle.signal().is_cancelled()
            && !*self.state.qq_stop.borrow()
            && match self.state.store.lock().unwrap().qq_config() {
                Ok(config) => config
                    .is_some_and(|c| c.enabled && c.id == self.id && c.revision == self.revision),
                Err(_) => {
                    self.failed_worker();
                    false
                }
            }
    }
    fn checkpoint(&self) -> Result<Option<Checkpoint>> {
        self.state
            .store
            .lock()
            .unwrap()
            .qq_checkpoint(&self.id, self.revision)
    }
    fn ingest(&self, payload: &Value, checkpoint: &Checkpoint) -> Result<()> {
        let mut store = self.state.store.lock().unwrap();
        self.state.lifecycle.check_locked(&store)?;
        store.qq_ingest(&self.id, self.revision, payload, checkpoint)?;
        drop(store);
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
        let mut store = self.state.store.lock().unwrap();
        // A newer configuration legitimately owns connection metadata now.
        // A stale generation must not overwrite it or invent cleanup Unknown.
        match store.qq_config() {
            Ok(Some(c)) if c.id == self.id && c.revision == self.revision => {}
            Ok(_) => return,
            Err(_) => {
                self.failed_worker();
                return;
            }
        }
        if store.qq_connection(&self.id, self.revision, state).is_err() {
            self.failed_worker();
        }
    }
    fn process_input(&self) -> Result<bool> {
        let settings = self.state.settings.read().unwrap().clone();
        let mut store = self.state.store.lock().unwrap();
        self.state.lifecycle.check_locked(&store)?;
        let (progress, event) = store.qq_process_input(&self.id, self.revision, &settings)?;
        drop(store);
        if let Some(event) = event {
            self.state.publish(event)
        }
        if progress {
            self.state.wake.notify_waiters()
        }
        Ok(progress)
    }
    fn claim(&self) -> Result<Option<OutboxClaim>> {
        let mut store = self.state.store.lock().unwrap();
        self.state.lifecycle.check_locked(&store)?;
        store.qq_claim(&self.id, self.revision)
    }
    fn allowed(&self, claim: &OutboxClaim) -> bool {
        let mut store = self.state.store.lock().unwrap();
        if self.state.lifecycle.check_locked(&store).is_err() {
            return false;
        }
        match store.qq_allowed(&self.id, self.revision, claim) {
            Ok(allowed) => allowed,
            Err(error) => {
                if runtime::storage_uncertain(&error) {
                    self.failed_worker();
                }
                false
            }
        }
    }

    fn finish(&self, claim: &OutboxClaim, outcome: DeliveryOutcome) -> Result<()> {
        self.state.store.lock().unwrap().qq_finish(claim, outcome)
    }
}
