use super::*;
use rusqlite::OpenFlags;
// Minimal storage fixtures deliberately have no application tables/indexes.
fn migrate(
    db: &mut Connection,
    baseline: impl FnOnce(&Transaction<'_>) -> Result<()>,
    upgrade: impl FnOnce(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    super::migrate(db, baseline, upgrade, |_| Ok(()))
}

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("sa-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        Self(p.canonicalize().unwrap())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY,body TEXT NOT NULL);")?;
    Ok(())
}
fn db(value: &str) -> Connection {
    let mut db = Connection::open_in_memory().unwrap();
    migrate(&mut db, schema, |_| Ok(())).unwrap();
    db.execute(
        "INSERT INTO sessions VALUES('s',?1)",
        [json!({"messages":[value]}).to_string()],
    )
    .unwrap();
    db
}
fn value(db: &Connection) -> String {
    db.query_row("SELECT body FROM sessions WHERE id='s'", [], |r| r.get(0))
        .unwrap()
}
fn bundle(root: &Root, value: &str) -> (PathBuf, BackupManifest) {
    let source = db(value);
    let manifest = export_backup(
        &source,
        &root.0,
        "bundle",
        Instant::now() + Duration::from_secs(5),
        || false,
    )
    .unwrap();
    (root.0.join("bundle"), manifest)
}
fn original(root: &Root) -> Connection {
    let path = root.0.join(MAIN);
    let mut source = Connection::open(path).unwrap();
    migrate(&mut source, schema, |_| Ok(())).unwrap();
    source
        .execute("INSERT INTO sessions VALUES('s','original')", [])
        .unwrap();
    source
}
fn read_original(root: &Root) -> Connection {
    Connection::open_with_flags(root.0.join(MAIN), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}
#[test]
fn foreign_key_violations_reject_before_backup_publication() {
    let root = Root::new();
    let c = db("source");
    c.execute_batch("PRAGMA foreign_keys=OFF; CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(parent_id INTEGER REFERENCES parent(id)); INSERT INTO child VALUES(7);").unwrap();
    assert_eq!(
        c.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert!(export_backup(
        &c,
        &root.0,
        "broken",
        Instant::now() + Duration::from_secs(5),
        || false
    )
    .is_err());
    assert!(!root.0.join("broken").exists());
    c.execute("INSERT INTO parent VALUES(7)", []).unwrap();
    export_backup(
        &c,
        &root.0,
        "valid",
        Instant::now() + Duration::from_secs(5),
        || false,
    )
    .unwrap();
    inspect_backup(&root.0.join("valid")).unwrap();
}
#[test]
fn migration_is_atomic_and_preserves_legacy_history() {
    let mut c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY,body TEXT NOT NULL);INSERT INTO sessions VALUES('s','old');").unwrap();
    assert!(migrate(
        &mut c,
        |tx| {
            schema(tx)?;
            bail!("fixture")
        },
        |_| Ok(())
    )
    .is_err());
    assert_eq!(preflight(&c).unwrap(), 0);
    assert!(!table_exists(&c, "tasks").unwrap());
    assert_eq!(value(&c), "old");
    migrate(&mut c, schema, |_| Ok(())).unwrap();
    assert_eq!(preflight(&c).unwrap(), 3);
    assert_eq!(value(&c), "old");
}
#[test]
fn restore_scrub_rejects_malformed_nested_capability_without_panic_and_preserves_tombstones() {
    let mut c = db("source");
    c.execute_batch("CREATE TABLE managed_capabilities(id TEXT PRIMARY KEY,body TEXT NOT NULL);")
        .unwrap();
    let bad = json!({"id":"mcp","kind":"mcp","definition":"bad"}).to_string();
    c.execute("INSERT INTO managed_capabilities VALUES('mcp',?1)", [&bad])
        .unwrap();
    {
        let tx = c.transaction().unwrap();
        assert!(invalidate_core_for_restore(&tx).is_err());
    }
    assert_eq!(
        c.query_row("SELECT body FROM managed_capabilities", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        bad
    );
    c.execute(
        "UPDATE managed_capabilities SET body=?1",
        [
            json!({"id":"mcp","kind":"mcp","deleted":true,"enabled":false,"revision":2})
                .to_string(),
        ],
    )
    .unwrap();
    let tx = c.transaction().unwrap();
    invalidate_core_for_restore(&tx).unwrap();
    tx.commit().unwrap();
    let value: Value = serde_json::from_str(
        &c.query_row("SELECT body FROM managed_capabilities", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(value["deleted"], true);
    assert_eq!(value["enabled"], false);
    assert!(value.get("definition").is_none());
}
#[test]
fn newer_and_malformed_versions_reject_without_writes() {
    let root = Root::new();
    let path = root.0.join("newer.sqlite");
    let mut c = Connection::open(&path).unwrap();
    c.execute_batch(
        "PRAGMA user_version=2;CREATE TABLE preserve(v);INSERT INTO preserve VALUES('untouched');",
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(migrate(&mut c, schema, |_| Ok(())).is_err());
    drop(c);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let c = db("one");
    c.execute("UPDATE storage_migrations SET identity='other'", [])
        .unwrap();
    assert!(preflight(&c).is_err());
}
#[test]
fn consistent_backup_includes_wal_and_private_bundle() {
    let root = Root::new();
    let c = original(&root);
    c.execute_batch(
        "PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;UPDATE sessions SET body='in-wal';",
    )
    .unwrap();
    assert!(root.0.join("harness.sqlite-wal").exists());
    let manifest = export_backup(
        &c,
        &root.0,
        "wal-backup",
        Instant::now() + Duration::from_secs(5),
        || false,
    )
    .unwrap();
    assert_eq!(
        inspect_backup(&root.0.join("wal-backup")).unwrap(),
        manifest
    );
    let bytes = std::fs::read(root.0.join("wal-backup/database.sqlite")).unwrap();
    assert_eq!(value(&deserialize(&bytes).unwrap()), "in-wal");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.0.join("wal-backup/database.sqlite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn tamper_wrong_digest_cancel_and_unsafe_schema_fail_closed() {
    let root = Root::new();
    let (path, manifest) = bundle(&root, "new");
    assert!(consistent_image(&db("x"), Instant::now() + Duration::from_secs(1), || true).is_err());
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert!(guard
        .restore(
            &path,
            &"0".repeat(64),
            |db| migrate(db, schema, |_| Ok(())),
            |_| Ok(())
        )
        .is_err());
    assert!(!root.0.join(MAIN).exists());
    let mut bytes = std::fs::read(path.join("database.sqlite")).unwrap();
    bytes[100] ^= 1;
    std::fs::write(path.join("database.sqlite"), bytes).unwrap();
    assert!(guard
        .restore(
            &path,
            &manifest.sha256,
            |db| migrate(db, schema, |_| Ok(())),
            |_| Ok(())
        )
        .is_err());
    let c = db("x");
    c.execute_batch("CREATE TRIGGER evil AFTER UPDATE ON sessions BEGIN UPDATE sessions SET body='unsafe'; END;").unwrap();
    assert!(consistent_image(&c, Instant::now() + Duration::from_secs(1), || false).is_err());
}
#[test]
fn successful_restore_closes_original_and_invalidates_active_records() {
    let root = Root::new();
    let source = db("new");
    source
        .execute(
            "INSERT INTO tasks VALUES('t',?1)",
            [json!({"status":"queued","objective":"preserved"}).to_string()],
        )
        .unwrap();
    let manifest = export_backup(
        &source,
        &root.0,
        "bundle",
        Instant::now() + Duration::from_secs(5),
        || false,
    )
    .unwrap();
    drop(source);
    let orig = original(&root);
    drop(orig);
    let guard = StorageGuard::acquire(&root.0).unwrap();
    guard
        .restore(
            &root.0.join("bundle"),
            &manifest.sha256,
            |db| migrate(db, schema, |_| Ok(())),
            |tx| {
                tx.execute_batch(
                    "CREATE TABLE adapter_revoked(v);INSERT INTO adapter_revoked VALUES(1);",
                )?;
                Ok(())
            },
        )
        .unwrap();
    let restored = read_original(&root);
    assert!(value(&restored).contains("new"));
    let t: String = restored
        .query_row("SELECT body FROM tasks WHERE id='t'", [], |r| r.get(0))
        .unwrap();
    let t: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(t["status"], "failed");
    assert_eq!(t["objective"], "preserved");
    assert_eq!(t["stop_reason"], "backup_restored");
    assert!(table_exists(&restored, "adapter_revoked").unwrap());
    assert_eq!(guard.recover_pending().unwrap(), RecoveryOutcome::None);
    drop(restored);
    drop(guard);
}
#[test]
fn every_restore_stage_interruption_is_recoverable() {
    for point in [
        RestorePoint::JournalDurable,
        RestorePoint::OldImageDurable,
        RestorePoint::NewImageDurable,
        RestorePoint::OriginalMoved,
        RestorePoint::WalMoved,
        RestorePoint::ShmMoved,
        RestorePoint::Installed,
        RestorePoint::Committed,
    ] {
        let root = Root::new();
        let (path, manifest) = bundle(&root, "replacement");
        let orig = original(&root);
        drop(orig);
        let guard = StorageGuard::acquire(&root.0).unwrap();
        let result = guard.restore_with_hook(
            &path,
            &manifest.sha256,
            |db| migrate(db, schema, |_| Ok(())),
            |_| Ok(()),
            |p| {
                if p == point {
                    bail!("simulated process interruption");
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        drop(guard);
        let reopened = StorageGuard::acquire(&root.0).unwrap();
        let outcome = reopened.recover_pending().unwrap();
        assert_eq!(
            outcome,
            if point == RestorePoint::Committed {
                RecoveryOutcome::Completed
            } else {
                RecoveryOutcome::RolledBack
            }
        );
        let restored = read_original(&root);
        assert_eq!(
            value(&restored),
            if point == RestorePoint::Committed {
                json!({"messages":["replacement"]}).to_string()
            } else {
                "original".into()
            }
        );
        drop(restored);
        assert_eq!(reopened.recover_pending().unwrap(), RecoveryOutcome::None);
    }
}
#[test]
fn live_daemon_lock_refuses_restore() {
    let root = Root::new();
    let first = StorageGuard::acquire(&root.0).unwrap();
    assert!(StorageGuard::acquire(&root.0).is_err());
    drop(first);
    assert!(StorageGuard::acquire(&root.0).is_ok());
}
#[cfg(unix)]
#[test]
fn symlink_paths_and_payloads_are_not_followed() {
    use std::os::unix::fs::symlink;
    let root = Root::new();
    let (path, _) = bundle(&root, "x");
    let linked = root.0.join("linked");
    symlink(&path, &linked).unwrap();
    assert!(inspect_backup(&linked).is_err());
    let payload = path.join("database.sqlite");
    std::fs::rename(&payload, path.join("original")).unwrap();
    symlink(path.join("original"), &payload).unwrap();
    assert!(inspect_backup(&path).is_err());
}
#[test]
fn completed_bundle_has_an_explicit_manifest() {
    let root = Root::new();
    let source = db("check");
    export_backup_inner(
        &source,
        &root.0,
        "check",
        Instant::now() + Duration::from_secs(5),
        || false,
    )
    .unwrap();
}

#[test]
fn no_replay_scrub_preserves_history_and_revokes_all_core_authority() {
    let mut c = db("private-history");
    for table in [
        "plans",
        "coordinator_jobs",
        "plan_changes",
        "deliveries",
        "session_contexts",
        "execution_requests",
        "capability_calls",
        "credential_operations",
        "mcp_credential_operations",
        "qq_credential_operations",
        "managed_capabilities",
        "named_providers",
    ] {
        c.execute_batch(&format!(
            "CREATE TABLE {table}(id TEXT PRIMARY KEY,body TEXT NOT NULL)"
        ))
        .unwrap();
    }
    c.execute_batch("CREATE TABLE mcp_credential_bindings(id TEXT PRIMARY KEY,body TEXT);INSERT INTO mcp_credential_bindings VALUES('old','opaque-reference');").unwrap();
    for (table, body) in [
        (
            "tasks",
            json!({"status":"waiting_input","objective":"retain","execution_cleanup":"pending","waiting_request_id":"request"}),
        ),
        ("plans", json!({"status":"running","nodes":[{"id":"node"}]})),
        ("coordinator_jobs", json!({"status":"pending"})),
        ("plan_changes", json!({"status":"queued"})),
        ("deliveries", json!({"status":"sending"})),
        (
            "session_contexts",
            json!({"state":"available","summary":"private summary","thread_id":"external-thread"}),
        ),
        (
            "execution_requests",
            json!({"status":"approved","action":{"name":"write"}}),
        ),
        ("capability_calls", json!({"status":"sending"})),
        (
            "credential_operations",
            json!({"status":"inflight","can_cleanup":true,"reference":"private-ref","cleanup_status":"pending"}),
        ),
        (
            "mcp_credential_operations",
            json!({"status":"unknown","can_cleanup":true,"reference":"private-ref"}),
        ),
        (
            "qq_credential_operations",
            json!({"status":"completed","can_cleanup":true,"reference":"private-ref"}),
        ),
        (
            "managed_capabilities",
            json!({"kind":"mcp","enabled":true,"definition":{"enabled":true},"catalog":[{}],"revoke_epoch":3}),
        ),
        (
            "named_providers",
            json!({"enabled":true,"revision":7,"credential_ref":"private-ref"}),
        ),
    ] {
        c.execute(
            &format!("INSERT INTO {table} VALUES('id',?1)"),
            [body.to_string()],
        )
        .unwrap();
    }
    c.execute(
        "INSERT INTO tasks VALUES('done',?1)",
        [json!({"status":"completed","result":"historic success"}).to_string()],
    )
    .unwrap();
    {
        let tx = c.transaction().unwrap();
        invalidate_core_for_restore(&tx).unwrap();
        tx.commit().unwrap();
    }
    let row = |table: &str| -> Value {
        let raw: String = c
            .query_row(
                &format!("SELECT body FROM {table} WHERE id='id'"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    };
    assert_eq!(row("tasks")["status"], "failed");
    assert_eq!(row("tasks")["execution_cleanup"], "unknown");
    assert_eq!(row("plans")["status"], "paused");
    assert_eq!(row("coordinator_jobs")["status"], "failed");
    assert_eq!(row("plan_changes")["status"], "failed");
    assert_eq!(row("deliveries")["status"], "unknown");
    assert_eq!(row("session_contexts")["state"], "invalidated");
    assert!(row("session_contexts")["summary"].is_null());
    assert!(row("session_contexts")["thread_id"].is_null());
    assert_eq!(row("execution_requests")["status"], "cancelled");
    assert_eq!(row("capability_calls")["status"], "unknown");
    for table in [
        "credential_operations",
        "mcp_credential_operations",
        "qq_credential_operations",
    ] {
        assert_eq!(row(table)["can_cleanup"], false);
        assert_eq!(row(table)["reference"], "private-ref");
    }
    assert_eq!(row("credential_operations")["status"], "unknown");
    assert_eq!(row("qq_credential_operations")["status"], "completed");
    assert_eq!(row("managed_capabilities")["enabled"], false);
    assert_eq!(row("managed_capabilities")["revoke_epoch"], 4);
    assert_eq!(row("named_providers")["enabled"], false);
    assert!(row("named_providers")["credential_ref"].is_null());
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM mcp_credential_bindings", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    let completed: String = c
        .query_row("SELECT body FROM tasks WHERE id='done'", [], |r| r.get(0))
        .unwrap();
    assert!(completed.contains("historic success"));
    assert!(value(&c).contains("private-history"));
}

#[test]
fn rejected_callbacks_never_replace_original() {
    let root = Root::new();
    let (path, manifest) = bundle(&root, "replacement");
    let source = original(&root);
    drop(source);
    let before = std::fs::read(root.0.join(MAIN)).unwrap();
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert!(guard
        .restore(
            &path,
            &manifest.sha256,
            |_| bail!("private-fixture-error-do-not-expose"),
            |_| Ok(())
        )
        .is_err());
    let result = guard.restore(
        &path,
        &manifest.sha256,
        |db| migrate(db, schema, |_| Ok(())),
        |_| bail!("private-fixture-error-do-not-expose"),
    );
    assert!(!format!("{:#}", result.unwrap_err()).contains("private-fixture-error"));
    assert_eq!(std::fs::read(root.0.join(MAIN)).unwrap(), before);
    assert_eq!(guard.recover_pending().unwrap(), RecoveryOutcome::None);
}

#[test]
fn interrupted_new_database_restore_does_not_invent_an_original() {
    for point in [
        RestorePoint::JournalDurable,
        RestorePoint::NewImageDurable,
        RestorePoint::Installed,
        RestorePoint::Committed,
    ] {
        let root = Root::new();
        let (path, manifest) = bundle(&root, "fresh");
        let guard = StorageGuard::acquire(&root.0).unwrap();
        assert!(guard
            .restore_with_hook(
                &path,
                &manifest.sha256,
                |db| migrate(db, schema, |_| Ok(())),
                |_| Ok(()),
                |p| {
                    if p == point {
                        bail!("interrupt");
                    }
                    Ok(())
                }
            )
            .is_err());
        drop(guard);
        let guard = StorageGuard::acquire(&root.0).unwrap();
        let outcome = guard.recover_pending().unwrap();
        assert_eq!(
            outcome,
            if point == RestorePoint::Committed {
                RecoveryOutcome::Completed
            } else {
                RecoveryOutcome::RolledBack
            }
        );
        assert_eq!(root.0.join(MAIN).exists(), point == RestorePoint::Committed);
    }
}

#[test]
fn interrupted_restore_rolls_back_committed_wal_without_mixing_sidecars() {
    let root = Root::new();
    let (path, manifest) = bundle(&root, "replacement");
    let source = original(&root);
    source.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;UPDATE sessions SET body='wal-original';").unwrap();
    let readonly = read_original(&root);
    assert_eq!(value(&readonly), "wal-original");
    drop(source);
    drop(readonly);
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert!(guard
        .restore_with_hook(
            &path,
            &manifest.sha256,
            |db| migrate(db, schema, |_| Ok(())),
            |_| Ok(()),
            |p| {
                if p == RestorePoint::WalMoved {
                    bail!("interrupt after WAL move");
                }
                Ok(())
            }
        )
        .is_err());
    assert_eq!(
        guard.recover_pending().unwrap(),
        RecoveryOutcome::RolledBack
    );
    let restored = read_original(&root);
    assert_eq!(value(&restored), "wal-original");
    drop(restored);
    assert!(!root.0.join("harness.sqlite-wal").exists());
    assert!(!root.0.join("harness.sqlite-shm").exists());
}

#[test]
fn forged_manifest_cannot_authorize_trigger_execution() {
    let root = Root::new();
    let (path, _) = bundle(&root, "replacement");
    let attacker = db("attacker");
    attacker.execute_batch("CREATE TRIGGER restore_override AFTER UPDATE ON tasks BEGIN DELETE FROM sessions; END;").unwrap();
    let bytes = attacker.serialize(MAIN_DB).unwrap().to_vec();
    let manifest = BackupManifest::for_bytes(&bytes, SCHEMA_VERSION);
    std::fs::write(path.join("database.sqlite"), &bytes).unwrap();
    std::fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert!(guard
        .restore(
            &path,
            &manifest.sha256,
            |db| migrate(db, schema, |_| Ok(())),
            |_| Ok(())
        )
        .is_err());
    assert!(!root.0.join(MAIN).exists());
    assert_eq!(guard.recover_pending().unwrap(), RecoveryOutcome::None);
}

#[test]
fn commit_marker_survives_interrupted_journal_cleanup() {
    let root = Root::new();
    let (path, manifest) = bundle(&root, "replacement");
    let orig = original(&root);
    drop(orig);
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert!(guard
        .restore_with_hook(
            &path,
            &manifest.sha256,
            |db| migrate(db, schema, |_| Ok(())),
            |_| Ok(()),
            |point| {
                if point == RestorePoint::Committed {
                    bail!("interrupt");
                }
                Ok(())
            }
        )
        .is_err());
    for name in SCRATCH {
        remove_if_present(&guard.dir, name).unwrap();
    }
    guard.dir.remove_file(JOURNAL).unwrap();
    drop(guard);
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert_eq!(guard.recover_pending().unwrap(), RecoveryOutcome::Completed);
    assert_eq!(guard.recover_pending().unwrap(), RecoveryOutcome::None);
}

#[test]
fn declared_page_and_manifest_budgets_reject_overflow_before_allocation() {
    assert_eq!(bounded_size(2048, 65536).unwrap(), MAX_DATABASE_BYTES);
    assert!(bounded_size(2049, 65536).is_err());
    assert!(bounded_size(u64::MAX, 4096).is_err());
    assert!(bounded_size(1, 0).is_err());
    assert!(bounded_size(1, 65537).is_err());
    let mut manifest = BackupManifest::for_bytes(b"fixture", SCHEMA_VERSION);
    manifest.bytes = MAX_DATABASE_BYTES + 1;
    assert!(manifest.verify(b"fixture").is_err());
}
#[test]
fn errors_do_not_echo_untrusted_manifest_values() {
    let root = Root::new();
    let (path, _) = bundle(&root, "private");
    std::fs::write(
        path.join("manifest.json"),
        br#"{"format":"secret-in-malformed-manifest","schema":1,"bytes":0,"sha256":"x"}"#,
    )
    .unwrap();
    let error = inspect_backup(&path).unwrap_err();
    assert!(!format!("{error:#}").contains("secret-in-malformed"));
}
#[test]
fn virtual_tables_are_rejected_independently_of_sql_whitespace() {
    let c = db("private");
    c.execute_batch("CREATE VIRTUAL TABLE virtual_fixture USING fts5(text)")
        .unwrap();
    assert!(validate_import(&c).is_err());
}

#[cfg(unix)]
#[test]
fn fifo_backup_and_recovery_inputs_fail_without_waiting_for_writer() {
    for name in ["manifest.json", "database.sqlite", JOURNAL] {
        let root = Root::new();
        let (path, _) = bundle(&root, "private");
        drop(original(&root));
        let before = std::fs::read(root.0.join(MAIN)).unwrap();
        let target = if name == JOURNAL {
            root.0.join(name)
        } else {
            path.join(name)
        };
        if target.exists() {
            std::fs::remove_file(&target).unwrap();
        }
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
        // SAFETY: live NUL-terminated path; fixture creates only a private FIFO.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        if name == JOURNAL {
            let guard = StorageGuard::acquire(&root.0).unwrap();
            // Direct read also covers a replacement between exists and open.
            assert!(read_file(&guard.dir, name, 8192).is_err());
            assert!(guard.recover_pending().is_err());
        } else {
            assert!(inspect_backup(&path).is_err());
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(std::fs::read(root.0.join(MAIN)).unwrap(), before);
    }
}

#[test]
fn inconsistent_existing_commit_marker_preserves_every_material() {
    for marker in ["f".repeat(64), "z".repeat(64), "abc".to_owned()] {
        let root = Root::new();
        let (path, manifest) = bundle(&root, "replacement");
        drop(original(&root));
        let guard = StorageGuard::acquire(&root.0).unwrap();
        assert!(guard
            .restore_with_hook(
                &path,
                &manifest.sha256,
                |db| migrate(db, schema, |_| Ok(())),
                |_| Ok(()),
                |point| {
                    if point == RestorePoint::Committed {
                        bail!("interrupt");
                    }
                    Ok(())
                }
            )
            .is_err());
        std::fs::write(root.0.join(COMMIT), marker).unwrap();
        let snapshot = || {
            let mut items: Vec<_> = std::fs::read_dir(&root.0)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (
                        entry.file_name(),
                        if entry.file_type().unwrap().is_file()
                            && entry.file_name() != "daemon.lock"
                        {
                            // The lease intentionally byte-locks daemon.lock
                            // on Windows. Compare every recovery material, not
                            // the unrelated lock's unreadable contents.
                            Some(std::fs::read(entry.path()).unwrap())
                        } else {
                            None
                        },
                    )
                })
                .collect();
            items.sort();
            items
        };
        let before = snapshot();
        assert!(guard.recover_pending().is_err());
        assert_eq!(snapshot(), before);
    }
}

#[test]
fn cancellation_after_snapshot_or_payload_never_publishes_bundle() {
    let root = Root::new();
    let source = db("private");
    // The first check after Backup::Done must observe cancellation.
    let calls = std::cell::Cell::new(0);
    assert!(
        consistent_image(&source, Instant::now() + Duration::from_secs(5), || {
            calls.set(calls.get() + 1);
            calls.get() >= 3
        })
        .is_err()
    );
    let path = root.0.join("cancelled");
    assert!(export_backup(
        &source,
        &root.0,
        "cancelled",
        Instant::now() + Duration::from_secs(5),
        || path.join("database.sqlite").exists()
    )
    .is_err());
    assert!(!path.exists());
    assert!(inspect_backup(&path).is_err());
    assert!(value(&source).contains("private"));
}

#[cfg(unix)]
#[test]
fn replaced_root_path_never_restores_into_pinned_or_replacement_directory() {
    let parent = Root::new();
    let root_path = parent.0.join("data");
    std::fs::create_dir(&root_path).unwrap();
    let root = Root(root_path.clone());
    let (bundle_path, manifest) = bundle(&parent, "replacement");
    drop(original(&root));
    let before_a = std::fs::read(root.0.join(MAIN)).unwrap();
    let guard = StorageGuard::acquire(&root.0).unwrap();
    let moved = parent.0.join("data-old");
    std::fs::rename(&root.0, &moved).unwrap();
    std::fs::create_dir(&root.0).unwrap();
    let b = original(&root);
    b.execute("UPDATE sessions SET body='different B'", [])
        .unwrap();
    drop(b);
    let before_b = std::fs::read(root.0.join(MAIN)).unwrap();
    assert!(guard
        .restore(
            &bundle_path,
            &manifest.sha256,
            |db| migrate(db, schema, |_| Ok(())),
            |_| Ok(())
        )
        .is_err());
    assert_eq!(std::fs::read(moved.join(MAIN)).unwrap(), before_a);
    assert_eq!(std::fs::read(root.0.join(MAIN)).unwrap(), before_b);
    assert!(!moved.join(JOURNAL).exists());
    assert!(!root.0.join(JOURNAL).exists());
}

#[test]
fn source_file_identity_change_during_prepare_is_rejected_without_restore_writes() {
    let root = Root::new();
    let (path, manifest) = bundle(&root, "replacement");
    drop(original(&root));
    let guard = StorageGuard::acquire(&root.0).unwrap();
    // Windows may deny renaming an open SQLite file. That refusal is itself
    // fail-closed and leaves the original untouched; Unix exercises the fence.
    let before = std::fs::read(root.0.join(MAIN)).unwrap();
    let moved = root.0.join("old-main");
    let result = guard.restore(
        &path,
        &manifest.sha256,
        |db| {
            std::fs::rename(root.0.join(MAIN), &moved)?;
            drop(original(&root));
            migrate(db, schema, |_| Ok(()))
        },
        |_| Ok(()),
    );
    assert!(result.is_err());
    if moved.exists() {
        assert_eq!(std::fs::read(&moved).unwrap(), before);
    } else {
        assert_eq!(std::fs::read(root.0.join(MAIN)).unwrap(), before);
    }
    assert!(!root.0.join(JOURNAL).exists());
}

#[test]
fn read_only_startup_rejects_newer_hot_journal_without_recovery_mutation() {
    let root = Root::new();
    let c = Connection::open(root.0.join(MAIN)).unwrap();
    c.execute_batch("PRAGMA page_size=4096; CREATE TABLE data(v); PRAGMA user_version=2;")
        .unwrap();
    drop(c);
    let main = std::fs::read(root.0.join(MAIN)).unwrap();
    // SQLite rollback journal header + one original page and its checksum.
    // A positive record count and no RESERVED lock make this journal hot.
    let mut journal = vec![0u8; 512];
    journal[..8].copy_from_slice(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]);
    for (offset, word) in [
        (8, 1u32),
        (12, 123),
        (16, (main.len() / 4096) as u32),
        (20, 512),
        (24, 4096),
    ] {
        journal[offset..offset + 4].copy_from_slice(&word.to_be_bytes());
    }
    journal.extend_from_slice(&1u32.to_be_bytes());
    journal.extend_from_slice(&main[..4096]);
    let mut checksum = 123u32;
    let mut index = 4096usize - 200;
    while index > 0 {
        checksum = checksum.wrapping_add(main[index] as u32);
        if index < 200 {
            break;
        }
        index -= 200;
    }
    journal.extend_from_slice(&checksum.to_be_bytes());
    let sidecar = root.0.join("harness.sqlite-journal");
    std::fs::write(&sidecar, &journal).unwrap();
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert!(guard.preflight_startup().is_err());
    assert_eq!(std::fs::read(root.0.join(MAIN)).unwrap(), main);
    assert_eq!(std::fs::read(&sidecar).unwrap(), journal);
    // Control proves this is not merely an inert sidecar: RW SQLite performs
    // rollback recovery on its first read and removes the journal.
    let control = Root::new();
    std::fs::write(control.0.join(MAIN), &main).unwrap();
    std::fs::write(control.0.join("harness.sqlite-journal"), &journal).unwrap();
    let c = Connection::open(control.0.join(MAIN)).unwrap();
    c.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .unwrap();
    assert!(!control.0.join("harness.sqlite-journal").exists());
}

#[test]
fn guarded_online_export_captures_wal_while_writer_stays_open() {
    let root = Root::new();
    let guard = StorageGuard::acquire(&root.0).unwrap();
    let source = original(&root);
    source.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE sessions SET body='online-wal-committed';").unwrap();
    assert!(root.0.join("harness.sqlite-wal").exists());
    let manifest = guard
        .export_backup(
            &root.0,
            "online",
            Instant::now() + Duration::from_secs(5),
            || false,
        )
        .unwrap();
    assert_eq!(inspect_backup(&root.0.join("online")).unwrap(), manifest);
    let copied =
        deserialize(&std::fs::read(root.0.join("online/database.sqlite")).unwrap()).unwrap();
    assert_eq!(value(&copied), "online-wal-committed");
    assert_eq!(guard.database_path(), root.0.join(MAIN));
    source
        .execute("UPDATE sessions SET body='writer-continues'", [])
        .unwrap();
    assert_eq!(value(&copied), "online-wal-committed");
}
#[cfg(unix)]
#[test]
fn guarded_export_rejects_replaced_root_before_bundle_publication() {
    for during_snapshot in [false, true] {
        let parent = Root::new();
        let path = parent.0.join("data");
        std::fs::create_dir(&path).unwrap();
        let root = Root(path);
        drop(original(&root));
        let guard = StorageGuard::acquire(&root.0).unwrap();
        let moved = parent.0.join("old-data");
        let replaced = std::cell::Cell::new(false);
        let checks = std::cell::Cell::new(0);
        let replace = || {
            std::fs::rename(&root.0, &moved).unwrap();
            std::fs::create_dir(&root.0).unwrap();
            drop(original(&root));
            replaced.set(true);
        };
        if !during_snapshot {
            replace();
        }
        assert!(guard
            .export_backup(
                &parent.0,
                "must-not-publish",
                Instant::now() + Duration::from_secs(5),
                || {
                    checks.set(checks.get() + 1);
                    if during_snapshot && !replaced.get() && checks.get() >= 4 {
                        replace();
                    }
                    false
                }
            )
            .is_err());
        assert!(replaced.get());
        assert!(!parent.0.join("must-not-publish").exists());
        assert!(!moved.join(JOURNAL).exists());
        assert!(!root.0.join(JOURNAL).exists());
    }
}
#[test]
fn create_and_acquire_builds_missing_private_directories_without_chmod_existing() {
    let root = Root::new();
    let path = root.0.join("missing/nested/data");
    let guard = StorageGuard::create_and_acquire(&path).unwrap();
    assert_eq!(guard.database_path(), path.join(MAIN));
    assert!(!guard.database_path().exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for relative in ["missing", "missing/nested", "missing/nested/data"] {
            assert_eq!(
                std::fs::metadata(root.0.join(relative))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }
    assert!(StorageGuard::create_and_acquire(&path).is_err());
}
#[cfg(unix)]
#[test]
fn create_and_acquire_rejects_existing_symlink_without_changing_target() {
    let root = Root::new();
    let target = root.0.join("target");
    std::fs::create_dir(&target).unwrap();
    let before = std::fs::metadata(&target).unwrap().permissions();
    std::os::unix::fs::symlink(&target, root.0.join("linked")).unwrap();
    assert!(StorageGuard::create_and_acquire(&root.0.join("linked/missing")).is_err());
    assert!(!target.join("missing").exists());
    assert!(!target.join("daemon.lock").exists());
    assert_eq!(std::fs::metadata(&target).unwrap().permissions(), before);
}

#[test]
fn v1_upgrade_skips_baseline_and_failed_v2_rolls_back_ddl_and_ledger() {
    let mut c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE storage_migrations(version INTEGER PRIMARY KEY,identity TEXT NOT NULL);INSERT INTO storage_migrations VALUES(1,'self-agent-schema-v1-atomic-baseline');PRAGMA user_version=1;").unwrap();
    assert!(migrate(
        &mut c,
        |_| panic!("v1 cannot run baseline"),
        |tx| {
            tx.execute_batch("CREATE TABLE upgrade_probe(id TEXT);")?;
            bail!("fixture upgrade failure")
        }
    )
    .is_err());
    assert_eq!(preflight(&c).unwrap(), 1);
    assert!(!table_exists(&c, "upgrade_probe").unwrap());
    migrate(
        &mut c,
        |_| panic!("v1 cannot run baseline"),
        |tx| {
            tx.execute_batch("CREATE TABLE upgrade_probe(id TEXT);")?;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(preflight(&c).unwrap(), 3);
    let records: Vec<(u32, String)> = c
        .prepare("SELECT version,identity FROM storage_migrations ORDER BY version")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        records,
        vec![
            (1, LEDGER_V1.into()),
            (2, LEDGER_V2.into()),
            (3, LEDGER_V3.into())
        ]
    );
    migrate(&mut c, |_| panic!("v2 baseline"), |_| panic!("v2 upgrade")).unwrap();
}
#[test]
fn fresh_v2_failure_rolls_back_even_the_baseline_and_readonly_ledger_preflight_writes_nothing() {
    let mut c = Connection::open_in_memory().unwrap();
    assert!(migrate(&mut c, schema, |_| bail!("fixture")).is_err());
    assert_eq!(preflight(&c).unwrap(), 0);
    assert!(!table_exists(&c, "sessions").unwrap());
    assert!(!table_exists(&c, "storage_migrations").unwrap());
    let root = Root::new();
    let path = root.0.join("ledger.sqlite");
    let mut disk = Connection::open(&path).unwrap();
    migrate(&mut disk, schema, |_| Ok(())).unwrap();
    drop(disk);
    let before = std::fs::read(&path).unwrap();
    let readonly = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(preflight(&readonly).unwrap(), 3);
    drop(readonly);
    assert_eq!(before, std::fs::read(&path).unwrap());
    let c = Connection::open(&path).unwrap();
    c.execute(
        "UPDATE storage_migrations SET identity='tampered' WHERE version=1",
        [],
    )
    .unwrap();
    drop(c);
    let before = std::fs::read(&path).unwrap();
    let readonly = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(preflight(&readonly).is_err());
    drop(readonly);
    assert_eq!(before, std::fs::read(&path).unwrap());
}

#[test]
fn directory_identity_uses_open_handle_and_empty_root_preflight_succeeds() {
    let root = Root::new();
    let dir = open_directory(&root.0).unwrap();
    let cloned = cap_std::fs::File::from_std(dir.try_clone().unwrap().into_std_file());
    assert_eq!(
        directory_identity(&dir).unwrap(),
        file_identity(&cloned).unwrap()
    );
    let guard = StorageGuard::acquire(&root.0).unwrap();
    assert_eq!(guard.preflight_startup().unwrap(), None);
    drop(original(&root));
    assert_eq!(guard.preflight_startup().unwrap(), Some(SCHEMA_VERSION));
}

#[test]
fn restore_reaches_commit_hook_with_directory_identity_checks_enabled() {
    let root = Root::new();
    let (path, manifest) = bundle(&root, "replacement");
    drop(original(&root));
    let guard = StorageGuard::acquire(&root.0).unwrap();
    let reached = std::cell::Cell::new(false);
    let result = guard.restore_with_hook(
        &path,
        &manifest.sha256,
        |db| migrate(db, schema, |_| Ok(())),
        |_| Ok(()),
        |point| {
            if point == RestorePoint::Committed {
                reached.set(true);
                bail!("fixture interrupt after commit");
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(
        reached.get(),
        "restore failed before the intended interruption hook: {result:?}"
    );
    assert_eq!(guard.recover_pending().unwrap(), RecoveryOutcome::Completed);
}
