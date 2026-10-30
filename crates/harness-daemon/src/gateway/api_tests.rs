//! Actual authenticated HTTP routes plus an in-memory OS-vault protocol fixture.
//! No production Gateway or operating-system credential service is contacted.
use super::*;
use crate::capabilities::vault::{
    test_support::{self, MockVaultControl},
    Vault, VaultReason,
};
use harness_core::new_id;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
const OWNER: &str = "gateway-api-fixture-owner-token";
const SECRET: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
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
        let root = root.canonicalize().unwrap();
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
            "Gateway fixture route status {}",
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
        self.request(reqwest::Method::GET, "/api/v1/gateway", None)
            .await
    }
    async fn config(&self) -> Value {
        self.post("/api/v1/gateway/commands",json!({"id":new_id(),"command":{"type":"configure","config_id":null,"expected_revision":null,"origin":"https://gateway.example.invalid","device_id":"11111111-1111-4111-8111-111111111111","enabled":false}})).await
    }
    async fn set(&self, config: &Value, id: &str) -> Value {
        self.post(
            &format!(
                "/api/v1/gateway/credentials/{}",
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
                        &format!("/api/v1/gateway/credential-operations/{id}"),
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
        .expect("Gateway fixture operation did not settle")
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
    let root = std::env::temp_dir().join(format!("gateway-api-{}", new_id()));
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
        .get(format!("{}/api/v1/gateway", http.base))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let config = http.config().await;
    assert_eq!(config["status"], "completed");
    let id = new_id();
    http.set(&config, &id).await;
    let op = http.settled(&id, "completed").await;
    assert_eq!(op["owner_kind"], "gateway");
    assert_eq!(control.counts().sets, 1);
    let replay = http
        .post(
            &format!(
                "/api/v1/gateway/credentials/{}",
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
    let enable=http.post("/api/v1/gateway/commands",json!({"id":new_id(),"command":{"type":"configure","config_id":config["config_id"],"expected_revision":state["configuration"]["revision"],"origin":"https://gateway.example.invalid","device_id":"11111111-1111-4111-8111-111111111111","enabled":true}})).await;
    assert_eq!(enable["status"], "completed");
    let pair_id = new_id();
    let pair = http
        .post(
            "/api/v1/gateway/commands",
            json!({"id":pair_id,"command":{"type":"issue_pairing","ttl_seconds":300}}),
        )
        .await;
    assert!(pair["pairing"]["code"].is_string());
    let receipt = http
        .request(
            reqwest::Method::GET,
            &format!("/api/v1/gateway/commands/{pair_id}"),
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
            "/api/v1/gateway/credentials/{}",
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
    let request=format!("POST /api/v1/gateway/credentials/{} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {OWNER}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",config["config_id"].as_str().unwrap(),body.len());
    socket.write_all(request.as_bytes()).await.unwrap();
    assert!(gate.entered().await);
    drop(socket);
    let changed=http.post("/api/v1/gateway/commands",json!({"id":new_id(),"command":{"type":"configure","config_id":config["config_id"],"expected_revision":config["revision"],"origin":"https://other.example.invalid","device_id":"11111111-1111-4111-8111-111111111111","enabled":false}})).await;
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
        &format!("/api/v1/gateway/credentials/cleanup/{id}"),
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
    assert_eq!(op["owner_kind"], "gateway");
    let cleanup = new_id();
    http.post(
        &format!("/api/v1/gateway/credentials/cleanup/{id}"),
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

#[test]
fn gateway_admission_is_bounded_nonwaiting_and_released_only_by_owner() {
    // Dedicated limiter avoids perturbing concurrently running HTTP fixtures.
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_ADMITTED));
    let mut held = Vec::new();
    for _ in 0..MAX_ADMITTED {
        held.push(reserve_from(&limit).unwrap());
    }
    assert_eq!(
        reserve_from(&limit).unwrap_err().0,
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(held.pop());
    let replacement = reserve_from(&limit).unwrap();
    assert_eq!(limit.available_permits(), 0);
    drop(replacement);
    drop(held);
    assert_eq!(limit.available_permits(), MAX_ADMITTED);
}

#[tokio::test(flavor = "current_thread")]
async fn store_lock_contention_keeps_timer_running_and_disconnected_admitted_set_finishes_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (http, control) = fixture().await;
    let config = http.config().await;
    let id = new_id();
    let (locked, entered) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let held = Arc::new(AtomicBool::new(false));
    let lock_state = http.state.clone();
    let held_worker = held.clone();
    let owner = std::thread::spawn(move || {
        let _lock = lock_state.store.lock().unwrap();
        held_worker.store(true, Ordering::Release);
        locked.send(()).unwrap();
        // Emergency timeout prevents the old blocking implementation hanging
        // the test forever; the held assertion detects that fallback.
        let _ = gate.recv_timeout(Duration::from_secs(5));
        held_worker.store(false, Ordering::Release);
    });
    entered.await.unwrap();
    let state = http.state.clone();
    let read = tokio::spawn(async move { snapshot(State(state)).await });
    let state = http.state.clone();
    let config_id = config["config_id"].as_str().unwrap().to_owned();
    let body = Bytes::from(
        json!({"id":id,"expected_revision":config["revision"],"secret":SECRET}).to_string(),
    );
    let write =
        tokio::spawn(async move { set_credential(State(state), Path(config_id), body).await });
    // Both endpoints are admitted and blocked on the Store, but a single-thread
    // runtime must still advance timers/heartbeat scheduling.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        held.load(Ordering::Acquire),
        "Tokio worker stalled until external lock timed out"
    );
    assert!(!read.is_finished());
    assert!(!write.is_finished());
    write.abort();
    let _ = write.await;
    release.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    owner.join().unwrap();
    let op = http.settled(&id, "completed").await;
    assert_eq!(op["status"], "completed");
    assert_eq!(control.counts().sets, 1);
    // The canceled observer must not cause another OS write on receipt replay.
    let replay = http.set(&config, &id).await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(control.counts().sets, 1);
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_keeps_original_gateway_vault_owner_and_rejects_new_reads_and_mutations() {
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
    for path in [
        "/api/v1/gateway".to_string(),
        format!("/api/v1/gateway/credential-operations/{id}"),
    ] {
        let response = http
            .client
            .get(format!("{}{path}", http.base))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    let rejected_id = new_id();
    let response = http
        .client
        .post(format!(
            "{}/api/v1/gateway/credentials/{}",
            http.base,
            config["config_id"].as_str().unwrap()
        ))
        .bearer_auth(OWNER)
        .json(&json!({"id":rejected_id,"expected_revision":config["revision"],"secret":SECRET}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(http
        .state
        .store
        .lock()
        .unwrap()
        .gateway_credential_operation(&rejected_id)
        .unwrap()
        .is_none());
    assert_eq!(control.counts().sets, 1);
    gate.release();
    let observed = http
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(observed.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    let operation = http
        .state
        .store
        .lock()
        .unwrap()
        .gateway_credential_operation(&id)
        .unwrap()
        .unwrap();
    // Cancellation may record Unknown after the OS effect, but it must retain
    // that one original receipt and never mistake it for unowned physical work.
    let status = serde_json::to_value(operation).unwrap();
    assert!(matches!(
        status["status"].as_str(),
        Some("completed" | "unknown")
    ));
    assert_eq!(control.counts().sets, 1);
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)] // Intentional admission-versus-Store-lock race.
async fn detached_gateway_store_waiter_is_retained_and_cannot_begin_after_drain() {
    use crate::reliability::lifecycle_runtime::ShutdownOutcome;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (http, _) = fixture().await;
    let ran = Arc::new(AtomicBool::new(false));
    let guard = http.state.store.lock().unwrap();
    let state = http.state.clone();
    let did_run = ran.clone();
    let observer = tokio::spawn(async move {
        database(state, move |_| {
            did_run.store(true, Ordering::Release);
            Ok(())
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while http.state.lifecycle.snapshot().workers.pending_workers < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap(); // Vault custodian + admitted database worker.
    observer.abort();
    let _ = observer.await;
    http.state.request_shutdown().unwrap();
    assert!(http.state.lifecycle.snapshot().workers.pending_workers >= 1);
    drop(guard);
    let observed = http
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(observed.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    assert!(!ran.load(Ordering::Acquire));
    clean(http.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_gateway_admin_validation_does_not_poison_shutdown() {
    let (http, _) = fixture().await;
    let response = http
        .client
        .post(format!("{}/api/v1/gateway/commands", http.base))
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
async fn gateway_current_finalization_failure_is_unknown_not_stopped() {
    use crate::reliability::lifecycle_runtime::ShutdownOutcome;
    let (http, control) = fixture().await;
    let config = http.config().await;
    let gate = control.block_next_after_effect();
    let id = new_id();
    http.set(&config, &id).await;
    assert!(gate.entered().await);
    // This fixture injects a real SQLite transaction failure AFTER the one OS
    // effect. The original receipt remains inflight and cannot prove cleanup.
    let db = rusqlite::Connection::open(http.root.join("state.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER gateway_fixture_fail_finish BEFORE UPDATE ON gateway_credential_operations BEGIN SELECT RAISE(ABORT,'fixture finish failed'); END;").unwrap();
    gate.release();
    tokio::time::timeout(Duration::from_secs(5), async {
        while http.state.lifecycle.snapshot().workers.accepting_work {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    http.state.request_shutdown().unwrap();
    let observed = http
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(observed.snapshot.outcome, Some(ShutdownOutcome::Unknown));
    assert_eq!(control.counts().sets, 1);
    let op = http
        .state
        .store
        .lock()
        .unwrap()
        .gateway_credential_operation(&id)
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(op).unwrap()["status"], "inflight");
    drop(db);
    let _ = http.shutdown.send(());
    http.server.await.unwrap();
    assert!(!http.state.shutdown_credentials().await);
    drop(http.client);
    drop(http.state);
    clean(http.root);
}
