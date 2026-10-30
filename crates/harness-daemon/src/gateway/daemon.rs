//! One outbound configuration generation. Browser commands remain bounded local
//! transactions; reconnection never replays them. Vault reads are one-shot per
//! config revision and a denied/unknown read waits for an explicit owner change.
use super::{
    configuration::Config,
    persistence::TrustedChannel,
    transport::{self, Backend, Transport},
    wire::{GatewayCode, GatewayRequest, GatewayResult},
};
use crate::{
    capabilities::vault::{CredentialRef, VaultCancellation},
    reliability::lifecycle::{WorkerExit, WorkerKind},
    AppState,
};
use anyhow::{Context, Result};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
use zeroize::Zeroizing;

pub(crate) fn fixture_enabled() -> bool {
    std::env::var("HARNESS_GATEWAY_FIXTURE_MODE").as_deref() == Ok("1")
}
fn fixture_origin() -> Result<String> {
    std::env::var("HARNESS_GATEWAY_FIXTURE_URL").context("网关夹具缺少数字 loopback 地址")
}
pub(crate) fn spawn(state: Arc<AppState>) -> Result<()> {
    let lifecycle = state.lifecycle.clone();
    lifecycle.spawn(WorkerKind::Channel, async move { supervise(state).await })?;
    Ok(())
}
async fn database<T: Send + 'static>(
    state: Arc<AppState>,
    f: impl FnOnce(&mut crate::store::Store) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || f(&mut state.store.lock().unwrap()))
        .await
        .context("网关数据库工作中断")?
}
async fn wait(state: &AppState, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! { _=state.wake.notified()=>{}, _=tokio::time::sleep(Duration::from_millis(500))=>{}, result=stop.changed()=>{if result.is_err(){return false}} }
    !*stop.borrow() && !state.lifecycle.signal().is_cancelled()
}
async fn supervise(state: Arc<AppState>) -> WorkerExit {
    let fault = Arc::new(AtomicBool::new(false));
    let mut stop = state.gateway_stop.subscribe();
    while !*stop.borrow()
        && !state.lifecycle.signal().is_cancelled()
        && !fault.load(Ordering::Acquire)
    {
        let config = match database(state.clone(), |s| s.gateway_config()).await {
            Ok(value) => value.filter(|c| c.enabled),
            Err(_) => return WorkerExit::Unknown,
        };
        let Some(config) = config else {
            if !wait(&state, &mut stop).await {
                break;
            }
            continue;
        };
        let backend = Arc::new(DaemonBackend {
            state: state.clone(),
            fault: fault.clone(),
            id: config.id.clone(),
            channel: TrustedChannel {
                device_id: config.device_id.clone(),
                config_revision: config.revision,
            },
            redaction_token: std::sync::Mutex::new(Zeroizing::new(String::new())),
        });
        let ready = transport(&state, &config, stop.clone()).await;
        match ready {
            Ok((transport, token)) => {
                *backend.redaction_token.lock().unwrap() = token;
                transport::run(transport, backend, stop.clone()).await;
            }
            Err(_) => {
                let id = config.id.clone();
                let rev = config.revision;
                if database(state.clone(), move |s| {
                    s.gateway_connection(&id, rev, "credential_unavailable")
                })
                .await
                .is_err_and(|error| storage_uncertain(&error))
                {
                    return WorkerExit::Unknown;
                }
                // Do not turn a locked/denied OS vault into an automatic prompt loop.
                while database(state.clone(), {
                    let id = config.id.clone();
                    move |s| {
                        Ok(s.gateway_config()?
                            .is_some_and(|c| c.id == id && c.revision == rev && c.enabled))
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    fault.store(true, Ordering::Release);
                    false
                }) {
                    if !wait(&state, &mut stop).await {
                        return if fault.load(Ordering::Acquire) {
                            WorkerExit::Unknown
                        } else {
                            WorkerExit::Settled
                        };
                    }
                }
            }
        }
    }
    if fault.load(Ordering::Acquire) {
        WorkerExit::Unknown
    } else {
        WorkerExit::Settled
    }
}
fn storage_uncertain(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<rusqlite::Error>().is_some()
            || cause.downcast_ref::<tokio::task::JoinError>().is_some()
    })
}
async fn transport(
    state: &Arc<AppState>,
    config: &Config,
    mut stop: watch::Receiver<bool>,
) -> Result<(Transport, Zeroizing<String>)> {
    if *stop.borrow() || state.lifecycle.signal().is_cancelled() {
        anyhow::bail!("后台停止接收网关连接");
    }
    if fixture_enabled() {
        // Process-only test override; never persisted or obtainable through API.
        let token = Zeroizing::new(
            std::env::var("HARNESS_GATEWAY_FIXTURE_TOKEN").context("网关夹具缺少临时设备凭据")?,
        );
        let transport = Transport::new(
            &fixture_origin()?,
            config.device_id.clone(),
            token.clone(),
            true,
        )?;
        return Ok((transport, token));
    }
    let binding = super::credential_binding(&config.id, &config.scope_digest)?;
    let reference = CredentialRef::parse(
        config.credential_ref().context("网关独立设备凭据未配置")?,
        &binding,
    )?;
    let cancellation = VaultCancellation::default();
    let read = state.vault.get(
        &reference,
        &binding,
        Instant::now() + Duration::from_secs(20),
        &cancellation,
    );
    tokio::pin!(read);
    let secret = loop {
        if *stop.borrow() || state.lifecycle.signal().is_cancelled() {
            cancellation.cancel();
        }
        tokio::select! {
            result=&mut read=>break result?,
            changed=stop.changed()=>{if changed.is_err() || *stop.borrow(){cancellation.cancel();}},
            _=tokio::time::sleep(Duration::from_millis(100))=>{
                let id=config.id.clone();let rev=config.revision;
                if !database(state.clone(),move|s|Ok(s.gateway_config()?.is_some_and(|c|c.id==id&&c.revision==rev&&c.enabled))).await.unwrap_or(false){cancellation.cancel();}
            }
        }
    };
    super::configuration::validate_device_token(secret.expose_secret())?;
    let token = Zeroizing::new(std::str::from_utf8(secret.expose_secret())?.to_owned());
    Ok((
        Transport::new(
            &config.origin,
            config.device_id.clone(),
            token.clone(),
            false,
        )?,
        token,
    ))
}
struct DaemonBackend {
    state: Arc<AppState>,
    fault: Arc<AtomicBool>,
    id: String,
    channel: TrustedChannel,
    redaction_token: std::sync::Mutex<Zeroizing<String>>,
}
impl Backend for DaemonBackend {
    fn current(&self) -> bool {
        !*self.state.gateway_stop.borrow()
            && !self.state.lifecycle.signal().is_cancelled()
            && !self.fault.load(Ordering::Acquire)
            && match self.state.store.lock().unwrap().gateway_config() {
                Ok(config) => config.is_some_and(|c| {
                    c.enabled
                        && c.id == self.id
                        && c.revision == self.channel.config_revision
                        && c.device_id == self.channel.device_id
                }),
                Err(_) => {
                    self.uncertain();
                    false
                }
            }
    }
    fn handle(&self, request: GatewayRequest) -> GatewayResult {
        if !self.current() {
            return GatewayResult::error(GatewayCode::Forbidden);
        }
        let mut settings = self.state.settings.read().unwrap().clone();
        settings
            .redaction_secrets
            .push(self.redaction_token.lock().unwrap().as_str().to_owned());
        let outcome = {
            let mut store = self.state.store.lock().unwrap();
            if self.state.lifecycle.check_locked(&store).is_err() {
                return GatewayResult::error(GatewayCode::Forbidden);
            }
            store.gateway_handle(&self.id, &self.channel, &request, &settings)
        };
        match outcome {
            Ok((result, event)) => {
                if result.code == GatewayCode::OutcomeUnknown {
                    self.uncertain();
                }
                if let Some(event) = event {
                    self.state.publish(event);
                }
                self.state.wake.notify_waiters();
                result
            }
            // Storage commit acknowledgements can be uncertain. Never translate
            // an error into a definitive rejection that invites a new execution.
            Err(_) => {
                self.uncertain();
                GatewayResult::error(
                    if matches!(request.rpc, super::wire::GatewayRpc::Command { .. }) {
                        GatewayCode::OutcomeUnknown
                    } else {
                        GatewayCode::Internal
                    },
                )
            }
        }
    }
    fn uncertain(&self) {
        self.fault.store(true, Ordering::Release);
    }
    fn revoked(&self, principals: &[(String, u64)]) -> Result<Vec<String>> {
        let mut store = self.state.store.lock().unwrap();
        // The fixed channel worker was registered before any receive; shutdown
        // gates new reconciliation while its already accepted receipts settle.
        if self.state.lifecycle.check_locked(&store).is_err() {
            return Ok(Vec::new());
        }
        let result = store.gateway_revoked(&self.id, &self.channel, principals);
        if result.as_ref().is_err_and(storage_uncertain) {
            self.uncertain();
        }
        result
    }
    fn connection(&self, connected: bool) {
        let result = self.state.store.lock().unwrap().gateway_connection(
            &self.id,
            self.channel.config_revision,
            if connected { "ready" } else { "disconnected" },
        );
        if result.as_ref().is_err_and(storage_uncertain) {
            self.uncertain();
        }
    }
}
