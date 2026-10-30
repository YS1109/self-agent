use super::*;
const DEVICE: &str = "11111111-1111-4111-8111-111111111111";
const OTHER: &str = "22222222-2222-4222-8222-222222222222";
const NOW: u64 = 1_800_000_000_000;
fn database() -> Connection {
    let mut db = Connection::open_in_memory().unwrap();
    let tx = db.transaction().unwrap();
    persistence::migrate(&tx).unwrap();
    migrate(&tx).unwrap();
    super::super::credentials::migrate(&tx).unwrap();
    tx.commit().unwrap();
    db
}
fn patch(c: &Config, enabled: bool) -> ConfigPatch {
    ConfigPatch {
        device_id: c.device_id.clone(),
        origin: c.origin.clone(),
        enabled,
    }
}
fn configured(db: &mut Connection) -> Config {
    let tx = db.transaction().unwrap();
    let c = create(&tx, "https://EXAMPLE.com:443/", DEVICE).unwrap();
    tx.commit().unwrap();
    c
}
#[test]
fn configuration_is_singleton_disabled_normalized_and_has_no_fixture_or_secret_fields() {
    let mut db = database();
    let c = configured(&mut db);
    assert!(!c.enabled);
    assert_eq!(c.revision, 1);
    assert_eq!(c.origin, "https://example.com/");
    assert_eq!(CREDENTIAL_PURPOSE, "gateway_device_token");
    let tx = db.transaction().unwrap();
    assert!(create(&tx, "https://elsewhere.test/", OTHER).is_err());
    assert!(serde_json::from_value::<ConfigPatch>(
        json!({"origin":c.origin,"device_id":DEVICE,"enabled":true,"fixture":true})
    )
    .is_err());
    assert!(serde_json::from_value::<ConfigPatch>(
        json!({"origin":c.origin,"device_id":DEVICE,"enabled":true,"token":"x"})
    )
    .is_err());
    assert!(update(&tx, &c.id, c.revision, patch(&c, true), NOW, |_| Ok(false)).is_err());
    for endpoint in [
        "http://127.0.0.1/",
        "https://user@example.com/",
        "https://example.com/path",
        "https://example.com/?key=x",
    ] {
        assert!(credential_binding_digest(endpoint, DEVICE).is_err());
    }
}
#[test]
fn credential_identity_digest_binds_origin_device_protocol_and_private_reference() {
    let mut db = database();
    let c = configured(&mut db);
    assert_eq!(
        c.scope_digest,
        credential_binding_digest("https://example.com:443/", DEVICE).unwrap()
    );
    assert_ne!(
        c.scope_digest,
        credential_binding_digest("https://example.com/", OTHER).unwrap()
    );
    assert_ne!(
        c.scope_digest,
        credential_binding_digest("https://other.test/", DEVICE).unwrap()
    );
    let tx = db.transaction().unwrap();
    let attached = attach_credential(
        &tx,
        &c.id,
        c.revision,
        "private-vault-reference",
        |reference, digest| {
            assert_eq!(reference, "private-vault-reference");
            assert_eq!(digest, c.scope_digest);
            Ok(())
        },
    )
    .unwrap();
    assert!(!attached
        .public_view()
        .to_string()
        .contains("private-vault-reference"));
    assert!(!attached.public_view().to_string().contains("scope_digest"));
    let changed = ConfigPatch {
        origin: "https://other.test/".into(),
        ..patch(&attached, false)
    };
    assert!(
        update(&tx, &attached.id, attached.revision, changed, NOW, |_| Ok(
            true
        ))
        .is_err()
    );
    let changed = ConfigPatch {
        device_id: OTHER.into(),
        ..patch(&attached, false)
    };
    assert!(
        update(&tx, &attached.id, attached.revision, changed, NOW, |_| Ok(
            true
        ))
        .is_err()
    );
    let detached = disconnect_credential(&tx, &attached.id, attached.revision, NOW).unwrap();
    assert!(detached.credential_ref().is_none());
    let changed = ConfigPatch {
        origin: "https://other.test/".into(),
        ..patch(&detached, false)
    };
    assert!(
        update(&tx, &detached.id, detached.revision, changed, NOW, |_| Ok(
            false
        ))
        .is_ok()
    );
    assert!(validate_device_token(&[b'a'; 64]).is_ok());
    for token in [vec![b'a'; 63], vec![b'G'; 64], vec![b'\n'; 64]] {
        assert!(validate_device_token(&token).is_err());
    }
}
#[test]
fn disable_and_reenable_revoke_issued_codes_and_do_not_resurrect_authority() {
    let mut db = database();
    let c = configured(&mut db);
    let tx = db.transaction().unwrap();
    let c = attach_credential(&tx, &c.id, c.revision, "vault-ref", |_, _| Ok(())).unwrap();
    let c = update(&tx, &c.id, c.revision, patch(&c, true), NOW, |_| Ok(true)).unwrap();
    let channel = persistence::TrustedChannel {
        device_id: DEVICE.into(),
        config_revision: c.revision,
    };
    let code = persistence::issue_pairing(&tx, &channel, NOW, 60).unwrap();
    let disabled = update(&tx, &c.id, c.revision, patch(&c, false), NOW, |_| {
        panic!("disable reads credential")
    })
    .unwrap();
    assert!(!disabled.enabled);
    assert_eq!(
        tx.query_row(
            "SELECT status FROM gateway_pairings WHERE id=?1",
            [code.id],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "revoked"
    );
    let enabled = update(
        &tx,
        &disabled.id,
        disabled.revision,
        patch(&disabled, true),
        NOW,
        |_| Ok(true),
    )
    .unwrap();
    assert!(enabled.enabled);
    assert!(enabled.revision > c.revision);
    assert!(load(&tx, &c.id, c.revision).is_err());
}
#[test]
fn stale_and_failed_config_updates_are_atomic_and_restore_disables() {
    let mut db = database();
    let c = configured(&mut db);
    {
        let tx = db.transaction().unwrap();
        let c = attach_credential(&tx, &c.id, c.revision, "vault-ref", |_, _| Ok(())).unwrap();
        let _ = update(&tx, &c.id, c.revision, patch(&c, true), NOW, |_| Ok(true)).unwrap();
        tx.commit().unwrap();
    }
    let saved = current(&db).unwrap().unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(update(
            &tx,
            &saved.id,
            saved.revision,
            patch(&saved, true),
            NOW,
            |_| Ok(false)
        )
        .is_err());
    }
    assert_eq!(current(&db).unwrap().unwrap().revision, saved.revision);
    let tx = db.transaction().unwrap();
    invalidate_restored(&tx, NOW).unwrap();
    let restored = current(&tx).unwrap().unwrap();
    assert!(!restored.enabled);
    assert!(restored.credential_ref().is_none());
    assert!(restored.revision > saved.revision);
}

#[test]
fn explicit_trusted_fixture_readiness_never_fabricates_a_vault_reference() {
    let mut db = database();
    let c = configured(&mut db);
    let tx = db.transaction().unwrap();
    let enabled = update(&tx, &c.id, c.revision, patch(&c, true), NOW, |candidate| {
        assert!(candidate.credential_ref().is_none());
        // Outer process-only fixture verifies its canary; no public flag exists.
        Ok(true)
    })
    .unwrap();
    assert!(enabled.enabled);
    assert!(enabled.credential_ref().is_none());
    assert_eq!(enabled.public_view()["credential_state"], "missing");
}
