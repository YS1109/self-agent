use super::*;
const DEVICE: &str = "11111111-1111-4111-8111-111111111111";
const NOW: u64 = 1_800_000_000_000;
fn database() -> (Connection, Config) {
    let mut db = Connection::open_in_memory().unwrap();
    let tx = db.transaction().unwrap();
    super::super::persistence::migrate(&tx).unwrap();
    configuration::migrate(&tx).unwrap();
    migrate(&tx).unwrap();
    let c = configuration::create(&tx, "https://gateway.example/", DEVICE).unwrap();
    tx.commit().unwrap();
    (db, c)
}
fn id() -> String {
    harness_core::new_id()
}
fn valid(owner: &str, digest: &str, reference: &str) -> Result<()> {
    uuid(owner)?;
    validate_digest(digest)?;
    if !reference.starts_with("gateway-ref-") {
        bail!("fixture wrong scope");
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
fn apply(db: &mut Connection, operation: &str, outcome: Outcome) {
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, operation, valid).unwrap().is_some());
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(finish(&tx, operation, outcome, valid).unwrap());
    tx.commit().unwrap();
}
#[test]
fn operation_id_replay_never_reapplies_secret_or_changes_metadata() {
    let (mut db, c) = database();
    let op = begin(&mut db, &c, "gateway-ref-one");
    let tx = db.transaction().unwrap();
    let replay = begin_set(&tx, &op, &c.id, c.revision, "not-reference", |_, _, _| {
        panic!("replay validation")
    })
    .unwrap();
    assert_eq!(replay["no_secret_applied"], true);
    assert!(!replay.to_string().contains("gateway-ref-one"));
    assert!(existing_set(&tx, &op, &c.id, c.revision + 1).is_err());
    tx.commit().unwrap();
    apply(&mut db, &op, Outcome::Succeeded);
    let current = configuration::current(&db).unwrap().unwrap();
    assert_eq!(current.credential_ref(), Some("gateway-ref-one"));
    assert!(!current.enabled);
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, &op, valid).unwrap().is_none());
    assert!(!finish(&tx, &op, Outcome::Unknown, valid).unwrap());
    assert_eq!(operation(&tx, &op).unwrap().owner_kind, "gateway");
    assert!(!serde_json::to_string(&operation(&tx, &op).unwrap())
        .unwrap()
        .contains("gateway-ref"));
}
#[test]
fn config_changes_before_and_after_os_work_cannot_bind_stale_result() {
    let (mut db, c) = database();
    let op = begin(&mut db, &c, "gateway-ref-before");
    {
        let tx = db.transaction().unwrap();
        configuration::update(
            &tx,
            &c.id,
            c.revision,
            configuration::ConfigPatch {
                origin: c.origin.clone(),
                device_id: c.device_id.clone(),
                enabled: false,
            },
            NOW,
            |_| Ok(false),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, &op, valid).unwrap().is_none());
    assert_eq!(operation(&tx, &op).unwrap().status, "rejected");
    tx.commit().unwrap();
    let c = configuration::current(&db).unwrap().unwrap();
    let op = begin(&mut db, &c, "gateway-ref-after");
    {
        let tx = db.transaction().unwrap();
        let work = claim(&tx, &op, valid).unwrap().unwrap();
        assert_eq!(work.scope_digest, c.scope_digest);
        assert_eq!(work.binding_digest, c.scope_digest);
        assert_eq!(work.config_id, c.id);
        tx.commit().unwrap();
    }
    {
        let tx = db.transaction().unwrap();
        configuration::update(
            &tx,
            &c.id,
            c.revision,
            configuration::ConfigPatch {
                origin: "https://another.example/".into(),
                device_id: c.device_id.clone(),
                enabled: false,
            },
            NOW,
            |_| Ok(false),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.transaction().unwrap();
    finish(&tx, &op, Outcome::Succeeded, valid).unwrap();
    assert_eq!(operation(&tx, &op).unwrap().status, "orphaned");
    assert!(operation(&tx, &op).unwrap().can_cleanup);
    assert!(configuration::current(&tx)
        .unwrap()
        .unwrap()
        .credential_ref()
        .is_none());
}
#[test]
fn unknown_recovery_never_replays_os_and_cleanup_requires_new_exact_intent() {
    let (mut db, c) = database();
    let op = begin(&mut db, &c, "gateway-ref-unknown");
    {
        let tx = db.transaction().unwrap();
        claim(&tx, &op, valid).unwrap().unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.transaction().unwrap();
        recover(&tx).unwrap();
        assert_eq!(operation(&tx, &op).unwrap().status, "unknown");
        assert!(claim(&tx, &op, valid).unwrap().is_none());
        tx.commit().unwrap();
    }
    let cleanup = id();
    {
        let tx = db.transaction().unwrap();
        begin_cleanup(&tx, &cleanup, &op).unwrap();
        let work = claim(&tx, &cleanup, valid).unwrap().unwrap();
        assert!(matches!(work.kind, WorkKind::Delete));
        assert_eq!(work.reference, "gateway-ref-unknown");
        tx.commit().unwrap();
    }
    {
        let tx = db.transaction().unwrap();
        finish(&tx, &cleanup, Outcome::Unknown, valid).unwrap();
        assert!(operation(&tx, &op).unwrap().can_cleanup);
        assert!(claim(&tx, &cleanup, valid).unwrap().is_none());
        tx.commit().unwrap();
    }
    let next = id();
    let tx = db.transaction().unwrap();
    begin_cleanup(&tx, &next, &op).unwrap();
    claim(&tx, &next, valid).unwrap().unwrap();
    finish(&tx, &next, Outcome::Succeeded, valid).unwrap();
    assert!(!operation(&tx, &op).unwrap().can_cleanup);
}
#[test]
fn delete_detaches_before_os_and_restore_never_authorizes_historical_cleanup() {
    let (mut db, c) = database();
    let op = begin(&mut db, &c, "gateway-ref-delete");
    apply(&mut db, &op, Outcome::Succeeded);
    let c = configuration::current(&db).unwrap().unwrap();
    let deletion = id();
    {
        let tx = db.transaction().unwrap();
        begin_delete(&tx, &deletion, &c.id, c.revision, NOW, valid).unwrap();
        assert!(configuration::current(&tx)
            .unwrap()
            .unwrap()
            .credential_ref()
            .is_none());
        assert!(!configuration::current(&tx).unwrap().unwrap().enabled);
        tx.commit().unwrap();
    }
    {
        let tx = db.transaction().unwrap();
        claim(&tx, &deletion, valid).unwrap().unwrap();
        tx.commit().unwrap();
    }
    let tx = db.transaction().unwrap();
    configuration::invalidate_restored(&tx, NOW).unwrap();
    let restored = operation(&tx, &deletion).unwrap();
    assert_eq!(restored.status, "unknown");
    assert!(!restored.can_cleanup);
    assert!(claim(&tx, &deletion, valid).unwrap().is_none());
    assert!(begin_cleanup(&tx, &id(), &deletion).is_err());
    assert!(!finish(&tx, &deletion, Outcome::Succeeded, valid).unwrap());
}
#[test]
fn rotation_keeps_old_reference_audit_and_no_reference_can_be_reused() {
    let (mut db, c) = database();
    let first = begin(&mut db, &c, "gateway-ref-old");
    apply(&mut db, &first, Outcome::Succeeded);
    let c = configuration::current(&db).unwrap().unwrap();
    let second = begin(&mut db, &c, "gateway-ref-new");
    apply(&mut db, &second, Outcome::Succeeded);
    let ops = operations(&db).unwrap();
    assert!(ops
        .iter()
        .any(|o| o.status == "cleanup_required" && o.can_cleanup));
    let c = configuration::current(&db).unwrap().unwrap();
    assert_eq!(c.credential_ref(), Some("gateway-ref-new"));
    let tx = db.transaction().unwrap();
    assert!(begin_set(&tx, &id(), &c.id, c.revision, "gateway-ref-old", valid).is_err());
}
#[test]
fn rejected_not_applied_and_invalid_scope_have_no_cleanup_authority() {
    let (mut db, c) = database();
    {
        let tx = db.transaction().unwrap();
        assert!(begin_set(&tx, &id(), &c.id, c.revision, "qq-ref-other-scope", valid).is_err());
    }
    assert!(operations(&db).unwrap().is_empty());
    let op = begin(&mut db, &c, "gateway-ref-not-applied");
    apply(&mut db, &op, Outcome::NotApplied);
    assert!(!operation(&db, &op).unwrap().can_cleanup);
    assert!(configuration::current(&db)
        .unwrap()
        .unwrap()
        .credential_ref()
        .is_none());
}
