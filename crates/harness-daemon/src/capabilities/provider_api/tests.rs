//! Real owner-authenticated HTTP routes and the cfg(test)-only memory vault.
//! No operating-system credential service or provider endpoint is contacted.
use crate::{
    capabilities::vault::{test_support, ShutdownStatus, Vault, VaultReason},
    AppState,
};
use harness_core::new_id;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

const TOKEN: &str = "fixture-provider-api-owner-token";
const KEY: &str = "fixture-provider-api-secret-canary";
const CHANGED: &str = "fixture-provider-api-changed-secret-canary";

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("provider-api-{}", new_id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let removed = std::fs::remove_dir_all(&self.0);
        if !std::thread::panicking() {
            removed.expect("fixture database handles must close before cleanup");
        }
    }
}
struct Http {
    state: Arc<AppState>,
    client: reqwest::Client,
    base: String,
    server: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}
impl Http {
    async fn open(root: &Root, vault: Arc<Vault>) -> Self {
        let state =
            AppState::open_with_vault(&root.0.join("state.sqlite"), TOKEN.into(), vault).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::router(state.clone());
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
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
    async fn post(&self, path: &str, body: Value) -> Value {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "fixture POST rejected: {}",
            response.status()
        );
        response.json().await.unwrap()
    }
    async fn registry(&self) -> Value {
        self.client
            .get(format!("{}/api/v1/providers", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn provider(&self) -> String {
        let receipt = self.post("/api/v1/providers/commands", json!({"id":new_id(),"command":{"type":"save_provider","definition":{"name":"仅内存凭据夹具","base_url":"https://provider-api.example.invalid/v1","model":"fixture-model"}}})).await;
        assert_eq!(receipt["status"], "completed");
        assert_eq!(receipt["revision"], 1);
        receipt["provider_id"].as_str().unwrap().to_owned()
    }
    async fn set(&self, provider: &str, operation: &str, revision: u64, secret: &str) -> Value {
        self.post(
            &format!("/api/v1/providers/{provider}/credential"),
            json!({"id":operation,"expected_revision":revision,"secret":secret}),
        )
        .await
    }
    async fn wait_operation(&self, operation: &str, status: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let op: Value = self
                    .client
                    .get(format!(
                        "{}/api/v1/credential-operations/{operation}",
                        self.base
                    ))
                    .bearer_auth(TOKEN)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                absent(&op.to_string());
                assert!(!op.to_string().contains("self-agent.vault.v1:"));
                if op["id"] == operation && op["status"] == status {
                    return op;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fixture credential operation did not settle")
    }
    async fn stop(self) {
        drop(self.client);
        let _ = self.shutdown.send(());
        tokio::time::timeout(Duration::from_secs(3), self.server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            self.state
                .vault
                .shutdown(Instant::now() + Duration::from_secs(3))
                .await,
            ShutdownStatus::Stopped
        );
        // Explicitly release router/AppState/SQLite handles before Root cleanup,
        // including on Windows where an open database cannot be unlinked.
        drop(self.state);
    }
}
fn absent(text: &str) {
    for secret in [KEY, CHANGED] {
        assert!(
            !text.contains(secret),
            "private credential escaped fixture boundary"
        );
        let digest = format!("{:x}", Sha256::digest(secret.as_bytes()));
        assert!(
            !text.contains(&digest),
            "credential-derived fingerprint escaped fixture boundary"
        );
    }
}
async fn assert_private(http: &Http, root: &Root, receipts: &[Value]) {
    let registry = http.registry().await;
    absent(&registry.to_string());
    assert!(!registry.to_string().contains("self-agent.vault.v1:"));
    absent(
        &http
            .state
            .store
            .lock()
            .unwrap()
            .snapshot()
            .unwrap()
            .to_string(),
    );
    for receipt in receipts {
        absent(&receipt.to_string());
        assert!(!receipt.to_string().contains("self-agent.vault.v1:"));
    }
    // SQLite may retain prior text in WAL/free pages, so inspect actual bytes,
    // not just the currently selected public rows.
    for entry in std::fs::read_dir(&root.0).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            absent(&String::from_utf8_lossy(&std::fs::read(path).unwrap()));
        }
    }
}

#[tokio::test]
async fn confirmed_key_and_changed_body_replay_are_safe_through_real_router() {
    let root = Root::new();
    let (vault, mock) = test_support::memory(8).unwrap();
    let http = Http::open(&root, vault).await;
    let mut events = http.state.events.subscribe();
    let provider = http.provider().await;
    let operation = new_id();
    let gate = mock.block_next_after_effect();
    let first = http.set(&provider, &operation, 1, KEY).await;
    assert_eq!(first["no_secret_applied"], false);
    assert!(gate.entered().await);
    assert_eq!(mock.counts().sets, 1);
    assert_eq!(mock.entry_count(), 1);
    assert_eq!(
        http.registry().await["providers"][0]["credential_configured"],
        false
    );
    let repeated = http.set(&provider, &operation, 1, CHANGED).await;
    assert_eq!(repeated["no_secret_applied"], true);
    assert_eq!(repeated["operation"]["status"], "inflight");
    gate.release();
    http.wait_operation(&operation, "completed").await;
    let registry = http.registry().await;
    assert_eq!(registry["providers"][0]["credential_configured"], true);
    assert_eq!(
        registry["providers"][0]["credential_status"],
        "configured_not_checked"
    );
    assert_eq!(registry["providers"][0]["revision"], 2);
    let replay = http.set(&provider, &operation, 1, CHANGED).await;
    assert_eq!(replay["no_secret_applied"], true);
    assert_eq!(mock.counts().sets, 1);
    assert_private(&http, &root, &[first, repeated, replay]).await;
    while let Ok(event) = events.try_recv() {
        absent(&serde_json::to_string(&event).unwrap());
    }
    http.stop().await;
}

#[tokio::test]
async fn rejected_and_unknown_sets_never_fall_back_or_replay_after_restart() {
    for uncertain in [false, true] {
        let root = Root::new();
        let (vault, mock) = test_support::memory(8).unwrap();
        let http = Http::open(&root, vault).await;
        let provider = http.provider().await;
        let operation = new_id();
        mock.reject_next(VaultReason::AccessDeniedOrLocked, uncertain);
        let first = http.set(&provider, &operation, 1, KEY).await;
        let status = if uncertain { "unknown" } else { "rejected" };
        let settled = http.wait_operation(&operation, status).await;
        assert_eq!(settled["can_cleanup"], uncertain);
        assert_eq!(
            http.registry().await["providers"][0]["credential_configured"],
            false
        );
        assert_eq!(mock.counts().sets, 1);
        assert_eq!(mock.entry_count(), 0);
        assert_private(&http, &root, &[first]).await;
        http.stop().await;
        let restarted = Http::open(&root, mock.open(8).unwrap()).await;
        restarted.wait_operation(&operation, status).await;
        let replay = restarted.set(&provider, &operation, 1, CHANGED).await;
        assert_eq!(replay["no_secret_applied"], true);
        assert_eq!(mock.counts().sets, 1);
        assert_eq!(mock.counts().gets, 0);
        assert_private(&restarted, &root, &[replay]).await;
        restarted.stop().await;
    }
}

#[tokio::test]
async fn disconnect_after_claim_cannot_cancel_a_vault_write() {
    let root = Root::new();
    let (vault, mock) = test_support::memory(8).unwrap();
    let http = Http::open(&root, vault).await;
    let provider = http.provider().await;
    let operation = new_id();
    let gate = mock.block_next_after_effect();
    let address = http.base.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let body = json!({"id":operation,"expected_revision":1,"secret":KEY}).to_string();
    let request = format!("POST /api/v1/providers/{provider}/credential HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    socket.write_all(request.as_bytes()).await.unwrap();
    assert!(gate.entered().await);
    // Never read the response. Closing this real TCP connection cannot retract
    // a durable claimed operation or cause an automatic second OS write.
    drop(socket);
    http.wait_operation(&operation, "inflight").await;
    gate.release();
    http.wait_operation(&operation, "completed").await;
    assert_eq!(
        http.registry().await["providers"][0]["credential_configured"],
        true
    );
    assert_eq!(mock.counts().sets, 1);
    assert_private(&http, &root, &[]).await;
    http.stop().await;
}

#[tokio::test]
async fn deletion_detaches_before_os_confirmation_and_unknown_is_not_replayed() {
    for uncertain in [false, true] {
        let root = Root::new();
        let (vault, mock) = test_support::memory(8).unwrap();
        let http = Http::open(&root, vault).await;
        let provider = http.provider().await;
        let enrollment = new_id();
        http.set(&provider, &enrollment, 1, KEY).await;
        http.wait_operation(&enrollment, "completed").await;
        let operation = new_id();
        let gate = if uncertain {
            mock.reject_next(VaultReason::Unavailable, true);
            None
        } else {
            Some(mock.block_next_after_effect())
        };
        let body = json!({"id":operation,"expected_revision":2});
        let path = format!("/api/v1/providers/{provider}/credential-delete");
        let receipt = http.post(&path, body.clone()).await;
        if let Some(gate) = &gate {
            assert!(gate.entered().await);
        }
        let registry = http.registry().await;
        assert_eq!(registry["providers"][0]["credential_configured"], false);
        assert_eq!(registry["providers"][0]["revision"], 3);
        if let Some(gate) = gate {
            gate.release();
        }
        let status = if uncertain { "unknown" } else { "completed" };
        http.wait_operation(&operation, status).await;
        assert_eq!(mock.counts().deletes, 1);
        assert_eq!(mock.entry_count(), usize::from(uncertain));
        assert_private(&http, &root, &[receipt]).await;
        http.stop().await;
        let restarted = Http::open(&root, mock.open(8).unwrap()).await;
        let replay = restarted.post(&path, body).await;
        assert_eq!(replay["no_secret_applied"], true);
        assert_eq!(replay["operation"]["status"], status);
        assert_eq!(
            restarted.registry().await["providers"][0]["credential_configured"],
            false
        );
        assert_eq!(mock.counts().deletes, 1);
        assert_private(&restarted, &root, &[replay]).await;
        if uncertain {
            // An unknown delete remains detached; only a new explicit owner
            // cleanup command may attempt its orphaned credential again.
            let cleanup = new_id();
            let cleanup_path = format!("/api/v1/credential-operations/{operation}/cleanup");
            let first = restarted.post(&cleanup_path, json!({"id":cleanup})).await;
            restarted.wait_operation(&cleanup, "completed").await;
            let replay = restarted.post(&cleanup_path, json!({"id":cleanup})).await;
            assert_eq!(replay["no_secret_applied"], true);
            assert_eq!(mock.counts().deletes, 2);
            assert_eq!(mock.entry_count(), 0);
            let original = restarted.wait_operation(&operation, "unknown").await;
            assert_eq!(original["cleanup_status"], "completed");
            assert_eq!(original["reason"], "vault_outcome_unknown");
            assert_eq!(original["can_cleanup"], false);
            let original_replay = restarted
                .post(&path, json!({"id":operation,"expected_revision":2}))
                .await;
            assert_eq!(original_replay["operation"]["status"], "unknown");
            assert_eq!(original_replay["no_secret_applied"], true);
            assert_private(&restarted, &root, &[first, replay, original_replay]).await;
        }
        restarted.stop().await;
    }
}

#[tokio::test]
async fn owner_authorization_is_required_before_accepting_secret_material() {
    let root = Root::new();
    let (vault, mock) = test_support::memory(8).unwrap();
    let http = Http::open(&root, vault).await;
    let provider = http.provider().await;
    let response = http
        .client
        .post(format!(
            "{}/api/v1/providers/{provider}/credential",
            http.base
        ))
        .json(&json!({"id":new_id(),"expected_revision":1,"secret":KEY}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    absent(&response.text().await.unwrap());
    assert_eq!(mock.counts().sets, 0);
    assert!(http.registry().await["credential_operations"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_private(&http, &root, &[]).await;
    http.stop().await;
}
