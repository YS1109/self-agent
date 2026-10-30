//! Actual AppState/Store/dispatcher and authenticated owner-router regressions.
//! Only cfg(test) memory Vault and demo execution; no real model or OS secrets.
use crate::{
    capabilities::vault::test_support,
    reliability::lifecycle::{LifecycleState, WorkerExit, WorkerKind},
    *,
};
use std::{path::PathBuf, time::Instant};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("shutdown-state-{}", new_id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn state(root: &Root) -> Arc<AppState> {
    let (vault, _) = test_support::memory(8).unwrap();
    AppState::open_with_vault(&root.0.join("state.sqlite"), "shutdown-owner".into(), vault).unwrap()
}
fn send(state: &AppState, command: Value) -> Value {
    let settings = state.settings.read().unwrap().clone();
    state.store_only(|store| Ok(store.command(json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"shutdown"},"command":command}), &settings)?.0)).unwrap()
}
async fn stopped(state: &Arc<AppState>) {
    tokio::time::timeout(Duration::from_secs(5), state.wait_stopped())
        .await
        .unwrap();
}

#[tokio::test]
async fn rejected_sql_only_input_is_settled_not_a_permanent_physical_unknown() {
    let root = Root::new();
    let state = state(&root);
    state.ensure_cleanup_owner().unwrap();
    let custodians = state.lifecycle.snapshot().workers.pending_workers;
    for _ in 0..3 {
        let result: ApiResult<()> =
            state.store_only(|_| Err(ApiError(StatusCode::BAD_REQUEST, "无效输入".into())));
        assert_eq!(result.unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(
            state.lifecycle.snapshot().workers.pending_workers,
            custodians
        );
        assert_eq!(state.lifecycle.snapshot().workers.active_admissions, 0);
    }
    assert_eq!(state.lifecycle.snapshot().state, LifecycleState::Running);
    assert_eq!(state.lifecycle.snapshot().workers.unknown_admissions, 0);
    state.request_shutdown().unwrap();
    stopped(&state).await;
}

#[tokio::test]
async fn actual_store_preclaimed_waiter_cannot_commit_after_shutdown_gate() {
    let root = Root::new();
    let state = state(&root);
    state.ensure_cleanup_owner().unwrap();
    let mut permit = state.lifecycle.admit().unwrap();
    {
        let store = state.store.lock().unwrap();
        state.request_shutdown().unwrap();
        assert!(permit.begin_intent(&store).is_err());
    }
    permit.settle();
    assert!(state.store_only(|_| Ok(())).is_err());
    stopped(&state).await;
}

#[tokio::test]
async fn late_task_finish_before_first_supervisor_store_turn_cannot_revive_or_advance() {
    let root = Root::new();
    let state = state(&root);
    state.settings.write().unwrap().execution_mode = "demo".into();
    let sid = send(
        &state,
        json!({"type":"create_session","title":"shutdown","project_path":root.0}),
    )["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = send(
        &state,
        json!({"type":"run_agent","session_id":sid,"content":"演示，无外部操作"}),
    )["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    state.store.lock().unwrap().claim_task().unwrap().unwrap();
    state.request_shutdown().unwrap();
    // No await: the spawned supervisor cannot have taken Store yet on this
    // current-thread runtime. finish must itself fence terminal advancement.
    assert!(finish(&state, &id, Ok("迟到成功不得恢复".into())));
    let snapshot = state.store.lock().unwrap().snapshot().unwrap();
    let task = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == id)
        .unwrap();
    assert_eq!(task["status"], "cancelled");
    assert_eq!(task["stop_reason"], "daemon_shutdown");
    assert_ne!(task["result"], "迟到成功不得恢复");
    stopped(&state).await;
}

#[tokio::test]
async fn real_fixed_dispatchers_cancel_demo_and_exit_without_reclaiming_queue() {
    let root = Root::new();
    let state = state(&root);
    state.settings.write().unwrap().execution_mode = "demo".into();
    let sid = send(
        &state,
        json!({"type":"create_session","title":"shutdown","project_path":root.0}),
    )["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = send(
        &state,
        json!({"type":"run_agent","session_id":sid,"content":"演示，无外部操作"}),
    )["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    spawn_workers(state.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = state.store.lock().unwrap().snapshot().unwrap();
            if snapshot["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|task| task["id"] == id && task["status"] == "running")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.request_shutdown().unwrap();
    stopped(&state).await;
    let snapshot = state.store.lock().unwrap().snapshot().unwrap();
    assert!(snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["status"] != "running" && task["status"] != "queued"));
    assert_eq!(state.lifecycle.snapshot().workers.pending_workers, 0);
}

#[tokio::test]
async fn owner_monitor_quiesces_real_store_when_worker_fails_without_http_polling() {
    let root = Root::new();
    let state = state(&root);
    state.settings.write().unwrap().execution_mode = "demo".into();
    let sid = send(
        &state,
        json!({"type":"create_session","title":"shutdown","project_path":root.0}),
    )["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &state,
        json!({"type":"run_agent","session_id":sid,"content":"不得执行"}),
    );
    let (trigger, triggered) = tokio::sync::oneshot::channel();
    state
        .lifecycle
        .spawn(WorkerKind::Task, async {
            triggered.await.unwrap();
            WorkerExit::Unknown
        })
        .unwrap();
    let observed = state.clone();
    let owner = tokio::spawn(async move {
        observed.monitor_shutdown().await;
    });
    trigger.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), owner)
        .await
        .unwrap()
        .unwrap();
    let report = state
        .lifecycle
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(
        report.snapshot.outcome,
        Some(reliability::lifecycle_runtime::ShutdownOutcome::Unknown)
    );
    let snapshot = state.store.lock().unwrap().snapshot().unwrap();
    assert!(snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["status"] == "cancelled"));
}

#[tokio::test]
async fn actual_owner_auth_and_wire_rejection_cannot_start_shutdown() {
    let root = Root::new();
    let state = state(&root);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state.clone());
    let (stop, stopped_http) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped_http.await;
            })
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let path = format!("{base}/api/v1/lifecycle/shutdown");
    assert_eq!(
        client
            .post(&path)
            .json(&json!({"expected_instance":state.lifecycle.instance_id()}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&path)
            .bearer_auth("shutdown-owner")
            .header("x-self-agent-protocol", "999")
            .json(&json!({"expected_instance":state.lifecycle.instance_id()}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UPGRADE_REQUIRED
    );
    assert_eq!(state.lifecycle.snapshot().state, LifecycleState::Running);
    let response = client
        .post(&path)
        .bearer_auth("shutdown-owner")
        .json(&json!({"expected_instance":state.lifecycle.instance_id()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    stopped(&state).await;
    let handshake: Value = client
        .get(format!("{base}/api/v1/compatibility"))
        .bearer_auth("shutdown-owner")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(handshake["lifecycle"], "stopped");
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn late_plan_child_completion_is_fenced_before_dependent_dispatch() {
    let root = Root::new();
    let state = state(&root);
    state.settings.write().unwrap().execution_mode = "demo".into();
    let sid = send(
        &state,
        json!({"type":"create_session","title":"plan shutdown","project_path":root.0}),
    )["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &state,
        json!({"type":"send_message","session_id":sid,"content":"明确编排"}),
    );
    let plan = {
        let mut store = state.store.lock().unwrap();
        let job = store.claim_coordinator_job().unwrap().unwrap();
        store.complete_coordinator_job(job["id"].as_str().unwrap(),Ok(json!({"action":"plan","message":"fixture","title":"two steps","nodes":[{"id":"a","title":"first","objective":"fixture a","depends_on":[]},{"id":"b","title":"second","objective":"fixture b","depends_on":["a"]}]}))).unwrap();
        store.snapshot().unwrap()["plans"][0].clone()
    };
    send(
        &state,
        json!({"type":"start_plan","plan_id":plan["id"],"expected_revision":plan["revision"]}),
    );
    let child = state.store.lock().unwrap().claim_task().unwrap().unwrap();
    assert_eq!(child["node_id"], "a");
    let before = state.store.lock().unwrap().snapshot().unwrap()["tasks"]
        .as_array()
        .unwrap()
        .len();
    state.request_shutdown().unwrap();
    assert!(finish(
        &state,
        child["id"].as_str().unwrap(),
        Ok("late result".into())
    ));
    let snapshot = state.store.lock().unwrap().snapshot().unwrap();
    assert_eq!(snapshot["tasks"].as_array().unwrap().len(), before);
    assert_eq!(snapshot["plans"][0]["status"], "cancelled");
    assert!(snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["node_id"] != "b"));
    stopped(&state).await;
}

#[test]
fn fallible_startup_preflight_never_creates_the_system_vault_worker() {
    let root = Root::new();
    std::fs::write(root.0.join("capability-probes"), b"not a directory").unwrap();
    let called = std::sync::atomic::AtomicBool::new(false);
    let result =
        AppState::open_prepared(&root.0.join("state.sqlite"), "owner".into(), None, || {
            called.store(true, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("vault factory should not be reached");
        });
    assert!(result.is_err());
    assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn metadata_startup_failure_retains_data_lease_until_actual_vault_thread_joins() {
    let root = Root::new();
    let lease = maintenance::DataLease::acquire(&root.0).unwrap();
    let (vault, gate) = test_support::memory_with_blocked_drop(8).unwrap();
    let state = AppState::open_with_vault_and_lease(
        &lease.database_path(),
        "startup".into(),
        vault,
        Some(lease.clone()),
    )
    .unwrap();
    // A directory at the fixed metadata leaf produces a real write error;
    // production refuses to replace it rather than weakening no-follow rules.
    std::fs::create_dir(root.0.join("daemon.json")).unwrap();
    let info = harness_core::DaemonInfo {
        base_url: "http://127.0.0.1:1".into(),
        token: "fixture".into(),
        pid: std::process::id(),
    };
    assert!(lease.write_daemon_metadata(&info).is_err());
    state.request_shutdown().unwrap();
    assert!(gate.entered().await);
    let pending = state
        .lifecycle
        .observe(Instant::now() + Duration::from_millis(30))
        .await;
    assert!(pending.deadline_reached);
    assert_ne!(pending.snapshot.state, LifecycleState::Stopped);
    drop(lease);
    assert!(
        maintenance::DataLease::acquire(&root.0).is_err(),
        "cleanup must retain the actual directory owner"
    );
    gate.release();
    stopped(&state).await;
    drop(state);
    let reopened = maintenance::DataLease::acquire(&root.0).unwrap();
    drop(reopened);
}

#[tokio::test]
async fn sql_only_error_preserves_committed_data_without_claiming_rollback_or_external_work() {
    let root = Root::new();
    let state = state(&root);
    state.ensure_cleanup_owner().unwrap();
    let before = state.lifecycle.snapshot().workers.pending_workers;
    let settings = Settings {
        model: "fixture-committed-before-error".into(),
        ..Settings::default()
    };
    let result: ApiResult<()> = state.store_only(|store| {
        store.save_settings(&settings)?;
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "同步动作已结束，但不能假定SQL回滚".into(),
        ))
    });
    assert!(result.is_err());
    assert_eq!(
        state.store.lock().unwrap().load_settings().unwrap().model,
        settings.model
    );
    assert_eq!(state.lifecycle.snapshot().workers.pending_workers, before);
    assert_eq!(state.lifecycle.snapshot().workers.unknown_admissions, 0);
    state.request_shutdown().unwrap();
    stopped(&state).await;
    assert_eq!(
        state.store.lock().unwrap().load_settings().unwrap().model,
        settings.model
    );
}
