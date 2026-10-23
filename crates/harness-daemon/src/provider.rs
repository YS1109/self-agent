use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::Settings;

const MAX_TURNS: usize = 10;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Native v0 deliberately uses Chat Completions-compatible models only.
pub async fn execute(
    settings: &Settings,
    root: &Path,
    objective: &str,
    progress: impl Fn(String),
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    if !settings.configured() {
        bail!("Provider configuration is unavailable; configure a compatible model before resuming native execution");
    }
    let root = std::fs::canonicalize(root).context("Project directory unavailable")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let endpoint = format!(
        "{}/chat/completions",
        settings.base_url.trim_end_matches('/')
    );
    let shell_enabled = std::env::var("HARNESS_ALLOW_COMMANDS").as_deref() == Ok("1");
    let mut messages = vec![
        json!({"role":"system", "content":
            "You are a local coding worker. Complete the user's objective inside the selected workspace. Use only the provided tools. Read relevant project instructions before editing. Preserve existing user changes. Do not claim tests passed unless you actually ran them and observed success. Finish with a concise result, files changed, verification, and limitations. You have at most 10 model turns. Shell execution, if available, is trusted local execution, not a sandbox. Never attempt to escape the selected workspace with file tools."}),
        json!({"role":"user", "content":objective}),
    ];
    let tools = tools(shell_enabled);
    for turn in 0..MAX_TURNS {
        if cancelled() {
            bail!("Cancelled");
        }
        progress(format!(
            "Native worker: model turn {} of {}",
            turn + 1,
            MAX_TURNS
        ));
        let request_body = json!({
            "model":settings.model, "messages":messages, "tools":tools,
            "tool_choice":"auto", "stream":false
        });
        if serde_json::to_vec(&request_body)?.len() > 2 * 1024 * 1024 {
            bail!("Native worker reached its 2 MiB context budget; inspect changes and start a follow-up task");
        }
        let mut request = client.post(&endpoint).json(&request_body);
        if !settings.api_key.is_empty() {
            request = request.bearer_auth(&settings.api_key);
        }
        let response = interruptible(async {
            let response = request.send().await.context("Provider request failed")?;
            if !response.status().is_success() {
                // Providers sometimes echo request bodies in errors. Keep diagnostics
                // to the status code instead of persisting potential secrets.
                bail!("Provider returned HTTP {}; check model, credentials and Chat Completions compatibility", response.status());
            }
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("Provider response stream failed")?;
                if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                    bail!("Provider response exceeded 2 MiB limit");
                }
                bytes.extend_from_slice(&chunk);
            }
            let body: Value = serde_json::from_slice(&bytes).context("Invalid provider JSON response")?;
            Ok(body)
        }, &cancelled).await?;
        let message = response
            .pointer("/choices/0/message")
            .context("Provider response has no assistant message")?
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
                .context("Provider returned an empty final message")?;
            return Ok(content.to_string());
        }
        if calls.len() > 16 {
            bail!("Provider exceeded the limit of 16 tool calls per turn");
        }
        messages.push(message);
        for call in calls {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .context("Tool call is missing id")?;
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .context("Tool call is missing name")?;
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .context("Tool call arguments must be JSON text")?;
            progress(format!("Native worker: {name}"));
            let result = match serde_json::from_str::<Value>(arguments) {
                Ok(arguments) => {
                    if !arguments.is_object() {
                        Err(anyhow!("Tool arguments must be an object"))
                    } else {
                        interruptible(tool(&root, name, arguments, shell_enabled), &cancelled).await
                    }
                }
                Err(error) => Err(anyhow!("Invalid JSON tool arguments: {error}")),
            };
            if cancelled() {
                bail!("Cancelled");
            }
            let output = match result {
                Ok(value) => value,
                Err(error) => json!({"error":error.to_string()}),
            };
            messages.push(json!({"role":"tool","tool_call_id":call_id,"content":serde_json::to_string(&output)?}));
        }
    }
    bail!("Native worker reached its 10-turn budget. Inspect changes and dispatch a follow-up task to continue.")
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
                if cancelled() { bail!("Cancelled"); }
            }
        }
    }
}

fn tools(shell: bool) -> Vec<Value> {
    let mut tools = vec![
        definition("list_files", "List files recursively within a workspace directory, skipping .git, node_modules and target. Up to 2000 entries.", json!({"path":{"type":"string"}}), &["path"]),
        definition("read_file", "Read a UTF-8 workspace file up to 256 KiB.", json!({"path":{"type":"string"}}), &["path"]),
        definition("search_files", "Search workspace text files for a literal string, up to 100 matching lines.", json!({"path":{"type":"string"},"query":{"type":"string"}}), &["path","query"]),
        definition("write_file", "Write a UTF-8 workspace file up to 256 KiB. Parent directory must already exist. Read existing files before replacing them.", json!({"path":{"type":"string"},"content":{"type":"string"}}), &["path","content"]),
    ];
    if shell {
        tools.push(definition("run_command", "Run a trusted local shell command with project cwd and 60 second timeout. This is NOT sandboxed; use only to fulfill the user's coding task.", json!({"command":{"type":"string"}}), &["command"]));
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
        .ok_or_else(|| anyhow!("{field} must be a string"))
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
        bail!("Use a workspace-relative path without '..'");
    }
    let root = std::fs::canonicalize(root)?;
    let joined = root.join(requested);
    if joined.exists() || std::fs::symlink_metadata(&joined).is_ok() {
        let resolved = std::fs::canonicalize(&joined)?;
        if !resolved.starts_with(&root) {
            bail!("Path escapes the project root");
        }
        return Ok(resolved);
    }
    if !writing {
        bail!("Workspace path does not exist");
    }
    let parent = joined.parent().context("Missing parent directory")?;
    let parent = std::fs::canonicalize(parent).context("Parent directory must exist")?;
    if !parent.starts_with(&root) {
        bail!("Path escapes the project root");
    }
    Ok(parent.join(joined.file_name().context("Missing filename")?))
}

async fn tool(root: &Path, name: &str, arguments: Value, shell: bool) -> Result<Value> {
    match name {
        "read_file" => {
            let path = scoped_path(root, argument(&arguments, "path")?, false)?;
            let metadata = tokio::fs::metadata(&path).await?;
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                bail!("Read requires a regular file at most 256 KiB");
            }
            let bytes = tokio::fs::read(path).await?;
            if bytes.len() as u64 > MAX_FILE_BYTES {
                bail!("File grew beyond 256 KiB");
            }
            Ok(json!({"content":String::from_utf8(bytes).context("File is not UTF-8 text")?}))
        }
        "write_file" => {
            let path = scoped_path(root, argument(&arguments, "path")?, true)?;
            let content = argument(&arguments, "content")?;
            if content.len() as u64 > MAX_FILE_BYTES {
                bail!("Write exceeds 256 KiB limit");
            }
            if let Ok(metadata) = tokio::fs::metadata(&path).await {
                if !metadata.is_file() {
                    bail!("Write requires a regular file");
                }
            }
            tokio::fs::write(path, content).await?;
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
            tokio::task::spawn_blocking(move || walk(&root, &path, query.as_deref())).await?
        }
        "run_command" if shell => {
            let command = argument(&arguments, "command")?;
            if command.len() > 16_000 {
                bail!("Command is too long");
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
                .context("Could not start command")?;
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
                .context("Command exceeded its 60 second timeout")?
                .map_err(Into::into)
        }
        _ => bail!("Unknown or disabled tool: {name}"),
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
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let canonical = std::fs::canonicalize(&path)?;
        if !canonical.starts_with(root) {
            bail!("Path escapes project root");
        }
        if metadata.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(&path)?.filter_map(Result::ok).collect();
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
