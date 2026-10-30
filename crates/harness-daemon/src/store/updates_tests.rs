//! Application adapter regressions: actual Store schema, exclusive DataLease,
//! pinned filesystem capabilities and offline SQLite restore. No live signing
//! service, production key, network, model, vault, installer or executable runs.
use super::*;
use crate::reliability::{storage, update, update_state as state};
use harness_core::new_id;
use ring::signature::{Ed25519KeyPair, KeyPair};
use rusqlite::params;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("actual-update-store-{}", new_id()));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn open(path: &Path) -> (Arc<DataLease>, Arc<Mutex<Store>>, Arc<Runtime>) {
    let lease = DataLease::acquire(path).unwrap();
    let store = Arc::new(Mutex::new(lease.open_store().unwrap()));
    let controller = runtime(
        store.clone(),
        lease.clone(),
        crate::reliability::lifecycle_runtime::LifecycleRuntime::new(),
    )
    .unwrap();
    (lease, store, controller)
}
fn key() -> Ed25519KeyPair {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
}
fn trust(key: &Ed25519KeyPair) -> state::TrustInput {
    state::TrustInput {
        key_id: "ephemeral-application-fixture".into(),
        source: "https://updates.example.invalid/stable/".into(),
        public_key: key.public_key().as_ref().try_into().unwrap(),
        enabled: true,
    }
}
fn evidence(
    key: &Ed25519KeyPair,
    bytes: &[u8],
    version: &str,
    sequence: u64,
) -> state::ManifestEvidence {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let raw=json!({"manifest_schema":1,"application":"self-agent","channel":"stable","source":"https://updates.example.invalid/stable/","key_id":"ephemeral-application-fixture","version":version,"sequence":sequence,"target":update::Target::current().unwrap(),"database_schema_min":1,"database_schema_max":storage::SCHEMA_VERSION,"issued_at":now-1,"expires_at":now+3600,"artifact":{"name":"fixture-data.bin","bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}}).to_string();
    let mut message = b"self-agent-update-manifest-v1\0".to_vec();
    message.extend_from_slice(raw.as_bytes());
    let signature = key.sign(&message);
    state::ManifestEvidence {
        raw_manifest: raw,
        signature_hex: signature
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    }
}
fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn actual_store_open_migrates_v3_to_v4_without_rewriting_history_or_prior_ledger() {
    let root = Root::new();
    let path = root.0.join("harness.sqlite");
    let store = Store::open(&path).unwrap();
    let id = new_id();
    let history=json!({"id":id,"title":"v3 历史", "project_path":"fixture-only","created_at":"then","updated_at":"then","messages":[{"id":new_id(),"role":"user","content":"保留的历史正文","created_at":"then"}]}).to_string();
    store
        .db
        .execute("INSERT INTO sessions VALUES(?1,?2)", params![id, history])
        .unwrap();
    let old: Vec<(u32, String)> = store
        .db
        .prepare(
            "SELECT version,identity FROM storage_migrations WHERE version<=3 ORDER BY version",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    store.db.execute_batch("DROP TABLE update_jobs;DROP TABLE update_trust;DROP TABLE update_floors;DELETE FROM storage_migrations WHERE version=4;PRAGMA user_version=3;").unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(storage::preflight(&reopened.db).unwrap(), 4);
    assert_eq!(
        reopened
            .db
            .query_row("SELECT body FROM sessions WHERE id=?1", [&id], |r| r
                .get::<_, String>(0))
            .unwrap(),
        history
    );
    let preserved: Vec<(u32, String)> = reopened
        .db
        .prepare(
            "SELECT version,identity FROM storage_migrations WHERE version<=3 ORDER BY version",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(old, preserved);
    assert_eq!(
        reopened
            .db
            .query_row(
                "SELECT identity FROM storage_migrations WHERE version=4",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "self-agent-schema-v4-update-verification"
    );
    state::validate(&reopened.db).unwrap();
    assert!(state::config(&reopened.db).unwrap().is_none());
    assert!(state::receipts(&reopened.db).unwrap().is_empty());
}

#[tokio::test]
async fn detached_upload_holds_actual_data_lease_until_files_and_receipt_settle() {
    use tokio::io::AsyncWriteExt;
    let root = Root::new();
    let data = root.0.join("data");
    let (lease, store, controller) = open(&data);
    let key = key();
    controller.configure(None, trust(&key)).await.unwrap();
    let id = new_id();
    controller
        .begin(id.clone(), 1, evidence(&key, b"fixture bytes", "2.0.0", 10))
        .await
        .unwrap();
    let (mut writer, reader) = tokio::io::duplex(64);
    let worker = controller.clone();
    let op = id.clone();
    let observer = tokio::spawn(async move { worker.upload(op, reader, cancel()).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if state::receipt(&store.lock().unwrap().db, &id)
                .unwrap()
                .unwrap()
                .status
                == state::Status::Verifying
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    observer.abort();
    let _ = observer.await;
    drop(controller);
    drop(store);
    drop(lease);
    assert!(
        DataLease::acquire(&data).is_err(),
        "detached verification must retain the exclusive application lease"
    );
    writer.write_all(b"fixture bytes").await.unwrap();
    drop(writer);
    let new_lease = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(lease) = DataLease::acquire(&data) {
                break lease;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let store = Arc::new(Mutex::new(new_lease.open_store().unwrap()));
    let controller = runtime(
        store,
        new_lease,
        crate::reliability::lifecycle_runtime::LifecycleRuntime::new(),
    )
    .unwrap();
    let receipt = controller.receipt(id).await.unwrap().unwrap();
    assert_eq!(receipt.receipt.status, state::Status::Verified);
    assert!(receipt.current_stage_verified);
    assert!(!receipt.installed);
    assert!(!receipt.automatic_update_enabled);
}

#[cfg(unix)]
#[tokio::test]
async fn actual_constructor_pins_stage_and_witness_roots_and_refuses_replacement_links() {
    use std::os::unix::fs::symlink;
    let root = Root::new();
    let data = root.0.join("data");
    let outside = root.0.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"unchanged").unwrap();
    let (lease, store, controller) = open(&data);
    let key = key();
    controller.configure(None, trust(&key)).await.unwrap();
    for name in ["updates-stage", "updates-witness"] {
        std::fs::rename(data.join(name), data.join(format!("{name}-pinned"))).unwrap();
        symlink(&outside, data.join(name)).unwrap();
    }
    assert!(
        runtime(
            store.clone(),
            lease.clone(),
            crate::reliability::lifecycle_runtime::LifecycleRuntime::new()
        )
        .is_err(),
        "new roots must be opened nofollow"
    );
    let id = new_id();
    controller
        .begin(id.clone(), 1, evidence(&key, b"fixture bytes", "2.0.0", 10))
        .await
        .unwrap();
    controller
        .upload(
            id.clone(),
            std::io::Cursor::new(b"fixture bytes".to_vec()),
            cancel(),
        )
        .await
        .unwrap();
    assert!(data
        .join("updates-stage-pinned")
        .join(format!(".update-{id}.verified"))
        .is_file());
    assert!(data
        .join("updates-witness-pinned")
        .join(format!("floor-{id}.json"))
        .is_file());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(outside.join("sentinel")).unwrap(),
        b"unchanged"
    );
    assert!(
        controller
            .receipt(id)
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
}

#[tokio::test]
async fn actual_old_backup_restore_disables_trust_and_external_witness_preserves_floor() {
    let root = Root::new();
    let data = root.0.join("data");
    let (lease, store, controller) = open(&data);
    let key = key();
    controller.configure(None, trust(&key)).await.unwrap();
    // The backup is schema v4 with enabled trust but BEFORE any verification/floor.
    let manifest = lease
        .export_backup(
            &root.0,
            "old-backup",
            Instant::now() + Duration::from_secs(15),
        )
        .unwrap();
    let id = new_id();
    controller
        .begin(id.clone(), 1, evidence(&key, b"fixture bytes", "2.0.0", 10))
        .await
        .unwrap();
    controller
        .upload(
            id.clone(),
            std::io::Cursor::new(b"fixture bytes".to_vec()),
            cancel(),
        )
        .await
        .unwrap();
    let witness_path = data
        .join("updates-witness")
        .join(format!("floor-{id}.json"));
    let witness_bytes = std::fs::read(&witness_path).unwrap();
    drop(controller);
    drop(store);
    drop(lease);
    crate::maintenance::restore_backup(&data, &root.0.join("old-backup"), &manifest.sha256)
        .unwrap();
    assert_eq!(std::fs::read(&witness_path).unwrap(), witness_bytes);
    let (_lease, store, controller) = open(&data);
    let before = state::config(&store.lock().unwrap().db).unwrap().unwrap();
    assert!(!before.enabled);
    let status = controller.recover().await.unwrap();
    assert!(!status.configuration.as_ref().unwrap().enabled);
    assert!(!status.automatic_update_enabled);
    assert!(!status.installed);
    let target = serde_json::to_value(update::Target::current().unwrap()).unwrap();
    assert_eq!(
        status
            .floors
            .iter()
            .find(|f| f.target == target.as_str().unwrap())
            .unwrap()
            .sequence,
        10
    );
    assert!(
        controller.receipt(id.clone()).await.unwrap().is_none(),
        "orphan stage must not recreate a receipt from bytes"
    );
    assert!(data
        .join("updates-stage")
        .join(format!(".update-{id}.verified"))
        .exists());
    let enabled = controller
        .configure(Some(before.revision), trust(&key))
        .await
        .unwrap();
    assert!(controller
        .begin(
            new_id(),
            enabled.revision,
            evidence(&key, b"old bytes", "1.9.0", 9)
        )
        .await
        .is_err());
    assert_eq!(
        state::floor(&store.lock().unwrap().db, target.as_str().unwrap())
            .unwrap()
            .sequence,
        10
    );
}

#[tokio::test]
async fn update_intent_waiting_for_actual_store_lock_is_rejected_after_drain() {
    use crate::reliability::lifecycle_runtime::{
        LifecycleRuntime, QuiesceStage, QuiescenceCheck, ShutdownBackend, ShutdownFault,
        ShutdownOutcome,
    };
    struct Backend;
    impl ShutdownBackend for Backend {
        fn stop_external_inputs(&self) {}
        fn quiesce(&self, _: QuiesceStage) -> std::result::Result<QuiescenceCheck, ShutdownFault> {
            Ok(Default::default())
        }
    }
    let root = Root::new();
    let lease = DataLease::acquire(&root.0).unwrap();
    let store = Arc::new(Mutex::new(lease.open_store().unwrap()));
    let lifecycle = LifecycleRuntime::new();
    let controller = runtime(store.clone(), lease, lifecycle.clone()).unwrap();
    let held_store = store.clone();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _held = held_store.lock().unwrap();
        let _ = entered.send(());
        wait.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    ready.await.unwrap();
    let worker = controller.clone();
    let input = trust(&key());
    let observer = tokio::spawn(async move { worker.configure(None, input).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while lifecycle.snapshot().workers.pending_workers == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    lifecycle.request_shutdown(Arc::new(Backend)).unwrap();
    release.send(()).unwrap();
    holder.join().unwrap();
    assert!(observer.await.unwrap().is_err());
    let drained = lifecycle
        .observe(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(drained.snapshot.outcome, Some(ShutdownOutcome::Stopped));
    let mut guard = store.lock().unwrap();
    let tx = guard.db.transaction().unwrap();
    assert!(state::config(&tx).unwrap().is_none());
}
