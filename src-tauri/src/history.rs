//! Native owner-only history transport. No bearer or backend error body reaches
//! the renderer; enforce byte budgets while streaming, before JSON allocation.
use reqwest::Response;
use serde_json::Value;
const MIB: usize = 1024 * 1024;

fn safe(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

pub(super) fn limit(path: &str) -> Option<usize> {
    if path == "/api/v1/snapshot?window=1" {
        return Some(12 * MIB);
    }
    if path == "/api/v1/snapshot" {
        return Some(16 * MIB);
    }
    if path
        .strip_prefix("/api/v1/tasks/")
        .is_some_and(|id| safe(id, 128))
    {
        return Some(8 * MIB);
    }
    let (route, query) = path
        .split_once('?')
        .map_or((path, None), |(route, query)| (route, Some(query)));
    let message = route
        .strip_prefix("/api/v1/history/sessions/")
        .and_then(|value| value.strip_suffix("/messages"))
        .is_some_and(|id| safe(id, 128));
    let page = route.strip_prefix("/api/v1/history/").is_some_and(|kind| {
        matches!(
            kind,
            "sessions" | "tasks" | "plans" | "plan_changes" | "deliveries" | "execution_requests"
        )
    });
    if !message && !page {
        return None;
    }
    let mut session_seen = false;
    let mut cursor_seen = false;
    if let Some(query) = query {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            match key {
                "session_id" if page && !session_seen && safe(value, 128) => session_seen = true,
                "cursor" if !cursor_seen && safe(value, 2048) => cursor_seen = true,
                _ => return None,
            }
        }
    }
    Some(2 * MIB)
}

async fn status_error(mut response: Response) -> &'static str {
    let status = response.status().as_u16();
    if response.content_length().is_some_and(|size| size > 4096) {
        return "HISTORY_UNAVAILABLE";
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= 4096usize.saturating_sub(bytes.len()) => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return "HISTORY_UNAVAILABLE",
        }
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return "HISTORY_UNAVAILABLE";
    };
    match (status, value["code"].as_str()) {
        (409, Some("history_reset")) => "HISTORY_RESET",
        (413, Some("record_too_large")) => "HISTORY_RECORD_TOO_LARGE",
        (404, Some("not_found")) => "HISTORY_NOT_FOUND",
        _ => "HISTORY_UNAVAILABLE",
    }
}

pub(super) async fn response(
    mut response: Response,
    bounded: Option<usize>,
) -> Result<Value, String> {
    let status = response.status();
    if bounded.is_some() && !status.is_success() {
        return Err(status_error(response).await.into());
    }
    let limit = bounded.unwrap_or(16 * MIB);
    let too_large = if bounded.is_some() {
        "HISTORY_RECORD_TOO_LARGE"
    } else {
        "后台响应超过16 MiB限制"
    };
    let unavailable = if bounded.is_some() {
        "HISTORY_UNAVAILABLE"
    } else {
        "后台服务返回的数据格式无效"
    };
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large.into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(too_large.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable)?;
    if status.is_success() {
        Ok(value)
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("后台服务拒绝了此请求")
            .to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_read_routes_and_queries() {
        for kind in [
            "sessions",
            "tasks",
            "plans",
            "plan_changes",
            "deliveries",
            "execution_requests",
        ] {
            let path = format!("/api/v1/history/{kind}");
            assert_eq!(limit(&path), Some(2 * MIB));
            assert_eq!(
                limit(&format!("{path}?session_id=s_1&cursor=c-2")),
                Some(2 * MIB)
            );
        }
        assert_eq!(limit("/api/v1/tasks/task_1"), Some(8 * MIB));
        assert_eq!(limit("/api/v1/snapshot?window=1"), Some(12 * MIB));
        assert_eq!(limit("/api/v1/snapshot"), Some(16 * MIB));
        assert_eq!(limit("/api/v1/snapshot?window=1&window=1"), None);
        assert_eq!(limit("/api/v1/snapshot?window=2"), None);
        assert_eq!(
            limit("/api/v1/history/sessions/s_1/messages?cursor=c"),
            Some(2 * MIB)
        );
        for path in [
            "/api/v1/tasks/a%2fb",
            "/api/v1/tasks/a?cursor=b",
            "/api/v1/history/tasks?cursor=",
            "/api/v1/history/tasks?cursor=a&cursor=b",
            "/api/v1/history/tasks?session_id=a&session_id=b",
            "/api/v1/history/tasks?cursor=a%2fb",
            "/api/v1/history/tasks?x=a",
            "/api/v1/history/tasks?cursor=a&",
            "/api/v1/history/sessions/a/messages?session_id=a",
            "/api/v1/history/tasks/../sessions",
            "/api/v1/history/qq",
            "//remote/api/v1/tasks/a",
            "/api/v1/history/tasks#secret",
        ] {
            assert_eq!(limit(path), None, "{path}");
        }
        assert_eq!(limit(&format!("/api/v1/tasks/{}", "a".repeat(129))), None);
        assert_eq!(
            limit(&format!(
                "/api/v1/history/tasks?cursor={}",
                "a".repeat(2049)
            )),
            None
        );
    }

    #[tokio::test]
    async fn bounded_reads_reject_status_body_and_chunked_overflow_before_json() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body, cap, expected) in [
            (409, "private-token".to_string(), 16, "HISTORY_UNAVAILABLE"),
            (413, "private-token".to_string(), 16, "HISTORY_UNAVAILABLE"),
            (404, "private-token".to_string(), 16, "HISTORY_UNAVAILABLE"),
            (500, "private-token".to_string(), 16, "HISTORY_UNAVAILABLE"),
            (
                404,
                r#"{"code":"not_found","error":"private-token"}"#.to_string(),
                16,
                "HISTORY_NOT_FOUND",
            ),
            (
                409,
                r#"{"code":"history_reset"}"#.to_string(),
                16,
                "HISTORY_RESET",
            ),
            (
                413,
                r#"{"code":"record_too_large"}"#.to_string(),
                16,
                "HISTORY_RECORD_TOO_LARGE",
            ),
            (
                404,
                r#"{"error":"private-token"}"#.to_string(),
                16,
                "HISTORY_UNAVAILABLE",
            ),
            (404, String::new(), 16, "HISTORY_UNAVAILABLE"),
            (
                200,
                "invalid-private-token".to_string(),
                64,
                "HISTORY_UNAVAILABLE",
            ),
            (200, "x".repeat(65), 64, "HISTORY_RECORD_TOO_LARGE"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut b = [0];
                    socket.read_exact(&mut b).await.unwrap();
                    headers.push(b[0]);
                }
                let wire = format!("HTTP/1.1 {status} Fixture\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n", body.len());
                let _ = socket.write_all(wire.as_bytes()).await;
            });
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(url)
                .send()
                .await
                .unwrap();
            assert_eq!(
                super::response(response, Some(cap)).await.unwrap_err(),
                expected
            );
            server.await.unwrap();
        }
    }
}
