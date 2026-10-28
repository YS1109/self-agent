use super::*;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
#[derive(Default)]
struct Observed {
    requests: Vec<(String, Value)>,
    frames: Vec<Value>,
    sends: usize,
    auths: usize,
    gateways: usize,
}
struct Fixture {
    origin: String,
    observed: Arc<Mutex<Observed>>,
    worker: tokio::task::JoinHandle<()>,
    auth_waiting: Arc<tokio::sync::Notify>,
    auth_release: Arc<tokio::sync::Notify>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
impl Fixture {
    async fn start(mode: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let observed = Arc::new(Mutex::new(Observed::default()));
        let observations = observed.clone();
        let auth_waiting = Arc::new(tokio::sync::Notify::new());
        let auth_release = Arc::new(tokio::sync::Notify::new());
        let waiting = auth_waiting.clone();
        let release = auth_release.clone();
        let ws_url = origin.replacen("http:", "ws:", 1) + "websocket";
        let worker = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let observed = observations.clone();
                let auth_waiting = waiting.clone();
                let auth_release = release.clone();
                let ws_url = ws_url.clone();
                tokio::spawn(async move {
                    let mut peek = [0; 4096];
                    let count = socket.peek(&mut peek).await.unwrap();
                    if String::from_utf8_lossy(&peek[..count]).starts_with("GET /websocket ") {
                        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                        ws.send(Message::Text(
                            json!({"op":10,"d":{"heartbeat_interval":300}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                        let identity: Value = serde_json::from_str(
                            ws.next().await.unwrap().unwrap().to_text().unwrap(),
                        )
                        .unwrap();
                        observed.lock().unwrap().frames.push(identity.clone());
                        if mode == "invalid" {
                            ws.send(Message::Text(json!({"op":9}).to_string().into()))
                                .await
                                .unwrap();
                            return;
                        }
                        if mode == "oversize" {
                            let _ = ws.send(Message::Text("x".repeat(LIMIT + 1).into())).await;
                            return;
                        }
                        if mode == "reconnect" {
                            ws.send(Message::Text(json!({"op":7}).to_string().into()))
                                .await
                                .unwrap();
                            return;
                        }
                        let first = if identity["op"] == 6 && mode == "resume-replay" {
                            json!({"op":0,"t":"C2C_MESSAGE_CREATE","s":8,"d":{"id":"replayed-message"}})
                        } else if identity["op"] == 6 {
                            json!({"op":0,"t":"RESUMED","s":8,"d":{}})
                        } else {
                            json!({"op":0,"t":"READY","s":1,"d":{"session_id":"fixture-session"}})
                        };
                        if mode != "no-ready" {
                            ws.send(Message::Text(first.to_string().into()))
                                .await
                                .unwrap();
                        }
                        if mode == "resume-replay" {
                            ws.send(Message::Text(
                                json!({"op":0,"t":"RESUMED","s":9,"d":{}})
                                    .to_string()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                        }
                        if mode == "backlog" || mode == "overflow" {
                            for seq in 2..if mode == "overflow" { 20 } else { 4 } {
                                let _=ws.send(Message::Text(json!({"op":0,"s":seq,"t":"C2C_MESSAGE_CREATE","d":{"id":format!("message-{seq}")}}).to_string().into())).await;
                            }
                        }
                        while let Some(Ok(frame)) = ws.next().await {
                            if let Message::Text(text) = frame {
                                let value: Value = serde_json::from_str(&text).unwrap();
                                observed.lock().unwrap().frames.push(value.clone());
                                if value["op"] == 1 && mode != "missing-ack" {
                                    let _ = ws
                                        .send(Message::Text(json!({"op":11}).to_string().into()))
                                        .await;
                                }
                            }
                        }
                        return;
                    }
                    let mut bytes = Vec::new();
                    let end;
                    loop {
                        let mut buffer = [0; 4096];
                        let n = socket.read(&mut buffer).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(i) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                            end = i + 4;
                            break;
                        }
                        assert!(bytes.len() < 32768);
                    }
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_string();
                    let path = headers
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_owned();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while bytes.len() < end + length {
                        let mut buffer = [0; 4096];
                        let n = socket.read(&mut buffer).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    let body = serde_json::from_slice::<Value>(&bytes[end..end + length])
                        .unwrap_or(Value::Null);
                    let (status, body, extra) = {
                        let mut state = observed.lock().unwrap();
                        state.requests.push((path.clone(), body.clone()));
                        if path == "/app/getAppAccessToken" {
                            state.auths += 1;
                            assert_eq!(body["clientSecret"], "fixture-secret");
                            (200,json!({"access_token":format!("fixture-token-{}",state.auths),"expires_in":if mode=="short-expiry"{json!(1)}else if mode=="numeric-expiry"{json!(7200)}else{json!("7200")}}).to_string(),"")
                        } else if path == "/gateway" {
                            state.gateways += 1;
                            assert!(headers
                                .to_ascii_lowercase()
                                .contains("authorization: qqbot fixture-token-"));
                            if mode == "gateway401" && state.gateways == 1 {
                                (401, "{}".into(), "")
                            } else {
                                (200,json!({"url":if mode=="bad-origin" {"ws://127.0.0.2:1/websocket"}else{&ws_url}}).to_string(),"")
                            }
                        } else {
                            assert!(headers
                                .to_ascii_lowercase()
                                .contains("authorization: qqbot fixture-token-"));
                            state.sends += 1;
                            match mode {
                                "401-always" => (401, "{}".into(), ""),
                                "401" | "gate-refresh" if state.sends == 1 => {
                                    (401, "{}".into(), "")
                                }
                                "429" => (429, "{}".into(), "Retry-After: 7\r\n"),
                                "500" => (500, "{}".into(), ""),
                                "lost" => return,
                                "redirect" => {
                                    (307, "{}".into(), "Location: http://127.0.0.2:1/stolen\r\n")
                                }
                                "bad-success" => (200, "{}".into(), ""),
                                "large-body" => (200, "x".repeat(LIMIT + 1), ""),
                                _ => (200, json!({"id":"sent-fixture"}).to_string(), ""),
                            }
                        }
                    };
                    let gate_auth = path == "/app/getAppAccessToken"
                        && (mode == "gate-token"
                            || (mode == "gate-refresh" && observed.lock().unwrap().auths == 2));
                    if gate_auth {
                        auth_waiting.notify_one();
                        auth_release.notified().await;
                    }
                    let response=format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",body.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            origin,
            observed,
            worker,
            auth_waiting,
            auth_release,
        }
    }
    fn transport(&self) -> Transport {
        Transport::fixture(
            Credentials::new("12345".into(), Zeroizing::new(b"fixture-secret".to_vec())).unwrap(),
            &self.origin,
        )
        .unwrap()
    }
}
fn reply() -> Reply<'static> {
    Reply {
        kind: ReplyKind::User,
        openid: "user_1",
        msg_id: "ROBOT1.0_original_1",
        msg_seq: 1,
        content: "fixture reply",
        expires_at: std::time::SystemTime::now() + Duration::from_secs(60),
    }
}
#[tokio::test]
async fn explicit_auth_rejection_refreshes_once_and_preserves_passive_identity() {
    let fixture = Fixture::start("401").await;
    let mut transport = fixture.transport();
    assert_eq!(
        transport.send_passive(reply()).await.unwrap(),
        SendOutcome::Sent {
            id: "sent-fixture".into()
        }
    );
    let state = fixture.observed.lock().unwrap();
    assert_eq!((state.auths, state.sends), (2, 2));
    let sends: Vec<_> = state
        .requests
        .iter()
        .filter(|(path, _)| path.starts_with("/v2/users/"))
        .collect();
    assert_eq!(sends[0].1, sends[1].1);
    assert_eq!(sends[0].1["msg_seq"], 1);
}
#[tokio::test]
async fn delivery_classification_never_retries_unknown_or_redirects() {
    for (mode, expected) in [
        (
            "429",
            SendOutcome::Rejected {
                status: 429,
                retry_after_seconds: Some(7),
            },
        ),
        ("500", SendOutcome::Unknown),
        ("lost", SendOutcome::Unknown),
        ("bad-success", SendOutcome::Unknown),
        ("large-body", SendOutcome::Unknown),
        (
            "redirect",
            SendOutcome::Rejected {
                status: 307,
                retry_after_seconds: None,
            },
        ),
    ] {
        let fixture = Fixture::start(mode).await;
        assert_eq!(
            fixture.transport().send_passive(reply()).await.unwrap(),
            expected
        );
        assert_eq!(fixture.observed.lock().unwrap().sends, 1);
    }
}
#[tokio::test]
async fn validation_is_before_network_and_credentials_are_cached() {
    let fixture = Fixture::start("numeric-expiry").await;
    let mut transport = fixture.transport();
    let mut invalid = reply();
    invalid.openid = "../elsewhere";
    assert!(transport.send_passive(invalid).await.is_err());
    let mut expired = reply();
    expired.expires_at = std::time::SystemTime::UNIX_EPOCH;
    assert!(transport.send_passive(expired).await.is_err());
    assert!(fixture.observed.lock().unwrap().requests.is_empty());
    transport.send_passive(reply()).await.unwrap();
    transport.send_passive(reply()).await.unwrap();
    assert_eq!(fixture.observed.lock().unwrap().auths, 1);
    for origin in [
        "http://example.com/",
        "http://127.0.0.1/path",
        "https://127.0.0.1/",
        "http://user@127.0.0.1/",
    ] {
        assert!(Transport::fixture(
            Credentials::new("1".into(), Zeroizing::new(b"secret".to_vec())).unwrap(),
            origin
        )
        .is_err());
    }
}
#[tokio::test]
async fn durable_ack_controls_heartbeat_sequence_and_resume() {
    let fixture = Fixture::start("normal").await;
    let mut transport = fixture.transport();
    let mut gateway = transport.connect(None).await.unwrap();
    let GatewayItem::Dispatch { checkpoint, .. } = gateway.next().await.unwrap() else {
        panic!("expected READY")
    };
    wait_frame(&fixture, |v| v["op"] == 1 && v["d"].is_null()).await;
    assert!(fixture
        .observed
        .lock()
        .unwrap()
        .frames
        .iter()
        .any(|v| v["op"] == 1 && v["d"].is_null()));
    assert!(gateway.next().await.is_err());
    assert!(gateway
        .commit_ack(&Checkpoint {
            session_id: checkpoint.session_id.clone(),
            seq: 99
        })
        .is_err());
    gateway.commit_ack(&checkpoint).unwrap();
    wait_frame(&fixture, |v| v["op"] == 1 && v["d"] == 1).await;
    assert!(fixture
        .observed
        .lock()
        .unwrap()
        .frames
        .iter()
        .any(|v| v["op"] == 1 && v["d"] == 1));
    gateway.shutdown().await;
    let mut resumed = transport
        .connect(Some(Checkpoint {
            session_id: "fixture-session".into(),
            seq: 7,
        }))
        .await
        .unwrap();
    let GatewayItem::Dispatch { checkpoint, .. } = resumed.next().await.unwrap() else {
        panic!("expected RESUMED")
    };
    assert_eq!(checkpoint.seq, 8);
    assert!(fixture
        .observed
        .lock()
        .unwrap()
        .frames
        .iter()
        .any(|v| v["op"] == 6 && v["d"]["seq"] == 7));
    resumed.commit_ack(&checkpoint).unwrap();
    resumed.shutdown().await;
}
#[tokio::test]
async fn gateway_controls_bounds_and_missed_heartbeat_fail_closed() {
    for mode in ["invalid", "reconnect", "oversize", "missing-ack"] {
        let fixture = Fixture::start(mode).await;
        let mut gateway = fixture.transport().connect(None).await.unwrap();
        let event = timeout(Duration::from_secs(3), gateway.next())
            .await
            .unwrap()
            .unwrap();
        if mode == "missing-ack" {
            let GatewayItem::Dispatch { checkpoint, .. } = event else {
                panic!("expected READY")
            };
            gateway.commit_ack(&checkpoint).unwrap();
            assert!(matches!(
                timeout(Duration::from_secs(3), gateway.next())
                    .await
                    .unwrap()
                    .unwrap(),
                GatewayItem::Reconnect
            ));
        } else if mode == "invalid" {
            assert!(matches!(event, GatewayItem::InvalidSession));
        } else {
            assert!(matches!(event, GatewayItem::Reconnect));
        }
        gateway.shutdown().await;
    }
    let fixture = Fixture::start("bad-origin").await;
    assert!(fixture.transport().connect(None).await.is_err());
    assert!(fixture.observed.lock().unwrap().frames.is_empty());
}

async fn wait_frame(fixture: &Fixture, predicate: impl Fn(&Value) -> bool) {
    timeout(Duration::from_secs(3), async {
        loop {
            if fixture
                .observed
                .lock()
                .unwrap()
                .frames
                .iter()
                .any(&predicate)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn backlog_is_bounded_and_never_acknowledges_uncommitted_events() {
    let fixture = Fixture::start("backlog").await;
    let mut gateway = fixture.transport().connect(None).await.unwrap();
    for seq in 1..=3 {
        let GatewayItem::Dispatch { checkpoint, .. } = gateway.next().await.unwrap() else {
            panic!("expected dispatch");
        };
        assert_eq!(checkpoint.seq, seq);
        gateway.commit_ack(&checkpoint).unwrap();
    }
    wait_frame(&fixture, |value| value["op"] == 1 && value["d"] == 3).await;
    gateway.shutdown().await;
    let fixture = Fixture::start("overflow").await;
    let mut gateway = fixture.transport().connect(None).await.unwrap();
    let GatewayItem::Dispatch { checkpoint, .. } = gateway.next().await.unwrap() else {
        panic!("expected READY");
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(gateway.commit_ack(&checkpoint).is_err());
    assert!(matches!(
        gateway.next().await.unwrap(),
        GatewayItem::Reconnect
    ));
    assert!(!fixture
        .observed
        .lock()
        .unwrap()
        .frames
        .iter()
        .any(|value| value["op"] == 1 && value["d"].as_u64().is_some()));
    gateway.shutdown().await;
}
#[tokio::test]
async fn token_expiry_and_repeated_401_are_bounded() {
    let fixture = Fixture::start("numeric-expiry").await;
    let mut transport = fixture.transport();
    transport.send_passive(reply()).await.unwrap();
    transport.token.lock().await.as_mut().unwrap().refresh_at = Instant::now();
    let mut group = reply();
    group.kind = ReplyKind::Group;
    group.msg_seq = 5;
    transport.send_passive(group).await.unwrap();
    {
        let state = fixture.observed.lock().unwrap();
        assert_eq!(state.auths, 2);
        assert!(state
            .requests
            .iter()
            .any(|(path, _)| path == "/v2/groups/user_1/messages"));
    }
    let fixture = Fixture::start("401-always").await;
    assert_eq!(
        fixture.transport().send_passive(reply()).await.unwrap(),
        SendOutcome::Rejected {
            status: 401,
            retry_after_seconds: None
        }
    );
    let state = fixture.observed.lock().unwrap();
    assert_eq!((state.auths, state.sends), (2, 2));
}

#[tokio::test]
async fn gateway_401_refresh_and_production_origin_allowlist() {
    let fixture = Fixture::start("gateway401").await;
    let mut gateway = fixture.transport().connect(None).await.unwrap();
    let GatewayItem::Dispatch { checkpoint, .. } = gateway.next().await.unwrap() else {
        panic!("expected READY")
    };
    gateway.commit_ack(&checkpoint).unwrap();
    gateway.shutdown().await;
    {
        let state = fixture.observed.lock().unwrap();
        assert_eq!((state.auths, state.gateways), (2, 2));
    }
    let transport = Transport::production(
        Credentials::new("123".into(), Zeroizing::new(b"fixture".to_vec())).unwrap(),
    )
    .unwrap();
    assert!(transport
        .validate_gateway(&Url::parse("wss://api.bot.qq.com/websocket/").unwrap())
        .is_ok());
    for url in [
        "wss://api.bot.qq.com.evil.test/websocket",
        "ws://api.bot.qq.com/websocket",
        "wss://api.sgroup.qq.com/websocket",
        "wss://api.bot.qq.com:444/websocket",
        "wss://user@api.bot.qq.com/websocket",
        "wss://api.bot.qq.com/websocket?redirect=elsewhere",
    ] {
        assert!(transport
            .validate_gateway(&Url::parse(url).unwrap())
            .is_err());
    }
    assert_eq!(reconnect_delay(100), Duration::from_secs(64));
}

#[tokio::test]
async fn short_expiry_has_usable_half_lifetime_and_gateway_refreshes() {
    let fixture = Fixture::start("short-expiry").await;
    let mut transport = fixture.transport();
    let mut gateway = transport.connect(None).await.unwrap();
    let GatewayItem::Dispatch { checkpoint, .. } = gateway.next().await.unwrap() else {
        panic!("short-lived credential never became ready")
    };
    gateway.commit_ack(&checkpoint).unwrap();
    let remaining = transport
        .token
        .lock()
        .await
        .as_ref()
        .unwrap()
        .refresh_at
        .saturating_duration_since(Instant::now());
    assert!(remaining > Duration::ZERO && remaining <= Duration::from_millis(500));
    assert_eq!(fixture.observed.lock().unwrap().auths, 1);
    assert!(matches!(
        timeout(Duration::from_secs(2), gateway.next())
            .await
            .unwrap()
            .unwrap(),
        GatewayItem::Reconnect
    ));
    gateway.shutdown().await;
    transport.send_passive(reply()).await.unwrap();
    assert_eq!(fixture.observed.lock().unwrap().auths, 2);
}
#[tokio::test]
async fn heartbeat_ack_does_not_substitute_for_authenticated_ready() {
    let fixture = Fixture::start("no-ready").await;
    let mut gateway = fixture.transport().connect(None).await.unwrap();
    let started = Instant::now();
    assert!(matches!(
        timeout(Duration::from_secs(18), gateway.next())
            .await
            .unwrap()
            .unwrap(),
        GatewayItem::Reconnect
    ));
    assert!(started.elapsed() >= Duration::from_secs(14));
    assert!(fixture
        .observed
        .lock()
        .unwrap()
        .frames
        .iter()
        .any(|value| value["op"] == 1));
    gateway.shutdown().await;
}

#[tokio::test]
async fn revocation_during_token_and_401_refresh_prevents_message_post() {
    use std::sync::atomic::AtomicBool;
    for (mode, expected_sends, expected_auths) in [("gate-token", 0, 1), ("gate-refresh", 1, 2)] {
        let fixture = Fixture::start(mode).await;
        let mut transport = fixture.transport();
        let allowed = Arc::new(AtomicBool::new(true));
        let worker_allowed = allowed.clone();
        let send = tokio::spawn(async move {
            transport
                .send_passive_with_control(reply(), &|| async {
                    worker_allowed.load(Ordering::Acquire)
                })
                .await
        });
        timeout(Duration::from_secs(3), fixture.auth_waiting.notified())
            .await
            .unwrap();
        assert_eq!(fixture.observed.lock().unwrap().sends, expected_sends);
        allowed.store(false, Ordering::Release);
        fixture.auth_release.notify_one();
        let error = timeout(Duration::from_secs(3), send)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "QQ 回复发送权限已失效，未发送");
        let observed = fixture.observed.lock().unwrap();
        assert_eq!(
            (observed.sends, observed.auths),
            (expected_sends, expected_auths)
        );
        for (path, body) in &observed.requests {
            if path.starts_with("/v2/") {
                assert_eq!(body["msg_id"], "ROBOT1.0_original_1");
                assert_eq!(body["msg_seq"], 1);
            }
        }
    }
    let fixture = Fixture::start("normal").await;
    assert!(fixture
        .transport()
        .send_passive_with_control(reply(), &|| async { false })
        .await
        .is_err());
    assert!(fixture.observed.lock().unwrap().requests.is_empty());
}

#[tokio::test]
async fn resume_replays_events_before_resumed_and_commits_them_in_order() {
    let fixture = Fixture::start("resume-replay").await;
    let mut transport = fixture.transport();
    let mut gateway = transport
        .connect(Some(Checkpoint {
            session_id: "fixture-session".into(),
            seq: 7,
        }))
        .await
        .unwrap();
    let GatewayItem::Dispatch {
        payload,
        checkpoint,
    } = gateway.next().await.unwrap()
    else {
        panic!("expected replay before RESUMED")
    };
    assert_eq!(payload["t"], "C2C_MESSAGE_CREATE");
    assert_eq!(checkpoint.seq, 8);
    wait_frame(&fixture, |value| value["op"] == 1 && value["d"] == 7).await;
    assert!(!fixture
        .observed
        .lock()
        .unwrap()
        .frames
        .iter()
        .any(|value| value["op"] == 1 && value["d"].as_u64().is_some_and(|seq| seq > 7)));
    gateway.commit_ack(&checkpoint).unwrap();
    let GatewayItem::Dispatch {
        payload,
        checkpoint,
    } = gateway.next().await.unwrap()
    else {
        panic!("expected RESUMED after replay")
    };
    assert_eq!(payload["t"], "RESUMED");
    assert_eq!(checkpoint.seq, 9);
    wait_frame(&fixture, |value| value["op"] == 1 && value["d"] == 8).await;
    gateway.commit_ack(&checkpoint).unwrap();
    wait_frame(&fixture, |value| value["op"] == 1 && value["d"] == 9).await;
    gateway.shutdown().await;
}

#[tokio::test]
async fn generation_workers_share_one_serialized_access_token_cache() {
    let fixture = Fixture::start("numeric-expiry").await;
    let mut first = fixture.transport();
    let mut second = first.fork();
    let (a, b) = tokio::join!(first.send_passive(reply()), second.send_passive(reply()));
    assert!(matches!(a.unwrap(), SendOutcome::Sent { .. }));
    assert!(matches!(b.unwrap(), SendOutcome::Sent { .. }));
    assert_eq!(fixture.observed.lock().unwrap().auths, 1);
    first.token.lock().await.as_mut().unwrap().refresh_at = Instant::now();
    let (a, b) = tokio::join!(first.send_passive(reply()), second.send_passive(reply()));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(fixture.observed.lock().unwrap().auths, 2);
}
