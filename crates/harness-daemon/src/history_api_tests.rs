use super::*;
use crate::Settings;
use harness_core::new_id;
use serde_json::Value;
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    state: Arc<AppState>,
    worker: tokio::task::JoinHandle<()>,
    url: String,
    session: String,
    task: String,
}
impl Fixture {
    async fn open(count: usize) -> Self {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("history-api-{}", new_id()));
        std::fs::create_dir(&root).unwrap();
        let state = AppState::open(&root.join("harness.sqlite"), "fixture-owner".into()).unwrap();
        let settings = Settings {
            execution_mode: "demo".into(),
            ..Settings::default()
        };
        let source = json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"local"});
        let (session, task) = {
            let mut store = state.store.lock().unwrap();
            let session = store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"create_session","title":"历史夹具","project_path":root}}), &settings).unwrap().0["session_id"].as_str().unwrap().to_owned();
            let mut task = String::new();
            for n in 0..count {
                task = store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"run_agent","session_id":session,"content":format!("fixture task {n}")}}), &settings).unwrap().0["task_id"].as_str().unwrap().to_owned();
                assert_eq!(store.claim_task().unwrap().unwrap()["id"], task);
                store
                    .finish(&task, Ok(format!("fixture result {n}")))
                    .unwrap();
            }
            (session, task)
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let application = crate::router(state.clone());
        let worker = tokio::spawn(async move {
            axum::serve(listener, application).await.unwrap();
        });
        Self {
            root,
            state,
            worker,
            url,
            session,
            task,
        }
    }
    async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("{}{path}", self.url))
            .bearer_auth("fixture-owner")
            .send()
            .await
            .unwrap()
    }
    async fn close(self) {
        self.worker.abort();
        let _ = self.worker.await;
        drop(self.state);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

#[tokio::test]
async fn actual_owner_routes_are_bounded_readonly_and_return_atomic_task_cursor() {
    let fixture = Fixture::open(1).await;
    let unauthorized = reqwest::Client::new()
        .get(format!("{}/api/v1/history/tasks", fixture.url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let snapshot: Value = fixture
        .get("/api/v1/snapshot?window=1")
        .await
        .json()
        .await
        .unwrap();
    let legacy: Value = fixture.get("/api/v1/snapshot").await.json().await.unwrap();
    assert!(legacy.get("history").is_none());
    assert_eq!(legacy["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["history"]["version"], 1);
    assert_eq!(snapshot["history"]["anchor"], fixture.state.history_epoch);
    let response = fixture
        .get(&format!("/api/v1/tasks/{}", fixture.task))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value = response.json().await.unwrap();
    assert_eq!(detail["task"]["id"], fixture.task);
    assert_eq!(detail["task"]["status"], "completed");
    assert_eq!(detail["task"]["result"], "fixture result 0");
    assert_eq!(detail["event_cursor"], snapshot["event_cursor"]);
    let page: Value = fixture
        .get("/api/v1/history/sessions")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(page["items"][0]["messages"], json!([]));
    assert!(
        page["session_messages"][&fixture.session]["total"]
            .as_u64()
            .unwrap()
            > 0
    );
    let messages: Value = fixture
        .get(&format!(
            "/api/v1/history/sessions/{}/messages",
            fixture.session
        ))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(messages["returned"], 2);
    assert_eq!(messages["items"][0]["history_offset"], 0);
    assert_eq!(
        fixture.get("/api/v1/tasks/missing").await.status(),
        StatusCode::NOT_FOUND
    );
    for path in [
        "/api/v1/tasks/a%5Fb",
        "/api/v1/history/tasks?unknown=value",
        "/api/v1/history/tasks?cursor=a&cursor=b",
        "/api/v1/history/tasks?session_id=unsafe.id",
        "/api/v1/snapshot?full=true",
        "/api/v1/snapshot?window=2",
        "/api/v1/snapshot?window=1&window=1",
    ] {
        assert_eq!(
            fixture.get(path).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    let method = reqwest::Client::new()
        .post(format!("{}/api/v1/history/tasks", fixture.url))
        .bearer_auth("fixture-owner")
        .json(&json!({"command":"not allowed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
    fixture.close().await;
}

#[tokio::test]
async fn actual_page_cursor_is_stable_and_restart_epoch_does_not_replay() {
    let fixture = Fixture::open(81).await;
    let first: Value = fixture
        .get("/api/v1/history/tasks")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(first["returned"], 80);
    assert_eq!(first["total"], 81);
    let message_path = format!("/api/v1/history/sessions/{}/messages", fixture.session);
    let before_messages: Value = fixture.get(&message_path).await.json().await.unwrap();
    {
        let settings = Settings {
            execution_mode: "demo".into(),
            ..Settings::default()
        };
        fixture.state.store.lock().unwrap().command(json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"local"},"command":{"type":"run_agent","session_id":fixture.session,"content":"new after first page"}}), &settings).unwrap();
    }
    let repeated: Value = fixture
        .get(&format!(
            "/api/v1/history/tasks?cursor={}",
            first["current_cursor"].as_str().unwrap()
        ))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(repeated["items"], first["items"]);
    assert_eq!(repeated["total"], 81);
    let refreshed: Value = fixture
        .get("/api/v1/history/tasks")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(refreshed["total"], 82);
    let repeated_messages: Value = fixture
        .get(&format!(
            "{message_path}?cursor={}",
            before_messages["current_cursor"].as_str().unwrap()
        ))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(repeated_messages["items"], before_messages["items"]);
    assert_eq!(repeated_messages["total"], before_messages["total"]);
    let refreshed_messages: Value = fixture.get(&message_path).await.json().await.unwrap();
    assert_eq!(
        refreshed_messages["total"].as_u64().unwrap(),
        before_messages["total"].as_u64().unwrap() + 1
    );
    let cursor = first["next_cursor"].as_str().unwrap();
    let second: Value = fixture
        .get(&format!("/api/v1/history/tasks?cursor={cursor}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(second["returned"], 1);
    assert_eq!(second["has_more"], false);
    let other = Fixture::open(1).await;
    let response = other
        .get(&format!("/api/v1/history/tasks?cursor={cursor}"))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["code"], "history_reset");
    assert_eq!(
        other.state.store.lock().unwrap().snapshot().unwrap()["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    other.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn history_admission_is_bounded_and_errors_never_expose_underlying_records() {
    let fixture = Fixture::open(1).await;
    let slots = fixture
        .state
        .history_gate
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    let response = fixture.get("/api/v1/snapshot?window=1").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "history_busy"
    );
    drop(slots);
    assert_eq!(
        fixture.get("/api/v1/snapshot?window=1").await.status(),
        StatusCode::OK
    );
    let response = fixture
        .get("/api/v1/history/tasks?cursor=fixture-private-invalid-cursor")
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!response.text().await.unwrap().contains("fixture-private"));
    fixture.close().await;
}

#[tokio::test]
async fn disconnected_history_observer_keeps_blocking_owner_until_actual_drain() {
    use std::time::{Duration, Instant};
    let fixture = Fixture::open(1).await;
    let store = fixture.state.store.clone();
    let (entered, held) = std::sync::mpsc::channel();
    let (release, proceed) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _store = store.lock().unwrap();
        entered.send(()).unwrap();
        let _ = proceed.recv_timeout(Duration::from_secs(5));
    });
    held.recv_timeout(Duration::from_secs(2)).unwrap();
    let url = format!("{}/api/v1/history/tasks", fixture.url);
    let observer = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url)
            .bearer_auth("fixture-owner")
            .send()
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.state.history_gate.available_permits() == 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    observer.abort();
    let _ = observer.await;
    fixture.state.request_shutdown().unwrap();
    let pending = fixture
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_millis(40))
        .await;
    assert!(pending.deadline_reached);
    assert_eq!(
        pending.snapshot.state,
        crate::reliability::lifecycle::LifecycleState::Draining
    );
    assert_eq!(fixture.state.history_gate.available_permits(), 1);
    release.send(()).unwrap();
    blocker.join().unwrap();
    let stopped = fixture
        .state
        .lifecycle
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(
        stopped.snapshot.outcome,
        Some(crate::reliability::lifecycle_runtime::ShutdownOutcome::Stopped)
    );
    assert_eq!(fixture.state.history_gate.available_permits(), 2);
    assert_eq!(
        fixture.get("/api/v1/history/tasks").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    fixture.close().await;
}
