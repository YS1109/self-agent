//! A bounded, single-turn Codex app-server client. Protocol data and stderr are
//! deliberately excluded from progress/errors because they can contain secrets.

use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};

use anyhow::{bail, Context, Result};
use harness_core::CommandEvidence;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{timeout, Instant},
};

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

pub async fn execute_with_evidence(
    binary: &str,
    model: &str,
    root: &Path,
    objective: &str,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
    evidence: impl Fn(CommandEvidence),
) -> Result<String> {
    if cancelled() {
        bail!("Codex 任务已取消");
    }
    if objective.len() > MAX_RESULT || model.len() > 256 {
        bail!("Codex 任务输入超过大小限制");
    }
    let root = root
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("Codex 项目目录不可访问"))?;
    if !root.is_dir() {
        bail!("Codex 项目路径必须为目录");
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
            &mut stdin, stdout, model, &root, objective, (&progress, &evidence), &mut ids,
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

async fn protocol(
    stdin: &mut ChildStdin,
    stdout: ChildStdout,
    model: &str,
    root: &Path,
    objective: &str,
    callbacks: (&impl Fn(String), &impl Fn(CommandEvidence)),
    ids: &mut RunIds,
) -> Result<String> {
    let (progress, evidence) = callbacks;
    progress("正在连接 Codex 执行器".into());
    send(
        stdin,
        json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"self_agent","title":"Self Agent","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":false}
        }}),
    )
    .await?;
    let mut reader = BufReader::new(stdout);
    let mut total = 0usize;
    let mut count = 0usize;
    let mut expected = 1u64;
    let mut answer = None;
    let mut command_ids = HashSet::new();
    loop {
        // `take` bounds allocation even when a peer never writes a newline.
        let mut line = Vec::new();
        let bytes = (&mut reader)
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| anyhow::anyhow!("读取 Codex 协议消息失败"))?;
        if bytes == 0 {
            bail!("Codex 在回合完成前关闭了连接");
        }
        total += bytes;
        count += 1;
        if bytes > MAX_LINE || total > MAX_OUTPUT || count > MAX_LINES {
            bail!("Codex 协议输出超过安全预算");
        }
        let message: Value = serde_json::from_slice(&line)
            .map_err(|_| anyhow::anyhow!("Codex 返回了无法解析的协议消息"))?;
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = message.get("id") {
                // No approval or interactive tool flow is implemented here.
                // Explicitly answer requests before stopping, rather than hang.
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
                ids.turn = params["turn"]["id"].as_str().map(str::to_owned);
            }
            let event_turn = if method == "turn/completed" {
                params["turn"]["id"].as_str()
            } else {
                params["turnId"].as_str()
            };
            if ids.turn.is_none() || event_turn != ids.turn.as_deref() {
                continue;
            }
            match method {
                "item/started" => match params["item"]["type"].as_str() {
                    Some("commandExecution") => progress("Codex 正在执行项目命令".into()),
                    Some("fileChange") => progress("Codex 正在修改项目文件".into()),
                    Some("mcpToolCall") => progress("Codex 正在调用已配置工具".into()),
                    _ => {}
                },
                "item/completed" => {
                    capture_answer(&params["item"], &mut answer)?;
                    capture_evidence(&params["item"], &mut command_ids, evidence);
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
                            capture_answer(item, &mut answer)?;
                        }
                    }
                    return answer
                        .filter(|text: &String| !text.trim().is_empty())
                        .ok_or_else(|| anyhow::anyhow!("Codex 回合结束但未返回最终结果"));
                }
                _ => {}
            }
            continue;
        }
        if expected == 0 || message["id"].as_u64() != Some(expected) {
            continue;
        }
        if message.get("error").is_some() {
            bail!("Codex 协议请求失败，请检查本机版本、登录和模型配置");
        }
        let result = &message["result"];
        match expected {
            1 => {
                if !result.is_object() {
                    bail!("Codex 初始化响应无效");
                }
                send(stdin, json!({"method":"initialized","params":{}})).await?;
                let mut params = json!({
                    "cwd":root,"approvalPolicy":"never","sandbox":"workspace-write",
                    "ephemeral":true,
                    "config":{
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
                send(
                    stdin,
                    json!({"id":2,"method":"thread/start","params":params}),
                )
                .await?;
                expected = 2;
            }
            2 => {
                let thread = result["thread"]["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Codex 未返回有效会话标识"))?;
                verify_sandbox(result, root)?;
                ids.thread = Some(thread.to_owned());
                progress("Codex 已创建独立执行会话，权限限定为项目目录写入".into());
                send(
                    stdin,
                    json!({"id":3,"method":"turn/start","params":{
                        "threadId":thread,"cwd":root,"approvalPolicy":"never",
                        "sandboxPolicy":{
                            "type":"workspaceWrite","writableRoots":[root],"networkAccess":false,
                            "excludeTmpdirEnvVar":true,"excludeSlashTmp":true
                        },
                        "input":[{"type":"text","text":objective}]
                    }}),
                )
                .await?;
                expected = 3;
            }
            3 => {
                let turn = result["turn"]["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Codex 未返回有效回合标识"))?;
                if ids.turn.as_deref().is_some_and(|known| known != turn) {
                    bail!("Codex 回合标识不一致");
                }
                ids.turn = Some(turn.to_owned());
                progress("Codex 正在处理任务".into());
                expected = 0;
            }
            _ => {}
        }
    }
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
    let redacted_command = redact_evidence(&normalized);
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
        output: bounded_evidence(&output),
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

fn bounded_evidence(text: &str) -> String {
    let mut text = redact_evidence(text);
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
            Self(path)
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

    #[tokio::test]
    async fn model_catalog_paginates_without_starting_a_thread() {
        let models = list_models(&fixture()).await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "fixture-coder");
        assert_eq!(models[1]["id"], "fixture-reviewer");
        assert!(models.iter().all(|model| model["id"] != "internal-id"));
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

    #[test]
    fn nonterminal_missing_exit_and_declined_commands_are_not_evidence() {
        let mut seen = HashSet::new();
        for item in [
            json!({"type":"commandExecution","id":"1","command":"true","status":"inProgress","exitCode":0}),
            json!({"type":"commandExecution","id":"2","command":"true","status":"declined","exitCode":0}),
            json!({"type":"commandExecution","id":"3","command":"true","status":"completed","exitCode":null}),
        ] {
            capture_evidence(&item, &mut seen, &|_| panic!("invalid evidence accepted"));
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
        let binary = fixture();
        let task = execute(
            &binary,
            "hang",
            &project.0,
            "本地测试目标",
            |_| {},
            || cancel.load(Ordering::SeqCst),
        );
        let cancel_later = async {
            timeout(Duration::from_secs(5), async {
                while !project.0.join("fixture-started").exists() {
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
