use super::super::{
    configuration::{Checkpoint, ConfigPatch},
    credentials::{self, Outcome},
    persistence::{ConversationKind, DeliveryOutcome, Identity, Inbound, OutboxKind},
};
use super::*;
use rusqlite::Connection;
const NOW: u64 = 1_800_000_000_000;
struct Fixture {
    db: Connection,
    config: String,
    binding: String,
    route: String,
    input: String,
    sent: String,
    unknown: String,
    sending: String,
    pending: String,
    credential_unknown: String,
    credential_completed: String,
    credential_pending: String,
}
fn fixture() -> Fixture {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    persistence::migrate(&db).unwrap();
    configuration::migrate(&db).unwrap();
    credentials::migrate(&db).unwrap();
    super::super::engine::migrate(&db).unwrap();
    super::super::notifications::migrate(&db).unwrap();
    let tx = db.transaction().unwrap();
    let c = configuration::create(&tx, "app_1").unwrap();
    let c = configuration::update(
        &tx,
        &c.id,
        c.revision,
        ConfigPatch {
            app_id: c.app_id.clone(),
            enabled: true,
            reply_policy: c.reply_policy.clone(),
        },
        NOW,
        |_| Ok(true),
    )
    .unwrap();
    let who = Identity {
        app_id: c.app_id.clone(),
        sender_open_id: "member".into(),
        kind: ConversationKind::Group,
        target_id: "group".into(),
    };
    let code = persistence::issue_pairing(&tx, &c.app_id, NOW, 60).unwrap();
    let pending = persistence::redeem_pairing(&tx, &who, &code.code, NOW).unwrap();
    let b = persistence::confirm_binding(
        &tx,
        &pending.principal_id,
        pending.revision,
        &who,
        json!({"session_ids":["session"]}),
        NOW + 600000,
        NOW - 31000,
    )
    .unwrap();
    persistence::grant_session(&tx, &b.principal_id, "derived", NOW).unwrap();
    tx.execute("INSERT INTO qq_selections VALUES('fixture','derived')", [])
        .unwrap();
    let spare = persistence::issue_pairing(&tx, &c.app_id, NOW, 60).unwrap();
    let other = Identity {
        sender_open_id: "another".into(),
        ..who.clone()
    };
    persistence::redeem_pairing(&tx, &other, &spare.code, NOW).unwrap();
    persistence::issue_pairing(&tx, &c.app_id, NOW, 60).unwrap();
    let incoming = Inbound {
        identity: who,
        message_id: "source_message".into(),
        event_aliases: vec!["alias".into()],
        canonical_input: json!({"kind":"command","content":"/帮助"}),
        received_at_ms: NOW,
    };
    let input = persistence::accept_input(&tx, &incoming, NOW).unwrap();
    let route = input.route_id.unwrap();
    let sent =
        persistence::enqueue_outbox(&tx, &route, "sent", OutboxKind::Ack, "已送达", NOW).unwrap();
    let claim = persistence::claim_outbox(&tx, NOW).unwrap().unwrap();
    persistence::finalize_outbox(
        &tx,
        &sent.id,
        claim.record.claim_nonce.as_deref().unwrap(),
        DeliveryOutcome::Sent {
            platform_id: "platform_1".into(),
        },
        NOW,
    )
    .unwrap();
    let unknown =
        persistence::enqueue_outbox(&tx, &route, "unknown", OutboxKind::Completion, "未知", NOW)
            .unwrap();
    let claim = persistence::claim_outbox(&tx, NOW).unwrap().unwrap();
    persistence::finalize_outbox(
        &tx,
        &unknown.id,
        claim.record.claim_nonce.as_deref().unwrap(),
        DeliveryOutcome::Unknown,
        NOW,
    )
    .unwrap();
    let sending =
        persistence::enqueue_outbox(&tx, &route, "sending", OutboxKind::Completion, "在途", NOW)
            .unwrap();
    persistence::claim_outbox(&tx, NOW).unwrap().unwrap();
    let pending =
        persistence::enqueue_outbox(&tx, &route, "pending", OutboxKind::Completion, "待发", NOW)
            .unwrap();
    configuration::ready_checkpoint(
        &tx,
        &Checkpoint {
            config_id: c.id.clone(),
            config_revision: c.revision,
            session_id: "session_1".into(),
            sequence: 12,
            bot_user_id: "bot_1".into(),
        },
    )
    .unwrap();
    let unknown_id = uuid::Uuid::new_v4().to_string();
    credentials::begin_set(
        &tx,
        &unknown_id,
        &c.id,
        c.revision,
        "private_ref_unknown",
        |_, _, _| Ok(()),
    )
    .unwrap();
    credentials::claim(&tx, &unknown_id, |_, _, _| Ok(())).unwrap();
    credentials::finish(&tx, &unknown_id, Outcome::Unknown, |_, _, _| Ok(())).unwrap();
    let completed = uuid::Uuid::new_v4().to_string();
    credentials::begin_set(
        &tx,
        &completed,
        &c.id,
        c.revision,
        "private_ref_bound",
        |_, _, _| Ok(()),
    )
    .unwrap();
    credentials::claim(&tx, &completed, |_, _, _| Ok(())).unwrap();
    credentials::finish(&tx, &completed, Outcome::Succeeded, |_, _, _| Ok(())).unwrap();
    let current = configuration::current(&tx).unwrap().unwrap();
    let credential_pending = uuid::Uuid::new_v4().to_string();
    credentials::begin_set(
        &tx,
        &credential_pending,
        &c.id,
        current.revision,
        "private_ref_pending",
        |_, _, _| Ok(()),
    )
    .unwrap();
    configuration::ready_checkpoint(
        &tx,
        &Checkpoint {
            config_id: c.id.clone(),
            config_revision: current.revision,
            session_id: "session_restored".into(),
            sequence: 13,
            bot_user_id: "bot_1".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    Fixture {
        db,
        config: c.id,
        binding: b.principal_id,
        route,
        input: input.inbox_id,
        sent: sent.id,
        unknown: unknown.id,
        sending: sending.id,
        pending: pending.id,
        credential_unknown: unknown_id,
        credential_completed: completed,
        credential_pending,
    }
}
fn count(db: &Connection, table: &str) -> u64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
#[test]
fn disables_restored_authority_without_replaying_or_deleting_audit_routes() {
    let mut f = fixture();
    let sent_before = persistence::outbox(&f.db, &f.sent).unwrap();
    let tx = f.db.transaction().unwrap();
    invalidate(&tx).unwrap();
    tx.commit().unwrap();
    let c = configuration::current(&f.db).unwrap().unwrap();
    assert!(!c.enabled);
    assert!(c.credential_ref().is_none());
    assert_eq!(
        configuration::connection_view(&f.db).unwrap()["state"],
        "disabled"
    );
    assert_eq!(count(&f.db, "qq_checkpoint"), 0);
    assert_eq!(count(&f.db, "qq_session_acl"), 0);
    assert_eq!(count(&f.db, "qq_selections"), 0);
    let b = persistence::binding(&f.db, &f.binding).unwrap();
    assert_eq!(b.status, "revoked");
    assert_eq!(b.expires_at_ms, 0);
    assert_eq!(
        persistence::route(&f.db, &f.route).unwrap().expires_at_ms,
        0
    );
    assert_eq!(count(&f.db, "qq_reply_routes"), 1);
    assert_eq!(count(&f.db, "qq_event_aliases"), 1);
    assert_eq!(persistence::outbox(&f.db, &f.sent).unwrap().status, "sent");
    assert_eq!(
        persistence::outbox(&f.db, &f.sent).unwrap().platform_id,
        sent_before.platform_id
    );
    assert_eq!(
        persistence::outbox(&f.db, &f.unknown).unwrap().status,
        "unknown"
    );
    assert_eq!(
        persistence::outbox(&f.db, &f.sending).unwrap().status,
        "unknown"
    );
    assert_eq!(
        persistence::outbox(&f.db, &f.pending).unwrap().status,
        "revoked"
    );
    assert_eq!(
        persistence::inbox(&f.db, &f.input)
            .unwrap()
            .receipt
            .unwrap()["reason"],
        "restored_no_replay"
    );
    assert!(persistence::pending_inputs(&f.db, 128).unwrap().is_empty());
    let tx = f.db.transaction().unwrap();
    assert!(persistence::claim_outbox(&tx, NOW).unwrap().is_none());
    assert!(tx
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query([])
        .unwrap()
        .next()
        .unwrap()
        .is_none());
    tx.commit().unwrap();
}
#[test]
fn restored_credential_unknown_and_completed_facts_survive_but_cleanup_is_denied() {
    let mut f = fixture();
    let tx = f.db.transaction().unwrap();
    invalidate(&tx).unwrap();
    tx.commit().unwrap();
    for (id, status) in [
        (&f.credential_unknown, "unknown"),
        (&f.credential_completed, "completed"),
        (&f.credential_pending, "unknown"),
    ] {
        let op = credentials::operation(&f.db, id).unwrap();
        assert_eq!(op.status, status);
        assert!(!op.can_cleanup);
        let tx = f.db.transaction().unwrap();
        assert!(credentials::begin_cleanup(&tx, &uuid::Uuid::new_v4().to_string(), id).is_err());
        assert!(credentials::claim(&tx, id, |_, _, _| panic!(
            "restored work must never reach vault binding callback"
        ))
        .unwrap()
        .is_none());
    }
    let tx = f.db.transaction().unwrap();
    credentials::recover(&tx).unwrap();
    tx.commit().unwrap();
    assert!(
        !credentials::operation(&f.db, &f.credential_unknown)
            .unwrap()
            .can_cleanup
    );
}
#[test]
fn caller_rollback_restores_all_tables_and_revocation_is_idempotent() {
    let mut f = fixture();
    let c = configuration::current(&f.db).unwrap().unwrap();
    let b = persistence::binding(&f.db, &f.binding).unwrap();
    let tx = f.db.transaction().unwrap();
    invalidate(&tx).unwrap();
    tx.rollback().unwrap();
    assert_eq!(
        configuration::current(&f.db).unwrap().unwrap().revision,
        c.revision
    );
    assert!(configuration::current(&f.db).unwrap().unwrap().enabled);
    assert_eq!(persistence::binding(&f.db, &f.binding).unwrap(), b);
    assert_eq!(
        persistence::outbox(&f.db, &f.sending).unwrap().status,
        "sending"
    );
    assert_eq!(
        persistence::inbox(&f.db, &f.input).unwrap().status,
        "pending"
    );
    for _ in 0..2 {
        let tx = f.db.transaction().unwrap();
        invalidate(&tx).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(persistence::outbox(&f.db, &f.sent).unwrap().status, "sent");
    assert_eq!(
        persistence::outbox(&f.db, &f.unknown).unwrap().status,
        "unknown"
    );
}
#[test]
fn malformed_or_mismatched_metadata_fails_closed_and_outer_rollback_is_complete() {
    for table in [
        "qq_bindings",
        "qq_reply_routes",
        "qq_inbox",
        "qq_credential_operations",
    ] {
        let mut f = fixture();
        f.db.execute(&format!("UPDATE {table} SET body='{{}}'"), [])
            .unwrap();
        let before =
            f.db.query_row("SELECT enabled FROM qq_configuration", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap();
        let tx = f.db.transaction().unwrap();
        assert!(invalidate(&tx).is_err(), "{table}");
        tx.rollback().unwrap();
        assert_eq!(
            f.db.query_row("SELECT enabled FROM qq_configuration", [], |r| r
                .get::<_, bool>(0))
                .unwrap(),
            before
        );
        assert_eq!(
            persistence::outbox(&f.db, &f.sending).unwrap().status,
            "sending"
        );
    }
    let mut f = fixture();
    f.db.execute("UPDATE qq_bindings SET revision=revision+1", [])
        .unwrap();
    let tx = f.db.transaction().unwrap();
    assert!(invalidate(&tx).is_err());
}
#[test]
fn saturated_revisions_never_wrap_and_still_revoke() {
    let mut f = fixture();
    let mut b = persistence::binding(&f.db, &f.binding).unwrap();
    b.revision = i64::MAX as u64;
    f.db.execute(
        "UPDATE qq_bindings SET revision=?1,body=?2 WHERE principal_id=?3",
        params![
            b.revision,
            serde_json::to_string(&b).unwrap(),
            b.principal_id
        ],
    )
    .unwrap();
    f.db.execute("UPDATE qq_configuration SET revision=?1", [i64::MAX])
        .unwrap();
    let tx = f.db.transaction().unwrap();
    invalidate(&tx).unwrap();
    tx.commit().unwrap();
    let c = configuration::current(&f.db).unwrap().unwrap();
    assert_eq!(c.id, f.config);
    assert_eq!(c.revision, i64::MAX as u64);
    assert!(!c.enabled);
    let b = persistence::binding(&f.db, &f.binding).unwrap();
    assert_eq!(b.revision, i64::MAX as u64);
    assert_eq!(b.status, "revoked");
}

#[test]
fn restored_inflight_cleanup_never_reenables_its_original_target() {
    let mut f = fixture();
    let tx = f.db.transaction().unwrap();
    credentials::claim(&tx, &f.credential_pending, |_, _, _| Ok(())).unwrap();
    credentials::finish(
        &tx,
        &f.credential_pending,
        Outcome::NotApplied,
        |_, _, _| Ok(()),
    )
    .unwrap();
    let cleanup = uuid::Uuid::new_v4().to_string();
    credentials::begin_cleanup(&tx, &cleanup, &f.credential_unknown).unwrap();
    credentials::claim(&tx, &cleanup, |_, _, _| Ok(())).unwrap();
    tx.commit().unwrap();
    let tx = f.db.transaction().unwrap();
    invalidate(&tx).unwrap();
    tx.commit().unwrap();
    let original = credentials::operation(&f.db, &f.credential_unknown).unwrap();
    assert_eq!(original.status, "unknown");
    assert!(!original.can_cleanup);
    assert_eq!(original.cleanup_status.as_deref(), Some("unknown"));
    assert_eq!(
        credentials::operation(&f.db, &cleanup).unwrap().status,
        "unknown"
    );
    let tx = f.db.transaction().unwrap();
    credentials::recover(&tx).unwrap();
    assert!(
        !credentials::operation(&tx, &f.credential_unknown)
            .unwrap()
            .can_cleanup
    );
    assert!(
        !credentials::finish(&tx, &cleanup, Outcome::Succeeded, |_, _, _| panic!(
            "no vault"
        ))
        .unwrap()
    );
}
