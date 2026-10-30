use super::*;
struct Root {
    path: std::path::PathBuf,
    dir: Option<Dir>,
}
impl Root {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("sa-floor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        let path = p.canonicalize().unwrap();
        Self {
            dir: Some(Dir::open_ambient_dir(&path, cap_std::ambient_authority()).unwrap()),
            path,
        }
    }
    fn dir(&self) -> &Dir {
        self.dir.as_ref().unwrap()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        drop(self.dir.take());
        let result = std::fs::remove_dir_all(&self.path);
        if !std::thread::panicking() {
            result.unwrap()
        }
    }
}
fn record(version: &str, sequence: u64) -> FloorRecord {
    FloorRecord {
        schema: 1,
        operation_id: uuid::Uuid::new_v4().to_string(),
        claim_nonce: uuid::Uuid::new_v4().to_string(),
        target: "linux-x86_64".into(),
        version: version.into(),
        sequence,
        manifest_sha256: "a".repeat(64),
        artifact_sha256: "b".repeat(64),
    }
}
#[test]
fn append_survives_reopen_and_original_id_never_rewrites() {
    let r = Root::new();
    let a = record("1.0.0", 1);
    append(r.dir(), &a).unwrap();
    let bytes = r.dir().read(a.name()).unwrap();
    append(r.dir(), &a).unwrap();
    assert_eq!(r.dir().read(a.name()).unwrap(), bytes);
    let reopened = Dir::open_ambient_dir(&r.path, cap_std::ambient_authority()).unwrap();
    assert_eq!(load(&reopened).unwrap(), vec![a.clone()]);
    let mut changed = a;
    changed.sequence = 2;
    assert_eq!(append(&reopened, &changed), Err(WitnessError::Invalid));
    assert_eq!(load(&reopened).unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn fifo_witness_is_rejected_without_waiting_for_a_peer() {
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let root = Root::new();
    let name = record("1.0.0", 1).name();
    let path = root.path.join(name);
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let dir = root.dir().try_clone().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = send.send(load(&dir));
    });
    let response = receive.recv_timeout(std::time::Duration::from_secs(1));
    if response.is_err() {
        // Only a regressed blocking open needs a peer to let the failed test
        // reclaim its thread. The successful path never opens a writer.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path);
        let _ = receive.recv_timeout(std::time::Duration::from_secs(1));
    }
    worker.join().unwrap();
    assert_eq!(response.unwrap(), Err(WitnessError::Invalid));
}
#[test]
fn floor_is_monotonic_across_old_database_restore_and_key_changes() {
    let r = Root::new();
    append(r.dir(), &record("1.0.0", 10)).unwrap();
    append(r.dir(), &record("2.0.0", 20)).unwrap();
    let floor = floors(&load(r.dir()).unwrap()).unwrap();
    assert_eq!(floor["linux-x86_64"].version, "2.0.0");
    for bad in [
        record("1.5.0", 21),
        record("3.0.0", 15),
        record("2.0.0", 21),
    ] {
        assert_eq!(append(r.dir(), &bad), Err(WitnessError::Invalid));
    }
    assert_eq!(load(r.dir()).unwrap().len(), 2);
}
#[test]
fn malformed_partial_or_unknown_records_block_without_cleanup() {
    for bytes in [b"{".as_slice(), b"{}", b"private-input-canary"] {
        let r = Root::new();
        r.dir()
            .write("floor-00000000-0000-4000-8000-000000000000.json", bytes)
            .unwrap();
        let error = load(r.dir()).unwrap_err();
        assert!(!error.to_string().contains("canary"));
        assert_eq!(r.dir().entries().unwrap().count(), 1);
        assert!(append(r.dir(), &record("1.0.0", 1)).is_err());
    }
}
#[test]
fn mismatched_filename_target_digest_or_version_rejected() {
    let mut a = record("1.0.0", 1);
    for bad in ["../out", "not-a-uuid"] {
        a.operation_id = bad.into();
        assert!(a.validate().is_err());
    }
    let mut a = record("1.0.0", 1);
    a.target = "freeform-canary".into();
    assert!(a.validate().is_err());
    let mut a = record("1.0.0", 1);
    a.manifest_sha256 = "A".repeat(64);
    assert!(a.validate().is_err());
    assert!(record("1.0.0+build", 1).validate().is_err());
}
#[test]
fn independent_targets_do_not_lower_each_other() {
    let a = record("2.0.0", 10);
    let mut b = record("1.0.0", 1);
    b.target = "macos-aarch64".into();
    let floors = floors(&[a, b]).unwrap();
    assert_eq!(floors.len(), 2);
}
#[cfg(unix)]
#[test]
fn symlink_record_never_reads_or_writes_outside() {
    let r = Root::new();
    let outside = Root::new();
    let a = record("1.0.0", 1);
    outside.dir().write("secret", b"outside-canary").unwrap();
    std::os::unix::fs::symlink(outside.path.join("secret"), r.path.join(a.name())).unwrap();
    assert!(load(r.dir()).is_err());
    assert!(append(r.dir(), &a).is_err());
    assert_eq!(outside.dir().read("secret").unwrap(), b"outside-canary");
}

#[test]
fn witness_capacity_fails_closed_without_pruning_high_watermarks() {
    let root = Root::new();
    for n in 1..=MAX_WITNESSES {
        let r = record(&format!("1.0.{n}"), n as u64);
        root.dir()
            .write(r.name(), serde_json::to_vec(&r).unwrap())
            .unwrap();
    }
    assert_eq!(load(root.dir()).unwrap().len(), MAX_WITNESSES);
    assert_eq!(
        append(root.dir(), &record("2.0.0", 1000)),
        Err(WitnessError::Capacity)
    );
    assert_eq!(load(root.dir()).unwrap().len(), MAX_WITNESSES);
}
