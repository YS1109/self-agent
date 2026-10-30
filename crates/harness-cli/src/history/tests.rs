use super::*;

fn options(args: &[&str]) -> Options {
    parse(args.iter().map(|s| (*s).into())).unwrap().unwrap()
}

#[test]
fn one_page_queries_are_safe_and_explicit() {
    assert_eq!(
        route(&options(&[
            "history",
            "tasks",
            "--cursor",
            "c-1",
            "--session",
            "s_1"
        ]))
        .unwrap(),
        "/api/v1/history/tasks?session_id=s_1&cursor=c-1"
    );
    assert_eq!(
        route(&options(&["history", "messages", "s", "--cursor", "c"])).unwrap(),
        "/api/v1/history/sessions/s/messages?cursor=c"
    );
    for args in [
        vec!["history", "tasks", "--cursor", "a%2fb"],
        vec!["history", "tasks", "--cursor", "a", "--cursor", "b"],
        vec!["history", "tasks", "--session", "a", "--session", "a"],
        vec!["history", "messages", "a", "--session", "a"],
        vec!["history", "tasks", "--unknown", "a"],
        vec!["history", "vault"],
        vec!["--follow", "history", "tasks"],
        vec!["--wait", "1", "history", "tasks"],
    ] {
        assert!(route(&options(&args)).is_err(), "{args:?}");
    }
    for id in ["", "../settings", "a/b", "a%2fb", "a?x=1", "中文"] {
        assert!(task_path(id).is_err());
    }
    assert!(task_path(&"a".repeat(129)).is_err());
    assert_eq!(limit("/api/v1/tasks/t"), Some(TASK_LIMIT));
    assert_eq!(limit("/api/v1/tasks/t/changes"), None);
}

#[tokio::test]
async fn status_follow_wait_and_requests_use_task_detail_not_snapshot() {
    let body = json!({"task":{"id":"t","status":"completed","execution_cleanup":"pending"},"event_cursor":50001,"execution_requests":[]}).to_string();
    for args in [
        vec!["status", "t"],
        vec!["follow", "t"],
        vec!["requests", "t"],
    ] {
        crate::tests::cli_fixture(
            &args,
            vec![("GET", "/api/v1/tasks/t", None, Some(body.clone()))],
        )
        .await
        .unwrap();
    }
    crate::tests::cli_fixture(
        &["--wait", "2", "run", "s", "fixture"],
        vec![
            (
                "POST",
                "/api/v1/commands",
                Some(json!({"type":"run_agent","session_id":"s","content":"fixture"})),
                Some(json!({"task_id":"t"}).to_string()),
            ),
            ("GET", "/api/v1/tasks/t", None, Some(body)),
        ],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn history_returns_exactly_one_page_without_accumulation() {
    crate::tests::cli_fixture(&["history", "tasks", "--session", "s", "--cursor", "opaque"], vec![("GET", "/api/v1/history/tasks?session_id=s&cursor=opaque", None, Some(json!({"items":[],"total":5000,"returned":0,"has_more":true,"next_cursor":"next","unavailable":[{"id":"big","reason":"record_too_large"}]}).to_string()))]).await.unwrap();
}

// Real HTTP fixture: preserve status handling and the byte limit even without a
// Content-Length. Bodies deliberately are not JSON, so overflow must win over
// parse errors, and raw backend strings must never escape.
async fn read_fixture(path: &str, status: u16, size: usize, chunked: bool) -> Result<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let expected = path.to_owned();
    let server = tokio::spawn(async move {
        let (mut socket, _) = crate::tests::accept_application(&listener).await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            socket.read_exact(&mut b).await.unwrap();
            headers.push(b[0]);
        }
        let headers = String::from_utf8(headers).unwrap();
        assert!(headers.starts_with(&format!("GET {expected} HTTP/1.1")));
        let headers = format!(
            "HTTP/1.1 {status} Fixture\r\nConnection: close\r\n{}\r\n",
            if chunked {
                "Transfer-Encoding: chunked\r\n".into()
            } else {
                format!("Content-Length: {size}\r\n")
            }
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        let body = vec![b'x'; 16384.min(size)];
        let mut remaining = size;
        while remaining != 0 {
            let n = remaining.min(body.len());
            let result = if chunked {
                let wire = format!("{n:x}\r\n{}\r\n", String::from_utf8_lossy(&body[..n]));
                socket.write_all(wire.as_bytes()).await
            } else {
                socket.write_all(&body[..n]).await
            };
            if result.is_err() {
                break;
            }
            remaining -= n;
        }
        if chunked {
            let _ = socket.write_all(b"0\r\n\r\n").await;
        }
        drop(socket);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "read must not automatically retry"
        );
    });
    let client = Client {
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap(),
        base: loopback_url(&base).unwrap(),
        token: "fixture".into(),
        protocol: tokio::sync::OnceCell::new(),
    };
    let result = client.request("GET", path, None).await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn exact_read_error_statuses_never_echo_or_retry() {
    for (status, expected) in [
        (409, "HISTORY_UNAVAILABLE"),
        (413, "HISTORY_UNAVAILABLE"),
        (404, "HISTORY_UNAVAILABLE"),
        (500, "HISTORY_UNAVAILABLE"),
    ] {
        let error = read_fixture("/api/v1/history/tasks?cursor=expired", status, 500, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("xxxx"));
        assert!(!permanent(&error));
    }
}

#[tokio::test]
async fn streamed_task_page_and_snapshot_limits_are_enforced_before_json() {
    for (path, cap) in [
        ("/api/v1/tasks/t", TASK_LIMIT),
        ("/api/v1/history/tasks", PAGE_LIMIT),
        ("/api/v1/snapshot?window=1", SNAPSHOT_LIMIT),
    ] {
        for chunked in [false, true] {
            let error = read_fixture(path, 200, cap + 1, chunked).await.unwrap_err();
            assert!(
                error.to_string().contains("HISTORY_RECORD_TOO_LARGE"),
                "{path}: {error}"
            );
        }
    }
}

#[tokio::test]
async fn large_full_snapshot_is_never_read_for_status_or_follow() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // This loopback server advertises a >16 MiB full snapshot if asked. Only task detail is
    // permitted for both operations, and the authoritative task is off-window.
    for command in ["status", "follow"] {
        let directory = std::env::temp_dir().join(format!("cli-detail-{}", new_id()));
        std::fs::create_dir(&directory).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(directory.join("daemon.json"), json!({"base_url":format!("http://{}",listener.local_addr().unwrap()),"token":"fixture","pid":1}).to_string()).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = crate::tests::accept_application(&listener).await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut b = [0];
                socket.read_exact(&mut b).await.unwrap();
                bytes.push(b[0]);
            }
            let request = String::from_utf8(bytes).unwrap();
            if request.starts_with("GET /api/v1/snapshot ") {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 40620326\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                panic!("single-task read fetched the full 40.6 MB snapshot");
            }
            assert!(request.starts_with("GET /api/v1/tasks/off-window "));
            let body =
                json!({"task":{"id":"off-window","status":"completed"},"event_cursor":100000})
                    .to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let mut options = options(&[command, "off-window"]);
        options.directory = directory.clone();
        let result = run(options).await;
        server.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        result.unwrap();
    }
}

#[tokio::test]
async fn follow_recovery_not_found_or_oversize_is_terminal_and_never_reposts() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (conflict, status, code) in [
        (true, 404, "HISTORY_NOT_FOUND"),
        (false, 413, "HISTORY_RECORD_TOO_LARGE"),
    ] {
        let directory = std::env::temp_dir().join(format!("cli-history-recovery-{}", new_id()));
        std::fs::create_dir(&directory).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(directory.join("daemon.json"), json!({"base_url":format!("http://{}",listener.local_addr().unwrap()),"token":"fixture","pid":1}).to_string()).unwrap();
        let server = tokio::spawn(async move {
            for (path, status, body) in [
                (
                    "/api/v1/events?after=7",
                    if conflict { 409 } else { 200 },
                    if conflict {
                        "private-do-not-echo"
                    } else {
                        "event: resync\ndata: snapshot_required\n\n"
                    },
                ),
                (
                    "/api/v1/tasks/t",
                    status,
                    if status == 404 {
                        r#"{"code":"not_found","error":"private-do-not-echo"}"#
                    } else {
                        r#"{"code":"record_too_large","error":"private-do-not-echo"}"#
                    },
                ),
            ] {
                let (mut socket, _) = tokio::time::timeout(
                    Duration::from_secs(2),
                    crate::tests::accept_application(&listener),
                )
                .await
                .unwrap()
                .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut b = [0];
                    socket.read_exact(&mut b).await.unwrap();
                    bytes.push(b[0]);
                }
                assert!(String::from_utf8(bytes)
                    .unwrap()
                    .starts_with(&format!("GET {path} HTTP/1.1")));
                socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "permanent read failure was retried"
            );
        });
        let result = follow_task(&directory, "t", 7, 3, &mut Vec::new())
            .await
            .unwrap_err();
        server.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(result.to_string().contains(code), "{result}");
        assert!(!result.to_string().contains("private"));
    }
}

#[tokio::test]
async fn initial_follow_detail_obeys_total_wait_deadline() {
    use tokio::io::AsyncReadExt;
    let directory = std::env::temp_dir().join(format!("cli-history-deadline-{}", new_id()));
    std::fs::create_dir(&directory).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(directory.join("daemon.json"),json!({"base_url":format!("http://{}",listener.local_addr().unwrap()),"token":"fixture","pid":1}).to_string()).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = crate::tests::accept_application(&listener).await.unwrap();
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut options = options(&["--wait", "1", "follow", "t"]);
    options.directory = directory.clone();
    let start = tokio::time::Instant::now();
    let result = run(options).await.unwrap_err();
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(directory).unwrap();
    assert!(result.to_string().contains("跟随超时"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn partial_sessions_and_empty_pending_requests_keep_authoritative_metadata() {
    let snapshot = json!({"event_cursor":7,"sessions":[{"id":"s"}],"history":{"collections":{"sessions":{"total":81,"returned":1,"has_more":true}}}});
    let output = collection_output(&snapshot, "sessions", snapshot["sessions"].clone());
    assert_eq!(output["history"]["collections"]["sessions"]["total"], 81);
    assert_eq!(output["sessions"].as_array().unwrap().len(), 1);
    let detail = json!({"event_cursor":8,"execution_requests":[],"history":{"execution_requests":{"total":1,"returned":0,"has_more":true,"pending":1}}});
    let output = collection_output(&detail, "execution_requests", json!([]));
    assert_eq!(output["execution_requests"], json!([]));
    assert_eq!(output["history"]["execution_requests"]["pending"], 1);
    let snapshot = json!({"sessions":[{"id":"s","messages":[]}],"history":{"collections":{"sessions":{"total":1,"returned":1,"has_more":false}},"session_messages":{"s":{"total":100,"returned":0,"has_more_before":true}}}});
    assert!(collection_output(&snapshot, "sessions", snapshot["sessions"].clone()).is_object());
    assert_eq!(
        collection_output(
            &json!({"history":{"execution_requests":{"total":0,"returned":0,"has_more":false,"pending":0}}}),
            "execution_requests",
            json!([])
        ),
        json!([])
    );
}

#[tokio::test]
async fn authoritative_error_codes_are_required_not_old_route_status_alone() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (status, body, expected) in [
        (404, "", "HISTORY_UNAVAILABLE"),
        (404, "<html>private</html>", "HISTORY_UNAVAILABLE"),
        (404, r#"{"error":"private"}"#, "HISTORY_UNAVAILABLE"),
        (
            404,
            r#"{"code":"not_found","error":"private"}"#,
            "HISTORY_NOT_FOUND",
        ),
        (409, r#"{"code":"history_reset"}"#, "HISTORY_RESET"),
        (
            413,
            r#"{"code":"record_too_large"}"#,
            "HISTORY_RECORD_TOO_LARGE",
        ),
        (404, r#"{"code":"history_reset"}"#, "HISTORY_UNAVAILABLE"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = crate::tests::accept_application(&listener).await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                bytes.push(byte[0]);
            }
            assert!(String::from_utf8(bytes)
                .unwrap()
                .starts_with("GET /api/v1/tasks/task HTTP/1.1"));
            socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let client = Client {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
            base: loopback_url(&base).unwrap(),
            token: "fixture".into(),
            protocol: tokio::sync::OnceCell::new(),
        };
        let error = client
            .request("GET", "/api/v1/tasks/task", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("private"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wait_deadline_preserves_last_observation_or_original_id_without_new_command() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for initial_observation in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = crate::tests::accept_application(&listener).await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                bytes.push(byte[0]);
            }
            assert!(String::from_utf8(bytes)
                .unwrap()
                .starts_with("GET /api/v1/tasks/task HTTP/1.1"));
            if initial_observation {
                let body = r#"{"task":{"id":"task","status":"running"},"event_cursor":1}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            } else {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let client = Client {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(40))
                .build()
                .unwrap(),
            base: loopback_url(&base).unwrap(),
            token: "fixture".into(),
            protocol: tokio::sync::OnceCell::new(),
        };
        let mut output = Vec::new();
        let started = std::time::Instant::now();
        assert!(super::super::wait_for_task(
            &client,
            "task",
            Duration::from_millis(100),
            &mut output
        )
        .await
        .is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        let value: Value = serde_json::from_slice(&output).unwrap();
        if initial_observation {
            assert_eq!(value["id"], "task");
            assert_eq!(value["status"], "running");
        } else {
            assert_eq!(value["task_id"], "task");
            assert_eq!(value["observation"], "timeout");
            assert!(value.get("status").is_none());
        }
        server.await.unwrap();
    }
}
