mod codex;
mod coordinator;
pub mod profiles;
pub mod provider;
pub mod store;

use std::{
    collections::HashMap,
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use harness_core::{new_id, now, RunEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify, Semaphore};

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    pub execution_mode: String,
    #[serde(default = "profiles::default_agents")]
    pub agents: Vec<profiles::AgentProfile>,
    #[serde(default = "profiles::default_agent_id")]
    pub default_agent_id: String,
    #[serde(default = "default_codex_binary")]
    pub codex_binary: String,
    #[serde(skip)]
    pub api_key: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: std::env::var("HARNESS_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            model: std::env::var("HARNESS_MODEL").unwrap_or_default(),
            execution_mode: "native".into(),
            agents: profiles::default_agents(),
            default_agent_id: profiles::default_agent_id(),
            codex_binary: default_codex_binary(),
            api_key: std::env::var("HARNESS_API_KEY").unwrap_or_default(),
        }
    }
}

fn default_codex_binary() -> String {
    if let Ok(binary) = std::env::var("HARNESS_CODEX_BINARY") {
        return binary;
    }
    let command = if cfg!(windows) { "codex.exe" } else { "codex" };
    if let Some(binary) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(command))
            .find(|candidate| candidate.is_file())
    }) {
        return binary.to_string_lossy().into_owned();
    }
    for directory in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let candidate = PathBuf::from(directory).join(command);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "codex".into()
}

impl Settings {
    pub fn configured(&self) -> bool {
        !self.model.trim().is_empty() && !self.base_url.trim().is_empty()
    }

    fn public(&self) -> Value {
        json!({
            "base_url": self.base_url, "model": self.model,
            "execution_mode": self.execution_mode, "configured": self.configured(),
            "api_key_configured": !self.api_key.is_empty(),
            "agents":self.agents, "default_agent_id":self.default_agent_id,
            "codex_binary":self.codex_binary
        })
    }
}

pub struct AppState {
    pub store: Mutex<store::Store>,
    pub settings: RwLock<Settings>,
    pub events: broadcast::Sender<RunEvent>,
    pub wake: Notify,
    pub token: String,
    pub workspace_gates: Mutex<HashMap<PathBuf, Arc<Semaphore>>>,
}

impl AppState {
    pub fn open(path: &std::path::Path, token: String) -> anyhow::Result<Arc<Self>> {
        let (events, _) = broadcast::channel(256);
        let store = store::Store::open(path)?;
        let settings = store.load_settings()?;
        Ok(Arc::new(Self {
            store: Mutex::new(store),
            settings: RwLock::new(settings),
            events,
            wake: Notify::new(),
            token,
            workspace_gates: Mutex::new(HashMap::new()),
        }))
    }

    pub fn publish(&self, event: RunEvent) {
        let _ = self.events.send(event);
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let private = Router::new()
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/commands", post(command))
        .route("/api/v1/settings", get(settings).put(update_settings))
        .route("/api/v1/codex/status", get(codex_status))
        .route("/api/v1/events", get(events))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route(
            "/health",
            get(|| async { Json(json!({"status":"ok", "version": env!("CARGO_PKG_VERSION")})) }),
        )
        .merge(private)
        .with_state(state)
}

async fn authenticate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let expected = format!("Bearer {}", state.token);
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| value == expected)
        .unwrap_or(false);
    if !authorized {
        return ApiError(StatusCode::UNAUTHORIZED, "需要本机身份验证".into()).into_response();
    }
    next.run(request).await
}

async fn snapshot(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let result = state.store.lock().unwrap().snapshot()?;
    Ok(Json(result))
}

async fn settings(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.settings.read().unwrap().public())
}

async fn codex_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    use tokio::io::AsyncReadExt;

    let binary = state.settings.read().unwrap().codex_binary.clone();
    let mut command = tokio::process::Command::new(&binary);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    prepend_binary_directory(&mut command, &binary);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let probe = async {
        let mut child = command.spawn().ok()?;
        let mut output = Vec::new();
        child
            .stdout
            .take()?
            .take(129)
            .read_to_end(&mut output)
            .await
            .ok()?;
        if output.len() > 128 || !child.wait().await.ok()?.success() {
            return None;
        }
        let text = String::from_utf8(output).ok()?;
        let version = text.trim().strip_prefix("codex-cli ")?;
        if version.is_empty()
            || !version
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || ".-_+".contains(character))
        {
            return None;
        }
        Some(version.to_owned())
    };
    let version = tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .ok()
        .flatten();
    Json(json!({
        "available": version.is_some(), "version":version,
        "message": if version.is_some() {"已找到本机 Codex CLI；登录状态和模型可用性在执行时检查"} else {"无法运行 Codex CLI，请检查可执行文件路径；此检测不会请求模型"}
    }))
}

fn prepend_binary_directory(command: &mut tokio::process::Command, binary: &str) {
    let path = std::path::Path::new(binary);
    if path.is_absolute() {
        if let Some(parent) = path.parent() {
            let mut paths = vec![parent.to_path_buf()];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(path) = std::env::join_paths(paths) {
                command.env("PATH", path);
            }
        }
    }
}

async fn update_settings(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut settings = state.settings.write().unwrap();
    let mut updated = settings.clone();
    for (field, target) in [
        ("base_url", &mut updated.base_url),
        ("model", &mut updated.model),
        ("execution_mode", &mut updated.execution_mode),
        ("api_key", &mut updated.api_key),
        ("default_agent_id", &mut updated.default_agent_id),
        ("codex_binary", &mut updated.codex_binary),
    ] {
        if let Some(value) = patch.get(field) {
            *target = value
                .as_str()
                .ok_or_else(|| {
                    ApiError(StatusCode::BAD_REQUEST, format!("{field} 字段必须为文本"))
                })?
                .trim()
                .to_string();
        }
    }
    if let Some(agents) = patch.get("agents") {
        updated.agents = serde_json::from_value(agents.clone())
            .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "子智能体角色配置格式无效".into()))?;
    }
    profiles::validate_agents(&updated.agents, &updated.default_agent_id)?;
    if updated.codex_binary.trim().is_empty()
        || updated.codex_binary.len() > 4096
        || updated.codex_binary.contains(['\0', '\n', '\r'])
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Codex 可执行文件需要填写有效命令名或路径，参数由后台管理".into(),
        ));
    }
    if !matches!(updated.execution_mode.as_str(), "native" | "demo") {
        return Err(ApiError(StatusCode::BAD_REQUEST, "执行模式无效".into()));
    }
    let url = reqwest::Url::parse(&updated.base_url)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "模型服务地址无效".into()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "模型服务地址必须使用 HTTP 或 HTTPS".into(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "模型服务地址不能包含凭证、查询参数或片段，请在 API 密钥字段填写凭证".into(),
        ));
    }
    state.store.lock().unwrap().save_settings(&updated)?;
    *settings = updated;
    Ok(Json(settings.public()))
}

async fn command(
    State(state): State<Arc<AppState>>,
    Json(envelope): Json<Value>,
) -> ApiResult<Json<Value>> {
    let settings = state.settings.read().unwrap().clone();
    let (receipt, event) = state.store.lock().unwrap().command(envelope, &settings)?;
    if let Some(event) = event {
        state.publish(event);
    }
    state.wake.notify_one();
    Ok(Json(receipt))
}

async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let mut receiver = state.events.subscribe();
    let stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    yield Ok(Event::default().id(event.id.clone())
                        .event("run_event").json_data(event).unwrap());
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Event::default().event("resync").data("snapshot_required"));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

pub fn spawn_workers(state: Arc<AppState>) {
    coordinator::spawn(state.clone());
    for _ in 0..2 {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                let task = state.store.lock().unwrap().claim_task();
                match task {
                    Ok(Some(task)) => run_task(state.clone(), task).await,
                    Ok(None) => {
                        tokio::select! {
                            _ = state.wake.notified() => {},
                            _ = tokio::time::sleep(Duration::from_millis(300)) => {},
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "无法领取子任务");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
    }
    tokio::spawn(async move {
        loop {
            let result = state.store.lock().unwrap().deliver_one();
            match result {
                Ok(Some(event)) => state.publish(event),
                Ok(None) => {}
                Err(error) => tracing::error!(%error, "回调投递失败"),
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
}

async fn run_task(state: Arc<AppState>, task: Value) {
    let task_id = task["id"].as_str().unwrap().to_string();
    let session_id = task["session_id"].as_str().unwrap().to_string();
    let root = state.store.lock().unwrap().project_path(&session_id);
    let root = match root {
        Ok(root) => PathBuf::from(root),
        Err(error) => {
            finish(&state, &task_id, Err(error));
            return;
        }
    };
    // Queries and coordinator acknowledgments never wait for a project writer.
    let workspace_gate = state
        .workspace_gates
        .lock()
        .unwrap()
        .entry(root.clone())
        .or_insert_with(|| Arc::new(Semaphore::new(1)))
        .clone();
    let permit = loop {
        tokio::select! {
            permit = workspace_gate.clone().acquire_owned() => break permit.unwrap(),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancelled(&state, &task_id) { return; }
                let mut store = state.store.lock().unwrap();
                if !store.task_may_start(&task_id).unwrap_or(false) {
                    if let Err(error) = store.defer_task(&task_id) {
                        tracing::error!(%error, %task_id, "无法恢复等待项目锁的任务");
                    }
                    return;
                }
            }
        }
    };
    if cancelled(&state, &task_id) {
        return;
    }
    let may_start = state
        .store
        .lock()
        .unwrap()
        .task_may_start(&task_id)
        .unwrap_or(false);
    if !may_start {
        if let Err(error) = state.store.lock().unwrap().defer_task(&task_id) {
            tracing::error!(%error, %task_id, "无法恢复未执行任务的排队状态");
        }
        drop(permit);
        return;
    }
    let result = if task["execution_mode"] == "demo" {
        demo(&state, &task_id).await
    } else {
        let mut settings = state.settings.read().unwrap().clone();
        let agent = task
            .get("agent")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value::<profiles::AgentProfile>(value.clone()))
            .transpose();
        let agent = match agent {
            Ok(Some(agent)) => agent,
            Ok(None) => profiles::AgentProfile::default(),
            Err(_) => {
                finish(
                    &state,
                    &task_id,
                    Err(anyhow::anyhow!("任务角色快照格式无效")),
                );
                return;
            }
        };
        if !agent.model.trim().is_empty() {
            settings.model = agent.model.clone();
        }
        let objective = task["objective"]
            .as_str()
            .or_else(|| task["title"].as_str())
            .unwrap_or_default();
        let objective = if let Some(command) = task["acceptance_command"].as_str() {
            format!(
                "{objective}\n\n节点验收要求：实际执行以下完整命令并报告真实退出结果；不得以其他命令或文字说明替代。命令失败时保留记录。\n{command}"
            )
        } else {
            objective.to_owned()
        };
        let objective = if agent.instructions.trim().is_empty() {
            objective
        } else {
            format!(
                "角色职责：\n{}\n\n任务目标：\n{objective}",
                agent.instructions
            )
        };
        let state_for_log = state.clone();
        let task_for_log = task_id.clone();
        let task_for_cancel = task_id.clone();
        let state_for_cancel = state.clone();
        let progress = move |detail: String| {
            if let Ok(event) = state_for_log
                .store
                .lock()
                .unwrap()
                .progress(&task_for_log, &detail)
            {
                state_for_log.publish(event);
            }
        };
        let is_cancelled = move || cancelled(&state_for_cancel, &task_for_cancel);
        let state_for_evidence = state.clone();
        let task_for_evidence = task_id.clone();
        let evidence_secret = settings.api_key.clone();
        let evidence = move |mut item: harness_core::CommandEvidence| {
            item.command = redact_secret(&item.command, &evidence_secret);
            item.output = redact_secret(&item.output, &evidence_secret);
            if let Err(error) = state_for_evidence
                .store
                .lock()
                .unwrap()
                .record_evidence(&task_for_evidence, item)
            {
                tracing::error!(%error, "无法保存命令执行证据");
            }
        };
        match agent.executor.as_str() {
            "native" => {
                provider::execute_with_evidence(
                    &settings,
                    &root,
                    &objective,
                    progress,
                    is_cancelled,
                    evidence,
                )
                .await
            }
            "codex" => codex::execute_with_evidence(
                &settings.codex_binary,
                &agent.model,
                &root,
                &objective,
                progress,
                is_cancelled,
                evidence,
            )
            .await
            .map(|text| redact_secret(&text, &settings.api_key))
            .map_err(|error| anyhow::anyhow!(redact_secret(&error.to_string(), &settings.api_key))),
            _ => Err(anyhow::anyhow!("任务角色使用了不支持的执行器")),
        }
    };
    drop(permit);
    finish(&state, &task_id, result);
}

fn redact_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, "[凭证已隐藏]")
    }
}

fn finish(state: &AppState, task_id: &str, result: anyhow::Result<String>) {
    match state.store.lock().unwrap().finish(task_id, result) {
        Ok(Some(event)) => state.publish(event),
        Ok(None) => {}
        Err(error) => tracing::error!(%error, %task_id, "无法保存子任务结果"),
    }
    state.wake.notify_waiters();
}

fn cancelled(state: &AppState, task_id: &str) -> bool {
    state
        .store
        .lock()
        .unwrap()
        .is_cancelled(task_id)
        .unwrap_or(true)
}

async fn demo(state: &AppState, task_id: &str) -> anyhow::Result<String> {
    let step_ms = std::env::var("HARNESS_DEMO_STEP_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1000)
        .clamp(100, 60_000);
    for detail in [
        "演示：模拟整理任务目标",
        "演示：模拟执行子任务",
        "演示：模拟验证与结果汇总",
    ] {
        if cancelled(state, task_id) {
            anyhow::bail!("任务已取消");
        }
        let event = state.store.lock().unwrap().progress(task_id, detail)?;
        state.publish(event);
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(step_ms / 10)).await;
            if cancelled(state, task_id) {
                anyhow::bail!("任务已取消");
            }
        }
    }
    Ok("演示已完成。本次仅模拟任务流程，没有修改项目文件，也没有运行真实检查。".into())
}

pub fn event(
    session_id: Option<String>,
    task_id: Option<String>,
    kind: &str,
    message: &str,
) -> RunEvent {
    RunEvent {
        id: new_id(),
        session_id,
        task_id,
        kind: kind.into(),
        message: message.into(),
        created_at: now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_settings_patch_cannot_partially_replace_roles_or_credentials() {
        let state = AppState::open(std::path::Path::new(":memory:"), "test".into()).unwrap();
        let original = state.settings.read().unwrap().clone();
        let response = update_settings(
            State(state.clone()),
            Json(json!({
                "api_key":"must-not-be-saved",
                "default_agent_id":"missing",
                "agents":original.agents,
            })),
        )
        .await;
        assert!(response.is_err());
        let settings = state.settings.read().unwrap();
        assert_eq!(settings.default_agent_id, original.default_agent_id);
        assert_eq!(settings.api_key, original.api_key);
        assert_eq!(
            state
                .store
                .lock()
                .unwrap()
                .load_settings()
                .unwrap()
                .default_agent_id,
            original.default_agent_id
        );
    }

    #[tokio::test]
    async fn settings_round_trip_preserves_roles_and_hides_api_key() {
        let state = AppState::open(std::path::Path::new(":memory:"), "test".into()).unwrap();
        let Json(public) = update_settings(
            State(state.clone()),
            Json(json!({
                "agents":[{"id":"implementer","name":"编码","instructions":"先阅读项目规则",
                    "executor":"codex","model":""}],
                "default_agent_id":"implementer",
                "codex_binary":"codex",
                "api_key":"secret-api-key",
            })),
        )
        .await
        .unwrap();
        assert_eq!(public["agents"][0]["executor"], "codex");
        assert_eq!(public["default_agent_id"], "implementer");
        assert!(public.get("api_key").is_none());
        let saved = state.store.lock().unwrap().load_settings().unwrap();
        assert_eq!(saved.agents[0].instructions, "先阅读项目规则");
        assert_eq!(saved.codex_binary, "codex");
        assert!(!serde_json::to_string(&saved)
            .unwrap()
            .contains("secret-api-key"));
        for binary in ["", "codex\n--unsafe", "codex\0bad"] {
            assert!(
                update_settings(State(state.clone()), Json(json!({"codex_binary":binary})))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn coordinator_accepts_and_cancels_while_worker_waits() {
        let state = AppState::open(std::path::Path::new(":memory:"), "test".into()).unwrap();
        let settings = Settings {
            execution_mode: "demo".into(),
            ..Settings::default()
        };
        *state.settings.write().unwrap() = settings.clone();
        let source =
            json!({"channel_id":"simulator","actor_id":"owner","conversation_id":"remote-1"});
        let envelope = |command| json!({"command_id":new_id(),"source":source,"command":command});
        let session = state.store.lock().unwrap().command(envelope(json!({
            "type":"create_session","title":"nonblocking","project_path":std::env::temp_dir()
        })), &settings).unwrap().0;
        let session_id = session["session_id"].as_str().unwrap();
        let task = state
            .store
            .lock()
            .unwrap()
            .command(
                envelope(json!({
                    "type":"start_task","session_id":session_id,"title":"synthetic wait"
                })),
                &settings,
            )
            .unwrap()
            .0;
        let task_id = task["task_id"].as_str().unwrap();
        spawn_workers(state.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            state.store.lock().unwrap().snapshot().unwrap()["tasks"][0]["status"],
            "running"
        );
        let started = std::time::Instant::now();
        state.store.lock().unwrap().command(envelope(json!({
            "type":"send_message","session_id":session_id,"content":"manage during execution"
        })), &settings).unwrap();
        state
            .store
            .lock()
            .unwrap()
            .command(
                envelope(json!({
                    "type":"cancel_task","task_id":task_id
                })),
                &settings,
            )
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(
            state.store.lock().unwrap().snapshot().unwrap()["tasks"][0]["status"],
            "cancelled"
        );
    }
}
