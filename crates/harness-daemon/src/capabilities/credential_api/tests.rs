//! Real local HTTP + Native + stdio, but an in-memory vault only. The fixture
//! secret includes JSON-sensitive and Unicode bytes to exercise semantic filters.
use super::*;
use crate::capabilities::vault::{test_support, VaultReason};
use axum::{extract::State, http::HeaderMap, routing::post};
use harness_core::{new_id, PermissionMode};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};
const OWNER: &str = "fixture-mcp-credential-owner";
const SECRET: &str = "fixture-env-\"私有\\key\n尾";
const PROVIDER: &str = "fixture-model-key-not-mcp";
struct Fixture {
    root: PathBuf,
    state: Arc<AppState>,
    mock: test_support::MockVaultControl,
    client: reqwest::Client,
    base: String,
    http: tokio::task::JoinHandle<()>,
}
fn python() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .flat_map(|p| {
            if cfg!(windows) {
                vec![p.join("python.exe"), p.join("python3.exe")]
            } else {
                vec![p.join("python3"), p.join("python")]
            }
        })
        .find(|p| p.is_file())
        .expect("official CI Python must be installed")
        .canonicalize()
        .unwrap()
}
fn no_secret(value: &Value) {
    let text = value.to_string();
    assert!(
        !text.contains("fixture-env-"),
        "encoded fixture credential leaked"
    );
    assert!(!text.contains(SECRET));
    let escaped = serde_json::to_string(SECRET).unwrap();
    assert!(!text.contains(&escaped[1..escaped.len() - 1]));
    assert!(!text.contains("self-agent.vault.v1:"));
}
impl Fixture {
    async fn open() -> Self {
        let root = std::env::temp_dir().join(format!("mcp-cred-{}", new_id()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let (vault, mock) = test_support::memory(8).unwrap();
        let state =
            AppState::open_with_vault(&root.join("state.sqlite"), OWNER.into(), vault).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::router(state.clone());
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            root,
            state,
            mock,
            client,
            base,
            http,
        }
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        let r = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(OWNER)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "HTTP {}", r.status());
        let v = r.json().await.unwrap();
        no_secret(&v);
        v
    }
    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn server(&self) -> String {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/mcp_fixture.py")
            .canonicalize()
            .unwrap();
        self.post("/api/v1/capabilities/commands",json!({"id":new_id(),"command":{"type":"save_mcp_server","definition":{"name":"私有环境夹具","program":python(),"args":["-I",script,"credential-json-text"],"credential_slots":["MCP_TOKEN"],"credential_refs":{},"enabled":false,"protocol":"modern2026"}}})).await["capability_id"].as_str().unwrap().into()
    }
    async fn operation(&self, id: &str, status: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let v = self
                    .get(&format!("/api/v1/capability-credential-operations/{id}"))
                    .await;
                no_secret(&v);
                if v["status"] == status {
                    return v;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("credential operation settles")
    }
    async fn server_view(&self, id: &str, status: Option<&str>) -> Value {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let registry = self.get("/api/v1/capabilities").await;
                no_secret(&registry);
                let v = registry["mcp_servers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["id"] == id)
                    .unwrap()
                    .clone();
                if status.is_none_or(|status| v["status"] == status) {
                    return v;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP state settles")
    }
    async fn set(&self, id: &str, revision: u64, secret: &str) -> String {
        let op = new_id();
        self.post(
            &format!("/api/v1/capabilities/mcp/{id}/credentials/MCP_TOKEN"),
            json!({"id":op,"expected_revision":revision,"secret":secret}),
        )
        .await;
        self.operation(&op, "completed").await;
        op
    }
    async fn ready(&self, id: &str) {
        let rev = self.server_view(id, None).await["revision"]
            .as_u64()
            .unwrap();
        self.post("/api/v1/capabilities/commands",json!({"id":new_id(),"command":{"type":"probe_mcp_server","id":id,"expected_revision":rev,"confirm_start":true}})).await;
        let rev = self.server_view(id, Some("ready")).await["revision"]
            .as_u64()
            .unwrap();
        self.post("/api/v1/capabilities/commands",json!({"id":new_id(),"command":{"type":"set_capability_enabled","kind":"mcp","id":id,"expected_revision":rev,"enabled":true}})).await;
    }
    fn enqueue(&self, id: &str) -> String {
        let project = self.root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("secret.txt"), SECRET).unwrap();
        let mut settings = self.state.settings.write().unwrap();
        settings.execution_controls.capabilities.mcp_servers = vec![id.into()];
        settings.execution_controls.permissions.managed_tools = PermissionMode::Allow;
        let settings = settings.clone();
        let source = json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"fixture"});
        let mut store = self.state.store.lock().unwrap();
        let session=store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"create_session","project_path":project,"title":"私有环境夹具"}}),&settings).unwrap().0;
        store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"run_agent","session_id":session["session_id"],"content":"使用两次已选工具并回复"}}),&settings).unwrap().0["task_id"].as_str().unwrap().into()
    }
    async fn close(self) {
        self.http.abort();
        let _ = self.http.await;
        assert!(self.state.shutdown_credentials().await);
        drop(self.client);
        drop(self.state);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}
struct Model {
    calls: AtomicUsize,
    seen: Mutex<Vec<Value>>,
}
async fn model(
    State(state): State<Arc<Model>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(
        headers.get("authorization").unwrap(),
        format!("Bearer {PROVIDER}").as_str()
    );
    no_secret(&body);
    state.seen.lock().unwrap().push(body.clone());
    let n = state.calls.fetch_add(1, Ordering::SeqCst);
    if n < 2 {
        let name = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|t| {
                t["function"]["name"]
                    .as_str()
                    .filter(|n| n.starts_with("mcp_"))
            })
            .unwrap();
        let content = "本地夹具工具调用";
        Json(
            json!({"choices":[{"message":{"role":"assistant","content":content,"tool_calls":[{"id":format!("mcp-{n}"),"type":"function","function":{"name":name,"arguments":"{\"text\":\"本地环境验证\"}"}}]},"finish_reason":"tool_calls"}]}),
        )
    } else {
        assert!(body.to_string().contains("凭证已隐藏"));
        Json(
            json!({"choices":[{"message":{"role":"assistant","content":"已完成两次本地工具验证"},"finish_reason":"stop"}]}),
        )
    }
}
async fn start_model(f: &Fixture) -> (Arc<Model>, tokio::task::JoinHandle<()>) {
    let model_state = Arc::new(Model {
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(model))
        .with_state(model_state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut settings = f.state.settings.write().unwrap();
    settings.base_url = base;
    settings.model = "fixture".into();
    settings.api_key = PROVIDER.into();
    (model_state, server)
}
#[tokio::test]
async fn owner_api_to_native_stdio_reads_each_slot_once_and_redacts_every_public_boundary() {
    let f = Fixture::open().await;
    let id = f.server().await;
    let op = f.set(&id, 1, SECRET).await;
    let replay = f
        .post(
            &format!("/api/v1/capabilities/mcp/{id}/credentials/MCP_TOKEN"),
            json!({"id":op,"expected_revision":1,"secret":"different-new-secret"}),
        )
        .await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["no_secret_applied"], true);
    assert_eq!(f.mock.counts().sets, 1);
    f.ready(&id).await;
    let before = f.mock.counts().gets;
    let (model, server) = start_model(&f).await;
    let task_id = f.enqueue(&id);
    let task = f.state.store.lock().unwrap().claim_task().unwrap().unwrap();
    tokio::time::timeout(
        Duration::from_secs(25),
        crate::run_task(f.state.clone(), task),
    )
    .await
    .unwrap();
    let snapshot = f.state.store.lock().unwrap().snapshot().unwrap();
    let task = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == task_id)
        .unwrap();
    assert_eq!(task["status"], "completed", "{task}");
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    assert_eq!(f.mock.counts().gets, before + 1);
    no_secret(&snapshot);
    no_secret(&f.state.store.lock().unwrap().changes(&task_id).unwrap());
    let trace = std::fs::read_to_string(f.root.join("project/mcp-trace.jsonl")).unwrap();
    assert_eq!(trace.matches("tools/call").count(), 2);
    for body in model.seen.lock().unwrap().iter() {
        no_secret(body);
    }
    for name in ["state.sqlite", "state.sqlite-wal"] {
        if let Ok(bytes) = std::fs::read(f.root.join(name)) {
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("fixture-env-"),
                "encoded credential persisted"
            );
            assert!(!text.contains(SECRET));
            assert!(!text.contains(
                &serde_json::to_string(SECRET).unwrap()
                    [1..serde_json::to_string(SECRET).unwrap().len() - 1]
            ));
        }
    }
    server.abort();
    let _ = server.await;
    f.close().await;
}
#[tokio::test]
async fn missing_or_rejected_slot_never_starts_a_process_and_has_no_provider_fallback() {
    let f = Fixture::open().await;
    let id = f.server().await;
    f.post("/api/v1/capabilities/commands",json!({"id":new_id(),"command":{"type":"probe_mcp_server","id":id,"expected_revision":1,"confirm_start":true}})).await;
    f.server_view(&id, Some("unavailable")).await;
    assert_eq!(f.mock.counts().gets, 0);
    assert!(!f.state.capability_probe_root.join("mcp-pid").exists());
    let revision = f.server_view(&id, None).await["revision"].as_u64().unwrap();
    f.set(&id, revision, SECRET).await;
    f.ready(&id).await;
    let (model, server) = start_model(&f).await;
    let task_id = f.enqueue(&id);
    let task = f.state.store.lock().unwrap().claim_task().unwrap().unwrap();
    f.mock.reject_next(VaultReason::Missing, false);
    crate::run_task(f.state.clone(), task).await;
    let snapshot = f.state.store.lock().unwrap().snapshot().unwrap();
    assert_eq!(
        snapshot["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == task_id)
            .unwrap()["status"],
        "failed"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert!(!f.root.join("project/mcp-pid").exists());
    server.abort();
    let _ = server.await;
    f.close().await;
}
#[tokio::test]
async fn revoke_during_vault_read_prevents_model_and_process_and_never_rebinds() {
    let f = Fixture::open().await;
    let id = f.server().await;
    f.set(&id, 1, SECRET).await;
    f.ready(&id).await;
    let (model, server) = start_model(&f).await;
    let task_id = f.enqueue(&id);
    let task = f.state.store.lock().unwrap().claim_task().unwrap().unwrap();
    let gate = f.mock.block_next_after_effect();
    let running = tokio::spawn(crate::run_task(f.state.clone(), task));
    assert!(gate.entered().await);
    let revision = f.server_view(&id, None).await["revision"].as_u64().unwrap();
    f.post("/api/v1/capabilities/commands",json!({"id":new_id(),"command":{"type":"set_capability_enabled","kind":"mcp","id":id,"expected_revision":revision,"enabled":false}})).await;
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    gate.release();
    let snapshot = f.state.store.lock().unwrap().snapshot().unwrap();
    let task = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == task_id)
        .unwrap();
    assert_eq!(task["status"], "failed");
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert!(!f.root.join("project/mcp-pid").exists());
    server.abort();
    let _ = server.await;
    f.close().await;
}
#[tokio::test]
async fn credential_routes_are_owner_only_and_invalid_slots_do_not_touch_vault() {
    let f = Fixture::open().await;
    let id = f.server().await;
    let body = json!({"id":new_id(),"expected_revision":1,"secret":SECRET});
    let path = format!(
        "{}/api/v1/capabilities/mcp/{id}/credentials/MCP_TOKEN",
        f.base
    );
    let denied = f.client.post(&path).json(&body).send().await.unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    for slot in [
        "PATH",
        "UNDECLARED_TOKEN",
        "PSMODULEPATH",
        "CORECLR_ENABLE_PROFILING",
        "GLIBC_TUNABLES",
        "LOCPATH",
        "NLSPATH",
        "BASH_XTRACEFD",
        "PROMPT_COMMAND",
        "XPROXY",
    ] {
        let r = f
            .client
            .post(format!(
                "{}/api/v1/capabilities/mcp/{id}/credentials/{slot}",
                f.base
            ))
            .bearer_auth(OWNER)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(!r.text().await.unwrap().contains(SECRET));
    }
    let view = f.server_view(&id, None).await;
    for slot in [
        "PATH",
        "PSMODULEPATH",
        "CORECLR_ENABLE_PROFILING",
        "GLIBC_TUNABLES",
        "LOCPATH",
        "NLSPATH",
        "BASH_XTRACEFD",
        "PROMPT_COMMAND",
        "XPROXY",
    ] {
        let response = f.client.post(format!("{}/api/v1/capabilities/commands", f.base)).bearer_auth(OWNER)
            .json(&json!({"id":new_id(),"command":{"type":"save_mcp_server","id":id,"expected_revision":view["revision"],"definition":{"name":"危险槽拒绝夹具","program":view["program"],"args":view["args"],"credential_slots":[slot],"credential_refs":{},"enabled":false,"protocol":"modern2026"}}}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{slot}");
    }
    assert_eq!(f.mock.counts().sets, 0);
    f.close().await;
}

#[tokio::test]
async fn unknown_set_and_delete_are_read_only_recoverable_and_cleanup_preserves_original_outcome() {
    let f = Fixture::open().await;
    let id = f.server().await;
    for kind in ["set", "delete"] {
        let revision = f.server_view(&id, None).await["revision"].as_u64().unwrap();
        if kind == "delete" {
            f.set(&id, revision, SECRET).await;
        }
        let revision = f.server_view(&id, None).await["revision"].as_u64().unwrap();
        let op = new_id();
        f.mock.reject_next(VaultReason::Unexpected, true);
        if kind == "set" {
            f.post(
                &format!("/api/v1/capabilities/mcp/{id}/credentials/MCP_TOKEN"),
                json!({"id":op,"expected_revision":revision,"secret":SECRET}),
            )
            .await;
        } else {
            f.post(
                &format!("/api/v1/capabilities/mcp/{id}/credentials/MCP_TOKEN/delete"),
                json!({"id":op,"expected_revision":revision}),
            )
            .await;
        }
        let original = f.operation(&op, "unknown").await;
        assert_eq!(original["can_cleanup"], true);
        let before = f.mock.counts();
        for _ in 0..2 {
            assert_eq!(
                f.get(&format!("/api/v1/capability-credential-operations/{op}"))
                    .await["status"],
                "unknown"
            );
        }
        assert_eq!(before, f.mock.counts());
        let cleanup = new_id();
        f.post(
            &format!("/api/v1/capability-credential-operations/{op}/cleanup"),
            json!({"id":cleanup}),
        )
        .await;
        f.operation(&cleanup, "completed").await;
        let after = f
            .get(&format!("/api/v1/capability-credential-operations/{op}"))
            .await;
        assert_eq!(after["status"], "unknown");
        assert_eq!(after["reason"], original["reason"]);
        assert_eq!(after["cleanup_status"], "completed");
        assert_eq!(after["can_cleanup"], false);
    }
    f.close().await;
}

#[tokio::test]
async fn enrollment_rejects_a_key_already_present_in_nested_public_configuration() {
    let f = Fixture::open().await;
    let id = f.server().await;
    // Owner-supplied argv is metadata, not a supported secret input. Enrollment
    // must not legitimize a string that has already appeared in that metadata.
    let view = f.server_view(&id, None).await;
    let mut args = view["args"].as_array().unwrap().clone();
    args.push(json!({"token":SECRET,SECRET:SECRET}).to_string().into());
    f.post("/api/v1/capabilities/commands",json!({"id":new_id(),"command":{"type":"save_mcp_server","id":id,"expected_revision":view["revision"],"definition":{"name":"私有环境夹具","program":view["program"],"args":args,"credential_slots":["MCP_TOKEN"],"credential_refs":{},"enabled":false,"protocol":"modern2026"}}})).await;
    let revision = f
        .state
        .store
        .lock()
        .unwrap()
        .mcp_credential_record(&id)
        .unwrap()
        .revision;
    let response = f
        .client
        .post(format!(
            "{}/api/v1/capabilities/mcp/{id}/credentials/MCP_TOKEN",
            f.base
        ))
        .bearer_auth(OWNER)
        .json(&json!({"id":new_id(),"expected_revision":revision,"secret":SECRET}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.text().await.unwrap().contains(SECRET));
    assert_eq!(f.mock.counts().sets, 0);
    f.close().await;
}
