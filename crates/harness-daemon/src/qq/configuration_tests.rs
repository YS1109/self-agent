use super::super::persistence::{ConversationKind, Identity, Inbound};
use super::*;
const NOW: u64 = 1_800_000_000_000;
fn db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    persistence::migrate(&db).unwrap();
    migrate(&db).unwrap();
    db
}
fn config(db: &mut Connection) -> Config {
    let tx = db.transaction().unwrap();
    let c = create(&tx, "app_1").unwrap();
    tx.commit().unwrap();
    c
}
fn patch(c: &Config, enabled: bool) -> ConfigPatch {
    ConfigPatch {
        app_id: c.app_id.clone(),
        enabled,
        reply_policy: c.reply_policy.clone(),
    }
}
fn enable(db: &mut Connection, c: &Config) -> Config {
    let tx = db.transaction().unwrap();
    let c = update(&tx, &c.id, c.revision, patch(c, true), NOW, |_| Ok(true)).unwrap();
    tx.commit().unwrap();
    c
}
fn point(c: &Config, session: &str, seq: u64) -> Checkpoint {
    Checkpoint {
        config_id: c.id.clone(),
        config_revision: c.revision,
        session_id: session.into(),
        sequence: seq,
        bot_user_id: "bot_1".into(),
    }
}
#[test]
fn creation_cas_and_credential_scope() {
    let mut db = db();
    let c = config(&mut db);
    assert!(!c.enabled);
    assert_eq!(uuid::Uuid::parse_str(&c.id).unwrap().to_string(), c.id);
    let digest = credential_binding_digest(&c.app_id).unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(update(&tx, &c.id, 0, patch(&c, true), NOW, |_| Ok(false)).is_err());
    }
    let tx = db.transaction().unwrap();
    let attached = attach_credential(&tx, &c.id, 0, "private_ref", |reference, hash| {
        assert_eq!(reference, "private_ref");
        assert_eq!(hash, digest);
        Ok(())
    })
    .unwrap();
    tx.commit().unwrap();
    assert!(!attached.public_view().to_string().contains("private_ref"));
    assert_eq!(attached.credential_ref(), Some("private_ref"));
    {
        let tx = db.transaction().unwrap();
        assert!(update(&tx, &c.id, 0, patch(&c, false), NOW, |_| Ok(true)).is_err());
    }
    {
        let tx = db.transaction().unwrap();
        let mut p = patch(&attached, false);
        p.app_id = "app_2".into();
        assert!(update(&tx, &c.id, attached.revision, p, NOW, |_| Ok(true)).is_err());
    }
    let tx = db.transaction().unwrap();
    let detached = disconnect_credential(&tx, &c.id, attached.revision, NOW).unwrap();
    let mut p = patch(&detached, false);
    p.app_id = "app_2".into();
    let swapped = update(&tx, &c.id, detached.revision, p, NOW, |_| Ok(false)).unwrap();
    tx.commit().unwrap();
    assert_ne!(credential_binding_digest(&swapped.app_id).unwrap(), digest);
}
#[test]
fn checkpoint_fences_restart_and_session_replacement() {
    let mut db = db();
    let c = config(&mut db);
    let c = enable(&mut db, &c);
    let tx = db.transaction().unwrap();
    ready_checkpoint(&tx, &point(&c, "s1", 10)).unwrap();
    advance_checkpoint(&tx, &c.id, c.revision, "s1", 11).unwrap();
    tx.commit().unwrap();
    for (session, seq) in [("s1", 9), ("s2", 12), ("s1", u64::MAX)] {
        let tx = db.transaction().unwrap();
        assert!(advance_checkpoint(&tx, &c.id, c.revision, session, seq).is_err());
    }
    {
        let tx = db.transaction().unwrap();
        let mut p = point(&c, "s1", 12);
        p.bot_user_id = "other".into();
        assert!(ready_checkpoint(&tx, &p).is_err());
    }
    let tx = db.transaction().unwrap();
    startup_recover(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(connection_view(&db).unwrap()["state"], "disconnected");
    assert_eq!(
        load_checkpoint(&db, &c.id, c.revision)
            .unwrap()
            .unwrap()
            .sequence,
        11
    );
    let tx = db.transaction().unwrap();
    assert!(ready_checkpoint(&tx, &point(&c, "s2", 0)).is_err());
    tx.rollback().unwrap();
    let tx = db.transaction().unwrap();
    clear_checkpoint(&tx, &c.id, c.revision).unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    ready_checkpoint(&tx, &point(&c, "s2", 0)).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        load_checkpoint(&db, &c.id, c.revision)
            .unwrap()
            .unwrap()
            .session_id,
        "s2"
    );
    let tx = db.transaction().unwrap();
    let c2 = update(&tx, &c.id, c.revision, patch(&c, true), NOW, |_| Ok(true)).unwrap();
    tx.commit().unwrap();
    assert!(load_checkpoint(&db, &c.id, c.revision).is_err());
    assert!(load_checkpoint(&db, &c2.id, c2.revision).unwrap().is_none());
    let tx = db.transaction().unwrap();
    assert!(ready_checkpoint(&tx, &point(&c, "stale", 20)).is_err());
}
#[test]
fn disable_revokes_pending_active_pairing_and_long_task() {
    let mut db = db();
    let c = config(&mut db);
    let c = enable(&mut db, &c);
    let identity = Identity {
        app_id: c.app_id.clone(),
        sender_open_id: "sender".into(),
        kind: ConversationKind::User,
        target_id: "sender".into(),
    };
    let tx = db.transaction().unwrap();
    let code = persistence::issue_pairing(&tx, &c.app_id, NOW, 60).unwrap();
    let pending = persistence::redeem_pairing(&tx, &identity, &code.code, NOW).unwrap();
    let binding = persistence::confirm_binding(
        &tx,
        &pending.principal_id,
        pending.revision,
        &identity,
        json!({"session_ids":[]}),
        NOW + 600_000,
        NOW - 31_000,
    )
    .unwrap();
    let input = Inbound {
        identity: identity.clone(),
        message_id: "msg".into(),
        event_aliases: vec![],
        canonical_input: json!({"content":"hello"}),
        received_at_ms: NOW,
    };
    let accepted = persistence::accept_input(&tx, &input, NOW).unwrap();
    let extra = persistence::issue_pairing(&tx, &c.app_id, NOW, 60).unwrap();
    let other = Identity {
        sender_open_id: "other".into(),
        target_id: "other".into(),
        ..identity.clone()
    };
    let other_pending = persistence::redeem_pairing(&tx, &other, &extra.code, NOW).unwrap();
    let unclaimed = persistence::issue_pairing(&tx, &c.app_id, NOW, 60).unwrap();
    tx.commit().unwrap();
    let route = accepted.route_id.unwrap();
    assert!(persistence::guard_route(&db, &route, NOW + 300_001).is_err());
    assert!(live_task_route(&db, &route, NOW + 300_001).is_ok());
    assert!(live_task_route(&db, &route, NOW + 600_000).is_err());
    let tx = db.transaction().unwrap();
    let disabled = update(&tx, &c.id, c.revision, patch(&c, false), NOW + 1, |_| {
        Ok(false)
    })
    .unwrap();
    tx.commit().unwrap();
    for id in [&binding.principal_id, &other_pending.principal_id] {
        assert_eq!(persistence::binding(&db, id).unwrap().status, "revoked");
    }
    let tx = db.transaction().unwrap();
    assert!(persistence::redeem_pairing(&tx, &identity, &unclaimed.code, NOW + 2).is_err());
    drop(tx);
    let _reenabled = enable(&mut db, &disabled);
    assert!(live_task_route(&db, &route, NOW + 3).is_err());
}
#[test]
fn errors_rollback_and_strict_bounds() {
    let mut db = db();
    let c = config(&mut db);
    {
        let tx = db.transaction().unwrap();
        assert!(attach_credential(&tx, &c.id, 0, "ref", |_, _| bail!("夹具拒绝")).is_err());
    }
    assert!(current(&db).unwrap().unwrap().credential_ref().is_none());
    for app in ["", "https://evil", "app\n"] {
        assert!(credential_binding_digest(app).is_err());
    }
    {
        let tx = db.transaction().unwrap();
        let mut p = patch(&c, true);
        p.reply_policy.window_ms = 300001;
        assert!(update(&tx, &c.id, 0, p, NOW, |_| Ok(true)).is_err());
    }
    assert!(serde_json::from_value::<ConfigPatch>(
        json!({"app_id":"app","enabled":false,"reply_policy":{},"endpoint":"http://fixture"})
    )
    .is_err());
    let tx = db.transaction().unwrap();
    assert!(set_connection_state(&tx, &c.id, 0, ConnectionState::Ready, None).is_err());
    tx.rollback().unwrap();
    assert_eq!(current(&db).unwrap().unwrap().revision, 0);
}

#[test]
fn failed_update_rolls_back_revocation_and_app_swap_invalidates_pairing() {
    let mut db = db();
    let c = config(&mut db);
    let tx = db.transaction().unwrap();
    let ticket = persistence::issue_pairing(&tx, &c.app_id, NOW, 60).unwrap();
    tx.commit().unwrap();
    // App change revokes before readiness validation: failure must roll back all.
    {
        let tx = db.transaction().unwrap();
        let mut p = patch(&c, true);
        p.app_id = "app_2".into();
        assert!(update(&tx, &c.id, c.revision, p, NOW, |_| Ok(false)).is_err());
        tx.rollback().unwrap();
    }
    let count: u64 = db
        .query_row("SELECT count(*) FROM qq_pairings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(current(&db).unwrap().unwrap().app_id, c.app_id);
    let tx = db.transaction().unwrap();
    let mut p = patch(&c, false);
    p.app_id = "app_2".into();
    update(&tx, &c.id, c.revision, p, NOW, |_| Ok(false)).unwrap();
    tx.commit().unwrap();
    let count: u64 = db
        .query_row(
            "SELECT count(*) FROM qq_pairings WHERE id=?1",
            [&ticket.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn clear_checkpoint_fences_and_preserves_no_false_ready() {
    let mut db = db();
    let c = config(&mut db);
    let c = enable(&mut db, &c);
    let tx = db.transaction().unwrap();
    ready_checkpoint(&tx, &point(&c, "session", 1)).unwrap();
    tx.commit().unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(clear_checkpoint(&tx, &c.id, c.revision + 1).is_err());
    }
    assert!(load_checkpoint(&db, &c.id, c.revision).unwrap().is_some());
    let tx = db.transaction().unwrap();
    clear_checkpoint(&tx, &c.id, c.revision).unwrap();
    tx.commit().unwrap();
    assert!(load_checkpoint(&db, &c.id, c.revision).unwrap().is_none());
    assert_eq!(connection_view(&db).unwrap()["state"], "disconnected");
    let tx = db.transaction().unwrap();
    assert!(set_connection_state(&tx, &c.id, c.revision, ConnectionState::Ready, None).is_err());
}

#[test]
fn checkpoint_identifiers_match_transport_and_events() {
    let mut db = db();
    let c = config(&mut db);
    let c = enable(&mut db, &c);
    for invalid in [
        "x.y".to_owned(),
        "a/b".into(),
        "中文".into(),
        "x".repeat(129),
    ] {
        for bot in [false, true] {
            let mut p = point(&c, "session", 0);
            if bot {
                p.bot_user_id = invalid.clone();
            } else {
                p.session_id = invalid.clone();
            }
            let tx = db.transaction().unwrap();
            assert!(ready_checkpoint(&tx, &p).is_err());
        }
    }
    let tx = db.transaction().unwrap();
    let mut p = point(&c, &"x".repeat(128), 0);
    p.bot_user_id = "bot_-1".into();
    ready_checkpoint(&tx, &p).unwrap();
    advance_checkpoint(&tx, &c.id, c.revision, &p.session_id, 0).unwrap();
    tx.commit().unwrap();
}
#[test]
fn connection_update_timestamp_is_persisted_not_read_time() {
    let mut db = db();
    let before = chrono::Utc::now().timestamp_millis();
    let c = config(&mut db);
    let first = connection_view(&db).unwrap();
    let stamp = first["updated_at_ms"].as_i64().unwrap();
    assert!(stamp >= before);
    assert!(stamp <= chrono::Utc::now().timestamp_millis());
    // Sentinel persisted timestamp proves views do not replace it with read time.
    db.execute("UPDATE qq_connection_state SET updated_at_ms=123", [])
        .unwrap();
    assert_eq!(connection_view(&db).unwrap()["updated_at_ms"], 123);
    let tx = db.transaction().unwrap();
    set_connection_state(
        &tx,
        &c.id,
        c.revision,
        ConnectionState::Disabled,
        Some(Reason::Stopped),
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(
        connection_view(&db).unwrap()["updated_at_ms"]
            .as_i64()
            .unwrap()
            >= before
    );
}
