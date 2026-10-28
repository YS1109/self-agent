use super::*;
use crate::qq::{
    persistence::{ConversationKind, Identity, Inbound},
    policy::{conservative_controls, RemoteGrant, RemotePermissions},
};
fn settings() -> Settings {
    Settings {
        execution_mode: "native".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "local-fixture-not-called".into(),
        api_key: String::new(),
        main_executor: "native".into(),
        ..Settings::default()
    }
}
fn owner(store: &mut Store, command: Value) -> Value {
    store.command(json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"local"},"command":command}),&settings()).unwrap().0
}
fn setup() -> (Store, config::Config, String, persistence::Binding) {
    let mut s = Store::open(Path::new(":memory:")).unwrap();
    let session = owner(
        &mut s,
        json!({"type":"create_session","title":"QQ 测试","project_path":std::env::temp_dir()}),
    )["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let receipt=s.qq_admin_command(&json!({"id":new_id(),"command":{"type":"configure","config_id":null,"expected_revision":null,"app_id":"app_1","enabled":false,"reply_policy":{"window_ms":300000,"max_user":4,"max_group":5}}}),&settings(),true).unwrap();
    assert_eq!(receipt["status"], "completed");
    let c = s.qq_config().unwrap().unwrap();
    let receipt=s.qq_admin_command(&json!({"id":new_id(),"command":{"type":"configure","config_id":c.id,"expected_revision":c.revision,"app_id":"app_1","enabled":true,"reply_policy":c.reply_policy}}),&settings(),true).unwrap();
    assert_eq!(receipt["status"], "completed");
    let c = s.qq_config().unwrap().unwrap();
    let tx = s.db.transaction().unwrap();
    let who = Identity {
        app_id: c.app_id.clone(),
        sender_open_id: "peer".into(),
        kind: ConversationKind::User,
        target_id: "peer".into(),
    };
    let ticket = persistence::issue_pairing(&tx, &c.app_id, millis(), 60).unwrap();
    let pending = persistence::redeem_pairing(&tx, &who, &ticket.code, millis()).unwrap();
    tx.commit().unwrap();
    let grant = RemoteGrant {
        session_ids: vec![session.clone()],
        permissions: RemotePermissions {
            view: true,
            run: true,
            cancel: true,
            answer: true,
            create_session: true,
            orchestrate: true,
            ..Default::default()
        },
        controls: conservative_controls(),
        expires_at_ms: millis() + 600000,
    };
    let receipt=s.qq_admin_command(&json!({"id":new_id(),"command":{"type":"confirm_binding","principal_id":pending.principal_id,"expected_revision":pending.revision,"identity":who,"grant":grant}}),&settings(),true).unwrap();
    assert_eq!(receipt["status"], "completed");
    let b = persistence::binding(&s.db, &pending.principal_id).unwrap();
    // Fixture QQ clock is 30s ahead; ensure a fresh message is after confirmation.
    std::thread::sleep(std::time::Duration::from_millis(2));
    (s, c, session, b)
}
fn input(s: &mut Store, b: &persistence::Binding, content: &str) -> persistence::AcceptedInput {
    let tx = s.db.transaction().unwrap();
    let a = persistence::accept_input(
        &tx,
        &Inbound {
            identity: b.identity.clone(),
            message_id: new_id(),
            event_aliases: vec![],
            canonical_input: json!({"kind":"command","content":content}),
            received_at_ms: millis() + 30_000,
        },
        millis(),
    )
    .unwrap();
    tx.commit().unwrap();
    a
}
fn run(s: &mut Store, c: &config::Config) {
    assert!(
        s.qq_process_input(&c.id, c.revision, &settings())
            .unwrap()
            .0
    );
}
#[test]
fn real_command_transaction_preserves_source_controls_selection_and_idempotence() {
    let (mut s, c, session, b) = setup();
    input(&mut s, &b, &format!("/选择 {session}"));
    run(&mut s, &c);
    let a = input(&mut s, &b, "执行本地模拟");
    run(&mut s, &c);
    let receipt = persistence::inbox(&s.db, &a.inbox_id)
        .unwrap()
        .receipt
        .unwrap();
    let task = record(&s.db, "tasks", receipt["task_id"].as_str().unwrap()).unwrap();
    assert_eq!(task["source"]["conversation_id"], a.route_id.unwrap());
    assert_eq!(task["source"]["actor_id"], b.principal_id);
    assert_eq!(task["controls"]["permissions"]["run_commands"], "deny");
    assert_eq!(task["controls"]["permissions"]["write_files"], "ask");
    assert!(s.check_task_remote(task["id"].as_str().unwrap()).is_ok());
    assert!(
        !s.qq_process_input(&c.id, c.revision, &settings())
            .unwrap()
            .0
    );
    assert_eq!(rows(&s.db, "tasks").unwrap().len(), 1);
}
#[test]
fn real_new_session_acl_and_ack_rollback_together_on_enqueue_fault() {
    let (mut s, c, session, b) = setup();
    let a = input(&mut s, &b, &format!("/新建 {session} 新会话"));
    s.db.execute_batch("CREATE TRIGGER fail_qq_outbox BEFORE INSERT ON qq_outbox BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    run(&mut s, &c);
    assert_eq!(rows(&s.db, "sessions").unwrap().len(), 1);
    let commands: u64 =
        s.db.query_row("SELECT count(*) FROM commands", [], |r| r.get(0))
            .unwrap();
    assert_eq!(commands, 1);
    let count: u64 =
        s.db.query_row("SELECT count(*) FROM qq_session_acl", [], |r| r.get(0))
            .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        persistence::inbox(&s.db, &a.inbox_id)
            .unwrap()
            .receipt
            .unwrap()["status"],
        "rejected"
    );
}
#[test]
fn disable_revokes_running_work_first_reason_and_no_reenable_revival() {
    let (mut s, c, session, b) = setup();
    input(&mut s, &b, &format!("/选择 {session}"));
    run(&mut s, &c);
    let a = input(&mut s, &b, "测试撤权");
    run(&mut s, &c);
    let receipt = persistence::inbox(&s.db, &a.inbox_id)
        .unwrap()
        .receipt
        .unwrap();
    let id = receipt["task_id"].as_str().unwrap();
    let before = record(&s.db, "tasks", id).unwrap();
    assert!(task_active(&before));
    let r=s.qq_admin_command(&json!({"id":new_id(),"command":{"type":"configure","config_id":c.id,"expected_revision":c.revision,"app_id":c.app_id,"enabled":false,"reply_policy":c.reply_policy}}),&settings(),true).unwrap();
    assert_eq!(r["status"], "completed");
    let task = record(&s.db, "tasks", id).unwrap();
    assert_eq!(task["status"], "cancelled");
    assert_eq!(task["stop_reason"], "remote_permission_revoked");
    assert!(s.check_task_remote(id).is_err());
    assert!(!rows(&s.db, "deliveries").unwrap().is_empty());
}
#[test]
fn ingest_ready_input_and_sequence_commit_atomically_conflicts_do_not_ack() {
    let (mut s, c, _, _) = setup();
    s.qq_ingest(
        &c.id,
        c.revision,
        &json!({"op":0,"t":"READY","d":{"user":{"id":"bot"}}}),
        &transport::Checkpoint {
            session_id: "session".into(),
            seq: 1,
        },
    )
    .unwrap();
    let payload = json!({"op":0,"t":"C2C_MESSAGE_CREATE","d":{"id":"message","author":{"user_openid":"peer"},"timestamp":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),"content":"/帮助"}});
    s.qq_ingest(
        &c.id,
        c.revision,
        &payload,
        &transport::Checkpoint {
            session_id: "session".into(),
            seq: 2,
        },
    )
    .unwrap();
    assert_eq!(s.qq_checkpoint(&c.id, c.revision).unwrap().unwrap().seq, 2);
    let mut conflict = payload.clone();
    conflict["d"]["content"] = json!("changed");
    assert!(s
        .qq_ingest(
            &c.id,
            c.revision,
            &conflict,
            &transport::Checkpoint {
                session_id: "session".into(),
                seq: 3
            }
        )
        .is_err());
    assert_eq!(s.qq_checkpoint(&c.id, c.revision).unwrap().unwrap().seq, 2);
    s.qq_ingest(
        &c.id,
        c.revision,
        &json!({"op":0,"t":"C2C_MESSAGE_CREATE","d":{}}),
        &transport::Checkpoint {
            session_id: "session".into(),
            seq: 3,
        },
    )
    .unwrap();
    assert_eq!(s.qq_checkpoint(&c.id, c.revision).unwrap().unwrap().seq, 3);
}
#[test]
fn source_guards_ignore_passive_window_but_reject_actor_and_expired_binding() {
    let (mut s, _, _, b) = setup();
    let a = input(&mut s, &b, "/帮助");
    let mut value = json!({"source":{"channel_id":"qq","actor_id":b.principal_id,"conversation_id":a.route_id}});
    assert!(check_source(&s.db, &value).is_ok());
    let route = value["source"]["conversation_id"].as_str().unwrap();
    let mut body = persistence::route(&s.db, route).unwrap();
    body.expires_at_ms = 0;
    s.db.execute(
        "UPDATE qq_reply_routes SET expires_at_ms=0,body=?1 WHERE route_id=?2",
        params![serde_json::to_string(&body).unwrap(), route],
    )
    .unwrap();
    assert!(check_source(&s.db, &value).is_ok());
    value["source"]["actor_id"] = json!("forged");
    assert!(check_source(&s.db, &value).is_err());
    assert!(check_source(&s.db, &json!({"source":{"channel_id":"desktop"}})).is_ok());
}

#[test]
fn real_question_answer_records_remote_actor_and_never_approves() {
    let (mut s, c, session, b) = setup();
    input(&mut s, &b, &format!("/选择 {session}"));
    run(&mut s, &c);
    let a = input(&mut s, &b, "问题测试");
    run(&mut s, &c);
    let receipt = persistence::inbox(&s.db, &a.inbox_id)
        .unwrap()
        .receipt
        .unwrap();
    let task = receipt["task_id"].as_str().unwrap();
    s.claim_task().unwrap().unwrap();
    let question = s
        .create_decision_request(task, "question", "请选择下一步", None, 60)
        .unwrap();
    let id = question["id"].as_str().unwrap();
    input(&mut s, &b, &format!("/回答 {id} 0 只读检查"));
    run(&mut s, &c);
    let answered = s.decision_request(id).unwrap();
    assert_eq!(answered["status"], "answered");
    assert_eq!(answered["decision_actor"], format!("qq:{}", b.principal_id));
    assert_eq!(
        record(&s.db, "tasks", task).unwrap()["status"],
        "waiting_input"
    );
    s.resume_decision_request(task, id).unwrap();
    let approval = s
        .create_decision_request(
            task,
            "approval",
            "批准写入",
            Some(json!({"path":"fixture.txt","content_sha256":"fixture"})),
            60,
        )
        .unwrap();
    let id = approval["id"].as_str().unwrap();
    let a = input(&mut s, &b, &format!("/回答 {id} 0 批准"));
    run(&mut s, &c);
    assert_eq!(
        persistence::inbox(&s.db, &a.inbox_id)
            .unwrap()
            .receipt
            .unwrap()["status"],
        "rejected"
    );
    assert_eq!(s.decision_request(id).unwrap()["status"], "pending");
}
#[test]
fn actual_coordinator_snapshots_are_restricted_and_revoke_prevents_late_work() {
    let (mut s, c, session, b) = setup();
    input(&mut s, &b, &format!("/选择 {session}"));
    run(&mut s, &c);
    let a = input(&mut s, &b, "/编排 本地模拟计划");
    run(&mut s, &c);
    let receipt = persistence::inbox(&s.db, &a.inbox_id)
        .unwrap()
        .receipt
        .unwrap();
    let job = record(
        &s.db,
        "coordinator_jobs",
        receipt["job_id"].as_str().unwrap(),
    )
    .unwrap();
    for role in job["agents"].as_array().unwrap() {
        assert_eq!(role["controls"]["permissions"]["run_commands"], "deny");
        assert_eq!(role["controls"]["permissions"]["managed_tools"], "deny");
    }
    let result=s.qq_admin_command(&json!({"id":new_id(),"command":{"type":"revoke_binding","principal_id":b.principal_id,"expected_revision":b.revision}}),&settings(),true).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(
        record(
            &s.db,
            "coordinator_jobs",
            receipt["job_id"].as_str().unwrap()
        )
        .unwrap()["status"],
        "cancelled"
    );
}

#[test]
fn actual_vault_intent_wrappers_are_qq_scoped_and_public_snapshot_redacted() {
    use crate::capabilities::vault::{FreshCredential, SecretBytes, VaultBinding};
    let (mut s, c, _, _) = setup();
    let binding = crate::qq::credential_binding(
        &c.id,
        &config::credential_binding_digest(&c.app_id).unwrap(),
    )
    .unwrap();
    let material = FreshCredential::new(
        &binding,
        SecretBytes::new(b"QQ_FIXTURE_SECRET_CANARY".to_vec()).unwrap(),
    );
    let op = new_id();
    s.qq_begin_set(&op, &c.id, c.revision, material.reference())
        .unwrap();
    assert!(s.qq_existing_set(&op, &c.id, c.revision).unwrap().is_some());
    let work = s.qq_claim_credential(&op).unwrap().unwrap();
    assert_eq!(work.config_id, c.id);
    s.qq_finish_credential(&op, credentials::Outcome::Succeeded)
        .unwrap();
    let snapshot = s.qq_snapshot(true).unwrap().to_string();
    assert!(!snapshot.contains("QQ_FIXTURE_SECRET_CANARY"));
    assert!(!snapshot.contains(material.reference().as_str()));
    assert!(!snapshot.contains("credential_ref"));
    assert_eq!(
        s.qq_credential_operation(&op).unwrap().unwrap().owner_kind,
        "qq"
    );
    assert!(s.qq_credential_operation(&new_id()).unwrap().is_none());
    let c = s.qq_config().unwrap().unwrap();
    let wrong = VaultBinding::provider(&c.id, "api_key", "https://example.invalid/v1").unwrap();
    let wrong = FreshCredential::new(&wrong, SecretBytes::new(b"fixture".to_vec()).unwrap());
    assert!(s
        .qq_begin_set(&new_id(), &c.id, c.revision, wrong.reference())
        .is_err());
    let delete = new_id();
    s.qq_begin_delete(&delete, &c.id, c.revision).unwrap();
    assert!(!s.qq_config().unwrap().unwrap().enabled);
    assert!(s.qq_config().unwrap().unwrap().credential_ref().is_none());
    assert!(s.qq_claim_credential(&delete).unwrap().is_some());
    s.qq_finish_credential(&delete, credentials::Outcome::Unknown)
        .unwrap();
    let cleanup = new_id();
    s.qq_begin_cleanup(&cleanup, &delete).unwrap();
    s.qq_claim_credential(&cleanup).unwrap().unwrap();
    s.qq_finish_credential(&cleanup, credentials::Outcome::Succeeded)
        .unwrap();
    assert_eq!(
        s.qq_credential_operation(&delete).unwrap().unwrap().status,
        "unknown"
    );
}
#[test]
fn real_terminal_delivery_outbox_unknown_does_not_replay_or_change_task_result() {
    let (mut s, c, session, b) = setup();
    input(&mut s, &b, &format!("/选择 {session}"));
    run(&mut s, &c);
    let a = input(&mut s, &b, "终态测试");
    run(&mut s, &c);
    let receipt = persistence::inbox(&s.db, &a.inbox_id)
        .unwrap()
        .receipt
        .unwrap();
    let id = receipt["task_id"].as_str().unwrap();
    s.claim_task().unwrap().unwrap();
    s.finish(id, Ok("RESULT_SECRET_CANARY".into())).unwrap();
    let mut seen = 0;
    while let Some(claim) = s.qq_claim(&c.id, c.revision).unwrap() {
        assert!(!claim.record.payload.contains("RESULT_SECRET_CANARY"));
        assert!(s.qq_allowed(&c.id, c.revision, &claim).unwrap());
        s.qq_finish(&claim, persistence::DeliveryOutcome::Unknown)
            .unwrap();
        seen += 1;
        assert!(seen <= 3);
    }
    assert_eq!(seen, 3);
    assert_eq!(record(&s.db, "tasks", id).unwrap()["status"], "completed");
    let delivery = rows(&s.db, "deliveries")
        .unwrap()
        .into_iter()
        .find(|d| d["task_id"] == id)
        .unwrap();
    assert_eq!(delivery["status"], "unknown");
    assert!(s.qq_claim(&c.id, c.revision).unwrap().is_none());
}

#[test]
fn admin_receipts_never_replay_pairing_code_or_persist_unknown_secret_fields() {
    let (mut s, _, _, _) = setup();
    let envelope = json!({"id":new_id(),"command":{"type":"issue_pairing","ttl_seconds":60}});
    let first = s.qq_admin_command(&envelope, &settings(), true).unwrap();
    let code = first["pairing"]["code"].as_str().unwrap();
    let replay = s.qq_admin_command(&envelope, &settings(), true).unwrap();
    assert!(replay["pairing"]["code"].is_null());
    assert!(!s
        .qq_admin_receipt(envelope["id"].as_str().unwrap())
        .unwrap()
        .unwrap()
        .to_string()
        .contains(code));
    assert!(!s.qq_snapshot(true).unwrap().to_string().contains(code));
    let invalid = json!({"id":new_id(),"command":{"type":"issue_pairing","ttl_seconds":60,"secret":"QQ_ADMIN_SECRET_CANARY"}});
    let receipt = s.qq_admin_command(&invalid, &settings(), true).unwrap();
    assert_eq!(receipt["status"], "rejected");
    let persisted: String =
        s.db.query_row(
            "SELECT digest||receipt FROM qq_admin_commands WHERE id=?1",
            [invalid["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!persisted.contains("QQ_ADMIN_SECRET_CANARY"));
}
#[test]
fn fresh_configuration_revision_tightens_send_without_rewriting_immutable_route() {
    let (mut s, c, _, b) = setup();
    let a = input(&mut s, &b, "/帮助");
    run(&mut s, &c);
    let route = persistence::route(&s.db, a.route_id.as_deref().unwrap()).unwrap();
    let tx = s.db.transaction().unwrap();
    let updated = config::update(
        &tx,
        &c.id,
        c.revision,
        config::ConfigPatch {
            app_id: c.app_id.clone(),
            enabled: true,
            reply_policy: persistence::ReplyPolicy {
                window_ms: 1,
                max_user: 1,
                max_group: 1,
            },
        },
        millis(),
        |_| Ok(true),
    )
    .unwrap();
    tx.commit().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let claim = s.qq_claim(&updated.id, updated.revision).unwrap().unwrap();
    assert!(!s.qq_allowed(&updated.id, updated.revision, &claim).unwrap());
    assert_eq!(
        persistence::route(&s.db, &route.route_id)
            .unwrap()
            .expires_at_ms,
        route.expires_at_ms
    );
    assert!(s.qq_allowed(&c.id, c.revision, &claim).is_err());
}

#[test]
fn changed_default_executor_does_not_block_cancelling_existing_scoped_native_task() {
    let (mut s, c, session, b) = setup();
    input(&mut s, &b, &format!("/选择 {session}"));
    run(&mut s, &c);
    let a = input(&mut s, &b, "原 Native 任务");
    run(&mut s, &c);
    let receipt = persistence::inbox(&s.db, &a.inbox_id)
        .unwrap()
        .receipt
        .unwrap();
    let id = receipt["task_id"].as_str().unwrap();
    assert_eq!(
        record(&s.db, "tasks", id).unwrap()["agent"]["executor"],
        "native"
    );
    let mut changed = settings();
    changed.main_executor = "codex".into();
    for role in &mut changed.agents {
        role.executor = "codex".into();
    }
    let cancel = input(&mut s, &b, &format!("/取消 {id}"));
    assert!(s.qq_process_input(&c.id, c.revision, &changed).unwrap().0);
    assert_ne!(
        persistence::inbox(&s.db, &cancel.inbox_id)
            .unwrap()
            .receipt
            .unwrap()["status"],
        "rejected"
    );
    assert_eq!(record(&s.db, "tasks", id).unwrap()["status"], "cancelled");
    let new = input(&mut s, &b, "不能放宽到 Codex");
    assert!(s.qq_process_input(&c.id, c.revision, &changed).unwrap().0);
    assert_eq!(
        persistence::inbox(&s.db, &new.inbox_id)
            .unwrap()
            .receipt
            .unwrap()["status"],
        "rejected"
    );
}

#[test]
fn future_timestamp_rejection_watermark_and_checkpoint_prevent_upgrade_after_clock_correction() {
    let (mut s, c, session, _) = setup();
    let now = millis();
    let future = now + 3_600_000;
    let point = |seq| transport::Checkpoint {
        session_id: "clock-fixture".into(),
        seq,
    };
    s.qq_ingest_at(
        &c.id,
        c.revision,
        &json!({"op":0,"t":"READY","d":{"session_id":"clock-fixture","user":{"id":"bot"}}}),
        &point(1),
        now,
    )
    .unwrap();
    let mut payload = json!({"op":0,"t":"C2C_MESSAGE_CREATE","d":{"id":"before-binding-clock-skew","author":{"user_openid":"lagged-peer"},"timestamp":chrono::DateTime::from_timestamp_millis(future as i64).unwrap().to_rfc3339(),"content":"/会话"}});
    s.qq_ingest_at(&c.id, c.revision, &payload, &point(2), now)
        .unwrap();
    assert_eq!(s.qq_checkpoint(&c.id, c.revision).unwrap().unwrap().seq, 2);
    assert_eq!(
        s.db.query_row(
            "SELECT latest_platform_ms FROM qq_ignored_inputs",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        future
    );
    assert!(s.qq_snapshot(true).unwrap()["inbox"]
        .as_array()
        .unwrap()
        .is_empty());
    let who = Identity {
        app_id: c.app_id.clone(),
        sender_open_id: "lagged-peer".into(),
        target_id: "lagged-peer".into(),
        kind: ConversationKind::User,
    };
    let tx = s.db.transaction().unwrap();
    let code = persistence::issue_pairing(&tx, &c.app_id, now, 60).unwrap();
    let pending = persistence::redeem_pairing(&tx, &who, &code.code, now).unwrap();
    tx.commit().unwrap();
    let receipt=s.qq_admin_command(&json!({"id":new_id(),"command":{"type":"confirm_binding","principal_id":pending.principal_id,"expected_revision":pending.revision,"identity":who,"grant":{"session_ids":[session],"permissions":{"view":true},"controls":conservative_controls(),"expires_at_ms":future+86_400_000}}}),&settings(),true).unwrap();
    assert_eq!(receipt["status"], "completed");
    assert_eq!(
        persistence::binding(&s.db, &pending.principal_id)
            .unwrap()
            .input_after_ms,
        future
    );
    // Now the same platform timestamp parses normally, but its persisted fence
    // still excludes it. A transport checkpoint is not execution authorization.
    s.qq_ingest_at(&c.id, c.revision, &payload, &point(3), future + 1)
        .unwrap();
    assert_eq!(s.qq_checkpoint(&c.id, c.revision).unwrap().unwrap().seq, 3);
    assert!(s.qq_snapshot(true).unwrap()["inbox"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(
        !s.qq_process_input(&c.id, c.revision, &settings())
            .unwrap()
            .0
    );
    payload["d"]["id"] = json!("genuinely-new-after-fence");
    payload["d"]["timestamp"] = json!(chrono::DateTime::from_timestamp_millis((future + 1) as i64)
        .unwrap()
        .to_rfc3339());
    s.qq_ingest_at(&c.id, c.revision, &payload, &point(4), future + 1)
        .unwrap();
    assert_eq!(
        s.qq_snapshot(true).unwrap()["inbox"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn clock_skew_after_active_binding_never_upgrades_on_corrected_clock_replay() {
    let (mut s, c, _, binding) = setup();
    let now = millis();
    let future = now + 60_000;
    let point = |seq| transport::Checkpoint {
        session_id: "active-clock".into(),
        seq,
    };
    s.qq_ingest_at(
        &c.id,
        c.revision,
        &json!({"op":0,"t":"READY","d":{"session_id":"active-clock","user":{"id":"bot"}}}),
        &point(1),
        now,
    )
    .unwrap();
    let mut payload = json!({"op":0,"t":"C2C_MESSAGE_CREATE","d":{"id":"clock-skew-active","author":{"user_openid":"peer"},"timestamp":chrono::DateTime::from_timestamp_millis(future as i64).unwrap().to_rfc3339(),"content":"/会话"}});
    s.qq_ingest_at(&c.id, c.revision, &payload, &point(2), now)
        .unwrap();
    s.qq_ingest_at(&c.id, c.revision, &payload, &point(3), future + 1)
        .unwrap();
    assert!(s.qq_snapshot(true).unwrap()["inbox"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        persistence::binding(&s.db, &binding.principal_id)
            .unwrap()
            .input_after_ms,
        binding.input_after_ms
    );
    assert_eq!(
        s.qq_snapshot(true).unwrap()["ignored_inputs"]["clock_fault_ms"],
        future
    );
    payload["d"]["id"] = json!("fresh-active");
    payload["d"]["timestamp"] = json!(chrono::DateTime::from_timestamp_millis((future + 1) as i64)
        .unwrap()
        .to_rfc3339());
    s.qq_ingest_at(&c.id, c.revision, &payload, &point(4), future + 1)
        .unwrap();
    assert_eq!(
        s.qq_snapshot(true).unwrap()["inbox"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn clock_anomaly_floor_rolls_back_if_durable_checkpoint_write_fails() {
    let (mut s, c, _, _) = setup();
    let now = millis();
    let future = now + 60_000;
    let point = |seq| transport::Checkpoint {
        session_id: "rollback-clock".into(),
        seq,
    };
    s.qq_ingest_at(
        &c.id,
        c.revision,
        &json!({"op":0,"t":"READY","d":{"session_id":"rollback-clock","user":{"id":"bot"}}}),
        &point(1),
        now,
    )
    .unwrap();
    let payload = json!({"op":0,"t":"C2C_MESSAGE_CREATE","d":{"id":"clock-rollback","author":{"user_openid":"peer"},"timestamp":chrono::DateTime::from_timestamp_millis(future as i64).unwrap().to_rfc3339(),"content":"/会话"}});
    s.db.execute_batch("CREATE TRIGGER reject_qq_checkpoint BEFORE UPDATE ON qq_checkpoint BEGIN SELECT RAISE(ABORT,'fixture checkpoint failure'); END;").unwrap();
    assert!(s
        .qq_ingest_at(&c.id, c.revision, &payload, &point(2), now)
        .is_err());
    assert_eq!(s.qq_checkpoint(&c.id, c.revision).unwrap().unwrap().seq, 1);
    assert_eq!(
        s.qq_snapshot(true).unwrap()["ignored_inputs"]["clock_fault_ms"],
        0
    );
    s.db.execute_batch("DROP TRIGGER reject_qq_checkpoint;")
        .unwrap();
    s.qq_ingest_at(&c.id, c.revision, &payload, &point(2), now)
        .unwrap();
    assert_eq!(
        s.qq_snapshot(true).unwrap()["ignored_inputs"]["clock_fault_ms"],
        future
    );
    assert_eq!(s.qq_checkpoint(&c.id, c.revision).unwrap().unwrap().seq, 2);
}
