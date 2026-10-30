use super::*;
const NOW: u64 = 1_800_000_000_000;
fn db() -> (Connection, Config) {
    let mut db = Connection::open_in_memory().unwrap();
    super::super::persistence::migrate(&db).unwrap();
    configuration::migrate(&db).unwrap();
    migrate(&db).unwrap();
    let tx = db.transaction().unwrap();
    let c = configuration::create(&tx, "app_1").unwrap();
    tx.commit().unwrap();
    (db, c)
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn valid(_: &str, _: &str, reference: &str) -> Result<()> {
    if !reference.starts_with("qq-ref-") {
        bail!("夹具拒绝错误作用域");
    }
    Ok(())
}
fn begin(db: &mut Connection, c: &Config, reference: &str) -> String {
    let op = id();
    let tx = db.transaction().unwrap();
    begin_set(&tx, &op, &c.id, c.revision, reference, valid).unwrap();
    tx.commit().unwrap();
    op
}
fn complete(db: &mut Connection, op: &str, outcome: Outcome) {
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, op, valid).unwrap().is_some());
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(finish(&tx, op, outcome, valid).unwrap());
    tx.commit().unwrap();
}
#[test]
fn metadata_replay_precedes_reference_validation_and_public_view_is_redacted() {
    let (mut db, c) = db();
    let op = begin(&mut db, &c, "qq-ref-first");
    assert!(existing_set(&db, &op, &c.id, c.revision).unwrap().is_some());
    let tx = db.transaction().unwrap();
    let replay = begin_set(&tx, &op, &c.id, c.revision, "NOT_A_REFERENCE", |_, _, _| {
        panic!("replay validation")
    })
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(replay["replayed"], true);
    assert!(!replay.to_string().contains("qq-ref"));
    assert!(!replay.to_string().contains("metadata"));
    assert!(existing_set(&db, &op, &c.id, c.revision + 1).is_err());
    complete(&mut db, &op, Outcome::NotApplied);
    assert!(!operation(&db, &op).unwrap().can_cleanup);
    let tx = db.transaction().unwrap();
    assert!(begin_set(&tx, &id(), &c.id, c.revision, "qq-ref-first", valid).is_err());
}
#[test]
fn bad_scope_revision_and_active_operation_reject_before_os() {
    let (mut db, c) = db();
    {
        let tx = db.transaction().unwrap();
        assert!(begin_set(&tx, &id(), &c.id, 0, "wrong-scope", valid).is_err());
    }
    assert!(operations(&db).unwrap().is_empty());
    {
        let tx = db.transaction().unwrap();
        assert!(begin_set(&tx, &id(), &c.id, MAX_REV, "qq-ref-overflow", valid).is_err());
    }
    let op = begin(&mut db, &c, "qq-ref-valid");
    {
        let tx = db.transaction().unwrap();
        assert!(begin_set(&tx, &id(), &c.id, 0, "qq-ref-second", valid).is_err());
    }
    let tx = db.transaction().unwrap();
    configuration::update(
        &tx,
        &c.id,
        0,
        configuration::ConfigPatch {
            app_id: c.app_id.clone(),
            enabled: false,
            reply_policy: c.reply_policy.clone(),
        },
        NOW,
        |_| Ok(false),
    )
    .unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, &op, |_, _, _| panic!("stale preclaim"))
        .unwrap()
        .is_none());
    tx.commit().unwrap();
    assert_eq!(operation(&db, &op).unwrap().status, "rejected");
    assert!(!operation(&db, &op).unwrap().can_cleanup);
}
#[test]
fn success_rotation_preserves_old_reference_for_cleanup() {
    let (mut db, c) = db();
    let first = begin(&mut db, &c, "qq-ref-first");
    complete(&mut db, &first, Outcome::Succeeded);
    let c = configuration::current(&db).unwrap().unwrap();
    assert_eq!(c.credential_ref(), Some("qq-ref-first"));
    let second = begin(&mut db, &c, "qq-ref-second");
    complete(&mut db, &second, Outcome::Succeeded);
    assert_eq!(
        configuration::current(&db)
            .unwrap()
            .unwrap()
            .credential_ref(),
        Some("qq-ref-second")
    );
    let old = all(&db)
        .unwrap()
        .into_iter()
        .find(|o| o.status == "cleanup_required")
        .unwrap();
    assert_eq!(old.reference, "qq-ref-first");
    assert!(old.can_cleanup);
    assert_eq!(old.owner_id, c.id);
    let cleanup = id();
    let tx = db.transaction().unwrap();
    begin_cleanup(&tx, &cleanup, &old.id).unwrap();
    tx.commit().unwrap();
    complete(&mut db, &cleanup, Outcome::Succeeded);
    assert_eq!(operation(&db, &old.id).unwrap().status, "cleaned");
}
#[test]
fn cas_after_claim_keeps_new_reference_orphaned_without_binding() {
    let (mut db, c) = db();
    let op = begin(&mut db, &c, "qq-ref-orphan");
    let tx = db.transaction().unwrap();
    let work = claim(&tx, &op, valid).unwrap().unwrap();
    assert_eq!(work.config_id, c.id);
    assert_eq!(work.app_id, c.app_id);
    assert_eq!(
        work.binding_digest,
        configuration::credential_binding_digest(&c.app_id).unwrap()
    );
    assert_eq!(work.operation_id, op);
    assert_eq!(work.reference, "qq-ref-orphan");
    assert!(work.kind == WorkKind::Set);
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    configuration::update(
        &tx,
        &c.id,
        c.revision,
        configuration::ConfigPatch {
            app_id: "app_2".into(),
            enabled: false,
            reply_policy: c.reply_policy.clone(),
        },
        NOW,
        |_| Ok(false),
    )
    .unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    finish(&tx, &op, Outcome::Succeeded, valid).unwrap();
    tx.commit().unwrap();
    assert_eq!(operation(&db, &op).unwrap().status, "orphaned");
    assert!(operation(&db, &op).unwrap().can_cleanup);
    assert!(configuration::current(&db)
        .unwrap()
        .unwrap()
        .credential_ref()
        .is_none());
}
#[test]
fn delete_disconnects_before_claim_and_not_applied_keeps_cleanup_reference() {
    let (mut db, c) = db();
    let op = begin(&mut db, &c, "qq-ref-delete");
    complete(&mut db, &op, Outcome::Succeeded);
    let c = configuration::current(&db).unwrap().unwrap();
    let deletion = id();
    let tx = db.transaction().unwrap();
    begin_delete(&tx, &deletion, &c.id, c.revision, NOW, valid).unwrap();
    assert!(configuration::current(&tx)
        .unwrap()
        .unwrap()
        .credential_ref()
        .is_none());
    assert!(!configuration::current(&tx).unwrap().unwrap().enabled);
    tx.commit().unwrap();
    complete(&mut db, &deletion, Outcome::NotApplied);
    assert!(operation(&db, &deletion).unwrap().can_cleanup);
}
#[test]
fn unknown_cleanup_history_recovery_no_replay_and_no_double_cleanup() {
    let (mut db, c) = db();
    let op = begin(&mut db, &c, "qq-ref-unknown");
    complete(&mut db, &op, Outcome::Unknown);
    let cleanup = id();
    let tx = db.transaction().unwrap();
    begin_cleanup(&tx, &cleanup, &op).unwrap();
    tx.commit().unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(begin_cleanup(&tx, &id(), &op).is_err());
    }
    let tx = db.transaction().unwrap();
    claim(&tx, &cleanup, valid).unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    recover(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(operation(&db, &cleanup).unwrap().status, "unknown");
    assert_eq!(operation(&db, &op).unwrap().status, "unknown");
    assert!(operation(&db, &op).unwrap().can_cleanup);
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, &cleanup, valid).unwrap().is_none());
    assert!(!finish(&tx, &cleanup, Outcome::Succeeded, valid).unwrap());
    tx.commit().unwrap();
    let again = id();
    let tx = db.transaction().unwrap();
    begin_cleanup(&tx, &again, &op).unwrap();
    tx.commit().unwrap();
    complete(&mut db, &again, Outcome::Succeeded);
    let original = operation(&db, &op).unwrap();
    assert_eq!(original.status, "unknown");
    assert_eq!(original.cleanup_status.as_deref(), Some("completed"));
    assert!(!original.can_cleanup);
}
#[test]
fn pending_recovery_is_unknown_and_bound_reference_cannot_be_cleaned() {
    let (mut db, c) = db();
    let op = begin(&mut db, &c, "qq-ref-pending");
    let tx = db.transaction().unwrap();
    recover(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(operation(&db, &op).unwrap().status, "unknown");
    let tx = db.transaction().unwrap();
    configuration::attach_credential(&tx, &c.id, c.revision, "qq-ref-pending", |_, _| Ok(()))
        .unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(begin_cleanup(&tx, &id(), &op).is_err());
    assert!(claim(&tx, &op, valid).unwrap().is_none());
}
fn fill(tx: &Transaction<'_>, c: &Config, n: usize, orphan: bool, cleanup: bool) {
    for _ in 0..n {
        let mut op = new(&id(), c, "set", &format!("qq-ref-{}", id()), Value::Null);
        op.status = "unknown".into();
        op.orphan = orphan;
        op.can_cleanup = orphan;
        if cleanup {
            op.target = Some(id());
            op.kind = "cleanup".into();
        }
        put(tx, &op).unwrap();
    }
}
#[test]
fn normal_reservation_leaves_rotation_audit_room_and_cleanup_has_own_tail() {
    let (mut db, c) = db();
    let first = begin(&mut db, &c, "qq-ref-bound");
    complete(&mut db, &first, Outcome::Succeeded);
    let c = configuration::current(&db).unwrap().unwrap();
    let tx = db.transaction().unwrap();
    fill(&tx, &c, NORMAL_LIMIT - 3, false, false);
    tx.commit().unwrap();
    let rotate = begin(&mut db, &c, "qq-ref-rotation");
    complete(&mut db, &rotate, Outcome::Succeeded);
    assert_eq!(all(&db).unwrap().len(), NORMAL_LIMIT);
    let target = all(&db)
        .unwrap()
        .into_iter()
        .find(|o| o.can_cleanup)
        .unwrap();
    {
        let tx = db.transaction().unwrap();
        let c = configuration::current(&tx).unwrap().unwrap();
        assert!(begin_set(&tx, &id(), &c.id, c.revision, "qq-ref-full", valid).is_err());
    }
    let cleanup = id();
    let tx = db.transaction().unwrap();
    begin_cleanup(&tx, &cleanup, &target.id).unwrap();
    tx.commit().unwrap();
    complete(&mut db, &cleanup, Outcome::Unknown);
    let tx = db.transaction().unwrap();
    fill(&tx, &c, CLEANUP_LIMIT - 1, false, true);
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(begin_cleanup(&tx, &id(), &target.id).is_err());
    assert!(operation(&tx, &target.id).unwrap().can_cleanup);
}
#[test]
fn orphan_limit_prevents_unreserved_os_work() {
    let (mut db, c) = db();
    let tx = db.transaction().unwrap();
    fill(&tx, &c, ORPHAN_LIMIT, true, false);
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(begin_set(
        &tx,
        &id(),
        &c.id,
        c.revision,
        "qq-ref-full",
        |_, _, _| panic!("capacity first")
    )
    .is_err());
}

#[test]
fn finalize_failure_retains_claim_for_recovery_and_disables_before_delete() {
    let (mut db, c) = db();
    let op = begin(&mut db, &c, "qq-ref-finalize");
    let tx = db.transaction().unwrap();
    claim(&tx, &op, valid).unwrap();
    tx.commit().unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(finish(&tx, &op, Outcome::Succeeded, |_, _, _| bail!(
            "夹具校验失败"
        ))
        .is_err());
        tx.rollback().unwrap();
    }
    assert_eq!(operation(&db, &op).unwrap().status, "inflight");
    assert!(configuration::current(&db)
        .unwrap()
        .unwrap()
        .credential_ref()
        .is_none());
    let tx = db.transaction().unwrap();
    recover(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(operation(&db, &op).unwrap().status, "unknown");
    assert!(operation(&db, &op).unwrap().can_cleanup);
    let next = begin(&mut db, &c, "qq-ref-working");
    complete(&mut db, &next, Outcome::Succeeded);
    let c = configuration::current(&db).unwrap().unwrap();
    let tx = db.transaction().unwrap();
    let enabled = configuration::update(
        &tx,
        &c.id,
        c.revision,
        configuration::ConfigPatch {
            app_id: c.app_id.clone(),
            enabled: true,
            reply_policy: c.reply_policy.clone(),
        },
        NOW,
        |_| Ok(true),
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(enabled.enabled);
    let deletion = id();
    let tx = db.transaction().unwrap();
    begin_delete(&tx, &deletion, &enabled.id, enabled.revision, NOW, valid).unwrap();
    tx.commit().unwrap();
    let disconnected = configuration::current(&db).unwrap().unwrap();
    assert!(!disconnected.enabled);
    assert!(disconnected.credential_ref().is_none());
    assert_eq!(operation(&db, &deletion).unwrap().status, "pending");
}
