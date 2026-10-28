use anyhow::{bail, Result};
use harness_core::ExecutionEvent;
use serde_json::Value;

pub(super) struct TextEmitter {
    pending: String,
    secrets: Vec<String>,
    emitted: usize,
}

impl TextEmitter {
    pub fn new(known_secrets: &[String]) -> Self {
        let mut secrets: Vec<_> = ["HARNESS_API_KEY", "OPENAI_API_KEY"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
            .collect();
        secrets.extend(
            known_secrets
                .iter()
                .filter(|value| !value.is_empty())
                .cloned(),
        );
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        Self {
            pending: String::new(),
            secrets,
            emitted: 0,
        }
    }
    pub fn push(&mut self, text: &str, emit: &impl Fn(ExecutionEvent) -> Result<()>) -> Result<()> {
        self.pending.push_str(text);
        if self.pending.len() > 256 * 1024 {
            bail!("Codex 回复超过安全预算");
        }
        self.drain(false, emit)
    }
    pub fn finish(&mut self, emit: &impl Fn(ExecutionEvent) -> Result<()>) -> Result<()> {
        self.drain(true, emit)
    }
    fn drain(
        &mut self,
        finished: bool,
        emit: &impl Fn(ExecutionEvent) -> Result<()>,
    ) -> Result<()> {
        let mut safe = String::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            let remaining = &self.pending[offset..];
            if !finished
                && self
                    .secrets
                    .iter()
                    .any(|secret| secret.starts_with(remaining))
            {
                break;
            }
            if let Some(secret) = self
                .secrets
                .iter()
                .find(|secret| remaining.starts_with(secret.as_str()))
            {
                safe.push_str("[凭证已隐藏]");
                offset += secret.len();
            } else {
                let ch = remaining.chars().next().unwrap();
                safe.push(ch);
                offset += ch.len_utf8();
            }
        }
        self.pending.drain(..offset);
        let mut text = safe.as_str();
        while !text.is_empty() {
            let mut end = text.len().min(4096);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.emitted += end;
            if self.emitted > 256 * 1024 {
                bail!("Codex 累计回复超过 256 KiB 限制");
            }
            emit(ExecutionEvent::AssistantDelta {
                text: text[..end].into(),
            })?;
            text = &text[end..];
        }
        Ok(())
    }
}

pub(super) fn tool_identity(item: &Value, secrets: &[String]) -> Option<(String, String)> {
    let name = match item["type"].as_str()? {
        "commandExecution" => "command_execution".to_owned(),
        "fileChange" => "file_change".to_owned(),
        "mcpToolCall" => format!(
            "mcp:{}:{}",
            item["server"].as_str().unwrap_or("unknown"),
            item["tool"].as_str().unwrap_or("unknown")
        ),
        _ => return None,
    };
    let id = item["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)?;
    Some((redact(id, secrets), bounded(&name, 512, secrets)))
}

pub(super) fn tool_result(
    item: &Value,
    id: String,
    name: String,
    secrets: &[String],
) -> ExecutionEvent {
    let success = item["status"] == "completed"
        && (item["type"] != "commandExecution" || item["exitCode"] == 0);
    // File diffs and MCP payloads can contain credentials: only known output is exposed.
    let output = match item["type"].as_str() {
        Some("commandExecution") => format!(
            "命令：{}\n退出码：{}\n{}",
            item["command"].as_str().unwrap_or(""),
            item["exitCode"],
            item["aggregatedOutput"].as_str().unwrap_or("")
        ),
        Some("fileChange") => format!(
            "文件修改状态：{}；涉及 {} 项变更",
            item["status"].as_str().unwrap_or("unknown"),
            item["changes"].as_array().map_or(0, Vec::len)
        ),
        _ => format!(
            "工具调用状态：{}",
            item["status"].as_str().unwrap_or("unknown")
        ),
    };
    ExecutionEvent::ToolFinished {
        call_id: id,
        name,
        success,
        output: bounded(&output, 12 * 1024, secrets),
    }
}

pub(super) fn redact(text: &str, secrets: &[String]) -> String {
    let mut value = super::redact_evidence(text);
    for secret in secrets.iter().filter(|value| !value.is_empty()) {
        value = value.replace(secret, "[凭证已隐藏]");
    }
    value
}

fn bounded(text: &str, limit: usize, secrets: &[String]) -> String {
    let mut value = redact(text, secrets);
    if value.len() > limit {
        let marker = "\n[输出已截断]";
        let mut end = limit - marker.len();
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push_str(marker);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redaction_keeps_prefix_across_items_and_utf8_chunks_are_bounded() {
        let mut text = TextEmitter {
            pending: String::new(),
            secrets: vec!["secret-value".into()],
            emitted: 0,
        };
        let result = std::sync::Mutex::new(String::new());
        let emit = |event| {
            if let ExecutionEvent::AssistantDelta { text } = event {
                assert!(text.len() <= 4096);
                result.lock().unwrap().push_str(&text);
            }
            Ok(())
        };
        text.push("中文 secret-", &emit).unwrap();
        text.push("value 完成", &emit).unwrap();
        text.push(&"长消息".repeat(5000), &emit).unwrap();
        text.finish(&emit).unwrap();
        assert!(result.lock().unwrap().starts_with("中文 [凭证已隐藏] 完成"));
        assert!(!result.lock().unwrap().contains("secret-value"));
    }
}
