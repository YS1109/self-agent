//! QQ official transport only. Durable inbox/outbox and configuration revision
//! ownership belong to the caller; no request here is a proactive message.
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Client, Url,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::IpAddr,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};
use zeroize::Zeroizing;

const LIMIT: usize = 256 * 1024;
const DEADLINE: Duration = Duration::from_secs(15);
const ACK_DEADLINE: Duration = Duration::from_secs(15);

/// Construct once per adapter configuration revision. Never derive Debug or
/// Serialize: credentials and ephemeral access tokens are private process data.
pub(crate) struct Credentials {
    app_id: String,
    secret: Zeroizing<Vec<u8>>,
}
impl Credentials {
    pub fn new(app_id: String, secret: Zeroizing<Vec<u8>>) -> Result<Self> {
        if !identifier(&app_id)
            || secret.is_empty()
            || secret.len() > 4096
            || std::str::from_utf8(&secret).is_err()
        {
            bail!("QQ 应用凭证格式无效");
        }
        Ok(Self { app_id, secret })
    }
}
struct Token {
    bytes: Zeroizing<Vec<u8>>,
    refresh_at: Instant,
}
pub(crate) struct Transport {
    credentials: Arc<Credentials>,
    client: Client,
    base: Url,
    fixture: bool,
    token: Arc<tokio::sync::Mutex<Option<Token>>>,
}
#[derive(Clone, Copy)]
pub(crate) enum ReplyKind {
    User,
    Group,
}
pub(crate) struct Reply<'a> {
    pub kind: ReplyKind,
    pub openid: &'a str,
    pub msg_id: &'a str,
    pub msg_seq: u32,
    pub content: &'a str,
    pub expires_at: std::time::SystemTime,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    Sent {
        id: String,
    },
    /// Explicit server rejection. Only 429 is automatically retryable; caller
    /// must retain original msg_id/msg_seq and passive reply expiry.
    Rejected {
        status: u16,
        retry_after_seconds: Option<u64>,
    },
    /// Delivery may have occurred. Never blindly resend this outcome.
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    pub session_id: String,
    pub seq: u64,
}
#[derive(Debug)]
pub(crate) enum GatewayItem {
    Dispatch {
        payload: Value,
        checkpoint: Checkpoint,
    },
    Reconnect,
    InvalidSession,
}
struct Delivery {
    item: GatewayItem,
    ack: Option<oneshot::Sender<Checkpoint>>,
}
pub(crate) struct Gateway {
    incoming: mpsc::Receiver<Delivery>,
    pending: Option<(Checkpoint, oneshot::Sender<Checkpoint>)>,
    worker: tokio::task::JoinHandle<()>,
    terminal: Arc<AtomicU8>,
}
impl Drop for Gateway {
    fn drop(&mut self) {
        if !self.worker.is_finished() {
            self.worker.abort();
        }
    }
}
impl Gateway {
    pub async fn shutdown(mut self) -> Result<()> {
        // Closing the receiver wakes sender.closed() in the heartbeat loop.
        // During bounded handshake/write it finishes that owned future first.
        self.incoming.close();
        self.pending.take();
        (&mut self.worker)
            .await
            .map_err(|_| anyhow::anyhow!("QQ 网关清理工作中断"))
    }
    pub async fn next(&mut self) -> Result<GatewayItem> {
        if self.pending.is_some() {
            bail!("QQ 事件必须持久化并确认后才能继续读取");
        }
        let Some(delivery) = self.incoming.recv().await else {
            return Ok(if self.terminal.load(Ordering::Acquire) == 2 {
                GatewayItem::InvalidSession
            } else {
                GatewayItem::Reconnect
            });
        };
        if let GatewayItem::Dispatch { checkpoint, .. } = &delivery.item {
            self.pending = Some((
                checkpoint.clone(),
                delivery.ack.context("QQ 事件缺少提交确认")?,
            ));
        }
        Ok(delivery.item)
    }
    /// Call ONLY after durable inbox transaction (including this checkpoint)
    /// commits. A stale or fabricated acknowledgement is rejected.
    pub fn commit_ack(&mut self, checkpoint: &Checkpoint) -> Result<()> {
        if self.pending.as_ref().map(|(expected, _)| expected) != Some(checkpoint) {
            bail!("QQ 事件提交检查点不匹配");
        }
        let (_, sender) = self.pending.take().unwrap();
        sender
            .send(checkpoint.clone())
            .map_err(|_| anyhow::anyhow!("QQ 网关已断开；已提交事件将按持久化检查点恢复"))
    }
}
impl Transport {
    pub fn production(credentials: Credentials) -> Result<Self> {
        Self::new(credentials, Url::parse("https://api.bot.qq.com/")?, false)
    }
    /// Explicit fixture-only construction. No hostname resolution, credentials
    /// forwarding to remote origins, proxy use, or redirect following.
    pub fn fixture(credentials: Credentials, origin: &str) -> Result<Self> {
        let url = Url::parse(origin).map_err(|_| anyhow::anyhow!("QQ 本地夹具地址无效"))?;
        if url.scheme() != "http"
            || !url
                .host_str()
                .and_then(|s| s.trim_matches(['[', ']']).parse::<IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback())
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            bail!("QQ 夹具仅允许显式回环 HTTP 根地址");
        }
        Self::new(credentials, url, true)
    }
    fn new(credentials: Credentials, base: Url, fixture: bool) -> Result<Self> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(DEADLINE)
            .build()
            .map_err(|_| anyhow::anyhow!("无法创建 QQ 传输客户端"))?;
        Ok(Self {
            credentials: Arc::new(credentials),
            client,
            base,
            fixture,
            token: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }
    pub(crate) fn fork(&self) -> Self {
        Self {
            credentials: self.credentials.clone(),
            client: self.client.clone(),
            base: self.base.clone(),
            fixture: self.fixture,
            token: self.token.clone(),
        }
    }
    async fn auth(&mut self, force: bool) -> Result<(HeaderValue, Instant)> {
        let mut cached = self.token.lock().await;
        if force
            || cached
                .as_ref()
                .is_none_or(|token| Instant::now() >= token.refresh_at)
        {
            let secret = std::str::from_utf8(&self.credentials.secret)
                .map_err(|_| anyhow::anyhow!("QQ 凭证编码无效"))?;
            #[derive(Serialize)]
            struct AuthRequest<'a> {
                #[serde(rename = "appId")]
                app_id: &'a str,
                #[serde(rename = "clientSecret")]
                secret: &'a str,
            }
            let body = Zeroizing::new(serde_json::to_vec(&AuthRequest {
                app_id: &self.credentials.app_id,
                secret,
            })?);
            let response = self
                .client
                .post(self.base.join("app/getAppAccessToken")?)
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_vec())
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("QQ 访问凭证请求未完成"))?;
            if !response.status().is_success() {
                bail!("QQ 访问凭证请求被拒绝");
            }
            let body = bounded_body(response).await?;
            #[derive(Deserialize)]
            struct Auth {
                access_token: String,
                expires_in: Value,
            }
            let auth: Auth = serde_json::from_slice(&body)
                .map_err(|_| anyhow::anyhow!("QQ 访问凭证响应无效"))?;
            let bytes = Zeroizing::new(auth.access_token.into_bytes());
            let expires = auth
                .expires_in
                .as_u64()
                .or_else(|| auth.expires_in.as_str().and_then(|v| v.parse().ok()))
                .filter(|v| (1..=7200).contains(v))
                .context("QQ 访问凭证有效期无效")?;
            if bytes.is_empty() || bytes.len() > 8192 || bytes.iter().any(|b| !b.is_ascii_graphic())
            {
                bail!("QQ 访问凭证格式无效");
            }
            *cached = Some(Token {
                bytes,
                refresh_at: Instant::now()
                    + Duration::from_millis(expires * 1000 - (expires * 500).min(60_000)),
            });
        }
        let token = &cached.as_ref().unwrap().bytes;
        let mut bytes = Zeroizing::new(b"QQBot ".to_vec());
        bytes.extend_from_slice(token);
        let mut value =
            HeaderValue::from_bytes(&bytes).map_err(|_| anyhow::anyhow!("QQ 访问凭证格式无效"))?;
        value.set_sensitive(true);
        Ok((value, cached.as_ref().unwrap().refresh_at))
    }
    #[cfg(test)]
    pub async fn send_passive(&mut self, reply: Reply<'_>) -> Result<SendOutcome> {
        self.send_passive_with_control(reply, &|| async { true })
            .await
    }
    /// Recheck revision/binding/expiry authority across token acquisition and
    /// explicit 401 refresh. Once POST starts, revocation cannot retract it;
    /// interruption must retain the caller's durable unknown-attempt state.
    pub async fn send_passive_with_control<F, Fut>(
        &mut self,
        reply: Reply<'_>,
        allowed: &F,
    ) -> Result<SendOutcome>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        if !identifier(reply.openid)
            || !opaque_id(reply.msg_id)
            || reply.msg_seq == 0
            || reply.msg_seq
                > match reply.kind {
                    ReplyKind::User => 4,
                    ReplyKind::Group => 5,
                }
            || reply.content.trim().is_empty()
            || reply.content.len() > 4000
            || std::time::SystemTime::now() >= reply.expires_at
        {
            bail!("QQ 被动回复参数无效");
        }
        let route = match reply.kind {
            ReplyKind::User => "users",
            ReplyKind::Group => "groups",
        };
        let url = self
            .base
            .join(&format!("v2/{route}/{}/messages", reply.openid))?;
        let body = serde_json::to_vec(
            &json!({"msg_type":0,"content":reply.content,"msg_id":reply.msg_id,"msg_seq":reply.msg_seq}),
        )?;
        for attempt in 0..2 {
            if !allowed().await {
                bail!("QQ 回复发送权限已失效，未发送");
            }
            let (auth, _) = self.auth(attempt == 1).await?;
            if std::time::SystemTime::now() >= reply.expires_at {
                bail!("QQ 被动回复窗口已过期，未发送");
            }
            if !allowed().await {
                bail!("QQ 回复发送权限已失效，未发送");
            }
            if std::time::SystemTime::now() >= reply.expires_at {
                bail!("QQ 被动回复窗口已过期，未发送");
            }
            let response = match self
                .client
                .post(url.clone())
                .header(AUTHORIZATION, auth)
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await
            {
                Ok(value) => value,
                Err(_) => return Ok(SendOutcome::Unknown),
            };
            let status = response.status().as_u16();
            if status == 401 && attempt == 0 {
                continue;
            }
            if (500..600).contains(&status) {
                return Ok(SendOutcome::Unknown);
            }
            if !(200..300).contains(&status) {
                let retry_after_seconds = if status == 429 {
                    response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|v| v.clamp(1, 3600))
                } else {
                    None
                };
                return Ok(SendOutcome::Rejected {
                    status,
                    retry_after_seconds,
                });
            }
            let bytes = match bounded_body(response).await {
                Ok(value) => value,
                Err(_) => return Ok(SendOutcome::Unknown),
            };
            let id = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["id"].as_str().map(str::to_owned))
                .filter(|id| opaque_id(id));
            return Ok(id.map_or(SendOutcome::Unknown, |id| SendOutcome::Sent { id }));
        }
        unreachable!()
    }
    pub async fn connect(&mut self, checkpoint: Option<Checkpoint>) -> Result<Gateway> {
        if checkpoint
            .as_ref()
            .is_some_and(|v| !identifier(&v.session_id))
        {
            bail!("QQ 恢复检查点无效");
        }
        let (mut auth, mut refresh_at) = self.auth(false).await?;
        let mut response = self
            .client
            .get(self.base.join("gateway")?)
            .header(AUTHORIZATION, auth.clone())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("QQ 网关发现失败"))?;
        if response.status().as_u16() == 401 {
            (auth, refresh_at) = self.auth(true).await?;
            response = self
                .client
                .get(self.base.join("gateway")?)
                .header(AUTHORIZATION, auth.clone())
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("QQ 网关发现失败"))?;
        }
        if !response.status().is_success() {
            bail!("QQ 网关发现被拒绝");
        }
        let bytes = bounded_body(response).await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("QQ 网关发现响应无效"))?;
        let url = Url::parse(value["url"].as_str().context("QQ 网关地址缺失")?)
            .map_err(|_| anyhow::anyhow!("QQ 网关地址无效"))?;
        self.validate_gateway(&url)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(LIMIT))
            .max_frame_size(Some(LIMIT));
        let (socket, _) = timeout(
            DEADLINE,
            connect_async_with_config(url.as_str(), Some(config), true),
        )
        .await
        .map_err(|_| anyhow::anyhow!("QQ 网关连接超时"))?
        .map_err(|_| anyhow::anyhow!("QQ 网关连接失败"))?;
        let token = Zeroizing::new(auth.as_bytes().to_vec());
        let (sender, incoming) = mpsc::channel(1);
        let terminal = Arc::new(AtomicU8::new(0));
        let actor_terminal = terminal.clone();
        let worker = tokio::spawn(async move {
            if gateway_loop(
                socket,
                token,
                checkpoint,
                &sender,
                refresh_at,
                &actor_terminal,
            )
            .await
            .is_err()
            {
                let _ = sender.try_send(Delivery {
                    item: GatewayItem::Reconnect,
                    ack: None,
                });
            }
        });
        Ok(Gateway {
            incoming,
            pending: None,
            worker,
            terminal,
        })
    }
    fn validate_gateway(&self, url: &Url) -> Result<()> {
        let allowed = if self.fixture {
            url.scheme() == "ws"
                && url.host() == self.base.host()
                && url.port_or_known_default() == self.base.port_or_known_default()
        } else {
            url.scheme() == "wss"
                && url.host_str() == Some("api.bot.qq.com")
                && url.port_or_known_default() == Some(443)
        };
        if !allowed
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
            || !matches!(url.path(), "/websocket" | "/websocket/")
        {
            bail!("QQ 网关地址不在允许的来源范围");
        }
        Ok(())
    }
}
fn opaque_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.bytes().all(|b| b.is_ascii_graphic())
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || b"_-".contains(&v))
}
async fn bounded_body(response: reqwest::Response) -> Result<Zeroizing<Vec<u8>>> {
    if response.content_length().is_some_and(|v| v > LIMIT as u64) {
        bail!("QQ 响应超过大小限制");
    }
    let mut stream = response.bytes_stream();
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("QQ 响应读取失败"))?;
        if body.len() + chunk.len() > LIMIT {
            bail!("QQ 响应超过大小限制");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
/// Caller-controlled retry schedule; never used to resend Unknown replies.
pub(crate) fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(6))
}
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn send_frame(socket: &mut Socket, value: Value) -> Result<()> {
    timeout(
        DEADLINE,
        socket.send(Message::Text(value.to_string().into())),
    )
    .await
    .map_err(|_| anyhow::anyhow!("QQ 网关发送超时"))?
    .map_err(|_| anyhow::anyhow!("QQ 网关发送失败"))
}
async fn gateway_loop(
    mut socket: Socket,
    token: Zeroizing<Vec<u8>>,
    mut committed: Option<Checkpoint>,
    sender: &mpsc::Sender<Delivery>,
    refresh_at: Instant,
    terminal: &AtomicU8,
) -> Result<()> {
    let hello = timeout(DEADLINE, socket.next())
        .await
        .map_err(|_| anyhow::anyhow!("QQ 网关握手超时"))?
        .context("QQ 网关握手中断")?
        .map_err(|_| anyhow::anyhow!("QQ 网关握手失败"))?;
    let Message::Text(text) = hello else {
        bail!("QQ 网关首帧无效")
    };
    let hello: Value =
        serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("QQ 网关握手格式无效"))?;
    if hello["op"] != 10 {
        bail!("QQ 网关缺少 Hello")
    };
    let interval = hello["d"]["heartbeat_interval"]
        .as_u64()
        .filter(|v| (100..=120000).contains(v))
        .context("QQ 网关心跳间隔无效")?;
    let token = std::str::from_utf8(&token).map_err(|_| anyhow::anyhow!("QQ 凭证格式无效"))?;
    let identify = if let Some(checkpoint) = &committed {
        json!({"op":6,"d":{"token":token,"session_id":checkpoint.session_id,"seq":checkpoint.seq}})
    } else {
        json!({"op":2,"d":{"token":token,"intents":1u64<<25,"shard":[0,1]}})
    };
    send_frame(&mut socket, identify).await?;
    // QQ replays missed dispatches BEFORE sending RESUMED. A resumed
    // connection therefore cannot require RESUMED to be its first dispatch.
    let resuming = committed.is_some();
    let ready_deadline = Instant::now() + DEADLINE;
    let mut ready = false;
    let mut heartbeat = tokio::time::interval_at(
        Instant::now() + Duration::from_millis(interval),
        Duration::from_millis(interval),
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut awaiting_heartbeat = false;
    let mut pending: Option<(Checkpoint, oneshot::Receiver<Checkpoint>, Instant)> = None;
    let mut backlog = std::collections::VecDeque::<(Value, usize)>::new();
    let mut backlog_bytes = 0usize;
    loop {
        tokio::select! {
            _=sender.closed()=>return Ok(()),
            _=tokio::time::sleep_until(ready_deadline), if !ready=>bail!("QQ 网关认证就绪超时"),
            _=tokio::time::sleep_until(refresh_at)=>bail!("QQ 凭证需要刷新，使用已提交检查点重连"),
            _=heartbeat.tick()=>{
                if awaiting_heartbeat {bail!("QQ 网关心跳确认超时");}
                send_frame(&mut socket,json!({"op":1,"d":committed.as_ref().map(|v|v.seq)})).await?;
                awaiting_heartbeat=true;
            },
            ack=async {match pending.as_mut() { Some((_,receiver,deadline))=>timeout_at_ack(*deadline,receiver).await,None=>std::future::pending().await }}=>{
                let ack=ack?;
                let (expected,_,_)=pending.take().context("QQ 提交确认状态无效")?;
                if ack!=expected {bail!("QQ 提交确认顺序无效");}
                committed=Some(ack);
                if let Some((payload,bytes))=backlog.pop_front() {
                    backlog_bytes-=bytes;
                    pending=Some(deliver(payload,&committed,sender)?);
                }
            },
            frame=socket.next()=>{
                let frame=frame.context("QQ 网关已关闭")?.map_err(|_|anyhow::anyhow!("QQ 网关帧读取失败"))?;
                match frame {
                    Message::Ping(bytes)=>{timeout(DEADLINE,socket.send(Message::Pong(bytes))).await.map_err(|_|anyhow::anyhow!("QQ 网关 Pong 超时"))?.map_err(|_|anyhow::anyhow!("QQ 网关 Pong 失败"))?;},
                    Message::Pong(_)=>{},
                    Message::Text(text)=>{
                        let payload:Value=serde_json::from_str(&text).map_err(|_|anyhow::anyhow!("QQ 网关事件格式无效"))?;
                        match payload["op"].as_u64() {
                            Some(11)=>awaiting_heartbeat=false,
                            Some(1)=>{send_frame(&mut socket,json!({"op":1,"d":committed.as_ref().map(|v|v.seq)})).await?;awaiting_heartbeat=true;},
                            Some(7)=>{let _=sender.try_send(Delivery{item:GatewayItem::Reconnect,ack:None});return Ok(());},
                            Some(9)=>{terminal.store(2,Ordering::Release);let _=sender.try_send(Delivery{item:GatewayItem::InvalidSession,ack:None});return Ok(());},
                            Some(0)=>{
                                if (resuming || ready) && payload["t"]=="READY" {
                                    bail!("QQ 网关不能在恢复中隐式切换会话");
                                }
                                if !ready {
                                    if !resuming && payload["t"]!="READY" {bail!("QQ 网关认证就绪事件无效");}
                                    pending=Some(deliver(payload,&committed,sender)?);
                                    ready=true;
                                    continue;
                                }
                                if pending.is_some() {
                                    if backlog.len()>=16 || backlog_bytes+text.len()>LIMIT {bail!("QQ 耐久事件队列已满，需从已提交检查点恢复");}
                                    backlog_bytes+=text.len();
                                    backlog.push_back((payload,text.len()));
                                } else { pending=Some(deliver(payload,&committed,sender)?); }
                            },
                            _=>bail!("QQ 网关操作码无效"),
                        }
                    },
                    _=>bail!("QQ 网关连接关闭或帧类型不受支持"),
                }
            }
        }
    }
}
fn deliver(
    payload: Value,
    committed: &Option<Checkpoint>,
    sender: &mpsc::Sender<Delivery>,
) -> Result<(Checkpoint, oneshot::Receiver<Checkpoint>, Instant)> {
    let seq = payload["s"].as_u64().context("QQ 事件缺少序列号")?;
    let session = if payload["t"] == "READY" {
        payload["d"]["session_id"]
            .as_str()
            .filter(|v| identifier(v))
            .context("QQ Ready 会话标识无效")?
            .to_owned()
    } else {
        committed
            .as_ref()
            .context("QQ 事件缺少已提交会话")?
            .session_id
            .clone()
    };
    if committed
        .as_ref()
        .is_some_and(|v| v.session_id == session && seq < v.seq)
    {
        bail!("QQ 事件序列号倒退");
    }
    let checkpoint = Checkpoint {
        session_id: session,
        seq,
    };
    let (ack, receiver) = oneshot::channel();
    sender
        .try_send(Delivery {
            item: GatewayItem::Dispatch {
                payload,
                checkpoint: checkpoint.clone(),
            },
            ack: Some(ack),
        })
        .map_err(|_| anyhow::anyhow!("QQ 事件接收端已关闭或未消费"))?;
    Ok((checkpoint, receiver, Instant::now() + ACK_DEADLINE))
}
async fn timeout_at_ack(
    deadline: Instant,
    receiver: &mut oneshot::Receiver<Checkpoint>,
) -> Result<Checkpoint> {
    tokio::time::timeout_at(deadline, receiver)
        .await
        .map_err(|_| anyhow::anyhow!("QQ 事件耐久提交确认超时"))?
        .map_err(|_| anyhow::anyhow!("QQ 事件未提交，连接结束"))
}
#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
