mod codex;
mod configuration;
mod context;
mod coordinator;
mod model_config;
mod models;
pub mod profiles;
pub mod provider;
pub mod store;

use std::{
    collections::{BTreeMap, HashMap},
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
use harness_core::{new_id, now, ModelContextConfig, RunEvent};
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_main_executor")]
    pub main_executor: String,
    #[serde(default)]
    pub models_endpoint: String,
    #[serde(default)]
    pub model_contexts: BTreeMap<String, ModelContextConfig>,
    pub execution_mode: String,
    #[serde(default = "configuration::default_source")]
    pub provider_source: String,
    #[serde(default = "profiles::default_agents")]
    pub agents: Vec<profiles::AgentProfile>,
    #[serde(default = "profiles::default_agent_id")]
    pub default_agent_id: String,
    #[serde(default = "default_codex_binary")]
    pub codex_binary: String,
    #[serde(skip)]
    pub api_key: String,
    #[serde(skip)]
    pub provider_headers: reqwest::header::HeaderMap,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: std::env::var("HARNESS_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            model: std::env::var("HARNESS_MODEL").unwrap_or_default(),
            main_executor: default_main_executor(),
            models_endpoint: String::new(),
            model_contexts: BTreeMap::new(),
            execution_mode: "native".into(),
            provider_source: configuration::default_source(),
            agents: profiles::default_agents(),
            default_agent_id: profiles::default_agent_id(),
            codex_binary: default_codex_binary(),
            api_key: std::env::var("HARNESS_API_KEY").unwrap_or_default(),
            provider_headers: reqwest::header::HeaderMap::new(),
        }
    }
}

fn default_main_executor() -> String {
    "native".into()
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
        configuration::resolve(self).is_ok()
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.headers(self.provider_headers.clone());
        if self.api_key.is_empty() {
            request
        } else {
            request.bearer_auth(&self.api_key)
        }
    }

    fn redact(&self, text: &str) -> String {
        let mut text = redact_secret(text, &self.api_key);
        for value in self.provider_headers.values() {
            if let Ok(value) = value.to_str() {
                text = redact_secret(&text, value);
                if let Some(secret) = value.strip_prefix("Bearer ") {
                    text = redact_secret(&text, secret);
                }
            }
        }
        text
    }

    fn public(&self) -> Value {
        let result = configuration::resolve(self);
        let effective = result.as_ref().unwrap_or(self);
        let selected_model = if self.main_executor == "codex" {
            &self.model
        } else {
            &effective.model
        };
        let model = if configuration::validate_model(selected_model).is_ok() {
            selected_model.as_str()
        } else {
            ""
        };
        let mut agents = self.agents.clone();
        for agent in &mut agents {
            if configuration::looks_like_credential(&agent.model) {
                agent.model.clear();
            }
        }
        json!({
            "base_url": effective.base_url, "model": model,
            "main_executor": self.main_executor,
            "models_endpoint": self.models_endpoint,
            "model_contexts": self.model_contexts,
            "provider_source": self.provider_source,
            "configuration_error": result.as_ref().err().map(ToString::to_string),
            "execution_mode": self.execution_mode, "configured": result.is_ok(),
            "api_key_configured": !effective.api_key.is_empty() || !effective.provider_headers.is_empty(),
            "agents":agents, "default_agent_id":self.default_agent_id,
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
        .route("/api/v1/settings/check", post(check_settings))
        .route("/api/v1/provider/local-codex", get(local_codex_provider))
        .route("/api/v1/codex/status", get(codex_status))
        .route("/api/v1/models", post(models::discover))
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
    let updated = patch_settings(&settings, &patch)?;
    state.store.lock().unwrap().save_settings(&updated)?;
    *settings = updated;
    Ok(Json(settings.public()))
}

fn patch_settings(settings: &Settings, patch: &Value) -> anyhow::Result<Settings> {
    let mut updated = settings.clone();
    for (field, target) in [
        ("base_url", &mut updated.base_url),
        ("model", &mut updated.model),
        ("main_executor", &mut updated.main_executor),
        ("models_endpoint", &mut updated.models_endpoint),
        ("execution_mode", &mut updated.execution_mode),
        ("api_key", &mut updated.api_key),
        ("default_agent_id", &mut updated.default_agent_id),
        ("codex_binary", &mut updated.codex_binary),
        ("provider_source", &mut updated.provider_source),
    ] {
        if let Some(value) = patch.get(field) {
            *target = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{field} 字段必须为文本"))?
                .trim()
                .to_string();
        }
    }
    if let Some(agents) = patch.get("agents") {
        updated.agents = serde_json::from_value(agents.clone())
            .map_err(|_| anyhow::anyhow!("子智能体角色配置格式无效"))?;
    }
    if let Some(configs) = patch.get("model_contexts") {
        updated.model_contexts = serde_json::from_value(configs.clone())
            .map_err(|_| anyhow::anyhow!("模型上下文配置格式无效"))?;
    }
    model_config::validate_registry(&updated.model_contexts)?;
    profiles::validate_agents(&updated.agents, &updated.default_agent_id)?;
    if !matches!(updated.main_executor.as_str(), "native" | "codex") {
        anyhow::bail!("主智能体执行器无效");
    }
    if updated.codex_binary.trim().is_empty()
        || updated.codex_binary.len() > 4096
        || updated.codex_binary.contains(['\0', '\n', '\r'])
    {
        anyhow::bail!("Codex 可执行文件需要填写有效命令名或路径，参数由后台管理");
    }
    if !matches!(updated.execution_mode.as_str(), "native" | "demo") {
        anyhow::bail!("执行模式无效");
    }
    configuration::validate_model(&updated.model)?;
    match updated.provider_source.as_str() {
        "manual" => {
            models::models_url(&updated.base_url, &updated.models_endpoint)?;
            // Retaining an old credential while changing hosts would send it to a new service.
            if (updated.base_url.trim_end_matches('/') != settings.base_url.trim_end_matches('/')
                || settings.provider_source != "manual")
                && patch.get("api_key").is_none()
            {
                updated.api_key.clear();
            }
        }
        "local_codex" => {
            configuration::resolve(&updated)?;
            updated.api_key.clear();
            updated.provider_headers.clear();
            // The address and credentials are read from the source for each request.
            updated.base_url.clear();
        }
        _ => anyhow::bail!("模型配置来源无效"),
    }
    Ok(updated)
}

async fn local_codex_provider() -> ApiResult<Json<Value>> {
    let settings = Settings {
        provider_source: "local_codex".into(),
        model: String::new(),
        ..Settings::default()
    };
    configuration::resolve(&settings)?;
    Ok(Json(settings.public()))
}

async fn check_settings(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> ApiResult<Json<Value>> {
    let settings = state.settings.read().unwrap().clone();
    let draft = patch_settings(&settings, &patch)?;
    Ok(Json(configuration::check(&draft).await?))
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
                        tracing::error!(%error, "无法领取任务");
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
        let settings = match configuration::resolve(&settings) {
            Ok(settings) => settings,
            Err(_) if agent.executor == "codex" => settings,
            Err(error) => {
                finish(&state, &task_id, Err(error));
                return;
            }
        };
        let context = match task.get("context_config").filter(|value| !value.is_null()) {
            Some(value) => serde_json::from_value::<ModelContextConfig>(value.clone())
                .map_err(anyhow::Error::from),
            None => model_config::resolve(&settings.model_contexts, &settings.model, Some(&agent)),
        };
        let context = match context {
            Ok(context) => context,
            Err(error) => {
                finish(&state, &task_id, Err(error));
                return;
            }
        };
        let history = if task["task_kind"] == "main" {
            let result = state.store.lock().unwrap().execution_history(&task_id);
            match result {
                Ok(history) => history,
                Err(error) => {
                    finish(&state, &task_id, Err(error));
                    return;
                }
            }
        } else {
            Vec::new()
        };
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
        let input = context::ExecutionInput {
            objective: &objective,
            history: &history,
            config: &context,
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
                provider::execute_with_context(
                    &settings,
                    &root,
                    input,
                    progress,
                    is_cancelled,
                    evidence,
                )
                .await
            }
            "codex" => codex::execute_with_context(
                &settings.codex_binary,
                &agent.model,
                &root,
                input,
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
        Err(error) => tracing::error!(%error, %task_id, "无法保存任务结果"),
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
        "演示：模拟执行任务",
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

    #[test]
    fn rejects_credential_models_and_clears_credentials_when_service_changes() {
        let settings = Settings {
            base_url: "http://localhost:1234/v1".into(),
            model: "fixture-model".into(),
            api_key: "fixture-old-secret".into(),
            ..Settings::default()
        };
        for patch in [
            json!({"model":format!("sk-{}", "a".repeat(40))}),
            json!({"agents":[{"id":"coder", "name":"编码", "executor":"native","model":format!("sk-{}", "a".repeat(40)), "instructions":""}]}),
        ] {
            assert!(patch_settings(&settings, &patch).is_err());
        }
        let updated =
            patch_settings(&settings, &json!({"base_url":"http://localhost:4321/v1"})).unwrap();
        assert!(updated.api_key.is_empty());
        assert_eq!(settings.api_key, "fixture-old-secret");
        assert_eq!(
            patch_settings(&settings, &json!({"model":"another-model"}))
                .unwrap()
                .api_key,
            "fixture-old-secret"
        );
        let malformed = Settings {
            model: format!("sk-{}", "a".repeat(40)),
            ..settings
        };
        let public = malformed.public();
        assert_eq!(public["configured"], false);
        assert_eq!(public["model"], "");
        assert!(!public.to_string().contains(&malformed.model));
    }

    #[tokio::test]
    async fn checking_a_draft_does_not_persist_it_or_block_settings_updates() {
        use axum::routing::post;
        let state = AppState::open(std::path::Path::new(":memory:"), "test".into()).unwrap();
        let before = state.settings.read().unwrap().public();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let (seen, release) = (started.clone(), finish.clone());
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let (seen, release) = (seen.clone(), release.clone());
                async move {
                    seen.notify_one();
                    release.notified().await;
                    Json(json!({"choices":[{"message":{"content":"{\"ok\":true}"}}]}))
                }
            }),
        );
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let task_state = state.clone();
        let check = tokio::spawn(async move {
            check_settings(
                State(task_state),
                Json(json!({"base_url":base,"model":"fixture-model","api_key":"fixture-secret"})),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        assert_eq!(state.settings.read().unwrap().public(), before);
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            update_settings(State(state.clone()), Json(json!({"model":"changed-model"}))),
        )
        .await
        .unwrap()
        .unwrap();
        finish.notify_one();
        assert_eq!(check.await.unwrap().unwrap().0["ok"], true);
        assert_eq!(state.settings.read().unwrap().model, "changed-model");
        assert!(state.settings.read().unwrap().api_key.is_empty());
        server.abort();
    }

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
                    "executor":"codex","model":"","compression_threshold":60}],
                "default_agent_id":"implementer",
                "main_executor":"codex",
                "models_endpoint":"/catalog?api-version=1",
                "model_contexts":{"fixture-model":{"context_window":64000,"compression_threshold":70}},
                "codex_binary":"codex",
                "api_key":"secret-api-key",
            })),
        )
        .await
        .unwrap();
        assert_eq!(public["agents"][0]["executor"], "codex");
        assert_eq!(public["default_agent_id"], "implementer");
        assert_eq!(public["main_executor"], "codex");
        assert_eq!(public["models_endpoint"], "/catalog?api-version=1");
        assert_eq!(
            public["model_contexts"]["fixture-model"]["context_window"],
            64000
        );
        assert!(public.get("api_key").is_none());
        let saved = state.store.lock().unwrap().load_settings().unwrap();
        assert_eq!(saved.agents[0].instructions, "先阅读项目规则");
        assert_eq!(saved.codex_binary, "codex");
        assert_eq!(saved.agents[0].compression_threshold, Some(60));
        assert_eq!(
            saved.model_contexts["fixture-model"].compression_threshold,
            70
        );
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

    #[test]
    fn invalid_context_or_endpoint_patch_is_rejected_atomically() {
        let original = Settings::default();
        for patch in [
            json!({"model_contexts":{"fixture":{"context_window":100,"compression_threshold":80}}}),
            json!({"model_contexts":{"fixture":{"context_window":128000,"compression_threshold":99}}}),
            json!({"main_executor":"other"}),
            json!({"models_endpoint":"https://another-service.invalid/models"}),
        ] {
            assert!(patch_settings(&original, &patch).is_err());
        }
        assert!(original.model_contexts.is_empty());
        assert!(original.models_endpoint.is_empty());
        assert_eq!(original.main_executor, "native");
    }

    #[test]
    fn public_codex_model_preserves_an_explicit_default_selection() {
        let settings = Settings {
            main_executor: "codex".into(),
            model: String::new(),
            ..Settings::default()
        };
        assert_eq!(settings.public()["model"], "");
        assert_eq!(settings.public()["main_executor"], "codex");
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
