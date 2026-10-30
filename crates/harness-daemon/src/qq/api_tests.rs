//! Actual authenticated HTTP routes plus an in-memory OS-vault protocol fixture.
//! No production QQ or operating-system credential service is contacted.
use super::*;
use crate::capabilities::vault::{
    test_support::{self, MockVaultControl},
    Vault, VaultReason,
};
use harness_core::new_id;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
const OWNER: &str = "qq-api-fixture-owner-token";
const SECRET: &str = "qq-api-private-canary-not-persisted";
struct Http {
    root: PathBuf,
    state: Arc<AppState>,
    client: reqwest::Client,
    base: String,
    server: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}
impl Http {
    async fn open(root: PathBuf, vault: Arc<Vault>) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        let state =
            AppState::open_with_vault(&root.join("state.sqlite"), OWNER.into(), vault).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::router(state.clone());
        let (shutdown, closed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = closed.await;
                })
                .await
                .unwrap();
        });
        Self {
            root,
            state,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            base,
            server,
            shutdown,
        }
    }
    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(OWNER);
        if let Some(body) = body {
            request = request.json(&body)
        }
        let response = request.send().await.unwrap();
        assert!(
            response.status().is_success(),
            "QQ fixture route status {}",
            response.status()
        );
        let value: Value = response.json().await.unwrap();
        safe(&value.to_string());
        value
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }
    async fn snapshot(&self) -> Value {
        self.request(reqwest::Method::GET, "/api/v1/qq", None).await
    }
    async fn config(&self) -> Value {
        self.post("/api/v1/qq/commands",json!({"id":new_id(),"command":{"type":"configure","config_id":null,"expected_revision":null,"app_id":"app_1","enabled":false,"reply_policy":{"window_ms":300000,"max_user":4,"max_group":5}}})).await
    }
    async fn set(&self, config: &Value, id: &str) -> Value {
        self.post(
            &format!(
                "/api/v1/qq/credentials/{}",
                config["config_id"].as_str().unwrap()
            ),
            json!({"id":id,"expected_revision":config["revision"],"secret":SECRET}),
        )
        .await
    }
    async fn settled(&self, id: &str, status: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let operation = self
                    .request(
                        reqwest::Method::GET,
                        &format!("/api/v1/qq/credential-operations/{id}"),
                        None,
                    )
                    .await;
                if operation["status"] == status {
                    return operation;
                }
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .expect("QQ fixture operation did not settle")
    }
    async fn stop(self) -> PathBuf {
        let _ = self.shutdown.send(());
        tokio::time::timeout(Duration::from_secs(3), self.server)
            .await
            .unwrap()
            .unwrap();
        assert!(self.state.shutdown_credentials().await);
        drop(self.client);
        drop(self.state);
        self.root
    }
}
fn safe(text: &str) {
    assert!(!text.contains(SECRET));
    assert!(!text.contains("self-agent.vault.v1:"));
}
async fn fixture() -> (Http, MockVaultControl) {
    let (vault, control) = test_support::memory(8).unwrap();
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("qq-api-{}", new_id()));
    (Http::open(root, vault).await, control)
}
fn clean(root: PathBuf) {
    for file in ["state.sqlite", "state.sqlite-wal"] {
        if let Ok(bytes) = std::fs::read(root.join(file)) {
            assert!(!bytes.windows(SECRET.len()).any(|v| v == SECRET.as_bytes()));
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_routes_set_replay_delete_and_public_metadata_are_scoped() {
    let (http, control) = fixture().await;
    let unauthorized = http
        .client
        .get(format!("{}/api/v1/qq", http.base))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let config = http.config().await;
    assert_eq!(config["status"], "completed");
    let id = new_id();
    http.set(&config, &id).await;
    let op = http.settled(&id, "completed").await;
    assert_eq!(op["owner_kind"], "qq");
    assert_eq!(control.counts().sets, 1);
    let replay = http
        .post(
            &format!(
                "/api/v1/qq/credentials/{}",
                config["config_id"].as_str().unwrap()
            ),
            json!({"id":id,"expected_revision":config["revision"],"secret":false}),
        )
        .await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["no_secret_applied"], true);
    assert_eq!(control.counts().sets, 1);
    let state = http.snapshot().await;
    assert_eq!(state["configuration"]["credential_state"], "configured");
    let enable=http.post("/api/v1/qq/commands",json!({"id":new_id(),"command":{"type":"configure","config_id":config["config_id"],"expected_revision":state["configuration"]["revision"],"app_id":"app_1","enabled":true,"reply_policy":{"window_ms":300000,"max_user":4,"max_group":5}}})).await;
    assert_eq!(enable["status"], "completed");
    let pair_id = new_id();
    let pair = http
        .post(
            "/api/v1/qq/commands",
            json!({"id":pair_id,"command":{"type":"issue_pairing","ttl_seconds":300}}),
        )
        .await;
    assert!(pair["pairing"]["code"].is_string());
    let receipt = http
        .request(
            reqwest::Method::GET,
            &format!("/api/v1/qq/commands/{pair_id}"),
            None,
        )
        .await;
    assert!(receipt["pairing"]["code"].is_null());
    assert!(!http
        .snapshot()
        .await
        .to_string()
        .contains(pair["pairing"]["code"].as_str().unwrap()));
    let gate = control.block_next_after_effect();
    let delete = new_id();
    http.request(
        reqwest::Method::DELETE,
        &format!(
            "/api/v1/qq/credentials/{}",
            config["config_id"].as_str().unwrap()
        ),
        Some(json!({"id":delete,"expected_revision":enable["revision"]})),
    )
    .await;
    assert!(gate.entered().await);
    let snapshot = http.snapshot().await;
    assert_eq!(snapshot["configuration"]["enabled"], false);
    assert_eq!(snapshot["configuration"]["credential_state"], "missing");
    gate.release();
    http.settled(&delete, "completed").await;
    assert_eq!(control.counts().deletes, 1);
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_set_finishes_once_and_changed_config_creates_cleanup_record() {
    let (http, control) = fixture().await;
    let config = http.config().await;
    let gate = control.block_next_after_effect();
    let id = new_id();
    let body = json!({"id":id,"expected_revision":config["revision"],"secret":SECRET}).to_string();
    let authority = http.base.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(authority).await.unwrap();
    let request=format!("POST /api/v1/qq/credentials/{} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {OWNER}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",config["config_id"].as_str().unwrap(),body.len());
    socket.write_all(request.as_bytes()).await.unwrap();
    assert!(gate.entered().await);
    drop(socket);
    let changed=http.post("/api/v1/qq/commands",json!({"id":new_id(),"command":{"type":"configure","config_id":config["config_id"],"expected_revision":config["revision"],"app_id":"app_2","enabled":false,"reply_policy":{"window_ms":300000,"max_user":4,"max_group":5}}})).await;
    assert_eq!(changed["status"], "completed");
    gate.release();
    let op = http.settled(&id, "orphaned").await;
    assert_eq!(op["can_cleanup"], true);
    assert_eq!(control.counts().sets, 1);
    assert_eq!(
        http.snapshot().await["configuration"]["credential_state"],
        "missing"
    );
    let cleanup = new_id();
    http.post(
        &format!("/api/v1/qq/credentials/cleanup/{id}"),
        json!({"id":cleanup}),
    )
    .await;
    http.settled(&cleanup, "completed").await;
    assert_eq!(control.entry_count(), 0);
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_survives_restart_and_cleanup_does_not_rewrite_history() {
    let (http, control) = fixture().await;
    let config = http.config().await;
    control.reject_next(VaultReason::Unavailable, true);
    let id = new_id();
    http.set(&config, &id).await;
    let op = http.settled(&id, "unknown").await;
    assert_eq!(op["can_cleanup"], true);
    let root = http.stop().await;
    let http = Http::open(root, control.open(8).unwrap()).await;
    let op = http.settled(&id, "unknown").await;
    assert_eq!(control.counts().sets, 1);
    assert_eq!(op["owner_kind"], "qq");
    let cleanup = new_id();
    http.post(
        &format!("/api/v1/qq/credentials/cleanup/{id}"),
        json!({"id":cleanup}),
    )
    .await;
    http.settled(&cleanup, "completed").await;
    let original = http.settled(&id, "unknown").await;
    assert_eq!(original["cleanup_status"], "completed");
    assert_eq!(original["can_cleanup"], false);
    assert_eq!(control.counts().sets, 1);
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_rejects_new_intents_but_awaits_original_vault_settlement() {
    use crate::reliability::lifecycle_runtime::ShutdownOutcome;
    let (http, control) = fixture().await;
    let config = http.config().await;
    let gate = control.block_next_after_effect();
    let id = new_id();
    http.set(&config, &id).await;
    assert!(gate.entered().await);
    http.state.request_shutdown().unwrap();
    let observed = http
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_millis(80))
        .await;
    assert!(observed.deadline_reached);
    assert_ne!(observed.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    let rejected = http
        .client
        .post(format!("{}/api/v1/qq/commands", http.base))
        .bearer_auth(OWNER)
        .json(&json!({"id":new_id(),"command":{"type":"issue_pairing","ttl_seconds":60}}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    let new_id = new_id();
    let rejected = http
        .client
        .post(format!(
            "{}/api/v1/qq/credentials/{}",
            http.base,
            config["config_id"].as_str().unwrap()
        ))
        .bearer_auth(OWNER)
        .json(&json!({"id":new_id,"expected_revision":config["revision"],"secret":SECRET}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(http
        .state
        .store
        .lock()
        .unwrap()
        .qq_credential_operation(&new_id)
        .unwrap()
        .is_none());
    assert_eq!(control.counts().sets, 1);
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let completed = http
                .state
                .store
                .lock()
                .unwrap()
                .qq_credential_operation(&id)
                .unwrap()
                .is_some_and(|op| {
                    matches!(
                        serde_json::to_value(op).unwrap()["status"].as_str(),
                        Some("completed" | "unknown")
                    )
                });
            if completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let denied_read = http
        .client
        .get(format!(
            "{}/api/v1/qq/credential-operations/{id}",
            http.base
        ))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap();
    assert_eq!(denied_read.status(), StatusCode::SERVICE_UNAVAILABLE);
    let observed = http
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_secs(3))
        .await;
    assert_eq!(observed.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_synchronous_admin_transaction_does_not_poison_shutdown() {
    let (http, _) = fixture().await;
    let response = http
        .client
        .post(format!("{}/api/v1/qq/commands", http.base))
        .bearer_auth(OWNER)
        .json(&json!({"id":"not-uuid","command":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(http.state.lifecycle.snapshot().workers.accepting_work);
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)] // Deliberately holds Store to force the shutdown admission race.
async fn preadmitted_qq_mutation_waiting_for_store_cannot_commit_after_drain() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (http, _) = fixture().await;
    let entered = Arc::new(AtomicBool::new(false));
    let store = http.state.store.lock().unwrap();
    let state = http.state.clone();
    let ran = entered.clone();
    let runtime = tokio::runtime::Handle::current();
    let worker = std::thread::spawn(move || {
        let _runtime = runtime.enter();
        begin(&state, |_| {
            ran.store(true, Ordering::Release);
            Ok(json!({"unexpected":true}))
        })
        .map(|(_, admission)| admission.settle())
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while http.state.lifecycle.snapshot().workers.active_admissions == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    http.state.request_shutdown().unwrap();
    drop(store);
    assert!(worker.join().unwrap().is_err());
    assert!(!entered.load(Ordering::Acquire));
    assert_eq!(
        http.state.lifecycle.snapshot().workers.unknown_admissions,
        0
    );
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qq_sql_intent_failure_keeps_armed_unknown_and_never_starts_os_effect() {
    use crate::reliability::lifecycle_runtime::ShutdownOutcome;
    let (http, control) = fixture().await;
    let config = http.config().await;
    let db = rusqlite::Connection::open(http.root.join("state.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER qq_fixture_reject_intent BEFORE INSERT ON qq_credential_operations BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
    let id = new_id();
    let response = http
        .client
        .post(format!(
            "{}/api/v1/qq/credentials/{}",
            http.base,
            config["config_id"].as_str().unwrap()
        ))
        .bearer_auth(OWNER)
        .json(&json!({"id":id,"expected_revision":config["revision"],"secret":SECRET}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(control.counts().sets, 0);
    assert!(http
        .state
        .store
        .lock()
        .unwrap()
        .qq_credential_operation(&id)
        .unwrap()
        .is_none());
    assert_eq!(
        http.state.lifecycle.snapshot().workers.unknown_admissions,
        1
    );
    http.state.request_shutdown().unwrap();
    let observed = http
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_secs(3))
        .await;
    assert_eq!(observed.snapshot.outcome, Some(ShutdownOutcome::Unknown));
    drop(db);
    let _ = http.shutdown.send(());
    http.server.await.unwrap();
    assert!(!http.state.shutdown_credentials().await);
    drop(http.client);
    drop(http.state);
    clean(http.root);
}
