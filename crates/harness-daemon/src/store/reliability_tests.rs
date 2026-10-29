use super::*;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::PathBuf, time::Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("self-agent-restore-records-{}", new_id()));
        std::fs::create_dir(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn store(&self, name: &str) -> Store {
        let directory = self.0.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        Store::open(&directory.join("harness.sqlite")).unwrap()
    }
    fn bundle(&self, store: &Store, name: &str) -> PathBuf {
        storage::export_backup(
            &store.db,
            &self.0,
            name,
            Instant::now() + Duration::from_secs(10),
            || false,
        )
        .unwrap();
        self.0.join(name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn session(id: &str) -> Value {
    json!({"id":id,"title":"真实结构","project_path":"fixture","created_at":"then","updated_at":"now","messages":[{"id":"message","role":"user","content":"private fixture history","created_at":"then"}]})
}
fn source() -> Value {
    json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"local"})
}
fn legacy_record(table: &str, id: &str) -> Value {
    match table {
        "sessions" => session(id),
        "tasks" => {
            json!({"id":id,"session_id":"session","title":"task","status":"completed","execution_mode":"demo","created_at":"then","updated_at":"now","source":source(),"steps":[]})
        }
        "plans" => {
            json!({"id":id,"session_id":"session","title":"plan","source":source(),"execution_mode":"demo","status":"completed","revision":1,"round":1,"max_rounds":1,"created_at":"then","updated_at":"now","nodes":[]})
        }
        "plan_changes" => {
            json!({"id":id,"plan_id":"plan","session_id":"session","base_revision":1,"source":source(),"content":"change","status":"discarded","created_at":"then","updated_at":"now"})
        }
        "deliveries" => {
            json!({"id":id,"task_id":"task","session_id":"session","target":source(),"status":"sent","payload":"done","attempts":1,"created_at":"then","updated_at":"now"})
        }
        "events" => json!({"id":id,"kind":"legacy","message":"event","created_at":"then"}),
        _ => unreachable!(),
    }
}
fn put_body(db: &Connection, table: &str, id: &str, value: &Value) {
    if table == "deliveries" {
        db.execute(
            "INSERT OR REPLACE INTO deliveries(id,task_id,body) VALUES(?1,'task',?2)",
            params![id, value.to_string()],
        )
        .unwrap();
    } else {
        db.execute(
            &format!("INSERT OR REPLACE INTO {table}(id,body) VALUES(?1,?2)"),
            params![id, value.to_string()],
        )
        .unwrap();
    }
}
fn forge(bundle: &Path, mutation: impl FnOnce(&Connection)) -> String {
    let image = bundle.join("database.sqlite");
    let db = Connection::open(&image).unwrap();
    mutation(&db);
    db.close().unwrap();
    let bytes = std::fs::read(image).unwrap();
    let sha = format!("{:x}", Sha256::digest(&bytes));
    let manifest = storage::BackupManifest {
        format: 1,
        schema: storage::SCHEMA_VERSION,
        bytes: bytes.len(),
        sha256: sha.clone(),
    };
    std::fs::write(
        bundle.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    // A matching hash and SQLite integrity are intentionally insufficient.
    assert!(crate::maintenance::inspect_backup(bundle).is_ok());
    sha
}
fn rejected_without_replacement(f: &Fixture, bundle: &Path, sha: &str) {
    let original = f.store("destination");
    put_body(&original.db, "sessions", "original", &session("original"));
    original
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(original);
    let target = f.0.join("destination");
    let path = target.join("harness.sqlite");
    let before = std::fs::read(&path).unwrap();
    let outcome =
        std::panic::catch_unwind(|| crate::maintenance::restore_backup(&target, bundle, sha));
    assert!(
        outcome.is_ok(),
        "invalid backup must return an error, not panic"
    );
    assert!(
        outcome.unwrap().is_err(),
        "invalid typed records/schema must fail before publish"
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        before,
        "the destination image must remain byte-for-byte unchanged"
    );
    assert!(!target.join(".restore-journal.json").exists());
    assert!(!target.join(".restore-new.sqlite").exists());
    let reopened = Store::open(&target.join("harness.sqlite")).unwrap();
    assert_eq!(
        reopened.snapshot().unwrap()["sessions"][0]["id"],
        "original"
    );
}

#[test]
fn required_schema_tracks_every_current_compiled_store_table_and_column() {
    let f = Fixture::new();
    let store = f.store("source");
    let actual = store.db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name<>'storage_migrations' ORDER BY name").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<BTreeSet<_>>>().unwrap();
    let expected = REQUIRED_SCHEMA
        .iter()
        .map(|(table, _)| table.to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual, expected,
        "compiled migrations changed: update trusted restore schema requirements"
    );
    for (table, columns) in REQUIRED_SCHEMA {
        let actual = store
            .db
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .unwrap();
        assert_eq!(
            actual,
            columns
                .iter()
                .map(|(name, _, _, _)| name.to_string())
                .collect(),
            "{table}"
        );
    }
    validate_records(&store.db).unwrap();
}
#[test]
fn every_required_table_and_column_is_checked_even_when_empty() {
    let f = Fixture::new();
    let store = f.store("source");
    for (table, columns) in REQUIRED_SCHEMA {
        store.db.execute_batch("SAVEPOINT damaged;").unwrap();
        store
            .db
            .execute_batch(&format!("PRAGMA foreign_keys=OFF; DROP TABLE {table};"))
            .unwrap();
        assert!(validate_records(&store.db).is_err(), "missing {table}");
        store
            .db
            .execute_batch("ROLLBACK TO damaged; RELEASE damaged;")
            .unwrap();
        for (column, _, _, _) in *columns {
            store.db.execute_batch("SAVEPOINT damaged;").unwrap();
            store
                .db
                .execute_batch(&format!(
                    "ALTER TABLE {table} RENAME COLUMN {column} TO invalid_column;"
                ))
                .unwrap();
            assert!(
                validate_records(&store.db).is_err(),
                "missing {table}.{column}"
            );
            store
                .db
                .execute_batch("ROLLBACK TO damaged; RELEASE damaged;")
                .unwrap();
        }
    }
}
#[test]
fn actual_schema_one_backup_missing_commands_or_receipt_tables_cannot_replace_destination() {
    for table in [
        "commands",
        "selections",
        "provider_commands",
        "capability_commands",
        "qq_admin_commands",
        "qq_inbox",
        "backup_jobs",
    ] {
        let f = Fixture::new();
        let store = f.store("source");
        let bundle = f.bundle(&store, "backup");
        drop(store);
        let sha = forge(&bundle, |db| {
            db.execute_batch(&format!("DROP TABLE {table};")).unwrap();
        });
        rejected_without_replacement(&f, &bundle, &sha);
    }
}
#[test]
fn actual_backup_with_corrupt_public_nested_records_is_rejected_before_replace() {
    for (table, field, bad) in [
        ("sessions", "messages", json!("bad")),
        ("tasks", "steps", json!({})),
        ("plans", "nodes", json!("bad")),
        ("plan_changes", "nodes", json!({})),
        ("deliveries", "target", json!("bad")),
        (
            "events",
            "data",
            json!({"kind":"assistant_delta","text":42}),
        ),
    ] {
        let f = Fixture::new();
        let store = f.store("source");
        put_body(&store.db, table, "record", &legacy_record(table, "record"));
        let bundle = f.bundle(&store, "backup");
        drop(store);
        let sha = forge(&bundle, |db| {
            let mut value = legacy_record(table, "record");
            value[field] = bad;
            put_body(db, table, "record", &value);
        });
        rejected_without_replacement(&f, &bundle, &sha);
    }
}
#[test]
fn actual_backup_rejects_private_mcp_provider_and_controls_shapes_without_panic() {
    for (table, body) in [
        (
            "managed_capabilities",
            json!({"id":"record","kind":"mcp","definition":"bad"}),
        ),
        (
            "managed_capabilities",
            json!({"id":"record","kind":"mcp","definition":{"name":"fixture","program":"/fixture/server","args":"bad"}}),
        ),
        (
            "named_providers",
            json!({"id":"record","definition":{"name":"fixture","base_url":42}}),
        ),
        (
            "plan_role_providers",
            json!({"id":"record","roles":{"coder":{"definition":"bad"}}}),
        ),
        ("tasks", {
            let mut task = legacy_record("tasks", "record");
            task["controls"] = json!({"budget":"bad"});
            task
        }),
    ] {
        let f = Fixture::new();
        let store = f.store("source");
        let bundle = f.bundle(&store, "backup");
        drop(store);
        let sha = forge(&bundle, |db| put_body(db, table, "record", &body));
        rejected_without_replacement(&f, &bundle, &sha);
    }
}
#[test]
fn actual_legacy_serde_defaults_and_deleted_capability_tombstones_survive_restore() {
    let f = Fixture::new();
    let store = f.store("source");
    for table in [
        "sessions",
        "tasks",
        "plans",
        "plan_changes",
        "deliveries",
        "events",
    ] {
        put_body(&store.db, table, "record", &legacy_record(table, "record"));
    }
    put_body(
        &store.db,
        "managed_capabilities",
        "deleted",
        &json!({"id":"deleted","kind":"mcp","revision":2,"deleted":true,"enabled":false,"updated_at":"then"}),
    );
    validate_records(&store.db).unwrap();
    let bundle = f.bundle(&store, "backup");
    let manifest = crate::maintenance::inspect_backup(&bundle).unwrap();
    drop(store);
    crate::maintenance::restore_backup(
        &f.0.join("restored"),
        &bundle,
        manifest["sha256"].as_str().unwrap(),
    )
    .unwrap();
    let restored = Store::open(&f.0.join("restored/harness.sqlite")).unwrap();
    let snapshot = restored.snapshot().unwrap();
    assert_eq!(
        snapshot["sessions"][0]["messages"][0]["content"],
        "private fixture history"
    );
    assert_eq!(snapshot["events"][0]["sequence"], 1);
    assert!(snapshot["tasks"][0]["agent"].is_object());
}

#[test]
fn diagnostics_count_qq_unknown_credentials_without_exposing_private_records() {
    let f = Fixture::new();
    let store = f.store("source");
    let id = new_id();
    let canary = "private-qq-credential-reference-canary";
    put_body(
        &store.db,
        "qq_credential_operations",
        &id,
        &json!({
            "id":id,"owner_kind":"qq","owner_id":new_id(),"kind":"set","status":"unknown",
            "reason":null,"can_cleanup":false,"created_at":"then","updated_at":"now",
            "reference":canary,"private_path":"/private/qq-fixture","secret":canary
        }),
    );
    let bytes = store.diagnostic_export().unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains(canary));
    assert!(!text.contains("/private/qq-fixture"));
    assert!(!text.contains(&id));
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["counts"]["credentials_unknown"], 1);
    assert_eq!(value["vault"], "not_checked");
    assert_eq!(value["connection_check_history"], "not_recorded");
}

#[test]
fn extra_required_column_in_forged_backup_cannot_break_restored_command_inserts() {
    let f = Fixture::new();
    let store = f.store("source");
    let bundle = f.bundle(&store, "backup");
    drop(store);
    let sha = forge(&bundle, |db| {
        db.execute_batch("ALTER TABLE commands ADD COLUMN unexpected TEXT NOT NULL;")
            .unwrap();
    });
    rejected_without_replacement(&f, &bundle, &sha);
}

#[test]
fn forged_events_without_autoincrement_or_with_comment_keyword_are_rejected() {
    for allocator in [
        "INTEGER PRIMARY KEY",
        "INTEGER PRIMARY KEY /* AUTOINCREMENT */",
        "INTEGER PRIMARY KEY AUTOINCREMENT /* harmless-looking extra */",
    ] {
        let f = Fixture::new();
        let store = f.store("source");
        let bundle = f.bundle(&store, "backup");
        drop(store);
        let sha = forge(&bundle, |db| {
            db.execute_batch(&format!("DROP TABLE events; CREATE TABLE events(sequence {allocator},id TEXT NOT NULL UNIQUE,body TEXT NOT NULL);")).unwrap();
        });
        rejected_without_replacement(&f, &bundle, &sha);
    }
}
#[test]
fn event_ddl_accepts_published_alter_quotes_but_not_extra_tokens_or_quoted_keywords() {
    let f = Fixture::new();
    let store = f.store("source");
    let actual: String = store
        .db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='events'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let expected = event_schema_tokens(&actual).unwrap();
    let at_limit = format!("{actual}{}", " ".repeat(4096 - actual.len()));
    assert_eq!(event_schema_tokens(&at_limit).unwrap(), expected);
    assert!(event_schema_tokens(&format!("{at_limit} ")).is_err());
    for quoted in ["create table \"events\" ( \"sequence\" integer primary key autoincrement, \"id\" text not null unique, \"body\" text not null)","CREATE TABLE [events] (`sequence` INTEGER PRIMARY KEY AUTOINCREMENT, [id] TEXT NOT NULL UNIQUE, `body` TEXT NOT NULL)"] {
        assert_eq!(event_schema_tokens(quoted).unwrap(),expected);
    }
    for invalid in ["CREATE TABLE events(sequence INTEGER PRIMARY KEY /* AUTOINCREMENT */ ,id TEXT NOT NULL UNIQUE,body TEXT NOT NULL)","CREATE TABLE events(sequence INTEGER PRIMARY KEY \"AUTOINCREMENT\",id TEXT NOT NULL UNIQUE,body TEXT NOT NULL)","CREATE TABLE events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,body TEXT NOT NULL); DELETE FROM commands"] {
        assert!(event_schema_tokens(invalid).is_err());
    }
}

fn downgrade_actual_store_to_v1(db: &Connection) {
    for (table, _) in REQUIRED_SCHEMA
        .iter()
        .filter(|(name, _)| name.starts_with("gateway_"))
    {
        db.execute_batch(&format!("DROP TABLE {table};")).unwrap();
    }
    db.execute_batch("DELETE FROM storage_migrations WHERE version=2; PRAGMA user_version=1;")
        .unwrap();
}
#[test]
fn real_v1_migration_only_adds_gateway_preserves_history_and_ledger_identity() {
    let f = Fixture::new();
    let mut store = f.store("v1");
    put_body(&store.db, "sessions", "session", &session("session"));
    downgrade_actual_store_to_v1(&store.db);
    let before: String = store
        .db
        .query_row("SELECT body FROM sessions WHERE id='session'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(storage::preflight(&store.db).unwrap(), 1);
    super::super::prepare_database(&mut store.db).unwrap();
    assert_eq!(storage::preflight(&store.db).unwrap(), 2);
    let after: String = store
        .db
        .query_row("SELECT body FROM sessions WHERE id='session'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(before, after);
    validate_records(&store.db).unwrap();
    let ids: Vec<String> = store
        .db
        .prepare("SELECT identity FROM storage_migrations ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        ids,
        [
            "self-agent-schema-v1-atomic-baseline",
            "self-agent-schema-v2-gateway"
        ]
    );
}
#[test]
fn malformed_v1_missing_column_or_unique_is_not_repaired_or_partially_upgraded() {
    for damage in ["ALTER TABLE tasks RENAME COLUMN body TO wrong", "ALTER TABLE deliveries RENAME TO saved_deliveries;CREATE TABLE deliveries(id TEXT PRIMARY KEY,task_id TEXT NOT NULL,body TEXT NOT NULL);DROP TABLE saved_deliveries;"] {
        let f=Fixture::new();let mut store=f.store("v1");downgrade_actual_store_to_v1(&store.db);store.db.execute_batch(damage).unwrap();
        assert!(super::super::prepare_database(&mut store.db).is_err());assert_eq!(storage::preflight(&store.db).unwrap(),1);
        assert_eq!(store.db.query_row("SELECT count(*) FROM sqlite_schema WHERE name LIKE 'gateway_%'",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    }
}
#[test]
fn real_v1_failed_gateway_upgrade_rolls_back_all_gateway_tables() {
    let f = Fixture::new();
    let mut store = f.store("v1");
    downgrade_actual_store_to_v1(&store.db);
    assert!(storage::migrate(
        &mut store.db,
        |_| panic!("must not run baseline"),
        |tx| {
            validate_v1_records(tx)?;
            super::super::gateway::migrate(tx)?;
            bail!("fixture after gateway DDL")
        }
    )
    .is_err());
    assert_eq!(storage::preflight(&store.db).unwrap(), 1);
    validate_v1_records(&store.db).unwrap();
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'gateway_%'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn actual_restore_revokes_gateway_authority_and_preserves_inert_audit_and_text() {
    use crate::capabilities::vault::{FreshCredential, SecretBytes};
    use crate::gateway::{configuration as config, credentials, persistence, resources};
    let f = Fixture::new();
    let mut store = f.store("gateway-source");
    let device = new_id();
    let principal = new_id();
    let pairing = new_id();
    let task = new_id();
    let session_id = new_id();
    let op = new_id();
    let command = new_id();
    let tx = store.db.transaction().unwrap();
    let c = config::create(&tx, "https://gateway.example.invalid/", &device).unwrap();
    let scoped = crate::gateway::credential_binding(&c.id, &c.scope_digest).unwrap();
    let fresh = FreshCredential::new(
        &scoped,
        SecretBytes::new(b"fixture-never-written".to_vec()).unwrap(),
    );
    let c = config::attach_credential(
        &tx,
        &c.id,
        c.revision,
        fresh.reference().as_str(),
        |_, _| Ok(()),
    )
    .unwrap();
    let c = config::update(
        &tx,
        &c.id,
        c.revision,
        config::ConfigPatch {
            origin: c.origin.clone(),
            device_id: device.clone(),
            enabled: true,
        },
        1,
        |_| Ok(true),
    )
    .unwrap();
    let ch = persistence::TrustedChannel {
        device_id: device.clone(),
        config_revision: c.revision,
    };
    let issued = persistence::issue_pairing(&tx, &ch, 1, 60).unwrap();
    let grant = crate::gateway::wire::GatewayGrant {
        session_ids: vec![session_id.clone()],
        permissions: Default::default(),
        controls: serde_json::to_value(harness_core::TaskControls::default()).unwrap(),
        expires_at_ms: 60000,
    };
    let b = persistence::Binding {
        principal_id: principal.clone(),
        device_id: device.clone(),
        config_revision: c.revision,
        revision: 1,
        status: "active".into(),
        label: "fixture browser".into(),
        pairing_id: pairing,
        expires_at_ms: 60000,
        grant: Some(grant),
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    tx.execute(
        "INSERT INTO gateway_bindings VALUES(?1,?2,?3,1,'active',60000,?4)",
        params![
            principal,
            device,
            c.revision,
            serde_json::to_string(&b).unwrap()
        ],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO gateway_receipts VALUES(?1,?2,?3,?4,1,?5,?6,'{}',0,1)",
        params![
            command,
            device,
            c.revision,
            principal,
            session_id,
            "a".repeat(64)
        ],
    )
    .unwrap();
    let next = FreshCredential::new(
        &scoped,
        SecretBytes::new(b"another-fixture-never-written".to_vec()).unwrap(),
    );
    credentials::begin_set(
        &tx,
        &op,
        &c.id,
        c.revision,
        next.reference().as_str(),
        |_, _, _| Ok(()),
    )
    .unwrap();
    credentials::claim(&tx, &op, |_, _, _| Ok(())).unwrap();
    let resource = resources::mint(
        &tx,
        &task,
        &session_id,
        "result",
        "result.txt",
        "historical redacted text",
    )
    .unwrap();
    tx.commit().unwrap();
    validate_records(&store.db).unwrap();
    let bundle = f.bundle(&store, "gateway-backup");
    let manifest = storage::inspect_backup(&bundle).unwrap();
    let target = f.0.join("gateway-restored");
    crate::maintenance::restore_backup(&target, &bundle, &manifest.sha256).unwrap();
    let mut restored = Store::open(&target.join("harness.sqlite")).unwrap();
    let c = config::current(&restored.db).unwrap().unwrap();
    assert!(!c.enabled);
    assert!(c.credential_ref().is_none());
    let tx = restored.db.transaction().unwrap();
    assert_eq!(
        persistence::binding(&tx, &principal).unwrap().status,
        "revoked"
    );
    assert_eq!(
        tx.query_row(
            "SELECT status FROM gateway_pairings WHERE id=?1",
            [issued.id],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "revoked"
    );
    assert_eq!(
        tx.query_row(
            "SELECT invalidated FROM gateway_receipts WHERE command_id=?1",
            [command],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        1
    );
    let operation = credentials::operation(&tx, &op).unwrap();
    assert_eq!(operation.status, "unknown");
    assert!(!operation.can_cleanup);
    assert!(credentials::claim(&tx, &op, |_, _, _| panic!("no replay"))
        .unwrap()
        .is_none());
    assert!(credentials::begin_cleanup(&tx, &new_id(), &op).is_err());
    assert_eq!(
        resources::read(&tx, &resource.id, &task, &session_id)
            .unwrap()
            .content,
        "historical redacted text"
    );
    assert!(resources::read(&tx, &resource.id, &task, &new_id()).is_err());
    assert_eq!(
        tx.query_row("SELECT count(*) FROM gateway_connection", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn gateway_unique_keys_are_compiled_requirements_not_imported_assumptions() {
    let f = Fixture::new();
    let store = f.store("unique");
    for (table, from, to) in [
        (
            "gateway_configuration",
            "id TEXT NOT NULL UNIQUE",
            "id TEXT NOT NULL",
        ),
        (
            "gateway_pairings",
            "code_hash TEXT NOT NULL UNIQUE",
            "code_hash TEXT NOT NULL",
        ),
        (
            "gateway_text_resources",
            ",\n        UNIQUE(task_id,kind)",
            "",
        ),
    ] {
        store.db.execute_batch("SAVEPOINT damage;").unwrap();
        let ddl: String = store
            .db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        let modified = ddl.replace(from, to);
        assert_ne!(ddl, modified, "fixture must remove constraint: {table}");
        store
            .db
            .execute_batch(&format!("DROP TABLE {table};{modified};"))
            .unwrap();
        assert!(
            validate_records(&store.db).is_err(),
            "missing unique: {table}"
        );
        store
            .db
            .execute_batch("ROLLBACK TO damage; RELEASE damage;")
            .unwrap();
    }
}
#[test]
fn forged_gateway_text_and_unscoped_vault_reference_reject_before_destination_replace() {
    for kind in ["resource_hash", "credential_scope"] {
        let f = Fixture::new();
        let mut store = f.store("source");
        let tx = store.db.transaction().unwrap();
        let c = crate::gateway::configuration::create(
            &tx,
            "https://gateway.example.invalid/",
            &new_id(),
        )
        .unwrap();
        let resource = crate::gateway::resources::mint(
            &tx,
            &new_id(),
            &new_id(),
            "result",
            "result.txt",
            "safe historical text",
        )
        .unwrap();
        tx.commit().unwrap();
        let bundle = f.bundle(&store, "gateway-forged");
        drop(store);
        let sha = forge(&bundle, |db| {
            if kind == "resource_hash" {
                db.execute(
                    "UPDATE gateway_text_resources SET sha256=?1 WHERE id=?2",
                    params!["0".repeat(64), resource.id],
                )
                .unwrap();
            } else {
                db.execute("UPDATE gateway_configuration SET credential_ref='unscoped-reference' WHERE id=?1",[c.id]).unwrap();
            }
        });
        rejected_without_replacement(&f, &bundle, &sha);
    }
}
