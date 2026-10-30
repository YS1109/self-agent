//! Online-only gateway. No task database, owner API proxy, or mutation replay.
pub mod directory;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path, Query, State,
    },
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use directory::{canonical_id, random_token, Device, Directory};
use futures_util::{SinkExt, StreamExt};
pub use harness_core::gateway as protocol;
use protocol::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use uuid::Uuid;
const SESSION_LIMIT: usize = 1024;
const UNPAIRED_SESSION_LIMIT: usize = 256;
const ANONYMOUS_TTL: Duration = Duration::from_secs(5 * 60);
const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const JS_MAX: u64 = 9_007_199_254_740_991;
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone)]
pub struct Config {
    pub origin: String,
    pub bind: SocketAddr,
    pub fixture: bool,
}
impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.bind.ip().is_loopback(), "网关只能绑定 loopback");
        let u = url::Url::parse(&self.origin)?;
        anyhow::ensure!(
            u.origin().ascii_serialization() == self.origin
                && u.username().is_empty()
                && u.password().is_none(),
            "必须配置精确公开 Origin，不含路径或认证信息"
        );
        if self.fixture {
            anyhow::ensure!(
                u.scheme() == "http"
                    && u.host_str().is_some_and(|h| h
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|i| i.is_loopback())),
                "夹具仅允许 numeric loopback HTTP"
            );
        } else {
            anyhow::ensure!(u.scheme() == "https", "生产网关要求 HTTPS Origin");
        }
        Ok(())
    }
    fn cookie_name(&self) -> &str {
        if self.fixture {
            "self-agent-fixture"
        } else {
            "__Host-self-agent"
        }
    }
    fn cookie(&self, token: &str, clear: bool) -> String {
        format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict{}; Max-Age={}",
            self.cookie_name(),
            token,
            if self.fixture { "" } else { "; Secure" },
            if clear { 0 } else { 43200 }
        )
    }
}
#[derive(Clone)]
struct Binding {
    device: String,
    principal: String,
    revision: u64,
    expires: u64,
    active: bool,
    permissions: GatewayPermissions,
    session_ids: Vec<String>,
}
#[derive(Clone)]
struct Session {
    csrf: String,
    expires: Instant,
    binding: Option<Binding>,
    streams: Arc<Semaphore>,
}
struct Link {
    id: String,
    ready: AtomicBool,
    device: Device,
    tx: mpsc::Sender<GatewayRequest>,
    pending: std::sync::Mutex<HashMap<String, Pending>>,
    retired: std::sync::Mutex<HashSet<String>>,
    slots: Arc<Semaphore>,
}
struct Inner {
    config: Config,
    directory: Directory,
    sessions: Mutex<HashMap<String, Session>>,
    links: Mutex<HashMap<String, Arc<Link>>>,
    global: Arc<Semaphore>,
    streams: Arc<Semaphore>,
}
#[derive(Clone)]
pub struct Gateway(Arc<Inner>);
impl Gateway {
    pub fn new(config: Config, directory: Directory) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self(Arc::new(Inner {
            config,
            directory,
            sessions: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            global: Arc::new(Semaphore::new(64)),
            streams: Arc::new(Semaphore::new(64)),
        })))
    }
    pub fn router(&self) -> Router {
        Router::new()
            .route("/gateway/api/session", get(session))
            .route("/gateway/api/pair", post(pair))
            .route("/gateway/api/pair-status", get(pair_status))
            .route("/gateway/api/rpc", post(rpc))
            .route("/gateway/api/logout", post(logout))
            .route("/gateway/api/devices", get(devices))
            .route("/gateway/api/events", get(events))
            .route("/gateway/device/{device_id}", get(device_ws))
            .layer(DefaultBodyLimit::max(GATEWAY_MAX_FRAME_BYTES))
            .layer(middleware::from_fn(no_store))
            .with_state(self.clone())
    }
    fn cookie(&self, h: &HeaderMap) -> Option<String> {
        let mut vals = h
            .get_all("cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|s| s.split(';'))
            .filter_map(|s| s.trim().split_once('='))
            .filter(|(k, _)| *k == self.0.config.cookie_name())
            .map(|(_, v)| v);
        let token = vals.next()?;
        if vals.next().is_some()
            || token.len() != 64
            || !token.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        Some(token.into())
    }
    fn origin(&self, h: &HeaderMap) -> bool {
        h.get_all("origin").iter().count() == 1
            && h.get("origin").and_then(|v| v.to_str().ok()) == Some(&self.0.config.origin)
    }
    fn same_site(&self, h: &HeaderMap) -> bool {
        h.get_all("sec-fetch-site").iter().count() == 1
            && h.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("same-origin")
            && h.get("origin").is_none_or(|_| self.origin(h))
    }
    async fn authenticate(
        &self,
        h: &HeaderMap,
        write: bool,
    ) -> Result<(String, Session), GatewayCode> {
        if !self.same_site(h) || (write && !self.origin(h)) {
            return Err(GatewayCode::Forbidden);
        }
        let token = self.cookie(h).ok_or(GatewayCode::Unauthorized)?;
        let session = self
            .0
            .sessions
            .lock()
            .await
            .get(&token)
            .filter(|s| s.expires > Instant::now())
            .cloned()
            .ok_or(GatewayCode::Unauthorized)?;
        if write
            && (h.get_all("x-csrf-token").iter().count() != 1
                || h.get("x-csrf-token").and_then(|v| v.to_str().ok()) != Some(&session.csrf))
        {
            return Err(GatewayCode::Forbidden);
        }
        if let Some(b) = &session.binding {
            self.check_device(&b.device, None).await?;
        }
        Ok((token, session))
    }
    // Unavailability is not proof of revocation. Fail the current operation
    // closed without destroying previously verified browser identity.
    async fn check_device(&self, id: &str, revision: Option<i64>) -> Result<(), GatewayCode> {
        match self.0.directory.verify(id.into(), None, revision).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                self.invalidate_device(id).await;
                Err(GatewayCode::Forbidden)
            }
            Err(_) => Err(GatewayCode::Busy),
        }
    }
    async fn invalidate_device(&self, id: &str) {
        self.0
            .sessions
            .lock()
            .await
            .retain(|_, s| s.binding.as_ref().is_none_or(|b| b.device != id));
    }
    async fn invalidate_principal(&self, id: &str, principal: &str) {
        self.0.sessions.lock().await.retain(|_, s| {
            s.binding
                .as_ref()
                .is_none_or(|b| b.device != id || b.principal != principal)
        });
        if let Some(link) = self.0.links.lock().await.get(id).cloned() {
            let mut pending = link.pending.lock().unwrap();
            let ids: Vec<_> = pending
                .iter()
                .filter(|(_, p)| p.principal == principal)
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                if let Some(p) = pending.remove(&id) {
                    if p.sent {
                        link.retire(id);
                    }
                    let _ = p.tx.send(GatewayResult::error(GatewayCode::OutcomeUnknown));
                }
            }
        }
    }
    async fn call(&self, token: &str, b: &Binding, rpc: GatewayRpc) -> GatewayResult {
        let Some(link) = self.0.links.lock().await.get(&b.device).cloned() else {
            return GatewayResult::error(GatewayCode::DeviceOffline);
        };
        if !link.ready.load(Ordering::Acquire) {
            return GatewayResult::error(GatewayCode::DeviceOffline);
        }
        if let Err(code) = self
            .check_device(&b.device, Some(link.device.revision))
            .await
        {
            return GatewayResult::error(code);
        }
        let Ok(_global) = self.0.global.clone().try_acquire_owned() else {
            return GatewayResult::error(GatewayCode::Busy);
        };
        let Ok(_device) = link.slots.clone().try_acquire_owned() else {
            return GatewayResult::error(GatewayCode::Busy);
        };
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        link.pending.lock().unwrap().insert(
            request_id.clone(),
            Pending {
                principal: b.principal.clone(),
                sent: false,
                tx,
            },
        );
        let _pending_guard = PendingGuard {
            link: link.clone(),
            id: request_id.clone(),
        };
        let req = GatewayRequest {
            request_id: request_id.clone(),
            principal_id: b.principal.clone(),
            binding_revision: b.revision,
            rpc,
        };
        // Fence enqueue with cookie invalidation: no await between authority check and enqueue.
        let sessions = self.0.sessions.lock().await;
        if !sessions.get(token).is_some_and(|s| {
            s.expires > Instant::now()
                && s.binding.as_ref().is_some_and(|current| {
                    current.device == b.device
                        && current.principal == b.principal
                        && current.revision == b.revision
                        && (!current.active || current.expires > now_ms())
                })
        }) {
            return GatewayResult::error(GatewayCode::Unauthorized);
        }
        if link.tx.try_send(req).is_err() {
            link.pending.lock().unwrap().remove(&request_id);
            return GatewayResult::error(GatewayCode::DeviceOffline);
        }
        drop(sessions);
        let result = match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            _ => GatewayResult::error(GatewayCode::OutcomeUnknown),
        };
        result
    }
    async fn pin(
        &self,
        token: &str,
        b: &Binding,
        result: &GatewayResult,
    ) -> Result<(), GatewayCode> {
        if result.code != GatewayCode::Ok {
            return Ok(());
        }
        let Some(d) = result.data.as_ref() else {
            return Err(GatewayCode::InvalidRequest);
        };
        if d.get("status").and_then(Value::as_str) != Some("active") {
            return Ok(());
        }
        let revision = d
            .get("binding_revision")
            .and_then(Value::as_u64)
            .filter(|r| *r > 0 && *r <= JS_MAX)
            .ok_or(GatewayCode::InvalidRequest)?;
        let expires = d
            .get("expires_at_ms")
            .and_then(Value::as_u64)
            .filter(|v| *v > now_ms() && *v <= JS_MAX)
            .ok_or(GatewayCode::InvalidRequest)?;
        if d.get("principal_id").and_then(Value::as_str) != Some(&b.principal)
            || d.get("permissions")
                .cloned()
                .and_then(|v| serde_json::from_value::<GatewayPermissions>(v).ok())
                .is_none()
            || !d
                .get("session_ids")
                .and_then(Value::as_array)
                .is_some_and(|v| {
                    v.len() <= 64 && v.iter().all(|v| v.as_str().is_some_and(canonical_id))
                })
        {
            return Err(GatewayCode::InvalidRequest);
        }
        let mut sessions = self.0.sessions.lock().await;
        let session = sessions.get_mut(token).ok_or(GatewayCode::Unauthorized)?;
        let current = session
            .binding
            .as_mut()
            .filter(|v| v.principal == b.principal && v.device == b.device)
            .ok_or(GatewayCode::Unauthorized)?;
        if current.active && current.revision != revision {
            sessions.remove(token);
            return Err(GatewayCode::Forbidden);
        }
        let first_activation = !current.active;
        current.active = true;
        current.revision = revision;
        current.expires = expires;
        current.permissions = serde_json::from_value(d["permissions"].clone())
            .map_err(|_| GatewayCode::InvalidRequest)?;
        current.session_ids = serde_json::from_value(d["session_ids"].clone())
            .map_err(|_| GatewayCode::InvalidRequest)?;
        if first_activation {
            session.expires = Instant::now() + SESSION_TTL;
        }
        Ok(())
    }
}
async fn no_store(req: axum::extract::Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    r.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    r.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    r.headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    r
}
fn response(result: GatewayResult) -> Response {
    Json(result).into_response()
}
fn error(code: GatewayCode) -> Response {
    response(GatewayResult::error(code))
}
async fn session(State(g): State<Gateway>, h: HeaderMap) -> Response {
    if !g.same_site(&h) {
        return error(GatewayCode::Forbidden);
    }
    if let Some(token) = g.cookie(&h) {
        let binding =
            g.0.sessions
                .lock()
                .await
                .get(&token)
                .and_then(|s| s.binding.clone());
        if let Some(b) = binding {
            match g.check_device(&b.device, None).await {
                Ok(()) | Err(GatewayCode::Forbidden) => {}
                Err(code) => return error(code),
            }
        }
    }

    let existing = g.cookie(&h);
    let mut sessions = g.0.sessions.lock().await;
    sessions.retain(|_, s| s.expires > Instant::now());
    let (token, fresh) = match existing.filter(|t| sessions.contains_key(t)) {
        Some(t) => (t, false),
        None => {
            let unpaired = sessions
                .values()
                .filter(|s| {
                    s.binding
                        .as_ref()
                        .is_none_or(|b| !b.active || b.expires <= now_ms())
                })
                .count();
            if sessions.len() >= SESSION_LIMIT || unpaired >= UNPAIRED_SESSION_LIMIT {
                return error(GatewayCode::Busy);
            }
            let t = random_token();
            sessions.insert(
                t.clone(),
                Session {
                    csrf: random_token(),
                    expires: Instant::now() + ANONYMOUS_TTL,
                    binding: None,
                    streams: Arc::new(Semaphore::new(2)),
                },
            );
            (t, true)
        }
    };
    let s = sessions.get(&token).unwrap();
    let data = json!({"mode":if g.0.config.fixture {"fixture"}else{"self_hosted"},"csrf":s.csrf,"paired":s.binding.as_ref().is_some_and(|b|b.active && b.expires>now_ms()),"device_id":s.binding.as_ref().map(|b|&b.device),"principal_id":s.binding.as_ref().map(|b|&b.principal),"binding_revision":s.binding.as_ref().map(|b|b.revision),"expires_at_ms":s.binding.as_ref().map(|b|b.expires),"permissions":s.binding.as_ref().filter(|b|b.active).map(|b|&b.permissions),"session_ids":s.binding.as_ref().filter(|b|b.active).map(|b|&b.session_ids)});
    let mut r = response(GatewayResult::ok(data));
    if fresh {
        r.headers_mut().insert(
            "set-cookie",
            HeaderValue::from_str(&g.0.config.cookie(&token, false)).unwrap(),
        );
    }
    r
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    device_id: String,
    code: String,
    label: String,
}
async fn pair(
    State(g): State<Gateway>,
    h: HeaderMap,
    body: Result<Json<Pair>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let (token, _) = match g.authenticate(&h, true).await {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    let Ok(Json(p)) = body else {
        return error(GatewayCode::InvalidRequest);
    };
    if !canonical_id(&p.device_id)
        || p.code.is_empty()
        || p.code.len() > 256
        || p.label.is_empty()
        || p.label.len() > 128
        || p.label.chars().any(char::is_control)
    {
        return error(GatewayCode::InvalidRequest);
    }
    // Allocate once BEFORE dispatch; ambiguous results retain this principal. No retry.
    let binding = Binding {
        device: p.device_id,
        principal: Uuid::new_v4().to_string(),
        revision: 0,
        expires: 0,
        active: false,
        permissions: GatewayPermissions::default(),
        session_ids: vec![],
    };
    {
        let mut sessions = g.0.sessions.lock().await;
        let Some(s) = sessions.get_mut(&token) else {
            return error(GatewayCode::Unauthorized);
        };
        if s.binding.is_some() {
            return error(GatewayCode::Conflict);
        }
        s.binding = Some(binding.clone());
        s.expires = Instant::now() + PAIRING_TTL;
    }
    let result = g
        .call(
            &token,
            &binding,
            GatewayRpc::PairClaim {
                code: p.code,
                label: p.label,
            },
        )
        .await;
    if let Err(e) = g.pin(&token, &binding, &result).await {
        return error(e);
    }
    response(result)
}
async fn pair_status(State(g): State<Gateway>, h: HeaderMap) -> Response {
    let (token, s) = match g.authenticate(&h, false).await {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    let Some(b) = s.binding else {
        return error(GatewayCode::Unauthorized);
    };
    let result = g.call(&token, &b, GatewayRpc::PairStatus).await;
    if let Err(e) = g.pin(&token, &b, &result).await {
        return error(e);
    }
    response(result)
}
fn active(s: &Session) -> Result<&Binding, GatewayCode> {
    s.binding
        .as_ref()
        .filter(|b| b.active && b.expires > now_ms())
        .ok_or(GatewayCode::Forbidden)
}
async fn rpc(
    State(g): State<Gateway>,
    h: HeaderMap,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let (token, s) = match g.authenticate(&h, true).await {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    let b = match active(&s) {
        Ok(b) => b,
        Err(e) => return error(e),
    };
    if h.get_all("content-type").iter().count() != 1
        || h.get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|s| s.split(';').next() != Some("application/json"))
    {
        return error(GatewayCode::InvalidRequest);
    }
    let Ok(body) = body else {
        return error(GatewayCode::InvalidRequest);
    };
    let Ok(_typed) = serde_json::from_slice::<GatewayRpc>(&body) else {
        return error(GatewayCode::InvalidRequest);
    };
    let raw: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Ok(rpc) = parse_gateway_rpc(raw) else {
        return error(GatewayCode::InvalidRequest);
    };
    if !valid_rpc(&rpc) || matches!(rpc, GatewayRpc::PairClaim { .. } | GatewayRpc::PairStatus) {
        return error(GatewayCode::InvalidRequest);
    }
    let result = g.call(&token, b, rpc).await;
    if !g.0.sessions.lock().await.contains_key(&token) {
        return error(GatewayCode::OutcomeUnknown);
    }
    response(result)
}
async fn logout(State(g): State<Gateway>, h: HeaderMap) -> Response {
    let (token, _) = match g.authenticate(&h, true).await {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    g.0.sessions.lock().await.remove(&token);
    let mut r = response(GatewayResult::ok(json!({"logged_out":true})));
    r.headers_mut().insert(
        "set-cookie",
        HeaderValue::from_str(&g.0.config.cookie("", true)).unwrap(),
    );
    r
}
async fn devices(State(g): State<Gateway>, h: HeaderMap) -> Response {
    let (_, s) = match g.authenticate(&h, false).await {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    let b = match active(&s) {
        Ok(b) => b,
        Err(e) => return error(e),
    };
    match g.0.directory.verify(b.device.clone(), None, None).await {
        Ok(Some(d)) => response(GatewayResult::ok(json!([d]))),
        Ok(None) => {
            g.invalidate_device(&b.device).await;
            error(GatewayCode::Forbidden)
        }
        Err(_) => error(GatewayCode::Busy),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsQuery {
    session_id: String,
    after: u64,
}
async fn events(
    State(g): State<Gateway>,
    h: HeaderMap,
    q: Result<Query<EventsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let (token, s) = match g.authenticate(&h, false).await {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    if active(&s).is_err() {
        return error(GatewayCode::Forbidden);
    }
    let Ok(Query(mut q)) = q else {
        return error(GatewayCode::InvalidRequest);
    };
    if !canonical_id(&q.session_id) || q.after > JS_MAX {
        return error(GatewayCode::InvalidRequest);
    }
    if let Some(last) = h.get("last-event-id") {
        let Ok(last) = last.to_str() else {
            return error(GatewayCode::InvalidRequest);
        };
        let Ok(cursor) = last.parse::<u64>() else {
            return error(GatewayCode::InvalidRequest);
        };
        if cursor > JS_MAX || cursor.to_string() != last || cursor < q.after {
            return error(GatewayCode::InvalidRequest);
        }
        q.after = cursor;
    }
    let Ok(global_slot) = g.0.streams.clone().try_acquire_owned() else {
        return error(GatewayCode::Busy);
    };
    let Ok(session_slot) = s.streams.clone().try_acquire_owned() else {
        return error(GatewayCode::Busy);
    };
    let (tx, mut rx) = mpsc::channel::<(GatewayResult, u64)>(16);
    let stream_g = g.clone();
    let stream_token = token.clone();
    tokio::spawn(async move {
        let mut cursor = q.after;
        loop {
            if tx.is_closed() {
                break;
            }
            let s =
                g.0.sessions
                    .lock()
                    .await
                    .get(&token)
                    .filter(|s| s.expires > Instant::now())
                    .cloned();
            let Some(s) = s else { break };
            let Ok(b) = active(&s) else { break };
            let result = g
                .call(
                    &token,
                    b,
                    GatewayRpc::Events {
                        session_id: q.session_id.clone(),
                        after: cursor,
                    },
                )
                .await;
            if !g.0.sessions.lock().await.contains_key(&token) {
                break;
            }
            let terminal = result.code != GatewayCode::Ok;
            if let Some(next) = result
                .data
                .as_ref()
                .and_then(|v| v.get("cursor"))
                .and_then(Value::as_u64)
                .filter(|v| *v >= cursor && *v <= JS_MAX)
            {
                cursor = next;
            }
            if !matches!(
                tokio::time::timeout(Duration::from_secs(5), tx.send((result, cursor))).await,
                Ok(Ok(()))
            ) {
                break;
            }
            if terminal {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let stream = async_stream::stream! {let _global_slot=global_slot;let _session_slot=session_slot;while let Some((result,cursor))=rx.recv().await{
        let live=stream_g.0.sessions.lock().await.get(&stream_token).is_some_and(|s|s.expires>Instant::now() && active(s).is_ok());if !live{break}
        yield Ok::<_,Infallible>(Event::default().event("gateway").data(serde_json::to_string(&result).unwrap()).id(cursor.to_string()));
    }};
    let mut r = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    r.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}
async fn device_ws(
    State(g): State<Gateway>,
    Path(id): Path<String>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if h.contains_key("origin")
        || h.get_all("authorization").iter().count() != 1
        || !canonical_id(&id)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(token) = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| s.len() == 64)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let device = match g.0.directory.verify(id, Some(token.into()), None).await {
        Ok(Some(device)) => device,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    // Reserve the identity before upgrade so parallel handshakes cannot replace a live link.
    let (tx, rx) = mpsc::channel(16);
    let link = Arc::new(Link {
        id: Uuid::new_v4().to_string(),
        ready: AtomicBool::new(false),
        device,
        tx,
        pending: std::sync::Mutex::new(HashMap::new()),
        retired: std::sync::Mutex::new(HashSet::new()),
        slots: Arc::new(Semaphore::new(8)),
    });
    {
        let mut links = g.0.links.lock().await;
        if links.contains_key(&link.device.device_id) {
            return StatusCode::CONFLICT.into_response();
        }
        if links.len() >= 1024 {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        links.insert(link.device.device_id.clone(), link.clone());
    }
    // A separate ready flag is represented by reserving all device permits until READY.
    let reserve = link.slots.clone().acquire_many_owned(8).await.unwrap();
    let failed_g = g.clone();
    let failed_link = link.clone();
    ws.on_failed_upgrade(move |_| {
        tokio::spawn(async move {
            let mut links = failed_g.0.links.lock().await;
            if links
                .get(&failed_link.device.device_id)
                .is_some_and(|v| v.id == failed_link.id)
            {
                links.remove(&failed_link.device.device_id);
            }
        });
    })
    .max_frame_size(GATEWAY_MAX_FRAME_BYTES)
    .max_message_size(GATEWAY_MAX_FRAME_BYTES)
    .on_upgrade(move |socket| async move { g.run_socket(socket, link, rx, reserve).await })
    .into_response()
}
impl Gateway {
    async fn run_socket(
        &self,
        socket: WebSocket,
        link: Arc<Link>,
        mut rx: mpsc::Receiver<GatewayRequest>,
        reserve: tokio::sync::OwnedSemaphorePermit,
    ) {
        let (mut sink, mut source) = socket.split();
        let ready = matches!(tokio::time::timeout(Duration::from_secs(5),source.next()).await,Ok(Some(Ok(Message::Text(t)))) if serde_json::from_str::<GatewayDaemonFrame>(&t).is_ok_and(|f|matches!(f,GatewayDaemonFrame::Ready{protocol,device_id} if protocol==GATEWAY_PROTOCOL&&device_id==link.device.device_id)));
        drop(reserve);
        if ready {
            link.ready.store(true, Ordering::Release);
            let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
            let mut guard = tokio::time::interval(Duration::from_secs(1));
            let mut last_seen = Instant::now();
            loop {
                tokio::select! {
                    _=guard.tick()=>{if !link.ready.load(Ordering::Acquire) || self.check_device(&link.device.device_id,Some(link.device.revision)).await.is_err(){break}}
                    _=heartbeat.tick()=>{if last_seen.elapsed()>Duration::from_secs(45){break}
                    if !send(&mut sink,Message::Ping(Vec::new().into())).await{break}}
                    req=rx.recv()=>{let Some(request)=req else{break};if self.check_device(&link.device.device_id,Some(link.device.revision)).await.is_err(){break}
                    {let mut pending=link.pending.lock().unwrap();let Some(entry)=pending.get_mut(&request.request_id)else{continue};entry.sent=true;}
                    let frame=GatewayServerFrame::Request{request};let Ok(text)=serde_json::to_string(&frame)else{break};if text.len()>GATEWAY_MAX_FRAME_BYTES||!send(&mut sink,Message::Text(text.into())).await{break}}
                    frame=source.next()=>{match frame {
                        Some(Ok(Message::Text(text)))=>{last_seen=Instant::now(); match serde_json::from_str::<GatewayDaemonFrame>(&text){
                            Ok(GatewayDaemonFrame::Reply{request_id,result})=>{let pending=link.pending.lock().unwrap().remove(&request_id);if let Some(p)=pending{let _=p.tx.send(result);}else if !link.retired.lock().unwrap().remove(&request_id){break}}
                            Ok(GatewayDaemonFrame::Revoked{principal_id}) if canonical_id(&principal_id)=>{self.invalidate_principal(&link.device.device_id,&principal_id).await;}
                            _=>break,
                        }}
                        Some(Ok(Message::Pong(_)))=>{last_seen=Instant::now();}
                        Some(Ok(Message::Ping(p)))=>{last_seen=Instant::now();if !send(&mut sink,Message::Pong(p)).await{break}}
                        _=>break,
                    }}
                }
            }
        }
        link.ready.store(false, Ordering::Release);
        {
            let mut links = self.0.links.lock().await;
            if links
                .get(&link.device.device_id)
                .is_some_and(|v| v.id == link.id)
            {
                links.remove(&link.device.device_id);
            }
        }
        for (_, tx) in link.pending.lock().unwrap().drain() {
            let _ = tx
                .tx
                .send(GatewayResult::error(GatewayCode::OutcomeUnknown));
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
    }
}
async fn send(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), sink.send(message)).await,
        Ok(Ok(()))
    )
}

fn valid_rpc(rpc: &GatewayRpc) -> bool {
    let text = |s: &str| !s.is_empty() && s.len() <= GATEWAY_MAX_INPUT_BYTES;
    match rpc {
        GatewayRpc::PairClaim { .. } | GatewayRpc::PairStatus | GatewayRpc::Sessions => true,
        GatewayRpc::Session { session_id } => canonical_id(session_id),
        GatewayRpc::Task { task_id }
        | GatewayRpc::Changes { task_id }
        | GatewayRpc::Artifacts { task_id } => canonical_id(task_id),
        GatewayRpc::Artifact { resource_id } => canonical_id(resource_id),
        GatewayRpc::Receipt { command_id } => canonical_id(command_id),
        GatewayRpc::Events { session_id, after } => canonical_id(session_id) && *after <= JS_MAX,
        GatewayRpc::Command {
            command_id,
            session_id,
            command,
        } => {
            canonical_id(command_id)
                && canonical_id(session_id)
                && match command {
                    GatewayCommand::Run { content } => text(content),
                    GatewayCommand::Cancel { task_id } => canonical_id(task_id),
                    GatewayCommand::Answer {
                        request_id, answer, ..
                    } => canonical_id(request_id) && text(answer),
                    GatewayCommand::Steer { task_id, content } => {
                        canonical_id(task_id) && text(content)
                    }
                    GatewayCommand::Continue { task_id, content } => {
                        canonical_id(task_id) && content.as_ref().is_none_or(|s| text(s))
                    }
                }
        }
    }
}

// Cancelled HTTP futures must release their pending slot even when no reply arrives.
struct PendingGuard {
    link: Arc<Link>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(pending) = self.link.pending.lock().unwrap().remove(&self.id) {
            if pending.sent {
                self.link.retire(self.id.clone());
            }
        }
    }
}

struct Pending {
    principal: String,
    sent: bool,
    tx: oneshot::Sender<GatewayResult>,
}

impl Link {
    fn retire(&self, id: String) {
        let mut retired = self.retired.lock().unwrap();
        if retired.len() >= 256 {
            // Never evict uncertainty and then confuse a late valid result
            // with a different request. Close this bounded generation instead.
            self.ready.store(false, Ordering::Release);
        } else {
            retired.insert(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn verifier_saturation_fails_closed_without_destroying_active_cookie() {
        let path = std::env::temp_dir().join(format!("gateway-unavailable-{}.db", Uuid::new_v4()));
        let directory = Directory::open(&path).unwrap();
        let (device, _) = directory.enroll("fixture").unwrap();
        let g = Gateway::new(
            Config {
                origin: "http://127.0.0.1:8788".into(),
                bind: "127.0.0.1:8788".parse().unwrap(),
                fixture: true,
            },
            directory.clone(),
        )
        .unwrap();
        let token = random_token();
        let principal = Uuid::new_v4().to_string();
        g.0.sessions.lock().await.insert(
            token.clone(),
            Session {
                csrf: random_token(),
                expires: Instant::now() + SESSION_TTL,
                binding: Some(Binding {
                    device: device.device_id.clone(),
                    principal: principal.clone(),
                    revision: 1,
                    expires: now_ms() + 60000,
                    active: true,
                    permissions: Default::default(),
                    session_ids: vec![],
                }),
                streams: Arc::new(Semaphore::new(2)),
            },
        );
        let mut headers = HeaderMap::new();
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        headers.insert(
            "cookie",
            format!("self-agent-fixture={token}").parse().unwrap(),
        );
        let held = directory.hold_all_verification_slots();
        assert!(matches!(
            g.authenticate(&headers, false).await,
            Err(GatewayCode::Busy)
        ));
        let response = session(State(g.clone()), headers.clone()).await;
        assert!(!response.headers().contains_key("set-cookie"));
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["code"],
            "busy"
        );
        assert!(g.0.sessions.lock().await.contains_key(&token));
        drop(held);
        let (_, restored) = g.authenticate(&headers, false).await.unwrap();
        assert_eq!(restored.binding.unwrap().principal, principal);
        directory.revoke(&device.device_id).unwrap();
        assert!(matches!(
            g.authenticate(&headers, false).await,
            Err(GatewayCode::Forbidden)
        ));
        assert!(!g.0.sessions.lock().await.contains_key(&token));
        drop(g);
        drop(directory);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn production_cookie_cannot_be_downgraded() {
        let config = Config {
            origin: "https://agent.example.invalid".into(),
            bind: "127.0.0.1:8788".parse().unwrap(),
            fixture: false,
        };
        config.validate().unwrap();
        let cookie = config.cookie("fixture", false);
        for expected in [
            "__Host-self-agent=",
            "HttpOnly",
            "SameSite=Strict",
            "; Secure",
            "Path=/",
        ] {
            assert!(cookie.contains(expected))
        }
        assert!(!cookie.contains("Domain"));
    }
    #[tokio::test]
    async fn cancelled_rpc_guard_releases_pending_memory() {
        let (tx, _rx) = mpsc::channel(16);
        let link = Arc::new(Link {
            id: Uuid::new_v4().to_string(),
            ready: AtomicBool::new(true),
            device: Device {
                device_id: Uuid::new_v4().to_string(),
                label: "fixture".into(),
                revision: 0,
                revoked: false,
            },
            tx,
            pending: std::sync::Mutex::new(HashMap::new()),
            retired: std::sync::Mutex::new(HashSet::new()),
            slots: Arc::new(Semaphore::new(8)),
        });
        let (reply, _) = oneshot::channel();
        let id = Uuid::new_v4().to_string();
        link.pending.lock().unwrap().insert(
            id.clone(),
            Pending {
                principal: "fixture".into(),
                sent: false,
                tx: reply,
            },
        );
        let guard = PendingGuard {
            link: link.clone(),
            id,
        };
        assert_eq!(link.pending.lock().unwrap().len(), 1);
        drop(guard);
        assert!(link.pending.lock().unwrap().is_empty());
        assert!(link.retired.lock().unwrap().is_empty());
        assert_eq!(link.tx.capacity(), 16);
        for _ in 0..256 {
            link.retire(Uuid::new_v4().to_string());
        }
        assert!(link.ready.load(Ordering::Acquire));
        link.retire(Uuid::new_v4().to_string());
        assert!(!link.ready.load(Ordering::Acquire));
        assert_eq!(link.retired.lock().unwrap().len(), 256);
    }
}
