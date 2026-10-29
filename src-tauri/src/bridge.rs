#[path = "history.rs"]
mod history;
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
use tauri::{ipc::Channel, AppHandle, Emitter, Manager, State, WebviewWindow};
use tokio::sync::Mutex;

pub struct Bridge {
    client: Client,
    startup: Mutex<crate::startup::Startup>,
    startup_observation: crate::startup::Observations,
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
            startup: Mutex::new(crate::startup::Startup::default()),
            startup_observation: crate::startup::Observations::default(),
            subscriptions: Arc::new(Subscriptions::default()),
        }
    }

    pub async fn ensure_daemon(&self, app: &AppHandle) -> Result<(DaemonInfo, u16), String> {
        let result = self.ensure_daemon_inner(app).await;
        // Report EVERY exit, including preflight metadata/incompatibility and
        // launch failures; bounded history errors must not hide the reason.
        self.report_startup(app, crate::startup::result_event(&result));
        result
    }
    fn report_startup(&self, app: &AppHandle, value: Value) {
        let _ = app.emit("daemon-status", self.startup_observation.record(value));
    }
    async fn ensure_daemon_inner(&self, app: &AppHandle) -> Result<(DaemonInfo, u16), String> {
        let mut startup = self.startup.lock().await;
        if let Some(info) = read_daemon_info().await? {
            if let Some(protocol) = self
                .healthy(&info)
                .await
                .map_err(|error| error.to_string())?
            {
                return Ok((info, protocol));
            }
        }
        startup.start_once(|| start_detached(&daemon_binary(app)?))?;
        let mut reported = None;
        startup
            .observe(
                || async {
                    let Some(info) = read_daemon_info().await? else {
                        return Ok(None);
                    };
                    match self.healthy(&info).await {
                        Ok(value) => Ok(value.map(|protocol| (info, protocol))),
                        Err(crate::compatibility::ProbeError::Unavailable) => Ok(None),
                        Err(error) => Err(error.to_string()),
                    }
                },
                crate::startup::READY_BUDGET,
                |elapsed| {
                    if reported != Some(elapsed) {
                        reported = Some(elapsed);
                        let message = format!(
                            "正在等待本机后台就绪并校验协议（{}秒，最多30秒）；不会重复启动进程。",
                            elapsed
                        );
                        self.report_startup(
                            app,
                            serde_json::json!({"status":"starting","message":message}),
                        );
                    }
                },
            )
            .await
    }

    async fn healthy(
        &self,
        info: &DaemonInfo,
    ) -> Result<Option<u16>, crate::compatibility::ProbeError> {
        let base = validate_base_url(&info.base_url)
            .map_err(|_| crate::compatibility::ProbeError::Incompatible)?;
        crate::compatibility::probe(&self.client, &base, &info.token)
            .await
            .map(|value| value.map(|v| v.protocol))
    }
}

impl Bridge {
    pub fn stop_window(&self, label: &str) {
        self.subscriptions.stop_window(label);
    }
}

#[tauri::command]
pub fn daemon_startup_status(bridge: State<'_, Bridge>) -> Value {
    bridge.startup_observation.snapshot()
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
    let bounded = (method == Method::GET)
        .then(|| history::limit(&path))
        .flatten();
    let (info, protocol) = bridge.ensure_daemon(&app).await.map_err(|error| {
        if bounded.is_some() {
            "HISTORY_UNAVAILABLE".into()
        } else {
            error
        }
    })?;
    let base = validate_base_url(&info.base_url)?;
    let url = base
        .join(&path)
        .map_err(|_| "本机接口路径无效".to_string())?;
    let mut request = bridge
        .client
        .request(method, url)
        .bearer_auth(&info.token)
        .header("X-Self-Agent-Protocol", protocol.to_string());
    if path == "/api/v1/settings/check" {
        request = request.timeout(Duration::from_secs(35));
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    // A failed mutation is never retried here: callers reuse their command ID.
    let response = request.send().await.map_err(|_| {
        if bounded.is_some() {
            "HISTORY_UNAVAILABLE"
        } else {
            "无法连接本机后台服务"
        }
        .to_owned()
    })?;
    history::response(response, bounded).await
}

async fn read_daemon_info() -> Result<Option<DaemonInfo>, String> {
    crate::startup::read_info(&data_dir().join("daemon.json")).await
}

fn validate_base_url(base_url: &str) -> Result<Url, String> {
    event_stream::validate_base_url(base_url)
}

fn task_path(path: &str, suffix: &str) -> bool {
    path.strip_prefix("/api/v1/tasks/")
        .and_then(|part| part.strip_suffix(suffix))
        .is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

fn provider_mutation(path: &str) -> bool {
    [
        ("/api/v1/providers/", "/credential"),
        ("/api/v1/providers/", "/credential-delete"),
        ("/api/v1/credential-operations/", "/cleanup"),
    ]
    .iter()
    .any(|(prefix, suffix)| {
        path.strip_prefix(prefix)
            .and_then(|part| part.strip_suffix(suffix))
            .is_some_and(|id| {
                id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            })
    })
}

fn scoped_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}
fn mcp_credential_route(method: &str, path: &str) -> bool {
    if let Some(rest) = path.strip_prefix("/api/v1/capability-credential-operations/") {
        return (method == "GET" && scoped_uuid(rest))
            || (method == "POST" && rest.strip_suffix("/cleanup").is_some_and(scoped_uuid));
    }
    if method != "POST" {
        return false;
    }
    let Some(rest) = path.strip_prefix("/api/v1/capabilities/mcp/") else {
        return false;
    };
    let Some((id, slot)) = rest.split_once("/credentials/") else {
        return false;
    };
    let slot = slot.strip_suffix("/delete").unwrap_or(slot);
    scoped_uuid(id)
        && !slot.is_empty()
        && slot.len() <= 64
        && slot.as_bytes()[0].is_ascii_uppercase()
        && slot
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn qq_route(method: &str, path: &str) -> bool {
    if (method == "GET" && path == "/api/v1/qq")
        || (method == "POST" && path == "/api/v1/qq/commands")
    {
        return true;
    }
    if method == "GET" {
        return ["/api/v1/qq/commands/", "/api/v1/qq/credential-operations/"]
            .iter()
            .any(|prefix| path.strip_prefix(prefix).is_some_and(scoped_uuid));
    }
    if method == "POST"
        && path
            .strip_prefix("/api/v1/qq/credentials/cleanup/")
            .is_some_and(scoped_uuid)
    {
        return true;
    }
    matches!(method, "POST" | "DELETE")
        && path
            .strip_prefix("/api/v1/qq/credentials/")
            .is_some_and(scoped_uuid)
}
fn gateway_route(method: &str, path: &str) -> bool {
    if (method == "GET" && path == "/api/v1/gateway")
        || (method == "POST" && path == "/api/v1/gateway/commands")
    {
        return true;
    }
    if method == "GET" {
        return [
            "/api/v1/gateway/commands/",
            "/api/v1/gateway/credential-operations/",
        ]
        .iter()
        .any(|prefix| path.strip_prefix(prefix).is_some_and(scoped_uuid));
    }
    if method == "POST"
        && path
            .strip_prefix("/api/v1/gateway/credentials/cleanup/")
            .is_some_and(scoped_uuid)
    {
        return true;
    }
    matches!(method, "POST" | "DELETE")
        && path
            .strip_prefix("/api/v1/gateway/credentials/")
            .is_some_and(scoped_uuid)
}

fn reliability_route(method: &str, path: &str) -> bool {
    (method == "GET"
        && matches!(
            path,
            "/api/v1/reliability" | "/api/v1/reliability/diagnostics"
        ))
        || (method == "POST" && path == "/api/v1/reliability/backups")
        || (method == "GET"
            && path
                .strip_prefix("/api/v1/reliability/backups/")
                .is_some_and(scoped_uuid))
}

fn validate_route(method: &str, path: &str) -> Result<Method, String> {
    if method == "GET"
        && (path == "/api/v1/reliability/updates"
            || path
                .strip_prefix("/api/v1/reliability/updates/verifications/")
                .is_some_and(scoped_uuid))
    {
        return Ok(Method::GET);
    }
    if method == "GET" && history::limit(path).is_some() {
        return Ok(Method::GET);
    }
    if reliability_route(method, path) {
        return Ok(if method == "GET" {
            Method::GET
        } else {
            Method::POST
        });
    }
    if qq_route(method, path) || gateway_route(method, path) {
        return Ok(match method {
            "GET" => Method::GET,
            "DELETE" => Method::DELETE,
            _ => Method::POST,
        });
    }
    if mcp_credential_route(method, path) {
        return Ok(if method == "GET" {
            Method::GET
        } else {
            Method::POST
        });
    }
    if method == "GET"
        && path
            .strip_prefix("/api/v1/credential-operations/")
            .is_some_and(|id| {
                id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            })
    {
        return Ok(Method::GET);
    }

    if method == "POST" && provider_mutation(path) {
        return Ok(Method::POST);
    }

    if method == "GET" && task_path(path, "/changes") {
        return Ok(Method::GET);
    }
    if method == "POST" && task_path(path, "/cleanup-resolution") {
        return Ok(Method::POST);
    }
    match (method, path) {
        (
            "GET",
            "/api/v1/snapshot"
            | "/api/v1/settings"
            | "/api/v1/capabilities"
            | "/api/v1/providers"
            | "/api/v1/codex/status"
            | "/api/v1/provider/local-codex",
        ) => Ok(Method::GET),
        (
            "POST",
            "/api/v1/commands"
            | "/api/v1/capabilities/commands"
            | "/api/v1/providers/commands"
            | "/api/v1/models"
            | "/api/v1/settings/check",
        ) => Ok(Method::POST),
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

fn start_detached(binary: &Path) -> Result<Arc<std::sync::atomic::AtomicBool>, String> {
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
    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marker = exited.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        marker.store(true, std::sync::atomic::Ordering::Release);
    });
    Ok(exited)
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
    fn history_routes_are_owner_get_only() {
        for path in [
            "/api/v1/tasks/t",
            "/api/v1/history/tasks?session_id=s&cursor=c",
            "/api/v1/history/sessions/s/messages?cursor=c",
        ] {
            assert!(validate_route("GET", path).is_ok());
            for method in ["POST", "PUT", "DELETE", "PATCH"] {
                assert!(validate_route(method, path).is_err());
            }
        }
        for path in [
            "/api/v1/history/tasks?cursor=a&cursor=b",
            "/api/v1/tasks/a%2fb",
            "/api/v1/history/tasks?owner=providers",
            "/api/v1/history/qq",
        ] {
            assert!(validate_route("GET", path).is_err());
        }
    }

    #[test]
    fn bridge_accepts_only_known_method_and_route_pairs() {
        assert!(validate_route("GET", "/api/v1/snapshot").is_ok());
        assert!(validate_route("GET", "/api/v1/snapshot?window=1").is_ok());
        assert!(validate_route("GET", "/api/v1/snapshot?window=2").is_err());
        let id = "94a846e4-7ac2-46d7-ac1d-fde573fd6e12";
        assert!(validate_route("GET", "/api/v1/providers").is_ok());
        assert!(validate_route("POST", "/api/v1/providers/commands").is_ok());
        for suffix in ["credential", "credential-delete"] {
            let path = format!("/api/v1/providers/{id}/{suffix}");
            assert!(validate_route("POST", &path).is_ok());
            assert!(validate_route("GET", &path).is_err());
            assert!(validate_route("POST", &format!("{path}?secret=value")).is_err());
        }
        assert!(validate_route("GET", &format!("/api/v1/credential-operations/{id}")).is_ok());
        assert!(validate_route(
            "POST",
            &format!("/api/v1/credential-operations/{id}/cleanup")
        )
        .is_ok());
        assert!(validate_route("POST", "/api/v1/providers/../credential").is_err());

        assert!(validate_route("GET", "/api/v1/capabilities").is_ok());
        assert!(validate_route("POST", "/api/v1/capabilities/commands").is_ok());
        assert!(validate_route("POST", "/api/v1/tasks/task-123/cleanup-resolution").is_ok());
        assert!(validate_route("GET", "/api/v1/tasks/task-123/cleanup-resolution").is_err());
        assert!(validate_route("POST", "/api/v1/tasks/../cleanup-resolution").is_err());
        assert!(validate_route("GET", "/api/v1/tasks/task-123/changes").is_ok());
        for path in [
            "/api/v1/tasks/../changes",
            "/api/v1/tasks//changes",
            "/api/v1/tasks/a%2fb/changes",
            "/api/v1/tasks/a/changes?x=1",
        ] {
            assert!(validate_route("GET", path).is_err());
        }
        assert!(validate_route("POST", "/api/v1/tasks/task-123/changes").is_err());
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

#[cfg(test)]
mod mcp_credential_route_tests {
    use super::*;
    #[test]
    fn scoped_mcp_credentials_routes_only() {
        let id = "11111111-1111-4111-8111-111111111111";
        let base = format!("/api/v1/capabilities/mcp/{id}/credentials/TOKEN");
        for path in [
            &base,
            &format!("{base}/delete"),
            &format!("/api/v1/capability-credential-operations/{id}/cleanup"),
        ] {
            assert!(validate_route("POST", path).is_ok());
            assert!(validate_route("GET", path).is_err());
        }
        assert!(validate_route(
            "GET",
            &format!("/api/v1/capability-credential-operations/{id}")
        )
        .is_ok());
        for path in [
            format!("{base}?secret=x"),
            format!("{base}/../PATH"),
            base.replace("TOKEN", "token"),
            base.replace(id, "../bad"),
        ] {
            assert!(validate_route("POST", &path).is_err());
        }
    }
}

#[cfg(test)]
mod qq_route_tests {
    use super::*;
    #[test]
    fn qq_scope_routes_have_exact_methods_and_ids() {
        let id = "11111111-1111-4111-8111-111111111111";
        for (method, path) in [
            ("GET", "/api/v1/qq".into()),
            ("POST", "/api/v1/qq/commands".into()),
            ("GET", format!("/api/v1/qq/commands/{id}")),
            ("GET", format!("/api/v1/qq/credential-operations/{id}")),
            ("POST", format!("/api/v1/qq/credentials/{id}")),
            ("DELETE", format!("/api/v1/qq/credentials/{id}")),
            ("POST", format!("/api/v1/qq/credentials/cleanup/{id}")),
        ] {
            assert!(validate_route(method, &path).is_ok());
            assert!(validate_route(method, &format!("{path}?owner=provider")).is_err());
        }
        for method in ["GET", "PUT", "PATCH"] {
            assert!(validate_route(method, &format!("/api/v1/qq/credentials/{id}")).is_err());
        }
        assert!(validate_route("POST", "/api/v1/qq/credentials/../providers").is_err());
    }
}

#[cfg(test)]
mod reliability_route_tests {
    use super::*;
    #[test]
    fn owner_backup_and_allowlisted_diagnostics_have_no_online_restore() {
        let id = "94a846e4-7ac2-46d7-ac1d-fde573fd6e12";
        for (method, path) in [
            ("GET", "/api/v1/reliability".to_string()),
            ("GET", "/api/v1/reliability/diagnostics".to_string()),
            ("POST", "/api/v1/reliability/backups".to_string()),
            ("GET", format!("/api/v1/reliability/backups/{id}")),
        ] {
            assert!(validate_route(method, &path).is_ok());
            assert!(validate_route(method, &format!("{path}?path=private")).is_err());
        }
        for (method, path) in [
            ("GET", "/api/v1/reliability/backups".to_string()),
            ("POST", "/api/v1/reliability/diagnostics".to_string()),
            ("POST", format!("/api/v1/reliability/backups/{id}")),
            ("DELETE", format!("/api/v1/reliability/backups/{id}")),
            ("POST", "/api/v1/reliability/restore".to_string()),
            ("POST", "/api/v1/reliability/stop".to_string()),
            ("GET", "/api/v1/reliability/backups/../snapshot".to_string()),
            (
                "GET",
                format!("/api/v1/reliability/backups/{}", id.to_uppercase()),
            ),
        ] {
            assert!(validate_route(method, &path).is_err());
        }
    }
}

#[cfg(test)]
mod gateway_route_tests {
    use super::*;
    #[test]
    fn gateway_scope_routes_have_exact_methods_and_ids() {
        let id = "11111111-1111-4111-8111-111111111111";
        for (method, path) in [
            ("GET", "/api/v1/gateway".into()),
            ("POST", "/api/v1/gateway/commands".into()),
            ("GET", format!("/api/v1/gateway/commands/{id}")),
            ("GET", format!("/api/v1/gateway/credential-operations/{id}")),
            ("POST", format!("/api/v1/gateway/credentials/{id}")),
            ("DELETE", format!("/api/v1/gateway/credentials/{id}")),
            ("POST", format!("/api/v1/gateway/credentials/cleanup/{id}")),
        ] {
            assert!(validate_route(method, &path).is_ok());
            assert!(validate_route(method, &format!("{path}?owner=provider")).is_err());
        }
        for method in ["GET", "PUT", "PATCH"] {
            assert!(validate_route(method, &format!("/api/v1/gateway/credentials/{id}")).is_err());
        }
        assert!(validate_route("POST", "/api/v1/gateway/credentials/../providers").is_err());
    }
}

#[cfg(test)]
mod updates_route_tests {
    use super::*;
    #[test]
    fn verifier_ui_is_read_only_without_staging_trust_or_install_proxy() {
        let id = "11111111-1111-4111-8111-111111111111";
        let base = "/api/v1/reliability/updates";
        for path in [base.to_string(), format!("{base}/verifications/{id}")] {
            assert!(validate_route("GET", &path).is_ok());
            for method in ["POST", "PUT", "DELETE"] {
                assert!(validate_route(method, &path).is_err());
            }
            assert!(validate_route("GET", &format!("{path}?path=private")).is_err());
        }
        for path in [
            format!("{base}/trust"),
            format!("{base}/verifications"),
            format!("{base}/verifications/{id}/artifact"),
            format!("{base}/verifications/{id}/cancel"),
            format!("{base}/stages/{id}/discard"),
            format!("{base}/install"),
        ] {
            for method in ["GET", "POST", "PUT", "DELETE"] {
                assert!(validate_route(method, &path).is_err());
            }
        }
    }
}
