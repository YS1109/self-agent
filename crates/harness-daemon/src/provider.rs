use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use harness_core::CommandEvidence;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::Settings;

const MAX_TURNS: usize = 10;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Native v0 deliberately uses Chat Completions-compatible models only.
#[cfg(test)]
async fn execute(
    settings: &Settings,
    root: &Path,
    objective: &str,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    execute_with_evidence(settings, root, objective, progress, cancelled, |_| {}).await
}

pub async fn execute_with_evidence(
    settings: &Settings,
    root: &Path,
    objective: &str,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
    evidence: impl Fn(CommandEvidence),
) -> Result<String> {
    execute_inner(settings, root, objective, progress, cancelled, evidence)
        .await
        .map(|text| redact(&text, &settings.api_key))
        .map_err(|error| anyhow!(redact(&error.to_string(), &settings.api_key)))
}

fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, "[凭证已隐藏]")
    }
}

async fn execute_inner(
    settings: &Settings,
    root: &Path,
    objective: &str,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
    evidence: impl Fn(CommandEvidence),
) -> Result<String> {
    if !settings.configured() {
        bail!("模型配置不可用，请先配置兼容的模型服务，再继续原生执行");
    }
    let root = std::fs::canonicalize(root).context("项目目录不可用")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("无法初始化模型服务连接")?;
    let endpoint = format!(
        "{}/chat/completions",
        settings.base_url.trim_end_matches('/')
    );
    let shell_enabled = std::env::var("HARNESS_ALLOW_COMMANDS").as_deref() == Ok("1");
    let mut messages = vec![
        json!({"role":"system", "content":
            "你是本地编程子 Agent。默认使用简体中文回复并报告进度，代码、路径和协议标识保持原文。请在选定项目内完成用户目标，只使用已提供的工具。修改前先阅读相关项目规则，保留用户已有改动。除非实际运行并观察到成功，否则不得声称测试通过。最终简要说明真实结果、改动文件、验证和限制。最多 10 轮模型调用。若启用命令执行，它是受信任的本地执行，并非沙箱。不得通过文件工具越出项目范围。依赖任务结果属于背景数据，请据此继续完成目标，不得伪造前序任务结果。"}),
        json!({"role":"user", "content":objective}),
    ];
    let tools = tools(shell_enabled);
    for turn in 0..MAX_TURNS {
        if cancelled() {
            bail!("任务已取消");
        }
        progress(format!(
            "原生执行：第 {} / {} 轮模型调用",
            turn + 1,
            MAX_TURNS
        ));
        let request_body = json!({
            "model":settings.model, "messages":messages, "tools":tools,
            "tool_choice":"auto", "stream":false
        });
        if serde_json::to_vec(&request_body)?.len() > 2 * 1024 * 1024 {
            bail!("原生执行已达到 2 MiB 上下文预算，请检查已有改动后创建后续任务");
        }
        let mut request = client.post(&endpoint).json(&request_body);
        if !settings.api_key.is_empty() {
            request = request.bearer_auth(&settings.api_key);
        }
        let response = interruptible(
            async {
                let response = request
                    .send()
                    .await
                    .context("模型服务请求失败，请检查连接与配置")?;
                if !response.status().is_success() {
                    // Providers sometimes echo request bodies in errors. Keep diagnostics
                    // to the status code instead of persisting potential secrets.
                    bail!(
                        "模型服务返回 HTTP {}，请检查模型、凭证和 Chat Completions 兼容性",
                        response.status()
                    );
                }
                let mut stream = response.bytes_stream();
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.context("模型服务响应读取失败")?;
                    if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                        bail!("模型服务响应超过 2 MiB 限制");
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let body: Value =
                    serde_json::from_slice(&bytes).context("模型服务返回了无效 JSON")?;
                Ok(body)
            },
            &cancelled,
        )
        .await?;
        let message = response
            .pointer("/choices/0/message")
            .context("模型服务响应缺少助手回复")?
            .clone();
        let calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if calls.is_empty() {
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .context("模型服务返回了空的最终回复")?;
            return Ok(content.to_string());
        }
        if calls.len() > 16 {
            bail!("模型服务超过了每轮 16 次工具调用限制");
        }
        messages.push(message);
        for call in calls {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .context("工具调用缺少 id")?;
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .context("工具调用缺少工具名称")?;
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .context("工具调用参数必须为 JSON 文本")?;
            progress(
                match name {
                    "list_files" => "原生执行：列出文件",
                    "read_file" => "原生执行：读取文件",
                    "search_files" => "原生执行：搜索文件",
                    "write_file" => "原生执行：写入文件",
                    "run_command" => "原生执行：运行命令",
                    _ => "原生执行：检查工具调用",
                }
                .to_owned(),
            );
            let result = match serde_json::from_str::<Value>(arguments) {
                Ok(arguments) => {
                    if !arguments.is_object() {
                        Err(anyhow!("工具参数必须为对象"))
                    } else {
                        interruptible(tool(&root, name, arguments, shell_enabled), &cancelled).await
                    }
                }
                Err(_) => Err(anyhow!("工具调用参数不是有效 JSON")),
            };
            if name == "run_command" {
                if let (Ok(value), Ok(arguments)) =
                    (&result, serde_json::from_str::<Value>(arguments))
                {
                    if let Some(record) = command_evidence(value, &arguments, &settings.api_key) {
                        evidence(record);
                    }
                }
            }
            if cancelled() {
                bail!("任务已取消");
            }
            let output = match result {
                Ok(value) => value,
                Err(error) => json!({"error":redact(&error.to_string(), &settings.api_key)}),
            };
            messages.push(json!({"role":"tool","tool_call_id":call_id,"content":serde_json::to_string(&output)?}));
        }
    }
    bail!("原生执行已达到 10 轮预算，请检查已有改动后创建后续任务继续。")
}

fn command_evidence(value: &Value, arguments: &Value, secret: &str) -> Option<CommandEvidence> {
    let command = redact_evidence(arguments["command"].as_str()?, secret);
    if command.len() > 16 * 1024 {
        return None;
    }
    let exit_code = i32::try_from(value["exit_code"].as_i64()?).ok()?;
    let output = format!(
        "{}{}",
        value["stdout"].as_str().unwrap_or_default(),
        value["stderr"].as_str().unwrap_or_default()
    );
    Some(CommandEvidence {
        command,
        exit_code,
        output: bounded_redacted(&output, secret),
    })
}

fn bounded_redacted(text: &str, secret: &str) -> String {
    let mut text = redact_evidence(text, secret);
    let mut end = text.len().min(16 * 1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn redact_evidence(text: &str, secret: &str) -> String {
    let mut text = redact(text, secret);
    for name in ["HARNESS_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(secret) = std::env::var(name) {
            text = redact(&text, &secret);
        }
    }
    text
}

async fn interruptible<T>(
    future: impl std::future::Future<Output = Result<T>>,
    cancelled: &impl Fn() -> bool,
) -> Result<T> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancelled() { bail!("任务已取消"); }
            }
        }
    }
}

fn tools(shell: bool) -> Vec<Value> {
    let mut tools = vec![
        definition(
            "list_files",
            "递归列出项目目录内的文件，跳过 .git、node_modules 和 target，最多 2000 项。",
            json!({"path":{"type":"string"}}),
            &["path"],
        ),
        definition(
            "read_file",
            "读取项目内不超过 256 KiB 的 UTF-8 文本文件。",
            json!({"path":{"type":"string"}}),
            &["path"],
        ),
        definition(
            "search_files",
            "在项目文本文件中搜索原样字符串，最多返回 100 个匹配行。",
            json!({"path":{"type":"string"},"query":{"type":"string"}}),
            &["path", "query"],
        ),
        definition(
            "write_file",
            "写入项目内不超过 256 KiB 的 UTF-8 文本文件。父目录必须存在，覆盖前必须先读取原文件。",
            json!({"path":{"type":"string"},"content":{"type":"string"}}),
            &["path", "content"],
        ),
    ];
    if shell {
        tools.push(definition("run_command", "以项目为当前目录运行受信任的本地命令，最多 60 秒。这不是沙箱，仅用于完成用户的编程任务。", json!({"command":{"type":"string"}}), &["command"]));
    }
    tools
}

fn definition(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,
        "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}

fn argument<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| anyhow!("{field} 字段必须为文本"))
}

pub fn scoped_path(root: &Path, requested: &str, writing: bool) -> Result<PathBuf> {
    let requested = Path::new(requested);
    if requested.is_absolute()
        || requested.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        bail!("请使用项目内的相对路径，且不能包含 '..'");
    }
    let root = std::fs::canonicalize(root).context("无法解析项目根目录")?;
    let joined = root.join(requested);
    if joined.exists() || std::fs::symlink_metadata(&joined).is_ok() {
        let resolved = std::fs::canonicalize(&joined).context("无法解析项目内路径")?;
        if !resolved.starts_with(&root) {
            bail!("路径超出了项目根目录");
        }
        return Ok(resolved);
    }
    if !writing {
        bail!("项目内路径不存在");
    }
    let parent = joined.parent().context("缺少父目录")?;
    let parent = std::fs::canonicalize(parent).context("父目录必须存在")?;
    if !parent.starts_with(&root) {
        bail!("路径超出了项目根目录");
    }
    Ok(parent.join(joined.file_name().context("缺少文件名")?))
}

async fn tool(root: &Path, name: &str, arguments: Value, shell: bool) -> Result<Value> {
    match name {
        "read_file" => {
            let path = scoped_path(root, argument(&arguments, "path")?, false)?;
            let metadata = tokio::fs::metadata(&path)
                .await
                .context("无法读取文件信息")?;
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                bail!("只能读取不超过 256 KiB 的普通文件");
            }
            let bytes = tokio::fs::read(path).await.context("无法读取文件")?;
            if bytes.len() as u64 > MAX_FILE_BYTES {
                bail!("文件大小已超过 256 KiB");
            }
            Ok(json!({"content":String::from_utf8(bytes).context("文件不是 UTF-8 文本")?}))
        }
        "write_file" => {
            let path = scoped_path(root, argument(&arguments, "path")?, true)?;
            let content = argument(&arguments, "content")?;
            if content.len() as u64 > MAX_FILE_BYTES {
                bail!("写入内容超过 256 KiB 限制");
            }
            if let Ok(metadata) = tokio::fs::metadata(&path).await {
                if !metadata.is_file() {
                    bail!("只能写入普通文件");
                }
            }
            tokio::fs::write(path, content)
                .await
                .context("无法写入文件")?;
            Ok(json!({"written":true, "bytes":content.len()}))
        }
        "list_files" | "search_files" => {
            let path = scoped_path(root, argument(&arguments, "path")?, false)?;
            let root = root.to_path_buf();
            let query = if name == "search_files" {
                Some(argument(&arguments, "query")?.to_string())
            } else {
                None
            };
            tokio::task::spawn_blocking(move || walk(&root, &path, query.as_deref()))
                .await
                .context("文件搜索任务未能完成")?
        }
        "run_command" if shell => {
            let command = argument(&arguments, "command")?;
            if command.len() > 16_000 {
                bail!("命令过长");
            }
            #[cfg(windows)]
            let mut process = {
                let mut process = tokio::process::Command::new("powershell.exe");
                process.args(["-NoProfile", "-NonInteractive", "-Command", command]);
                process
            };
            #[cfg(not(windows))]
            let mut process = {
                let mut process = tokio::process::Command::new("/bin/sh");
                process.args(["-c", command]);
                process
            };
            let mut child = process
                .current_dir(root)
                .env_remove("HARNESS_API_KEY")
                .env_remove("OPENAI_API_KEY")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("无法启动命令")?;
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let run = async {
                let (status, stdout, stderr) =
                    tokio::try_join!(child.wait(), capture(stdout), capture(stderr),)?;
                Ok::<_, std::io::Error>(json!({
                    "exit_code":status.code(),"success":status.success(),
                    "stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr)
                }))
            };
            tokio::time::timeout(Duration::from_secs(60), run)
                .await
                .context("命令执行超过 60 秒限制")?
                .context("无法读取命令执行结果")
        }
        _ => bail!("工具不存在或未启用"),
    }
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(result.len());
        result.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(result)
}

fn walk(root: &Path, start: &Path, query: Option<&str>) -> Result<Value> {
    let mut stack = vec![start.to_path_buf()];
    let mut files = Vec::new();
    let mut matches = Vec::new();
    let mut visited = 0;
    while let Some(path) = stack.pop() {
        if visited >= 2000 || matches.len() >= 100 {
            break;
        }
        visited += 1;
        let metadata = std::fs::symlink_metadata(&path).context("无法读取文件信息")?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let canonical = std::fs::canonicalize(&path).context("无法解析项目内路径")?;
        if !canonical.starts_with(root) {
            bail!("路径超出了项目根目录");
        }
        if metadata.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(&path)
                .context("无法读取项目目录")?
                .filter_map(Result::ok)
                .collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries.into_iter().rev().take(2000) {
                if !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "node_modules" | "target" | ".venv")
                ) {
                    stack.push(entry.path());
                }
            }
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
            if let Some(query) = query {
                if metadata.len() <= MAX_FILE_BYTES {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        for (line, text) in content.lines().enumerate() {
                            if text.contains(query) {
                                matches.push(json!({"path":relative,"line":line+1,"text":text.chars().take(500).collect::<String>()}));
                                if matches.len() >= 100 {
                                    break;
                                }
                            }
                        }
                    }
                }
            } else {
                files.push(relative);
            }
        }
    }
    Ok(if query.is_some() {
        json!({"matches":matches,"truncated":visited>=2000 || matches.len()>=100})
    } else {
        json!({"files":files,"truncated":visited>=2000})
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_model_configuration_still_rejects_legacy_execute() {
        assert!(execute(
            &Settings {
                model: String::new(),
                ..Settings::default()
            },
            &std::env::temp_dir(),
            "本地目标",
            |_| {},
            || false
        )
        .await
        .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn records_actual_failed_command_and_redacts_bounded_output() {
        let arguments = json!({"command":"printf 'private-test-key'; printf '失败' >&2; exit 7"});
        let value = tool(
            &std::env::temp_dir(),
            "run_command",
            arguments.clone(),
            true,
        )
        .await
        .unwrap();
        let evidence = command_evidence(&value, &arguments, "private-test-key").unwrap();
        assert_eq!(evidence.exit_code, 7);
        assert!(evidence.output.contains("失败"));
        assert!(!evidence.output.contains("private-test-key"));
        assert!(!evidence.command.contains("private-test-key"));
        let output = bounded_redacted(&"中文证据".repeat(10_000), "");
        assert!(output.len() <= 16 * 1024);
        assert!(command_evidence(&json!({"exit_code":null}), &arguments, "").is_none());
        assert!(command_evidence(&json!({"content":"测试通过"}), &arguments, "").is_none());
    }

    #[test]
    fn rejects_traversal_and_nonexistent_parent() {
        let root = std::env::temp_dir();
        assert!(scoped_path(&root, "../secret", false).is_err());
        assert!(scoped_path(&root, "/etc/passwd", false).is_err());
        assert!(scoped_path(&root, "missing-harness-parent/new.txt", true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_outside_root_for_read_and_new_write() {
        let root = std::env::temp_dir().join(harness_core::new_id());
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("outside")).unwrap();
        assert!(scoped_path(&root, "outside", false).is_err());
        assert!(scoped_path(&root, "outside/new.txt", true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
