use super::*;
use harness_core::{CommandEvidence, ExecutionEvent};

fn fixture() -> (Arc<AppState>, String, ExecutionOutput) {
    fixture_at(&std::env::temp_dir())
}

fn fixture_at(root: &std::path::Path) -> (Arc<AppState>, String, ExecutionOutput) {
    let state = AppState::open(std::path::Path::new(":memory:"), "fixture".into()).unwrap();
    let settings = crate::Settings {
        execution_mode: "native".into(),
        main_executor: "codex".into(),
        ..crate::Settings::default()
    };
    *state.settings.write().unwrap() = settings.clone();
    let command = |value| {
        state.store.lock().unwrap().command(json!({"command_id":harness_core::new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"fixture"},"command":value}), &settings).unwrap().0
    };
    let session = command(json!({"type":"create_session","title":"output","project_path":root}));
    let receipt = command(
        json!({"type":"run_agent","session_id":session["session_id"],"content":"fixture","controls":{"budget":{"max_output_bytes":1024}}}),
    );
    let id = receipt["task_id"].as_str().unwrap().to_owned();
    state.store.lock().unwrap().claim_task().unwrap().unwrap();
    let control = Arc::new(RunControl::start(state.clone(), &id, "codex").unwrap());
    let output = ExecutionOutput::new(state.clone(), &id, control, true, String::new());
    (state, id, output)
}
fn task(state: &AppState, id: &str) -> Value {
    state.store.lock().unwrap().snapshot().unwrap()["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .unwrap()
        .clone()
}

#[test]
fn codex_evidence_and_event_share_precommitted_logical_output_budget() {
    let (state, id, output) = fixture();
    output.evidence(CommandEvidence {
        command: "fixture command".into(),
        exit_code: 0,
        output: "a".repeat(900),
    });
    assert!(task(&state, &id)["evidence"].as_array().unwrap().is_empty());
    output
        .emit(ExecutionEvent::ToolFinished {
            call_id: "one".into(),
            name: "command_execution".into(),
            success: true,
            output: "b".repeat(1000),
        })
        .unwrap();
    assert_eq!(task(&state, &id)["budget_usage"]["output_bytes"], 1000);
    assert_eq!(
        task(&state, &id)["evidence"][0]["output"]
            .as_str()
            .unwrap()
            .len(),
        900
    );
    output.evidence(CommandEvidence {
        command: "fixture failure".into(),
        exit_code: 7,
        output: "c".repeat(800),
    });
    assert!(output
        .emit(ExecutionEvent::ToolFinished {
            call_id: "two".into(),
            name: "command_execution".into(),
            success: false,
            output: "d".repeat(900)
        })
        .is_err());
    let saved = task(&state, &id);
    assert_eq!(saved["budget_usage"]["output_bytes"], 1000);
    assert_eq!(saved["stop_reason"], "output_budget_exhausted");
    assert_eq!(saved["evidence"][1]["exit_code"], 7);
    assert_eq!(saved["evidence"][1]["output"], "");
    assert!(!saved.to_string().contains(&"d".repeat(100)));
}

#[test]
fn unmatched_codex_evidence_is_bounded_and_flushed_without_public_overflow() {
    let (state, id, output) = fixture();
    output.evidence(CommandEvidence {
        command: "fixture".into(),
        exit_code: 0,
        output: "x".repeat(1500),
    });
    assert!(output.flush().is_err());
    let saved = task(&state, &id);
    assert_eq!(saved["budget_usage"]["output_bytes"], 0);
    assert_eq!(saved["evidence"][0]["output"], "");
    assert_eq!(saved["evidence"][0]["exit_code"], 0);
    assert!(output.control.stopped());
}

#[cfg(unix)]
async fn assert_codex_evidence_event_pairing(mode: &str, expected_evidence: usize) {
    struct Root(std::path::PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let root = std::env::temp_dir().join(format!("self-agent-pair-{}", harness_core::new_id()));
    std::fs::create_dir(&root).unwrap();
    let root = Root(root.canonicalize().unwrap());
    let (state, id, output) = fixture_at(&root.0);
    let binary =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/codex_fixture.py");
    crate::codex::execute_with_control(
        binary.to_str().unwrap(),
        mode,
        &root.0,
        crate::context::ExecutionInput {
            objective: "本地测试目标",
            history: &[],
            config: &harness_core::ModelContextConfig::default(),
        },
        |_| {},
        || output.control.stopped(),
        |item| output.evidence(item),
        None,
        &[],
        |event| output.emit(event),
        |_| Ok(()),
        || Ok(Vec::new()),
        |_, _| Ok(()),
        |_| {},
    )
    .await
    .unwrap();
    output.flush().unwrap();
    let saved = task(&state, &id);
    assert_eq!(
        saved["evidence"].as_array().unwrap().len(),
        expected_evidence,
        "unmatched evidence must not share another result's reservation: {saved}"
    );
    assert!(
        !saved.to_string().contains("ORPHAN"),
        "Unmatched output became public: {saved}"
    );
    let events =
        serde_json::to_string(&state.store.lock().unwrap().events_after(0).unwrap()).unwrap();
    assert!(events.contains(&"SECOND".repeat(100)));
    assert!(!events.contains("ORPHAN"));
    assert!(!events.contains("private-fixture-config-value"));
    assert!(saved["budget_usage"]["output_bytes"].as_u64().unwrap() <= 1024);
    assert!(saved["budget_usage"]["output_bytes"].as_u64().unwrap() >= 600);
}

#[cfg(unix)]
#[tokio::test]
async fn codex_invalid_item_id_cannot_publish_unpaired_budget_evidence() {
    assert_codex_evidence_event_pairing("evidence-orphan", 0).await;
}

#[cfg(unix)]
#[tokio::test]
async fn codex_redacted_duplicate_id_cannot_publish_unpaired_budget_evidence() {
    assert_codex_evidence_event_pairing("evidence-redacted-id-collision", 1).await;
}
