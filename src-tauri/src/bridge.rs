use crate::event_stream::{self, Sink, Subscriptions};
use harness_core::{data_dir, DaemonInfo};
use reqwest::{Client, Method, Url};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tauri::{ipc::Channel, AppHandle, Manager, State, WebviewWindow};
use tokio::sync::Mutex;

pub struct Bridge {
    client: Client,
    startup: Mutex<()>,
    subscriptions: Arc<Subscriptions>,
}

impl Bridge {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("无法创建本机服务客户端"),
            startup: Mutex::new(()),
            subscriptions: Arc::new(Subscriptions::default()),
        }
    }

    pub async fn ensure_daemon(&self, app: &AppHandle) -> Result<DaemonInfo, String> {
        let _guard = self.startup.lock().await;
        if let Ok(info) = read_daemon_info().await {
            if self.healthy(&info).await? {
                return Ok(info);
            }
        }

        let binary = daemon_binary(app)?;
        start_detached(&binary)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline {
            if let Ok(info) = read_daemon_info().await {
                if self.healthy(&info).await? {
                    return Ok(info);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("后台服务未能在 6 秒内就绪，请检查安装文件和本机服务端口。".into())
    }

    async fn healthy(&self, info: &DaemonInfo) -> Result<bool, String> {
        let Ok(base) = validate_base_url(&info.base_url) else {
            return Ok(false);
        };
        let Ok(url) = base.join("health") else {
            return Ok(false);
        };
        let Ok(response) = self
            .client
            .get(url)
            .timeout(Duration::from_millis(400))
            .send()
            .await
        else {
            return Ok(false);
        };
        if !response.status().is_success() {
            return Ok(false);
        }
        let Ok(health) = response.json::<Value>().await else {
            return Ok(false);
        };
        if health["version"].as_str() != Some(env!("CARGO_PKG_VERSION")) {
            return Err("检测到仍在运行的旧版后台服务。请先结束旧版后台进程，再重新打开桌面；已有会话会保留。".into());
        }
        Ok(health["status"] == "ok")
    }
}

impl Bridge {
    pub fn stop_window(&self, label: &str) {
        self.subscriptions.stop_window(label);
    }
}

#[tauri::command]
pub async fn start_event_stream(
    app: AppHandle,
    window: WebviewWindow,
    bridge: State<'_, Bridge>,
    subscription_id: String,
    after: Option<u64>,
    channel: Channel<Value>,
) -> Result<(), String> {
    if after.is_some_and(|cursor| cursor > i64::MAX as u64) {
        return Err("事件游标超出范围".into());
    }
    let label = window.label().to_owned();
    let mut subscription = bridge.subscriptions.start(&label, &subscription_id)?;
    let generation = subscription.generation;
    let registry = bridge.subscriptions.clone();
    tauri::async_runtime::spawn(async move {
        let send_registry = registry.clone();
        let send_label = label.clone();
        let send_id = subscription_id.clone();
        let mut sink = Sink::new(subscription.credits, move |value| {
            send_registry.send(&send_label, &send_id, generation, || {
                channel.send(value).is_ok()
            })
        });
        tokio::select! {
            biased;
            _ = subscription.cancel.changed() => {},
            _ = async {
                // Keep the existing daemon version check; do not silently stream
                // from a known incompatible daemon after ensure_daemon rejects it.
                match app.state::<Bridge>().ensure_daemon(&app).await {
                    Ok(_) => event_stream::run(&data_dir().join("daemon.json"), after, sink).await,
                    Err(message) => { sink.status("error", &message); }
                }
            } => {},
        }
        registry.finish(&label, &subscription_id, generation);
    });
    Ok(())
}

#[tauri::command]
pub fn stop_event_stream(
    window: WebviewWindow,
    bridge: State<'_, Bridge>,
    subscription_id: String,
) {
    bridge.subscriptions.stop(window.label(), &subscription_id);
}

#[tauri::command]
pub fn ack_event_stream(
    window: WebviewWindow,
    bridge: State<'_, Bridge>,
    subscription_id: String,
    delivery_id: u64,
) {
    bridge
        .subscriptions
        .acknowledge(window.label(), &subscription_id, delivery_id);
}

#[tauri::command]
pub async fn api_request(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<Value, String> {
    let method = validate_route(&method, &path)?;
    let info = bridge.ensure_daemon(&app).await?;
    let base = validate_base_url(&info.base_url)?;
    let url = base
        .join(&path)
        .map_err(|_| "本机接口路径无效".to_string())?;
    let mut request = bridge.client.request(method, url).bearer_auth(&info.token);
    if path == "/api/v1/settings/check" {
        request = request.timeout(Duration::from_secs(35));
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    // A failed mutation is never retried here: callers reuse their command ID.
    let response = request
        .send()
        .await
        .map_err(|error| format!("无法连接本机后台服务：{error}"))?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|_| "后台服务返回的数据格式无效".to_string())?;
    if status.is_success() {
        Ok(value)
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("后台服务拒绝了此请求")
            .to_string())
    }
}

async fn read_daemon_info() -> Result<DaemonInfo, String> {
    let contents = tokio::fs::read(data_dir().join("daemon.json"))
        .await
        .map_err(|_| "未找到本机后台服务信息".to_string())?;
    let info: DaemonInfo =
        serde_json::from_slice(&contents).map_err(|_| "本机后台服务信息格式无效".to_string())?;
    validate_base_url(&info.base_url)?;
    if info.token.is_empty() {
        return Err("缺少本机后台服务凭据".into());
    }
    Ok(info)
}

fn validate_base_url(base_url: &str) -> Result<Url, String> {
    event_stream::validate_base_url(base_url)
}

fn validate_route(method: &str, path: &str) -> Result<Method, String> {
    match (method, path) {
        (
            "GET",
            "/api/v1/snapshot"
            | "/api/v1/settings"
            | "/api/v1/codex/status"
            | "/api/v1/provider/local-codex",
        ) => Ok(Method::GET),
        ("POST", "/api/v1/commands" | "/api/v1/models" | "/api/v1/settings/check") => {
            Ok(Method::POST)
        }
        ("PUT", "/api/v1/settings") => Ok(Method::PUT),
        _ => Err("不允许使用此本机接口路径或请求方法".into()),
    }
}

fn daemon_binary(app: &AppHandle) -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "harness-daemon.exe"
    } else {
        "harness-daemon"
    };
    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join(name));
        }
    }
    if let Ok(resources) = app.path().resource_dir() {
        candidates.push(resources.join(name));
        candidates.push(resources.join("binaries").join(name));
    }
    #[cfg(debug_assertions)]
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug")
            .join(name),
    );
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| "缺少后台服务程序，请先构建桌面附带服务再启动应用。".into())
}

fn start_detached(binary: &Path) -> Result<(), String> {
    let mut command = Command::new(binary);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A separate session lets the service survive desktop process termination.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动后台服务：{error}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_rejects_external_hosts_and_credential_urls() {
        for value in [
            "https://127.0.0.1:47371",
            "http://example.com",
            "http://127.0.0.1.evil.test",
            "http://owner:secret@127.0.0.1:47371",
            "http://127.0.0.1:47371/elsewhere",
            "http://127.0.0.1:47371/?target=example.com",
        ] {
            assert!(validate_base_url(value).is_err(), "{value}");
        }
        assert!(validate_base_url("http://127.0.0.1:47371").is_ok());
    }

    #[test]
    fn bridge_accepts_only_known_method_and_route_pairs() {
        assert!(validate_route("GET", "/api/v1/snapshot").is_ok());
        assert!(validate_route("POST", "/api/v1/commands").is_ok());
        assert!(validate_route("POST", "/api/v1/models").is_ok());
        assert!(validate_route("GET", "/api/v1/provider/local-codex").is_ok());
        assert!(validate_route("POST", "/api/v1/settings/check").is_ok());
        assert!(validate_route("PUT", "/api/v1/settings").is_ok());
        assert!(validate_route("GET", "/api/v1/codex/status").is_ok());
        for (method, path) in [
            ("DELETE", "/api/v1/commands"),
            ("GET", "/api/v1/../health"),
            ("GET", "//example.com/api/v1/snapshot"),
            ("GET", "/api/v1/snapshot?url=http://example.com"),
            ("GET", "/health"),
            ("POST", "/api/v1/codex/status"),
        ] {
            assert!(validate_route(method, path).is_err());
        }
    }
}
