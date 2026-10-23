use harness_core::{data_dir, DaemonInfo};
use reqwest::{Client, Method, Url};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;

pub struct Bridge {
    client: Client,
    startup: Mutex<()>,
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
    let url = Url::parse(base_url).map_err(|_| "本机服务地址无效".to_string())?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() == Some(0)
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("本机服务必须使用 HTTP 回环地址".into());
    }
    Ok(url)
}

fn validate_route(method: &str, path: &str) -> Result<Method, String> {
    match (method, path) {
        ("GET", "/api/v1/snapshot" | "/api/v1/settings" | "/api/v1/codex/status") => {
            Ok(Method::GET)
        }
        ("POST", "/api/v1/commands") => Ok(Method::POST),
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
