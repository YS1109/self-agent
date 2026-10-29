use super::*;
use ring::signature::{Ed25519KeyPair, KeyPair};
use rusqlite::Connection;
use serde_json::json;
use std::path::PathBuf;
struct Database {
    connection: Mutex<Connection>,
    reject_finalize: AtomicBool,
}
impl UpdateDatabase for Database {
    fn transaction<T>(&self, action: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let result = action(&tx)?;
        let verified: u64 = tx.query_row(
            "SELECT COUNT(*) FROM update_jobs WHERE status='verified'",
            [],
            |r| r.get(0),
        )?;
        if verified > 0 && self.reject_finalize.swap(false, Ordering::AcqRel) {
            bail!("fixture commit failed after witness");
        }
        tx.commit()?;
        Ok(result)
    }
}
struct Fixture {
    root: PathBuf,
    runtime: Arc<UpdateRuntime<Database>>,
    key: Ed25519KeyPair,
}
impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("update-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("stage")).unwrap();
        std::fs::create_dir(root.join("witness")).unwrap();
        let mut db = Connection::open(root.join("state.sqlite")).unwrap();
        let tx = db.transaction().unwrap();
        state::migrate(&tx).unwrap();
        tx.commit().unwrap();
        let database = Arc::new(Database {
            connection: Mutex::new(db),
            reject_finalize: AtomicBool::new(false),
        });
        let runtime = UpdateRuntime::new(
            database,
            Dir::open_ambient_dir(root.join("stage"), cap_std::ambient_authority()).unwrap(),
            Dir::open_ambient_dir(root.join("witness"), cap_std::ambient_authority()).unwrap(),
            "1.0.0".into(),
            4,
        )
        .unwrap();
        let key = Ed25519KeyPair::from_pkcs8(
            Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .unwrap()
                .as_ref(),
        )
        .unwrap();
        let f = Self { root, runtime, key };
        f.runtime.configure(None, f.trust(true)).await.unwrap();
        f
    }
    fn trust(&self, enabled: bool) -> state::TrustInput {
        state::TrustInput {
            key_id: "ephemeral-fixture".into(),
            source: "https://updates.example.invalid/stable/".into(),
            public_key: self.key.public_key().as_ref().try_into().unwrap(),
            enabled,
        }
    }
    fn evidence(&self, bytes: &[u8], version: &str, sequence: u64) -> state::ManifestEvidence {
        let manifest = json!({"manifest_schema":1,"application":"self-agent","channel":"stable","source":"https://updates.example.invalid/stable/","key_id":"ephemeral-fixture","version":version,"sequence":sequence,"target":target_name(self.runtime.target),"database_schema_min":1,"database_schema_max":4,"issued_at":now()-1,"expires_at":now()+3600,"artifact":{"name":"fixture.bin","bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}});
        let raw = manifest.to_string();
        let mut signed = b"self-agent-update-manifest-v1\0".to_vec();
        signed.extend_from_slice(raw.as_bytes());
        let signature = self.key.sign(&signed);
        state::ManifestEvidence {
            raw_manifest: raw,
            signature_hex: signature
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        }
    }
    async fn begin(&self, bytes: &[u8], version: &str, sequence: u64) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.runtime
            .begin(id.clone(), 1, self.evidence(bytes, version, sequence))
            .await
            .unwrap();
        id
    }
    fn names(&self) -> Vec<String> {
        std::fs::read_dir(self.root.join("stage"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
async fn claimed(f: &Fixture, id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = f
                .runtime
                .db
                .transaction(|tx| Ok(state::receipt(tx, id)?.unwrap().status))
                .unwrap();
            if status == state::Status::Verifying {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn real_bytes_witness_then_receipt_and_digest_matched_discard() {
    let f = Fixture::new().await;
    let bytes = b"real local artifact bytes".to_vec();
    let id = f.begin(&bytes, "2.0.0", 10).await;
    let done = f
        .runtime
        .upload(id.clone(), std::io::Cursor::new(bytes), cancel())
        .await
        .unwrap();
    assert_eq!(done.status, state::Status::Verified);
    assert!(done.stage_authority);
    let report = f.runtime.receipt(id.clone()).await.unwrap().unwrap();
    assert!(report.current_stage_verified);
    assert!(!report.installed);
    assert!(!report.automatic_update_enabled);
    assert_eq!(f.names(), [format!(".update-{id}.verified")]);
    assert_eq!(witness::load(&f.runtime.witness).unwrap().len(), 1);
    assert!(f.runtime.discard(id.clone(), "0".repeat(64)).await.is_err());
    assert_eq!(f.names().len(), 1);
    let removed = f
        .runtime
        .discard(id.clone(), done.metadata.sha256)
        .await
        .unwrap();
    assert!(!removed.stage_authority);
    assert_eq!(removed.status, state::Status::Verified);
    assert!(f.names().is_empty());
    assert!(
        !f.runtime
            .receipt(id)
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
    assert_eq!(
        f.runtime
            .db
            .transaction(|tx| state::floor(tx, &target_name(f.runtime.target)))
            .unwrap()
            .sequence,
        10
    );
}
#[tokio::test]
async fn signatures_schema_and_platform_are_verified_before_intent_or_upload() {
    let f = Fixture::new().await;
    let mut evidence = f.evidence(b"bytes", "2.0.0", 10);
    evidence.signature_hex = "a".repeat(128);
    assert!(f
        .runtime
        .begin(uuid::Uuid::new_v4().to_string(), 1, evidence)
        .await
        .is_err());
    assert!(f.runtime.status().await.unwrap().receipts.is_empty());
    assert!(f.names().is_empty());
    let mut bad = f.evidence(b"bytes", "2.0.0", 10);
    bad.signature_hex = "é".repeat(64);
    assert!(f
        .runtime
        .begin(uuid::Uuid::new_v4().to_string(), 1, bad)
        .await
        .is_err());
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    assert!(f
        .runtime
        .upload(
            id.clone(),
            std::io::Cursor::new(b"wrong".to_vec()),
            cancel()
        )
        .await
        .is_err());
    assert!(f.names().is_empty());
    assert_eq!(
        f.runtime.receipt(id).await.unwrap().unwrap().receipt.status,
        state::Status::Failed
    );
}
#[tokio::test]
async fn stalled_upload_keeps_timers_alive_and_policy_revoke_cancels_without_stage() {
    let f = Fixture::new().await;
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    let (_sender, reader) = tokio::io::duplex(64);
    let runtime = f.runtime.clone();
    let op = id.clone();
    let upload = tokio::spawn(async move { runtime.upload(op, reader, cancel()).await });
    claimed(&f, &id).await;
    let tick = Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(tick.elapsed() >= Duration::from_millis(20));
    assert!(f
        .runtime
        .upload(
            id.clone(),
            std::io::Cursor::new(b"bytes".to_vec()),
            cancel()
        )
        .await
        .is_err());
    f.runtime.configure(Some(1), f.trust(false)).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), upload)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(f.names().is_empty());
    assert!(
        !f.runtime
            .receipt(id)
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
}
#[tokio::test]
async fn explicit_cancel_is_durable_not_cleanup_claim_and_observer_drop_does_not_stop_work() {
    use tokio::io::AsyncWriteExt;
    let f = Fixture::new().await;
    let pending = f.begin(b"bytes", "2.0.0", 10).await;
    assert_eq!(
        f.runtime.cancel(pending.clone()).await.unwrap().status,
        state::Status::Failed
    );
    assert!(f
        .runtime
        .upload(pending, std::io::Cursor::new(b"bytes".to_vec()), cancel())
        .await
        .is_err());
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    let (mut sender, reader) = tokio::io::duplex(64);
    let runtime = f.runtime.clone();
    let op = id.clone();
    let observe = tokio::spawn(async move { runtime.upload(op, reader, cancel()).await });
    claimed(&f, &id).await;
    observe.abort();
    let _ = observe.await;
    sender.write_all(b"bytes").await.unwrap();
    drop(sender);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.runtime
                .receipt(id.clone())
                .await
                .unwrap()
                .unwrap()
                .receipt
                .status
                == state::Status::Verified
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        f.runtime
            .receipt(id)
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
    let active = f.begin(b"later", "3.0.0", 20).await;
    let (_sender, reader) = tokio::io::duplex(64);
    let runtime = f.runtime.clone();
    let op = active.clone();
    let upload = tokio::spawn(async move { runtime.upload(op, reader, cancel()).await });
    claimed(&f, &active).await;
    let requested = f.runtime.cancel(active.clone()).await.unwrap();
    assert_eq!(requested.reason.as_deref(), Some("cancellation_requested"));
    assert_eq!(requested.status, state::Status::Verifying);
    assert!(tokio::time::timeout(Duration::from_secs(5), upload)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert_eq!(
        f.runtime
            .receipt(active)
            .await
            .unwrap()
            .unwrap()
            .receipt
            .status,
        state::Status::Failed
    );
}
#[tokio::test]
async fn failed_db_finalize_keeps_witness_floor_and_unknown_never_replays() {
    let f = Fixture::new().await;
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    f.runtime.db.reject_finalize.store(true, Ordering::Release);
    assert!(f
        .runtime
        .upload(
            id.clone(),
            std::io::Cursor::new(b"bytes".to_vec()),
            cancel()
        )
        .await
        .is_err());
    assert!(f.names().is_empty());
    assert_eq!(witness::load(&f.runtime.witness).unwrap().len(), 1);
    let receipt = f.runtime.receipt(id.clone()).await.unwrap().unwrap();
    assert_eq!(receipt.receipt.status, state::Status::Unknown);
    assert!(!receipt.current_stage_verified);
    assert!(f
        .runtime
        .upload(id, std::io::Cursor::new(b"bytes".to_vec()), cancel())
        .await
        .is_err());
    assert!(f
        .runtime
        .begin(
            uuid::Uuid::new_v4().to_string(),
            1,
            f.evidence(b"old", "1.9.0", 9)
        )
        .await
        .is_err());
}
#[tokio::test]
async fn restored_old_db_imports_external_floor_and_partial_witness_blocks_only_verification() {
    let f = Fixture::new().await;
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    f.runtime
        .upload(id, std::io::Cursor::new(b"bytes".to_vec()), cancel())
        .await
        .unwrap();
    // Model restoring an older image: DB jobs/floors gone, external witness stays.
    f.runtime
        .db
        .transaction(|tx| {
            tx.execute("DELETE FROM update_jobs", [])?;
            tx.execute("DELETE FROM update_floors", [])?;
            state::restore(tx, millis())
        })
        .unwrap();
    f.runtime.recover().await.unwrap();
    assert_eq!(
        f.runtime
            .db
            .transaction(|tx| state::floor(tx, &target_name(f.runtime.target)))
            .unwrap()
            .sequence,
        10
    );
    f.runtime.configure(Some(2), f.trust(true)).await.unwrap();
    assert!(f
        .runtime
        .begin(
            uuid::Uuid::new_v4().to_string(),
            3,
            f.evidence(b"old", "1.9.0", 9)
        )
        .await
        .is_err());
    std::fs::write(
        f.root
            .join("witness")
            .join(format!("floor-{}.json", uuid::Uuid::new_v4())),
        b"partial",
    )
    .unwrap();
    let status = f.runtime.status().await.unwrap();
    assert_eq!(status.blocked_reason, Some("witness_unavailable"));
    assert!(!status.automatic_update_enabled);
    assert!(f
        .runtime
        .begin(
            uuid::Uuid::new_v4().to_string(),
            3,
            f.evidence(b"next", "3.0.0", 20)
        )
        .await
        .is_err());
    // Unrelated authoritative DB work remains available.
    f.runtime
        .db
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE unrelated(value TEXT);INSERT INTO unrelated VALUES('works');",
            )?;
            Ok(())
        })
        .unwrap();
}
#[tokio::test]
async fn quota_and_tampered_stage_fail_closed_without_arbitrary_path_cleanup() {
    let f = Fixture::new().await;
    let unknown = uuid::Uuid::new_v4().to_string();
    let large = std::fs::File::create(
        f.root
            .join("stage")
            .join(format!(".update-{unknown}.verified")),
    )
    .unwrap();
    large.set_len(QUOTA).unwrap();
    drop(large);
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    assert!(f
        .runtime
        .upload(id, std::io::Cursor::new(b"bytes".to_vec()), cancel())
        .await
        .is_err());
    assert!(f
        .runtime
        .discard("../outside".into(), "a".repeat(64))
        .await
        .is_err());
    assert!(f
        .runtime
        .discard(unknown.clone(), "a".repeat(64))
        .await
        .is_err());
    std::fs::remove_file(
        f.root
            .join("stage")
            .join(format!(".update-{unknown}.verified")),
    )
    .unwrap();
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    let receipt = f
        .runtime
        .upload(
            id.clone(),
            std::io::Cursor::new(b"bytes".to_vec()),
            cancel(),
        )
        .await
        .unwrap();
    std::fs::write(
        f.root.join("stage").join(format!(".update-{id}.verified")),
        b"wrong",
    )
    .unwrap();
    assert!(
        !f.runtime
            .receipt(id.clone())
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
    assert!(f
        .runtime
        .discard(id, receipt.metadata.sha256)
        .await
        .is_err());
    assert_eq!(f.names().len(), 1);
}

#[tokio::test]
async fn runtime_deadline_stops_stalled_reader_without_witness_or_automatic_retry() {
    let f = Fixture::new().await;
    let id = f.begin(b"bytes", "2.0.0", 10).await;
    let (_writer, reader) = tokio::io::duplex(64);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        f.runtime.upload_until(
            id.clone(),
            reader,
            cancel(),
            Instant::now() + Duration::from_millis(100),
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(f.names().is_empty());
    assert!(witness::load(&f.runtime.witness).unwrap().is_empty());
    assert_eq!(
        f.runtime
            .receipt(id.clone())
            .await
            .unwrap()
            .unwrap()
            .receipt
            .status,
        state::Status::Failed
    );
    assert!(f
        .runtime
        .upload(id, std::io::Cursor::new(b"bytes".to_vec()), cancel())
        .await
        .is_err());
}

#[tokio::test]
async fn historical_replay_cancel_and_status_never_claim_current_bytes_without_rechecking() {
    let f = Fixture::new().await;
    let evidence = f.evidence(b"bytes", "2.0.0", 10);
    let id = uuid::Uuid::new_v4().to_string();
    f.runtime
        .begin(id.clone(), 1, evidence.clone())
        .await
        .unwrap();
    f.runtime
        .upload(
            id.clone(),
            std::io::Cursor::new(b"bytes".to_vec()),
            cancel(),
        )
        .await
        .unwrap();
    std::fs::remove_file(f.root.join("stage").join(format!(".update-{id}.verified"))).unwrap();
    assert!(
        !f.runtime
            .begin(id.clone(), 1, evidence)
            .await
            .unwrap()
            .stage_authority
    );
    assert!(!f.runtime.cancel(id.clone()).await.unwrap().stage_authority);
    assert!(!f.runtime.status().await.unwrap().receipts[0].stage_authority);
    assert!(
        !f.runtime
            .receipt(id)
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
}

#[tokio::test]
async fn expiry_during_gated_stream_never_advances_floor_and_expired_stage_is_not_current() {
    use tokio::io::AsyncWriteExt;
    let mut f = Fixture::new().await;
    let time = Arc::new(std::sync::atomic::AtomicU64::new(now()));
    Arc::get_mut(&mut f.runtime).unwrap().test_now = Some(time.clone());
    let first = f.begin(b"first", "2.0.0", 10).await;
    f.runtime
        .upload(first.clone(), std::io::Cursor::new(b"first"), cancel())
        .await
        .unwrap();
    assert!(
        f.runtime
            .receipt(first.clone())
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
    let second = f.begin(b"second", "3.0.0", 11).await;
    let (mut writer, reader) = tokio::io::duplex(64);
    let runtime = f.runtime.clone();
    let id = second.clone();
    let upload = tokio::spawn(async move { runtime.upload(id, reader, cancel()).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !f
            .root
            .join("stage")
            .join(format!(".update-{second}.part"))
            .exists()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // The genuine stream is paused after signature acceptance/create_new; move
    // only this runtime's test clock past signed expiry before its remaining bytes.
    time.store(now() + 4000, Ordering::Release);
    writer.write_all(b"second").await.unwrap();
    writer.shutdown().await.unwrap();
    assert!(upload.await.unwrap().is_err());
    assert_eq!(witness::load(&f.runtime.witness).unwrap().len(), 1);
    assert!(!f.names().contains(&format!(".update-{second}.verified")));
    assert!(
        !f.runtime
            .receipt(first)
            .await
            .unwrap()
            .unwrap()
            .current_stage_verified
    );
    assert_eq!(
        f.runtime
            .db
            .transaction(|tx| state::floor(tx, &target_name(f.runtime.target)))
            .unwrap()
            .sequence,
        10
    );
}

#[tokio::test]
async fn staging_reserves_both_publication_names_before_writing_at_capacity() {
    for count in [511, 512] {
        let f = Fixture::new().await;
        for _ in 0..count {
            std::fs::write(
                f.root
                    .join("stage")
                    .join(format!(".update-{}.part", uuid::Uuid::new_v4())),
                b"",
            )
            .unwrap();
        }
        let id = f.begin(b"bytes", "2.0.0", 10).await;
        assert!(f
            .runtime
            .upload(id, std::io::Cursor::new(b"bytes"), cancel())
            .await
            .is_err());
        assert_eq!(f.names().len(), count);
        assert!(witness::load(&f.runtime.witness).unwrap().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readback_rechecks_trust_revocation_and_newer_floor_after_hash() {
    for rotate in [true, false] {
        let mut f = Fixture::new().await;
        let first = f.begin(b"first", "2.0.0", 10).await;
        f.runtime
            .upload(first.clone(), std::io::Cursor::new(b"first"), cancel())
            .await
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let receiver = Mutex::new(release_rx);
        Arc::get_mut(&mut f.runtime).unwrap().test_after_hash = Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            receiver
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }));
        let runtime = f.runtime.clone();
        let lookup = tokio::spawn(async move { runtime.receipt(first).await });
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap()
        })
        .await
        .unwrap();
        if rotate {
            f.runtime.configure(Some(1), f.trust(false)).await.unwrap();
        } else {
            let next = f.begin(b"second", "3.0.0", 11).await;
            f.runtime
                .upload(next, std::io::Cursor::new(b"second"), cancel())
                .await
                .unwrap();
        }
        release_tx.send(()).unwrap();
        let observed = lookup.await.unwrap().unwrap().unwrap();
        assert!(!observed.current_stage_verified);
        assert!(!observed.receipt.stage_authority);
    }
}
