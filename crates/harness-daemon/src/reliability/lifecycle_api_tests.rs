use super::*;
use crate::reliability::{
    lifecycle::{LifecycleState, WorkerExit, WorkerKind},
    lifecycle_runtime::{QuiesceStage, QuiescenceCheck, ShutdownFault, ShutdownOutcome},
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, sync::oneshot, task::JoinHandle};

#[derive(Default)]
struct Backend {
    signals: AtomicUsize,
    checks: AtomicUsize,
}
impl ShutdownBackend for Backend {
    fn stop_external_inputs(&self) {
        self.signals.fetch_add(1, Ordering::SeqCst);
    }
    fn quiesce(&self, _: QuiesceStage) -> Result<QuiescenceCheck, ShutdownFault> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Ok(QuiescenceCheck::default())
    }
}
struct Server {
    address: std::net::SocketAddr,
    stop: oneshot::Sender<()>,
    worker: JoinHandle<()>,
}
impl Server {
    async fn open(runtime: Arc<LifecycleRuntime>, backend: Arc<Backend>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app: Router = routes(runtime, backend);
        let (stop, stopped) = oneshot::channel();
        let worker = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            address,
            stop,
            worker,
        }
    }
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }
    async fn close(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.worker)
            .await
            .unwrap()
            .unwrap();
    }
}
fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn malformed_control_body_never_closes_gate_or_echoes_private_material() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let server = Server::open(runtime.clone(), backend.clone()).await;
    let client = client();
    for body in [
        "{\"secret-canary\":\"private-token\"}".to_owned(),
        "[]".to_owned(),
        "null".to_owned(),
        "{malformed-secret-canary".to_owned(),
        format!("{{\"secret-canary\":\"{}\"}}", "private-token".repeat(32)),
    ] {
        let response = client
            .post(server.url("/api/v1/lifecycle/shutdown"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
        ));
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let text = response.text().await.unwrap();
        assert!(!text.contains("secret-canary"));
        assert!(!text.contains("private-token"));
        assert_eq!(runtime.snapshot().state, LifecycleState::Running);
    }
    assert_eq!(backend.signals.load(Ordering::SeqCst), 0);
    assert_eq!(backend.checks.load(Ordering::SeqCst), 0);
    server.close().await;
}

#[tokio::test]
async fn repeated_control_responses_do_not_wait_for_or_duplicate_actual_cleanup() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let (release, released) = oneshot::channel();
    runtime
        .spawn(WorkerKind::BlockingCleanup, async move {
            released.await.unwrap();
            WorkerExit::Settled
        })
        .unwrap();
    let server = Server::open(runtime.clone(), backend.clone()).await;
    let client = client();
    for _ in 0..2 {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            client
                .post(server.url("/api/v1/lifecycle/shutdown"))
                .json(&json!({"expected_instance":runtime.instance_id()}))
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state: serde_json::Value = response.json().await.unwrap();
        assert_eq!(state["state"], "draining");
        assert_eq!(state["workers"]["pending_workers"], 1);
    }
    let status = client
        .get(server.url("/api/v1/lifecycle"))
        .send()
        .await
        .unwrap();
    assert_eq!(status.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        status.json::<serde_json::Value>().await.unwrap()["state"],
        "draining"
    );
    release.send(()).unwrap();
    let result = runtime
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(result.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    assert_eq!(backend.signals.load(Ordering::SeqCst), 1);
    assert_eq!(backend.checks.load(Ordering::SeqCst), 2);
    server.close().await;
}

#[tokio::test]
async fn disconnected_control_observer_does_not_cancel_shutdown_supervisor() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let (release, released) = oneshot::channel();
    runtime
        .spawn(WorkerKind::BlockingCleanup, async move {
            released.await.unwrap();
            WorkerExit::Settled
        })
        .unwrap();
    let server = Server::open(runtime.clone(), backend.clone()).await;
    let mut socket = tokio::net::TcpStream::connect(server.address)
        .await
        .unwrap();
    let body = json!({"expected_instance":runtime.instance_id()}).to_string();
    socket.write_all(format!("POST /api/v1/lifecycle/shutdown HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
    // Wait for actual admission closure, not for an HTTP response: thereafter
    // dropping the connection is only observer loss, never a cancellation API.
    tokio::time::timeout(Duration::from_secs(5), runtime.signal().cancelled())
        .await
        .unwrap();
    drop(socket);
    release.send(()).unwrap();
    let result = runtime
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(result.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    assert_eq!(backend.signals.load(Ordering::SeqCst), 1);
    assert_eq!(backend.checks.load(Ordering::SeqCst), 2);
    server.close().await;
}

#[tokio::test]
async fn invalid_wait_query_is_static_and_never_admits_shutdown() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let server = Server::open(runtime.clone(), backend.clone()).await;
    let client = client();
    for query in [
        "wait_ms=30001",
        "wait_ms=-1",
        "wait_ms=bad-secret-canary",
        "private-token=1",
        "wait_ms=1&wait_ms=2",
    ] {
        let response = client
            .post(server.url(&format!("/api/v1/lifecycle/shutdown?{query}")))
            .json(&json!({"expected_instance":runtime.instance_id()}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.text().await.unwrap();
        assert!(!body.contains("bad-secret-canary"));
        assert!(!body.contains("private-token"));
        assert_eq!(runtime.snapshot().state, LifecycleState::Running);
    }
    assert_eq!(backend.signals.load(Ordering::SeqCst), 0);
    server.close().await;
}

#[tokio::test]
async fn bounded_wait_returns_pending_without_stopping_worker_then_confirms_stopped() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let (release, released) = oneshot::channel();
    runtime
        .spawn(WorkerKind::BlockingCleanup, async move {
            released.await.unwrap();
            WorkerExit::Settled
        })
        .unwrap();
    let server = Server::open(runtime.clone(), backend).await;
    let client = client();
    let pending = client
        .post(server.url("/api/v1/lifecycle/shutdown?wait_ms=1"))
        .json(&json!({"expected_instance":runtime.instance_id()}))
        .send()
        .await
        .unwrap();
    assert_eq!(pending.status(), StatusCode::ACCEPTED);
    assert_eq!(
        pending.json::<serde_json::Value>().await.unwrap()["state"],
        "draining"
    );
    release.send(()).unwrap();
    let settled = client
        .post(server.url("/api/v1/lifecycle/shutdown?wait_ms=5000"))
        .json(&json!({"expected_instance":runtime.instance_id()}))
        .send()
        .await
        .unwrap();
    assert_eq!(settled.status(), StatusCode::OK);
    let snapshot = settled.json::<serde_json::Value>().await.unwrap();
    assert_eq!(snapshot["state"], "stopped");
    assert_eq!(snapshot["outcome"], "stopped");
    server.close().await;
}

#[tokio::test]
async fn dropped_wait_response_remains_only_an_observer_not_a_worker() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let (release, released) = oneshot::channel();
    runtime
        .spawn(WorkerKind::BlockingCleanup, async move {
            released.await.unwrap();
            WorkerExit::Settled
        })
        .unwrap();
    let server = Server::open(runtime.clone(), backend).await;
    let mut socket = tokio::net::TcpStream::connect(server.address)
        .await
        .unwrap();
    let body = json!({"expected_instance":runtime.instance_id()}).to_string();
    socket.write_all(format!("POST /api/v1/lifecycle/shutdown?wait_ms=30000 HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), runtime.signal().cancelled())
        .await
        .unwrap();
    assert_eq!(runtime.snapshot().workers.pending_workers, 1);
    drop(socket);
    release.send(()).unwrap();
    let outcome = runtime
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(outcome.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    server.close().await;
}

#[tokio::test]
async fn old_instance_observation_cannot_shutdown_the_current_runtime() {
    let runtime = LifecycleRuntime::new();
    let old = LifecycleRuntime::new();
    assert_ne!(runtime.instance_id(), old.instance_id());
    let backend = Arc::new(Backend::default());
    let server = Server::open(runtime.clone(), backend.clone()).await;
    let response = client()
        .post(server.url("/api/v1/lifecycle/shutdown?wait_ms=10"))
        .json(&json!({"expected_instance":old.instance_id()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(runtime.snapshot().state, LifecycleState::Running);
    assert_eq!(backend.signals.load(Ordering::SeqCst), 0);
    let response = client()
        .get(server.url("/api/v1/lifecycle"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(response["instance_id"], runtime.instance_id());
    server.close().await;
}
