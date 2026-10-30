//! Bounded, read-only history queries. A page is never automatically accumulated
//! and an expired cursor never causes a mutation or a silent restart at page one.
use super::*;
use std::fmt;

pub(super) const PAGE_LIMIT: usize = 2 * 1024 * 1024;
pub(super) const TASK_LIMIT: usize = 8 * 1024 * 1024;
pub(super) const SNAPSHOT_LIMIT: usize = 12 * 1024 * 1024;

pub(super) fn safe(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

pub(super) fn task_path(id: &str) -> Result<String> {
    if !safe(id, 128) {
        bail!("任务ID格式无效");
    }
    Ok(format!("/api/v1/tasks/{id}"))
}

pub(super) fn limit(path: &str) -> Option<usize> {
    if path == "/api/v1/snapshot?window=1" {
        Some(SNAPSHOT_LIMIT)
    } else if path.starts_with("/api/v1/history/") {
        Some(PAGE_LIMIT)
    } else if path
        .strip_prefix("/api/v1/tasks/")
        .is_some_and(|id| safe(id, 128))
    {
        Some(TASK_LIMIT)
    } else {
        None
    }
}

pub(super) fn collection_output(snapshot: &Value, kind: &str, items: Value) -> Value {
    let history = &snapshot["history"];
    let meta = if history["collections"][kind].is_object() {
        &history["collections"][kind]
    } else {
        &history[kind]
    };
    let incomplete = meta["has_more"] == true
        || meta["pending"].as_u64().unwrap_or(0) > 0
        || meta["unavailable"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        || (kind == "sessions"
            && history["session_messages"]
                .as_object()
                .is_some_and(|sessions| {
                    sessions.values().any(|meta| {
                        meta["has_more_before"] == true
                            || meta["unavailable"]
                                .as_array()
                                .is_some_and(|items| !items.is_empty())
                    })
                }));
    if incomplete {
        let mut result = json!({"history":history,"event_cursor":snapshot["event_cursor"],"notice":"仅载入窗口，未载入不表示不存在；请使用 history 只读分页检查完整记录"});
        result[kind] = items;
        result
    } else {
        items
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ReadError {
    Reset,
    TooLarge,
    NotFound,
    Unavailable,
}
impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Reset => "历史游标已失效；请清除 --cursor 后重新只读查询第一页（HISTORY_RESET）",
            Self::TooLarge => "历史记录或响应超过大小限制，未截断内容（HISTORY_RECORD_TOO_LARGE）",
            Self::NotFound => "历史记录不存在（HISTORY_NOT_FOUND）",
            Self::Unavailable => "历史读取未确认；可稍后只读重试（HISTORY_UNAVAILABLE）",
        })
    }
}
impl std::error::Error for ReadError {}
pub(super) async fn response_failure(mut response: reqwest::Response) -> ReadError {
    let status = response.status().as_u16();
    if response.content_length().is_some_and(|size| size > 4096) {
        return ReadError::Unavailable;
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= 4096usize.saturating_sub(bytes.len()) => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return ReadError::Unavailable,
        }
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return ReadError::Unavailable;
    };
    match (status, value["code"].as_str()) {
        (409, Some("history_reset")) => ReadError::Reset,
        (413, Some("record_too_large")) => ReadError::TooLarge,
        (404, Some("not_found")) => ReadError::NotFound,
        _ => ReadError::Unavailable,
    }
}
pub(super) fn permanent(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ReadError>(),
        Some(ReadError::Reset | ReadError::TooLarge | ReadError::NotFound)
    )
}

pub(super) fn route(options: &Options) -> Result<String> {
    if options.wait.is_some() || options.follow {
        bail!("history 为只读分页查询，不支持 --wait 或 --follow");
    }
    let args = &options.args;
    let kind = args
        .first()
        .map(String::as_str)
        .context("请指定 history 类别或 messages 会话ID")?;
    let (mut path, mut offset) = if kind == "messages" {
        let id = args.get(1).context("messages 缺少会话ID")?;
        if !safe(id, 128) {
            bail!("会话ID格式无效");
        }
        (format!("/api/v1/history/sessions/{id}/messages"), 2)
    } else if matches!(
        kind,
        "sessions" | "tasks" | "plans" | "plan_changes" | "deliveries" | "execution_requests"
    ) {
        (format!("/api/v1/history/{kind}"), 1)
    } else {
        bail!("未知历史类别");
    };
    let mut session = None;
    let mut cursor = None;
    while offset < args.len() {
        let key = &args[offset];
        let value = args.get(offset + 1).context("历史查询选项缺少参数")?;
        match key.as_str() {
            "--session" if kind != "messages" && session.is_none() && safe(value, 128) => {
                session = Some(value)
            }
            "--cursor" if cursor.is_none() && safe(value, 2048) => cursor = Some(value),
            _ => bail!("历史查询选项无效、重复或格式不安全"),
        }
        offset += 2;
    }
    if let Some(session) = session {
        path.push_str(&format!("?session_id={session}"));
    }
    if let Some(cursor) = cursor {
        path.push_str(if session.is_some() {
            "&cursor="
        } else {
            "?cursor="
        });
        path.push_str(cursor);
    }
    Ok(path)
}

pub(super) async fn detail(client: &Client, id: &str) -> Result<Value> {
    let value = client.request("GET", &task_path(id)?, None).await?;
    if value["task"]["id"] != id
        || !value["task"].is_object()
        || value["event_cursor"].as_u64().is_none()
    {
        return Err(ReadError::Unavailable.into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
