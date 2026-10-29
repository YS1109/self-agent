use super::*;
fn db() -> Connection {
    let mut db = Connection::open_in_memory().unwrap();
    let tx = db.transaction().unwrap();
    migrate(&tx).unwrap();
    tx.commit().unwrap();
    db
}
fn trust() -> TrustInput {
    use ring::signature::KeyPair;
    let pkcs8 =
        ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    TrustInput {
        key_id: "fixture-key".into(),
        source: "https://updates.example.invalid/releases/".into(),
        public_key: pair.public_key().as_ref().try_into().unwrap(),
        enabled: true,
    }
}
fn setup() -> Connection {
    let mut db = db();
    let tx = db.transaction().unwrap();
    configure(&tx, None, trust(), 1).unwrap();
    tx.commit().unwrap();
    db
}
fn data(version: &str, sequence: u64) -> (Metadata, ManifestEvidence) {
    let raw = ManifestEvidence {
        raw_manifest: format!("{{\"version\":\"{version}\",\"sequence\":{sequence}}}"),
        signature_hex: "a".repeat(128),
    };
    let m = Metadata {
        version: version.into(),
        sequence,
        target: "linux-x86_64".into(),
        bytes: 10,
        sha256: "b".repeat(64),
        manifest_sha256: format!("{:x}", Sha256::digest(raw.raw_manifest.as_bytes())),
    };
    (m, raw)
}
fn queued(db: &mut Connection, version: &str, sequence: u64) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let (m, raw) = data(version, sequence);
    let rev = config(db).unwrap().unwrap().revision;
    let digest = request_digest(rev, &m, &raw).unwrap();
    let tx = db.transaction().unwrap();
    begin(&tx, &id, &digest, rev, m, raw, 2).unwrap();
    tx.commit().unwrap();
    id
}
fn claimed(db: &mut Connection, version: &str, sequence: u64) -> Claim {
    let id = queued(db, version, sequence);
    let tx = db.transaction().unwrap();
    let c = claim(&tx, &id, 3).unwrap().unwrap();
    tx.commit().unwrap();
    c
}
fn witnessed(c: &Claim) -> TrustedWitness {
    TrustedWitness {
        target: c.metadata.target.clone(),
        version: c.metadata.version.clone(),
        sequence: c.metadata.sequence,
        manifest_sha256: c.metadata.manifest_sha256.clone(),
        operation_id: c.id.clone(),
        claim_nonce: c.nonce.clone(),
        sha256: c.metadata.sha256.clone(),
    }
}
#[test]
fn explicit_migration_and_configuration_cas_are_atomic_verification_only() {
    let mut db = Connection::open_in_memory().unwrap();
    {
        let tx = db.transaction().unwrap();
        migrate(&tx).unwrap();
    }
    assert!(config(&db).is_err());
    let tx = db.transaction().unwrap();
    migrate(&tx).unwrap();
    migrate(&tx).unwrap();
    let c = configure(&tx, None, trust(), 1).unwrap();
    assert_eq!(c.revision, 1);
    assert_eq!(c.fingerprint, format!("{:x}", Sha256::digest(c.public_key)));
    assert!(configure(&tx, None, trust(), 2).is_err());
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    let mut disabled = trust();
    disabled.enabled = false;
    configure(&tx, Some(1), disabled, 2).unwrap();
    drop(tx);
    assert!(config(&db).unwrap().unwrap().enabled);
    assert_eq!(config(&db).unwrap().unwrap().revision, 1);
    let encoded = serde_json::to_string(&config(&db).unwrap()).unwrap();
    assert!(!encoded.contains("install"));
    assert!(!encoded.contains("download"));
}
#[test]
fn trust_input_rejects_non_https_auth_query_fragment_and_malformed_key_id() {
    for source in [
        "http://updates.example.invalid/",
        "https://name:pass@example.invalid/",
        "https://example.invalid/?key=x",
        "https://example.invalid/#x",
    ] {
        let mut db = db();
        let tx = db.transaction().unwrap();
        let mut t = trust();
        t.source = source.into();
        assert!(configure(&tx, None, t, 1).is_err());
        assert!(config(&tx).unwrap().is_none());
    }
    let mut db = db();
    let tx = db.transaction().unwrap();
    let mut t = trust();
    t.key_id = "../key".into();
    assert!(configure(&tx, None, t, 1).is_err());
}
#[test]
fn begin_is_exact_idempotent_private_evidence_and_claim_once() {
    let mut db = setup();
    let id = queued(&mut db, "1.1.0", 2);
    let (m, raw) = data("1.1.0", 2);
    let digest = request_digest(1, &m, &raw).unwrap();
    let tx = db.transaction().unwrap();
    let replay = begin(&tx, &id, &digest, 1, m.clone(), raw.clone(), 4).unwrap();
    assert_eq!(replay.created_at_ms, 2);
    assert!(!serde_json::to_string(&replay)
        .unwrap()
        .contains("raw_manifest"));
    assert!(!serde_json::to_string(&replay)
        .unwrap()
        .contains(&raw.signature_hex));
    let mut altered = raw.clone();
    altered.signature_hex = "b".repeat(128);
    assert!(begin(
        &tx,
        &id,
        &request_digest(1, &m, &altered).unwrap(),
        1,
        m,
        altered,
        4
    )
    .is_err());
    let claim = claim(&tx, &id, 5).unwrap().unwrap();
    assert_eq!(claim.evidence, raw);
    assert!(super::claim(&tx, &id, 6).unwrap().is_none());
    tx.commit().unwrap();
}
#[test]
fn evidence_bounds_hash_and_exact_request_digest_are_enforced() {
    let mut db = setup();
    let tx = db.transaction().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let (m, raw) = data("1.1.0", 2);
    assert!(begin(&tx, &id, &"0".repeat(64), 1, m.clone(), raw.clone(), 2).is_err());
    for bad in [
        "A".repeat(128),
        "a".repeat(127),
        format!("{}\n", "a".repeat(128)),
    ] {
        let raw = ManifestEvidence {
            signature_hex: bad,
            ..raw.clone()
        };
        assert!(request_digest(1, &m, &raw).is_err());
    }
    let huge = ManifestEvidence {
        raw_manifest: "x".repeat(16385),
        ..raw
    };
    assert!(request_digest(1, &m, &huge).is_err());
    assert!(receipts(&tx).unwrap().is_empty());
}
#[test]
fn successful_finalize_requires_exact_witness_and_keeps_floor_receipt_atomic() {
    let mut db = setup();
    let c = claimed(&mut db, "2.0.0", 10);
    let mut w = witnessed(&c);
    {
        let tx = db.transaction().unwrap();
        w.operation_id = uuid::Uuid::new_v4().to_string();
        assert!(complete_verified(&tx, &c, &w, 4).is_err());
        assert_eq!(floor(&tx, &w.target).unwrap().sequence, 0);
    }
    w = witnessed(&c);
    {
        let tx = db.transaction().unwrap();
        let r = complete_verified(&tx, &c, &w, 4).unwrap();
        assert!(r.stage_authority);
        assert_eq!(floor(&tx, &w.target).unwrap().sequence, 10);
    }
    assert_eq!(
        receipt(&db, &c.id).unwrap().unwrap().status,
        Status::Verifying
    );
    assert_eq!(floor(&db, &w.target).unwrap().sequence, 0);
    let tx = db.transaction().unwrap();
    let r = complete_verified(&tx, &c, &w, 4).unwrap();
    assert_eq!(complete_verified(&tx, &c, &w, 5).unwrap(), r);
    tx.commit().unwrap();
    validate(&db).unwrap();
}
#[test]
fn trust_rotation_invalidates_inflight_and_staged_authority_without_resetting_floor() {
    let mut db = setup();
    let a = claimed(&mut db, "2.0.0", 10);
    let tx = db.transaction().unwrap();
    complete_verified(&tx, &a, &witnessed(&a), 4).unwrap();
    tx.commit().unwrap();
    let b = claimed(&mut db, "3.0.0", 20);
    let tx = db.transaction().unwrap();
    let mut rotated = trust();
    rotated.key_id = "replacement-key".into();
    rotated.source = "https://another.example.invalid/manifest".into();
    configure(&tx, Some(1), rotated, 5).unwrap();
    assert_eq!(
        receipt(&tx, &b.id).unwrap().unwrap().status,
        Status::Unknown
    );
    assert!(!receipt(&tx, &a.id).unwrap().unwrap().stage_authority);
    assert_eq!(
        receipt(&tx, &a.id).unwrap().unwrap().status,
        Status::Verified
    );
    assert!(complete_verified(&tx, &b, &witnessed(&b), 6).is_err());
    assert_eq!(floor(&tx, "linux-x86_64").unwrap().sequence, 10);
    tx.commit().unwrap();
    let (m, raw) = data("1.9.0", 9);
    let tx = db.transaction().unwrap();
    assert!(begin(
        &tx,
        &uuid::Uuid::new_v4().to_string(),
        &request_digest(2, &m, &raw).unwrap(),
        2,
        m,
        raw,
        7
    )
    .is_err());
}
#[test]
fn restart_interrupts_pending_and_unknown_claimed_without_replay() {
    let mut db = setup();
    let pending = queued(&mut db, "2.0.0", 10);
    let verifying = claimed(&mut db, "3.0.0", 20);
    let tx = db.transaction().unwrap();
    recover(&tx, 8).unwrap();
    recover(&tx, 9).unwrap();
    assert_eq!(
        receipt(&tx, &pending).unwrap().unwrap().status,
        Status::Interrupted
    );
    assert_eq!(
        receipt(&tx, &verifying.id).unwrap().unwrap().status,
        Status::Unknown
    );
    assert!(claim(&tx, &pending, 10).unwrap().is_none());
    assert!(claim(&tx, &verifying.id, 10).unwrap().is_none());
    assert!(complete_verified(&tx, &verifying, &witnessed(&verifying), 11).is_err());
}
#[test]
fn witness_history_is_monotonic_crossed_or_equal_digest_conflicts_fail() {
    let mut db = setup();
    let low = claimed(&mut db, "2.0.0", 10);
    let high = claimed(&mut db, "3.0.0", 20);
    let tx = db.transaction().unwrap();
    let top = merge_witness(&tx, &witnessed(&high)).unwrap();
    assert_eq!(merge_witness(&tx, &witnessed(&low)).unwrap(), top);
    let mut cross = witnessed(&low);
    cross.version = "4.0.0".into();
    assert!(merge_witness(&tx, &cross).is_err());
    cross.version = "1.0.0".into();
    cross.sequence = 30;
    assert!(merge_witness(&tx, &cross).is_err());
    let mut conflict = witnessed(&high);
    conflict.sha256 = "c".repeat(64);
    assert!(merge_witness(&tx, &conflict).is_err());
    assert_eq!(floor(&tx, "linux-x86_64").unwrap(), top);
}
#[test]
fn floor_import_after_crash_or_old_backup_never_reopens_downgrade() {
    let mut db = setup();
    let c = claimed(&mut db, "3.0.0", 20);
    let w = witnessed(&c);
    // Simulates witness fsync then crash before the DB completion transaction.
    let tx = db.transaction().unwrap();
    recover(&tx, 5).unwrap();
    merge_witness(&tx, &w).unwrap();
    restore(&tx, 6).unwrap();
    assert_eq!(floor(&tx, &w.target).unwrap().sequence, 20);
    let cfg = config(&tx).unwrap().unwrap();
    assert!(!cfg.enabled);
    assert_eq!(
        receipt(&tx, &c.id).unwrap().unwrap().status,
        Status::Unknown
    );
    tx.commit().unwrap();
    // A separate old backup has no floor. Trusted external witness restores it.
    let mut old = setup();
    let tx = old.transaction().unwrap();
    restore(&tx, 7).unwrap();
    merge_witness(&tx, &w).unwrap();
    let rev = config(&tx).unwrap().unwrap().revision;
    configure(&tx, Some(rev), trust(), 8).unwrap();
    let (m, raw) = data("2.9.0", 19);
    assert!(begin(
        &tx,
        &uuid::Uuid::new_v4().to_string(),
        &request_digest(rev + 1, &m, &raw).unwrap(),
        rev + 1,
        m,
        raw,
        9
    )
    .is_err());
    let effective = effective_floor(&tx, &w.target, "4.0.0", Some(&w)).unwrap();
    assert_eq!(effective.version, "4.0.0");
    assert_eq!(effective.sequence, 20);
}
#[test]
fn operation_capacity_preserves_audit_and_does_not_auto_prune() {
    let mut db = setup();
    for n in 0..256 {
        queued(&mut db, "2.0.0", n + 1);
    }
    let (m, raw) = data("2.0.0", 257);
    let tx = db.transaction().unwrap();
    assert!(begin(
        &tx,
        &uuid::Uuid::new_v4().to_string(),
        &request_digest(1, &m, &raw).unwrap(),
        1,
        m,
        raw,
        2
    )
    .is_err());
    assert_eq!(receipts(&tx).unwrap().len(), 256);
}
#[test]
fn failed_and_unknown_work_have_no_stage_authority_and_never_reclaim() {
    for reason in [Failure::Rejected, Failure::Cancelled, Failure::Unknown] {
        let mut db = setup();
        let c = claimed(&mut db, "2.0.0", 10);
        let tx = db.transaction().unwrap();
        let r = fail(&tx, &c, reason, 8).unwrap();
        assert!(!r.stage_authority);
        assert!(claim(&tx, &c.id, 9).unwrap().is_none());
        assert!(complete_verified(&tx, &c, &witnessed(&c), 10).is_err());
        assert_eq!(floor(&tx, "linux-x86_64").unwrap().sequence, 0);
    }
}
#[test]
fn malformed_index_and_raw_evidence_fail_closed() {
    let mut db = setup();
    let id = queued(&mut db, "2.0.0", 10);
    db.execute("UPDATE update_jobs SET status='verified' WHERE id=?1", [id])
        .unwrap();
    assert!(validate(&db).is_err());
    let db = setup();
    db.execute("UPDATE update_trust SET revision=2", [])
        .unwrap();
    assert!(config(&db).is_err());
}

#[test]
fn concurrent_lower_claim_cannot_finalize_after_newer_floor_even_with_old_witness() {
    let mut db = setup();
    let low = claimed(&mut db, "2.0.0", 10);
    let high = claimed(&mut db, "3.0.0", 20);
    let tx = db.transaction().unwrap();
    complete_verified(&tx, &high, &witnessed(&high), 4).unwrap();
    assert!(complete_verified(&tx, &low, &witnessed(&low), 5).is_err());
    assert_eq!(
        receipt(&tx, &low.id).unwrap().unwrap().status,
        Status::Verifying
    );
    assert!(!receipt(&tx, &low.id).unwrap().unwrap().stage_authority);
    assert_eq!(floor(&tx, "linux-x86_64").unwrap().sequence, 20);
}
