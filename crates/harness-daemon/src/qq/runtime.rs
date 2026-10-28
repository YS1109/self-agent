//! Bounded adapter workers. SQLite work runs on awaited blocking workers, never
//! holding a database lock across transport awaits. One backend is fenced to
//! one immutable configuration revision; heartbeat remains independently polled.
use super::{
    persistence::{ConversationKind, DeliveryOutcome, FailureReason, OutboxClaim},
    transport::{self, Checkpoint, GatewayItem, Reply, ReplyKind, SendOutcome, Transport},
};
use anyhow::Result;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    Connecting,
    Ready,
    Disconnected,
    StorageUnavailable,
    CredentialUnavailable,
}

/// Implementations use short authoritative transactions and never trust a route,
/// principal or revision supplied by remote input. Failing reads fail closed.
pub(crate) trait Backend: Send + Sync + 'static {
    fn current(&self) -> bool;
    fn checkpoint(&self) -> Result<Option<Checkpoint>>;
    /// Commit input AND checkpoint atomically before returning success.
    fn ingest(&self, payload: &Value, checkpoint: &Checkpoint) -> Result<()>;
    fn clear_checkpoint(&self) -> Result<()>;
    fn connection(&self, state: ConnectionState);
    /// At most one restricted input transaction; uncertain commits are recovered
    /// by command/inbox ID, never by repeating the execution.
    fn process_input(&self) -> Result<bool>;
    /// Return only a durably claimed sending row, with immutable nonce and route.
    fn claim(&self) -> Result<Option<OutboxClaim>>;
    /// Recheck exact principal/revision/expiry before auth and every POST.
    fn allowed(&self, claim: &OutboxClaim) -> bool;
    /// CAS on original claim nonce. Never reset unknown/sent to pending.
    fn finish(&self, claim: &OutboxClaim, outcome: DeliveryOutcome) -> Result<()>;
}
const TICK: Duration = Duration::from_millis(100);
async fn database<B: Backend, T: Send + 'static>(
    backend: Arc<B>,
    operation: impl FnOnce(&B) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || operation(backend.as_ref()))
        .await
        .map_err(|_| anyhow::anyhow!("QQ 本地事务工作中断"))?
}
async fn current(backend: Arc<impl Backend>) -> bool {
    database(backend, |b| Ok(b.current()))
        .await
        .unwrap_or(false)
}
async fn connection(backend: Arc<impl Backend>, state: ConnectionState) {
    let _ = database(backend, move |b| {
        b.connection(state);
        Ok(())
    })
    .await;
}
async fn stopped(backend: Arc<impl Backend>, stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow() || !current(backend.clone()).await {
            return;
        }
        tokio::select! {
            changed = stop.changed() => if changed.is_err() { return; },
            _ = tokio::time::sleep(TICK) => {}
        }
    }
}
async fn pause(
    backend: Arc<impl Backend>,
    stop: &mut watch::Receiver<bool>,
    duration: Duration,
) -> bool {
    tokio::select! {
        _ = stopped(backend, stop) => false,
        _ = tokio::time::sleep(duration) => true,
    }
}
/// Await the entire generation before starting another. Receive, restricted
/// command dispatch and delivery progress independently; a model or HTTP reply
/// cannot block status/cancel ingestion. Interrupted POST means unknown.
pub(crate) async fn run(
    backend: Arc<impl Backend>,
    receive: Transport,
    delivery: Transport,
    stop: watch::Receiver<bool>,
) {
    tokio::join!(
        receive_loop(backend.clone(), receive, stop.clone()),
        command_loop(backend.clone(), stop.clone()),
        delivery_loop(backend, delivery, stop),
    );
}
async fn receive_loop(
    backend: Arc<impl Backend>,
    mut transport: Transport,
    mut stop: watch::Receiver<bool>,
) {
    let mut attempt = 0u32;
    while !*stop.borrow() && current(backend.clone()).await {
        let checkpoint = match database(backend.clone(), |b| b.checkpoint()).await {
            Ok(value) => value,
            Err(_) => {
                connection(backend.clone(), ConnectionState::StorageUnavailable).await;
                if !pause(backend.clone(), &mut stop, Duration::from_secs(1)).await {
                    break;
                }
                continue;
            }
        };
        connection(backend.clone(), ConnectionState::Connecting).await;
        let opened = tokio::select! {
            _ = stopped(backend.clone(), &mut stop) => break,
            result = transport.connect(checkpoint) => result,
        };
        let mut storage_failed = false;
        if let Ok(mut gateway) = opened {
            let connected_at = tokio::time::Instant::now();
            loop {
                let item = tokio::select! {
                    _ = stopped(backend.clone(), &mut stop) => break,
                    result = gateway.next() => result,
                };
                match item {
                    Ok(GatewayItem::Dispatch {
                        payload,
                        checkpoint,
                    }) => {
                        if *stop.borrow() || !current(backend.clone()).await {
                            break;
                        }
                        let committed_point = checkpoint.clone();
                        if database(backend.clone(), move |b| {
                            b.ingest(&payload, &committed_point)
                        })
                        .await
                        .is_err()
                        {
                            connection(backend.clone(), ConnectionState::StorageUnavailable).await;
                            storage_failed = true;
                            break;
                        }
                        if gateway.commit_ack(&checkpoint).is_err() {
                            break;
                        }
                        // READY followed by immediate disconnect must not reset
                        // backoff indefinitely. Only a stable connection does.
                        if connected_at.elapsed() >= Duration::from_secs(30) {
                            attempt = 0;
                        }
                        connection(backend.clone(), ConnectionState::Ready).await;
                    }
                    Ok(GatewayItem::InvalidSession) => {
                        if database(backend.clone(), |b| b.clear_checkpoint())
                            .await
                            .is_err()
                        {
                            connection(backend.clone(), ConnectionState::StorageUnavailable).await;
                            storage_failed = true;
                        }
                        break;
                    }
                    Ok(GatewayItem::Reconnect) | Err(_) => break,
                }
            }
            gateway.shutdown().await;
        }
        if !storage_failed {
            connection(backend.clone(), ConnectionState::Disconnected).await;
        }
        if !pause(
            backend.clone(),
            &mut stop,
            transport::reconnect_delay(attempt),
        )
        .await
        {
            break;
        }
        attempt = attempt.saturating_add(1);
    }
    connection(backend, ConnectionState::Disconnected).await;
}
async fn command_loop(backend: Arc<impl Backend>, mut stop: watch::Receiver<bool>) {
    while !*stop.borrow() && current(backend.clone()).await {
        match database(backend.clone(), |b| b.process_input()).await {
            Ok(true) => tokio::task::yield_now().await,
            Ok(false) | Err(_) => {
                if !pause(backend.clone(), &mut stop, TICK).await {
                    break;
                }
            }
        }
    }
}
async fn delivery_loop(
    backend: Arc<impl Backend>,
    mut transport: Transport,
    mut stop: watch::Receiver<bool>,
) {
    while !*stop.borrow() && current(backend.clone()).await {
        let claim = match database(backend.clone(), |b| b.claim()).await {
            Ok(Some(value)) => value,
            Ok(None) | Err(_) => {
                if !pause(backend.clone(), &mut stop, TICK).await {
                    break;
                }
                continue;
            }
        };
        let send_stop = stop.clone();
        let allowed = || {
            let backend = backend.clone();
            let claim = claim.clone();
            let send_stop = send_stop.clone();
            async move {
                if *send_stop.borrow() {
                    return false;
                }
                let valid = database(backend, move |b| Ok(b.current() && b.allowed(&claim)))
                    .await
                    .unwrap_or(false);
                valid && !*send_stop.borrow()
            }
        };
        if !allowed().await {
            finish(
                backend.clone(),
                claim.clone(),
                DeliveryOutcome::Failed {
                    reason: FailureReason::ConfigurationDisabled,
                },
            )
            .await;
            continue;
        }
        let reply = Reply {
            kind: match claim.route.identity.kind {
                ConversationKind::User => ReplyKind::User,
                ConversationKind::Group => ReplyKind::Group,
            },
            openid: &claim.route.identity.target_id,
            msg_id: &claim.route.message_id,
            msg_seq: claim.record.msg_seq,
            content: &claim.record.payload,
            expires_at: std::time::UNIX_EPOCH + Duration::from_millis(claim.route.expires_at_ms),
        };
        let outcome = tokio::select! {
            // Dropping an HTTP future cannot establish whether QQ received it.
            _ = stopped(backend.clone(), &mut stop) => DeliveryOutcome::Unknown,
            result = transport.send_passive_with_control(reply, &allowed) => match result {
                Ok(SendOutcome::Sent { id }) => DeliveryOutcome::Sent { platform_id: id },
                Ok(SendOutcome::Unknown) => DeliveryOutcome::Unknown,
                Ok(SendOutcome::Rejected { status: 429, retry_after_seconds }) => {
                    let delay = retry_after_seconds.unwrap_or(1).clamp(1, 3600) * 1000;
                    DeliveryOutcome::RateLimited { retry_at_ms: now_ms().saturating_add(delay) }
                }
                Ok(SendOutcome::Rejected { .. }) => DeliveryOutcome::Failed { reason: FailureReason::PlatformRejected },
                // Errors precede POST (or follow a definite 401 rejection).
                Err(_) => DeliveryOutcome::Failed { reason: FailureReason::PlatformRejected },
            },
        };
        finish(backend.clone(), claim, outcome).await;
    }
}
async fn finish(backend: Arc<impl Backend>, claim: OutboxClaim, outcome: DeliveryOutcome) {
    let _ = database(backend, move |b| {
        if b.finish(&claim, outcome).is_err() {
            // Retry only the local CAS, never HTTP. Prior commit is preserved by
            // the original nonce and terminal-state check.
            b.finish(&claim, DeliveryOutcome::Unknown)?;
        }
        Ok(())
    })
    .await;
}
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}
#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
