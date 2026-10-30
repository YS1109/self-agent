//! Token-private event transport and bounded renderer delivery. No Tauri dependency:
//! these lifecycle, parser and recovery rules can also be tested headlessly.
use harness_core::DaemonInfo;
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

const MAX_FRAME: usize = 512 * 1024;
const MAX_SNAPSHOT: usize = 12 * 1024 * 1024;
const MAX_PENDING: usize = 64;
const MAX_PENDING_KIB: usize = 32 * 1024;

#[derive(Default)]
pub struct Subscriptions {
    inner: Mutex<Registry>,
    deliveries: Arc<AtomicU64>,
}
#[derive(Default)]
struct Registry {
    next: u64,
    entries: HashMap<(String, String), Entry>,
}
struct Entry {
    generation: u64,
    cancel: watch::Sender<()>,
    credits: Arc<Credits>,
}
pub struct Subscription {
    pub generation: u64,
    pub cancel: watch::Receiver<()>,
    pub credits: Arc<Credits>,
}

impl Subscriptions {
    pub fn start(&self, window: &str, id: &str) -> Result<Subscription, String> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("事件订阅标识无效".into());
        }
        let mut registry = self.inner.lock().unwrap();
        let key = (window.to_owned(), id.to_owned());
        if !registry.entries.contains_key(&key)
            && (registry.entries.len() >= 128
                || registry
                    .entries
                    .keys()
                    .filter(|(label, _)| label == window)
                    .count()
                    >= 16)
        {
            return Err("事件订阅数量超过限制".into());
        }
        registry.next = registry.next.checked_add(1).ok_or("事件订阅序号已耗尽")?;
        let generation = registry.next;
        let (cancel, receiver) = watch::channel(());
        let credits = Arc::new(Credits::with_counter(self.deliveries.clone()));
        if let Some(old) = registry.entries.insert(
            key,
            Entry {
                generation,
                cancel,
                credits: credits.clone(),
            },
        ) {
            let _ = old.cancel.send(());
        }
        Ok(Subscription {
            generation,
            cancel: receiver,
            credits,
        })
    }

    pub fn send(
        &self,
        window: &str,
        id: &str,
        generation: u64,
        send: impl FnOnce() -> bool,
    ) -> bool {
        // Keep the lock through the non-blocking IPC enqueue: stop/supersede cannot
        // race between checking ownership and emitting a late message.
        let registry = self.inner.lock().unwrap();
        registry
            .entries
            .get(&(window.to_owned(), id.to_owned()))
            .is_some_and(|entry| entry.generation == generation)
            && send()
    }

    pub fn acknowledge(&self, window: &str, id: &str, delivery_id: u64) {
        if let Some(entry) = self
            .inner
            .lock()
            .unwrap()
            .entries
            .get(&(window.to_owned(), id.to_owned()))
        {
            entry.credits.acknowledge(delivery_id);
        }
    }

    pub fn stop(&self, window: &str, id: &str) {
        if let Some(entry) = self
            .inner
            .lock()
            .unwrap()
            .entries
            .remove(&(window.to_owned(), id.to_owned()))
        {
            let _ = entry.cancel.send(());
        }
    }

    pub fn stop_window(&self, window: &str) {
        self.inner
            .lock()
            .unwrap()
            .entries
            .retain(|(label, _), entry| {
                if label == window {
                    let _ = entry.cancel.send(());
                    false
                } else {
                    true
                }
            });
    }

    pub fn finish(&self, window: &str, id: &str, generation: u64) {
        let mut registry = self.inner.lock().unwrap();
        let key = (window.to_owned(), id.to_owned());
        if registry
            .entries
            .get(&key)
            .is_some_and(|entry| entry.generation == generation)
        {
            registry.entries.remove(&key);
        }
    }
}

pub struct Credits {
    ids: Arc<AtomicU64>,
    semaphore: Arc<Semaphore>,
    byte_budget: Arc<Semaphore>,
    ledger: Mutex<(u64, VecDeque<DeliveryCredit>)>,
}
struct DeliveryCredit {
    id: u64,
    _count: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}
impl Credits {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_counter(Arc::new(AtomicU64::new(0)))
    }
    fn with_counter(ids: Arc<AtomicU64>) -> Self {
        Self {
            ids,
            semaphore: Arc::new(Semaphore::new(MAX_PENDING)),
            byte_budget: Arc::new(Semaphore::new(MAX_PENDING_KIB)),
            ledger: Mutex::new((0, VecDeque::new())),
        }
    }
    async fn reserve(&self, size: usize) -> Result<u64, String> {
        let units = size.div_ceil(1024).max(1);
        if units > MAX_PENDING_KIB {
            return Err("前端事件超过投递预算".into());
        }
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "事件订阅已关闭")?;
        let bytes = self
            .byte_budget
            .clone()
            .acquire_many_owned(units as u32)
            .await
            .map_err(|_| "事件订阅已关闭")?;
        let mut ledger = self.ledger.lock().unwrap();
        let id = self
            .ids
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| "事件投递序号已耗尽")?
            + 1;
        ledger.0 = id;
        ledger.1.push_back(DeliveryCredit {
            id,
            _count: permit,
            _bytes: bytes,
        });
        Ok(id)
    }
    fn try_reserve(&self) -> Option<u64> {
        let permit = self.semaphore.clone().try_acquire_owned().ok()?;
        let bytes = self.byte_budget.clone().try_acquire_owned().ok()?;
        let mut ledger = self.ledger.lock().unwrap();
        let id = self
            .ids
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()?
            + 1;
        ledger.0 = id;
        ledger.1.push_back(DeliveryCredit {
            id,
            _count: permit,
            _bytes: bytes,
        });
        Some(id)
    }
    fn acknowledge(&self, id: u64) {
        let mut ledger = self.ledger.lock().unwrap();
        if id == 0 || id > ledger.0 {
            return;
        }
        while ledger.1.front().is_some_and(|pending| pending.id <= id) {
            ledger.1.pop_front();
        }
    }
}

pub struct Sink {
    send: Box<dyn Fn(Value) -> bool + Send + Sync>,
    credits: Arc<Credits>,
    status: String,
}
impl Sink {
    pub fn new(
        credits: Arc<Credits>,
        send: impl Fn(Value) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            send: Box::new(send),
            credits,
            status: String::new(),
        }
    }
    async fn data(&self, mut value: Value) -> Result<(), String> {
        let size = serde_json::to_vec(&value)
            .map_err(|_| "事件格式无效")?
            .len()
            + 64;
        value["delivery_id"] = json!(self.credits.reserve(size).await?);
        if !(self.send)(value) {
            return Err("事件窗口已关闭".into());
        }
        Ok(())
    }
    pub(crate) fn status(&mut self, state: &str, message: &str) -> bool {
        if self.status == state {
            return true;
        }
        let Some(delivery_id) = self.credits.try_reserve() else {
            return true;
        };
        self.status = state.to_owned();
        (self.send)(
            json!({"type":"status", "state":state, "message":message, "delivery_id":delivery_id}),
        )
    }
}

pub fn validate_base_url(base: &str) -> Result<Url, String> {
    let url = Url::parse(base).map_err(|_| "本机服务地址无效")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() == Some(0)
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("本机服务必须使用 HTTP 回环地址".into());
    }
    Ok(url)
}

async fn metadata(path: &Path) -> Result<DaemonInfo, String> {
    crate::startup::read_info(path)
        .await?
        .ok_or_else(|| "未找到本机后台服务信息".into())
}

async fn snapshot(client: &Client, info: &DaemonInfo, protocol: u16) -> Result<Value, String> {
    let url = validate_base_url(&info.base_url)?
        .join("api/v1/snapshot?window=1")
        .map_err(|_| "本机接口路径无效")?;
    let mut response = client
        .get(url)
        .bearer_auth(&info.token)
        .header("X-Self-Agent-Protocol", protocol.to_string())
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "无法读取后台快照")?;
    if !response.status().is_success() {
        return Err("后台快照暂不可用".into());
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SNAPSHOT as u64)
    {
        return Err("后台快照超过12 MiB限制".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "后台快照读取中断")? {
        if bytes.len() + chunk.len() > MAX_SNAPSHOT {
            return Err("后台快照超过12 MiB限制".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "后台快照格式无效")?;
    if value["event_cursor"].as_u64().is_none() {
        return Err("后台快照缺少事件游标".into());
    }
    Ok(value)
}

/// Reconnect only reads. No project command is retried. The outer task drops this
/// future on stop/supersede/window-close, including blocked HTTP or renderer credit.
pub async fn run(path: &Path, after: Option<u64>, mut sink: Sink) {
    let Ok(client) = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .read_timeout(Duration::from_secs(45))
        .build()
    else {
        sink.status("error", "无法创建本机事件连接");
        return;
    };
    let mut cursor = after;
    let mut identity: Option<(String, String, u32)> = None;
    let mut reset_snapshot = false;
    let mut snapshot_floor = None;
    let mut backoff = Duration::from_millis(200);
    if !sink.status("connecting", "正在连接执行事件") {
        return;
    }
    loop {
        let mut compatibility_rejected = false;
        let result: Result<(), String> = async {
            let info = metadata(path).await?;
            let negotiated = crate::compatibility::probe(
                &client,
                &validate_base_url(&info.base_url)?,
                &info.token,
            )
            .await
            .map_err(|error| {
                compatibility_rejected = error.permanent();
                error.to_string()
            })?
            .ok_or("后台连接暂不可用")?;
            let next_identity = (info.base_url.clone(), info.token.clone(), info.pid);
            if identity.as_ref().is_some_and(|old| old != &next_identity) {
                cursor = None;
                reset_snapshot = true;
            }
            identity = Some(next_identity);
            if cursor.is_none() {
                let value = snapshot(&client, &info, negotiated.protocol).await?;
                let next = value["event_cursor"].as_u64().unwrap();
                reset_snapshot |= snapshot_floor.is_some_and(|old| next < old);
                snapshot_floor = None;
                sink.data(json!({"type":"snapshot", "snapshot":value, "reset":reset_snapshot}))
                    .await?;
                reset_snapshot = false;
                cursor = Some(next);
            }
            let mut url = validate_base_url(&info.base_url)?
                .join("api/v1/events")
                .map_err(|_| "本机接口路径无效")?;
            url.query_pairs_mut()
                .append_pair("after", &cursor.unwrap().to_string());
            let mut response = client
                .get(url)
                .bearer_auth(&info.token)
                .header("X-Self-Agent-Protocol", negotiated.protocol.to_string())
                .send()
                .await
                .map_err(|_| "事件连接暂不可用")?;
            if response.status() == reqwest::StatusCode::CONFLICT {
                snapshot_floor = cursor;
                cursor = None;
                return Err("事件游标已失效，正在恢复快照".to_owned());
            }
            if !response.status().is_success() {
                return Err("后台事件暂不可用".into());
            }
            if response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_none_or(|v| v.split(';').next().unwrap_or("").trim() != "text/event-stream")
            {
                return Err("后台事件格式无效".into());
            }
            if !sink.status("connected", "执行事件已连接") {
                return Err("事件窗口已关闭".into());
            }
            backoff = Duration::from_millis(200);
            let mut decoder = Decoder::default();
            loop {
                let chunk = tokio::time::timeout(Duration::from_secs(45), response.chunk())
                    .await
                    .map_err(|_| "事件连接等待超时")?
                    .map_err(|_| "事件连接已断开")?;
                let Some(chunk) = chunk else {
                    return Err("事件连接已结束".into());
                };
                for frame in decoder.push(&chunk)? {
                    if frame.event == "resync" {
                        snapshot_floor = cursor;
                        cursor = None;
                        return Err("正在重新取得执行快照".into());
                    }
                    if frame.event != "run_event" {
                        continue;
                    }
                    let (sequence, value) = frame.event_value()?;
                    if sequence <= cursor.unwrap() {
                        continue;
                    }
                    sink.data(json!({"type":"event", "event":value})).await?;
                    cursor = Some(sequence);
                }
            }
        }
        .await;
        if compatibility_rejected {
            sink.status(
                "error",
                "后台协议不兼容，已停止事件读取；未替换后台。请使用匹配版本后重新连接。",
            );
            return;
        }
        if result.is_err()
            && !sink.status("reconnecting", "事件连接中断，正在恢复；后台任务继续执行")
        {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

#[derive(Default)]
struct Frame {
    event: String,
    id: Option<String>,
    data: String,
}
impl Frame {
    fn event_value(&self) -> Result<(u64, Value), String> {
        let id = self.id.as_deref().ok_or("后台事件缺少游标")?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err("后台事件游标无效".into());
        }
        let sequence: u64 = id.parse().map_err(|_| "后台事件游标无效")?;
        let value: Value = serde_json::from_str(&self.data).map_err(|_| "后台事件JSON无效")?;
        if sequence == 0 || value["sequence"].as_u64() != Some(sequence) {
            return Err("后台事件游标不一致".into());
        }
        Ok((sequence, value))
    }
}
#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    frame: Frame,
    bytes: usize,
    skip_lf: bool,
    has_data: bool,
}
impl Decoder {
    fn push(&mut self, input: &[u8]) -> Result<Vec<Frame>, String> {
        let mut frames = Vec::new();
        for &byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.bytes += 1;
            if self.bytes > MAX_FRAME {
                return Err("后台事件超过512 KiB限制".into());
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                if self.line.is_empty() {
                    let frame = std::mem::take(&mut self.frame);
                    if self.has_data || !frame.event.is_empty() {
                        frames.push(frame);
                    }
                    self.has_data = false;
                    self.bytes = 0;
                } else {
                    let bytes = std::mem::take(&mut self.line);
                    let line = std::str::from_utf8(&bytes).map_err(|_| "后台事件UTF-8无效")?;
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "event" => self.frame.event = value.to_owned(),
                        "id" if !value.contains('\0') => self.frame.id = Some(value.to_owned()),
                        "data" => {
                            if self.has_data {
                                self.frame.data.push('\n');
                            }
                            self.frame.data.push_str(value);
                            self.has_data = true;
                        }
                        _ => {}
                    }
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_handles_split_utf8_crlf_duplicates_and_bad_sequences() {
        let bytes =
            "event: run_event\r\nid: 42\r\ndata: {\"sequence\":42,\"text\":\"中文\"}\r\n\r\n"
                .as_bytes();
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        for byte in bytes {
            frames.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_value().unwrap().1["text"], "中文");
        frames[0].id = Some("43".into());
        assert!(frames[0].event_value().is_err());
        assert!(Decoder::default().push(&vec![b'x'; MAX_FRAME + 1]).is_err());
        assert!(Decoder::default().push(&[0xff, b'\n']).is_err());
    }
    #[test]
    fn superseded_and_closed_subscriptions_cannot_emit_or_cross_windows() {
        let registry = Subscriptions::default();
        let old = registry.start("a", "same").unwrap();
        let current = registry.start("a", "same").unwrap();
        let other = registry.start("b", "same").unwrap();
        assert!(old.cancel.has_changed().unwrap_or(true));
        assert!(!registry.send("a", "same", old.generation, || panic!("stale callback")));
        registry.finish("a", "same", old.generation);
        assert!(registry.send("a", "same", current.generation, || true));
        registry.stop_window("a");
        assert!(!registry.send("a", "same", current.generation, || panic!(
            "closed callback"
        )));
        assert!(registry.send("b", "same", other.generation, || true));
        registry.stop("b", "same");
        assert!(!registry.send("b", "same", other.generation, || true));
    }
    #[tokio::test]
    async fn renderer_backpressure_is_bounded_and_acks_are_cumulative() {
        let credits = Credits::new();
        for id in 1..=MAX_PENDING as u64 {
            assert_eq!(credits.reserve(1).await.unwrap(), id);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(10), credits.reserve(1))
                .await
                .is_err()
        );
        credits.acknowledge(9999); // Unissued acknowledgements cannot create free credits.
        assert_eq!(credits.semaphore.available_permits(), 0);
        credits.acknowledge(2);
        assert_eq!(credits.semaphore.available_permits(), 2);
        assert_eq!(credits.reserve(1).await.unwrap(), 65);
        credits.acknowledge(2);
        assert_eq!(credits.semaphore.available_permits(), 1);
        let bytes = Credits::new();
        bytes.reserve(16 * 1024 * 1024).await.unwrap();
        bytes.reserve(16 * 1024 * 1024).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), bytes.reserve(1))
                .await
                .is_err()
        );
        bytes.acknowledge(1);
        assert_eq!(bytes.reserve(1).await.unwrap(), 3);
    }
    #[tokio::test]
    async fn acknowledgements_from_superseded_subscription_cannot_release_new_messages() {
        let registry = Subscriptions::default();
        let old = registry.start("main", "reused").unwrap();
        let old_delivery = old.credits.reserve(1).await.unwrap();
        let current = registry.start("main", "reused").unwrap();
        let current_delivery = current.credits.reserve(1).await.unwrap();
        assert!(current_delivery > old_delivery);
        registry.acknowledge("main", "reused", old_delivery);
        assert_eq!(
            current.credits.semaphore.available_permits(),
            MAX_PENDING - 1
        );
        registry.acknowledge("main", "reused", current_delivery);
        assert_eq!(current.credits.semaphore.available_permits(), MAX_PENDING);
    }

    #[test]
    fn rejects_external_metadata_and_unbounded_subscription_ids() {
        for url in [
            "https://127.0.0.1",
            "http://localhost",
            "http://x@127.0.0.1",
            "http://127.0.0.1/path",
            "http://127.0.0.1/?q=1",
        ] {
            assert!(validate_base_url(url).is_err());
        }
        let registry = Subscriptions::default();
        assert!(registry.start("main", "../x").is_err());
        for id in 0..16 {
            registry.start("main", &format!("id-{id}")).unwrap();
        }
        assert!(registry.start("main", "overflow").is_err());
        assert!(registry.start("main", "id-0").is_ok());
    }

    async fn serve_response(
        socket: &mut tokio::net::TcpStream,
        status: &str,
        kind: &str,
        body: &str,
    ) {
        use tokio::io::AsyncWriteExt;
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    }

    async fn accept_application(
        listener: &tokio::net::TcpListener,
    ) -> (tokio::net::TcpStream, String) {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            if !request.starts_with("GET /api/v1/compatibility ") {
                return (socket, request);
            }
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer "));
            let body = harness_core::compatibility::handshake(
                harness_core::compatibility::LifecycleState::Running,
            )
            .unwrap();
            serve_response(
                &mut socket,
                "200 OK",
                "application/json",
                std::str::from_utf8(&body).unwrap(),
            )
            .await;
        }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            assert!(bytes.len() < 16 * 1024);
            if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn frame(sequence: u64) -> String {
        format!("id: {sequence}\nevent: run_event\ndata: {{\"sequence\":{sequence},\"kind\":\"assistant_delta\",\"data\":{{\"text\":\"中文\"}}}}\n\n")
    }

    fn collecting_sink() -> (Sink, Arc<Mutex<Vec<Value>>>) {
        let credits = Arc::new(Credits::new());
        let output = Arc::new(Mutex::new(Vec::new()));
        let collected = output.clone();
        let acks = credits.clone();
        let sink = Sink::new(credits, move |value| {
            let id = value["delivery_id"].as_u64();
            collected.lock().unwrap().push(value);
            if let Some(id) = id {
                acks.acknowledge(id);
            }
            true
        });
        (sink, output)
    }

    async fn wait_event(output: &Arc<Mutex<Vec<Value>>>, sequence: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if output
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|v| v["event"]["sequence"] == sequence)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_recovery_replaces_snapshots_and_deduplicates_replayed_events() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let path = std::env::temp_dir().join(format!("bridge-{}.json", harness_core::new_id()));
        tokio::fs::write(
            &path,
            json!({"base_url":format!("http://{address}"),"token":"fixture-token","pid":1})
                .to_string(),
        )
        .await
        .unwrap();
        let server = tokio::spawn(async move {
            for phase in 0..5 {
                let (mut socket, request) = accept_application(&listener).await;
                assert!(request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fixture-token"));
                match phase {
                    0 => {
                        assert!(request.starts_with("GET /api/v1/events?after=100 "));
                        serve_response(
                            &mut socket,
                            "409 Conflict",
                            "application/json",
                            "{\"code\":\"snapshot_required\"}",
                        )
                        .await;
                    }
                    1 | 3 => {
                        assert!(request.starts_with("GET /api/v1/snapshot?window=1 "));
                        serve_response(
                            &mut socket,
                            "200 OK",
                            "application/json",
                            &format!(
                                "{{\"event_cursor\":{},\"tasks\":[]}}",
                                if phase == 1 { 5 } else { 6 }
                            ),
                        )
                        .await;
                    }
                    2 => {
                        assert!(request.starts_with("GET /api/v1/events?after=5 "));
                        serve_response(
                            &mut socket,
                            "200 OK",
                            "text/event-stream",
                            &(frame(6) + &frame(6) + "event: resync\ndata: snapshot_required\n\n"),
                        )
                        .await;
                    }
                    4 => {
                        assert!(request.starts_with("GET /api/v1/events?after=6 "));
                        serve_response(&mut socket, "200 OK", "text/event-stream", &frame(7)).await;
                    }
                    _ => unreachable!(),
                }
            }
        });
        let (sink, output) = collecting_sink();
        let run_path = path.clone();
        let transport = tokio::spawn(async move {
            run(&run_path, Some(100), sink).await;
        });
        wait_event(&output, 7).await;
        transport.abort();
        server.await.unwrap();
        let messages = output.lock().unwrap().clone();
        assert_eq!(
            messages.iter().filter(|v| v["type"] == "snapshot").count(),
            2
        );
        let snapshots: Vec<_> = messages
            .iter()
            .filter(|v| v["type"] == "snapshot")
            .collect();
        assert_eq!(snapshots[0]["reset"], true); // Future cursor means an earlier database incarnation.
        assert_eq!(snapshots[1]["reset"], false); // Ordinary resync keeps the current event epoch.
        assert_eq!(
            messages
                .iter()
                .filter(|v| v["event"]["sequence"] == 6)
                .count(),
            1
        );
        assert!(!serde_json::to_string(&*messages)
            .unwrap()
            .contains("fixture-token"));
        drop(messages);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn daemon_metadata_rotation_uses_new_endpoint_token_and_reset_snapshot() {
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_address = first.local_addr().unwrap();
        let second_address = second.local_addr().unwrap();
        let path = std::env::temp_dir().join(format!("bridge-{}.json", harness_core::new_id()));
        tokio::fs::write(
            &path,
            json!({"base_url":format!("http://{first_address}"),"token":"old-fixture","pid":1})
                .to_string(),
        )
        .await
        .unwrap();
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            let (mut socket, request) = accept_application(&first).await;
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer old-fixture"));
            tokio::fs::write(&server_path, json!({"base_url":format!("http://{second_address}"),"token":"new-fixture","pid":2}).to_string()).await.unwrap();
            serve_response(&mut socket, "200 OK", "text/event-stream", &frame(1)).await;
            let (mut socket, request) = accept_application(&second).await;
            assert!(request.starts_with("GET /api/v1/snapshot?window=1 "));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer new-fixture"));
            serve_response(
                &mut socket,
                "200 OK",
                "application/json",
                "{\"event_cursor\":1,\"tasks\":[]}",
            )
            .await;
            let (mut socket, request) = accept_application(&second).await;
            assert!(request.starts_with("GET /api/v1/events?after=1 "));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer new-fixture"));
            serve_response(&mut socket, "200 OK", "text/event-stream", &frame(2)).await;
        });
        let (sink, output) = collecting_sink();
        let run_path = path.clone();
        let transport = tokio::spawn(async move {
            run(&run_path, Some(0), sink).await;
        });
        wait_event(&output, 2).await;
        transport.abort();
        server.await.unwrap();
        let messages = output.lock().unwrap().clone();
        assert!(messages
            .iter()
            .any(|value| value["type"] == "snapshot" && value["reset"] == true));
        let serialized = serde_json::to_string(&*messages).unwrap();
        assert!(!serialized.contains("old-fixture") && !serialized.contains("new-fixture"));
        drop(messages);
        tokio::fs::remove_file(path).await.unwrap();
    }
    #[tokio::test]
    async fn snapshot_recovery_refuses_over_12_mib_before_body_or_json() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info: DaemonInfo = serde_json::from_value(json!({
            "base_url": format!("http://{}", listener.local_addr().unwrap()),
            "token": "fixture-private", "pid": 1
        }))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert!(request.starts_with("GET /api/v1/snapshot?window=1 "));
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        12 * 1024 * 1024 + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            // No body: accepting the header would otherwise block until timeout.
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), snapshot(&client, &info, 1))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error, "后台快照超过12 MiB限制");
        server.abort();
        let _ = server.await;
    }
}
