mod coordinator;
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
            api_key: std::env::var("HARNESS_API_KEY").unwrap_or_default(),
        }
    }
}

impl Settings {
    pub fn configured(&self) -> bool {
        !self.model.trim().is_empty() && !self.base_url.trim().is_empty()
    }

    fn public(&self) -> Value {
        json!({
            "base_url": self.base_url, "model": self.model,
            "execution_mode": self.execution_mode, "configured": self.configured(),
            "api_key_configured": !self.api_key.is_empty()
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
            }
        }
    };
    if cancelled(&state, &task_id) {
        return;
    }
    let result = if task["execution_mode"] == "demo" {
        demo(&state, &task_id).await
    } else {
        let settings = state.settings.read().unwrap().clone();
        let state_for_log = state.clone();
        let task_for_log = task_id.clone();
        let task_for_cancel = task_id.clone();
        let state_for_cancel = state.clone();
        provider::execute(
            &settings,
            &root,
            task["objective"]
                .as_str()
                .or_else(|| task["title"].as_str())
                .unwrap_or_default(),
            move |detail| {
                if let Ok(event) = state_for_log
                    .store
                    .lock()
                    .unwrap()
                    .progress(&task_for_log, &detail)
                {
                    state_for_log.publish(event);
                }
            },
            move || cancelled(&state_for_cancel, &task_for_cancel),
        )
        .await
    };
    drop(permit);
    finish(&state, &task_id, result);
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
