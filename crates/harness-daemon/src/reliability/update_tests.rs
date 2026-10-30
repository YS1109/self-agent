use super::*;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc,
    },
};
use tokio::io::AsyncWriteExt;

const NOW: u64 = 1_800_000_000;
const SOURCE: &str = "https://updates.example.invalid/self-agent/stable/";
fn key() -> Ed25519KeyPair {
    use ring::rand::SecureRandom;
    static SEED: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let seed = SEED.get_or_init(|| {
        let mut seed = [0; 32];
        ring::rand::SystemRandom::new().fill(&mut seed).unwrap();
        seed
    });
    Ed25519KeyPair::from_seed_unchecked(seed).unwrap()
}
fn policy() -> UpdatePolicy {
    UpdatePolicy {
        trust: Some(Trust {
            public_key: key().public_key().as_ref().try_into().unwrap(),
            key_id: "fixture-release-key-v1".into(),
            source: SOURCE.into(),
        }),
    }
}
fn context() -> UpdateContext {
    UpdateContext::new("0.6.0", "0.6.0", 6, 1, Target::LinuxX86_64, NOW).unwrap()
}
fn manifest(bytes: &[u8]) -> Value {
    json!({"manifest_schema":1,"application":"self-agent","channel":"stable","source":SOURCE,"key_id":"fixture-release-key-v1","version":"1.0.0","sequence":7,"target":"linux-x86_64","database_schema_min":1,"database_schema_max":1,"issued_at":NOW-60,"expires_at":NOW+3600,"artifact":{"name":"self-agent-fixture.bin","bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}})
}
fn sign_raw(raw: &[u8]) -> Vec<u8> {
    let mut message = DOMAIN.to_vec();
    message.extend_from_slice(raw);
    key().sign(&message).as_ref().to_vec()
}
fn verified(bytes: &[u8]) -> VerifiedManifest {
    let raw = serde_json::to_vec(&manifest(bytes)).unwrap();
    verify_manifest(&raw, &sign_raw(&raw), &policy(), &context()).unwrap()
}
fn error_for(value: Value) -> UpdateError {
    let raw = serde_json::to_vec(&value).unwrap();
    verify_manifest(&raw, &sign_raw(&raw), &policy(), &context())
        .err()
        .unwrap()
}
struct Root {
    path: PathBuf,
    dir: Option<Dir>,
}
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "self-agent-update-fixture-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).unwrap();
        let path = path.canonicalize().unwrap();
        let dir = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).unwrap();
        Self {
            path,
            dir: Some(dir),
        }
    }
    fn dir(&self) -> &Dir {
        self.dir.as_ref().unwrap()
    }
    fn names(&self) -> Vec<String> {
        self.dir()
            .entries()
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        drop(self.dir.take());
        let result = std::fs::remove_dir_all(&self.path);
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[test]
fn production_is_disabled_without_a_compiled_trust_root() {
    let disabled = production_policy();
    assert_eq!(
        disabled.status(),
        UpdateStatus::DisabledNoTrustedReleaseConfiguration
    );
    assert_eq!(
        verify_manifest(b"untrusted", b"signature", &disabled, &context()).err(),
        Some(UpdateError::Disabled)
    );
    assert_eq!(policy().status(), UpdateStatus::VerificationOnly);
}

#[test]
fn exact_bytes_domain_signature_and_fixed_key_are_required() {
    let raw = serde_json::to_vec(&manifest(b"signed fixture")).unwrap();
    let signature = sign_raw(&raw);
    assert!(verify_manifest(&raw, &signature, &policy(), &context()).is_ok());
    let mut tampered = raw.clone();
    tampered.push(b' ');
    assert_eq!(
        verify_manifest(&tampered, &signature, &policy(), &context()).err(),
        Some(UpdateError::Signature)
    );
    let wrong = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
    let wrong_policy = UpdatePolicy {
        trust: Some(Trust {
            public_key: wrong.public_key().as_ref().try_into().unwrap(),
            key_id: "fixture-release-key-v1".into(),
            source: SOURCE.into(),
        }),
    };
    assert_eq!(
        verify_manifest(&raw, &signature, &wrong_policy, &context()).err(),
        Some(UpdateError::Signature)
    );
    assert_eq!(
        verify_manifest(&raw, key().sign(&raw).as_ref(), &policy(), &context()).err(),
        Some(UpdateError::Signature)
    );
    for signature in [&[][..], &[0; 63][..], &[0; 64][..], &[0; 65][..]] {
        assert_eq!(
            verify_manifest(&raw, signature, &policy(), &context()).err(),
            Some(UpdateError::Signature)
        );
    }
    assert_eq!(
        verify_manifest(
            &vec![b'x'; MAX_MANIFEST_BYTES + 1],
            &[0; 64],
            &policy(),
            &context()
        )
        .err(),
        Some(UpdateError::ManifestTooLarge)
    );
}

#[test]
fn signed_manifest_strictly_binds_schema_platform_source_and_limits() {
    let baseline = manifest(b"content");
    for (field, value, error) in [
        ("manifest_schema", json!(2), UpdateError::Format),
        ("application", json!("another-app"), UpdateError::Format),
        ("channel", json!("nightly"), UpdateError::Format),
        (
            "source",
            json!("https://attacker.invalid/"),
            UpdateError::Source,
        ),
        ("key_id", json!("untrusted-key"), UpdateError::Source),
        ("target", json!("windows-x86_64"), UpdateError::Platform),
        ("database_schema_min", json!(2), UpdateError::Schema),
        ("database_schema_max", json!(0), UpdateError::Schema),
        ("expires_at", json!(NOW), UpdateError::Expired),
        ("issued_at", json!(NOW + 301), UpdateError::Expired),
        (
            "expires_at",
            json!(NOW + MAX_MANIFEST_LIFETIME + 1),
            UpdateError::Expired,
        ),
        ("version", json!("1.0.0+new-build"), UpdateError::Version),
        ("version", json!("1.0.0-beta"), UpdateError::Version),
        ("version", json!("01.0.0"), UpdateError::Version),
        ("version", json!("0.6.0"), UpdateError::Downgrade),
        ("version", json!("0.5.99"), UpdateError::Downgrade),
        ("sequence", json!(6), UpdateError::Downgrade),
        ("sequence", json!(0), UpdateError::Downgrade),
    ] {
        let mut changed = baseline.clone();
        changed[field] = value;
        assert_eq!(error_for(changed), error, "{field}");
    }
}

#[test]
fn artifact_names_hashes_lengths_and_json_shape_cannot_escape_validation() {
    for (field, value, error) in [
        ("name", json!("../escape.exe"), UpdateError::Format),
        ("name", json!("C:\\escape.exe"), UpdateError::Format),
        ("name", json!("/tmp/escape"), UpdateError::Format),
        ("name", json!(".hidden"), UpdateError::Format),
        ("name", json!("file?secret=canary"), UpdateError::Format),
        ("bytes", json!(0), UpdateError::Size),
        ("bytes", json!(MAX_ARTIFACT_BYTES + 1), UpdateError::Size),
        ("sha256", json!("x".repeat(64)), UpdateError::Digest),
        ("sha256", json!("A".repeat(64)), UpdateError::Digest),
    ] {
        let mut changed = manifest(b"x");
        changed["artifact"][field] = value;
        assert_eq!(error_for(changed), error);
    }
    let mut unknown = manifest(b"x");
    unknown["credential"] = json!("fixture-private-key");
    assert_eq!(error_for(unknown), UpdateError::Format);
    let mut nested = manifest(b"x");
    nested["artifact"]["url"] = json!("https://attacker.invalid/");
    assert_eq!(error_for(nested), UpdateError::Format);
    let raw = serde_json::to_string(&manifest(b"x")).unwrap();
    let duplicate = raw.replacen('{', "{\"version\":\"9.0.0\",", 1);
    assert_eq!(
        verify_manifest(
            duplicate.as_bytes(),
            &sign_raw(duplicate.as_bytes()),
            &policy(),
            &context()
        )
        .err(),
        Some(UpdateError::Format)
    );
}

#[test]
fn trusted_rollback_floor_cannot_be_lowered_by_an_old_signed_manifest() {
    let raw = serde_json::to_vec(&manifest(b"x")).unwrap();
    for context in [
        UpdateContext::new("0.6.0", "1.0.0", 6, 1, Target::LinuxX86_64, NOW).unwrap(),
        UpdateContext::new("0.6.0", "0.6.0", 7, 1, Target::LinuxX86_64, NOW).unwrap(),
        UpdateContext::new("1.1.0", "1.1.0", 7, 1, Target::LinuxX86_64, NOW).unwrap(),
    ] {
        assert_eq!(
            verify_manifest(&raw, &sign_raw(&raw), &policy(), &context).err(),
            Some(UpdateError::Downgrade)
        );
    }
    assert_eq!(
        UpdateContext::new("1.0.0", "0.6.0", 0, 1, Target::LinuxX86_64, NOW).err(),
        Some(UpdateError::Downgrade)
    );
}

#[tokio::test]
async fn valid_artifact_is_actually_hashed_staged_private_and_never_installed() {
    let root = Root::new();
    let bytes = vec![42; CHUNK_BYTES + 123];
    let staged = stage_artifact(
        std::io::Cursor::new(bytes.clone()),
        verified(&bytes),
        root.dir(),
        deadline(),
        &|| false,
    )
    .await
    .unwrap();
    let names = root.names();
    assert_eq!(names.len(), 1);
    assert!(names[0].ends_with(".verified"));
    assert_eq!(root.dir().read(&names[0]).unwrap(), bytes);
    let summary = serde_json::to_value(staged.summary()).unwrap();
    assert_eq!(summary["installed"], false);
    assert_eq!(summary["version"], "1.0.0");
    assert!(!summary.to_string().contains(SOURCE));
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        assert_eq!(
            root.dir().metadata(&names[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(staged);
    assert!(root.names().is_empty());
}

#[tokio::test]
async fn truncated_extra_or_tampered_payload_cannot_publish_a_ready_artifact() {
    for (bytes, error) in [
        (b"abc".to_vec(), UpdateError::Size),
        (b"abcde".to_vec(), UpdateError::Size),
        (b"abcz".to_vec(), UpdateError::Digest),
    ] {
        let root = Root::new();
        let result = stage_artifact(
            std::io::Cursor::new(bytes),
            verified(b"abcd"),
            root.dir(),
            deadline(),
            &|| false,
        )
        .await;
        assert_eq!(result.err(), Some(error));
        assert!(root.names().is_empty());
    }
}

#[tokio::test]
async fn stalled_read_deadline_and_cancellation_never_publish_or_retry() {
    let root = Root::new();
    let (_held, reader) = tokio::io::duplex(16);
    let result = stage_artifact(
        reader,
        verified(b"abcd"),
        root.dir(),
        Instant::now() + Duration::from_millis(60),
        &|| false,
    )
    .await;
    assert_eq!(result.err(), Some(UpdateError::Deadline));
    assert!(root.names().is_empty());
    let (mut writer, reader) = tokio::io::duplex(16);
    writer.write_all(b"ab").await.unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    let dir = root.dir().try_clone().unwrap();
    let task = tokio::spawn(async move {
        stage_artifact(reader, verified(b"abcd"), &dir, deadline(), &|| {
            signal.load(AtomicOrdering::SeqCst)
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while root.names().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    cancelled.store(true, AtomicOrdering::SeqCst);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .err(),
        Some(UpdateError::Cancelled)
    );
    drop(writer);
    assert!(root.names().iter().all(|name| !name.ends_with(".verified")));
}

#[tokio::test]
async fn cancellation_before_staging_touches_no_files_and_dropped_future_leaves_no_ready_file() {
    let root = Root::new();
    assert_eq!(
        stage_artifact(
            std::io::Cursor::new(b"x"),
            verified(b"x"),
            root.dir(),
            deadline(),
            &|| true
        )
        .await
        .err(),
        Some(UpdateError::Cancelled)
    );
    assert!(root.names().is_empty());
    let (_writer, reader) = tokio::io::duplex(16);
    let dir = root.dir().try_clone().unwrap();
    let task = tokio::spawn(async move {
        stage_artifact(reader, verified(b"x"), &dir, deadline(), &|| false).await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while root.names().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert!(root.names().iter().all(|name| !name.ends_with(".verified")));
}

#[tokio::test]
async fn reader_errors_expose_only_static_codes_not_private_details() {
    struct BadReader;
    impl AsyncRead for BadReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Err(io::Error::other(
                "fixture-secret-path-token-url-canary",
            )))
        }
    }
    let root = Root::new();
    let error = stage_artifact(BadReader, verified(b"x"), root.dir(), deadline(), &|| false)
        .await
        .err()
        .unwrap();
    assert_eq!(error, UpdateError::Io);
    assert!(!format!("{error:?} {error}").contains("canary"));
    assert!(root.names().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn pinned_staging_directory_does_not_follow_replaced_root_path() {
    let root = Root::new();
    let outside = Root::new();
    let moved = root.path.with_extension("moved");
    std::fs::rename(&root.path, &moved).unwrap();
    std::os::unix::fs::symlink(&outside.path, &root.path).unwrap();
    let result = stage_artifact(
        std::io::Cursor::new(b"x"),
        verified(b"x"),
        root.dir(),
        deadline(),
        &|| false,
    )
    .await;
    assert!(outside.names().is_empty());
    let artifact = result.unwrap();
    assert_eq!(root.names().len(), 1);
    assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
    drop(artifact);
    std::fs::remove_file(&root.path).unwrap();
    std::fs::rename(&moved, &root.path).unwrap();
    assert!(root.names().is_empty());
}

#[test]
fn owner_verification_only_trust_is_separate_from_disabled_production() {
    let public_key = key().public_key().as_ref().try_into().unwrap();
    let trust =
        UpdatePolicy::verification_only(public_key, "fixture-release-key-v1", SOURCE).unwrap();
    let raw = serde_json::to_vec(&manifest(b"bounded package")).unwrap();
    assert!(verify_manifest(&raw, &sign_raw(&raw), &trust, &context()).is_ok());
    assert_eq!(
        production_policy().status(),
        UpdateStatus::DisabledNoTrustedReleaseConfiguration
    );
    for source in [
        "http://updates.example.invalid/",
        "https://user@example.invalid/",
        "https://example.invalid/?token=secret",
        "https://example.invalid/#secret",
        "https://EXAMPLE.invalid/",
    ] {
        assert!(UpdatePolicy::verification_only(public_key, "fixture", source).is_err());
    }
    for key_id in ["", "../key", "key\ncanary"] {
        assert!(UpdatePolicy::verification_only(public_key, key_id, SOURCE).is_err());
    }
}
#[tokio::test]
async fn operation_bound_staging_is_private_retained_only_by_explicit_runtime_commit() {
    let root = Root::new();
    let operation = uuid::Uuid::new_v4().to_string();
    let bytes = b"never execute me";
    let stage = stage_artifact_for(
        &operation,
        std::io::Cursor::new(bytes),
        verified(bytes),
        root.dir(),
        deadline(),
        &|| false,
    )
    .await
    .unwrap();
    assert_eq!(
        stage.private_name(),
        format!(".update-{operation}.verified")
    );
    assert!(!stage.summary().installed);
    stage.retain();
    assert_eq!(root.names().len(), 1);
    assert!(stage_artifact_for(
        &operation,
        std::io::Cursor::new(bytes),
        verified(bytes),
        root.dir(),
        deadline(),
        &|| false
    )
    .await
    .is_err());
    assert_eq!(root.names().len(), 1);
    assert_eq!(root.dir().read(root.names()[0].clone()).unwrap(), bytes);
    assert!(stage_artifact_for(
        "../escape",
        std::io::Cursor::new(bytes),
        verified(bytes),
        root.dir(),
        deadline(),
        &|| false
    )
    .await
    .is_err());
}

#[tokio::test]
async fn create_new_collision_preserves_preexisting_partial_file() {
    let root = Root::new();
    let id = uuid::Uuid::new_v4().to_string();
    let name = format!(".update-{id}.part");
    root.dir()
        .write(&name, b"unowned existing partial bytes")
        .unwrap();
    let bytes = b"new signed fixture";
    assert!(stage_artifact_for(
        &id,
        std::io::Cursor::new(bytes),
        verified(bytes),
        root.dir(),
        deadline(),
        &|| false
    )
    .await
    .is_err());
    assert_eq!(
        root.dir().read(&name).unwrap(),
        b"unowned existing partial bytes"
    );
    assert_eq!(root.names(), vec![name]);
}

#[cfg(unix)]
#[tokio::test]
async fn create_new_collision_preserves_symlink_and_outside_bytes() {
    let root = Root::new();
    let outside = Root::new();
    outside
        .dir()
        .write("sentinel", b"outside must not change")
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let name = format!(".update-{id}.part");
    std::os::unix::fs::symlink(outside.path.join("sentinel"), root.path.join(&name)).unwrap();
    let bytes = b"new signed fixture";
    assert!(stage_artifact_for(
        &id,
        std::io::Cursor::new(bytes),
        verified(bytes),
        root.dir(),
        deadline(),
        &|| false
    )
    .await
    .is_err());
    assert!(std::fs::symlink_metadata(root.path.join(&name))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        outside.dir().read("sentinel").unwrap(),
        b"outside must not change"
    );
    assert_eq!(root.names(), vec![name]);
}

#[tokio::test]
async fn cancelled_staging_keeps_synchronous_write_and_cleanup_owned_until_return() {
    let root = Root::new();
    let dir = root.dir().try_clone().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let bytes = b"owned synchronous write";
    let manifest = verified(bytes);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancelled.clone();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let entered = std::sync::Mutex::new(Some(entered));
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    let handle = tokio::runtime::Handle::current();
    let worker = tokio::task::spawn_blocking(move || {
        let hook = || {
            if let Some(entered) = entered.lock().unwrap().take() {
                let _ = entered.send(());
                wait.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
        };
        handle.block_on(stage_artifact_inner(
            &id,
            std::io::Cursor::new(bytes),
            manifest,
            &dir,
            deadline(),
            &|| worker_cancel.load(AtomicOrdering::Acquire),
            Some(&hook),
        ))
    });
    ready.await.unwrap();
    cancelled.store(true, AtomicOrdering::Release);
    assert!(
        !worker.is_finished(),
        "cancellation is not proof the write owner has stopped"
    );
    assert_eq!(
        root.names().len(),
        1,
        "the owner still holds its partial file"
    );
    release.send(()).unwrap();
    assert!(matches!(worker.await.unwrap(), Err(UpdateError::Cancelled)));
    assert!(
        root.names().is_empty(),
        "cleanup finished before the worker returned"
    );
}
