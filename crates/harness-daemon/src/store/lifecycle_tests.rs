use super::*;
use crate::steering::InputAcknowledgement;

fn settings() -> Settings {
    Settings {
        base_url: "https://fixture.invalid/v1".into(),
        model: "fixture-model".into(),
        ..Default::default()
    }
}
fn send(store: &mut Store, command: Value) -> Value {
    store.command(json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"shutdown-fixture"},"command":command}),&settings()).unwrap().0
}
fn session(store: &mut Store) -> String {
    send(store,json!({"type":"create_session","title":"停止夹具","project_path":std::env::temp_dir().canonicalize().unwrap()}))["session_id"].as_str().unwrap().into()
}
fn enqueue(store: &mut Store, session: &str) -> String {
    send(
        store,
        json!({"type":"run_agent","session_id":session,"content":"保留已有修改"}),
    )["task_id"]
        .as_str()
        .unwrap()
        .into()
}
fn running(store: &mut Store, session: &str) -> String {
    let id = enqueue(store, session);
    assert_eq!(store.claim_task().unwrap().unwrap()["id"], id);
    id
}
fn new_store() -> Store {
    Store::open(Path::new(":memory:")).unwrap()
}
fn plan_result() -> Value {
    json!({"action":"plan","message":"待人工启动","title":"夹具计划","nodes":[{"id":"a","title":"前置","objective":"读取现有内容","depends_on":[]},{"id":"b","title":"后继","objective":"等待前置完成","depends_on":["a"]}]})
}
fn draft(store: &mut Store, session: &str) -> Value {
    send(
        store,
        json!({"type":"send_message","session_id":session,"content":"明确请求编排"}),
    );
    let job = store.claim_coordinator_job().unwrap().unwrap();
    store
        .complete_coordinator_job(text(&job, "id").unwrap(), Ok(plan_result()))
        .unwrap();
    rows(&store.db, "plans").unwrap().pop().unwrap()
}
fn assert_idle(store: &mut Store) {
    assert!(store.claim_task().unwrap().is_none());
    assert!(store.claim_coordinator_job().unwrap().is_none());
    assert!(store.deliver_one().unwrap().is_none());
}
#[test]
fn empty_and_repeated_quiescence_are_idempotent_not_physical_stop_proof() {
    let mut store = new_store();
    let before = store.snapshot().unwrap();
    let first = store.quiesce_for_shutdown().unwrap();
    assert!(!first.changed());
    assert!(!first.process_cleanup_confirmed);
    assert!(first.events.is_empty());
    assert_eq!(store.snapshot().unwrap(), before);
    assert!(store.quiesce_for_shutdown().unwrap().events.is_empty());
}
#[test]
fn running_and_queued_main_are_terminal_and_late_success_cannot_resurrect() {
    let mut store = new_store();
    let sid = session(&mut store);
    let active = running(&mut store, &sid);
    store
        .execution_event(
            &active,
            ExecutionEvent::AssistantDelta {
                text: "已确认的片段".into(),
            },
        )
        .unwrap();
    let queued = enqueue(&mut store, &sid);
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.cancelled_tasks, 2);
    assert_eq!(report.stopped_deliveries, 2);
    assert!(!report.process_cleanup_confirmed);
    assert!(report
        .events
        .windows(2)
        .all(|w| w[0].sequence < w[1].sequence));
    assert!(report.events.iter().all(|e| e.sequence > 0));
    for id in [&active, &queued] {
        let task = record(&store.db, "tasks", id).unwrap();
        assert_eq!(task["status"], "cancelled");
        assert_eq!(task["stop_reason"], STOP_REASON);
        assert!(!store.task_may_start(id).unwrap());
        assert!(store
            .finish(id, Ok("迟到成功不能生效".into()))
            .unwrap()
            .is_none());
    }
    assert_eq!(
        record(&store.db, "tasks", &active).unwrap()["stream_output"],
        "已确认的片段"
    );
    assert!(store
        .execution_event(
            &active,
            ExecutionEvent::AssistantDelta {
                text: "迟到".into()
            }
        )
        .is_err());
    let after = store.snapshot().unwrap();
    assert!(!store.quiesce_for_shutdown().unwrap().changed());
    assert_eq!(store.snapshot().unwrap(), after);
    assert_idle(&mut store);
}
#[test]
fn pending_approval_is_cancelled_and_prior_approval_is_audit_only() {
    let mut store = new_store();
    let first_session = session(&mut store);
    let first = running(&mut store, &first_session);
    let pending = store
        .create_decision_request(
            &first,
            "approval",
            "授权写入？",
            Some(json!({"tool":"write_file","digest":"fixture"})),
            900,
        )
        .unwrap();
    let second_session = session(&mut store);
    let second = running(&mut store, &second_session);
    let approved = store
        .create_decision_request(
            &second,
            "approval",
            "授权命令？",
            Some(json!({"tool":"run_command","digest":"fixture-2"})),
            900,
        )
        .unwrap();
    send(
        &mut store,
        json!({"type":"decide_request","request_id":approved["id"],"expected_revision":0,"approve":true,"action_digest":approved["action_digest"]}),
    );
    store.quiesce_for_shutdown().unwrap();
    assert_eq!(
        store
            .decision_request(text(&pending, "id").unwrap())
            .unwrap()["status"],
        "cancelled"
    );
    let audited = store
        .decision_request(text(&approved, "id").unwrap())
        .unwrap();
    assert_eq!(audited["status"], "approved");
    assert!(audited["consumed_at"].is_null());
    assert!(store
        .resume_decision_request(&first, text(&pending, "id").unwrap())
        .is_err());
    assert!(store
        .resume_decision_request(&second, text(&approved, "id").unwrap())
        .is_err());
    assert_idle(&mut store);
}
#[test]
fn supplements_keep_applied_unknown_and_not_applied_distinct() {
    let mut store = new_store();
    let sid = session(&mut store);
    let id = running(&mut store, &sid);
    send(
        &mut store,
        json!({"type":"steer_task","task_id":id,"content":"已提交"}),
    );
    let first = store.claim_inputs(&id).unwrap();
    store
        .ack_inputs(
            &id,
            &[first[0].id.clone()],
            InputAcknowledgement::Applied {
                via: "native_request".into(),
            },
        )
        .unwrap();
    send(
        &mut store,
        json!({"type":"steer_task","task_id":id,"content":"提交未确认"}),
    );
    let second = store.claim_inputs(&id).unwrap();
    send(
        &mut store,
        json!({"type":"steer_task","task_id":id,"content":"尚未提交"}),
    );
    store.quiesce_for_shutdown().unwrap();
    let task = record(&store.db, "tasks", &id).unwrap();
    let states: Vec<_> = task["supplements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["status"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["applied", "unknown", "not_applied"]);
    assert!(store.claim_inputs(&id).unwrap().is_empty());
    assert!(store
        .ack_inputs(
            &id,
            &[second[0].id.clone()],
            InputAcknowledgement::Applied {
                via: "codex_turn_steer".into()
            }
        )
        .is_err());
}
#[test]
fn executing_plan_stops_without_dispatching_dependents_and_draft_survives() {
    let mut store = new_store();
    let sid = session(&mut store);
    let plan = draft(&mut store, &sid);
    send(
        &mut store,
        json!({"type":"start_plan","plan_id":plan["id"],"expected_revision":plan["revision"]}),
    );
    let child = store.claim_task().unwrap().unwrap();
    assert_eq!(child["node_id"], "a");
    let other = session(&mut store);
    let saved_draft = draft(&mut store, &other);
    let before_tasks = rows(&store.db, "tasks").unwrap().len();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.cancelled_plans, 1);
    assert_eq!(rows(&store.db, "tasks").unwrap().len(), before_tasks);
    let terminal = record(&store.db, "plans", text(&plan, "id").unwrap()).unwrap();
    assert_eq!(terminal["status"], "cancelled");
    assert!(terminal["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n["status"] == "cancelled"));
    assert_eq!(
        record(&store.db, "plans", text(&saved_draft, "id").unwrap()).unwrap(),
        saved_draft
    );
    assert!(store
        .finish(text(&child, "id").unwrap(), Ok("迟到工具完成".into()))
        .unwrap()
        .is_none());
    assert_idle(&mut store);
    assert_eq!(
        record(&store.db, "plans", text(&plan, "id").unwrap()).unwrap(),
        terminal
    );
}
#[test]
fn coordinator_results_cannot_create_plans_after_stop() {
    let mut store = new_store();
    let sid = session(&mut store);
    send(
        &mut store,
        json!({"type":"send_message","session_id":sid,"content":"运行中的规划"}),
    );
    let active = store.claim_coordinator_job().unwrap().unwrap();
    send(
        &mut store,
        json!({"type":"send_message","session_id":sid,"content":"排队中的规划"}),
    );
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.cancelled_coordinator_jobs, 2);
    assert!(store
        .complete_coordinator_job(text(&active, "id").unwrap(), Ok(plan_result()))
        .unwrap()
        .is_empty());
    assert!(rows(&store.db, "plans").unwrap().is_empty());
    assert_idle(&mut store);
}
#[test]
fn cleanup_pending_and_unknown_are_preserved_until_actual_worker_ack() {
    let mut store = new_store();
    let sid = session(&mut store);
    let pending = running(&mut store, &sid);
    store.set_execution_cleanup(&pending, true).unwrap();
    let other = session(&mut store);
    let unknown = running(&mut store, &other);
    store.set_execution_cleanup(&unknown, true).unwrap();
    store.mark_execution_cleanup_unknown(&unknown).unwrap();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.cleanup_pending, 1);
    assert_eq!(report.cleanup_unknown, 1);
    assert!(!report.process_cleanup_confirmed);
    store.set_execution_cleanup(&pending, false).unwrap();
    let settled = store.quiesce_for_shutdown().unwrap();
    assert_eq!(settled.cleanup_pending, 0);
    assert_eq!(settled.cleanup_unknown, 1);
    assert!(!settled.process_cleanup_confirmed);
    assert_eq!(
        record(&store.db, "tasks", &pending).unwrap()["status"],
        "cancelled"
    );
    assert_eq!(
        record(&store.db, "tasks", &unknown).unwrap()["execution_cleanup"],
        "unknown"
    );
}
#[test]
fn database_failure_rolls_back_entire_stop_including_events_and_approvals() {
    let mut store = new_store();
    let sid = session(&mut store);
    let task = running(&mut store, &sid);
    store
        .create_decision_request(&task, "question", "保留哪些文件？", None, 900)
        .unwrap();
    let before = store.snapshot().unwrap();
    store.db.execute_batch("CREATE TRIGGER reject_shutdown_delivery BEFORE INSERT ON deliveries BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
    assert!(store.quiesce_for_shutdown().is_err());
    assert_eq!(store.snapshot().unwrap(), before);
    store
        .db
        .execute_batch("DROP TRIGGER reject_shutdown_delivery")
        .unwrap();
    assert_eq!(store.quiesce_for_shutdown().unwrap().cancelled_tasks, 1);
}
#[test]
fn committed_shutdown_survives_store_restart_without_replay() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("shutdown-recovery-{}", new_id()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("state.db");
    let (task, plan_id);
    {
        let mut store = Store::open(&path).unwrap();
        let sid = session(&mut store);
        task = running(&mut store, &sid);
        store.set_execution_cleanup(&task, true).unwrap();
        let other = session(&mut store);
        let plan = draft(&mut store, &other);
        plan_id = text(&plan, "id").unwrap().to_owned();
        send(
            &mut store,
            json!({"type":"start_plan","plan_id":plan_id,"expected_revision":plan["revision"]}),
        );
        send(
            &mut store,
            json!({"type":"send_message","session_id":other,"content":"未完成编排"}),
        );
        assert!(store.claim_coordinator_job().unwrap().is_some());
        store.quiesce_for_shutdown().unwrap();
    }
    {
        let mut reopened = Store::open(&path).unwrap();
        assert_eq!(
            record(&reopened.db, "tasks", &task).unwrap()["status"],
            "cancelled"
        );
        assert_eq!(
            record(&reopened.db, "tasks", &task).unwrap()["execution_cleanup"],
            "unknown"
        );
        assert_eq!(
            record(&reopened.db, "plans", &plan_id).unwrap()["status"],
            "cancelled"
        );
        assert_idle(&mut reopened);
        assert!(!reopened.quiesce_for_shutdown().unwrap().changed());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn pending_and_running_plan_suggestions_are_discarded_but_ready_draft_is_preserved() {
    let mut store = new_store();
    let first = session(&mut store);
    let ready_plan = draft(&mut store, &first);
    let ready = send(
        &mut store,
        json!({"type":"request_plan_change","plan_id":ready_plan["id"],"expected_revision":ready_plan["revision"],"content":"补充测试"}),
    );
    let job = store.claim_coordinator_job().unwrap().unwrap();
    store
        .complete_coordinator_job(
            text(&job, "id").unwrap(),
            Ok(json!({"action":"change","message":"等待人工确认","nodes":plan_result()["nodes"]})),
        )
        .unwrap();
    let ready_before = record(
        &store.db,
        "plan_changes",
        text(&ready, "change_id").unwrap(),
    )
    .unwrap();
    assert_eq!(ready_before["status"], "ready");
    let second = session(&mut store);
    let active_plan = draft(&mut store, &second);
    let active = send(
        &mut store,
        json!({"type":"request_plan_change","plan_id":active_plan["id"],"expected_revision":active_plan["revision"],"content":"正在生成"}),
    );
    let active_job = store.claim_coordinator_job().unwrap().unwrap();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.discarded_plan_changes, 1);
    assert_eq!(
        record(
            &store.db,
            "plan_changes",
            text(&active, "change_id").unwrap()
        )
        .unwrap()["status"],
        "discarded"
    );
    assert_eq!(
        record(
            &store.db,
            "plan_changes",
            text(&ready, "change_id").unwrap()
        )
        .unwrap(),
        ready_before
    );
    assert!(store
        .complete_coordinator_job(
            text(&active_job, "id").unwrap(),
            Ok(json!({"action":"change","message":"迟到","nodes":plan_result()["nodes"]}))
        )
        .unwrap()
        .is_empty());
    assert_idle(&mut store);
}

#[test]
fn credential_intents_are_counted_not_claimed_or_relabelled_by_sql_shutdown() {
    use crate::capabilities::{
        providers as provider_validation,
        vault::{FreshCredential, SecretBytes},
    };
    let mut store = new_store();
    use super::super::providers::CredentialOutcome;
    let mut ids = Vec::new();
    for index in 0..3 {
        let receipt=store.provider_command(&json!({"id":new_id(),"command":{"type":"save_provider","definition":{"name":format!("凭据夹具{index}"),"base_url":"https://fixture.invalid/v1","models_endpoint":"","model":"fixture","allow_insecure_http":false}}}),&[]).unwrap();
        let provider = store
            .provider_record(text(&receipt, "provider_id").unwrap())
            .unwrap();
        let binding =
            provider_validation::credential_binding(&provider.id, &provider.definition).unwrap();
        let fresh = FreshCredential::new(
            &binding,
            SecretBytes::new(b"shutdown-secret-canary".to_vec()).unwrap(),
        );
        let id = new_id();
        store
            .begin_provider_credential_set(&id, &provider.id, provider.revision, fresh.reference())
            .unwrap();
        if index > 0 {
            assert!(store.claim_credential_operation(&id).unwrap().is_some());
        }
        if index == 2 {
            store
                .finish_credential_operation(&id, CredentialOutcome::Unknown)
                .unwrap();
        }
        ids.push(id);
    }
    let before: Vec<_> = ids
        .iter()
        .map(|id| record(&store.db, "credential_operations", id).unwrap())
        .collect();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(
        (
            report.credential_operations_pending,
            report.credential_operations_inflight,
            report.credential_operations_unknown
        ),
        (1, 1, 1)
    );
    assert!(!report.process_cleanup_confirmed);
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("shutdown-secret-canary"));
    for (id, previous) in ids.iter().zip(before) {
        assert_eq!(
            record(&store.db, "credential_operations", id).unwrap(),
            previous
        );
    }
    store
        .finish_credential_operation(&ids[1], CredentialOutcome::Succeeded)
        .unwrap();
    assert_eq!(
        record(&store.db, "credential_operations", &ids[1]).unwrap()["status"],
        "completed"
    );
}

#[test]
fn in_flight_external_call_preserves_ownership_and_late_factual_result_only() {
    use harness_core::McpToolDefinition;
    let mut store = new_store();
    let mut command = |value: Value| {
        store
            .capability_command(&json!({"id":new_id(),"command":value}), &[])
            .unwrap()
    };
    let server = command(
        json!({"type":"save_mcp_server","definition":{"name":"只记录不启动夹具","program":std::env::current_exe().unwrap(),"args":[],"protocol":"modern2026","enabled":false,"credential_refs":{}}}),
    );
    let server = command(
        json!({"type":"set_capability_enabled","kind":"mcp","id":server["capability_id"],"expected_revision":server["revision"],"enabled":true}),
    );
    let server = command(
        json!({"type":"probe_mcp_server","id":server["capability_id"],"expected_revision":server["revision"],"confirm_start":true}),
    );
    let server_id = text(&server, "capability_id").unwrap();
    let revision = server["revision"].as_u64().unwrap();
    assert!(store
        .claim_pending_probe(server_id, revision)
        .unwrap()
        .is_some());
    store
        .finish_probe(
            server_id,
            revision,
            Ok(vec![McpToolDefinition {
                name: "fixture_echo".into(),
                description: "仅 Store 回归，不启动进程".into(),
                input_schema: json!({"type":"object"}),
                schema_digest: String::new(),
                annotations: json!({}),
            }]),
            &[],
        )
        .unwrap();
    let sid = session(&mut store);
    let mut configured = settings();
    configured.execution_controls.capabilities.mcp_servers = vec![server_id.into()];
    configured.execution_controls.permissions.managed_tools = harness_core::PermissionMode::Allow;
    let receipt=store.command(json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"shutdown-fixture"},"command":{"type":"run_agent","session_id":sid,"content":"显式能力任务"}}),&configured).unwrap().0;
    let task = text(&receipt, "task_id").unwrap();
    assert_eq!(store.claim_task().unwrap().unwrap()["id"], task);
    let call = store
        .begin_capability_call(task, server_id, revision, "fixture_echo", &"a".repeat(64))
        .unwrap();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.external_calls_sending, 1);
    assert_eq!(report.cleanup_pending, 1);
    assert_eq!(
        record(&store.db, "capability_calls", &call).unwrap()["status"],
        "sending"
    );
    assert!(store
        .finish_capability_call(&call, "completed", None)
        .unwrap());
    assert_eq!(
        record(&store.db, "tasks", task).unwrap()["status"],
        "cancelled"
    );
    assert!(!store
        .finish_capability_call(&call, "unknown", None)
        .unwrap());
    assert!(store
        .begin_capability_call(task, server_id, revision, "fixture_echo", &"a".repeat(64))
        .is_err());
}

fn qq_route(store: &mut Store) -> (crate::qq::persistence::Identity, String, u64) {
    use crate::qq::persistence as q;
    let now = crate::qq::runtime::now_ms();
    let identity = q::Identity {
        app_id: "shutdown_app".into(),
        sender_open_id: "shutdown_sender".into(),
        kind: q::ConversationKind::User,
        target_id: "shutdown_sender".into(),
    };
    let tx = store.db.transaction().unwrap();
    let ticket = q::issue_pairing(&tx, &identity.app_id, now, 60).unwrap();
    let pending = q::redeem_pairing(&tx, &identity, &ticket.code, now).unwrap();
    q::confirm_binding(
        &tx,
        &pending.principal_id,
        pending.revision,
        &identity,
        json!({"session_ids":[],"permissions":{"view":true}}),
        now + 3_600_000,
        now - 31_000,
    )
    .unwrap();
    let accepted = q::accept_input(
        &tx,
        &q::Inbound {
            identity: identity.clone(),
            message_id: "shutdown_input".into(),
            event_aliases: vec!["shutdown_alias".into()],
            canonical_input: json!({"kind":"command","content":"/会话"}),
            received_at_ms: now,
        },
        now,
    )
    .unwrap();
    tx.commit().unwrap();
    (identity, accepted.route_id.unwrap(), now)
}

#[test]
fn qq_pending_inputs_and_sends_are_stopped_without_ack_or_replay_unknown_sends_untouched() {
    use crate::qq::persistence as q;
    let mut store = new_store();
    let (_, route, now) = qq_route(&mut store);
    let (sent_later, retry_later, pending, unknown);
    {
        let tx = store.db.transaction().unwrap();
        q::enqueue_outbox(
            &tx,
            &route,
            "send-success",
            q::OutboxKind::Completion,
            "状态一",
            now,
        )
        .unwrap();
        sent_later = q::claim_outbox(&tx, now).unwrap().unwrap();
        q::enqueue_outbox(
            &tx,
            &route,
            "send-retry",
            q::OutboxKind::Completion,
            "状态二",
            now,
        )
        .unwrap();
        retry_later = q::claim_outbox(&tx, now).unwrap().unwrap();
        q::enqueue_outbox(
            &tx,
            &route,
            "send-unknown",
            q::OutboxKind::Completion,
            "状态三",
            now,
        )
        .unwrap();
        unknown = q::claim_outbox(&tx, now).unwrap().unwrap();
        q::finalize_outbox(
            &tx,
            &unknown.record.id,
            unknown.record.claim_nonce.as_deref().unwrap(),
            q::DeliveryOutcome::Unknown,
            now,
        )
        .unwrap();
        pending = q::enqueue_outbox(
            &tx,
            &route,
            "not-sent",
            q::OutboxKind::Completion,
            "状态四",
            now,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.rejected_remote_inputs, 1);
    assert_eq!(report.stopped_remote_outbox, 1);
    assert_eq!(report.remote_sending, 2);
    assert_eq!(report.remote_unknown, 1);
    assert_eq!(
        count(&store.db, "SELECT count(*) FROM qq_outbox").unwrap(),
        4,
        "no shutdown ACK created"
    );
    assert!(q::pending_inputs(&store.db, 128).unwrap().is_empty());
    let receipt: String = store
        .db
        .query_row("SELECT receipt FROM qq_inbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&receipt).unwrap()["reason"],
        STOP_REASON
    );
    assert_eq!(q::outbox(&store.db, &pending.id).unwrap().status, "failed");
    assert_eq!(
        q::outbox(&store.db, &sent_later.record.id)
            .unwrap()
            .claim_nonce,
        sent_later.record.claim_nonce
    );
    {
        let tx = store.db.transaction().unwrap();
        assert!(q::claim_outbox(&tx, now).unwrap().is_none());
        assert_eq!(
            q::finalize_outbox(
                &tx,
                &sent_later.record.id,
                sent_later.record.claim_nonce.as_deref().unwrap(),
                q::DeliveryOutcome::Sent {
                    platform_id: "fixture-confirmed".into()
                },
                now
            )
            .unwrap()
            .status,
            "sent"
        );
        assert_eq!(
            q::finalize_outbox(
                &tx,
                &retry_later.record.id,
                retry_later.record.claim_nonce.as_deref().unwrap(),
                q::DeliveryOutcome::RateLimited {
                    retry_at_ms: now + 1000
                },
                now
            )
            .unwrap()
            .status,
            "pending"
        );
        tx.commit().unwrap();
    }
    let final_report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(final_report.stopped_remote_outbox, 1);
    assert_eq!(final_report.remote_unknown, 1);
    assert_eq!(final_report.remote_sending, 0);
    assert!(!final_report.process_cleanup_confirmed);
    assert!(!store.quiesce_for_shutdown().unwrap().changed());
    let tx = store.db.transaction().unwrap();
    assert!(q::claim_outbox(&tx, now + 2000).unwrap().is_none());
}

#[test]
fn actual_qq_coordinator_shutdown_suppresses_restart_bridge_without_revoking_binding() {
    use crate::qq::{
        persistence as q,
        policy::{conservative_controls, RemoteGrant, RemotePermissions},
    };
    let mut store = new_store();
    let sid = session(&mut store);
    let cfg=store.qq_admin_command(&json!({"id":new_id(),"command":{"type":"configure","config_id":null,"expected_revision":null,"app_id":"shutdown-app","enabled":false,"reply_policy":{"window_ms":300000,"max_user":4,"max_group":5}}}),&settings(),true).unwrap();
    assert_eq!(cfg["status"], "completed");
    let cfg = store.qq_config().unwrap().unwrap();
    store.qq_admin_command(&json!({"id":new_id(),"command":{"type":"configure","config_id":cfg.id,"expected_revision":cfg.revision,"app_id":cfg.app_id,"enabled":true,"reply_policy":cfg.reply_policy}}),&settings(),true).unwrap();
    let cfg = store.qq_config().unwrap().unwrap();
    let identity = q::Identity {
        app_id: cfg.app_id.clone(),
        sender_open_id: "owner-peer".into(),
        kind: q::ConversationKind::User,
        target_id: "owner-peer".into(),
    };
    let now = crate::qq::runtime::now_ms();
    let pending = {
        let tx = store.db.transaction().unwrap();
        let ticket = q::issue_pairing(&tx, &identity.app_id, now, 60).unwrap();
        let p = q::redeem_pairing(&tx, &identity, &ticket.code, now).unwrap();
        tx.commit().unwrap();
        p
    };
    let grant = RemoteGrant {
        session_ids: vec![sid.clone()],
        permissions: RemotePermissions {
            view: true,
            run: true,
            orchestrate: true,
            ..Default::default()
        },
        controls: conservative_controls(),
        expires_at_ms: now + 600000,
    };
    store.qq_admin_command(&json!({"id":new_id(),"command":{"type":"confirm_binding","principal_id":pending.principal_id,"expected_revision":pending.revision,"identity":identity,"grant":grant}}),&settings(),true).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    for content in [
        format!("/选择 {sid}"),
        "/编排 停止前已接收的规划".into(),
        "直接任务的既有结果".into(),
    ] {
        let now = crate::qq::runtime::now_ms();
        let tx = store.db.transaction().unwrap();
        q::accept_input(
            &tx,
            &q::Inbound {
                identity: identity.clone(),
                message_id: new_id(),
                event_aliases: vec![],
                canonical_input: json!({"kind":"command","content":content}),
                received_at_ms: now + 30000,
            },
            now,
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(
            store
                .qq_process_input(&cfg.id, cfg.revision, &settings())
                .unwrap()
                .0
        );
    }
    let job = store.claim_coordinator_job().unwrap().unwrap();
    assert_eq!(job["source"]["channel_id"], "qq");
    let completed = store.claim_task().unwrap().unwrap();
    store
        .finish(
            text(&completed, "id").unwrap(),
            Ok("已确认完成，不受投递停止影响".into()),
        )
        .unwrap();
    {
        let tx = store.db.transaction().unwrap();
        crate::qq::notifications::bridge(&tx, crate::qq::runtime::now_ms()).unwrap();
        tx.commit().unwrap();
    }
    let delivery = rows(&store.db, "deliveries").unwrap().pop().unwrap();
    assert_eq!(delivery["status"], "queued_remote");
    let before = q::binding(&store.db, &pending.principal_id).unwrap();
    let count_before = count(&store.db, "SELECT count(*) FROM qq_outbox").unwrap();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.cancelled_coordinator_jobs, 1);
    assert_eq!(report.suppressed_remote_notifications, 1);
    assert_eq!(
        record(&store.db, "tasks", text(&completed, "id").unwrap()).unwrap()["status"],
        "completed"
    );
    assert_eq!(
        record(&store.db, "deliveries", text(&delivery, "id").unwrap()).unwrap()["status"],
        "failed"
    );
    let marker: String = store
        .db
        .query_row(
            "SELECT state FROM qq_notifications WHERE logical_key=?1",
            [format!("coordinator:{}", text(&job, "id").unwrap())],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marker, "not_queued");
    assert!(store
        .complete_coordinator_job(text(&job, "id").unwrap(), Ok(plan_result()))
        .unwrap()
        .is_empty());
    // Simulate a future bridge pass, not a network sender. It must find the
    // durable suppression marker rather than enqueue a new completion notice.
    let tx = store.db.transaction().unwrap();
    crate::qq::notifications::bridge(&tx, now + 1).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        count(&store.db, "SELECT count(*) FROM qq_outbox").unwrap(),
        count_before
    );
    let after = q::binding(&store.db, &pending.principal_id).unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.status, "active");
    assert!(!store.quiesce_for_shutdown().unwrap().changed());
}

#[test]
fn qq_failure_rolls_back_main_stop_and_inbox_receipt_in_same_transaction() {
    use crate::qq::persistence as q;
    let mut store = new_store();
    let sid = session(&mut store);
    running(&mut store, &sid);
    let (_, route, now) = qq_route(&mut store);
    let tx = store.db.transaction().unwrap();
    q::enqueue_outbox(
        &tx,
        &route,
        "pending-before-fault",
        q::OutboxKind::Completion,
        "状态",
        now,
    )
    .unwrap();
    tx.commit().unwrap();
    let before = store.snapshot().unwrap();
    store.db.execute_batch("CREATE TRIGGER refuse_shutdown_outbox BEFORE UPDATE ON qq_outbox BEGIN SELECT RAISE(ABORT,'fixture stop rollback'); END;").unwrap();
    assert!(store.quiesce_for_shutdown().is_err());
    assert_eq!(store.snapshot().unwrap(), before);
    assert_eq!(q::pending_inputs(&store.db, 128).unwrap().len(), 1);
    assert_eq!(
        count(
            &store.db,
            "SELECT count(*) FROM qq_outbox WHERE status='pending'"
        )
        .unwrap(),
        1
    );
    store
        .db
        .execute_batch("DROP TRIGGER refuse_shutdown_outbox")
        .unwrap();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(
        (
            report.cancelled_tasks,
            report.rejected_remote_inputs,
            report.stopped_remote_outbox
        ),
        (1, 1, 1)
    );
}

#[test]
fn successful_terminal_result_remains_factual_when_unsent_callback_is_stopped() {
    let mut store = new_store();
    let sid = session(&mut store);
    let id = running(&mut store, &sid);
    store
        .finish(&id, Ok("真实已完成的验证结果".into()))
        .unwrap();
    let task = record(&store.db, "tasks", &id).unwrap();
    let report = store.quiesce_for_shutdown().unwrap();
    assert_eq!(report.cancelled_tasks, 0);
    assert_eq!(report.stopped_deliveries, 1);
    assert_eq!(record(&store.db, "tasks", &id).unwrap(), task);
    assert_idle(&mut store);
}
