//! Authenticated outbound-only gateway transport. No local-owner token, proxy
//! environment, reconnect replay or offline command queue enters this module.
//! Accepted database callbacks are awaited even after socket loss; their durable
//! original command receipt, not a resend, resolves an uncertain outcome.
use super::wire::{
    GatewayCode, GatewayDaemonFrame, GatewayRequest, GatewayResult, GatewayServerFrame,
    GATEWAY_MAX_FRAME_BYTES, GATEWAY_PROTOCOL,
};
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig, Message,
    },
};
use zeroize::Zeroizing;

const IO_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const HEARTBEAT: Duration = Duration::from_secs(15);
#[cfg(test)]
const HEARTBEAT: Duration = Duration::from_millis(50);
const IDLE: Duration = Duration::from_secs(45);
const MAX_INFLIGHT: usize = 8;
const MAX_SEEN_PRINCIPALS: usize = 128;

/// Secrets intentionally have no Debug/Serialize implementation.
pub(crate) struct Transport {
    endpoint: Url,
    device_id: String,
    token: Zeroizing<String>,
}

pub(crate) fn endpoint(origin: &str, device_id: &str, fixture: bool) -> Result<Url> {
    if !uuid::Uuid::parse_str(device_id).is_ok_and(|id| id.to_string() == device_id) {
        bail!("网关设备标识必须为规范 UUID");
    }
    let mut url = Url::parse(origin).map_err(|_| anyhow::anyhow!("网关地址无效"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
        || if fixture {
            url.scheme() != "http" || !loopback
        } else {
            url.scheme() != "https"
        }
    {
        bail!("网关需要无路径、参数或用户信息的 HTTPS origin；夹具仅允许数字 loopback HTTP");
    }
    url.set_scheme(if fixture { "ws" } else { "wss" })
        .map_err(|_| anyhow::anyhow!("网关协议无效"))?;
    url.set_path(&format!("/gateway/device/{device_id}"));
    Ok(url)
}

impl Transport {
    pub(crate) fn new(
        origin: &str,
        device_id: String,
        token: Zeroizing<String>,
        fixture: bool,
    ) -> Result<Self> {
        // Local gateway enrollment emits a high-entropy 64-character lowercase hexadecimal token.
        // A narrow format also prevents accidental header/control injection.
        if token.len() != 64
            || !token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("网关设备凭据格式无效");
        }
        Ok(Self {
            endpoint: endpoint(origin, &device_id, fixture)?,
            device_id,
            token,
        })
    }
}

/// Implementations run only bounded trusted SQLite transactions. Each handler
/// must check its captured config revision and current principal ACL inside the
/// transaction immediately before applying effects. No network await in Store.
pub(crate) trait Backend: Send + Sync + 'static {
    fn current(&self) -> bool;
    fn handle(&self, request: GatewayRequest) -> GatewayResult;
    fn revoked(&self, principals: &[(String, u64)]) -> Result<Vec<String>>;
    fn connection(&self, connected: bool);
    /// Current worker/DB ownership or durable finalization is uncertain. This
    /// is NOT the same as a historical remote delivery OutcomeUnknown.
    fn uncertain(&self);
}

enum Control {
    Stop,
    Revoked(String),
}

async fn send<S>(socket: &mut S, frame: &GatewayDaemonFrame) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    let mut body = serde_json::to_string(frame)?;
    if body.len() > GATEWAY_MAX_FRAME_BYTES {
        // Escaping can expand otherwise bounded raw text. Preserve the live
        // link and original request identity; never report a committed mutation
        // as definitely rejected merely because its display could not fit.
        if let GatewayDaemonFrame::Reply { request_id, .. } = frame {
            body = serde_json::to_string(&GatewayDaemonFrame::Reply {
                request_id: request_id.clone(),
                result: GatewayResult::error(GatewayCode::OutcomeUnknown),
            })?;
        }
        if body.len() > GATEWAY_MAX_FRAME_BYTES {
            bail!("网关响应超出帧限制");
        }
    }
    timeout(IO_DEADLINE, socket.send(Message::Text(body.into())))
        .await
        .map_err(|_| anyhow::anyhow!("网关发送超时"))?
        .map_err(|_| anyhow::anyhow!("网关连接已断开"))
}

async fn database<B: Backend, T: Send + 'static>(
    backend: Arc<B>,
    f: impl FnOnce(&B) -> T + Send + 'static,
) -> Result<T> {
    let owned = backend.clone();
    tokio::task::spawn_blocking(move || f(owned.as_ref()))
        .await
        .map_err(|_| {
            backend.uncertain();
            anyhow::anyhow!("网关本地事务工作中断")
        })
}

/// One generation may reconnect, but never retains or replays its prior RPC.
/// Backend config revocation and caller shutdown independently stop the link.
pub(crate) async fn run(
    transport: Transport,
    backend: Arc<impl Backend>,
    mut stop: watch::Receiver<bool>,
) {
    let mut delay = 1;
    loop {
        if *stop.borrow()
            || !database(backend.clone(), |b| b.current())
                .await
                .unwrap_or(false)
        {
            break;
        }
        let _ = run_once(&transport, backend.clone(), stop.clone()).await;
        let _ = database(backend.clone(), |b| b.connection(false)).await;
        tokio::select! {
            changed = stop.changed() => { if changed.is_err() || *stop.borrow() { break; } },
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
        }
        delay = (delay * 2).min(30);
    }
}

async fn run_once(
    transport: &Transport,
    backend: Arc<impl Backend>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let mut request = transport
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| anyhow::anyhow!("网关地址无效"))?;
    let mut auth = HeaderValue::from_str(&format!("Bearer {}", transport.token.as_str()))
        .map_err(|_| anyhow::anyhow!("网关凭据格式无效"))?;
    auth.set_sensitive(true);
    request.headers_mut().insert("authorization", auth);
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(GATEWAY_MAX_FRAME_BYTES))
        .max_frame_size(Some(GATEWAY_MAX_FRAME_BYTES))
        .write_buffer_size(0)
        .max_write_buffer_size(GATEWAY_MAX_FRAME_BYTES * 2);
    let opened = tokio::select! {
        _ = stop.changed() => return Ok(()),
        result = timeout(IO_DEADLINE, connect_async_with_config(request, Some(ws_config), false)) => result,
    };
    let (mut socket, _) = opened
        .map_err(|_| anyhow::anyhow!("网关连接超时"))?
        .map_err(|_| anyhow::anyhow!("网关连接未通过认证或不可用"))?;
    if !database(backend.clone(), |b| b.current())
        .await
        .unwrap_or(false)
        || *stop.borrow()
    {
        return Ok(());
    }
    send(
        &mut socket,
        &GatewayDaemonFrame::Ready {
            protocol: GATEWAY_PROTOCOL,
            device_id: transport.device_id.clone(),
        },
    )
    .await?;
    database(backend.clone(), |b| b.connection(true)).await?;
    let (control_tx, mut control_rx) = mpsc::channel(16);
    let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, u64)>::new()));
    let watch_seen = seen.clone();
    let watch_backend = backend.clone();
    let (watch_stop_tx, mut watch_stop_rx) = watch::channel(false);
    // This independent awaited worker keeps slow SQLite reads off the socket
    // heartbeat path. Its own blocking read is retained through shutdown.
    let monitor = tokio::spawn(async move {
        loop {
            let peers = watch_seen.lock().unwrap().clone();
            let result = database(watch_backend.clone(), move |b| {
                if !b.current() {
                    return None;
                }
                b.revoked(&peers).ok()
            })
            .await
            .ok()
            .flatten();
            let Some(revoked) = result else {
                let _ = control_tx.send(Control::Stop).await;
                break;
            };
            for principal in revoked {
                if control_tx.send(Control::Revoked(principal)).await.is_err() {
                    return;
                }
            }
            tokio::select! {
                _ = watch_stop_rx.changed() => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    });
    let mut inflight = JoinSet::new();
    let mut request_ids = HashSet::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_rx = Instant::now();
    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                changed = stop.changed() => { if changed.is_err() || *stop.borrow() { break; } },
                control = control_rx.recv() => match control {
                    Some(Control::Revoked(principal_id)) => { send(&mut socket, &GatewayDaemonFrame::Revoked { principal_id }).await?; },
                    Some(Control::Stop) | None => break,
                },
                _ = heartbeat.tick() => {
                    if last_rx.elapsed() > IDLE { bail!("网关心跳已过期"); }
                    timeout(IO_DEADLINE, socket.send(Message::Ping(Vec::new().into()))).await
                        .map_err(|_| anyhow::anyhow!("网关心跳发送超时"))?
                        .map_err(|_| anyhow::anyhow!("网关心跳连接中断"))?;
                },
                result = inflight.join_next(), if !inflight.is_empty() => {
                    let (id, result) = result.ok_or_else(|| anyhow::anyhow!("网关事务结果缺失"))?
                        .map_err(|_| {backend.uncertain();anyhow::anyhow!("网关事务工作中断")})?;
                    request_ids.remove(&id);
                    send(&mut socket, &GatewayDaemonFrame::Reply { request_id: id, result }).await?;
                },
                incoming = socket.next() => {
                    let Some(Ok(message)) = incoming else { break; };
                    last_rx = Instant::now();
                    match message {
                        Message::Text(raw) => {
                            if raw.len() > GATEWAY_MAX_FRAME_BYTES { bail!("网关请求超过限制"); }
                            let GatewayServerFrame::Request { request } = super::wire::parse_gateway_server_frame(&raw)
                                .map_err(|_| anyhow::anyhow!("网关请求类型无效"))?;
                            if !canonical_id(&request.request_id) || !canonical_id(&request.principal_id) { bail!("网关请求标识无效"); }
                            if request_ids.contains(&request.request_id) { bail!("网关在途请求标识重复"); }
                            if inflight.len() >= MAX_INFLIGHT {
                                send(&mut socket, &GatewayDaemonFrame::Reply { request_id: request.request_id, result: GatewayResult::error(GatewayCode::Busy) }).await?;
                                continue;
                            }
                            if request.binding_revision > 0 {
                                let mut peers = seen.lock().unwrap();
                                if !peers.contains(&(request.principal_id.clone(), request.binding_revision)) {
                                    if peers.len() >= MAX_SEEN_PRINCIPALS { bail!("网关连接身份数量已达限制"); }
                                    peers.push((request.principal_id.clone(), request.binding_revision));
                                }
                            }
                            let id = request.request_id.clone();
                            request_ids.insert(id.clone());
                            let worker = backend.clone();
                            inflight.spawn(async move {
                                let result = database(worker, move |b| {
                                    if b.current() { b.handle(request) } else { GatewayResult::error(GatewayCode::Unauthorized) }
                                }).await.unwrap_or_else(|_| GatewayResult::error(GatewayCode::OutcomeUnknown));
                                (id, result)
                            });
                        },
                        Message::Ping(data) => { timeout(IO_DEADLINE, socket.send(Message::Pong(data))).await
                            .map_err(|_| anyhow::anyhow!("网关心跳响应超时"))?
                            .map_err(|_| anyhow::anyhow!("网关心跳响应中断"))?; },
                        Message::Pong(_) => {},
                        Message::Close(_) => break,
                        _ => bail!("网关只接受 JSON 文本帧"),
                    }
                }
            }
        }
        Ok(())
    }.await;
    // Do not abort DB effects on connection loss; finish the original receipt.
    drop(socket);
    while let Some(result) = inflight.join_next().await {
        if result.is_err() {
            backend.uncertain();
        }
    }
    let _ = watch_stop_tx.send(true);
    drop(control_rx);
    if monitor.await.is_err() {
        backend.uncertain();
    }
    outcome
}

fn canonical_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|value| value.to_string() == id)
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
