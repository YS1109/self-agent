use super::*;
fn db() -> Connection {
    let mut db = Connection::open_in_memory().unwrap();
    let tx = db.transaction().unwrap();
    migrate(&tx).unwrap();
    tx.commit().unwrap();
    db
}
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn parent() -> PathBuf {
    std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join("private-selected-parent")
}
fn enqueue(db: &mut Connection, id: &str) {
    let tx = db.transaction().unwrap();
    accept(&tx, id, &parent(), "backup").unwrap();
    tx.commit().unwrap();
}
fn own(db: &mut Connection, id: &str) -> ClaimedBackup {
    let tx = db.transaction().unwrap();
    let work = claim(&tx, id).unwrap().unwrap();
    tx.commit().unwrap();
    work
}
#[test]
fn acceptance_is_idempotent_private_and_transactional() {
    let mut db = db();
    let id = id();
    {
        let tx = db.transaction().unwrap();
        accept(&tx, &id, &parent(), "backup").unwrap();
    }
    assert!(get(&db, &id).unwrap().is_none());
    enqueue(&mut db, &id);
    let before = get(&db, &id).unwrap().unwrap();
    let tx = db.transaction().unwrap();
    assert_eq!(accept(&tx, &id, &parent(), "backup").unwrap(), before);
    assert!(accept(&tx, &id, &parent(), "different").is_err());
    tx.commit().unwrap();
    assert_eq!(list(&db).unwrap().len(), 1);
    let public = serde_json::to_string(&list(&db).unwrap()).unwrap();
    assert!(!public.contains("private-selected-parent"));
    assert!(!public.contains("fingerprint"));
    assert!(!public.contains("token"));
}
#[test]
fn claim_once_finish_cas_and_terminal_retry_do_not_replay() {
    let mut db = db();
    let id = id();
    enqueue(&mut db, &id);
    let work = own(&mut db, &id);
    assert_eq!(work.parent, parent());
    assert_eq!(work.name, "backup");
    assert_eq!(work.id, id);
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, &id).unwrap().is_none());
    assert!(finish(
        &tx,
        &id,
        &Uuid::new_v4().to_string(),
        BackupOutcome::Unknown
    )
    .is_err());
    let result = finish(
        &tx,
        &id,
        &work.token,
        BackupOutcome::Failed(BackupReason::Cancelled),
    )
    .unwrap();
    assert!(finish(&tx, &id, &work.token, BackupOutcome::Unknown).is_err());
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert_eq!(accept(&tx, &id, &parent(), "backup").unwrap(), result);
    assert!(claim(&tx, &id).unwrap().is_none());
}
#[test]
fn restart_marks_pending_running_unknown_and_rejects_late_completion() {
    let mut db = db();
    let pending = id();
    let running = id();
    enqueue(&mut db, &pending);
    enqueue(&mut db, &running);
    let work = own(&mut db, &running);
    let tx = db.transaction().unwrap();
    assert_eq!(recover_interrupted(&tx).unwrap(), 2);
    assert_eq!(recover_interrupted(&tx).unwrap(), 0);
    assert!(finish(&tx, &running, &work.token, BackupOutcome::Unknown).is_err());
    tx.commit().unwrap();
    for id in [pending, running] {
        let r = get(&db, &id).unwrap().unwrap();
        assert_eq!(r.state, BackupState::Unknown);
        assert_eq!(r.reason, Some(BackupReason::Restarted));
        let tx = db.transaction().unwrap();
        assert!(claim(&tx, &id).unwrap().is_none());
    }
}
#[test]
fn finish_and_recovery_roll_back_with_outer_transaction() {
    let mut db = db();
    let id = id();
    enqueue(&mut db, &id);
    let work = own(&mut db, &id);
    {
        let tx = db.transaction().unwrap();
        finish(&tx, &id, &work.token, BackupOutcome::Unknown).unwrap();
    }
    assert_eq!(get(&db, &id).unwrap().unwrap().state, BackupState::Running);
    {
        let tx = db.transaction().unwrap();
        recover_interrupted(&tx).unwrap();
    }
    assert_eq!(get(&db, &id).unwrap().unwrap().state, BackupState::Running);
    let tx = db.transaction().unwrap();
    finish(&tx, &id, &work.token, BackupOutcome::Unknown).unwrap();
    tx.commit().unwrap();
}
#[test]
fn capacity_keeps_terminal_and_unresolved_idempotency_history() {
    let mut db = db();
    let first = id();
    enqueue(&mut db, &first);
    for _ in 1..MAX_BACKUP_JOBS {
        enqueue(&mut db, &id());
    }
    let tx = db.transaction().unwrap();
    assert!(accept(&tx, &id(), &parent(), "new").is_err());
    assert!(accept(&tx, &first, &parent(), "backup").is_ok());
    recover_interrupted(&tx).unwrap();
    assert!(accept(&tx, &id(), &parent(), "new").is_err());
    tx.commit().unwrap();
    assert_eq!(list(&db).unwrap().len(), MAX_BACKUP_JOBS);
}
#[test]
fn completed_receipt_uses_actual_export_manifest_without_public_path() {
    let root = std::env::temp_dir().join(format!("job-{}", id()));
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut source = Connection::open_in_memory().unwrap();
    super::super::storage::migrate(&mut source, |_| Ok(()), |_| Ok(()), |_| Ok(())).unwrap();
    let manifest = super::super::storage::export_backup(
        &source,
        &root,
        "fixture",
        std::time::Instant::now() + std::time::Duration::from_secs(5),
        || false,
    )
    .unwrap();
    let mut db = db();
    let id = id();
    enqueue(&mut db, &id);
    let work = own(&mut db, &id);
    let tx = db.transaction().unwrap();
    let result = finish(
        &tx,
        &id,
        &work.token,
        BackupOutcome::Completed(manifest.clone()),
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(result.manifest, Some(manifest));
    assert_eq!(result.state, BackupState::Completed);
    assert!(result.reason.is_none());
    let tx = db.transaction().unwrap();
    assert_eq!(recover_interrupted(&tx).unwrap(), 0);
    tx.commit().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn malformed_inputs_and_forged_manifest_fail_without_claim_loss() {
    let mut db = db();
    let key = id();
    let tx = db.transaction().unwrap();
    for (id, path, name) in [
        ("not-id", parent(), "ok"),
        (key.as_str(), PathBuf::from("relative"), "ok"),
        (key.as_str(), parent(), "../private-canary"),
    ] {
        let error = accept(&tx, id, &path, name).unwrap_err();
        assert!(!format!("{error:#}").contains("private-canary"));
    }
    tx.commit().unwrap();
    assert!(list(&db).unwrap().is_empty());
    enqueue(&mut db, &key);
    let work = own(&mut db, &key);
    let tx = db.transaction().unwrap();
    assert!(finish(
        &tx,
        &key,
        &work.token,
        BackupOutcome::Completed(BackupManifest {
            format: 1,
            schema: SCHEMA_VERSION,
            bytes: 100,
            sha256: "invalid".into()
        })
    )
    .is_err());
    assert_eq!(get(&tx, &key).unwrap().unwrap().state, BackupState::Running);
    assert!(finish(
        &tx,
        &key,
        &work.token,
        BackupOutcome::Failed(BackupReason::Restarted)
    )
    .is_err());
}
#[test]
fn claim_rolls_back_and_private_argument_tamper_fails_closed() {
    let mut db = db();
    let id = id();
    enqueue(&mut db, &id);
    {
        let tx = db.transaction().unwrap();
        claim(&tx, &id).unwrap().unwrap();
    }
    assert_eq!(get(&db, &id).unwrap().unwrap().state, BackupState::Pending);
    db.execute("UPDATE backup_jobs SET name='changed' WHERE id=?1", [&id])
        .unwrap();
    let tx = db.transaction().unwrap();
    assert!(claim(&tx, &id).is_err());
    assert_eq!(get(&tx, &id).unwrap().unwrap().state, BackupState::Pending);
}

#[test]
fn malformed_public_record_is_not_reported_as_completed() {
    let mut db = db();
    let id = id();
    enqueue(&mut db, &id);
    db.execute(
        "UPDATE backup_jobs SET receipt=?2 WHERE id=?1",
        rusqlite::params![
            id,
            serde_json::json!({"id":id,"state":"completed","reason":null,"manifest":null})
                .to_string()
        ],
    )
    .unwrap();
    assert!(get(&db, &id).is_err());
    assert!(list(&db).is_err());
}
