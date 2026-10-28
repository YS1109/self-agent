//! Deterministic local-only write lifetime regressions.
use super::*;
use crate::AppState;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};

struct Release(Arc<(Mutex<bool>, Condvar)>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }
}
fn command(state: &AppState, value: Value) -> Value {
    let settings = state.settings.read().unwrap().clone();
    state
        .store
        .lock()
        .unwrap()
        .command(
            json!({
                "command_id":harness_core::new_id(),
                "source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"fixture"},
                "command":value
            }),
            &settings,
        )
        .unwrap()
        .0
}
fn task(state: &AppState, id: &str) -> Value {
    state.store.lock().unwrap().snapshot().unwrap()["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == id)
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_settles_before_executor_returns_and_project_lease_releases() {
    for deadline in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "self-agent-write-lifetime-{}",
            harness_core::new_id()
        ));
        std::fs::create_dir(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|| async {
            axum::Json(json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{
                "id":"write-fixture","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"result.txt\",\"content\":\"persisted\"}"}
            }]},"finish_reason":"tool_calls"}]}))
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let state = AppState::open(Path::new(":memory:"), "fixture".into()).unwrap();
        *state.settings.write().unwrap() = Settings {
            provider_source: "manual".into(),
            base_url: format!("http://{address}/v1"),
            model: "fixture".into(),
            execution_mode: "native".into(),
            ..Settings::default()
        };
        let session = command(
            &state,
            json!({"type":"create_session","title":"fixture","project_path":root}),
        );
        let receipt = command(
            &state,
            json!({"type":"run_agent","session_id":session["session_id"],"content":"write fixture","controls":{"budget":{"max_runtime_seconds":if deadline {1} else {900}}}}),
        );
        let id = receipt["task_id"].as_str().unwrap().to_owned();
        state.store.lock().unwrap().claim_task().unwrap().unwrap();
        let control = Arc::new(RunControl::start(state.clone(), &id, "native").unwrap());
        let mut project = ScopedProject::open(&root).unwrap();
        let reached = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(barrier.clone());
        let reached_hook = reached.clone();
        project.set_before_rename_hook(Arc::new(move || {
            reached_hook.store(true, Ordering::SeqCst);
            let (lock, condition) = &*barrier;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condition.wait(released).unwrap();
            }
        }));
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        state
            .workspace_gates
            .lock()
            .unwrap()
            .insert(root.clone(), semaphore.clone());
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let execution_state = state.clone();
        let execution_id = id.clone();
        let execution_root = root.clone();
        let execution_control = control.clone();
        let execution = tokio::spawn(async move {
            // Mirrors run_task's lease lifetime around the actual provider call.
            let _permit = permit;
            let settings = execution_state.settings.read().unwrap().clone();
            let result = execute_with_runtime(
                &settings,
                &execution_root,
                ExecutionInput {
                    objective: "write fixture",
                    history: &[],
                    config: &ModelContextConfig::default(),
                },
                |_| {},
                || execution_control.stopped(),
                |_| {},
                |_| Ok(()),
                None,
                |_| Ok(()),
                || Ok(Vec::new()),
                |_, _| Ok(()),
                Some(&execution_control),
                Some(&project),
            )
            .await;
            assert!(result.is_err());
            crate::finish(&execution_state, &execution_id, result);
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !reached.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(task(&state, &id)["execution_cleanup"], "pending");
        if deadline {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(control.stopped());
        } else {
            command(&state, json!({"type":"cancel_task","task_id":id}));
            assert_eq!(task(&state, &id)["status"], "cancelled");
        }
        assert_eq!(task(&state, &id)["execution_cleanup"], "pending");
        assert!(!execution.is_finished());
        assert!(
            semaphore.try_acquire().is_err(),
            "writer lease released before atomic write settled"
        );
        assert!(!root.join("result.txt").exists());
        drop(release);
        tokio::time::timeout(Duration::from_secs(10), execution)
            .await
            .unwrap()
            .unwrap();
        assert!(semaphore.try_acquire().is_ok());
        let final_task = task(&state, &id);
        assert_eq!(final_task["execution_cleanup"], "settled");
        assert_eq!(
            final_task["status"],
            if deadline { "failed" } else { "cancelled" }
        );
        assert_eq!(
            std::fs::read_to_string(root.join("result.txt")).unwrap(),
            "persisted"
        );
        let snapshot = state.store.lock().unwrap().snapshot().unwrap();
        assert_eq!(
            snapshot["deliveries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|delivery| delivery["task_id"] == id)
                .count(),
            1
        );
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}
