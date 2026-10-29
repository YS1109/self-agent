mod capabilities;
mod changes;
mod codex;
mod configuration;
mod context;
mod continuation;
mod coordinator;
mod execution_control;
mod gateway;
pub mod maintenance;
mod model_config;
mod models;
pub mod profiles;
mod project_files;
pub mod provider;
mod qq;
mod reliability;
mod steering;
pub mod store;

use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use axum::{
    extract::{Path as ApiPath, Query, Request, State},
    http::{HeaderMap, StatusCode},
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
    #[serde(default)]
    pub execution_controls: harness_core::TaskControls,
    pub execution_mode: String,
    #[serde(default = "configuration::default_source")]
    pub provider_source: String,
    #[serde(default)]
    pub provider_id: Option<String>,
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
    #[serde(skip)]
    pub(crate) redaction_secrets: Vec<String>,
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
            execution_controls: harness_core::TaskControls::default(),
            execution_mode: "native".into(),
            provider_source: configuration::default_source(),
            provider_id: None,
            agents: profiles::default_agents(),
            default_agent_id: profiles::default_agent_id(),
            codex_binary: default_codex_binary(),
            api_key: std::env::var("HARNESS_API_KEY").unwrap_or_default(),
            provider_headers: reqwest::header::HeaderMap::new(),
            redaction_secrets: Vec::new(),
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
        provider::redact_known(text, &continuation::known_secrets(self))
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
            "execution_controls": self.execution_controls,
            "provider_source": self.provider_source,
            "provider_id": self.provider_id,
            "configuration_error": result.as_ref().err().map(ToString::to_string),
            "execution_mode": self.execution_mode, "configured": result.is_ok(),
            "api_key_configured": !effective.api_key.is_empty() || !effective.provider_headers.is_empty(),
            "agents":agents, "default_agent_id":self.default_agent_id,
            "codex_binary":self.codex_binary
        })
    }
}

pub struct AppState {
    pub(crate) vault: Arc<capabilities::vault::Vault>,
    pub store: Mutex<store::Store>,
    pub settings: RwLock<Settings>,
    pub events: broadcast::Sender<RunEvent>,
    pub wake: Notify,
    pub token: String,
    pub workspace_gates: Mutex<HashMap<PathBuf, Arc<Semaphore>>>,
    pub capability_probe_root: PathBuf,
    pub capability_probe_gate: Arc<Semaphore>,
    pub(crate) storage_lease: Option<Arc<maintenance::DataLease>>,
    pub(crate) backup_gate: Arc<Semaphore>,
    pub(crate) gateway_stop: tokio::sync::watch::Sender<bool>,
    pub(crate) gateway_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(crate) qq_stop: tokio::sync::watch::Sender<bool>,
    pub(crate) qq_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AppState {
    pub fn open(path: &std::path::Path, token: String) -> anyhow::Result<Arc<Self>> {
        Self::open_with_vault(
            path,
            token,
            Arc::new(capabilities::vault::Vault::system(32)?),
        )
    }

    pub fn open_managed(
        lease: Arc<maintenance::DataLease>,
        token: String,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open_with_vault_and_lease(
            &lease.database_path(),
            token,
            Arc::new(capabilities::vault::Vault::system(32)?),
            Some(lease.clone()),
        )
    }

    pub(crate) fn open_with_vault(
        path: &std::path::Path,
        token: String,
        vault: Arc<capabilities::vault::Vault>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open_with_vault_and_lease(path, token, vault, None)
    }
    pub(crate) fn open_with_vault_and_lease(
        path: &std::path::Path,
        token: String,
        vault: Arc<capabilities::vault::Vault>,
        storage_lease: Option<Arc<maintenance::DataLease>>,
    ) -> anyhow::Result<Arc<Self>> {
        let (events, _) = broadcast::channel(256);
        let store = if let Some(lease) = &storage_lease {
            if path != lease.database_path() {
                anyhow::bail!("后台数据库与已锁定目录不一致");
            }
            lease.open_store()?
        } else {
            store::Store::open(path)?
        };
        let settings = store.load_settings()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let capability_probe_root = if let Some(lease) = &storage_lease {
            lease.guard.probe_directory()?
        } else {
            let root = std::fs::canonicalize(parent)?.join("capability-probes");
            std::fs::create_dir_all(&root)?;
            root
        };
        // Reject a preexisting linked probe directory instead of following it.
        project_files::ScopedProject::open(&capability_probe_root)?;
        Ok(Arc::new(Self {
            vault,
            store: Mutex::new(store),
            settings: RwLock::new(settings),
            events,
            wake: Notify::new(),
            token,
            workspace_gates: Mutex::new(HashMap::new()),
            capability_probe_root,
            capability_probe_gate: Arc::new(Semaphore::new(2)),
            storage_lease,
            backup_gate: Arc::new(Semaphore::new(2)),
            gateway_stop: tokio::sync::watch::channel(false).0,
            gateway_worker: Mutex::new(None),
            qq_stop: tokio::sync::watch::channel(false).0,
            qq_worker: Mutex::new(None),
        }))
    }

    /// Best-effort bounded draining; a blocked OS mutation remains uncertain
    /// and its durable intent is recovered without replay on the next startup.
    pub async fn shutdown_credentials(&self) -> bool {
        self.gateway_stop.send_replace(true);
        let gateway_worker = { self.gateway_worker.lock().unwrap().take() };
        let gateway_settled = if let Some(mut worker) = gateway_worker {
            // Do not abort an accepted SQLite worker. It keeps AppState/data
            // lease until the original transaction settles; no RPC replay.
            tokio::time::timeout(Duration::from_secs(2), &mut worker)
                .await
                .is_ok()
        } else {
            true
        };
        self.qq_stop.send_replace(true);
        let worker = { self.qq_worker.lock().unwrap().take() };
        if let Some(mut worker) = worker {
            if tokio::time::timeout(Duration::from_secs(2), &mut worker)
                .await
                .is_err()
            {
                worker.abort();
                let _ = worker.await;
            }
        }
        let vault_stopped = self
            .vault
            .shutdown(std::time::Instant::now() + Duration::from_secs(2))
            .await
            == capabilities::vault::ShutdownStatus::Stopped;
        gateway_settled && vault_stopped
    }

    pub fn publish(&self, event: RunEvent) {
        let _ = self.events.send(event);
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let private = Router::new()
        .route("/api/v1/snapshot", get(snapshot))
        .merge(capabilities::credential_api::routes())
        .merge(reliability::api::routes())
        .merge(gateway::api::routes())
        .route("/api/v1/qq", get(qq::api::snapshot))
        .route(
            "/api/v1/qq/commands",
            post(qq::api::command).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/api/v1/qq/commands/{id}", get(qq::api::receipt))
        .route(
            "/api/v1/qq/credentials/{id}",
            post(qq::api::set_credential)
                .delete(qq::api::delete_credential)
                .layer(axum::extract::DefaultBodyLimit::max(8192)),
        )
        .route(
            "/api/v1/qq/credentials/cleanup/{id}",
            post(qq::api::cleanup),
        )
        .route(
            "/api/v1/qq/credential-operations/{id}",
            get(qq::api::operation),
        )
        .route(
            "/api/v1/credential-operations/{id}",
            get(capabilities::provider_api::operation),
        )
        .route(
            "/api/v1/providers",
            get(capabilities::provider_api::registry),
        )
        .route(
            "/api/v1/providers/commands",
            post(capabilities::provider_api::command),
        )
        .route(
            "/api/v1/providers/{id}/credential",
            post(capabilities::provider_api::set_credential)
                .layer(axum::extract::DefaultBodyLimit::max(8192)),
        )
        .route(
            "/api/v1/providers/{id}/credential-delete",
            post(capabilities::provider_api::delete_credential),
        )
        .route(
            "/api/v1/credential-operations/{id}/cleanup",
            post(capabilities::provider_api::cleanup),
        )
        .route("/api/v1/capabilities", get(capabilities::api::registry))
        .route(
            "/api/v1/capabilities/commands",
            post(capabilities::api::command)
                .layer(axum::extract::DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/api/v1/tasks/{task_id}/changes", get(task_changes))
        .route(
            "/api/v1/tasks/{task_id}/cleanup-resolution",
            post(resolve_cleanup),
        )
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

async fn task_changes(
    State(state): State<Arc<AppState>>,
    ApiPath(task_id): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    if task_id.is_empty() || task_id.len() > 128 {
        return Err(ApiError(StatusCode::BAD_REQUEST, "任务标识无效".into()));
    }
    Ok(Json(state.store.lock().unwrap().changes(&task_id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupResolution {
    expected_updated_at: String,
    confirm_processes_stopped: bool,
}
async fn resolve_cleanup(
    State(state): State<Arc<AppState>>,
    ApiPath(task_id): ApiPath<String>,
    Json(request): Json<CleanupResolution>,
) -> ApiResult<Json<Value>> {
    if task_id.len() > 128
        || request.expected_updated_at.len() > 128
        || !request.confirm_processes_stopped
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "必须确认相关进程已经停止".into(),
        ));
    }
    let result = state
        .store
        .lock()
        .unwrap()
        .resolve_execution_cleanup(&task_id, &request.expected_updated_at)?;
    state.wake.notify_waiters();
    Ok(Json(result))
}

async fn snapshot(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let result = state.store.lock().unwrap().snapshot()?;
    Ok(Json(result))
}

async fn settings(State(state): State<Arc<AppState>>) -> Json<Value> {
    {
        let settings = state.settings.read().unwrap().clone();
        Json(capabilities::provider_runtime::public_settings(
            &state, &settings,
        ))
    }
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
    Ok(Json(capabilities::provider_runtime::public_settings(
        &state, &settings,
    )))
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
    if let Some(id) = patch.get("provider_id") {
        updated.provider_id = if id.is_null() {
            None
        } else {
            let id = id
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("模型服务标识必须是 UUID 或 null"))?;
            if uuid::Uuid::parse_str(id).is_err() {
                anyhow::bail!("模型服务标识无效");
            }
            Some(id.into())
        };
    }
    if let Some(agents) = patch.get("agents") {
        let mut agents = agents.clone();
        if let Some(items) = agents.as_array_mut() {
            for item in items {
                if item.get("controls").is_none() {
                    if let Some(prior) = settings.agents.iter().find(|prior| item["id"] == prior.id)
                    {
                        item["controls"] = serde_json::to_value(&prior.controls)?;
                    }
                }
            }
        }
        updated.agents = serde_json::from_value(agents)
            .map_err(|_| anyhow::anyhow!("子智能体角色配置格式无效"))?;
    }
    if let Some(configs) = patch.get("model_contexts") {
        updated.model_contexts = serde_json::from_value(configs.clone())
            .map_err(|_| anyhow::anyhow!("模型上下文配置格式无效"))?;
    }
    if let Some(controls) = patch.get("execution_controls") {
        let mut merged = serde_json::to_value(&settings.execution_controls)?;
        merge_control_patch(&mut merged, controls)?;
        updated.execution_controls = serde_json::from_value(merged)
            .map_err(|_| anyhow::anyhow!("执行预算或权限配置格式无效"))?;
    }
    store::execution::resolve_controls(&updated.execution_controls, None, None, "native")?;
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
        "named" => {
            if updated.provider_id.is_none() {
                anyhow::bail!("请选择命名模型服务");
            }
            if patch.get("api_key").is_some() {
                anyhow::bail!("命名服务凭据只能通过专用系统凭据接口设置");
            }
            updated.api_key.clear();
            updated.provider_headers.clear();
            updated.base_url.clear();
            updated.models_endpoint.clear();
            if updated.main_executor == "native" {
                updated.model.clear();
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

// Partial settings updates preserve every omitted limit. Unknown keys still
// fail typed deserialization; null never silently restores permissive defaults.
fn merge_control_patch(target: &mut Value, patch: &Value) -> anyhow::Result<()> {
    let values = patch
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("执行配置补丁必须为对象"))?;
    for (key, value) in values {
        if target.get(key).is_some_and(Value::is_object) {
            merge_control_patch(&mut target[key], value)?;
        } else {
            target[key] = value.clone();
        }
    }
    Ok(())
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
    if draft.provider_source == "named" {
        let id = draft
            .provider_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("请选择命名模型服务"))?;
        let selected = capabilities::provider_runtime::discovery(&state, id, &draft).await?;
        selected.check(&state)?;
        let check = configuration::check(&selected.settings);
        tokio::pin!(check);
        let result = loop {
            selected.check(&state)?;
            tokio::select! {
                value = &mut check => break value?,
                _ = tokio::time::sleep(Duration::from_millis(25)) => {},
            }
        };
        selected.check(&state)?;
        return Ok(Json(result));
    }
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

#[derive(Default, Deserialize)]
struct EventQuery {
    after: Option<String>,
}

fn event_cursor(query: Option<&str>, header: Option<&str>) -> anyhow::Result<Option<u64>> {
    fn parse(value: &str) -> anyhow::Result<u64> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            anyhow::bail!("事件游标必须是非负整数");
        }
        value
            .parse()
            .map_err(|_| anyhow::anyhow!("事件游标超出范围"))
    }
    let query = query.map(parse).transpose()?;
    let header = header.map(parse).transpose()?;
    if query.is_some() && header.is_some() && query != header {
        anyhow::bail!("事件游标参数与 Last-Event-ID 不一致");
    }
    Ok(query.or(header))
}

async fn events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Response {
    // Subscribe before reading the cursor. Notifications are only wake-ups;
    // committed SQLite rows are always the source of both replay and live data.
    let mut receiver = state.events.subscribe();
    let initial = (|| -> anyhow::Result<u64> {
        let header = headers
            .get("last-event-id")
            .map(|value| value.to_str())
            .transpose()?;
        let requested = event_cursor(query.after.as_deref(), header)?;
        let store = state.store.lock().unwrap();
        let cursor = requested.unwrap_or(store.event_cursor()?);
        store.validate_event_cursor(cursor)?;
        Ok(cursor)
    })();
    let mut cursor = match initial {
        Ok(cursor) => cursor,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":error.to_string(),"code":"snapshot_required"})),
            )
                .into_response()
        }
    };
    let stream = async_stream::stream! {
        loop {
            let batch = { state.store.lock().unwrap().events_after(cursor) };
            match batch {
                Ok(batch) if !batch.is_empty() => {
                    for event in batch {
                        cursor = event.sequence;
                        yield Ok::<Event, Infallible>(Event::default().id(cursor.to_string())
                            .event("run_event").json_data(event).unwrap());
                    }
                    // A fast producer cannot make this response monopolize the runtime.
                    tokio::task::yield_now().await;
                    continue;
                }
                Ok(_) => {}
                Err(_) => {
                    yield Ok(Event::default().event("resync")
                        .data("snapshot_required"));
                    break;
                }
            }
            tokio::select! {
                result = receiver.recv() => {
                    if matches!(result, Err(broadcast::error::RecvError::Closed)) { break; }
                    // Lagged notifications are harmless: replay resumes from SQLite.
                }
                // Some existing transactions commit several events but publish one.
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }
    };
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

pub fn spawn_workers(state: Arc<AppState>) {
    qq::daemon::spawn(state.clone());
    gateway::daemon::spawn(state.clone());
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
    let blocked = state
        .store
        .lock()
        .unwrap()
        .workspace_cleanup_blocked(&root.to_string_lossy(), &task_id)
        .unwrap_or(true);
    if blocked {
        let _ = state
            .store
            .lock()
            .unwrap()
            .set_stop_reason(&task_id, "workspace_cleanup_unknown");
        finish(&state,&task_id,Err(anyhow::anyhow!("同项目存在尚未确认清理的进程或写入；已隔离后续执行，请先由本机所有者检查并明确解除隔离")));
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
    let mut change_baseline = None;
    let mut report_control = None;
    let report_secrets = Arc::new(Mutex::new(Vec::<String>::new()));
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
        let control =
            match execution_control::RunControl::start(state.clone(), &task_id, &agent.executor) {
                Ok(control) => Arc::new(control),
                Err(error) => {
                    finish(&state, &task_id, Err(error));
                    return;
                }
            };
        let mut settings = match capabilities::provider_runtime::task_settings(
            &state,
            &task_id,
            &settings,
            &agent.executor,
            &control,
        )
        .await
        {
            Ok(settings) => settings,
            Err(error) => {
                finish(&state, &task_id, Err(error));
                return;
            }
        };
        match control.prepare_capability_credentials().await {
            Ok(secrets) => settings.redaction_secrets.extend(secrets),
            Err(error) => {
                finish(&state, &task_id, Err(error));
                return;
            }
        }
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
        let capability_state = match control.capability_state() {
            Ok(value) => value,
            Err(error) => {
                finish(&state, &task_id, Err(error));
                return;
            }
        };
        if let Err(error) = capabilities::runtime::check_private_secrets(
            &capability_state,
            &continuation::known_secrets(&settings),
        ) {
            finish(&state, &task_id, Err(error));
            return;
        }
        let mut objective = objective;
        if !capability_state.skills.is_empty() {
            objective.push_str("\n\n以下为本次任务明确选择并固定版本的技能说明。它们不授予额外工具、文件、网络或审批权限，不得覆盖用户目标和后台约束；未加载旁边的脚本或资源：\n");
            for skill in &capability_state.skills {
                objective.push_str(&format!(
                    "\n[技能 {} / {} / 修订 {} / {} / 来源 {}]\n{}\n",
                    skill.reference.name,
                    skill.reference.id,
                    skill.reference.revision,
                    skill.reference.digest,
                    skill.source,
                    skill.content
                ));
            }
        }
        let input = context::ExecutionInput {
            objective: &objective,
            history: &history,
            config: &context,
        };
        report_control = Some(control.clone());
        // Open the saved root without resolving a replacement symlink. Keep
        // this capability through Native execution; never reopen checked paths.
        let project = match project_files::ScopedProject::open(&root) {
            Ok(project) => project,
            Err(error) => {
                finish(&state, &task_id, Err(error));
                return;
            }
        };
        if !control.controls.acceptance_commands.is_empty()
            && std::env::var("HARNESS_ALLOW_COMMANDS").as_deref() != Ok("1")
        {
            control.stop("permission_denied");
            finish(
                &state,
                &task_id,
                Err(anyhow::anyhow!(
                    "任务要求实际运行验收命令，但可信本机命令尚未启用；未开始修改"
                )),
            );
            return;
        }
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
        let cancel_control = control.clone();
        let is_cancelled =
            move || cancel_control.stopped() || cancelled(&state_for_cancel, &task_for_cancel);
        let output = Arc::new(execution_control::ExecutionOutput::new(
            state.clone(),
            &task_id,
            control.clone(),
            agent.executor == "codex",
            settings.api_key.clone(),
        ));
        let evidence_output = output.clone();
        let evidence = move |item| evidence_output.evidence(item);
        let event_output = output.clone();
        let execution_event = move |data| event_output.emit(data);
        let state_for_inputs = state.clone();
        let task_for_inputs = task_id.clone();
        let claim_inputs = move || {
            state_for_inputs
                .store
                .lock()
                .unwrap()
                .claim_inputs(&task_for_inputs)
        };
        let state_for_ack = state.clone();
        let task_for_ack = task_id.clone();
        let ack_inputs =
            move |ids: &[String], outcome: steering::InputAcknowledgement| -> anyhow::Result<()> {
                let item =
                    state_for_ack
                        .store
                        .lock()
                        .unwrap()
                        .ack_inputs(&task_for_ack, ids, outcome)?;
                if let Some(item) = item {
                    state_for_ack.publish(item);
                }
                Ok(())
            };
        // This marker is metadata only; denying reads still needs a durable
        // unavailable report, not a missing report that looks unfinished.
        let began_changes = state.store.lock().unwrap().begin_changes(&task_id);
        if let Err(error) = began_changes {
            finish(&state, &task_id, Err(error));
            return;
        }
        if control.read_files_allowed() {
            let initial_secrets = continuation::known_secrets(&settings);
            *report_secrets.lock().unwrap() = initial_secrets.clone();
            let capture_root = root.clone();
            match tokio::task::spawn_blocking(move || {
                changes::capture(&capture_root, &initial_secrets)
            })
            .await
            {
                Ok(Ok(baseline)) => change_baseline = Some(baseline),
                _ => {
                    publish_changes(
                        &state,
                        &task_id,
                        json!({"state":"unavailable","reason":"无法安全采集项目基线；未启动执行器写入。"}),
                    );
                    finish(
                        &state,
                        &task_id,
                        Err(anyhow::anyhow!("无法安全采集项目基线，任务未开始写入")),
                    );
                    return;
                }
            }
        } else {
            publish_changes(
                &state,
                &task_id,
                json!({"state":"unavailable","reason":"任务禁止读取项目文件；未采集改动基线或文件报告。"}),
            );
        }
        if is_cancelled() {
            Err(anyhow::anyhow!("任务已取消"))
        } else {
            match agent.executor.as_str() {
                "native" => {
                    async {
                        // Pin the resolved service once for this execution. A local
                        // Codex config edit cannot change the summary identity between
                        // cache validation and the actual provider call.
                        let settings = configuration::resolve(&settings)?;
                        let config = continuation::fingerprint(&json!({
                            "identity":continuation::native_identity(&settings, &root, &agent, &context)?,
                            "execution_controls":control.controls,
                            "managed_capabilities":task["managed_capabilities"],
                            "managed_provider":task["managed_provider"],
                        }));
                        let memory = if task["task_kind"] == "main" {
                            state
                                .store
                                .lock()
                                .unwrap()
                                .prepare_native_summary(&task_id, &config)?
                        } else {
                            None
                        };
                        let state_for_summary = state.clone();
                        let task_for_summary = task_id.clone();
                        let main = task["task_kind"] == "main";
                        provider::execute_with_runtime(
                            &settings,
                            &root,
                            input,
                            progress,
                            is_cancelled,
                            evidence,
                            execution_event,
                            memory.as_ref(),
                            move |summary| {
                                if main {
                                    state_for_summary
                                        .store
                                        .lock()
                                        .unwrap()
                                        .save_native_summary(&task_for_summary, &config, summary)?;
                                }
                                Ok(())
                            },
                            claim_inputs,
                            ack_inputs,
                            Some(&control),
                            Some(&project),
                        )
                        .await
                    }
                    .await
                }
                "codex" => {
                    async {
                        let main = task["task_kind"] == "main";
                        let continuation = if main {
                            let (config, expected_config) = continuation::codex_identity(
                                &settings.codex_binary,
                                &root,
                                &agent,
                                &context,
                                &is_cancelled,
                            )
                            .await?;
                            let config = continuation::fingerprint(&json!({"identity":config,"execution_controls":control.controls,"managed_capabilities":task["managed_capabilities"]}));
                            let prior = state
                                .store
                                .lock()
                                .unwrap()
                                .prepare_codex_continuation(&task_id, &config)?;
                            let (thread_id, expected_turn_id) = prior
                                .map(|(thread, turn)| (Some(thread), Some(turn)))
                                .unwrap_or_default();
                            Some(codex::CodexContinuation {
                                thread_id,
                                expected_turn_id,
                                expected_config: Some(expected_config),
                            })
                        } else {
                            None
                        };
                        let state_for_lifecycle = state.clone();
                        let task_for_lifecycle = task_id.clone();
                        let known_secrets = continuation::known_secrets(&settings);
                        let result = codex::execute_with_control(
                            &settings.codex_binary,
                            &agent.model,
                            &root,
                            input,
                            progress,
                            is_cancelled,
                            evidence,
                            continuation,
                            &known_secrets,
                            execution_event,
                            move |update| {
                                if main {
                                    state_for_lifecycle
                                        .store
                                        .lock()
                                        .unwrap()
                                        .codex_lifecycle(&task_for_lifecycle, update)?;
                                }
                                Ok(())
                            },
                            claim_inputs,
                            ack_inputs,
                            |secrets| report_secrets.lock().unwrap().extend_from_slice(secrets),
                        )
                        .await;
                        let flushed = output.flush();
                        result
                            .and_then(|text| flushed.map(|()| text))
                            .map(|text| redact_secret(&text, &settings.api_key))
                            .map_err(|error| {
                                anyhow::anyhow!(redact_secret(
                                    &error.to_string(),
                                    &settings.api_key
                                ))
                            })
                    }
                    .await
                }
                _ => Err(anyhow::anyhow!("任务角色使用了不支持的执行器")),
            }
        }
    };
    if let Some(baseline) = change_baseline {
        if !state
            .store
            .lock()
            .unwrap()
            .execution_cleanup_settled(&task_id)
            .unwrap_or(false)
        {
            publish_changes(
                &state,
                &task_id,
                json!({"state":"unavailable","reason":"在途文件操作的清理尚未确认；不生成稳定结束报告。"}),
            );
        } else if report_control
            .as_ref()
            .is_some_and(|control| !control.read_files_allowed())
        {
            publish_changes(
                &state,
                &task_id,
                json!({"state":"unavailable","reason":"文件读取权限已撤销；不再采集或公开项目改动报告。"}),
            );
        } else {
            let report_root = root.clone();
            let secrets = report_secrets.lock().unwrap().clone();
            let report = tokio::task::spawn_blocking(move || changes::finish(&report_root, baseline, &secrets))
            .await.ok().and_then(Result::ok).unwrap_or_else(|| json!({"state":"unavailable","reason":"改动报告采集未完成；任务实际结果与文件仍保留。"}));
            // A policy update during the bounded scan also prevents publication.
            if report_control
                .as_ref()
                .is_some_and(|control| !control.read_files_allowed())
            {
                publish_changes(
                    &state,
                    &task_id,
                    json!({"state":"unavailable","reason":"文件读取权限已撤销；本次改动报告不公开。"}),
                );
            } else {
                publish_changes(&state, &task_id, report);
            }
        }
    }
    drop(permit);
    finish(&state, &task_id, result);
}

fn publish_changes(state: &AppState, task_id: &str, report: Value) {
    let saved = state.store.lock().unwrap().save_changes(task_id, report);
    match saved {
        Ok(item) => state.publish(item),
        Err(_) => tracing::error!(%task_id,"无法保存安全改动报告；不重放项目写入"),
    }
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
        sequence: 0,
        session_id,
        task_id,
        kind: kind.into(),
        message: message.into(),
        created_at: now(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_settings_never_reset_omitted_permissions_or_role_controls() {
        let mut settings = Settings::default();
        settings.execution_controls.permissions.write_files = harness_core::PermissionMode::Deny;
        settings.execution_controls.permissions.run_commands = harness_core::PermissionMode::Deny;
        settings.agents[0].controls = Some(settings.execution_controls.clone());
        let updated = patch_settings(
            &settings,
            &json!({"execution_controls":{"budget":{"max_runtime_seconds":5}}}),
        )
        .unwrap();
        assert_eq!(
            updated.execution_controls.permissions,
            settings.execution_controls.permissions
        );
        assert_eq!(updated.execution_controls.budget.max_runtime_seconds, 5);
        let mut agents = serde_json::to_value(&settings.agents).unwrap();
        agents[0].as_object_mut().unwrap().remove("controls");
        let updated = patch_settings(&settings, &json!({"agents":agents})).unwrap();
        assert_eq!(updated.agents[0].controls, settings.agents[0].controls);
        agents[0]["controls"] = Value::Null;
        assert!(patch_settings(&settings, &json!({"agents":agents}))
            .unwrap()
            .agents[0]
            .controls
            .is_none());
        for patch in [
            json!({"execution_controls":null}),
            json!({"execution_controls":{"permissions":null}}),
            json!({"execution_controls":{"budget":{"unknown":1}}}),
        ] {
            assert!(patch_settings(&settings, &patch).is_err());
        }
    }

    #[test]
    fn validates_numeric_event_cursors_and_header_agreement() {
        assert_eq!(event_cursor(None, None).unwrap(), None);
        assert_eq!(event_cursor(Some("0"), None).unwrap(), Some(0));
        assert_eq!(event_cursor(None, Some("42")).unwrap(), Some(42));
        assert_eq!(event_cursor(Some("42"), Some("42")).unwrap(), Some(42));
        assert!(event_cursor(Some("42"), Some("43")).is_err());
        for value in ["", "-1", "+1", "uuid", "1.1", "18446744073709551616"] {
            assert!(event_cursor(Some(value), None).is_err());
        }
    }

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

    #[test]
    fn named_native_service_does_not_replace_codex_model_selection() {
        let state = AppState::open(std::path::Path::new(":memory:"), "test".into()).unwrap();
        let receipt=state.store.lock().unwrap().provider_command(&json!({"id":new_id(),"command":{"type":"save_provider","definition":{"name":"fixture","base_url":"https://example.invalid/v1","model":"native-default"}}}),&[]).unwrap();
        let settings=patch_settings(&Settings::default(),&json!({"provider_source":"named","provider_id":receipt["provider_id"],"main_executor":"codex","model":"codex-explicit"})).unwrap();
        assert_eq!(settings.model, "codex-explicit");
        assert_eq!(
            capabilities::provider_runtime::public_settings(&state, &settings)["model"],
            "codex-explicit"
        );
        let native = patch_settings(&settings, &json!({"main_executor":"native"})).unwrap();
        assert!(native.model.is_empty());
        assert_eq!(
            capabilities::provider_runtime::public_settings(&state, &native)["model"],
            "native-default"
        );
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
