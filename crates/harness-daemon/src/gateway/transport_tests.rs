use super::*;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Condvar, Mutex,
};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_hdr_async;

const DEVICE: &str = "11111111-1111-4111-8111-111111111111";
const PRINCIPAL: &str = "22222222-2222-4222-8222-222222222222";
const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Fixture {
    current: AtomicBool,
    calls: AtomicUsize,
    entered: AtomicBool,
    gate: (Mutex<bool>, Condvar),
    revoked: AtomicBool,
    oversized: AtomicBool,
}
impl Fixture {
    fn new(blocked: bool) -> Arc<Self> {
        Arc::new(Self {
            current: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            entered: AtomicBool::new(false),
            gate: (Mutex::new(!blocked), Condvar::new()),
            revoked: AtomicBool::new(false),
            oversized: AtomicBool::new(false),
        })
    }
    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}
impl Backend for Fixture {
    fn current(&self) -> bool {
        self.current.load(Ordering::SeqCst)
    }
    fn handle(&self, request: GatewayRequest) -> GatewayResult {
        self.entered.store(true, Ordering::SeqCst);
        let mut open = self.gate.0.lock().unwrap();
        while !*open {
            open = self.gate.1.wait(open).unwrap();
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.oversized.load(Ordering::SeqCst) {
            return GatewayResult::ok(json!({"text":"\u{0001}".repeat(64*1024)}));
        }
        GatewayResult::ok(json!({"principal_id":request.principal_id,"rpc":request.rpc}))
    }
    fn revoked(&self, peers: &[(String, u64)]) -> Result<Vec<String>> {
        Ok(if self.revoked.load(Ordering::SeqCst) {
            peers.iter().map(|p| p.0.clone()).collect()
        } else {
            vec![]
        })
    }
    fn connection(&self, _: bool) {}
}

fn request() -> GatewayServerFrame {
    GatewayServerFrame::Request {
        request: GatewayRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            principal_id: PRINCIPAL.into(),
            binding_revision: 1,
            rpc: super::super::wire::GatewayRpc::Sessions,
        },
    }
}
async fn connected() -> (
    Transport,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio::task::JoinHandle<()>,
    Arc<Fixture>,
    watch::Sender<bool>,
) {
    setup(false).await
}
#[allow(clippy::result_large_err)] // tungstenite's handshake callback fixes this response type.
async fn setup(
    blocked: bool,
) -> (
    Transport,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio::task::JoinHandle<()>,
    Arc<Fixture>,
    watch::Sender<bool>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let transport =
        Transport::new(&origin, DEVICE.into(), Zeroizing::new(TOKEN.into()), true).unwrap();
    let runner =
        Transport::new(&origin, DEVICE.into(), Zeroizing::new(TOKEN.into()), true).unwrap();
    let backend = Fixture::new(blocked);
    let b = backend.clone();
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(async move { run(runner, b, rx).await });
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_hdr_async(
        stream,
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request, reply| {
            assert_eq!(req.uri().path(), format!("/gateway/device/{DEVICE}"));
            assert_eq!(req.headers()["authorization"], format!("Bearer {TOKEN}"));
            Ok(reply)
        },
    )
    .await
    .unwrap();
    let raw = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(matches!(
        serde_json::from_str::<GatewayDaemonFrame>(&raw).unwrap(),
        GatewayDaemonFrame::Ready { protocol: 1, .. }
    ));
    (transport, socket, task, backend, stop)
}
async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> GatewayDaemonFrame {
    timeout(Duration::from_secs(3), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => socket.send(Message::Pong(data)).await.unwrap(),
                _ => {}
            }
        }
    })
    .await
    .unwrap()
}
async fn stop_link(
    mut socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    task: tokio::task::JoinHandle<()>,
    stop: watch::Sender<bool>,
) {
    let _ = stop.send(true);
    let _ = socket.close(None).await;
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn endpoint_and_credential_validation_never_downgrades_or_accepts_proxy_paths() {
    assert_eq!(
        endpoint("https://gateway.example", DEVICE, false)
            .unwrap()
            .scheme(),
        "wss"
    );
    for origin in [
        "http://gateway.example",
        "https://u:p@gateway.example",
        "https://gateway.example/path",
        "https://gateway.example?secret=x",
        "https://gateway.example/#x",
    ] {
        assert!(endpoint(origin, DEVICE, false).is_err());
    }
    assert!(endpoint("http://localhost:1234", DEVICE, true).is_err());
    assert!(endpoint("http://192.0.2.1:1234", DEVICE, true).is_err());
    assert!(endpoint("http://127.0.0.1:1234", DEVICE, true).is_ok());
    assert!(Transport::new(
        "https://gateway.example",
        DEVICE.into(),
        Zeroizing::new("token\r\nother:value".into()),
        false
    )
    .is_err());
}

#[tokio::test]
async fn actual_authenticated_websocket_preserves_principal_and_request_identity() {
    let (_, mut socket, task, backend, stop) = connected().await;
    let message = request();
    let GatewayServerFrame::Request { request: req } = message.clone();
    socket
        .send(Message::Text(
            serde_json::to_string(&message).unwrap().into(),
        ))
        .await
        .unwrap();
    let GatewayDaemonFrame::Reply { request_id, result } = next_json(&mut socket).await else {
        panic!("not reply")
    };
    assert_eq!(request_id, req.request_id);
    assert_eq!(result.code, GatewayCode::Ok);
    assert_eq!(result.data.unwrap()["principal_id"], PRINCIPAL);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    stop_link(socket, task, stop).await;
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_transaction_does_not_stop_socket_heartbeat_and_is_awaited_after_disconnect() {
    let (_, mut socket, task, backend, stop) = setup(true).await;
    socket
        .send(Message::Text(
            serde_json::to_string(&request()).unwrap().into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while !backend.entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut pings = 0;
    timeout(Duration::from_secs(2), async {
        while pings < 3 {
            if let Message::Ping(data) = socket.next().await.unwrap().unwrap() {
                pings += 1;
                socket.send(Message::Pong(data)).await.unwrap();
            }
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    drop(socket);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !task.is_finished(),
        "accepted blocking effect must remain owned"
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    backend.release();
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revocation_notifies_browser_principal_without_waiting_for_another_command() {
    let (_, mut socket, task, backend, stop) = connected().await;
    socket
        .send(Message::Text(
            serde_json::to_string(&request()).unwrap().into(),
        ))
        .await
        .unwrap();
    next_json(&mut socket).await;
    backend.revoked.store(true, Ordering::SeqCst);
    assert!(
        matches!(next_json(&mut socket).await,GatewayDaemonFrame::Revoked{principal_id} if principal_id==PRINCIPAL)
    );
    stop_link(socket, task, stop).await;
}

#[tokio::test]
async fn invalid_wire_never_reaches_command_callback() {
    let (_, mut socket, task, backend, stop) = connected().await;
    socket
        .send(Message::Text(
            "{\"type\":\"owner_command\",\"command\":{}}".into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn worst_case_json_escaping_returns_bounded_unknown_without_breaking_generation() {
    let (_, mut socket, task, backend, stop) = connected().await;
    backend.oversized.store(true, Ordering::SeqCst);
    let first = request();
    let GatewayServerFrame::Request {
        request: first_request,
    } = &first;
    socket
        .send(Message::Text(serde_json::to_string(&first).unwrap().into()))
        .await
        .unwrap();
    let response = next_json(&mut socket).await;
    assert!(serde_json::to_vec(&response).unwrap().len() < GATEWAY_MAX_FRAME_BYTES);
    assert!(
        matches!(response,GatewayDaemonFrame::Reply{request_id,result} if request_id==first_request.request_id&&result.code==GatewayCode::OutcomeUnknown&&result.data.is_none())
    );
    backend.oversized.store(false, Ordering::SeqCst);
    let second = request();
    let GatewayServerFrame::Request {
        request: second_request,
    } = &second;
    socket
        .send(Message::Text(
            serde_json::to_string(&second).unwrap().into(),
        ))
        .await
        .unwrap();
    assert!(
        matches!(next_json(&mut socket).await,GatewayDaemonFrame::Reply{request_id,result} if request_id==second_request.request_id&&result.code==GatewayCode::Ok)
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    stop_link(socket, task, stop).await;
}
