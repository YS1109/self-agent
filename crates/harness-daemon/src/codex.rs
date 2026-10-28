//! A bounded Codex app-server client with persistent main-thread continuation.
//! Protocol data and stderr are
//! deliberately excluded from progress/errors because they can contain secrets.

use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};

use crate::context::ExecutionInput;
use crate::steering::InputAcknowledgement;
use anyhow::{bail, Context, Result};
#[cfg(all(test, unix))]
use harness_core::ModelContextConfig;
use harness_core::{CommandEvidence, ExecutionEvent, TaskSupplement};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{timeout, Instant},
};

#[path = "codex/stream_events.rs"]
mod stream_events;

#[path = "codex/config_identity.rs"]
mod config_identity;

#[path = "codex/steering.rs"]
mod steering;

const MAX_LINE: usize = 256 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_LINES: usize = 20_000;
const MAX_RESULT: usize = 128 * 1024;
const TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub(crate) async fn list_models(binary: &str) -> Result<Vec<Value>> {
    let mut command = Command::new(binary);
    command
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    crate::prepend_binary_directory(&mut command, binary);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("无法获取 Codex 模型，请检查 CLI 路径和安装状态"))?;
    let process_id = child.id();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let result = timeout(Duration::from_secs(12), async {
        send(&mut stdin, json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"self_agent","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":false}
        }})).await?;
        let mut reader = BufReader::new(stdout);
        let mut expected = 1u64;
        let mut total = 0usize;
        let mut rows = Vec::new();
        let mut cursors = HashSet::new();
        loop {
            let mut line = Vec::new();
            let length = (&mut reader).take((MAX_LINE + 1) as u64).read_until(b'\n', &mut line).await?;
            total += length;
            if length == 0 || length > MAX_LINE || total > 2 * 1024 * 1024 {
                bail!("Codex 模型列表连接已关闭或响应超出限制");
            }
            let response: Value = serde_json::from_slice(&line)
                .map_err(|_| anyhow::anyhow!("Codex 模型列表响应格式无效"))?;
            if response["method"].is_string() {
                if response.get("id").is_some() {
                    bail!("获取 Codex 模型列表时出现了不支持的交互请求");
                }
                continue;
            }
            if response["id"].as_u64() != Some(expected) {
                continue;
            }
            if response.get("error").is_some() {
                bail!("Codex 模型列表请求失败，请检查 CLI 版本和登录状态");
            }
            if expected == 1 {
                send(&mut stdin, json!({"method":"initialized"})).await?;
                expected = 2;
                send(&mut stdin, json!({"id":expected,"method":"model/list","params":{"limit":100,"includeHidden":false}})).await?;
                continue;
            }
            let page = response["result"]["data"].as_array()
                .context("Codex 模型列表响应缺少 data 数组")?;
            rows.extend(page.iter().cloned());
            if rows.len() > 2000 {
                bail!("Codex 模型列表超过 2000 项限制");
            }
            let Some(cursor) = response["result"]["nextCursor"].as_str() else {
                return crate::models::normalize_models(&rows, true);
            };
            if cursor.is_empty() || !cursors.insert(cursor.to_owned()) || cursors.len() > 20 {
                bail!("Codex 模型列表分页无效");
            }
            expected += 1;
            send(&mut stdin, json!({"id":expected,"method":"model/list","params":{"limit":100,"includeHidden":false,"cursor":cursor}})).await?;
        }
    }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("获取 Codex 模型列表超时，请重试或使用本机默认模型")));
    drop(stdin);
    stop(&mut child, process_id).await;
    result
}

/// Read effective project configuration without starting a thread or invoking a model.
/// Raw configuration (including headers) never escapes this function.
#[allow(dead_code)]
pub(crate) async fn probe_config_identity(binary: &str, root: &Path) -> Result<Value> {
    probe_config_identity_with_control(binary, root, &|| false).await
}

pub(crate) async fn probe_config_identity_with_control(
    binary: &str,
    root: &Path,
    cancelled: &impl Fn() -> bool,
) -> Result<Value> {
    if cancelled() {
        bail!("Codex 配置检查已取消");
    }
    let root = root.to_path_buf();
    let mut command = Command::new(binary);
    command
        .args(["app-server", "--listen", "stdio://"])
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    crate::prepend_binary_directory(&mut command, binary);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let _root_guard = open_root_nofollow(&root)?;
    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("无法读取 Codex 有效配置，请检查执行器安装"))?;
    let pid = child.id();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let result = tokio::select! {
        _ = wait_cancelled(cancelled) => Err(anyhow::anyhow!("Codex 配置检查已取消")),
        result = timeout(Duration::from_secs(12),async {
        send(&mut stdin,json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"self_agent","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false}
        }})).await?;
        let mut expected=1;
        let mut private_config: Option<config_identity::PrivateConfig> = None;
        let mut total=0;
        loop {
            let mut line=Vec::new();
            let bytes=(&mut reader).take((MAX_LINE+1) as u64).read_until(b'\n',&mut line).await?;
            total+=bytes;
            if bytes==0 || bytes>MAX_LINE || total>2*1024*1024 {bail!("Codex 配置响应关闭或超过安全预算");}
            let message:Value=serde_json::from_slice(&line).map_err(|_|anyhow::anyhow!("Codex 配置响应格式无效"))?;
            if message.get("method").is_some() {
                if message.get("id").is_some() {bail!("读取 Codex 配置时出现不支持的交互请求");}
                continue;
            }
            if message["id"].as_u64()!=Some(expected) {continue;}
            if message.get("error").is_some() {bail!("Codex 有效配置读取失败，不能安全恢复主会话");}
            if expected==1 {
                send(&mut stdin,json!({"method":"initialized","params":{}})).await?;
                send(&mut stdin,json!({"id":2,"method":"config/read","params":{"cwd":root,"includeLayers":false}})).await?;
                expected=2;
                continue;
            }
            if expected==2 {
                private_config=Some(config_identity::from_config(&message["result"]["config"])?);
                send(&mut stdin,json!({"id":3,"method":"account/read","params":{"refreshToken":false}})).await?;
                expected=3;
                continue;
            }
            return private_config.as_ref().unwrap().with_account(&message["result"]);
        }
    }) => result.unwrap_or_else(|_|Err(anyhow::anyhow!("读取 Codex 有效配置超时，未启动任务"))),
    };
    drop(stdin);
    stop(&mut child, pid).await;
    result
}

/// Bounded, cancellable version inspection. Never initializes auth or a model.
/// Kept available for newer controlled callers while the old wrapper stays valid.
#[allow(dead_code)]
pub(crate) async fn probe_version_with_control(
    binary: &str,
    root: &Path,
    cancelled: &impl Fn() -> bool,
) -> Result<String> {
    if cancelled() {
        bail!("Codex 版本检查已取消");
    }
    let mut command = Command::new(binary);
    command
        .arg("--version")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    crate::prepend_binary_directory(&mut command, binary);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let _root_guard = open_root_nofollow(root)?;
    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("无法检查 Codex 版本"))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout").take(4097);
    let result = tokio::select! {
        _ = wait_cancelled(cancelled) => Err(anyhow::anyhow!("Codex 版本检查已取消")),
        result = timeout(Duration::from_secs(5), async {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output).await.map_err(|_|anyhow::anyhow!("无法读取 Codex 版本"))?;
            if output.len() > 4096 { bail!("Codex 版本响应超过安全预算"); }
            if !child.wait().await.map_err(|_|anyhow::anyhow!("Codex 版本检查失败"))?.success() {
                bail!("Codex 版本检查失败");
            }
            let version = std::str::from_utf8(&output).map_err(|_|anyhow::anyhow!("Codex 版本格式无效"))?.trim();
            if version.is_empty() { bail!("Codex 版本响应为空"); }
            Ok(version.to_owned())
        }) => result.unwrap_or_else(|_|Err(anyhow::anyhow!("Codex 版本检查超时"))),
    };
    stop(&mut child, pid).await;
    result
}

async fn wait_cancelled(cancelled: &impl Fn() -> bool) {
    while !cancelled() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Never reinterpret a queued project path through a newly introduced symlink.
/// Keep the checked handle alive, but do not claim portable path-based Codex
/// cwd/sandbox parameters are an atomic capability against concurrent renames.
fn open_root_nofollow(root: &Path) -> Result<cap_std::fs::Dir> {
    use cap_fs_ext::DirExt;
    use std::path::{Component, PathBuf};
    if !root.is_absolute() {
        bail!("Codex 项目根目录必须是绝对路径");
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                if !matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                ) {
                    bail!("Codex 项目根目录不能为远程或设备路径");
                }
                anchor.push(prefix.as_os_str());
            }
            Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name),
            Component::CurDir => {}
            Component::ParentDir => bail!("Codex 项目根目录不能包含上级路径"),
        }
    }
    let mut directory = cap_std::fs::Dir::open_ambient_dir(anchor, cap_std::ambient_authority())
        .map_err(|_| anyhow::anyhow!("Codex 项目所在文件系统不可访问"))?;
    for name in names {
        directory = directory
            .open_dir_nofollow(name)
            .map_err(|_| anyhow::anyhow!("Codex 项目目录不可访问或含符号链接"))?;
    }
    Ok(directory)
}

#[cfg(all(test, unix))]
async fn execute(
    binary: &str,
    model: &str,
    root: &Path,
    objective: &str,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    execute_with_evidence(binary, model, root, objective, progress, cancelled, |_| {}).await
}

#[cfg(all(test, unix))]
pub async fn execute_with_evidence(
    binary: &str,
    model: &str,
    root: &Path,
    objective: &str,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
    evidence: impl Fn(CommandEvidence),
) -> Result<String> {
    execute_with_context(
        binary,
        model,
        root,
        ExecutionInput {
            objective,
            history: &[],
            config: &ModelContextConfig::default(),
        },
        progress,
        cancelled,
        evidence,
    )
    .await
}

#[derive(Clone, Debug)]
pub(crate) struct CodexContinuation {
    pub thread_id: Option<String>,
    pub expected_config: Option<Value>,
    pub expected_turn_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum CodexLifecycle {
    ThreadReady { thread_id: String },
    TurnStarting { thread_id: String },
    TurnStarted { thread_id: String, turn_id: String },
    TurnCompleted { thread_id: String, turn_id: String },
}

#[cfg(all(test, unix))]
pub(crate) async fn execute_with_context(
    binary: &str,
    model: &str,
    root: &Path,
    input: ExecutionInput<'_>,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
    evidence: impl Fn(CommandEvidence),
) -> Result<String> {
    execute_with_continuation(
        binary,
        model,
        root,
        input,
        progress,
        cancelled,
        evidence,
        None,
        &[],
        |_| Ok(()),
        |_| Ok(()),
    )
    .await
}

// Keep the established executor callbacks explicit at the integration boundary.
#[cfg(all(test, unix))]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_with_continuation(
    binary: &str,
    model: &str,
    root: &Path,
    input: ExecutionInput<'_>,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
    evidence: impl Fn(CommandEvidence),
    continuation: Option<CodexContinuation>,
    known_secrets: &[String],
    events: impl Fn(ExecutionEvent) -> Result<()>,
    lifecycle: impl Fn(CodexLifecycle) -> Result<()>,
) -> Result<String> {
    execute_with_control(
        binary,
        model,
        root,
        input,
        progress,
        cancelled,
        evidence,
        continuation,
        known_secrets,
        events,
        lifecycle,
        || Ok(Vec::new()),
        |_, _| Ok(()),
        |_| {},
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_with_control(
    binary: &str,
    model: &str,
    root: &Path,
    input: ExecutionInput<'_>,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
    evidence: impl Fn(CommandEvidence),
    continuation: Option<CodexContinuation>,
    known_secrets: &[String],
    events: impl Fn(ExecutionEvent) -> Result<()>,
    lifecycle: impl Fn(CodexLifecycle) -> Result<()>,
    claim_inputs: impl Fn() -> Result<Vec<TaskSupplement>>,
    ack_inputs: impl Fn(&[String], InputAcknowledgement) -> Result<()>,
    redaction_secrets: impl Fn(&[String]),
) -> Result<String> {
    if cancelled() {
        bail!("Codex 任务已取消");
    }
    if continuation
        .as_ref()
        .and_then(|value| value.thread_id.as_ref())
        .is_some_and(|id| id.is_empty() || id.len() > 256)
    {
        bail!("Codex 已保存会话标识无效，未启动任务");
    }
    if input.objective.len() > 2 * 1024 * 1024 || model.len() > 256 {
        bail!("Codex 任务输入超过大小限制");
    }
    if !(1024..=2_000_000).contains(&input.config.context_window)
        || !(10..=95).contains(&input.config.compression_threshold)
    {
        bail!("Codex 任务上下文配置无效");
    }
    let root = root.to_path_buf();
    if cancelled() {
        bail!("Codex 任务已取消");
    }
    let mut command = Command::new(binary);
    command
        .args(["app-server", "--listen", "stdio://"])
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if Path::new(binary).is_absolute() {
        if let Some(parent) = Path::new(binary).parent() {
            // GUI launches may omit the sibling Node.js used by Codex's shebang.
            let mut paths = vec![parent.to_path_buf()];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(path) = std::env::join_paths(paths) {
                command.env("PATH", path);
            }
        }
    }
    #[cfg(unix)]
    {
        // Isolate the app-server and its tool subprocesses for cancellation.
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let _root_guard = open_root_nofollow(&root)?;
    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("无法启动 Codex，请检查可执行文件路径与安装状态"))?;
    let process_id = child.id();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut ids = RunIds::default();
    let started = Instant::now();
    let result = tokio::select! {
        result = timeout(TURN_TIMEOUT, protocol(
            &mut stdin, stdout, model, &root, input,
            ProtocolCallbacks { progress: &progress, evidence: &evidence, events: &events, lifecycle: &lifecycle, cancelled: &cancelled, known_secrets, claim_inputs: &claim_inputs, ack_inputs: &ack_inputs, redaction_secrets: &redaction_secrets },
            &mut ids, continuation.as_ref(),
        )) => match result {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("Codex 任务已超过 15 分钟时限")),
        },
        _ = async {
            loop {
                if cancelled() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } => Err(anyhow::anyhow!("Codex 任务已取消")),
    };
    if result.is_err() {
        if let (Some(thread), Some(turn)) = (&ids.thread, &ids.turn) {
            let _ = timeout(
                Duration::from_millis(300),
                send(
                    &mut stdin,
                    json!({"id":4,"method":"turn/interrupt","params":{"threadId":thread,"turnId":turn}}),
                ),
            )
            .await;
            let _ = timeout(Duration::from_millis(300), child.wait()).await;
        }
    }
    drop(stdin);
    stop(&mut child, process_id).await;
    if result.is_ok() {
        progress(format!(
            "Codex 执行完成，用时 {} 秒",
            started.elapsed().as_secs()
        ));
    }
    result
}

#[derive(Default)]
struct RunIds {
    thread: Option<String>,
    turn: Option<String>,
}

async fn send(stdin: &mut ChildStdin, message: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&message)?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|_| anyhow::anyhow!("无法向 Codex 发送协议消息"))?;
    stdin
        .flush()
        .await
        .map_err(|_| anyhow::anyhow!("Codex 协议连接已关闭"))
}

struct ProtocolCallbacks<'a, P, E, V, L, C, Q, A, R> {
    progress: &'a P,
    evidence: &'a E,
    events: &'a V,
    lifecycle: &'a L,
    cancelled: &'a C,
    known_secrets: &'a [String],
    claim_inputs: &'a Q,
    ack_inputs: &'a A,
    redaction_secrets: &'a R,
}

fn thread_params(model: &str, root: &Path, context: &harness_core::ModelContextConfig) -> Value {
    let mut params = json!({
        "cwd":root,"approvalPolicy":"never","sandbox":"workspace-write",
        "config":{
            "model_context_window":context.context_window,
            "model_auto_compact_token_limit":crate::context::compression_limit(context),
            "sandbox_workspace_write":{
                "writable_roots":[],"network_access":false,
                "exclude_tmpdir_env_var":true,"exclude_slash_tmp":true
            }
        },
        "developerInstructions":"使用简体中文说明进度和结果。仅修改当前项目目录内的文件，不请求额外权限。完成目标后报告真实修改与验证结果。"
    });
    if !model.trim().is_empty() {
        params["model"] = json!(model);
    }
    params
}

async fn start_thread(
    stdin: &mut ChildStdin,
    model: &str,
    root: &Path,
    context: &harness_core::ModelContextConfig,
    resuming: Option<&str>,
    ephemeral: bool,
) -> Result<()> {
    let mut params = thread_params(model, root, context);
    let method = if let Some(thread) = resuming {
        params["threadId"] = json!(thread);
        params["excludeTurns"] = json!(true);
        "thread/resume"
    } else {
        params["ephemeral"] = json!(ephemeral);
        "thread/start"
    };
    send(stdin, json!({"id":2,"method":method,"params":params})).await
}

#[allow(clippy::too_many_arguments)]
async fn protocol(
    stdin: &mut ChildStdin,
    stdout: ChildStdout,
    model: &str,
    root: &Path,
    input: ExecutionInput<'_>,
    callbacks: ProtocolCallbacks<
        '_,
        impl Fn(String),
        impl Fn(CommandEvidence),
        impl Fn(ExecutionEvent) -> Result<()>,
        impl Fn(CodexLifecycle) -> Result<()>,
        impl Fn() -> bool,
        impl Fn() -> Result<Vec<TaskSupplement>>,
        impl Fn(&[String], InputAcknowledgement) -> Result<()>,
        impl Fn(&[String]),
    >,
    ids: &mut RunIds,
    continuation: Option<&CodexContinuation>,
) -> Result<String> {
    let ProtocolCallbacks {
        progress,
        evidence,
        events,
        lifecycle,
        cancelled,
        known_secrets,
        claim_inputs,
        ack_inputs,
        redaction_secrets,
    } = callbacks;
    progress("正在连接 Codex 执行器".into());
    send(stdin, json!({"id":1,"method":"initialize","params":{
        "clientInfo":{"name":"self_agent","title":"Self Agent","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":false}
    }})).await?;
    let mut reader = BufReader::new(stdout);
    // read_until is cancellation safe only when its partial bytes survive polls.
    let mut line = Vec::new();
    let mut poll_inputs = tokio::time::interval(Duration::from_millis(100));
    poll_inputs.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut steering = steering::Steering::default();
    let mut total = 0usize;
    let mut count = 0usize;
    let mut expected = 1u64;
    let mut resuming = continuation.and_then(|value| value.thread_id.as_deref());
    let mut answer = None;
    let mut command_ids = HashSet::new();
    let mut tool_started = HashSet::new();
    let mut tool_finished = HashSet::new();
    let mut assistant_items = std::collections::HashMap::<String, String>::new();
    let mut pending = std::collections::VecDeque::new();
    let mut credentials = known_secrets.to_vec();
    let mut private_config: Option<config_identity::PrivateConfig> = None;
    let mut pending_thread = None;
    let mut text_redactor = stream_events::TextEmitter::new(&credentials);
    let mut last_usage = None;
    let mut compaction_completed = false;
    loop {
        if cancelled() {
            bail!("Codex 任务已取消");
        }
        let message: Value = if (expected == 0 || expected == 9) && !pending.is_empty() {
            pending.pop_front().unwrap()
        } else {
            let remaining = (MAX_LINE + 1).saturating_sub(line.len());
            if remaining == 0 {
                bail!("Codex 协议输出超过安全预算");
            }
            let mut limited = (&mut reader).take(remaining as u64);
            let read = tokio::select! {
                bytes = limited.read_until(b'\n', &mut line) => Some(bytes.map_err(|_|anyhow::anyhow!("读取 Codex 协议消息失败"))?),
                _ = poll_inputs.tick() => None,
            };
            let Some(bytes) = read else {
                if cancelled() {
                    bail!("Codex 任务已取消");
                }
                if expected == 0 && !steering.is_pending() {
                    if let (Some(thread), Some(turn)) = (&ids.thread, &ids.turn) {
                        let inputs = claim_inputs()?;
                        if let Some(request) = steering.prepare(inputs, thread, turn, ack_inputs)? {
                            // Persistence already marked these inputs delivering.
                            // A failed/partial send is unknown and must not retry.
                            if cancelled() {
                                bail!("Codex 任务已取消");
                            }
                            send(stdin, request).await?;
                        }
                    }
                }
                continue;
            };
            if bytes == 0 && line.is_empty() {
                bail!("Codex 在回合完成前关闭了连接");
            }
            total += line.len();
            count += 1;
            if line.len() > MAX_LINE || total > MAX_OUTPUT || count > MAX_LINES {
                bail!("Codex 协议输出超过安全预算");
            }
            let message = serde_json::from_slice(&line)
                .map_err(|_| anyhow::anyhow!("Codex 返回了无法解析的协议消息"))?;
            line.clear();
            message
        };
        if cancelled() {
            bail!("Codex 任务已取消");
        }
        if steering.response(&message, ids.turn.as_deref(), ack_inputs)? {
            continue;
        }

        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = message.get("id") {
                let response = match method {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        json!({"id":id,"result":{"decision":"cancel"}})
                    }
                    "item/permissions/requestApproval" => {
                        json!({"id":id,"result":{"permissions":{},"scope":"turn"}})
                    }
                    _ => {
                        json!({"id":id,"error":{"code":-32601,"message":"此执行器不支持交互请求"}})
                    }
                };
                send(stdin, response).await?;
                bail!("Codex 请求了当前执行器不支持的授权或交互，任务已停止");
            }
            let params = &message["params"];
            if params["threadId"].as_str() != ids.thread.as_deref() {
                continue;
            }
            if method == "turn/started" && expected == 3 {
                let turn = params["turn"]["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .context("Codex 未返回有效回合标识")?;
                if ids.turn.as_deref().is_some_and(|known| known != turn) {
                    bail!("Codex 回合标识不一致");
                }
                ids.turn = Some(turn.into());
            }
            if expected == 3 {
                pending.push_back(message);
                continue;
            }
            let event_turn = if matches!(method, "turn/completed" | "turn/started") {
                params["turn"]["id"].as_str()
            } else {
                params["turnId"].as_str()
            };
            if ids.turn.is_none() || event_turn != ids.turn.as_deref() {
                continue;
            }
            match method {
                "item/agentMessage/delta" => {
                    let item_id = params["itemId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .context("Codex 回复增量缺少项目标识")?;
                    let delta = params["delta"].as_str().context("Codex 回复增量格式无效")?;
                    let emitted = assistant_items.entry(item_id.to_owned()).or_default();
                    if emitted.len() + delta.len() > MAX_RESULT {
                        bail!("Codex 单条回复超过大小限制");
                    }
                    emitted.push_str(delta);
                    text_redactor.push(delta, events)?;
                }
                "item/started" => {
                    let item = &params["item"];
                    if item["type"] == "contextCompaction" {
                        compaction_completed = false;
                        events(ExecutionEvent::ContextCompaction {
                            state: "started".into(),
                        })?;
                    }
                    if let Some((id, name)) = stream_events::tool_identity(item, &credentials) {
                        if tool_started.insert(id.clone()) {
                            progress("Codex 正在执行项目命令或工具".into());
                            events(ExecutionEvent::ToolStarted { call_id: id, name })?;
                        }
                    }
                }
                "item/completed" => {
                    let item = &params["item"];
                    emit_answer_item(item, &mut assistant_items, &mut text_redactor, events)?;
                    capture_answer(item, &mut answer)?;
                    capture_evidence(item, &mut command_ids, &credentials, evidence);
                    if let Some((id, name)) = stream_events::tool_identity(item, &credentials) {
                        if tool_finished.insert(id.clone()) {
                            events(stream_events::tool_result(item, id, name, &credentials))?;
                        }
                    }
                    if item["type"] == "contextCompaction" && !compaction_completed {
                        compaction_completed = true;
                        events(ExecutionEvent::ContextCompaction {
                            state: "completed".into(),
                        })?;
                    }
                }
                "thread/compacted" => {
                    if !compaction_completed {
                        compaction_completed = true;
                        events(ExecutionEvent::ContextCompaction {
                            state: "completed".into(),
                        })?;
                    }
                }
                "thread/tokenUsage/updated" => {
                    // The schema's `last` is per model request, `total` is thread cumulative.
                    // Publish changes in the cumulative pair once; do not sum repeated notifications.
                    let usage = &params["tokenUsage"]["total"];
                    let value = (
                        usage["inputTokens"].as_u64(),
                        usage["outputTokens"].as_u64(),
                    );
                    if value != (None, None) && last_usage.as_ref() != Some(&value) {
                        last_usage = Some(value);
                        events(ExecutionEvent::Usage {
                            scope: "thread_total".into(),
                            input_tokens: value.0,
                            output_tokens: value.1,
                        })?;
                    }
                }
                "turn/completed" => {
                    match params["turn"]["status"].as_str() {
                        Some("completed") => {}
                        Some("interrupted") => bail!("Codex 回合已中断"),
                        Some("failed") => {
                            bail!("Codex 回合执行失败，请检查本机登录、模型和执行器配置")
                        }
                        _ => bail!("Codex 返回了未知回合状态"),
                    }
                    if let Some(items) = params["turn"]["items"].as_array() {
                        for item in items {
                            emit_answer_item(
                                item,
                                &mut assistant_items,
                                &mut text_redactor,
                                events,
                            )?;
                            capture_answer(item, &mut answer)?;
                        }
                    }
                    let answer = answer
                        .filter(|text: &String| !text.trim().is_empty())
                        .context("Codex 回合结束但未返回最终结果")?;
                    text_redactor.finish(events)?;
                    if cancelled() {
                        bail!("Codex 任务已取消");
                    }
                    if continuation.is_some() {
                        lifecycle(CodexLifecycle::TurnCompleted {
                            thread_id: ids.thread.clone().unwrap(),
                            turn_id: ids.turn.clone().unwrap(),
                        })?;
                    }
                    return Ok(stream_events::redact(&answer, &credentials));
                }
                _ => {}
            }
            continue;
        }
        if expected == 0 || message["id"].as_u64() != Some(expected) {
            continue;
        }
        if message.get("error").is_some() {
            if expected == 2 && resuming.is_some() {
                // Only an explicit failed resume, before any turn request, permits fallback.
                progress(
                    "Codex 原会话恢复失败；已保留应用历史，将新建会话续接（不重放旧任务）".into(),
                );
                resuming = None;
                let mut params = thread_params(model, root, input.config);
                params["ephemeral"] = json!(false);
                send(
                    stdin,
                    json!({"id":5,"method":"thread/start","params":params}),
                )
                .await?;
                expected = 5;
                continue;
            }
            bail!("Codex 协议请求失败，请检查本机版本、登录和模型配置");
        }
        let result = &message["result"];
        match expected {
            1 => {
                if !result.is_object() {
                    bail!("Codex 初始化响应无效");
                }
                send(stdin, json!({"method":"initialized","params":{}})).await?;
                send(stdin, json!({"id":6,"method":"config/read","params":{"cwd":root,"includeLayers":false}})).await?;
                expected = 6;
            }
            6 => {
                let config = config_identity::from_config(&result["config"])?;
                credentials.extend(config.secrets.iter().cloned());
                // Failed account/config identity checks still produce a final
                // diff report; give its private filter these credentials now.
                redaction_secrets(&credentials);
                private_config = Some(config);
                send(
                    stdin,
                    json!({"id":7,"method":"account/read","params":{"refreshToken":false}}),
                )
                .await?;
                expected = 7;
            }
            7 => {
                credentials.extend(config_identity::account_secrets(result));
                redaction_secrets(&credentials);
                let actual = private_config.as_ref().unwrap().with_account(result)?;
                if continuation
                    .and_then(|value| value.expected_config.as_ref())
                    .is_some_and(|expected| expected != &actual)
                {
                    bail!("Codex 有效配置或账户状态已变化，未恢复会话或启动新回合；请重新发送目标");
                }
                text_redactor = stream_events::TextEmitter::new(&credentials);
                if continuation.is_some() && actual["reuse_safe"] != true {
                    progress("Codex 当前登录身份无法可靠核对；将新建会话并保留完整应用历史，不复用旧线程".into());
                    resuming = None;
                }
                start_thread(
                    stdin,
                    model,
                    root,
                    input.config,
                    resuming,
                    continuation.is_none(),
                )
                .await?;
                expected = 2;
            }
            8 => {
                let latest = result["data"]
                    .as_array()
                    .filter(|turns| turns.len() == 1)
                    .and_then(|turns| turns.first())
                    .context("Codex 无法确认上轮完成边界，未启动回合")?;
                if latest["status"] != "completed"
                    || latest["id"].as_str()
                        != continuation.and_then(|value| value.expected_turn_id.as_deref())
                {
                    bail!("Codex 外部会话已变化或上轮状态未知，未启动回合或重放任务");
                }
                // Continue through the already validated thread result only after
                // the persisted remote turn boundary matches our own last success.
                let result = pending_thread.take().unwrap();
                pending.push_front(json!({"id":9,"result":result}));
                expected = 9;
            }
            2 | 5 | 9 => {
                let thread = result["thread"]["id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .context("Codex 未返回有效会话标识")?;
                if resuming.is_some_and(|expected| expected != thread) {
                    bail!("Codex 恢复会话标识不一致，未开始新回合");
                }
                verify_sandbox(result, root)?;
                if result["thread"]["status"]["type"] != "idle"
                    || result["thread"]["ephemeral"] != continuation.is_none()
                {
                    bail!("Codex 会话状态不适合续接，未自动重放或注入在途回合");
                }
                private_config
                    .as_ref()
                    .unwrap()
                    .verify_thread_identity(result, model)?;
                if resuming.is_some() && expected != 9 {
                    if continuation
                        .and_then(|value| value.expected_turn_id.as_deref())
                        .is_none()
                    {
                        bail!("Codex 缺少可信的上轮完成边界，未启动回合");
                    }
                    pending_thread = Some(result.clone());
                    send(stdin,json!({"id":8,"method":"thread/turns/list","params":{
                        "threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"
                    }})).await?;
                    expected = 8;
                    continue;
                }
                ids.thread = Some(thread.into());
                if continuation.is_some() {
                    lifecycle(CodexLifecycle::ThreadReady {
                        thread_id: thread.into(),
                    })?;
                }
                progress(
                    if resuming.is_some() {
                        "Codex 已恢复原主会话，权限限定为项目目录写入"
                    } else if continuation.is_some() {
                        "Codex 已创建持久主会话，权限限定为项目目录写入"
                    } else {
                        "Codex 已创建独立执行会话，权限限定为项目目录写入"
                    }
                    .into(),
                );
                let objective = if resuming.is_some() {
                    input.objective.to_owned()
                } else {
                    crate::context::history_prompt(input.history, input.objective)?
                };
                if objective.len() > 2 * 1024 * 1024 {
                    bail!("Codex 新会话历史超过 2 MiB 输入预算，未启动回合");
                }
                events(ExecutionEvent::ContextEstimate {
                    estimated_input_tokens: crate::context::estimate_text(&objective) as u64,
                    context_window: input.config.context_window,
                    compression_threshold: input.config.compression_threshold,
                    source: if resuming.is_some() {
                        "codex_current_input_only"
                    } else {
                        "codex_local_bootstrap"
                    }
                    .into(),
                    phase: "before_turn".into(),
                })?;
                if cancelled() {
                    bail!("Codex 任务已取消");
                }
                if continuation.is_some() {
                    lifecycle(CodexLifecycle::TurnStarting {
                        thread_id: thread.into(),
                    })?;
                }
                send(stdin,json!({"id":3,"method":"turn/start","params":{
                    "threadId":thread,"cwd":root,"approvalPolicy":"never",
                    "sandboxPolicy":{"type":"workspaceWrite","writableRoots":[root],"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true},
                    "input":[{"type":"text","text":objective}]
                }})).await?;
                expected = 3;
            }
            3 => {
                let turn = result["turn"]["id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .context("Codex 未返回有效回合标识")?;
                if ids.turn.as_deref().is_some_and(|known| known != turn) {
                    bail!("Codex 回合标识不一致");
                }
                ids.turn = Some(turn.into());
                if continuation.is_some() {
                    lifecycle(CodexLifecycle::TurnStarted {
                        thread_id: ids.thread.clone().unwrap(),
                        turn_id: turn.into(),
                    })?;
                }
                progress("Codex 正在处理任务".into());
                expected = 0;
            }
            _ => {}
        }
    }
}

fn emit_answer_item(
    item: &Value,
    assistant_items: &mut std::collections::HashMap<String, String>,
    text_redactor: &mut stream_events::TextEmitter,
    events: &impl Fn(ExecutionEvent) -> Result<()>,
) -> Result<()> {
    if item["type"] != "agentMessage" {
        return Ok(());
    }
    let id = item["id"]
        .as_str()
        .unwrap_or(if item["phase"] == "commentary" {
            "legacy-commentary"
        } else {
            "legacy-final"
        });
    if let Some(text) = item["text"].as_str() {
        let emitted = assistant_items.entry(id.into()).or_default();
        if text.len() > MAX_RESULT || !text.starts_with(emitted.as_str()) {
            bail!("Codex 回复增量与最终项目不一致");
        }
        text_redactor.push(&text[emitted.len()..], events)?;
        *emitted = text.into();
    }
    Ok(())
}

fn verify_sandbox(result: &Value, root: &Path) -> Result<()> {
    let sandbox = &result["sandbox"];
    let roots = sandbox["writableRoots"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Codex 未确认项目目录权限限制"))?;
    if result["approvalPolicy"] != "never"
        || result["cwd"].as_str() != root.to_str()
        || sandbox["type"] != "workspaceWrite"
        || sandbox["networkAccess"] != false
        || sandbox["excludeTmpdirEnvVar"] != true
        || sandbox["excludeSlashTmp"] != true
        || roots.iter().any(|entry| entry.as_str() != root.to_str())
    {
        bail!("Codex 返回的权限超出项目目录限制，任务已停止");
    }
    Ok(())
}

fn capture_answer(item: &Value, answer: &mut Option<String>) -> Result<()> {
    if item["type"] == "agentMessage"
        && (item["phase"].is_null() || item["phase"] == "final_answer")
    {
        let text = item["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Codex 最终消息格式无效"))?;
        if text.len() > MAX_RESULT {
            bail!("Codex 最终结果超过大小限制");
        }
        *answer = Some(text.to_owned());
    }
    Ok(())
}

fn capture_evidence(
    item: &Value,
    command_ids: &mut HashSet<String>,
    credentials: &[String],
    evidence: &impl Fn(CommandEvidence),
) {
    if item["type"] != "commandExecution"
        || !matches!(item["status"].as_str(), Some("completed" | "failed"))
    {
        return;
    }
    let (Some(id), Some(command), Some(exit_code)) = (
        item["id"].as_str().filter(|id| !id.is_empty()),
        item["command"].as_str(),
        item["exitCode"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok()),
    ) else {
        return;
    };
    if !command_ids.insert(id.to_owned()) {
        return;
    }
    let normalized = normalize_command(command);
    let redacted_command = stream_events::redact(&normalized, credentials);
    // Never turn an oversized command into a shorter, falsely matching command.
    if normalized.len() > 16_000 || redacted_command.len() > 16 * 1024 {
        return;
    }
    let output = item["aggregatedOutput"].as_str().unwrap_or_default();
    let output = if normalized != command.trim() {
        format!("原始命令：{command}\n{output}")
    } else {
        output.to_owned()
    };
    evidence(CommandEvidence {
        command: redacted_command,
        exit_code,
        output: bounded_evidence(&output, credentials),
    });
}

fn normalize_command(command: &str) -> String {
    let original = command.trim();
    // shlex splits words without expanding shell syntax. Reject syntax that an
    // outer shell could expand or treat as operators before unwrapping a shell.
    if !literal_shell_words(original) {
        return original.to_owned();
    }
    let Some(words) = shlex::split(original) else {
        return original.to_owned();
    };
    if words.len() == 3
        && matches!(
            words[0].as_str(),
            "sh" | "bash"
                | "zsh"
                | "/bin/sh"
                | "/bin/bash"
                | "/bin/zsh"
                | "/usr/bin/sh"
                | "/usr/bin/bash"
                | "/usr/bin/zsh"
        )
        && matches!(words[1].as_str(), "-c" | "-lc")
    {
        words[2].trim().to_owned()
    } else {
        original.to_owned()
    }
}

fn literal_shell_words(command: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('\'') {
            if character == '\'' {
                quote = None;
            }
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if quote == Some('"') {
            match character {
                '"' => quote = None,
                '$' | '`' => return false,
                _ => {}
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '$' | '`' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | ']' | '{'
            | '}' | '\n' | '#' | '~' => return false,
            _ => {}
        }
    }
    !escaped && quote.is_none()
}

fn bounded_evidence(text: &str, credentials: &[String]) -> String {
    // Redact complete input before the byte limit can cut a credential in half.
    let mut text = stream_events::redact(text, credentials);
    let mut end = text.len().min(16 * 1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn redact_evidence(text: &str) -> String {
    let mut text = text.to_owned();
    for name in ["HARNESS_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(secret) = std::env::var(name) {
            if !secret.is_empty() {
                text = text.replace(&secret, "[凭证已隐藏]");
            }
        }
    }
    text
}

async fn stop(child: &mut Child, process_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = process_id {
        // Kill the isolated process group, including a running shell/tool.
        let _ = timeout(
            Duration::from_secs(1),
            Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;
    }
    #[cfg(windows)]
    if let Some(pid) = process_id {
        let _ = timeout(
            Duration::from_secs(2),
            Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };

    struct Project(std::path::PathBuf);

    impl Project {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("codex-fixture-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/codex_fixture.py")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn fixture_previous_turn(root: &Path, thread: Option<&str>) -> Option<String> {
        thread.map(|thread| {
            let state: Value = std::fs::read(root.join("fixture-thread-state.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(Value::Null);
            format!(
                "fixture-turn-{}",
                state[thread]["inputs"].as_array().map_or(1, Vec::len)
            )
        })
    }

    async fn persistent_run(
        mode: &str,
        root: &Path,
        thread_id: Option<String>,
        history: &[Value],
        objective: &str,
        lifecycle: &Mutex<Vec<CodexLifecycle>>,
        events: &Mutex<Vec<Value>>,
    ) -> Result<String> {
        execute_with_continuation(
            &fixture(),
            mode,
            root,
            ExecutionInput {
                objective,
                history,
                config: &ModelContextConfig::default(),
            },
            |_| {},
            || false,
            |_| {},
            Some(CodexContinuation {
                thread_id: thread_id.clone(),
                expected_config: None,
                expected_turn_id: fixture_previous_turn(root, thread_id.as_deref()),
            }),
            &[],
            |event| {
                events.lock().unwrap().push(serde_json::to_value(event)?);
                Ok(())
            },
            |event| {
                lifecycle.lock().unwrap().push(event);
                Ok(())
            },
        )
        .await
    }

    fn ready_thread(lifecycle: &Mutex<Vec<CodexLifecycle>>) -> String {
        lifecycle
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                CodexLifecycle::ThreadReady { thread_id } => Some(thread_id.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn managed_account() -> Value {
        json!({"account":{"type":"chatgpt","email":"private-user@example.test","planType":"team"},
            "requiresOpenaiAuth":true,"workspaceRouting":{"chatgptAccountId":"private-workspace-id",
                "backendOrigin":"https://chatgpt.com","accountRoutingOverride":"NO_CONSTRAINT"}})
    }

    fn write_managed_account(root: &Path, account: &Value) {
        std::fs::write(
            root.join("fixture-managed-account.json"),
            serde_json::to_vec(account).unwrap(),
        )
        .unwrap();
    }

    fn managed_project() -> Project {
        let project = Project::new();
        std::fs::write(
            project.0.join("fixture-routing-version"),
            "codex-cli 0.166.0-alpha.1",
        )
        .unwrap();
        write_managed_account(&project.0, &managed_account());
        project
    }

    struct ManagedResult {
        output: String,
        thread: String,
        public: String,
    }

    async fn managed_run(
        root: &Path,
        expected: Value,
        saved: Option<String>,
        mode: &str,
        objective: &str,
    ) -> Result<ManagedResult> {
        let public = Mutex::new(Vec::new());
        let lifecycle = Mutex::new(Vec::new());
        let output = execute_with_continuation(
            &fixture(),
            mode,
            root,
            ExecutionInput {
                objective,
                history: &[json!({"role":"assistant","content":"MANAGED_BOOTSTRAP"})],
                config: &ModelContextConfig::default(),
            },
            |progress| public.lock().unwrap().push(json!(progress)),
            || false,
            |evidence| {
                public
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(evidence).unwrap())
            },
            Some(CodexContinuation {
                expected_turn_id: fixture_previous_turn(root, saved.as_deref()),
                thread_id: saved,
                expected_config: Some(expected),
            }),
            &[],
            |event| {
                public.lock().unwrap().push(serde_json::to_value(event)?);
                Ok(())
            },
            |event| {
                lifecycle.lock().unwrap().push(event);
                Ok(())
            },
        )
        .await?;
        let public = serde_json::to_string(&*public.lock().unwrap())?;
        Ok(ManagedResult {
            output,
            thread: ready_thread(&lifecycle),
            public,
        })
    }

    #[tokio::test]
    async fn complete_managed_workspace_metadata_still_rebuilds_three_fresh_processes_privately() {
        let project = managed_project();
        let identity = probe_config_identity(&fixture(), &project.0).await.unwrap();
        assert_eq!(identity["reuse_safe"], false);
        let mut saved = None;
        for round in 1..=3 {
            let current = probe_config_identity(&fixture(), &project.0).await.unwrap();
            assert_eq!(current, identity);
            let result = managed_run(
                &project.0,
                current,
                saved.clone(),
                "managed-metadata",
                &format!("ROUND_{round}"),
            )
            .await
            .unwrap();
            if let Some(previous) = &saved {
                assert_ne!(&result.thread, previous);
            }
            saved = Some(result.thread);
            assert_eq!(result.output, "[凭证已隐藏] | [凭证已隐藏] | [凭证已隐藏]");
            for needle in [
                "private-user@example.test",
                "private-workspace-id",
                "https://chatgpt.com",
                "workspaceRouting",
            ] {
                assert!(!identity.to_string().contains(needle));
                assert!(!result.public.contains(needle));
                assert!(!result.output.contains(needle));
            }
        }
        let state: Value = serde_json::from_slice(
            &std::fs::read(project.0.join("fixture-thread-state.json")).unwrap(),
        )
        .unwrap();
        let entries = state.as_object().unwrap();
        assert_eq!(entries.len(), 3);
        for entry in entries.values() {
            let inputs = entry["inputs"].as_array().unwrap();
            assert_eq!(inputs.len(), 1);
            assert!(inputs[0].as_str().unwrap().contains("MANAGED_BOOTSTRAP"));
        }
        let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
        assert!(!trace.contains("thread/resume"));
        assert!(!trace.contains("thread/turns/list"));
        assert!(!trace.contains("workspaceRouting") && !trace.contains("historyMode"));
    }

    #[tokio::test]
    async fn changed_managed_identity_fails_before_any_thread_operation_and_refresh_bootstraps() {
        let project = managed_project();
        let expected = probe_config_identity(&fixture(), &project.0).await.unwrap();
        let first = managed_run(&project.0, expected.clone(), None, "success", "FIRST")
            .await
            .unwrap();
        let trace_path = project.0.join("fixture-rpc-trace.jsonl");
        let trace = std::fs::read(&trace_path).unwrap();
        for (pointer, value) in [
            ("/account/type", json!("apiKey")),
            ("/account/email", json!("other-private-user@example.test")),
            (
                "/workspaceRouting/chatgptAccountId",
                json!("other-private-workspace"),
            ),
            (
                "/workspaceRouting/backendOrigin",
                json!("https://other.example.test"),
            ),
            ("/workspaceRouting/accountRoutingOverride", json!("us_cr")),
        ] {
            let mut changed = managed_account();
            *changed.pointer_mut(pointer).unwrap() = value;
            write_managed_account(&project.0, &changed);
            let fresh = probe_config_identity(&fixture(), &project.0).await.unwrap();
            assert_ne!(fresh["fingerprint"], expected["fingerprint"]);
            for saved in [None, Some(first.thread.clone())] {
                let error = managed_run(
                    &project.0,
                    expected.clone(),
                    saved,
                    "success",
                    "MUST_NOT_START",
                )
                .await
                .err()
                .unwrap()
                .to_string();
                assert!(error.contains("有效配置或账户状态已变化"));
                assert!(!error.contains("private") && !error.contains("https://"));
                assert_eq!(std::fs::read(&trace_path).unwrap(), trace);
            }
        }
        let fresh = probe_config_identity(&fixture(), &project.0).await.unwrap();
        // The store chooses None after the identity hash changes; no hidden replay.
        let second = managed_run(&project.0, fresh, None, "success", "SECOND")
            .await
            .unwrap();
        assert_ne!(second.thread, first.thread);
        let state: Value = serde_json::from_slice(
            &std::fs::read(project.0.join("fixture-thread-state.json")).unwrap(),
        )
        .unwrap();
        assert!(state[second.thread]["inputs"][0]
            .as_str()
            .unwrap()
            .contains("MANAGED_BOOTSTRAP"));
    }

    #[tokio::test]
    async fn unsupported_version_and_missing_routing_preserve_fresh_managed_history() {
        for case in [
            "unsupported",
            "missing",
            "null",
            "missing-field",
            "bad-origin",
            "bad-policy",
            "missing-principal",
        ] {
            let project = managed_project();
            let mut account = managed_account();
            match case {
                "unsupported" => std::fs::write(
                    project.0.join("fixture-routing-version"),
                    "codex-cli 0.166.0-alpha.2",
                )
                .unwrap(),
                "missing" => {
                    account.as_object_mut().unwrap().remove("workspaceRouting");
                }
                "null" => account["workspaceRouting"] = Value::Null,
                "missing-field" => {
                    account["workspaceRouting"]
                        .as_object_mut()
                        .unwrap()
                        .remove("chatgptAccountId");
                }
                "bad-origin" => {
                    account["workspaceRouting"]["backendOrigin"] = json!("http://chatgpt.com")
                }
                "bad-policy" => {
                    account["workspaceRouting"]["accountRoutingOverride"] = json!("unknown")
                }
                "missing-principal" => account["account"]["email"] = Value::Null,
                _ => unreachable!(),
            }
            write_managed_account(&project.0, &account);
            let identity = probe_config_identity(&fixture(), &project.0).await.unwrap();
            assert_eq!(identity["reuse_safe"], false, "{case}");
            let first = managed_run(&project.0, identity.clone(), None, "success", "FIRST")
                .await
                .unwrap();
            let second = managed_run(
                &project.0,
                identity,
                Some(first.thread.clone()),
                "success",
                "SECOND",
            )
            .await
            .unwrap();
            assert_ne!(first.thread, second.thread, "{case}");
            let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
            assert!(!trace.contains("thread/resume"));
            assert!(second.public.contains("不复用旧线程"));
            let init =
                std::fs::read_to_string(project.0.join("fixture-initialize-trace.jsonl")).unwrap();
            assert!(init
                .lines()
                .all(
                    |line| serde_json::from_str::<Value>(line).unwrap()["capabilities"]
                        ["experimentalApi"]
                        == false
                ));
        }
    }

    #[tokio::test]
    async fn cancellable_preflight_reaps_stalled_version_and_config_without_a_thread() {
        for marker in ["fixture-version-stall", "fixture-config-stall"] {
            let project = Project::new();
            std::fs::write(project.0.join(marker), "yes").unwrap();
            let started = Instant::now();
            let cancelled = || started.elapsed() > Duration::from_millis(500);
            let error = if marker == "fixture-version-stall" {
                probe_version_with_control(&fixture(), &project.0, &cancelled)
                    .await
                    .map(|_| Value::Null)
            } else {
                probe_config_identity_with_control(&fixture(), &project.0, &cancelled).await
            }
            .unwrap_err()
            .to_string();
            assert!(error.contains("已取消"));
            assert!(started.elapsed() < Duration::from_secs(3));
            let pid = std::fs::read_to_string(project.0.join("fixture-probe-pid")).unwrap();
            assert!(!Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .await
                .unwrap()
                .success());
            assert!(!project.0.join("fixture-rpc-trace.jsonl").exists());
        }
        let project = Project::new();
        assert!(
            probe_config_identity_with_control(&fixture(), &project.0, &|| true)
                .await
                .unwrap_err()
                .to_string()
                .contains("已取消")
        );
        assert!(std::fs::read_dir(&project.0).unwrap().next().is_none());
        std::fs::write(project.0.join("fixture-version-oversize"), "yes").unwrap();
        assert!(
            probe_version_with_control(&fixture(), &project.0, &|| false)
                .await
                .unwrap_err()
                .to_string()
                .contains("安全预算")
        );
    }

    #[tokio::test]
    async fn config_probe_hashes_full_config_without_returning_credentials_or_starting_thread() {
        let project = Project::new();
        let identity = probe_config_identity(&fixture(), &project.0).await.unwrap();
        assert_eq!(identity["model"], "fixture-default");
        assert_eq!(identity["fingerprint"].as_str().unwrap().len(), 64);
        assert!(!identity
            .to_string()
            .contains("private-fixture-config-value"));
        assert!(!identity.to_string().contains("http_headers"));
        assert!(!project.0.join("fixture-started").exists());
        assert!(!project.0.join("fixture-thread-state.json").exists());
    }

    #[tokio::test]
    async fn same_process_config_check_prevents_changed_or_failed_probe_before_thread_operations() {
        let project = Project::new();
        let expected = probe_config_identity(&fixture(), &project.0).await.unwrap();
        let run = |thread_id: Option<String>| {
            let expected = expected.clone();
            let root = project.0.clone();
            async move {
                execute_with_continuation(
                    &fixture(),
                    "success",
                    &root,
                    ExecutionInput {
                        objective: "GOAL",
                        history: &[],
                        config: &ModelContextConfig::default(),
                    },
                    |_| {},
                    || false,
                    |_| {},
                    Some(CodexContinuation {
                        expected_turn_id: fixture_previous_turn(&root, thread_id.as_deref()),
                        thread_id,
                        expected_config: Some(expected),
                    }),
                    &[],
                    |_| Ok(()),
                    |_| Ok(()),
                )
                .await
            }
        };
        run(None).await.unwrap();
        let state: Value = serde_json::from_slice(
            &std::fs::read(project.0.join("fixture-thread-state.json")).unwrap(),
        )
        .unwrap();
        let thread = state.as_object().unwrap().keys().next().unwrap().clone();
        run(Some(thread.clone())).await.unwrap();
        let trace_path = project.0.join("fixture-rpc-trace.jsonl");
        let accepted_trace = std::fs::read(&trace_path).unwrap();
        let accepted_state = std::fs::read(project.0.join("fixture-thread-state.json")).unwrap();
        for marker in [
            "fixture-config-changed",
            "fixture-config-error",
            "fixture-account-error",
        ] {
            std::fs::write(project.0.join(marker), "yes").unwrap();
            for saved in [None, Some(thread.clone())] {
                let error = run(saved).await.unwrap_err().to_string();
                assert!(!error.contains("private-") && !error.contains("secret-"));
                if marker == "fixture-config-changed" {
                    assert!(error.contains("有效配置或账户状态已变化"));
                }
                assert_eq!(std::fs::read(&trace_path).unwrap(), accepted_trace);
                assert_eq!(
                    std::fs::read(project.0.join("fixture-thread-state.json")).unwrap(),
                    accepted_state
                );
            }
            std::fs::remove_file(project.0.join(marker)).unwrap();
        }
    }

    #[tokio::test]
    async fn persistent_main_survives_three_processes_without_duplicate_bootstrap_history() {
        let project = Project::new();
        let history = vec![json!({"role":"assistant","content":"FIRST_HISTORY"})];
        let mut thread = None;
        for round in 1..=3 {
            let lifecycle = Mutex::new(Vec::new());
            let events = Mutex::new(Vec::new());
            let result = persistent_run(
                "success",
                &project.0,
                thread.clone(),
                &history,
                &format!("ROUND_{round}"),
                &lifecycle,
                &events,
            )
            .await
            .unwrap();
            assert_eq!(result, "测试执行结果");
            let id = ready_thread(&lifecycle);
            if let Some(previous) = &thread {
                assert_eq!(&id, previous);
            } else {
                thread = Some(id.clone());
            }
            let lifecycle = lifecycle.lock().unwrap();
            assert_eq!(lifecycle.len(), 4);
            assert!(
                matches!(&lifecycle[1],CodexLifecycle::TurnStarting {thread_id} if thread_id == &id)
            );
            assert!(
                matches!(&lifecycle[2],CodexLifecycle::TurnStarted {thread_id,turn_id} if thread_id == &id && turn_id == &format!("fixture-turn-{round}"))
            );
            assert!(
                matches!(&lifecycle[3],CodexLifecycle::TurnCompleted {thread_id,turn_id} if thread_id == &id && turn_id == &format!("fixture-turn-{round}"))
            );
            let events = events.lock().unwrap();
            let estimate = events
                .iter()
                .find(|event| event["kind"] == "context_estimate")
                .unwrap();
            assert_eq!(
                estimate["source"],
                if round == 1 {
                    "codex_local_bootstrap"
                } else {
                    "codex_current_input_only"
                }
            );
            if round > 1 {
                assert_eq!(
                    estimate["estimated_input_tokens"],
                    crate::context::estimate_text(&format!("ROUND_{round}"))
                );
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["kind"] == "usage")
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["kind"] == "tool_finished")
                    .count(),
                1
            );
            let text = events
                .iter()
                .filter_map(|event| event["text"].as_str())
                .collect::<String>();
            assert!(text.starts_with("测试执行结果"));
            assert!(!text.contains("wrong-thread"));
        }
        let state: Value = serde_json::from_slice(
            &std::fs::read(project.0.join("fixture-thread-state.json")).unwrap(),
        )
        .unwrap();
        let inputs = state[thread.unwrap()]["inputs"].as_array().unwrap().clone();
        assert!(inputs[0].as_str().unwrap().contains("FIRST_HISTORY"));
        assert_eq!(inputs[1], "ROUND_2");
        assert_eq!(inputs[2], "ROUND_3");
    }

    #[tokio::test]
    async fn missing_resume_falls_back_once_before_turn_and_bootstraps_once() {
        let project = Project::new();
        let lifecycle = Mutex::new(Vec::new());
        let events = Mutex::new(Vec::new());
        let history = vec![json!({"role":"assistant","content":"ORIGINAL_HISTORY"})];
        persistent_run(
            "success",
            &project.0,
            Some("missing-thread".into()),
            &history,
            "NEW_GOAL",
            &lifecycle,
            &events,
        )
        .await
        .unwrap();
        let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
        let methods: Vec<_> = trace
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["method"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(methods, vec!["thread/resume", "thread/start", "turn/start"]);
        let state: Value = serde_json::from_slice(
            &std::fs::read(project.0.join("fixture-thread-state.json")).unwrap(),
        )
        .unwrap();
        let input = state[ready_thread(&lifecycle)]["inputs"][0]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(input.matches("ORIGINAL_HISTORY").count(), 1);
        assert_eq!(input.matches("NEW_GOAL").count(), 1);
    }

    #[tokio::test]
    async fn unsafe_active_and_mismatched_resumes_fail_without_starting_new_turn() {
        for mode in ["resume-unsafe", "resume-active", "resume-wrong-id"] {
            let project = Project::new();
            let lifecycle = Mutex::new(Vec::new());
            let events = Mutex::new(Vec::new());
            persistent_run(
                "success",
                &project.0,
                None,
                &[],
                "FIRST",
                &lifecycle,
                &events,
            )
            .await
            .unwrap();
            let id = ready_thread(&lifecycle);
            let lifecycle = Mutex::new(Vec::new());
            assert!(persistent_run(
                mode,
                &project.0,
                Some(id),
                &[],
                "SECOND",
                &lifecycle,
                &events
            )
            .await
            .is_err());
            assert!(lifecycle.lock().unwrap().is_empty());
            let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
            assert_eq!(
                trace
                    .lines()
                    .filter(
                        |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                            == "turn/start"
                    )
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn unknown_turn_start_is_never_replayed_or_marked_completed() {
        let project = Project::new();
        let lifecycle = Mutex::new(Vec::new());
        let events = Mutex::new(Vec::new());
        assert!(persistent_run(
            "unknown-start",
            &project.0,
            None,
            &[],
            "MUTATION",
            &lifecycle,
            &events
        )
        .await
        .is_err());
        let lifecycle = lifecycle.lock().unwrap();
        assert_eq!(lifecycle.len(), 2);
        assert!(matches!(&lifecycle[1], CodexLifecycle::TurnStarting { .. }));
        let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
        assert_eq!(
            trace
                .lines()
                .filter(
                    |line| serde_json::from_str::<Value>(line).unwrap()["method"] == "turn/start"
                )
                .count(),
            1
        );
        assert!(project.0.join("fixture-unknown-write").exists());
    }

    #[tokio::test]
    async fn lifecycle_storage_failure_prevents_sending_turn() {
        let project = Project::new();
        let result = execute_with_continuation(
            &fixture(),
            "success",
            &project.0,
            ExecutionInput {
                objective: "MUTATION",
                history: &[],
                config: &ModelContextConfig::default(),
            },
            |_| {},
            || false,
            |_| {},
            Some(CodexContinuation {
                thread_id: None,
                expected_config: None,
                expected_turn_id: None,
            }),
            &[],
            |_| Ok(()),
            |event| {
                if matches!(event, CodexLifecycle::TurnStarting { .. }) {
                    bail!("状态保存失败");
                }
                Ok(())
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("状态保存失败"));
        assert!(!project.0.join("fixture-started").exists());
        assert!(!project.0.join("fixture-result.txt").exists());
    }

    #[tokio::test]
    async fn child_runs_remain_ephemeral_and_do_not_emit_persistent_lifecycle() {
        let project = Project::new();
        execute_with_continuation(
            &fixture(),
            "success",
            &project.0,
            ExecutionInput {
                objective: "本地测试目标",
                history: &[],
                config: &ModelContextConfig::default(),
            },
            |_| {},
            || false,
            |_| {},
            None,
            &[],
            |_| Ok(()),
            |_| panic!("child association"),
        )
        .await
        .unwrap();
        assert!(!project.0.join("fixture-thread-state.json").exists());
    }

    #[tokio::test]
    async fn buffered_events_before_start_reply_are_correlated_and_mismatches_fail() {
        let project = Project::new();
        let lifecycle = Mutex::new(Vec::new());
        let events = Mutex::new(Vec::new());
        persistent_run(
            "pre-ack",
            &project.0,
            None,
            &[],
            "GOAL",
            &lifecycle,
            &events,
        )
        .await
        .unwrap();
        let text = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| event["text"].as_str())
            .collect::<String>();
        assert!(text.starts_with("测试执行结果"));
        assert_eq!(text.matches("测试执行结果").count(), 1);
        let project = Project::new();
        let lifecycle = Mutex::new(Vec::new());
        let events = Mutex::new(Vec::new());
        assert!(persistent_run(
            "turn-id-mismatch",
            &project.0,
            None,
            &[],
            "GOAL",
            &lifecycle,
            &events
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("标识不一致"));
        assert!(!lifecycle
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, CodexLifecycle::TurnCompleted { .. })));
    }

    #[tokio::test]
    async fn known_credentials_are_redacted_across_codex_deltas_and_final_reply() {
        let project = Project::new();
        let events = Mutex::new(Vec::new());
        let result = execute_with_continuation(
            &fixture(),
            "stream-secret",
            &project.0,
            ExecutionInput {
                objective: "本地测试目标",
                history: &[],
                config: &ModelContextConfig::default(),
            },
            |_| {},
            || false,
            |_| {},
            None,
            &["fixture-stream-secret".into()],
            |event| {
                events.lock().unwrap().push(serde_json::to_value(event)?);
                Ok(())
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(result, "[凭证已隐藏]");
        let events = events.lock().unwrap();
        let text = events
            .iter()
            .filter_map(|event| event["text"].as_str())
            .collect::<String>();
        assert!(text.starts_with("[凭证已隐藏]"));
        assert!(!text.contains("fixture-stream-secret"));
    }

    #[tokio::test]
    async fn codex_private_config_credentials_redact_deltas_tools_evidence_and_final_reply() {
        for persistent in [false, true] {
            let project = Project::new();
            let events = Mutex::new(Vec::new());
            let evidence = Mutex::new(Vec::new());
            let continuation = if persistent {
                Some(CodexContinuation {
                    thread_id: None,
                    expected_turn_id: None,
                    expected_config: Some(
                        probe_config_identity(&fixture(), &project.0).await.unwrap(),
                    ),
                })
            } else {
                None
            };
            let result = execute_with_continuation(
                &fixture(),
                "config-secret",
                &project.0,
                ExecutionInput {
                    objective: "本地测试目标",
                    history: &[],
                    config: &ModelContextConfig::default(),
                },
                |_| {},
                || false,
                |item| evidence.lock().unwrap().push(item),
                continuation,
                &[],
                |event| {
                    events.lock().unwrap().push(serde_json::to_value(event)?);
                    Ok(())
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
            assert_eq!(result, "[凭证已隐藏]");
            let events = events.lock().unwrap();
            let text = events
                .iter()
                .filter_map(|event| event["text"].as_str())
                .collect::<String>();
            assert!(text.starts_with("[凭证已隐藏]"));
            assert!(!serde_json::to_string(&*events)
                .unwrap()
                .contains("private-fixture-config-value"));
            assert!(events.iter().any(|event| event["kind"] == "tool_finished"
                && event["output"].as_str().unwrap().contains("[凭证已隐藏]")));
            let evidence = evidence.lock().unwrap();
            assert!(!serde_json::to_string(&*evidence)
                .unwrap()
                .contains("private-fixture-config-value"));
            assert!(evidence[0].output.contains("[凭证已隐藏]"));
        }
    }

    #[tokio::test]
    async fn managed_account_never_resumes_and_rebuilds_full_history() {
        let project = Project::new();
        std::fs::write(project.0.join("fixture-account-chatgpt"), "yes").unwrap();
        let identity = probe_config_identity(&fixture(), &project.0).await.unwrap();
        assert_eq!(identity["reuse_safe"], false);
        assert!(!identity.to_string().contains("private-user@example.test"));
        let history = vec![json!({"role":"assistant","content":"FULL_HISTORY"})];
        let events = Mutex::new(Vec::new());
        let first = Mutex::new(Vec::new());
        persistent_run(
            "success", &project.0, None, &history, "FIRST", &first, &events,
        )
        .await
        .unwrap();
        let old = ready_thread(&first);
        let second = Mutex::new(Vec::new());
        persistent_run(
            "success",
            &project.0,
            Some(old.clone()),
            &history,
            "SECOND",
            &second,
            &events,
        )
        .await
        .unwrap();
        let new = ready_thread(&second);
        assert_ne!(old, new);
        let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
        assert!(!trace.contains("thread/resume"));
        let state: Value = serde_json::from_slice(
            &std::fs::read(project.0.join("fixture-thread-state.json")).unwrap(),
        )
        .unwrap();
        assert!(state[new]["inputs"][0]
            .as_str()
            .unwrap()
            .contains("FULL_HISTORY"));
    }

    #[tokio::test]
    async fn externally_changed_failed_or_unreadable_latest_turn_never_starts_or_replays() {
        for mode in ["resume-external", "resume-failed", "resume-list-error"] {
            let project = Project::new();
            let first = Mutex::new(Vec::new());
            let events = Mutex::new(Vec::new());
            persistent_run("success", &project.0, None, &[], "FIRST", &first, &events)
                .await
                .unwrap();
            let thread = ready_thread(&first);
            let second = Mutex::new(Vec::new());
            let error = persistent_run(
                mode,
                &project.0,
                Some(thread),
                &[],
                "SECOND",
                &second,
                &events,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(!error.contains("private-list-error"));
            assert!(second.lock().unwrap().is_empty());
            let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
            let methods: Vec<String> = trace
                .lines()
                .map(|line| {
                    serde_json::from_str::<Value>(line).unwrap()["method"]
                        .as_str()
                        .unwrap()
                        .into()
                })
                .collect();
            assert_eq!(
                methods,
                vec![
                    "thread/start",
                    "turn/start",
                    "thread/resume",
                    "thread/turns/list"
                ]
            );
        }
    }

    fn supplement() -> TaskSupplement {
        TaskSupplement {
            id: "input-1".into(),
            task_id: "task-1".into(),
            content: "运行中补充目标".into(),
            status: "delivering".into(),
            reason: None,
            applied_via: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    async fn control_run(
        mode: &str,
        root: &Path,
        cancelled: impl Fn() -> bool,
        claim: impl Fn() -> Result<Vec<TaskSupplement>>,
        ack: impl Fn(&[String], InputAcknowledgement) -> Result<()>,
        events: impl Fn(ExecutionEvent) -> Result<()>,
    ) -> Result<String> {
        // This outer test deadline includes Python startup and process-tree
        // cleanup under a parallel CI suite; protocol ACK/count assertions below
        // still prove bounded delivery and no replay. It is not a runtime budget.
        timeout(
            Duration::from_secs(15),
            execute_with_control(
                &fixture(),
                mode,
                root,
                ExecutionInput {
                    objective: "本地测试目标",
                    history: &[],
                    config: &ModelContextConfig::default(),
                },
                |_| {},
                cancelled,
                |_| {},
                None,
                &[],
                events,
                |_| Ok(()),
                claim,
                ack,
                |_| {},
            ),
        )
        .await
        .expect("steering fixture terminates")
    }

    #[tokio::test]
    async fn failed_account_checks_still_supply_private_material_for_final_diff_filtering() {
        for case in ["account-error", "config-changed", "managed-changed"] {
            let project = if case == "managed-changed" {
                managed_project()
            } else {
                Project::new()
            };
            let expected = probe_config_identity(&fixture(), &project.0).await.unwrap();
            let needle = match case {
                "account-error" => {
                    std::fs::write(project.0.join("fixture-account-error"), "yes").unwrap();
                    "private-fixture-config-value"
                }
                "config-changed" => {
                    std::fs::write(project.0.join("fixture-config-changed"), "yes").unwrap();
                    "private-changed-value"
                }
                _ => {
                    let mut account = managed_account();
                    account["workspaceRouting"]["chatgptAccountId"] =
                        json!("private-changed-workspace");
                    write_managed_account(&project.0, &account);
                    "private-changed-workspace"
                }
            };
            let collected = Mutex::new(Vec::new());
            let result = execute_with_control(
                &fixture(),
                "success",
                &project.0,
                ExecutionInput {
                    objective: "NEVER_START",
                    history: &[],
                    config: &ModelContextConfig::default(),
                },
                |_| {},
                || false,
                |_| {},
                Some(CodexContinuation {
                    thread_id: None,
                    expected_turn_id: None,
                    expected_config: Some(expected),
                }),
                &[],
                |_| Ok(()),
                |_| Ok(()),
                || Ok(vec![]),
                |_, _| Ok(()),
                |values| collected.lock().unwrap().extend_from_slice(values),
            )
            .await;
            assert!(result.is_err());
            assert!(
                collected.lock().unwrap().contains(&needle.to_owned()),
                "{case}"
            );
            assert!(!project.0.join("fixture-rpc-trace.jsonl").exists());
        }
    }

    #[tokio::test]
    async fn queued_project_symlink_is_rejected_before_preflight_or_execution() {
        let project = Project::new();
        let outside = Project::new();
        let selected = project.0.join("selected");
        std::os::unix::fs::symlink(&outside.0, &selected).unwrap();
        let error = probe_config_identity(&fixture(), &selected)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("符号链接"));
        let error = run("success", &selected).await.unwrap_err().to_string();
        assert!(error.contains("符号链接"));
        assert!(std::fs::read_dir(&outside.0).unwrap().next().is_none());
        assert!(
            probe_version_with_control(&fixture(), Path::new("relative-root"), &|| false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn private_redaction_material_is_available_before_any_thread_operation() {
        let project = Project::new();
        let received = AtomicBool::new(false);
        execute_with_control(
            &fixture(),
            "success",
            &project.0,
            ExecutionInput {
                objective: "本地测试目标",
                history: &[],
                config: &ModelContextConfig::default(),
            },
            |_| {},
            || false,
            |_| {},
            None,
            &["private-harness-value".into()],
            |_| Ok(()),
            |_| Ok(()),
            || Ok(vec![]),
            |_, _| Ok(()),
            |secrets| {
                assert!(!project.0.join("fixture-rpc-trace.jsonl").exists());
                assert!(secrets.contains(&"private-fixture-config-value".to_owned()));
                assert!(secrets.contains(&"private-harness-value".to_owned()));
                received.store(true, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
        assert!(received.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn steering_polls_silent_turns_preserves_partial_utf8_and_deduplicates_ack() {
        for mode in ["steer-silent", "steer-partial", "steer-duplicate"] {
            let project = Project::new();
            let claimed = AtomicBool::new(false);
            let acks = Mutex::new(Vec::new());
            let events = Mutex::new(Vec::new());
            let queued_after = Instant::now() + Duration::from_millis(350);
            control_run(
                mode,
                &project.0,
                || false,
                || {
                    if Instant::now() < queued_after || claimed.swap(true, Ordering::SeqCst) {
                        Ok(vec![])
                    } else {
                        Ok(vec![supplement()])
                    }
                },
                |ids, status| {
                    acks.lock().unwrap().push((ids.to_vec(), status));
                    Ok(())
                },
                |event| {
                    events.lock().unwrap().push(serde_json::to_value(event)?);
                    Ok(())
                },
            )
            .await
            .unwrap();
            let acks = acks.lock().unwrap();
            assert_eq!(acks.len(), 1);
            assert_eq!(acks[0].0, vec!["input-1"]);
            assert!(
                matches!(&acks[0].1,InputAcknowledgement::Applied {via} if via=="codex_turn_steer")
            );
            let events = events.lock().unwrap();
            let estimate = events
                .iter()
                .find(|event| event["kind"] == "context_estimate")
                .unwrap();
            assert_eq!(estimate["source"], "codex_local_bootstrap");
            assert_eq!(estimate["phase"], "before_turn");
            if mode == "steer-partial" {
                let text = events
                    .iter()
                    .filter_map(|event| event["text"].as_str())
                    .collect::<String>();
                assert!(text.starts_with("分片"));
            }
            let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
            assert_eq!(
                trace
                    .lines()
                    .filter(
                        |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                            == "turn/steer"
                    )
                    .count(),
                1
            );
            assert_eq!(
                trace
                    .lines()
                    .filter(
                        |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                            == "turn/start"
                    )
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn steering_rejects_errors_and_never_acks_uncertain_or_late_responses() {
        for (mode, success, not_applied) in [
            ("steer-reject", true, true),
            ("steer-mismatch", false, false),
            ("steer-wrong-id", true, false),
            ("steer-complete-first", true, false),
            ("steer-disconnect", false, false),
        ] {
            let project = Project::new();
            let claimed = AtomicBool::new(false);
            let acks = Mutex::new(Vec::new());
            let result = control_run(
                mode,
                &project.0,
                || false,
                || {
                    Ok(if claimed.swap(true, Ordering::SeqCst) {
                        vec![]
                    } else {
                        vec![supplement()]
                    })
                },
                |ids, status| {
                    acks.lock().unwrap().push((ids.to_vec(), status));
                    Ok(())
                },
                |_| Ok(()),
            )
            .await;
            assert_eq!(result.is_ok(), success, "{mode}: {result:?}");
            let acks = acks.lock().unwrap();
            assert_eq!(acks.len(), usize::from(not_applied), "{mode}");
            if not_applied {
                assert!(
                    matches!(&acks[0].1,InputAcknowledgement::NotApplied {reason} if !reason.contains("private-steer-error"))
                );
            }
            let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
            assert_eq!(
                trace
                    .lines()
                    .filter(
                        |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                            == "turn/steer"
                    )
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn cancellation_with_inflight_steering_preserves_unknown_receipt_and_interrupts() {
        let project = Project::new();
        let cancel = AtomicBool::new(false);
        let claimed = AtomicBool::new(false);
        let task = control_run(
            "steer-cancel",
            &project.0,
            || cancel.load(Ordering::SeqCst),
            || {
                Ok(if claimed.swap(true, Ordering::SeqCst) {
                    vec![]
                } else {
                    vec![supplement()]
                })
            },
            |_, _| panic!("unconfirmed receipt"),
            |_| Ok(()),
        );
        let cancellation = async {
            timeout(Duration::from_secs(4), async {
                while !project.0.join("fixture-steer-received").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            cancel.store(true, Ordering::SeqCst);
        };
        let (result, ()) = tokio::join!(task, cancellation);
        assert!(result.unwrap_err().to_string().contains("已取消"));
        assert!(project.0.join("fixture-interrupted").exists());
    }

    #[tokio::test]
    async fn steering_ack_storage_failure_never_replays_the_input() {
        let project = Project::new();
        let claimed = AtomicBool::new(false);
        let error = control_run(
            "steer-silent",
            &project.0,
            || false,
            || {
                Ok(if claimed.swap(true, Ordering::SeqCst) {
                    vec![]
                } else {
                    vec![supplement()]
                })
            },
            |_, _| bail!("回执保存失败"),
            |_| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("回执保存失败"));
        let trace = std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
        assert_eq!(
            trace
                .lines()
                .filter(
                    |line| serde_json::from_str::<Value>(line).unwrap()["method"] == "turn/steer"
                )
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn model_catalog_paginates_without_starting_a_thread() {
        let models = list_models(&fixture()).await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "fixture-coder");
        assert_eq!(models[1]["id"], "fixture-reviewer");
        assert!(models.iter().all(|model| model["id"] != "internal-id"));
    }

    #[tokio::test]
    async fn model_context_and_prior_conversation_reach_codex_protocol() {
        use std::os::unix::fs::PermissionsExt;
        let project = Project::new();
        let history = vec![
            json!({"role":"user","content":"前一轮目标"}),
            json!({"role":"assistant","content":"前一轮真实结果"}),
        ];
        let expected = crate::context::history_prompt(&history, "本地测试目标").unwrap();
        let fixture_source = std::fs::read_to_string(fixture()).unwrap();
        let source = fixture_source
            .replacen(
                "params = thread[\"params\"]",
                "params = thread[\"params\"]\nassert params[\"config\"][\"model_context_window\"] == 64000\nassert params[\"config\"][\"model_auto_compact_token_limit\"] == 44800",
                1,
            )
            .replace(
                "assert turn[\"params\"][\"input\"][0][\"text\"] == \"本地测试目标\"",
                &format!(
                    "assert turn[\"params\"][\"input\"][0][\"text\"] == {}",
                    serde_json::to_string(&expected).unwrap()
                ),
            );
        let binary = project.0.join("context-fixture.py");
        std::fs::write(&binary, source).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = execute_with_context(
            binary.to_str().unwrap(),
            "success",
            &project.0,
            ExecutionInput {
                objective: "本地测试目标",
                history: &history,
                config: &ModelContextConfig {
                    context_window: 64_000,
                    compression_threshold: 70,
                },
            },
            |_| {},
            || false,
            |_| {},
        )
        .await
        .unwrap();
        assert!(!result.is_empty());
        assert!(project.0.join("fixture-started").exists());
    }

    async fn run(mode: &str, root: &Path) -> Result<String> {
        timeout(
            Duration::from_secs(8),
            execute(&fixture(), mode, root, "本地测试目标", |_| {}, || false),
        )
        .await
        .expect("fixture must terminate")
    }

    #[tokio::test]
    async fn real_completion_and_final_message_are_required() {
        let project = Project::new();
        let progress = Mutex::new(Vec::new());
        let output = execute(
            &fixture(),
            "success",
            &project.0,
            "本地测试目标",
            |message| progress.lock().unwrap().push(message),
            || false,
        )
        .await
        .unwrap();
        assert_eq!(output, "测试执行结果");
        assert_eq!(
            std::fs::read_to_string(project.0.join("fixture-result.txt")).unwrap(),
            "真实本地夹具写入"
        );
        assert!(progress
            .lock()
            .unwrap()
            .iter()
            .any(|message| message.contains("执行项目命令")));
    }

    #[tokio::test]
    async fn failed_turn_keeps_real_command_evidence_once() {
        let project = Project::new();
        let evidence = Mutex::new(Vec::new());
        let result = execute_with_evidence(
            &fixture(),
            "evidence-failure",
            &project.0,
            "本地测试目标",
            |_| {},
            || false,
            |record| evidence.lock().unwrap().push(record),
        )
        .await;
        assert!(result.is_err());
        let evidence = evidence.lock().unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].command, "python3 check.py");
        assert_eq!(evidence[0].exit_code, 1);
        assert!(evidence[0]
            .output
            .starts_with("原始命令：/bin/zsh -lc 'python3 check.py'\n"));
        assert!(evidence[0].output.ends_with("测试失败"));
    }

    #[tokio::test]
    async fn agent_text_and_other_turn_cannot_forge_command_evidence() {
        for mode in ["evidence-text", "evidence-wrong-turn"] {
            let project = Project::new();
            let evidence = Mutex::new(Vec::new());
            execute_with_evidence(
                &fixture(),
                mode,
                &project.0,
                "本地测试目标",
                |_| {},
                || false,
                |record| evidence.lock().unwrap().push(record),
            )
            .await
            .unwrap();
            assert!(evidence.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn command_evidence_is_bounded_without_breaking_utf8() {
        for (mode, count) in [("evidence-large", 1), ("evidence-many", 70)] {
            let project = Project::new();
            let evidence = Mutex::new(Vec::new());
            execute_with_evidence(
                &fixture(),
                mode,
                &project.0,
                "本地测试目标",
                |_| {},
                || false,
                |record| evidence.lock().unwrap().push(record),
            )
            .await
            .unwrap();
            let evidence = evidence.lock().unwrap();
            assert_eq!(evidence.len(), count);
            assert!(evidence
                .iter()
                .all(|record| record.output.len() <= 16 * 1024));
            if mode == "evidence-large" {
                assert!(evidence[0].output.len() >= 16 * 1024 - 3);
            } else {
                assert_eq!(evidence[64].exit_code, 1);
                assert!(evidence[64].output.ends_with("第 65 条命令失败"));
            }
        }
    }

    #[tokio::test]
    async fn provider_credentials_are_redacted_before_evidence_truncation() {
        for persistent in [false, true] {
            let project = Project::new();
            let evidence = Mutex::new(Vec::new());
            execute_with_continuation(
                &fixture(),
                "config-secret-boundary",
                &project.0,
                ExecutionInput {
                    objective: "本地测试目标",
                    history: &[],
                    config: &ModelContextConfig::default(),
                },
                |_| {},
                || false,
                |item| evidence.lock().unwrap().push(item),
                persistent.then_some(CodexContinuation {
                    thread_id: None,
                    expected_turn_id: None,
                    expected_config: None,
                }),
                &[],
                |_| Ok(()),
                |_| Ok(()),
            )
            .await
            .unwrap();
            let evidence = evidence.lock().unwrap();
            assert_eq!(evidence.len(), 1);
            assert!(evidence[0].output.len() <= 16 * 1024);
            assert!(!evidence[0].output.contains("private-fixture"));
            assert!(evidence[0].output.ends_with("[凭证已隐藏]"));
        }
    }

    #[tokio::test]
    async fn missing_or_mismatched_effective_model_and_provider_never_start_turn() {
        for mode in [
            "model-missing",
            "model-null",
            "model-mismatch",
            "provider-missing",
            "provider-null",
            "provider-mismatch",
        ] {
            for resume in [false, true] {
                let project = Project::new();
                let first = Mutex::new(Vec::new());
                let events = Mutex::new(Vec::new());
                let thread = if resume {
                    persistent_run("success", &project.0, None, &[], "FIRST", &first, &events)
                        .await
                        .unwrap();
                    Some(ready_thread(&first))
                } else {
                    None
                };
                let rejected = Mutex::new(Vec::new());
                let error = persistent_run(
                    mode,
                    &project.0,
                    thread,
                    &[],
                    "REJECTED",
                    &rejected,
                    &events,
                )
                .await
                .unwrap_err()
                .to_string();
                assert!(error.contains("模型"));
                assert!(rejected.lock().unwrap().is_empty());
                let trace =
                    std::fs::read_to_string(project.0.join("fixture-rpc-trace.jsonl")).unwrap();
                let turns = trace
                    .lines()
                    .filter(|line| {
                        serde_json::from_str::<Value>(line).unwrap()["method"] == "turn/start"
                    })
                    .count();
                assert_eq!(turns, usize::from(resume));
                assert!(!trace.contains("REJECTED"));
            }
        }
    }

    #[test]
    fn nonterminal_missing_exit_and_declined_commands_are_not_evidence() {
        let mut seen = HashSet::new();
        for item in [
            json!({"type":"commandExecution","id":"1","command":"true","status":"inProgress","exitCode":0}),
            json!({"type":"commandExecution","id":"2","command":"true","status":"declined","exitCode":0}),
            json!({"type":"commandExecution","id":"3","command":"true","status":"completed","exitCode":null}),
        ] {
            capture_evidence(&item, &mut seen, &[], &|_| {
                panic!("invalid evidence accepted")
            });
        }
        assert!(seen.is_empty());
    }

    #[test]
    fn shell_normalization_keeps_the_complete_script_and_rejects_outer_syntax() {
        for (input, expected) in [
            ("/bin/zsh -lc 'python3 check.py'", "python3 check.py"),
            ("bash -c \"python3 check.py\"", "python3 check.py"),
            (
                "/bin/sh -c 'python3 check.py || true'",
                "python3 check.py || true",
            ),
            (
                r#"sh -c 'printf '\''quoted'\''; python3 check.py'"#,
                "printf 'quoted'; python3 check.py",
            ),
            (" python3 check.py ", "python3 check.py"),
            ("sh -c true extra", "sh -c true extra"),
            ("sh -c 'unterminated", "sh -c 'unterminated"),
            (
                "sh -c 'python3 check.py'; true",
                "sh -c 'python3 check.py'; true",
            ),
            ("sh -c \"$(echo true)\"", "sh -c \"$(echo true)\""),
            (
                "/tmp/sh -c 'python3 check.py'",
                "/tmp/sh -c 'python3 check.py'",
            ),
        ] {
            assert_eq!(normalize_command(input), expected, "{input}");
        }
    }

    #[tokio::test]
    async fn failure_after_agent_message_is_not_reported_as_success() {
        let project = Project::new();
        let error = run("failure", &project.0).await.unwrap_err().to_string();
        assert!(error.contains("执行失败"));
        assert!(!error.contains("secret"));
    }

    #[tokio::test]
    async fn unsafe_thread_configuration_is_rejected_before_starting_turn() {
        let project = Project::new();
        let error = run("unsafe", &project.0).await.unwrap_err().to_string();
        assert!(error.contains("权限超出"));
        assert!(!project.0.join("fixture-started").exists());
    }

    #[tokio::test]
    async fn cancelling_hanging_turn_interrupts_and_reaps_process() {
        let project = Project::new();
        let cancel = AtomicBool::new(false);
        let acknowledged = AtomicBool::new(false);
        let binary = fixture();
        let task = execute(
            &binary,
            "hang",
            &project.0,
            "本地测试目标",
            |message| {
                if message == "Codex 正在处理任务" {
                    acknowledged.store(true, Ordering::SeqCst);
                }
            },
            || cancel.load(Ordering::SeqCst),
        );
        let cancel_later = async {
            timeout(Duration::from_secs(5), async {
                // A server-side file only proves it wrote the ACK, not that
                // our client has consumed it and knows the interrupt turn ID.
                while !acknowledged.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            cancel.store(true, Ordering::SeqCst);
        };
        let (result, ()) = tokio::join!(task, cancel_later);
        assert!(result.unwrap_err().to_string().contains("已取消"));
        assert!(project.0.join("fixture-interrupted").exists());
        let pid = std::fs::read_to_string(project.0.join("fixture-started")).unwrap();
        let alive = Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(!alive.success(), "app-server was not reaped");
    }

    #[tokio::test]
    async fn approval_request_is_denied_without_hanging() {
        let project = Project::new();
        let error = run("approval", &project.0).await.unwrap_err().to_string();
        assert!(error.contains("不支持的授权"));
        assert!(project.0.join("fixture-denied").exists());
    }

    #[tokio::test]
    async fn malformed_truncated_and_oversized_streams_are_rejected() {
        for (mode, expected) in [
            ("eof", "回合完成前"),
            ("malformed", "无法解析"),
            ("oversize", "安全预算"),
            ("rpc-error", "协议请求失败"),
        ] {
            let project = Project::new();
            let error = run(mode, &project.0).await.unwrap_err().to_string();
            assert!(error.contains(expected), "{mode}: {error}");
            assert!(!error.contains("secret"));
        }
    }
}
