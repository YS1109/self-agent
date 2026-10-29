use super::*;
use crate::{capabilities::vault::test_support, maintenance::DataLease, store::Store, Settings};
use harness_core::new_id;
use std::path::Path;
const OWNER: &str = "fixture-backup-owner";
const PRIVATE: &str = "fixture-private-diagnostic-canary";
struct Fixture {
    root: PathBuf,
    state: Arc<AppState>,
    client: reqwest::Client,
    base: String,
    http: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn open() -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("sa-maint-{}", new_id()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let lease = DataLease::acquire(&root.join("data")).unwrap();
        let (vault, _) = test_support::memory(8).unwrap();
        let state = AppState::open_with_vault_and_lease(
            &lease.database_path(),
            OWNER.into(),
            vault,
            Some(lease.clone()),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::router(state.clone());
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            root,
            state,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            base,
            http,
        }
    }
    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn post(&self, id: &str, name: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/api/v1/reliability/backups", self.base))
            .bearer_auth(OWNER)
            .json(&json!({"id":id,"parent":self.root,"name":name}))
            .send()
            .await
            .unwrap()
    }
    async fn completed(&self, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let receipt = self.get(&format!("/api/v1/reliability/backups/{id}")).await;
                match receipt["state"].as_str() {
                    Some("completed") => return receipt,
                    Some("pending" | "running") => {}
                    _ => panic!("backup did not complete: {receipt}"),
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn close(self) {
        self.http.abort();
        let _ = self.http.await;
        drop(self.client);
        self.state.shutdown_credentials().await;
        drop(self.state);
        let _ = std::fs::remove_dir_all(self.root);
    }
    fn seed(&self) -> String {
        let project = self.root.join(PRIVATE);
        std::fs::create_dir(&project).unwrap();
        let settings = Settings {
            execution_mode: "demo".into(),
            ..Settings::default()
        };
        let source = json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"local"});
        let mut store = self.state.store.lock().unwrap();
        let session=store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"create_session","title":PRIVATE,"project_path":project}}),&settings).unwrap().0;
        store.command(json!({"command_id":new_id(),"source":source,"command":{"type":"run_agent","session_id":session["session_id"],"content":PRIVATE}}),&settings).unwrap().0["task_id"].as_str().unwrap().into()
    }
}
#[tokio::test]
async fn owner_http_backup_is_real_idempotent_private_and_restore_never_replays_queued_work() {
    let f = Fixture::open().await;
    let task = f.seed();
    let id = new_id();
    let response = f.post(&id, "snapshot").await;
    assert!(response.status().is_success());
    drop(response);
    let receipt = f.completed(&id).await;
    let manifest = &receipt["manifest"];
    assert_eq!(
        crate::maintenance::inspect_backup(&f.root.join("snapshot")).unwrap(),
        *manifest
    );
    assert_eq!(
        f.post(&id, "snapshot").await.json::<Value>().await.unwrap(),
        receipt
    );
    assert_eq!(
        f.post(&id, "different").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(!f.root.join("different").exists());
    assert!(!receipt.to_string().contains(f.root.to_str().unwrap()));
    let diagnostics = f.get("/api/v1/reliability/diagnostics").await;
    assert!(!diagnostics.to_string().contains(PRIVATE));
    assert!(!diagnostics.to_string().contains(f.root.to_str().unwrap()));
    assert_eq!(diagnostics["counts"]["tasks_pending"], 1);
    assert_eq!(diagnostics["connection_check_history"], "not_recorded");
    assert_eq!(diagnostics["vault"], "not_checked");
    let sha = manifest["sha256"].as_str().unwrap();
    assert!(
        crate::maintenance::restore_backup(&f.root.join("data"), &f.root.join("snapshot"), sha)
            .is_err(),
        "active daemon lease must block restore"
    );
    let target = f.root.join("restored");
    crate::maintenance::restore_backup(&target, &f.root.join("snapshot"), sha).unwrap();
    let restored = Store::open(&target.join("harness.sqlite")).unwrap();
    let snapshot = restored.snapshot().unwrap();
    let restored_task = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == task)
        .unwrap();
    assert_eq!(restored_task["status"], "failed");
    assert_eq!(restored_task["stop_reason"], "backup_restored");
    assert!(restored
        .backup_receipts()
        .unwrap()
        .iter()
        .all(|r| !matches!(
            r.state,
            jobs::BackupState::Pending | jobs::BackupState::Running
        )));
    assert_eq!(
        f.state.store.lock().unwrap().snapshot().unwrap()["tasks"][0]["status"],
        "queued"
    );
    drop(restored);
    f.close().await;
}
#[tokio::test]
async fn backup_routes_require_owner_and_capacity_failure_creates_no_intent() {
    let f = Fixture::open().await;
    let id = new_id();
    assert_eq!(
        f.client
            .get(format!("{}/api/v1/reliability", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let permits = f
        .state
        .backup_gate
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    assert_eq!(
        f.post(&id, "blocked").await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert!(f
        .state
        .store
        .lock()
        .unwrap()
        .backup_receipt(&id)
        .unwrap()
        .is_none());
    assert!(!f.root.join("blocked").exists());
    drop(permits);
    assert_eq!(
        f.client
            .get(format!(
                "{}/api/v1/reliability/backups/{}",
                f.base,
                new_id()
            ))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    f.close().await;
}
#[test]
fn actual_store_migrates_legacy_events_and_rejects_newer_database_before_writes() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("sa-schema-{}", new_id()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("harness.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE events(id TEXT PRIMARY KEY,body TEXT NOT NULL);INSERT INTO events VALUES('old-event','{\"id\":\"old-event\",\"kind\":\"legacy\",\"message\":\"历史事件\",\"created_at\":\"2026-01-01T00:00:00Z\"}');").unwrap();
    drop(db);
    let store = Store::open(&path).unwrap();
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(storage::preflight(&db).unwrap(), storage::SCHEMA_VERSION);
    assert_eq!(
        db.query_row("SELECT id FROM events WHERE sequence=1", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "old-event"
    );
    db.execute_batch(
        "PRAGMA wal_checkpoint(TRUNCATE);PRAGMA journal_mode=DELETE;PRAGMA user_version=999;",
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(Store::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn invalid_legacy_record_rolls_back_all_schema_and_ledger_changes() {
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE settings(id TEXT PRIMARY KEY,body TEXT NOT NULL);INSERT INTO settings VALUES('provider','[]');").unwrap();
    assert!(crate::store::prepare_database(&mut db).is_err());
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='storage_migrations' OR name='tasks'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT body FROM settings", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "[]"
    );
}
#[test]
fn offline_inspection_rejects_relative_bundle_paths() {
    assert!(crate::maintenance::inspect_backup(Path::new("relative")).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn managed_open_rejects_replaced_directory_before_any_database_or_probe_writes() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("sa-lease-{}", new_id()));
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let selected = root.join("selected");
    let other = root.join("other");
    std::fs::create_dir(&selected).unwrap();
    std::fs::create_dir(&other).unwrap();
    drop(Store::open(&selected.join("harness.sqlite")).unwrap());
    drop(Store::open(&other.join("harness.sqlite")).unwrap());
    let a = std::fs::read(selected.join("harness.sqlite")).unwrap();
    let b = std::fs::read(other.join("harness.sqlite")).unwrap();
    let lease = DataLease::acquire(&selected).unwrap();
    std::fs::rename(&selected, root.join("original")).unwrap();
    std::fs::rename(&other, &selected).unwrap();
    let (vault, _) = test_support::memory(8).unwrap();
    assert!(AppState::open_with_vault_and_lease(
        &lease.database_path(),
        OWNER.into(),
        vault,
        Some(lease.clone())
    )
    .is_err());
    assert_eq!(
        std::fs::read(root.join("original/harness.sqlite")).unwrap(),
        a
    );
    assert_eq!(std::fs::read(selected.join("harness.sqlite")).unwrap(), b);
    assert!(!selected.join("capability-probes").exists());
    assert!(lease
        .write_daemon_metadata(&harness_core::DaemonInfo {
            base_url: "http://127.0.0.1:1".into(),
            token: "fixture".into(),
            pid: 1
        })
        .is_err());
    assert!(!selected.join("daemon.json").exists());
    drop(lease);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn managed_open_rejects_replaced_main_file_after_lease_acquisition() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("sa-lease-file-{}", new_id()));
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let main = root.join("harness.sqlite");
    drop(Store::open(&main).unwrap());
    let before = std::fs::read(&main).unwrap();
    let lease = DataLease::acquire(&root).unwrap();
    std::fs::rename(&main, root.join("original.sqlite")).unwrap();
    std::fs::write(&main, &before).unwrap();
    let (vault, _) = test_support::memory(8).unwrap();
    assert!(
        AppState::open_with_vault_and_lease(&main, OWNER.into(), vault, Some(lease.clone()))
            .is_err()
    );
    assert_eq!(std::fs::read(&main).unwrap(), before);
    assert_eq!(std::fs::read(root.join("original.sqlite")).unwrap(), before);
    assert!(!root.join("capability-probes").exists());
    drop(lease);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn online_backup_never_switches_from_open_store_to_replaced_main_file() {
    for preexisting in [false, true] {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("sa-online-file-{}", new_id()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        let main = data.join("harness.sqlite");
        if preexisting {
            drop(Store::open(&main).unwrap());
        }
        let lease = DataLease::acquire(&data).unwrap();
        let (vault, _) = test_support::memory(8).unwrap();
        let state =
            AppState::open_with_vault_and_lease(&main, OWNER.into(), vault, Some(lease.clone()))
                .unwrap();
        // A complete ordinary replacement, not a racing ABA or linked path.
        let other = root.join("other.sqlite");
        drop(Store::open(&other).unwrap());
        std::fs::rename(&main, data.join("original.sqlite")).unwrap();
        std::fs::rename(&other, &main).unwrap();
        let before = std::fs::read(&main).unwrap();
        assert!(lease
            .export_backup(
                &root,
                "must-not-exist",
                Instant::now() + Duration::from_secs(5)
            )
            .is_err());
        assert!(!root.join("must-not-exist").exists());
        assert_eq!(std::fs::read(&main).unwrap(), before);
        state.shutdown_credentials().await;
        drop(state);
        drop(lease);
        std::fs::remove_dir_all(root).unwrap();
    }
}
