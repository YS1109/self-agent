//! Bounded Chat Completions SSE decoding. No tool is exposed before a complete turn.
use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const MAX_FRAME: usize = 512 * 1024;
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_TEXT: usize = 128 * 1024;

#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: String,
    total: usize,
    content: String,
    calls: BTreeMap<usize, Call>,
    finish: Option<String>,
    done: bool,
    error_event: bool,
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

pub(super) enum Delta {
    Text(String),
    Usage {
        input: Option<u64>,
        output: Option<u64>,
    },
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Delta>> {
        self.total += bytes.len();
        if self.total > MAX_RESPONSE {
            bail!("模型服务流式响应超过 2 MiB 限制");
        }
        let mut deltas = Vec::new();
        for byte in bytes {
            if *byte != b'\n' {
                self.line.push(*byte);
                if self.line.len() + self.data.len() > MAX_FRAME {
                    bail!("模型服务流式事件超过 512 KiB 限制");
                }
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            let line = String::from_utf8(std::mem::take(&mut self.line))
                .context("模型服务流式响应包含无效 UTF-8")?;
            if line.is_empty() {
                self.dispatch(&mut deltas)?;
            } else if let Some(value) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            } else if let Some(value) = line.strip_prefix("event:") {
                self.error_event = value.trim() == "error";
            }
        }
        Ok(deltas)
    }

    fn dispatch(&mut self, deltas: &mut Vec<Delta>) -> Result<()> {
        if self.error_event {
            bail!("模型服务在流式执行中返回错误，未重试请求");
        }
        self.error_event = false;
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data);
        if self.done {
            bail!("模型服务在流式终止标记后继续发送数据");
        }
        if data.trim() == "[DONE]" {
            if self.finish.is_none() {
                bail!("模型服务缺少流式回合完成状态");
            }
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(&data).context("模型服务返回了无效流式 JSON")?;
        if value.get("error").is_some() {
            bail!("模型服务在流式执行中返回错误，未重试请求");
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            deltas.push(Delta::Usage {
                input: usage["prompt_tokens"].as_u64(),
                output: usage["completion_tokens"].as_u64(),
            });
        }
        let choices = value["choices"]
            .as_array()
            .context("模型流式响应缺少 choices")?;
        for choice in choices {
            if choice["index"].as_u64().unwrap_or(0) != 0 || choices.len() > 1 {
                bail!("模型流式响应必须只包含一个候选回复");
            }
            let delta = &choice["delta"];
            if self.finish.is_some()
                && (delta.get("content").is_some_and(|value| !value.is_null())
                    || delta.get("tool_calls").is_some())
            {
                bail!("模型服务在回合完成后继续发送增量");
            }
            if let Some(text) = delta.get("content").filter(|value| !value.is_null()) {
                let text = text.as_str().context("模型回复增量必须为文本")?;
                if self.content.len() + text.len() > MAX_TEXT {
                    bail!("模型回复超过 128 KiB 限制");
                }
                self.content.push_str(text);
                if !text.is_empty() {
                    deltas.push(Delta::Text(text.into()));
                }
            }
            if let Some(calls) = delta.get("tool_calls") {
                for call in calls.as_array().context("工具调用增量必须为数组")? {
                    let index = call["index"].as_u64().context("工具调用增量缺少序号")?;
                    if index >= 16 {
                        bail!("模型服务超过了每轮 16 次工具调用限制");
                    }
                    if call.get("type").is_some_and(|kind| kind != "function") {
                        bail!("模型服务返回了不支持的工具调用类型");
                    }
                    let target = self.calls.entry(index as usize).or_default();
                    append(&mut target.id, call.get("id"), 256)?;
                    append(&mut target.name, call.pointer("/function/name"), 128)?;
                    append(
                        &mut target.arguments,
                        call.pointer("/function/arguments"),
                        MAX_FRAME,
                    )?;
                }
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .filter(|reason| !reason.is_null())
            {
                let reason = reason.as_str().context("模型回合完成状态无效")?;
                if !matches!(reason, "stop" | "tool_calls") {
                    bail!("模型回合未完整结束（输出预算或服务限制），未执行残缺工具调用");
                }
                if self.finish.is_some() {
                    bail!("模型服务重复发送回合完成状态");
                }
                self.finish = Some(reason.into());
            }
        }
        Ok(())
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn finish(self) -> Result<Value> {
        if !self.done || !self.line.is_empty() || !self.data.is_empty() {
            bail!("模型流式连接提前结束，未执行残缺工具调用");
        }
        let calls: Vec<_> = self.calls.into_values().map(|call| {
            json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}})
        }).collect();
        if (!calls.is_empty() && self.finish.as_deref() != Some("tool_calls"))
            || (calls.is_empty() && self.finish.as_deref() == Some("tool_calls"))
        {
            bail!("模型回合完成状态与工具调用不一致");
        }
        let message = json!({"role":"assistant","content":self.content,"tool_calls":calls});
        validate_calls(&message)?;
        Ok(message)
    }
}

/// A server may return JSON to stream:true, but it must still prove a complete turn.
pub(super) fn complete_json_message(response: &Value) -> Result<Value> {
    if response.get("error").is_some() {
        bail!("模型服务返回错误，未重试请求");
    }
    let choices = response["choices"]
        .as_array()
        .context("模型服务响应缺少 choices")?;
    if choices.len() != 1 {
        bail!("模型服务响应必须只包含一个完整候选回复");
    }
    let choice = &choices[0];
    let reason = choice["finish_reason"]
        .as_str()
        .context("模型服务缺少明确回合完成状态，未执行工具调用")?;
    let message = choice
        .get("message")
        .filter(|message| message.is_object())
        .context("模型服务响应缺少助手回复")?;
    validate_calls(message)?;
    let has_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    if (has_calls && reason != "tool_calls") || (!has_calls && reason != "stop") {
        bail!("模型回合完成状态与回复不一致，未执行工具调用");
    }
    Ok(message.clone())
}

fn append(target: &mut String, value: Option<&Value>, max: usize) -> Result<()> {
    if let Some(value) = value.filter(|value| !value.is_null()) {
        let text = value.as_str().context("工具调用增量字段必须为文本")?;
        if target.len() + text.len() > max {
            bail!("模型工具调用超过字段长度限制");
        }
        target.push_str(text);
    }
    Ok(())
}

/// Validate the whole batch before any tool can have a side effect.
pub(super) fn validate_calls(message: &Value) -> Result<()> {
    let Some(calls) = message.get("tool_calls").filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let calls = calls.as_array().context("模型工具调用必须为数组")?;
    if calls.len() > 16 {
        bail!("模型服务超过了每轮 16 次工具调用限制");
    }
    let mut ids = std::collections::HashSet::new();
    for call in calls {
        let id = call["id"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .context("工具调用缺少有效 id")?;
        if !ids.insert(id) {
            bail!("模型工具调用包含重复 id");
        }
        call.pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .context("工具调用缺少有效名称")?;
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .context("工具调用参数必须为 JSON 文本")?;
        let parsed: Value =
            serde_json::from_str(arguments).context("工具调用参数不完整，未执行本轮工具")?;
        if !parsed.is_object() {
            bail!("工具调用参数必须为对象，未执行本轮工具");
        }
    }
    Ok(())
}

/// Hold only a suffix which could become a credential on the next chunk.
pub(super) struct Redactor {
    pending: String,
    secrets: std::sync::Arc<Vec<String>>,
}

impl Redactor {
    pub fn new(secrets: Vec<String>) -> Self {
        Self::with_patterns(Self::compile_patterns(secrets))
    }

    pub fn with_patterns(secrets: std::sync::Arc<Vec<String>>) -> Self {
        Self {
            pending: String::new(),
            secrets,
        }
    }

    pub fn compile_patterns(mut secrets: Vec<String>) -> std::sync::Arc<Vec<String>> {
        // Common JSON string representations must also be held across stream
        // boundaries before any presentation truncation. Full JSON text gets
        // additional bounded semantic parsing at the value/output boundary.
        let original = secrets.clone();
        for secret in original.iter().filter(|value| !value.is_empty()) {
            let mut encoded = secret.clone();
            for _ in 0..2 {
                let json = serde_json::to_string(&encoded).expect("string serializes");
                encoded = json[1..json.len() - 1].to_owned();
                secrets.push(encoded.clone());
            }
            let mut ascii = String::new();
            for ch in secret.chars() {
                if ch.is_ascii() {
                    let json = serde_json::to_string(&ch.to_string()).expect("string serializes");
                    ascii.push_str(&json[1..json.len() - 1]);
                } else {
                    for unit in ch.encode_utf16(&mut [0; 2]) {
                        ascii.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
            secrets.push(ascii.clone());
            let json = serde_json::to_string(&ascii).expect("string serializes");
            secrets.push(json[1..json.len() - 1].to_owned());
        }
        secrets.retain(|secret| !secret.is_empty());
        secrets.sort();
        secrets.dedup();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        std::sync::Arc::new(secrets)
    }

    pub fn push(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        self.drain(false)
    }

    pub fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn drain(&mut self, finished: bool) -> String {
        let mut output = String::new();
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
                output.push_str("[凭证已隐藏]");
                offset += secret.len();
            } else {
                let ch = remaining.chars().next().unwrap();
                output.push(ch);
                offset += ch.len_utf8();
            }
        }
        self.pending.drain(..offset);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: Value) -> String {
        format!("data: {value}\r\n\r\n")
    }
    fn chunk(delta: Value, finish: Value) -> String {
        frame(json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}))
    }

    #[test]
    fn json_encoded_credentials_are_held_across_every_stream_boundary() {
        let key = "quote\"私有\\token\n😀";
        let encoded = serde_json::to_string(key).unwrap();
        let ascii = r#"quote\"\u79c1\u6709\\token\n\ud83d\ude00"#;
        for value in [encoded[1..encoded.len() - 1].to_owned(), ascii.into()] {
            let input = format!("before {value} after");
            for split in input.char_indices().map(|(index, _)| index) {
                let mut redactor = Redactor::new(vec![key.into()]);
                let output = redactor.push(&input[..split])
                    + &redactor.push(&input[split..])
                    + &redactor.finish();
                assert_eq!(output, "before [凭证已隐藏] after", "split {split}");
            }
        }
    }

    #[test]
    fn chinese_byte_splits_comments_crlf_and_usage_are_preserved() {
        let input = format!(
            ": 心跳\r\n\r\n{}{}{}data: [DONE]\r\n\r\n",
            chunk(json!({"content":"中文流式"}), Value::Null),
            chunk(json!({}), json!("stop")),
            frame(json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":4}}))
        );
        let mut decoder = Decoder::default();
        let mut text = String::new();
        let mut usage = false;
        for byte in input.as_bytes() {
            for delta in decoder.push(&[*byte]).unwrap() {
                match delta {
                    Delta::Text(value) => text.push_str(&value),
                    Delta::Usage { input, output } => {
                        usage = input == Some(12) && output == Some(4)
                    }
                }
            }
        }
        assert_eq!(text, "中文流式");
        assert!(usage);
        assert_eq!(decoder.finish().unwrap()["content"], text);
    }

    #[test]
    fn tool_fragments_are_only_available_after_done_and_validated_as_a_batch() {
        let input = format!(
            "{}{}{}data: [DONE]\n\n",
            chunk(
                json!({"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"write_","arguments":"{\"path\":"}}]}),
                Value::Null
            ),
            chunk(
                json!({"tool_calls":[{"index":0,"function":{"name":"file","arguments":"\"新.txt\",\"content\":\"内容\"}"}}]}),
                Value::Null
            ),
            chunk(json!({}), json!("tool_calls"))
        );
        let mut decoder = Decoder::default();
        for bytes in input.as_bytes().chunks(7) {
            decoder.push(bytes).unwrap();
        }
        let message = decoder.finish().unwrap();
        assert_eq!(message["tool_calls"][0]["function"]["name"], "write_file");
        assert!(message["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("新.txt"));
        let mut decoder = Decoder::default();
        decoder
            .push(chunk(json!({"content":"未完成"}), Value::Null).as_bytes())
            .unwrap();
        assert!(decoder.finish().is_err());
        let invalid = json!({"tool_calls":[{"id":"one","function":{"name":"write_file","arguments":"{}"}},
            {"id":"two","function":{"name":"write_file","arguments":"{"}}]});
        assert!(validate_calls(&invalid).is_err());
    }

    #[test]
    fn error_length_and_oversized_frames_are_rejected() {
        for input in [
            "event: error\ndata: {}\n\n".into(),
            frame(json!({"error":{"message":"secret upstream detail"}})),
            chunk(json!({}), json!("length")),
            "data: [DONE]\n\n".into(),
        ] {
            assert!(Decoder::default().push(input.as_bytes()).is_err());
        }
        assert!(Decoder::default().push(&vec![b'x'; MAX_FRAME + 1]).is_err());
    }

    #[test]
    fn redacts_credentials_at_every_split_without_losing_unicode() {
        let source = "中文 alpha-secret 尾部 Bearer alpha-secret 终点";
        for split in source
            .char_indices()
            .map(|(index, _)| index)
            .chain([source.len()])
        {
            let mut redactor =
                Redactor::new(vec!["alpha-secret".into(), "Bearer alpha-secret".into()]);
            let output = redactor.push(&source[..split])
                + &redactor.push(&source[split..])
                + &redactor.finish();
            assert_eq!(output, "中文 [凭证已隐藏] 尾部 [凭证已隐藏] 终点");
        }
        let mut redactor = Redactor::new(vec!["secret".into()]);
        assert_eq!(redactor.push("se"), "");
        assert_eq!(redactor.push("parate"), "separate");
        assert_eq!(redactor.finish(), "");
    }
}
