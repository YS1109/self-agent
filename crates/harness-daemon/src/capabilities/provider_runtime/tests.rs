use super::*;
use crate::capabilities::vault::{test_support, FreshCredential};
use axum::{
    extract::State,
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use harness_core::new_id;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

const KEY: &str = "fixture-named-provider-secret";
struct Root(std::path::PathBuf);
impl Root {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("named-{}", new_id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p.canonicalize().unwrap())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Model {
    seen: Mutex<Vec<Value>>,
    calls: AtomicUsize,
    blocked: bool,
    started: tokio::sync::Notify,
}
async fn completion(
    State(model): State<Arc<Model>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(
        headers.get("authorization").unwrap(),
        format!("Bearer {KEY}").as_str()
    );
    assert_eq!(body["model"], "profile-model");
    model.seen.lock().unwrap().push(body.clone());
    let turn = model.calls.fetch_add(1, Ordering::SeqCst);
    model.started.notify_one();
    if model.blocked {
        std::future::pending::<()>().await;
    }
    if turn == 0 {
        Json(
            json!({"choices":[{"message":{"role":"assistant","content":KEY,"tool_calls":[{"id":"read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"fixture.txt\"}"}}]},"finish_reason":"tool_calls"}]}),
        )
    } else {
        assert!(!body.to_string().contains(KEY));
        Json(
            json!({"choices":[{"message":{"role":"assistant","content":format!("已读取夹具 {KEY}")},"finish_reason":"stop"}]}),
        )
    }
}
async fn models(headers: HeaderMap) -> Json<Value> {
    assert_eq!(
        headers.get("authorization").unwrap(),
        format!("Bearer {KEY}").as_str()
    );
    Json(json!({"data":[{"id":"profile-model"},{"id":KEY}]}))
}
async fn setup(
    blocked: bool,
) -> (
    Root,
    Arc<AppState>,
    test_support::MockVaultControl,
    String,
    Arc<Model>,
    tokio::task::JoinHandle<()>,
) {
    let root = Root::new();
    let (vault, mock) = test_support::memory(8).unwrap();
    let state =
        AppState::open_with_vault(&root.0.join("state.sqlite"), "fixture-token".into(), vault)
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let model = Arc::new(Model {
        seen: Mutex::new(vec![]),
        calls: AtomicUsize::new(0),
        blocked,
        started: tokio::sync::Notify::new(),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(completion))
        .route("/v1/models", get(models))
        .with_state(model.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let receipt=state.store.lock().unwrap().provider_command(&json!({"id":new_id(),"command":{"type":"save_provider","definition":{"name":"夹具服务","base_url":base,"model":"profile-model"}}}),&[]).unwrap();
    let id = receipt["provider_id"].as_str().unwrap().to_owned();
    let record = state.store.lock().unwrap().provider_record(&id).unwrap();
    let binding = providers::credential_binding(&id, &record.definition).unwrap();
    let fresh = FreshCredential::new(&binding, SecretBytes::new(KEY.as_bytes().to_vec()).unwrap());
    let op = new_id();
    state
        .store
        .lock()
        .unwrap()
        .begin_provider_credential_set(&op, &id, record.revision, fresh.reference())
        .unwrap();
    state
        .store
        .lock()
        .unwrap()
        .claim_credential_operation(&op)
        .unwrap()
        .unwrap();
    state
        .vault
        .set(
            fresh,
            &binding,
            Instant::now() + Duration::from_secs(5),
            &VaultCancellation::default(),
        )
        .await
        .unwrap();
    state
        .store
        .lock()
        .unwrap()
        .finish_credential_operation(&op, crate::store::providers::CredentialOutcome::Succeeded)
        .unwrap();
    let record = state.store.lock().unwrap().provider_record(&id).unwrap();
    state.store.lock().unwrap().provider_command(&json!({"id":new_id(),"command":{"type":"set_provider_enabled","id":id,"expected_revision":record.revision,"enabled":true}}),&[]).unwrap();
    let mut settings = state.settings.write().unwrap();
    settings.provider_source = "named".into();
    settings.provider_id = Some(id.clone());
    settings.model = "legacy-must-not-be-used".into();
    settings.api_key = "legacy-must-not-be-sent".into();
    drop(settings);
    (root, state, mock, id, model, server)
}
fn enqueue(state: &Arc<AppState>, root: &Root) -> String {
    let project = root.0.join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("fixture.txt"), KEY).unwrap();
    let settings = state.settings.read().unwrap().clone();
    let source = json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"fixture"});
    let mut store = state.store.lock().unwrap();
    let session=store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"create_session","title":"夹具","project_path":project}}),&settings).unwrap().0;
    let receipt=store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"run_agent","session_id":session["session_id"],"content":"读取fixture.txt并回复"}}),&settings).unwrap().0;
    receipt["task_id"].as_str().unwrap().into()
}
fn disable(state: &AppState, id: &str) {
    let mut store = state.store.lock().unwrap();
    let record = store.provider_record(id).unwrap();
    store.provider_command(&json!({"id":new_id(),"command":{"type":"set_provider_enabled","id":id,"expected_revision":record.revision,"enabled":false}}),&[]).unwrap();
}
#[tokio::test]
async fn named_native_uses_one_vault_snapshot_and_redacts_model_tools_events_and_report() {
    let (root, state, mock, id, model, server) = setup(false).await;
    let result = crate::models::discover(
        State(state.clone()),
        Json(serde_json::from_value(json!({"provider_source":"named","provider_id":id})).unwrap()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(result["models"].as_array().unwrap().len(), 1);
    assert!(!result.to_string().contains(KEY));
    let before = mock.counts().gets;
    let task_id = enqueue(&state, &root);
    let task = state.store.lock().unwrap().claim_task().unwrap().unwrap();
    tokio::time::timeout(
        Duration::from_secs(15),
        crate::run_task(state.clone(), task),
    )
    .await
    .unwrap();
    assert_eq!(mock.counts().gets, before + 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let snapshot = state.store.lock().unwrap().snapshot().unwrap();
    let task = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == task_id)
        .unwrap();
    assert_eq!(task["status"], "completed", "{task}");
    assert!(!snapshot.to_string().contains(KEY));
    let report = state.store.lock().unwrap().changes(&task_id).unwrap();
    assert!(!report.to_string().contains(KEY));
    server.abort();
    let _ = server.await;
    drop(state);
    drop(root);
}
#[tokio::test]
async fn live_named_provider_revoke_cancels_waiting_http_without_retry() {
    let (root, state, _mock, id, model, server) = setup(true).await;
    let task_id = enqueue(&state, &root);
    let task = state.store.lock().unwrap().claim_task().unwrap().unwrap();
    let running = tokio::spawn(crate::run_task(state.clone(), task));
    tokio::time::timeout(Duration::from_secs(10), model.started.notified())
        .await
        .unwrap();
    disable(&state, &id);
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    let snapshot = state.store.lock().unwrap().snapshot().unwrap();
    let task = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == task_id)
        .unwrap();
    assert_eq!(task["status"], "failed");
    assert_eq!(task["stop_reason"], "provider_revoked");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    server.abort();
    let _ = server.await;
    drop(state);
    drop(root);
}
#[tokio::test]
async fn missing_vault_key_fails_without_legacy_key_or_network_fallback() {
    let (root, state, mock, _id, model, server) = setup(false).await;
    let task_id = enqueue(&state, &root);
    mock.reject_next(super::super::vault::VaultReason::Missing, false);
    let task = state.store.lock().unwrap().claim_task().unwrap().unwrap();
    crate::run_task(state.clone(), task).await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    let snapshot = state.store.lock().unwrap().snapshot().unwrap();
    let task = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == task_id)
        .unwrap();
    assert_eq!(task["status"], "failed");
    assert!(!snapshot.to_string().contains("legacy-must-not-be-sent"));
    server.abort();
    let _ = server.await;
    drop(state);
    drop(root);
}
