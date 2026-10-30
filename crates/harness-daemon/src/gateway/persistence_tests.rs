use super::super::wire::{GatewayCommand, GatewayPermissions};
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
const NOW: u64 = 1_800_000_000_000;
const DEVICE: &str = "11111111-1111-4111-8111-111111111111";
const PRINCIPAL: &str = "22222222-2222-4222-8222-222222222222";
const SESSION: &str = "33333333-3333-4333-8333-333333333333";
const OTHER: &str = "44444444-4444-4444-8444-444444444444";
const TASK: &str = "55555555-5555-4555-8555-555555555555";
const QUESTION: &str = "66666666-6666-4666-8666-666666666666";
struct Context {
    open: AtomicBool,
    controls: harness_core::TaskControls,
    task_session: String,
    question: bool,
}
impl Default for Context {
    fn default() -> Self {
        Self {
            open: AtomicBool::new(true),
            controls: Default::default(),
            task_session: SESSION.into(),
            question: true,
        }
    }
}
impl ResourceContext for Context {
    fn channel_current(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
    fn session_exists(&self, id: &str) -> Result<bool> {
        Ok(matches!(id, SESSION | OTHER))
    }
    fn task_session(&self, id: &str) -> Result<String> {
        if id != TASK {
            bail!("fixture task missing");
        }
        Ok(self.task_session.clone())
    }
    fn task_controls(&self, _: &str) -> Result<harness_core::TaskControls> {
        Ok(self.controls.clone())
    }
    fn question_task(&self, id: &str, revision: u32) -> Result<(String, bool)> {
        if id != QUESTION || revision != 1 {
            bail!("fixture revision mismatch");
        }
        Ok((TASK.into(), self.question))
    }
    fn artifact_task(&self, id: &str) -> Result<String> {
        policy::identifier(id)?;
        Ok(TASK.into())
    }
}
fn channel_fixture() -> TrustedChannel {
    TrustedChannel {
        device_id: DEVICE.into(),
        config_revision: 1,
    }
}
fn database() -> rusqlite::Connection {
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    let tx = db.transaction().unwrap();
    migrate(&tx).unwrap();
    tx.execute_batch("CREATE TABLE fixture_effects(id TEXT PRIMARY KEY);")
        .unwrap();
    tx.commit().unwrap();
    db
}
fn grant() -> GatewayGrant {
    GatewayGrant {
        session_ids: vec![SESSION.into()],
        permissions: GatewayPermissions {
            view: true,
            read_results: true,
            read_artifacts: true,
            run: true,
            cancel: true,
            answer: true,
        },
        controls: serde_json::to_value(harness_core::TaskControls::default()).unwrap(),
        expires_at_ms: NOW + 60000,
    }
}
fn active(tx: &Transaction<'_>, context: &Context) -> Binding {
    let channel = channel_fixture();
    let pairing = issue_pairing(tx, &channel, NOW, 60).unwrap();
    let claim =
        claim_pairing(tx, &channel, PRINCIPAL, &pairing.code, "手机", NOW, context).unwrap();
    confirm_binding(
        tx,
        &channel,
        PRINCIPAL,
        claim.revision,
        "手机",
        &claim.pairing_id,
        grant(),
        NOW,
        context,
    )
    .unwrap()
}
fn request(command: GatewayCommand) -> GatewayRequest {
    GatewayRequest {
        request_id: new_id(),
        principal_id: PRINCIPAL.into(),
        binding_revision: 1,
        rpc: GatewayRpc::Command {
            command_id: new_id(),
            session_id: SESSION.into(),
            command,
        },
    }
}
#[test]
fn migration_is_explicit_transactional_and_idempotent() {
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    {
        let tx = db.transaction().unwrap();
        migrate(&tx).unwrap();
    }
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='gateway_bindings'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
    let tx = db.transaction().unwrap();
    migrate(&tx).unwrap();
    migrate(&tx).unwrap();
    tx.commit().unwrap();
}
#[test]
fn pairing_is_hashed_bounded_single_use_and_exact_owner_confirmation() {
    let mut db = database();
    let tx = db.transaction().unwrap();
    let channel = channel_fixture();
    let context = Context::default();
    for _ in 0..50 {
        assert!(claim_pairing(
            &tx,
            &channel,
            PRINCIPAL,
            &"a".repeat(64),
            "stranger",
            NOW,
            &context
        )
        .is_err());
    }
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM gateway_bindings", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    let pairing = issue_pairing(&tx, &channel, NOW, 60).unwrap();
    let stored: String = tx
        .query_row(
            "SELECT code_hash FROM gateway_pairings WHERE id=?1",
            [&pairing.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(stored, pairing.code);
    let claim = claim_pairing(
        &tx,
        &channel,
        PRINCIPAL,
        &pairing.code,
        "phone",
        NOW,
        &context,
    )
    .unwrap();
    assert_eq!(
        claim_pairing(
            &tx,
            &channel,
            PRINCIPAL,
            &pairing.code,
            "phone",
            NOW,
            &context
        )
        .unwrap()
        .revision,
        0
    );
    assert!(claim_pairing(&tx, &channel, OTHER, &pairing.code, "phone", NOW, &context).is_err());
    assert!(claim_pairing(
        &tx,
        &channel,
        PRINCIPAL,
        &pairing.code,
        "changed",
        NOW,
        &context
    )
    .is_err());
    assert!(confirm_binding(
        &tx,
        &channel,
        PRINCIPAL,
        0,
        "changed",
        &claim.pairing_id,
        grant(),
        NOW,
        &context
    )
    .is_err());
    assert!(confirm_binding(
        &tx,
        &channel,
        PRINCIPAL,
        2,
        "phone",
        &claim.pairing_id,
        grant(),
        NOW,
        &context
    )
    .is_err());
    let active = confirm_binding(
        &tx,
        &channel,
        PRINCIPAL,
        0,
        "phone",
        &claim.pairing_id,
        grant(),
        NOW,
        &context,
    )
    .unwrap();
    assert_eq!(active.revision, 1);
    assert!(claim_pairing(
        &tx,
        &channel,
        PRINCIPAL,
        &pairing.code,
        "phone",
        NOW,
        &context
    )
    .is_err());
    let status = pair_status(&tx, &channel, PRINCIPAL, NOW).unwrap();
    assert_eq!(status["status"], "active");
    let public = status.to_string();
    assert!(!public.contains(&pairing.code));
    assert!(!public.contains("controls"));
    for _ in 0..32 {
        issue_pairing(&tx, &channel, NOW, 60).unwrap();
    }
    assert!(issue_pairing(&tx, &channel, NOW, 60).is_err());
    assert!(claim_pairing(
        &tx,
        &TrustedChannel {
            device_id: OTHER.into(),
            config_revision: 1
        },
        OTHER,
        &pairing.code,
        "phone",
        NOW,
        &context
    )
    .is_err());
}
#[test]
fn explicit_controls_cannot_inherit_default_allow_or_exceed_ceilings() {
    let valid = grant();
    policy::validate_grant(&valid, NOW).unwrap();
    for path in [
        vec!["budget"],
        vec!["permissions"],
        vec!["permissions", "managed_tools"],
        vec!["permissions", "read_files"],
        vec!["budget", "max_model_requests"],
    ] {
        let mut invalid = grant();
        let mut value = &mut invalid.controls;
        for key in &path[..path.len() - 1] {
            value = &mut value[*key];
        }
        value.as_object_mut().unwrap().remove(*path.last().unwrap());
        assert!(policy::validate_grant(&invalid, NOW).is_err());
    }
    let mut invalid = grant();
    invalid.controls = json!({});
    assert!(policy::validate_grant(&invalid, NOW).is_err());
    invalid = grant();
    invalid.controls["permissions"]["write_files"] = json!("deny");
    assert!(policy::validate_grant(&invalid, NOW).is_err());
    invalid = grant();
    invalid.controls["budget"]["max_runtime_seconds"] = json!(901);
    assert!(policy::validate_grant(&invalid, NOW).is_err());
    invalid = grant();
    invalid.expires_at_ms = NOW + 30 * 24 * 60 * 60 * 1000 + 1;
    assert!(policy::validate_grant(&invalid, NOW).is_err());
    invalid = grant();
    invalid.session_ids = vec![SESSION.into(), SESSION.into()];
    assert!(policy::validate_grant(&invalid, NOW).is_err());
    let permissions: GatewayPermissions = serde_json::from_value(json!({})).unwrap();
    assert!(!permissions.view && !permissions.run && !permissions.answer);
}
#[test]
fn scoped_resources_task_session_revision_and_questions_are_enforced() {
    let mut db = database();
    let tx = db.transaction().unwrap();
    let context = Context::default();
    active(&tx, &context);
    let channel = channel_fixture();
    let cancel = request(GatewayCommand::Cancel {
        task_id: TASK.into(),
    });
    authorize(&tx, &channel, &cancel, NOW, &context).unwrap();
    let cross = Context {
        task_session: OTHER.into(),
        ..Context::default()
    };
    assert!(authorize(&tx, &channel, &cancel, NOW, &cross).is_err());
    let answer = request(GatewayCommand::Answer {
        request_id: QUESTION.into(),
        expected_revision: 1,
        answer: "回答".into(),
    });
    authorize(&tx, &channel, &answer, NOW, &context).unwrap();
    let approval = Context {
        question: false,
        ..Context::default()
    };
    assert!(authorize(&tx, &channel, &answer, NOW, &approval).is_err());
    context.open.store(false, Ordering::Release);
    assert!(authorize(&tx, &channel, &cancel, NOW, &context).is_err());
    context.open.store(true, Ordering::Release);
    assert!(authorize(&tx, &channel, &cancel, NOW + 60000, &context).is_err());
    assert!(authorize(
        &tx,
        &TrustedChannel {
            device_id: DEVICE.into(),
            config_revision: 2
        },
        &cancel,
        NOW,
        &context
    )
    .is_err());
}
#[test]
fn effects_and_receipts_commit_atomically_and_exact_duplicates_never_reexecute() {
    let mut db = database();
    let context = Context::default();
    let channel = channel_fixture();
    {
        let tx = db.transaction().unwrap();
        active(&tx, &context);
        tx.commit().unwrap();
    }
    let command = request(GatewayCommand::Run {
        content: "目标".into(),
    });
    {
        let tx = db.transaction().unwrap();
        let CommandStart::Apply(permit) =
            begin_command(&tx, &channel, &command, NOW, &context).unwrap()
        else {
            panic!()
        };
        tx.execute(
            "INSERT INTO fixture_effects VALUES(?1)",
            [permit.command_id()],
        )
        .unwrap();
        complete_command(
            &tx,
            &channel,
            &permit,
            json!({"task_id":TASK}),
            NOW,
            &context,
        )
        .unwrap();
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM fixture_effects", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM gateway_receipts", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    {
        let tx = db.transaction().unwrap();
        let CommandStart::Apply(permit) =
            begin_command(&tx, &channel, &command, NOW, &context).unwrap()
        else {
            panic!()
        };
        tx.execute(
            "INSERT INTO fixture_effects VALUES(?1)",
            [permit.command_id()],
        )
        .unwrap();
        complete_command(
            &tx,
            &channel,
            &permit,
            json!({"task_id":TASK}),
            NOW,
            &context,
        )
        .unwrap();
        drop(permit);
        tx.commit().unwrap();
    }
    let tx = db.transaction().unwrap();
    let mut duplicate = command.clone();
    duplicate.request_id = new_id();
    assert!(
        matches!(begin_command(&tx,&channel,&duplicate,NOW,&context).unwrap(),CommandStart::Existing(value) if value==json!({"task_id":TASK}))
    );
    if let GatewayRpc::Command { command, .. } = &mut duplicate.rpc {
        *command = GatewayCommand::Run {
            content: "different".into(),
        };
    }
    assert!(begin_command(&tx, &channel, &duplicate, NOW, &context).is_err());
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM fixture_effects", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn answered_question_duplicate_receipt_is_available_without_reanswering() {
    let mut db = database();
    let context = Context::default();
    let channel = channel_fixture();
    let tx = db.transaction().unwrap();
    active(&tx, &context);
    let command = request(GatewayCommand::Answer {
        request_id: QUESTION.into(),
        expected_revision: 1,
        answer: "yes".into(),
    });
    let CommandStart::Apply(permit) =
        begin_command(&tx, &channel, &command, NOW, &context).unwrap()
    else {
        panic!()
    };
    complete_command(
        &tx,
        &channel,
        &permit,
        json!({"answered":true}),
        NOW,
        &context,
    )
    .unwrap();
    drop(permit);
    let closed = Context {
        question: false,
        ..Context::default()
    };
    assert!(matches!(
        begin_command(&tx, &channel, &command, NOW, &closed).unwrap(),
        CommandStart::Existing(_)
    ));
    closed.open.store(false, Ordering::Release);
    assert!(begin_command(&tx, &channel, &command, NOW, &closed).is_err());
}
#[test]
fn revoke_and_restore_invalidate_pairings_bindings_and_receipts_without_replay() {
    let mut db = database();
    let context = Context::default();
    let channel = channel_fixture();
    let tx = db.transaction().unwrap();
    active(&tx, &context);
    let issued = issue_pairing(&tx, &channel, NOW, 60).unwrap();
    let request = request(GatewayCommand::Run {
        content: "goal".into(),
    });
    let CommandStart::Apply(permit) =
        begin_command(&tx, &channel, &request, NOW, &context).unwrap()
    else {
        panic!()
    };
    let id = permit.command_id().to_owned();
    complete_command(
        &tx,
        &channel,
        &permit,
        json!({"task_id":TASK}),
        NOW,
        &context,
    )
    .unwrap();
    drop(permit);
    assert!(read_receipt(&tx, &channel, PRINCIPAL, 1, &id, NOW, &context).is_ok());
    restore_invalidate(&tx, NOW + 1).unwrap();
    restore_invalidate(&tx, NOW + 2).unwrap();
    assert_eq!(binding(&tx, PRINCIPAL).unwrap().status, "revoked");
    assert_eq!(binding(&tx, PRINCIPAL).unwrap().revision, 2);
    assert!(read_receipt(&tx, &channel, PRINCIPAL, 1, &id, NOW + 2, &context).is_err());
    assert!(claim_pairing(
        &tx,
        &channel,
        OTHER,
        &issued.code,
        "phone",
        NOW + 2,
        &context
    )
    .is_err());
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM gateway_receipts WHERE invalidated=1",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        1
    );
}
#[test]
fn revocation_before_final_receipt_rolls_back_effect_and_no_receipt_queue_exists() {
    let mut db = database();
    let context = Context::default();
    let channel = channel_fixture();
    {
        let tx = db.transaction().unwrap();
        active(&tx, &context);
        tx.commit().unwrap();
    }
    {
        let tx = db.transaction().unwrap();
        let request = request(GatewayCommand::Run {
            content: "goal".into(),
        });
        let CommandStart::Apply(permit) =
            begin_command(&tx, &channel, &request, NOW, &context).unwrap()
        else {
            panic!()
        };
        tx.execute(
            "INSERT INTO fixture_effects VALUES(?1)",
            [permit.command_id()],
        )
        .unwrap();
        context.open.store(false, Ordering::Release);
        assert!(
            complete_command(&tx, &channel, &permit, json!({"ok":true}), NOW, &context).is_err()
        );
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM fixture_effects", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM gateway_receipts", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn receipt_insertion_failure_rolls_back_real_effect_and_foreign_receipts_are_denied() {
    let mut db = database();
    let context = Context::default();
    let channel = channel_fixture();
    {
        let tx = db.transaction().unwrap();
        active(&tx, &context);
        tx.commit().unwrap();
    }
    db.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON gateway_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let request = request(GatewayCommand::Run {
        content: "goal".into(),
    });
    {
        let tx = db.transaction().unwrap();
        let CommandStart::Apply(permit) =
            begin_command(&tx, &channel, &request, NOW, &context).unwrap()
        else {
            panic!()
        };
        tx.execute(
            "INSERT INTO fixture_effects VALUES(?1)",
            [permit.command_id()],
        )
        .unwrap();
        assert!(
            complete_command(&tx, &channel, &permit, json!({"ok":true}), NOW, &context).is_err()
        );
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM fixture_effects", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_receipt;").unwrap();
    let tx = db.transaction().unwrap();
    let CommandStart::Apply(permit) =
        begin_command(&tx, &channel, &request, NOW, &context).unwrap()
    else {
        panic!()
    };
    let id = permit.command_id().to_owned();
    complete_command(&tx, &channel, &permit, json!({"ok":true}), NOW, &context).unwrap();
    drop(permit);
    let code = issue_pairing(&tx, &channel, NOW, 60).unwrap();
    let claim = claim_pairing(
        &tx,
        &channel,
        OTHER,
        &code.code,
        "other phone",
        NOW,
        &context,
    )
    .unwrap();
    confirm_binding(
        &tx,
        &channel,
        OTHER,
        0,
        "other phone",
        &claim.pairing_id,
        grant(),
        NOW,
        &context,
    )
    .unwrap();
    assert!(read_receipt(&tx, &channel, OTHER, 1, &id, NOW, &context).is_err());
    let mut foreign = request.clone();
    foreign.principal_id = OTHER.into();
    assert!(begin_command(&tx, &channel, &foreign, NOW, &context).is_err());
    assert!(read_receipt(&tx, &channel, PRINCIPAL, 0, &id, NOW, &context).is_err());
    revoke_binding(&tx, PRINCIPAL, 1, NOW + 1).unwrap();
    assert!(read_receipt(&tx, &channel, PRINCIPAL, 2, &id, NOW + 1, &context).is_err());
}
#[test]
fn resource_read_scopes_allow_granted_results_but_reject_paths_and_other_sessions() {
    let context = Context::default();
    let grant = grant();
    assert_eq!(
        policy::authorize_rpc(
            &grant,
            PRINCIPAL,
            2,
            &GatewayRpc::Changes {
                task_id: TASK.into()
            },
            &context
        )
        .unwrap(),
        Some(SESSION.into())
    );
    assert!(policy::authorize_rpc(
        &grant,
        PRINCIPAL,
        2,
        &GatewayRpc::Artifact {
            resource_id: QUESTION.into()
        },
        &context
    )
    .is_ok());
    assert!(policy::authorize_rpc(
        &grant,
        PRINCIPAL,
        2,
        &GatewayRpc::Artifact {
            resource_id: "../../secret".into()
        },
        &context
    )
    .is_err());
    let foreign = Context {
        task_session: OTHER.into(),
        ..Context::default()
    };
    assert!(policy::authorize_rpc(
        &grant,
        PRINCIPAL,
        2,
        &GatewayRpc::Changes {
            task_id: TASK.into()
        },
        &foreign
    )
    .is_err());
    let mut denied = grant.clone();
    denied.permissions.read_artifacts = false;
    assert!(policy::authorize_rpc(
        &denied,
        PRINCIPAL,
        2,
        &GatewayRpc::Artifact {
            resource_id: QUESTION.into()
        },
        &context
    )
    .is_err());
    denied.permissions.read_results = false;
    assert!(policy::authorize_rpc(
        &denied,
        PRINCIPAL,
        2,
        &GatewayRpc::Changes {
            task_id: TASK.into()
        },
        &context
    )
    .is_err());
    assert!(serde_json::from_value::<GatewayCommand>(
        json!({"type":"approve","request_id":QUESTION})
    )
    .is_err());
    assert!(serde_json::from_value::<GatewayRpc>(json!({"type":"settings"})).is_err());
}
#[test]
fn expired_or_closed_pairing_does_not_create_binding_and_restore_can_rollback() {
    let mut db = database();
    let context = Context::default();
    let channel = channel_fixture();
    let tx = db.transaction().unwrap();
    let code = issue_pairing(&tx, &channel, NOW, 1).unwrap();
    assert!(claim_pairing(
        &tx,
        &channel,
        PRINCIPAL,
        &code.code,
        "phone",
        NOW + 1000,
        &context
    )
    .is_err());
    context.open.store(false, Ordering::Release);
    assert!(claim_pairing(&tx, &channel, PRINCIPAL, &code.code, "phone", NOW, &context).is_err());
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM gateway_bindings", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    context.open.store(true, Ordering::Release);
    active(&tx, &context);
    tx.commit().unwrap();
    {
        let tx = db.transaction().unwrap();
        restore_invalidate(&tx, NOW + 1).unwrap();
    }
    let tx = db.transaction().unwrap();
    assert_eq!(binding(&tx, PRINCIPAL).unwrap().status, "active");
    assert_eq!(bindings(&tx, DEVICE, 20).unwrap().len(), 1);
    assert!(bindings(&tx, DEVICE, 257).is_err());
}
#[test]
fn inconsistent_binding_indexes_fail_closed() {
    let mut db = database();
    let context = Context::default();
    let channel = channel_fixture();
    let tx = db.transaction().unwrap();
    active(&tx, &context);
    tx.execute(
        "UPDATE gateway_bindings SET status='revoked' WHERE principal_id=?1",
        [PRINCIPAL],
    )
    .unwrap();
    assert!(active_binding(&tx, &channel, PRINCIPAL, 1, NOW, &context).is_err());
    assert!(pair_status(&tx, &channel, PRINCIPAL, NOW).is_err());
}

#[test]
fn desktop_task_inputs_require_all_control_ceilings_but_cancel_and_continue_are_session_scoped() {
    use harness_core::{PermissionMode, TaskControls};
    let mut g = grant();
    let mut ceiling = TaskControls::default();
    ceiling.budget.max_runtime_seconds = 300;
    ceiling.budget.max_output_bytes = 65536;
    ceiling.permissions.write_files = PermissionMode::Ask;
    ceiling.permissions.run_commands = PermissionMode::Ask;
    ceiling.permissions.managed_tools = PermissionMode::Ask;
    ceiling.acceptance_commands = vec!["python verify.py".into()];
    ceiling.capabilities.mcp_servers = vec![OTHER.into()];
    ceiling.capabilities.skills = vec![OTHER.into()];
    g.controls = serde_json::to_value(&ceiling).unwrap();
    let context = Context {
        controls: ceiling.clone(),
        ..Context::default()
    };
    let steer = GatewayRpc::Command {
        command_id: harness_core::new_id(),
        session_id: SESSION.into(),
        command: GatewayCommand::Steer {
            task_id: TASK.into(),
            content: "补充".into(),
        },
    };
    let answer = GatewayRpc::Command {
        command_id: harness_core::new_id(),
        session_id: SESSION.into(),
        command: GatewayCommand::Answer {
            request_id: QUESTION.into(),
            expected_revision: 1,
            answer: "回答".into(),
        },
    };
    for rpc in [&steer, &answer] {
        policy::authorize_rpc(&g, PRINCIPAL, 1, rpc, &context).unwrap();
    }
    let mut higher = Vec::new();
    macro_rules! change {
        ($field:ident) => {{
            let mut target = ceiling.clone();
            target.budget.$field += 1;
            higher.push(target);
        }};
    }
    change!(max_model_requests);
    change!(max_runtime_seconds);
    change!(max_retries);
    change!(max_repair_attempts);
    change!(max_output_bytes);
    change!(max_external_calls);
    for field in ["write", "run", "managed"] {
        let mut target = ceiling.clone();
        match field {
            "write" => target.permissions.write_files = PermissionMode::Allow,
            "run" => target.permissions.run_commands = PermissionMode::Allow,
            _ => target.permissions.managed_tools = PermissionMode::Allow,
        }
        higher.push(target);
    }
    let mut target = ceiling.clone();
    target.acceptance_commands.clear();
    higher.push(target);
    let mut target = ceiling.clone();
    target.capabilities.mcp_servers.push(PRINCIPAL.into());
    higher.push(target);
    let mut target = ceiling.clone();
    target.capabilities.skills.push(PRINCIPAL.into());
    higher.push(target);
    for controls in higher {
        let context = Context {
            controls,
            ..Context::default()
        };
        for rpc in [&steer, &answer] {
            assert!(policy::authorize_rpc(&g, PRINCIPAL, 1, rpc, &context).is_err());
        }
        // Cancellation executes nothing; continuation creates a new task whose
        // controls Store must intersect with prior + grant + local/role limits.
        for command in [
            GatewayCommand::Cancel {
                task_id: TASK.into(),
            },
            GatewayCommand::Continue {
                task_id: TASK.into(),
                content: None,
            },
        ] {
            let rpc = GatewayRpc::Command {
                command_id: harness_core::new_id(),
                session_id: SESSION.into(),
                command,
            };
            policy::authorize_rpc(&g, PRINCIPAL, 1, &rpc, &context).unwrap();
        }
    }
    ceiling.permissions.read_files = false;
    ceiling.permissions.run_commands = PermissionMode::Deny;
    ceiling.permissions.managed_tools = PermissionMode::Deny;
    g.controls = serde_json::to_value(&ceiling).unwrap();
    let mut target = ceiling.clone();
    target.permissions.read_files = true;
    let context = Context {
        controls: target,
        ..Context::default()
    };
    assert!(policy::authorize_rpc(&g, PRINCIPAL, 1, &steer, &context).is_err());
    g.permissions.cancel = false;
    let rpc = GatewayRpc::Command {
        command_id: harness_core::new_id(),
        session_id: SESSION.into(),
        command: GatewayCommand::Cancel {
            task_id: TASK.into(),
        },
    };
    assert!(policy::authorize_rpc(&g, PRINCIPAL, 1, &rpc, &context).is_err());
    g.controls = json!({});
    assert!(policy::authorize_rpc(&g, PRINCIPAL, 1, &answer, &context).is_err());
}

#[test]
fn artifact_grant_cannot_bypass_explicit_result_permission() {
    let mut value = grant();
    value.permissions.read_results = false;
    value.permissions.read_artifacts = true;
    assert!(policy::validate_grant(&value, 10).is_err());
}
