//! Real loopback HTTP/WS + durable SQLite worker contracts; no platform or model calls.
use super::*;
use crate::qq::{events, persistence as p, transport::Credentials};
use futures_util::{SinkExt, StreamExt};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{broadcast, Notify},
    time::timeout,
};
use tokio_tungstenite::tungstenite::Message;
use zeroize::Zeroizing;

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct TestBackend {
    db: Mutex<Connection>,
    path: PathBuf,
    enabled: AtomicBool,
    failed: AtomicBool,
    authority: AtomicBool,
    outbox: bool,
    gate: Option<Arc<Gate>>,
    states: Mutex<Vec<ConnectionState>>,
}
impl Drop for TestBackend {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
    }
}
impl TestBackend {
    fn new(outbox: bool, gate: Option<Arc<Gate>>) -> Arc<Self> {
        let path = std::env::temp_dir().join(format!("qq-runtime-{}.sqlite", uuid::Uuid::new_v4()));
        let mut db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        p::migrate(&db).unwrap();
        db.execute_batch("CREATE TABLE fixture_checkpoint(id INTEGER PRIMARY KEY,body TEXT); CREATE TABLE fixture_commands(id TEXT PRIMARY KEY); CREATE TABLE fixture_clears(id INTEGER PRIMARY KEY); CREATE TABLE fixture_meta(id INTEGER PRIMARY KEY,bot TEXT);").unwrap();
        let identity = p::Identity {
            app_id: "app_1".into(),
            sender_open_id: "sender_1".into(),
            kind: ConversationKind::User,
            target_id: "sender_1".into(),
        };
        let now = now_ms() - 31_000;
        let tx = db.transaction().unwrap();
        let code = p::issue_pairing(&tx, "app_1", now, 60).unwrap();
        let pending = p::redeem_pairing(&tx, &identity, &code.code, now).unwrap();
        p::confirm_binding(
            &tx,
            &pending.principal_id,
            pending.revision,
            &identity,
            json!({"permissions":{"view":true},"session_ids":["fixture"]}),
            now + 3600000,
            now,
        )
        .unwrap();
        tx.commit().unwrap();
        Arc::new(Self {
            db: Mutex::new(db),
            path,
            enabled: AtomicBool::new(true),
            failed: AtomicBool::new(false),
            authority: AtomicBool::new(true),
            outbox,
            gate,
            states: Mutex::new(Vec::new()),
        })
    }
    fn count(&self, table: &str) -> u64 {
        self.db
            .lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
    fn statuses(&self) -> Vec<String> {
        let db = self.db.lock().unwrap();
        let mut query = db
            .prepare("SELECT status FROM qq_outbox ORDER BY created_at_ms,id")
            .unwrap();
        query
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
    fn recover(&self) {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction().unwrap();
        p::startup_recover(&tx, now_ms()).unwrap();
        tx.commit().unwrap();
    }
}
impl Backend for TestBackend {
    fn failed_worker(&self) {
        self.failed.store(true, Ordering::Release);
    }
    fn worker_certain(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
    }

    fn current(&self) -> bool {
        // Match the real Store-backed generation check, including mutex contention.
        let _db = self.db.lock().unwrap();
        self.enabled.load(Ordering::Acquire) && self.worker_certain()
    }
    fn checkpoint(&self) -> Result<Option<Checkpoint>> {
        let db = self.db.lock().unwrap();
        let raw: Option<String> = db
            .query_row(
                "SELECT body FROM fixture_checkpoint WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
    fn ingest(&self, payload: &Value, checkpoint: &Checkpoint) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let bot: Option<String> = tx
            .query_row("SELECT bot FROM fixture_meta WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        match events::dispatch("app_1", bot.as_deref(), payload, now_ms())? {
            events::Dispatch::Ready { bot_user_id } => {
                tx.execute("INSERT INTO fixture_meta VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET bot=excluded.bot",[bot_user_id])?;
            }
            events::Dispatch::Input(input) => {
                p::accept_input(&tx, &input, now_ms())?;
            }
            events::Dispatch::ClockSkew { timestamp_ms } => {
                p::record_clock_skew(&tx, timestamp_ms, now_ms())?
            }
            events::Dispatch::Ignored => {}
        }
        tx.execute("INSERT INTO fixture_checkpoint VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",[serde_json::to_string(checkpoint)?])?;
        if checkpoint.seq == 2 {
            if let Some(gate) = &self.gate {
                gate.block();
            }
        }
        tx.commit()?;
        Ok(())
    }
    fn clear_checkpoint(&self) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM fixture_checkpoint", [])?;
        tx.execute("INSERT INTO fixture_clears DEFAULT VALUES", [])?;
        tx.commit()?;
        Ok(())
    }
    fn connection(&self, state: ConnectionState) {
        self.states.lock().unwrap().push(state);
    }
    fn process_input(&self) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let Some(input) = p::pending_inputs(&tx, 1)?.into_iter().next() else {
            return Ok(false);
        };
        p::guard_input(&tx, &input.inbox_id, now_ms())?;
        tx.execute(
            "INSERT INTO fixture_commands VALUES(?1)",
            [&input.command_id],
        )?;
        if self.outbox {
            if let Some(route) = &input.route_id {
                p::enqueue_outbox(
                    &tx,
                    route,
                    &input.command_id,
                    p::OutboxKind::Ack,
                    "模拟指令已处理",
                    now_ms(),
                )?;
            }
        }
        p::mark_handled(
            &tx,
            &input.inbox_id,
            &input.command_id,
            json!({"fixture":true}),
            now_ms(),
        )?;
        tx.commit()?;
        Ok(true)
    }
    fn claim(&self) -> Result<Option<OutboxClaim>> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let claim = p::claim_outbox(&tx, now_ms())?;
        tx.commit()?;
        Ok(claim)
    }
    fn allowed(&self, claim: &OutboxClaim) -> bool {
        if !self.authority.load(Ordering::Acquire) {
            return false;
        }
        let result = (|| -> Result<bool> {
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction()?;
            let allowed = p::authorize_send(
                &tx,
                &claim.record.id,
                claim.record.claim_nonce.as_deref().unwrap(),
                now_ms(),
            )?;
            tx.commit()?;
            Ok(allowed)
        })();
        result.unwrap_or(false)
    }
    fn finish(&self, claim: &OutboxClaim, outcome: DeliveryOutcome) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        p::finalize_outbox(
            &tx,
            &claim.record.id,
            claim.record.claim_nonce.as_deref().unwrap(),
            outcome,
            now_ms(),
        )?;
        tx.commit()?;
        Ok(())
    }
}
#[derive(Default)]
struct Seen {
    frames: Vec<Value>,
    posts: Vec<Value>,
    identities: Vec<Value>,
    auths: usize,
}
struct Fixture {
    origin: String,
    seen: Arc<Mutex<Seen>>,
    push: broadcast::Sender<Value>,
    post_entered: Arc<Notify>,
    post_release: Arc<Notify>,
    auth_entered: Arc<Notify>,
    auth_release: Arc<Notify>,
    fresh_entered: Arc<Notify>,
    fresh_release: Arc<Notify>,
    worker: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.post_release.notify_waiters();
        self.auth_release.notify_waiters();
        self.fresh_release.notify_waiters();
        self.worker.abort();
    }
}
fn inbound(id: &str, seq: u64, timestamp: &str) -> Value {
    json!({"op":0,"s":seq,"t":"C2C_MESSAGE_CREATE","id":format!("event-{id}"),"d":{"id":id,"author":{"user_openid":"sender_1"},"content":"/会话","timestamp":timestamp}})
}
impl Fixture {
    async fn start(mode: &'static str, replay: Option<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let ws_url = origin.replacen("http:", "ws:", 1) + "websocket";
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (push, _) = broadcast::channel::<Value>(32);
        let post_entered = Arc::new(Notify::new());
        let post_release = Arc::new(Notify::new());
        let auth_entered = Arc::new(Notify::new());
        let auth_release = Arc::new(Notify::new());
        let fresh_entered = Arc::new(Notify::new());
        let fresh_release = Arc::new(Notify::new());
        let state = seen.clone();
        let events = push.clone();
        let pe = post_entered.clone();
        let pr = post_release.clone();
        let ae = auth_entered.clone();
        let ar = auth_release.clone();
        let fe = fresh_entered.clone();
        let fr = fresh_release.clone();
        let worker = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let seen = state.clone();
                let ws_url = ws_url.clone();
                let mut events = events.subscribe();
                let pe = pe.clone();
                let pr = pr.clone();
                let ae = ae.clone();
                let ar = ar.clone();
                let fe = fe.clone();
                let fr = fr.clone();
                let replay = replay.clone();
                tokio::spawn(async move {
                    let mut peek = [0; 4096];
                    loop {
                        let n = socket.peek(&mut peek).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        if peek[..n].windows(2).any(|w| w == b"\r\n") {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                    if String::from_utf8_lossy(&peek).starts_with("GET /websocket ") {
                        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                        ws.send(Message::Text(
                            json!({"op":10,"d":{"heartbeat_interval":500}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                        let identity: Value = serde_json::from_str(
                            ws.next().await.unwrap().unwrap().to_text().unwrap(),
                        )
                        .unwrap();
                        let number = {
                            let mut state = seen.lock().unwrap();
                            state.identities.push(identity.clone());
                            state.identities.len()
                        };
                        if mode == "replay-invalid" && number == 3 {
                            let _ = ws
                                .send(Message::Text(json!({"op":9}).to_string().into()))
                                .await;
                            return;
                        }
                        if mode == "replay-invalid" && number == 4 {
                            fe.notify_one();
                            fr.notified().await;
                        }
                        let ready = if identity["op"] == 6 {
                            if mode == "replay-invalid" && number == 2 {
                                let mut replay = replay.unwrap();
                                replay["s"] = json!(3);
                                ws.send(Message::Text(replay.to_string().into()))
                                    .await
                                    .unwrap();
                                json!({"op":0,"t":"RESUMED","s":4,"d":{}})
                            } else {
                                json!({"op":0,"t":"RESUMED","s":identity["d"]["seq"].as_u64().unwrap()+1,"d":{}})
                            }
                        } else {
                            json!({"op":0,"t":"READY","s":1,"d":{"session_id":format!("fixture-{number}"),"user":{"id":"bot_1"}}})
                        };
                        ws.send(Message::Text(ready.to_string().into()))
                            .await
                            .unwrap();
                        loop {
                            tokio::select! {
                                frame=ws.next()=>{let Some(Ok(Message::Text(text)))=frame else {break;};let value:Value=serde_json::from_str(&text).unwrap();seen.lock().unwrap().frames.push(value.clone());if value["op"]==1 && ws.send(Message::Text(json!({"op":11}).to_string().into())).await.is_err(){break;}},
                                event=events.recv()=>{let Ok(event)=event else {break;};if ws.send(Message::Text(event.to_string().into())).await.is_err(){break;}}
                            }
                        }
                        return;
                    }
                    let mut bytes = Vec::new();
                    let end;
                    loop {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(i) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                            end = i + 4;
                            break;
                        }
                        assert!(bytes.len() < 32768);
                    }
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_string();
                    let path = headers
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_owned();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while bytes.len() < end + length {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    let body: Value =
                        serde_json::from_slice(&bytes[end..end + length]).unwrap_or(Value::Null);
                    let (status, response, extra) = if path == "/app/getAppAccessToken" {
                        assert_eq!(body["clientSecret"], "fixture-secret");
                        seen.lock().unwrap().auths += 1;
                        if mode == "auth-gate" {
                            ae.notify_one();
                            ar.notified().await;
                        }
                        (
                            200,
                            json!({"access_token":"fixture-token","expires_in":7200}),
                            "",
                        )
                    } else if path == "/gateway" {
                        (200, json!({"url":ws_url}), "")
                    } else {
                        assert_eq!(path, "/v2/users/sender_1/messages");
                        let count = {
                            let mut state = seen.lock().unwrap();
                            state.posts.push(body);
                            state.posts.len()
                        };
                        pe.notify_one();
                        if mode == "post-gate" {
                            pr.notified().await;
                        }
                        if mode == "429" && count == 1 {
                            (429, json!({}), "Retry-After: 1\r\n")
                        } else {
                            (200, json!({"id":format!("sent-{count}")}), "")
                        }
                    };
                    let response = response.to_string();
                    let raw=format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{response}",response.len());
                    let _ = socket.write_all(raw.as_bytes()).await;
                });
            }
        });
        Self {
            origin,
            seen,
            push,
            post_entered,
            post_release,
            auth_entered,
            auth_release,
            fresh_entered,
            fresh_release,
            worker,
        }
    }
    fn transport(&self) -> Transport {
        Transport::fixture(
            Credentials::new("app_1".into(), Zeroizing::new(b"fixture-secret".to_vec())).unwrap(),
            &self.origin,
        )
        .unwrap()
    }
    fn emit(&self, value: Value) {
        self.push.send(value).unwrap();
    }
}
async fn until(check: impl Fn() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
fn start(
    backend: Arc<TestBackend>,
    receive: &Fixture,
    delivery: &Fixture,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<Result<()>>) {
    let (tx, rx) = watch::channel(false);
    (
        tx,
        tokio::spawn(run(backend, receive.transport(), delivery.transport(), rx)),
    )
}
async fn stop(tx: watch::Sender<bool>, worker: tokio::task::JoinHandle<Result<()>>) {
    let _ = tx.send(true);
    timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}
struct ReleaseGate(Arc<Gate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[tokio::test]
async fn sqlite_checkpoint_is_committed_before_transport_ack_and_heartbeat() {
    let gate = Arc::new(Gate::default());
    let _release = ReleaseGate(gate.clone());
    let backend = TestBackend::new(false, Some(gate.clone()));
    let fixture = Fixture::start("normal", None).await;
    let (shutdown, worker) = start(backend.clone(), &fixture, &fixture);
    until(|| backend.checkpoint().unwrap().is_some_and(|cp| cp.seq == 1)).await;

    fixture.emit(inbound("message-1", 2, &timestamp()));

    until(|| gate.entered.load(Ordering::Acquire)).await;
    // An independent reader sees only the last committed snapshot while the
    // real inbox+checkpoint transaction is paused just before COMMIT.

    let observer = Connection::open(&backend.path).unwrap();

    let checkpoint: String = observer
        .query_row("SELECT body FROM fixture_checkpoint WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Checkpoint>(&checkpoint).unwrap().seq,
        1
    );
    assert_eq!(
        observer
            .query_row("SELECT COUNT(*) FROM qq_inbox", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    until(|| {
        fixture
            .seen
            .lock()
            .unwrap()
            .frames
            .iter()
            .any(|v| v["op"] == 1 && v["d"] == 1)
    })
    .await;
    assert!(!fixture
        .seen
        .lock()
        .unwrap()
        .frames
        .iter()
        .any(|v| v["d"] == 2));

    gate.release();
    until(|| {
        fixture
            .seen
            .lock()
            .unwrap()
            .frames
            .iter()
            .any(|v| v["op"] == 1 && v["d"] == 2)
    })
    .await;

    let checkpoint: String = observer
        .query_row("SELECT body FROM fixture_checkpoint WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Checkpoint>(&checkpoint).unwrap().seq,
        2
    );
    assert_eq!(
        observer
            .query_row("SELECT COUNT(*) FROM qq_inbox", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    until(|| backend.count("fixture_commands") == 1).await;

    stop(shutdown, worker).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_is_idempotent_and_invalid_session_clears_checkpoint_durably() {
    let original = inbound("replayed-message", 2, &timestamp());
    let fixture = Fixture::start("replay-invalid", Some(original.clone())).await;
    let backend = TestBackend::new(false, None);
    let (shutdown, worker) = start(backend.clone(), &fixture, &fixture);
    until(|| backend.checkpoint().unwrap().is_some_and(|c| c.seq == 1)).await;
    fixture.emit(original);
    until(|| backend.count("fixture_commands") == 1).await;
    fixture.emit(json!({"op":7}));
    until(|| backend.checkpoint().unwrap().is_some_and(|c| c.seq == 4)).await;
    assert_eq!(backend.count("qq_inbox"), 1);
    assert_eq!(backend.count("fixture_commands"), 1);
    fixture.emit(json!({"op":7}));
    timeout(Duration::from_secs(10), fixture.fresh_entered.notified())
        .await
        .unwrap();
    assert_eq!(backend.count("fixture_clears"), 1);
    assert!(backend.checkpoint().unwrap().is_none());
    let observer = Connection::open(&backend.path).unwrap();
    assert_eq!(
        observer
            .query_row("SELECT COUNT(*) FROM fixture_checkpoint", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    fixture.fresh_release.notify_one();
    until(|| {
        backend
            .checkpoint()
            .unwrap()
            .is_some_and(|c| c.session_id == "fixture-4")
    })
    .await;
    let ops: Vec<_> = fixture
        .seen
        .lock()
        .unwrap()
        .identities
        .iter()
        .map(|v| v["op"].as_u64().unwrap())
        .collect();
    assert_eq!(ops, vec![2, 6, 6, 2]);
    stop(shutdown, worker).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commands_continue_during_blocked_post_and_stop_unknown_is_never_resent() {
    let receive = Fixture::start("normal", None).await;
    let delivery = Fixture::start("post-gate", None).await;
    let backend = TestBackend::new(true, None);
    let (shutdown, mut worker) = start(backend.clone(), &receive, &delivery);
    until(|| backend.checkpoint().unwrap().is_some()).await;
    receive.emit(inbound("first-message", 2, &timestamp()));
    timeout(Duration::from_secs(10), delivery.post_entered.notified())
        .await
        .unwrap();
    assert_eq!(backend.statuses(), vec!["sending"]);
    // Real HTTP response remains gated, but the independent receive/command
    // futures must ingest and finish another restricted-command transaction.
    receive.emit(inbound("second-message", 3, &timestamp()));
    until(|| backend.count("fixture_commands") == 2).await;
    assert_eq!(delivery.seen.lock().unwrap().posts.len(), 1);
    shutdown.send(true).unwrap();
    assert!(timeout(Duration::from_millis(100), &mut worker)
        .await
        .is_err());
    delivery.post_release.notify_one();
    stop(shutdown, worker).await;
    let statuses = backend.statuses();
    assert!(statuses.contains(&"unknown".into()) && statuses.contains(&"pending".into()));
    backend.recover();
    delivery.post_release.notify_one();
    let next_receive = Fixture::start("normal", None).await;
    let next_delivery = Fixture::start("normal", None).await;
    let (shutdown, worker) = start(backend.clone(), &next_receive, &next_delivery);
    until(|| backend.statuses().contains(&"sent".into())).await;
    let posts = next_delivery.seen.lock().unwrap().posts.clone();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["msg_id"], "second-message");
    assert_eq!(backend.count("fixture_commands"), 2);
    assert!(backend.statuses().contains(&"unknown".into()));
    stop(shutdown, worker).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_retries_same_durable_message_and_sequence() {
    let receive = Fixture::start("normal", None).await;
    let delivery = Fixture::start("429", None).await;
    let backend = TestBackend::new(true, None);
    let (shutdown, worker) = start(backend.clone(), &receive, &delivery);
    until(|| backend.checkpoint().unwrap().is_some()).await;
    receive.emit(inbound("retry-message", 2, &timestamp()));
    until(|| backend.statuses() == vec!["sent"]).await;
    let posts = delivery.seen.lock().unwrap().posts.clone();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0], posts[1]);
    assert_eq!(posts[0]["msg_id"], "retry-message");
    assert_eq!(posts[0]["msg_seq"], 1);
    assert_eq!(
        backend
            .db
            .lock()
            .unwrap()
            .query_row("SELECT attempts FROM qq_outbox", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    stop(shutdown, worker).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authority_revocation_while_token_is_pending_prevents_post() {
    let receive = Fixture::start("normal", None).await;
    let delivery = Fixture::start("auth-gate", None).await;
    let backend = TestBackend::new(true, None);
    let (shutdown, worker) = start(backend.clone(), &receive, &delivery);
    until(|| backend.checkpoint().unwrap().is_some()).await;
    receive.emit(inbound("revoked-message", 2, &timestamp()));
    timeout(Duration::from_secs(10), delivery.auth_entered.notified())
        .await
        .unwrap();
    assert_eq!(backend.statuses(), vec!["sending"]);
    backend.authority.store(false, Ordering::Release);
    delivery.auth_release.notify_one();
    until(|| backend.statuses() == vec!["failed"]).await;
    assert!(delivery.seen.lock().unwrap().posts.is_empty());
    assert_eq!(backend.count("fixture_commands"), 1);
    stop(shutdown, worker).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_original_delivery_finalization_returns_cleanup_unknown_not_success() {
    let receive = Fixture::start("normal", None).await;
    let delivery = Fixture::start("post-gate", None).await;
    let backend = TestBackend::new(true, None);
    let (_shutdown, worker) = start(backend.clone(), &receive, &delivery);
    until(|| backend.checkpoint().unwrap().is_some()).await;
    receive.emit(inbound("failed-finalize", 2, &timestamp()));
    timeout(Duration::from_secs(10), delivery.post_entered.notified())
        .await
        .unwrap();
    backend.db.lock().unwrap().execute_batch("CREATE TRIGGER reject_finalize BEFORE UPDATE ON qq_outbox BEGIN SELECT RAISE(ABORT, 'fixture rejected finalize'); END;").unwrap();
    delivery.post_release.notify_one();
    assert!(timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert_eq!(backend.statuses(), vec!["sending"]);
    assert_eq!(delivery.seen.lock().unwrap().posts.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql_command_transaction_failure_stops_generation_without_retry_or_send() {
    let receive = Fixture::start("normal", None).await;
    let delivery = Fixture::start("normal", None).await;
    let backend = TestBackend::new(true, None);
    backend.db.lock().unwrap().execute_batch("CREATE TRIGGER reject_command BEFORE INSERT ON fixture_commands BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
    let (_shutdown, worker) = start(backend.clone(), &receive, &delivery);
    until(|| backend.checkpoint().unwrap().is_some()).await;
    receive.emit(inbound("failed-command", 2, &timestamp()));
    assert!(timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(!backend.worker_certain());
    assert_eq!(backend.count("fixture_commands"), 0);
    assert!(delivery.seen.lock().unwrap().posts.is_empty());
}
