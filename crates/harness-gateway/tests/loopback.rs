use futures_util::{SinkExt, StreamExt};
use harness_gateway::{
    directory::{Device, Directory},
    protocol::*,
    Config, Gateway,
};
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
struct Fixture {
    dir: Directory,
    path: PathBuf,
    device: Device,
    token: String,
    origin: String,
    server: JoinHandle<()>,
    client: Client,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
    }
}
impl Fixture {
    async fn new() -> Self {
        let path = std::env::temp_dir().join(format!("gateway-{}.db", Uuid::new_v4()));
        let dir = Directory::open(&path).unwrap();
        let (device, token) = dir.enroll("夹具电脑").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let origin = format!("http://{addr}");
        let gateway = Gateway::new(
            Config {
                origin: origin.clone(),
                bind: addr,
                fixture: true,
            },
            dir.clone(),
        )
        .unwrap();
        let server =
            tokio::spawn(async move { axum::serve(listener, gateway.router()).await.unwrap() });
        Self {
            dir,
            path,
            device,
            token,
            origin,
            server,
            client: Client::new(),
        }
    }
    fn get(&self, path: &str, cookie: &str) -> RequestBuilder {
        self.client
            .get(format!("{}{path}", self.origin))
            .header("sec-fetch-site", "same-origin")
            .header("cookie", cookie)
    }
    fn post(&self, path: &str, cookie: &str, csrf: &str, value: Value) -> RequestBuilder {
        self.client
            .post(format!("{}{path}", self.origin))
            .header("sec-fetch-site", "same-origin")
            .header("origin", &self.origin)
            .header("cookie", cookie)
            .header("x-csrf-token", csrf)
            .json(&value)
    }
    async fn session(&self) -> (String, String) {
        let r = self.get("/gateway/api/session", "").send().await.unwrap();
        assert_eq!(r.headers()["cache-control"], "no-store");
        let cookie = r.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let v: Value = r.json().await.unwrap();
        (cookie, v["data"]["csrf"].as_str().unwrap().into())
    }
    async fn ws(&self) -> Ws {
        let mut req = format!(
            "{}/gateway/device/{}",
            self.origin.replace("http:", "ws:"),
            self.device.device_id
        )
        .into_client_request()
        .unwrap();
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws.send(Message::Text(
            serde_json::to_string(&GatewayDaemonFrame::Ready {
                protocol: 1,
                device_id: self.device.device_id.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap(); // Wait for server ping to prove ready is consumed.
        loop {
            if matches!(ws.next().await, Some(Ok(Message::Ping(_)))) {
                ws.flush().await.unwrap();
                break;
            }
        }
        ws
    }
    async fn pair(&self, ws: &mut Ws, cookie: &str, csrf: &str) -> String {
        let request = self.post(
            "/gateway/api/pair",
            cookie,
            csrf,
            json!({"device_id":self.device.device_id,"code":"fixture-code","label":"手机"}),
        );
        let job =
            tokio::spawn(
                async move { request.send().await.unwrap().json::<Value>().await.unwrap() },
            );
        let req = next_request(ws).await;
        assert_eq!(req.binding_revision, 0);
        assert!(matches!(req.rpc, GatewayRpc::PairClaim { .. }));
        let principal = req.principal_id.clone();
        reply(ws,&req,GatewayResult::ok(json!({"status":"active","principal_id":principal,"binding_revision":1,"expires_at_ms":4_000_000_000_000u64,"permissions":{},"session_ids":[]}))).await;
        assert_eq!(job.await.unwrap()["code"], "ok");
        principal
    }
}
async fn next_request(ws: &mut Ws) -> GatewayRequest {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Text(t) => {
                    let GatewayServerFrame::Request { request } = serde_json::from_str(&t).unwrap();
                    return request;
                }
                Message::Ping(_) => {
                    ws.flush().await.unwrap();
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    })
    .await
    .unwrap()
}
async fn reply(ws: &mut Ws, r: &GatewayRequest, result: GatewayResult) {
    ws.send(Message::Text(
        serde_json::to_string(&GatewayDaemonFrame::Reply {
            request_id: r.request_id.clone(),
            result,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn session_origin_csrf_and_strict_bodies() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let v: Value = f
        .client
        .get(format!("{}/gateway/api/session", f.origin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["code"], "forbidden");
    for req in [
        f.post("/gateway/api/logout", &cookie, "wrong", json!({})),
        f.post("/gateway/api/logout", &cookie, &csrf, json!({}))
            .header("origin", "https://attacker.invalid"),
    ] {
        assert_eq!(
            req.send().await.unwrap().json::<Value>().await.unwrap()["code"],
            "forbidden"
        )
    }
    let v=f.post("/gateway/api/pair",&cookie,&csrf,json!({"device_id":f.device.device_id,"code":"x","label":"手机","principal_id":Uuid::new_v4()})).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(v["code"], "invalid_request");
    assert_eq!(
        f.get("/gateway/api/devices", &cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["code"],
        "forbidden"
    );
    let refresh = f.get("/gateway/api/session", &cookie).send().await.unwrap();
    assert!(!refresh.headers().contains_key("set-cookie"));
    assert_eq!(refresh.json::<Value>().await.unwrap()["data"]["csrf"], csrf);
    let r = f
        .post("/gateway/api/logout", &cookie, &csrf, json!({}))
        .send()
        .await
        .unwrap();
    assert!(r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));
    let new = f.get("/gateway/api/session", &cookie).send().await.unwrap();
    assert!(new.headers().contains_key("set-cookie"));
    assert_eq!(new.json::<Value>().await.unwrap()["data"]["paired"], false);
}
#[tokio::test]
async fn pair_pins_principal_command_id_and_rejects_browser_overrides() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let mut ws = f.ws().await;
    let principal = f.pair(&mut ws, &cookie, &csrf).await;
    let command = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let request=f.post("/gateway/api/rpc",&cookie,&csrf,json!({"type":"command","command_id":command,"session_id":session,"command":{"type":"run","content":"fixture"}}));
    let job =
        tokio::spawn(async move { request.send().await.unwrap().json::<Value>().await.unwrap() });
    let req = next_request(&mut ws).await;
    assert_eq!(req.principal_id, principal);
    assert_eq!(req.binding_revision, 1);
    assert!(matches!(&req.rpc,GatewayRpc::Command{command_id,..} if command_id==&command));
    reply(
        &mut ws,
        &req,
        GatewayResult::ok(json!({"command_id":command})),
    )
    .await;
    assert_eq!(job.await.unwrap()["data"]["command_id"], command);
    for rpc in [
        json!({"type":"pair_status"}),
        json!({"type":"sessions","principal_id":Uuid::new_v4()}),
        json!({"type":"command","command_id":command,"session_id":session,"command":{"type":"approve","request_id":Uuid::new_v4()}}),
    ] {
        assert_eq!(
            f.post("/gateway/api/rpc", &cookie, &csrf, rpc)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["code"],
            "invalid_request"
        )
    }
    let directory = f
        .get("/gateway/api/devices", &cookie)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(directory["data"].as_array().unwrap().len(), 1);
    assert!(!directory.to_string().contains(&f.token));
    assert_eq!(
        f.post(
            "/gateway/api/pair",
            &cookie,
            &csrf,
            json!({"device_id":f.device.device_id,"code":"again","label":"手机"})
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["code"],
        "conflict"
    );
}
#[tokio::test]
async fn offline_and_ambiguous_pair_never_queue_or_retry() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    assert_eq!(
        f.post(
            "/gateway/api/pair",
            &cookie,
            &csrf,
            json!({"device_id":f.device.device_id,"code":"once","label":"手机"})
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["code"],
        "device_offline"
    );
    let mut ws = f.ws().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), next_request(&mut ws))
            .await
            .is_err()
    );
    let (cookie2, csrf2) = f.session().await;
    let req = f.post(
        "/gateway/api/pair",
        &cookie2,
        &csrf2,
        json!({"device_id":f.device.device_id,"code":"uncertain","label":"手机"}),
    );
    let job = tokio::spawn(async move { req.send().await.unwrap().json::<Value>().await.unwrap() });
    let sent = next_request(&mut ws).await;
    ws.close(None).await.unwrap();
    assert_eq!(job.await.unwrap()["code"], "outcome_unknown");
    let v = f
        .get("/gateway/api/session", &cookie2)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(v["data"]["principal_id"], sent.principal_id);
    assert_eq!(v["data"]["paired"], false);
    let mut ws = f.ws().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), next_request(&mut ws))
            .await
            .is_err()
    );
    let req = f.get("/gateway/api/pair-status", &cookie2);
    let job = tokio::spawn(async move { req.send().await.unwrap().json::<Value>().await.unwrap() });
    let status = next_request(&mut ws).await;
    assert!(matches!(status.rpc, GatewayRpc::PairStatus));
    assert_eq!(status.principal_id, sent.principal_id);
    reply(
        &mut ws,
        &status,
        GatewayResult::ok(json!({"status":"pending"})),
    )
    .await;
    assert_eq!(job.await.unwrap()["code"], "ok");
}
#[tokio::test]
async fn durable_revoke_and_restart_drop_browser_authority() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let mut ws = f.ws().await;
    f.pair(&mut ws, &cookie, &csrf).await;
    f.dir.revoke(&f.device.device_id).unwrap();
    assert_eq!(
        f.post(
            "/gateway/api/rpc",
            &cookie,
            &csrf,
            json!({"type":"sessions"})
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["code"],
        "forbidden"
    );
    assert!(Directory::open(&f.path)
        .unwrap()
        .verify(f.device.device_id.clone(), Some(f.token.clone()), None)
        .await
        .unwrap()
        .is_none());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let g = Gateway::new(
        Config {
            origin: format!("http://{addr}"),
            bind: addr,
            fixture: true,
        },
        f.dir.clone(),
    )
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, g.router()).await.unwrap() });
    let r = f
        .client
        .get(format!("http://{addr}/gateway/api/session"))
        .header("sec-fetch-site", "same-origin")
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert!(r.headers().contains_key("set-cookie"));
    assert_eq!(
        r.json::<Value>().await.unwrap()["data"]["principal_id"],
        Value::Null
    );
    server.abort();
}
#[tokio::test]
async fn device_auth_ready_and_replacement_are_separate_from_cookie() {
    let f = Fixture::new().await;
    let (cookie, _) = f.session().await;
    let url = format!(
        "{}/gateway/device/{}",
        f.origin.replace("http:", "ws:"),
        f.device.device_id
    );
    let mut req = url.clone().into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    let mut ws = f.ws().await;
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", f.token).parse().unwrap(),
    );
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    ws.send(Message::Text(
        serde_json::to_string(&GatewayDaemonFrame::Reply {
            request_id: Uuid::new_v4().to_string(),
            result: GatewayResult::ok(json!({"unrequested":true})),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .is_ok());
}
#[tokio::test]
async fn inflight_limit_and_disconnect_resolve_without_reexecution() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let mut ws = f.ws().await;
    f.pair(&mut ws, &cookie, &csrf).await;
    let mut jobs = vec![];
    for _ in 0..8 {
        let req = f.post(
            "/gateway/api/rpc",
            &cookie,
            &csrf,
            json!({"type":"sessions"}),
        );
        jobs.push(tokio::spawn(async move {
            req.send().await.unwrap().json::<Value>().await.unwrap()
        }));
        let _ = next_request(&mut ws).await;
    }
    assert_eq!(
        f.post(
            "/gateway/api/rpc",
            &cookie,
            &csrf,
            json!({"type":"sessions"})
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["code"],
        "busy"
    );
    ws.close(None).await.unwrap();
    for job in jobs {
        assert_eq!(job.await.unwrap()["code"], "outcome_unknown")
    }
    let mut ws = f.ws().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), next_request(&mut ws))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn principal_revoked_frame_invalidates_cookie() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let mut ws = f.ws().await;
    let principal = f.pair(&mut ws, &cookie, &csrf).await;
    ws.send(Message::Text(
        serde_json::to_string(&GatewayDaemonFrame::Revoked {
            principal_id: principal,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    // WebSocket ordering barrier: do not race a request before revoke was consumed.
    ws.send(Message::Ping(vec![1].into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(ws.next().await, Some(Ok(Message::Pong(_)))) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.post(
            "/gateway/api/rpc",
            &cookie,
            &csrf,
            json!({"type":"sessions"})
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["code"],
        "unauthorized"
    );
}
#[test]
fn origin_and_nginx_fail_closed_contract() {
    let bind = "127.0.0.1:8788".parse().unwrap();
    for origin in [
        "http://example.com",
        "http://localhost:123",
        "http://127.0.0.1/path",
        "https://127.0.0.1",
    ] {
        assert!(Config {
            origin: origin.into(),
            bind,
            fixture: true
        }
        .validate()
        .is_err())
    }
    assert!(Config {
        origin: "http://127.0.0.1:123".into(),
        bind,
        fixture: false
    }
    .validate()
    .is_err());
    assert!(Config {
        origin: "https://example.invalid".into(),
        bind,
        fixture: false
    }
    .validate()
    .is_ok());
    let nginx = include_str!("../../../deploy/nginx-gateway.conf.example");
    for directive in [
        "proxy_next_upstream off;",
        "proxy_buffering off;",
        "proxy_cache off;",
        "access_log off;",
        "proxy_read_timeout 60s;",
        "send_timeout 10s;",
    ] {
        assert!(nginx.contains(directive))
    }
}

#[tokio::test]
async fn sse_cursor_bounds_and_revoke_close_streams() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let mut ws = f.ws().await;
    let principal = f.pair(&mut ws, &cookie, &csrf).await;
    let session = Uuid::new_v4().to_string();
    let mut responses = vec![];
    for after in [7, 9] {
        let req = f
            .get(
                &format!("/gateway/api/events?session_id={session}&after=0"),
                &cookie,
            )
            .header("last-event-id", after.to_string());
        let job = tokio::spawn(async move { req.send().await.unwrap() });
        let r = next_request(&mut ws).await;
        assert!(matches!(r.rpc,GatewayRpc::Events{after:a,..} if a==after));
        reply(
            &mut ws,
            &r,
            GatewayResult::ok(json!({"events":[],"cursor":after+1})),
        )
        .await;
        let mut response = job.await.unwrap();
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        let chunk = response.chunk().await.unwrap().unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        assert!(text.contains("event: gateway"));
        assert!(text.contains(&format!("id: {}", after + 1)));
        responses.push(response);
    }
    assert_eq!(
        f.get(
            &format!("/gateway/api/events?session_id={session}&after=0"),
            &cookie
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["code"],
        "busy"
    );
    let bad = f
        .get(
            &format!("/gateway/api/events?session_id={session}&after=3"),
            &cookie,
        )
        .header("last-event-id", "2")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(bad["code"], "invalid_request");
    ws.send(Message::Text(
        serde_json::to_string(&GatewayDaemonFrame::Revoked {
            principal_id: principal,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    for mut response in responses {
        assert!(
            tokio::time::timeout(Duration::from_secs(3), response.chunk())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        )
    }
}
#[tokio::test]
async fn revoke_while_mutation_is_inflight_is_unknown_not_rejected() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let mut ws = f.ws().await;
    let principal = f.pair(&mut ws, &cookie, &csrf).await;
    let command = Uuid::new_v4().to_string();
    let req=f.post("/gateway/api/rpc",&cookie,&csrf,json!({"type":"command","command_id":command,"session_id":Uuid::new_v4(),"command":{"type":"run","content":"possibly committed"}}));
    let job = tokio::spawn(async move { req.send().await.unwrap().json::<Value>().await.unwrap() });
    let received = next_request(&mut ws).await;
    assert!(matches!(received.rpc, GatewayRpc::Command { .. }));
    ws.send(Message::Text(
        serde_json::to_string(&GatewayDaemonFrame::Revoked {
            principal_id: principal,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), job)
            .await
            .unwrap()
            .unwrap()["code"],
        "outcome_unknown"
    );
    // This reply belongs to the precisely known retired request. It must not
    // resurrect its cookie, but it must not kill unrelated device traffic.
    reply(&mut ws, &received, GatewayResult::ok(json!({"late":true}))).await;
    let (fresh_cookie, fresh_csrf) = f.session().await;
    let fresh_principal = f.pair(&mut ws, &fresh_cookie, &fresh_csrf).await;
    assert_ne!(fresh_principal, received.principal_id);
}
#[tokio::test]
async fn directory_stores_verifier_only_and_wrong_credentials_fail() {
    let f = Fixture::new().await;
    let bytes = std::fs::read(&f.path).unwrap();
    assert!(!bytes
        .windows(f.token.len())
        .any(|w| w == f.token.as_bytes()));
    assert!(f
        .dir
        .verify(f.device.device_id.clone(), Some("0".repeat(64)), None)
        .await
        .unwrap()
        .is_none());
    let rows = f.dir.list().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!serde_json::to_string(&rows).unwrap().contains(&f.token));
}

#[tokio::test]
async fn anonymous_admission_cap_does_not_displace_an_existing_active_session() {
    let f = Fixture::new().await;
    let (cookie, csrf) = f.session().await;
    let mut ws = f.ws().await;
    f.pair(&mut ws, &cookie, &csrf).await;
    for _ in 0..256 {
        f.session().await;
    }
    let rejected: Value = f
        .get("/gateway/api/session", "")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rejected["code"], "busy");
    let existing: Value = f
        .get("/gateway/api/session", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(existing["code"], "ok");
    assert_eq!(existing["data"]["paired"], true);
    assert_eq!(existing["data"]["csrf"], csrf);
}

#[cfg(unix)]
#[test]
fn directory_rejects_symlinks_and_does_not_recreate_missing_authority_file() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = std::env::temp_dir().join(format!("gateway-dir-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let target = root.join("target");
    std::fs::write(&target, b"sentinel").unwrap();
    let link = root.join("link");
    symlink(&target, &link).unwrap();
    assert!(Directory::open(&link).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"sentinel");
    let path = root.join("devices.db");
    let dir = Directory::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&path).unwrap();
    assert!(dir.list().is_err());
    assert!(!path.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn actual_local_cli_enrollment_list_and_revoke() {
    let path = std::env::temp_dir().join(format!("gateway-cli-{}.db", Uuid::new_v4()));
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_harness-gateway"))
            .arg("--db")
            .arg(&path)
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&["enroll", "本地模拟设备"]);
    assert!(out.status.success());
    let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = receipt["device"]["device_id"].as_str().unwrap();
    let token = receipt["transport_token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    let list = run(&["list"]);
    assert!(list.status.success());
    assert!(!String::from_utf8_lossy(&list.stdout).contains(token));
    assert!(run(&["revoke", id]).status.success());
    let list: Value = serde_json::from_slice(&run(&["list"]).stdout).unwrap();
    assert_eq!(list[0]["revoked"], true);
    assert!(!run(&[
        "serve",
        "--origin",
        "http://example.invalid",
        "--bind",
        "127.0.0.1:0"
    ])
    .status
    .success());
    let _ = std::fs::remove_file(path);
}
