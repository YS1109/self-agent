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
