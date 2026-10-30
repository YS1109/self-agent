use super::*;
use rusqlite::TransactionBehavior;
use std::{
    sync::{Arc, Barrier},
    time::Duration,
};

const NOW: u64 = 1_800_000_000_000;

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    migrate(&db).unwrap();
    db
}
fn identity(kind: ConversationKind) -> Identity {
    Identity {
        app_id: "app_1".into(),
        sender_open_id: "sender_1".into(),
        kind,
        target_id: if kind == ConversationKind::User {
            "sender_1"
        } else {
            "group_1"
        }
        .into(),
    }
}
fn active(db: &mut Connection, identity: &Identity) -> Binding {
    let tx = db.transaction().unwrap();
    let ticket = issue_pairing(&tx, &identity.app_id, NOW, 60).unwrap();
    let pending = redeem_pairing(&tx, identity, &ticket.code, NOW).unwrap();
    let active = confirm_binding(
        &tx,
        &pending.principal_id,
        pending.revision,
        identity,
        json!({"session_ids":["session_1"],"permissions":{"view":true}}),
        NOW + 3_600_000,
        NOW - 31_000,
    )
    .unwrap();
    tx.commit().unwrap();
    active
}
fn input(identity: &Identity, id: &str) -> Inbound {
    Inbound {
        identity: identity.clone(),
        message_id: id.into(),
        event_aliases: vec![format!("alias_{id}")],
        canonical_input: json!({"kind":"command","content":"/会话"}),
        received_at_ms: NOW,
    }
}
fn accepted(db: &mut Connection, identity: &Identity, id: &str) -> AcceptedInput {
    let tx = db.transaction().unwrap();
    let accepted = accept_input(&tx, &input(identity, id), NOW).unwrap();
    tx.commit().unwrap();
    accepted
}
fn enqueued(db: &mut Connection, route: &str, key: &str) -> OutboxRecord {
    let tx = db.transaction().unwrap();
    let result = enqueue_outbox(&tx, route, key, OutboxKind::Completion, "安全结果", NOW).unwrap();
    tx.commit().unwrap();
    result
}
fn claimed(db: &mut Connection, time: u64) -> OutboxClaim {
    let tx = db.transaction().unwrap();
    let result = claim_outbox(&tx, time).unwrap().unwrap();
    tx.commit().unwrap();
    result
}
fn finalize(
    db: &mut Connection,
    claim: &OutboxClaim,
    outcome: DeliveryOutcome,
    time: u64,
) -> OutboxRecord {
    let tx = db.transaction().unwrap();
    let result = finalize_outbox(
        &tx,
        &claim.record.id,
        claim.record.claim_nonce.as_deref().unwrap(),
        outcome,
        time,
    )
    .unwrap();
    tx.commit().unwrap();
    result
}

#[test]
fn pairing_persists_only_digest_and_requires_exact_owner_confirmation() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let tx = db.transaction().unwrap();
    let ticket = issue_pairing(&tx, &who.app_id, NOW, 60).unwrap();
    let pending = redeem_pairing(&tx, &who, &ticket.code, NOW).unwrap();
    assert_eq!(pending.status, "pending");
    assert!(live_binding(&tx, &who, NOW).unwrap().is_none());
    let mut different = who.clone();
    different.app_id = "app_2".into();
    assert!(confirm_binding(
        &tx,
        &pending.principal_id,
        0,
        &different,
        json!({}),
        NOW + 1000,
        NOW
    )
    .is_err());
    assert!(confirm_binding(
        &tx,
        &pending.principal_id,
        1,
        &who,
        json!({}),
        NOW + 1000,
        NOW
    )
    .is_err());
    assert!(redeem_pairing(&tx, &who, &ticket.code, NOW).is_err());
    let confirmed = confirm_binding(
        &tx,
        &pending.principal_id,
        0,
        &who,
        json!({"session_ids":["session_1"]}),
        NOW + 60_000,
        NOW,
    )
    .unwrap();
    assert_eq!(confirmed.revision, 1);
    assert!(live_binding(&tx, &who, NOW).unwrap().is_some());
    assert!(live_binding(&tx, &who, NOW + 60_000).unwrap().is_none());
    let raw: String = tx
        .query_row("SELECT code_hash FROM qq_pairings", [], |row| row.get(0))
        .unwrap();
    assert!(!raw.contains(&ticket.code));
    let public = public_snapshot(&tx, 20).unwrap().to_string();
    assert!(!public.contains(&ticket.code));
    assert!(!public.contains(&raw));
    tx.commit().unwrap();
}

#[test]
fn pairing_expiry_context_and_group_member_are_not_privileges() {
    let mut db = database();
    let who = identity(ConversationKind::Group);
    let tx = db.transaction().unwrap();
    let ticket = issue_pairing(&tx, &who.app_id, NOW, 1).unwrap();
    assert!(redeem_pairing(&tx, &who, &ticket.code, NOW + 1000).is_err());
    assert!(issue_pairing(&tx, &who.app_id, NOW, 601).is_err());
    let ticket = issue_pairing(&tx, &who.app_id, NOW, 60).unwrap();
    let pending = redeem_pairing(&tx, &who, &ticket.code, NOW).unwrap();
    let mut other = who.clone();
    other.sender_open_id = "administrator".into();
    assert!(confirm_binding(
        &tx,
        &pending.principal_id,
        0,
        &other,
        json!({}),
        NOW + 3000,
        NOW
    )
    .is_err());
    assert!(live_binding(&tx, &other, NOW).unwrap().is_none());
    tx.commit().unwrap();
}

#[test]
fn pairing_input_is_restartable_idempotent_and_never_has_a_reply_route() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let tx = db.transaction().unwrap();
    let ticket = issue_pairing(&tx, &who.app_id, NOW, 60).unwrap();
    let mut message = input(&who, "pair-message");
    message.canonical_input = pairing_canonical(&who.app_id, &ticket.code).unwrap();
    let first = accept_input(&tx, &message, NOW).unwrap();
    assert!(first.route_id.is_none());
    tx.commit().unwrap();
    assert_eq!(pending_inputs(&db, 10).unwrap().len(), 1);
    let tx = db.transaction().unwrap();
    let binding = redeem_pairing_input(&tx, &first.inbox_id, NOW + 1).unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    let replay = accept_input(&tx, &message, NOW + 2).unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.command_id, first.command_id);
    assert_eq!(
        redeem_pairing_input(&tx, &first.inbox_id, NOW + 2).unwrap(),
        binding
    );
    let body: String = tx
        .query_row("SELECT body FROM qq_inbox", [], |row| row.get(0))
        .unwrap();
    assert!(!body.contains(&ticket.code));
    assert!(guard_input(&tx, &first.inbox_id, NOW + 2).is_err());
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM qq_outbox", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    tx.commit().unwrap();
}

#[test]
fn inbox_hash_ignores_delivery_aliases_but_binds_identity_content_and_platform_time() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let mut message = input(&who, "msg_1");
    let tx = db.transaction().unwrap();
    let first = accept_input(&tx, &message, NOW).unwrap();
    tx.commit().unwrap();
    message.event_aliases = vec!["new_alias".into()];
    let tx = db.transaction().unwrap();
    let duplicate = accept_input(&tx, &message, NOW + 1_000_000).unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.route_id, first.route_id);
    assert_eq!(duplicate.command_id, first.command_id);
    message.canonical_input["content"] = json!("/执行 other");
    assert!(accept_input(&tx, &message, NOW + 1).is_err());
    message.canonical_input["content"] = json!("/会话");
    message.received_at_ms += 1;
    assert!(accept_input(&tx, &message, NOW + 1).is_err());
    message.received_at_ms = NOW;
    message.identity.sender_open_id = "other".into();
    message.identity.target_id = "other".into();
    assert!(accept_input(&tx, &message, NOW + 1).is_err());
    tx.commit().unwrap();
}

#[test]
fn alias_conflict_and_transaction_rollback_leave_no_partial_input_or_route() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let first = accepted(&mut db, &who, "msg_1");
    {
        let tx = db.transaction().unwrap();
        let mut second = input(&who, "msg_2");
        second.event_aliases = vec!["alias_msg_1".into()];
        assert!(accept_input(&tx, &second, NOW).is_err());
    }
    assert_eq!(pending_inputs(&db, 10).unwrap().len(), 1);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM qq_reply_routes", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    {
        let tx = db.transaction().unwrap();
        mark_handled(
            &tx,
            &first.inbox_id,
            &first.command_id,
            json!({"task_id":"task_1"}),
            NOW,
        )
        .unwrap();
    }
    assert_eq!(pending_inputs(&db, 10).unwrap().len(), 1);
    let tx = db.transaction().unwrap();
    mark_handled(
        &tx,
        &first.inbox_id,
        &first.command_id,
        json!({"task_id":"task_1"}),
        NOW,
    )
    .unwrap();
    mark_handled(
        &tx,
        &first.inbox_id,
        &first.command_id,
        json!({"task_id":"task_1"}),
        NOW,
    )
    .unwrap();
    assert!(mark_handled(
        &tx,
        &first.inbox_id,
        &first.command_id,
        json!({"task_id":"task_2"}),
        NOW
    )
    .is_err());
    tx.commit().unwrap();
    assert!(pending_inputs(&db, 10).unwrap().is_empty());
}

fn confirm_at(db: &mut Connection, who: &Identity, at: u64) -> Binding {
    let tx = db.transaction().unwrap();
    let code = issue_pairing(&tx, &who.app_id, at, 60).unwrap();
    let pending = redeem_pairing(&tx, who, &code.code, at).unwrap();
    let result = confirm_binding(
        &tx,
        &pending.principal_id,
        pending.revision,
        who,
        json!({"session_ids":["session_1"]}),
        at + 3_600_000,
        at,
    )
    .unwrap();
    tx.commit().unwrap();
    result
}
fn admit(db: &mut Connection, value: &Inbound, now: u64) -> Option<AcceptedInput> {
    let tx = db.transaction().unwrap();
    let result = accept_input_with_policy(&tx, value, now, &ReplyPolicy::default()).unwrap();
    tx.commit().unwrap();
    result
}
#[test]
fn unbound_future_input_and_exact_activation_boundary_never_acquire_authority() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let mut old = input(&who, "preauth-future");
    old.received_at_ms = NOW + PLATFORM_CLOCK_SKEW_MS;
    assert!(admit(&mut db, &old, NOW).is_none());
    let binding = confirm_at(&mut db, &who, NOW + 1);
    assert_eq!(binding.input_after_ms, NOW + 1 + PLATFORM_CLOCK_SKEW_MS);
    assert!(admit(&mut db, &old, NOW + 1).is_none());
    let mut edge = input(&who, "equal-boundary");
    edge.received_at_ms = binding.input_after_ms;
    assert!(admit(&mut db, &edge, edge.received_at_ms).is_none());
    let mut fresh = input(&who, "fresh-after-activation");
    fresh.received_at_ms = binding.input_after_ms + 1;
    let accepted = admit(&mut db, &fresh, fresh.received_at_ms).unwrap();
    assert!(accepted.route_id.is_some());
    let tx = db.transaction().unwrap();
    startup_recover(&tx, fresh.received_at_ms).unwrap();
    tx.commit().unwrap();
    assert!(admit(&mut db, &old, fresh.received_at_ms).is_none());
    assert!(
        admit(&mut db, &fresh, fresh.received_at_ms)
            .unwrap()
            .duplicate
    );
}

#[test]
fn routes_keep_original_sender_target_revision_and_selection_is_separate() {
    let mut db = database();
    let who = identity(ConversationKind::Group);
    let principal = active(&mut db, &who);
    let one = accepted(&mut db, &who, "one");
    let two = accepted(&mut db, &who, "two");
    let r1 = route(&db, one.route_id.as_deref().unwrap()).unwrap();
    let r2 = route(&db, two.route_id.as_deref().unwrap()).unwrap();
    assert_ne!(r1.route_id, r2.route_id);
    assert_eq!(r1.identity, who);
    assert_eq!(r1.binding_revision, principal.revision);
    assert_eq!(r1.message_id, "one");
    assert_ne!(selection_key(&principal).unwrap(), r1.route_id);
    let mut other = who.clone();
    other.sender_open_id = "member_2".into();
    assert!(live_binding(&db, &other, NOW).unwrap().is_none());
    let tx = db.transaction().unwrap();
    grant_session(&tx, &principal.principal_id, "session_2", NOW).unwrap();
    assert!(allowed_session(&tx, &principal.principal_id, "session_1", NOW).unwrap());
    assert!(allowed_session(&tx, &principal.principal_id, "session_2", NOW).unwrap());
    assert!(!allowed_session(&tx, &principal.principal_id, "session_3", NOW).unwrap());
    revoke_binding(&tx, &principal.principal_id, principal.revision, NOW + 1).unwrap();
    assert!(!allowed_session(&tx, &principal.principal_id, "session_2", NOW + 1).unwrap());
    assert!(guard_route(&tx, &r1.route_id, NOW + 1).is_err());
    tx.commit().unwrap();
}

#[test]
fn logical_reply_idempotency_and_passive_quotas_allocate_sequences_once() {
    for (kind, limit) in [(ConversationKind::User, 4), (ConversationKind::Group, 5)] {
        let mut db = database();
        let who = identity(kind);
        active(&mut db, &who);
        let accepted = accepted(&mut db, &who, "message");
        let route = accepted.route_id.unwrap();
        let first = enqueued(&mut db, &route, "completion-1");
        let tx = db.transaction().unwrap();
        let duplicate = enqueue_outbox(
            &tx,
            &route,
            "completion-1",
            OutboxKind::Completion,
            "安全结果",
            NOW + 1,
        )
        .unwrap();
        assert_eq!(first.id, duplicate.id);
        assert_eq!(duplicate.msg_seq, 1);
        assert!(enqueue_outbox(
            &tx,
            &route,
            "completion-1",
            OutboxKind::Completion,
            "changed",
            NOW
        )
        .is_err());
        tx.commit().unwrap();
        for seq in 2..=limit {
            assert_eq!(
                enqueued(&mut db, &route, &format!("completion-{seq}")).msg_seq,
                seq
            );
        }
        let tx = db.transaction().unwrap();
        assert!(enqueue_outbox(
            &tx,
            &route,
            "too-many",
            OutboxKind::Completion,
            "too many",
            NOW
        )
        .is_err());
        tx.commit().unwrap();
    }
}

#[test]
fn only_explicit_429_retries_same_sequence_and_stale_claim_cannot_finalize() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    enqueued(&mut db, &route, "one");
    let first = claimed(&mut db, NOW);
    let pending = finalize(
        &mut db,
        &first,
        DeliveryOutcome::RateLimited {
            retry_at_ms: NOW + 1000,
        },
        NOW,
    );
    assert_eq!(pending.status, "pending");
    let tx = db.transaction().unwrap();
    assert!(claim_outbox(&tx, NOW + 999).unwrap().is_none());
    tx.commit().unwrap();
    let second = claimed(&mut db, NOW + 1000);
    assert_eq!(first.record.msg_seq, second.record.msg_seq);
    assert_ne!(first.record.claim_nonce, second.record.claim_nonce);
    let tx = db.transaction().unwrap();
    assert!(finalize_outbox(
        &tx,
        &first.record.id,
        first.record.claim_nonce.as_deref().unwrap(),
        DeliveryOutcome::Sent {
            platform_id: "stale".into()
        },
        NOW + 1001
    )
    .is_err());
    tx.commit().unwrap();
    let pending = finalize(
        &mut db,
        &second,
        DeliveryOutcome::RateLimited {
            retry_at_ms: NOW + 2000,
        },
        NOW + 1000,
    );
    assert_eq!(pending.status, "pending");
    let third = claimed(&mut db, NOW + 2000);
    let failed = finalize(
        &mut db,
        &third,
        DeliveryOutcome::RateLimited {
            retry_at_ms: NOW + 3000,
        },
        NOW + 2000,
    );
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.reason.as_deref(), Some("rate_limit_exhausted"));
}

#[test]
fn timeout_and_restart_sending_are_unknown_and_never_automatically_retried() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    let one = enqueued(&mut db, &route, "one");
    let first = claimed(&mut db, NOW);
    assert_eq!(
        finalize(&mut db, &first, DeliveryOutcome::Unknown, NOW).status,
        "unknown"
    );
    let two = enqueued(&mut db, &route, "two");
    let second = claimed(&mut db, NOW);
    let tx = db.transaction().unwrap();
    startup_recover(&tx, NOW + 1).unwrap();
    assert!(claim_outbox(&tx, NOW + 1).unwrap().is_none());
    tx.commit().unwrap();
    assert_eq!(outbox(&db, &one.id).unwrap().status, "unknown");
    assert_eq!(outbox(&db, &two.id).unwrap().status, "unknown");
    let tx = db.transaction().unwrap();
    assert!(finalize_outbox(
        &tx,
        &second.record.id,
        second.record.claim_nonce.as_deref().unwrap(),
        DeliveryOutcome::Sent {
            platform_id: "late".into()
        },
        NOW + 1
    )
    .is_err());
    tx.commit().unwrap();
}

#[test]
fn send_success_requires_platform_id_and_failed_finalize_cannot_resend() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    let item = enqueued(&mut db, &route, "one");
    let claim = claimed(&mut db, NOW);
    {
        let tx = db.transaction().unwrap();
        assert!(finalize_outbox(
            &tx,
            &item.id,
            claim.record.claim_nonce.as_deref().unwrap(),
            DeliveryOutcome::Sent {
                platform_id: String::new()
            },
            NOW
        )
        .is_err());
    }
    assert_eq!(outbox(&db, &item.id).unwrap().status, "sending");
    db.execute_batch("CREATE TRIGGER reject_sent BEFORE UPDATE ON qq_outbox WHEN NEW.status='sent' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(finalize_outbox(
            &tx,
            &item.id,
            claim.record.claim_nonce.as_deref().unwrap(),
            DeliveryOutcome::Sent {
                platform_id: "platform-id".into()
            },
            NOW
        )
        .is_err());
    }
    assert_eq!(outbox(&db, &item.id).unwrap().status, "sending");
    let tx = db.transaction().unwrap();
    assert!(claim_outbox(&tx, NOW + 1).unwrap().is_none());
    startup_recover(&tx, NOW + 1).unwrap();
    tx.commit().unwrap();
    assert_eq!(outbox(&db, &item.id).unwrap().status, "unknown");
}

#[test]
fn revoke_blocks_unsent_and_rechecks_claim_but_cannot_recall_inflight_success() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let principal = active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    let inflight = enqueued(&mut db, &route, "inflight");
    let pending = enqueued(&mut db, &route, "pending");
    let claim = claimed(&mut db, NOW);
    let tx = db.transaction().unwrap();
    assert!(authorize_send(
        &tx,
        &claim.record.id,
        claim.record.claim_nonce.as_deref().unwrap(),
        NOW
    )
    .unwrap());
    revoke_binding(&tx, &principal.principal_id, principal.revision, NOW + 1).unwrap();
    tx.commit().unwrap();
    assert_eq!(outbox(&db, &pending.id).unwrap().status, "revoked");
    assert_eq!(
        finalize(
            &mut db,
            &claim,
            DeliveryOutcome::Sent {
                platform_id: "sent-before-revoke".into()
            },
            NOW + 2
        )
        .status,
        "sent"
    );
    assert_eq!(
        outbox(&db, &inflight.id).unwrap().platform_id.as_deref(),
        Some("sent-before-revoke")
    );
    let tx = db.transaction().unwrap();
    assert!(claim_outbox(&tx, NOW + 2).unwrap().is_none());
    tx.commit().unwrap();
}

#[test]
fn revoke_after_claim_before_send_and_expiry_do_not_send_or_fallback() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let principal = active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    enqueued(&mut db, &route, "one");
    let claim = claimed(&mut db, NOW);
    let tx = db.transaction().unwrap();
    revoke_binding(&tx, &principal.principal_id, principal.revision, NOW + 1).unwrap();
    assert!(!authorize_send(
        &tx,
        &claim.record.id,
        claim.record.claim_nonce.as_deref().unwrap(),
        NOW + 1
    )
    .unwrap());
    tx.commit().unwrap();
    assert_eq!(outbox(&db, &claim.record.id).unwrap().status, "revoked");
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    let pending = enqueued(&mut db, &route, "one");
    let tx = db.transaction().unwrap();
    assert!(claim_outbox(&tx, NOW + PASSIVE_WINDOW_MS)
        .unwrap()
        .is_none());
    tx.commit().unwrap();
    assert_eq!(outbox(&db, &pending.id).unwrap().status, "expired");
}

#[test]
fn explicit_platform_rejection_is_terminal_and_429_cannot_outlive_route() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    enqueued(&mut db, &route, "one");
    let claim = claimed(&mut db, NOW);
    assert_eq!(
        finalize(
            &mut db,
            &claim,
            DeliveryOutcome::Failed {
                reason: FailureReason::PlatformRejected
            },
            NOW
        )
        .status,
        "failed"
    );
    enqueued(&mut db, &route, "two");
    let claim = claimed(&mut db, NOW);
    assert_eq!(
        finalize(
            &mut db,
            &claim,
            DeliveryOutcome::RateLimited {
                retry_at_ms: NOW + PASSIVE_WINDOW_MS
            },
            NOW
        )
        .status,
        "expired"
    );
}

#[test]
fn re_pairing_changes_revision_and_clears_inherited_created_session_acl() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let old = active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    let tx = db.transaction().unwrap();
    grant_session(&tx, &old.principal_id, "created_session", NOW).unwrap();
    revoke_binding(&tx, &old.principal_id, old.revision, NOW + 1).unwrap();
    let ticket = issue_pairing(&tx, &who.app_id, NOW + 2, 60).unwrap();
    let pending = redeem_pairing(&tx, &who, &ticket.code, NOW + 2).unwrap();
    let new = confirm_binding(
        &tx,
        &pending.principal_id,
        pending.revision,
        &who,
        json!({"session_ids":["different_session"]}),
        NOW + 3_600_000,
        NOW + 2,
    )
    .unwrap();
    assert_eq!(old.principal_id, new.principal_id);
    assert!(new.revision > old.revision);
    assert!(!allowed_session(&tx, &new.principal_id, "created_session", NOW + 2).unwrap());
    assert!(guard_route(&tx, &route, NOW + 2).is_err());
    tx.commit().unwrap();
}

#[test]
fn raw_pairing_payloads_oversized_inputs_and_future_timestamps_are_rejected() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let tx = db.transaction().unwrap();
    let mut message = input(&who, "one");
    message.canonical_input =
        json!({"kind":"pairing","code_hash":"a".repeat(64),"code":"raw-code"});
    assert!(accept_input(&tx, &message, NOW).is_err());
    message.canonical_input = json!({"content":"x".repeat(MAX_INPUT_BYTES)});
    assert!(accept_input(&tx, &message, NOW).is_err());
    message.canonical_input = json!({"kind":"command","content":"/会话"});
    message.received_at_ms = NOW + 30_001;
    assert!(accept_input(&tx, &message, NOW).is_err());
    tx.commit().unwrap();
}

#[test]
fn concurrent_one_time_pairing_claim_has_exactly_one_winner() {
    let directory = std::env::temp_dir().join(format!("qq-persistence-{}", new_id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("test.sqlite");
    let mut db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    migrate(&db).unwrap();
    let tx = db.transaction().unwrap();
    let ticket = issue_pairing(&tx, "app_1", NOW, 60).unwrap();
    tx.commit().unwrap();
    drop(db);
    let barrier = Arc::new(Barrier::new(2));
    let mut joins = Vec::new();
    for sender in ["sender_1", "sender_2"] {
        let path = path.clone();
        let code = ticket.code.clone();
        let barrier = barrier.clone();
        joins.push(std::thread::spawn(move || {
            let mut db = Connection::open(path).unwrap();
            db.busy_timeout(Duration::from_secs(5)).unwrap();
            barrier.wait();
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let who = Identity {
                app_id: "app_1".into(),
                sender_open_id: sender.into(),
                kind: ConversationKind::User,
                target_id: sender.into(),
            };
            let won = redeem_pairing(&tx, &who, &code, NOW).is_ok();
            if won {
                tx.commit().unwrap();
            }
            won
        }));
    }
    assert_eq!(
        joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .filter(|won| *won)
            .count(),
        1
    );
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM qq_bindings", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    drop(db);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn reply_payload_byte_limit_and_enqueue_rollback_preserve_sequence() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let route = accepted(&mut db, &who, "one").route_id.unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(enqueue_outbox(
            &tx,
            &route,
            "too-long",
            OutboxKind::Ack,
            &"界".repeat(1334),
            NOW
        )
        .is_err());
    }
    db.execute_batch("CREATE TRIGGER reject_outbox BEFORE INSERT ON qq_outbox BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(enqueue_outbox(&tx, &route, "one", OutboxKind::Ack, "safe", NOW).is_err());
    }
    assert_eq!(
        db.query_row(
            "SELECT next_msg_seq FROM qq_reply_routes WHERE route_id=?1",
            [&route],
            |row| row.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    db.execute_batch("DROP TRIGGER reject_outbox;").unwrap();
    let tx = db.transaction().unwrap();
    let item = enqueue_outbox(&tx, &route, "one", OutboxKind::Ack, &"x".repeat(4000), NOW).unwrap();
    assert_eq!(item.msg_seq, 1);
    assert_eq!(item.kind, OutboxKind::Ack);
    assert_eq!(item.payload.len(), 4000);
    tx.commit().unwrap();
    let public = public_snapshot(&db, 10).unwrap().to_string();
    assert!(!public.contains(&"x".repeat(100)));
    assert!(!public.contains("claim_nonce"));
}

#[test]
fn per_route_policy_is_snapshotted_can_only_tighten_and_duplicate_cannot_widen() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let principal = active(&mut db, &who);
    let tx = db.transaction().unwrap();
    let message = input(&who, "one");
    let policy = ReplyPolicy {
        window_ms: 1000,
        max_user: 2,
        max_group: 3,
    };
    let first = accept_input_with_policy(&tx, &message, NOW, &policy)
        .unwrap()
        .unwrap();
    let original = route(&tx, first.route_id.as_deref().unwrap()).unwrap();
    assert_eq!(original.expires_at_ms, NOW + 1000);
    assert_eq!(original.max_messages, 2);
    let duplicate = accept_input(&tx, &message, NOW + 500).unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(
        route(&tx, duplicate.route_id.as_deref().unwrap())
            .unwrap()
            .max_messages,
        2
    );
    assert!(accept_input_with_policy(
        &tx,
        &input(&who, "invalid"),
        NOW,
        &ReplyPolicy {
            window_ms: 300001,
            max_user: 4,
            max_group: 5
        }
    )
    .is_err());
    assert!(accept_input_with_policy(
        &tx,
        &input(&who, "invalid"),
        NOW,
        &ReplyPolicy {
            window_ms: 300000,
            max_user: 5,
            max_group: 5
        }
    )
    .is_err());
    let selection = selection_key(&principal).unwrap();
    let revoked =
        revoke_binding(&tx, &principal.principal_id, principal.revision, NOW + 1).unwrap();
    assert_ne!(selection, selection_key(&revoked).unwrap());
    tx.commit().unwrap();
}

#[test]
fn global_logical_reply_identity_cannot_be_redirected_to_a_new_message_route() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let original = accepted(&mut db, &who, "original").route_id.unwrap();
    let later = accepted(&mut db, &who, "later").route_id.unwrap();
    let first = enqueued(&mut db, &original, "completion:task_1");
    let tx = db.transaction().unwrap();
    assert!(enqueue_outbox(
        &tx,
        &later,
        "completion:task_1",
        OutboxKind::Completion,
        "安全结果",
        NOW
    )
    .is_err());
    let same = enqueue_outbox(
        &tx,
        &original,
        "completion:task_1",
        OutboxKind::Completion,
        "安全结果",
        NOW,
    )
    .unwrap();
    assert_eq!(same.id, first.id);
    assert_eq!(same.route_id, original);
    assert_eq!(
        tx.query_row(
            "SELECT next_msg_seq FROM qq_reply_routes WHERE route_id=?1",
            [&later],
            |row| row.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM qq_outbox", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    tx.commit().unwrap();
}

#[test]
fn unpaired_flood_is_constant_size_and_does_not_consume_authorized_or_pairing_capacity() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let original = accepted(&mut db, &who, "accepted-before-flood");
    let pending = enqueued(
        &mut db,
        original.route_id.as_deref().unwrap(),
        "keep-unknown",
    );
    let claim = claimed(&mut db, NOW);
    let tx = db.transaction().unwrap();
    finalize_outbox(
        &tx,
        &claim.record.id,
        claim.record.claim_nonce.as_deref().unwrap(),
        DeliveryOutcome::Unknown,
        NOW,
    )
    .unwrap();
    for n in 0..12_000 {
        let outsider = Identity {
            sender_open_id: format!("stranger_{n}"),
            target_id: format!("stranger_{n}"),
            ..who.clone()
        };
        let mut value = input(&outsider, &format!("spam_{n}"));
        assert!(
            accept_input_with_policy(&tx, &value, NOW, &ReplyPolicy::default())
                .unwrap()
                .is_none()
        );
        value.message_id.push_str("-pair");
        value.canonical_input = json!({"kind":"pairing","code_hash":"a".repeat(64)});
        assert!(
            accept_input_with_policy(&tx, &value, NOW, &ReplyPolicy::default())
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        tx.query_row("SELECT count FROM qq_ignored_inputs", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        24_000
    );
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM qq_inbox", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM qq_event_aliases", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert_eq!(outbox(&tx, &pending.id).unwrap().status, "unknown");
    assert_eq!(
        tx.query_row("SELECT clock_fault_ms FROM qq_ignored_inputs", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert!(accept_input(&tx, &input(&who, "authorized-after-flood"), NOW).is_ok());
    let code = issue_pairing(&tx, &who.app_id, NOW, 60).unwrap();
    let mut pair = input(&who, "valid-pair-input");
    pair.canonical_input = pairing_canonical(&who.app_id, &code.code).unwrap();
    let first = accept_input_with_policy(&tx, &pair, NOW, &ReplyPolicy::default())
        .unwrap()
        .unwrap();
    pair.message_id = "second-valid-code-message".into();
    pair.event_aliases.clear();
    assert!(
        accept_input_with_policy(&tx, &pair, NOW, &ReplyPolicy::default())
            .unwrap()
            .is_none()
    );
    pair.message_id = "valid-pair-input".into();
    pair.event_aliases = vec!["alias_valid-pair-input".into()];
    assert_eq!(
        accept_input(&tx, &pair, NOW).unwrap().inbox_id,
        first.inbox_id
    );
    tx.commit().unwrap();
}

#[test]
fn ignored_watermark_survives_restart_and_rebind_clock_rollback() {
    let path = std::env::temp_dir().join(format!("qq-ingress-{}.sqlite", new_id()));
    let who = identity(ConversationKind::User);
    let mut ignored = input(&who, "old-clock-ahead");
    ignored.received_at_ms = NOW + 30_000;
    {
        let mut db = Connection::open(&path).unwrap();
        migrate(&db).unwrap();
        assert!(admit(&mut db, &ignored, NOW).is_none());
        let tx = db.transaction().unwrap();
        record_clock_skew(&tx, ignored.received_at_ms, NOW - 60_000).unwrap();
        tx.commit().unwrap();
    }
    let mut db = Connection::open(&path).unwrap();
    migrate(&db).unwrap();
    assert_eq!(
        db.query_row("SELECT clock_fault_ms FROM qq_ignored_inputs", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        ignored.received_at_ms
    );
    let first = confirm_at(&mut db, &who, NOW - 60_000);
    assert_eq!(first.input_after_ms, ignored.received_at_ms);
    assert!(admit(&mut db, &ignored, NOW + 30_000).is_none());
    let tx = db.transaction().unwrap();
    revoke_binding(&tx, &first.principal_id, first.revision, NOW + 30_001).unwrap();
    tx.commit().unwrap();
    let mut before_rebind = input(&who, "old-during-revocation");
    before_rebind.received_at_ms = NOW + 60_002;
    assert!(admit(&mut db, &before_rebind, NOW + 30_002).is_none());
    let rebound = confirm_at(&mut db, &who, NOW);
    assert_eq!(rebound.input_after_ms, before_rebind.received_at_ms);
    assert!(admit(&mut db, &before_rebind, NOW + 60_002).is_none());
    let mut fresh = input(&who, "new-after-rebind");
    fresh.received_at_ms = rebound.input_after_ms + 1;
    assert!(admit(&mut db, &fresh, fresh.received_at_ms).is_some());
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn ignored_watermark_rolls_back_with_failed_checkpoint_transaction() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let mut value = input(&who, "rolled-back");
    value.received_at_ms = NOW + 30_000;
    {
        let tx = db.transaction().unwrap();
        assert!(
            accept_input_with_policy(&tx, &value, NOW, &ReplyPolicy::default())
                .unwrap()
                .is_none()
        );
        tx.rollback().unwrap();
    }
    let binding = confirm_at(&mut db, &who, NOW - 60_000);
    assert_eq!(binding.input_after_ms, NOW - 30_000);
}

#[test]
fn bounded_pending_inbox_stops_new_messages_without_losing_dedup() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    active(&mut db, &who);
    let tx = db.transaction().unwrap();
    for n in 0..MAX_PENDING_INPUTS {
        accept_input(&tx, &input(&who, &format!("bounded-{n}")), NOW).unwrap();
    }
    assert!(accept_input(&tx, &input(&who, "bounded-extra"), NOW).is_err());
    assert!(
        accept_input(&tx, &input(&who, "bounded-0"), NOW)
            .unwrap()
            .duplicate
    );
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM qq_inbox", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        MAX_PENDING_INPUTS
    );
    tx.commit().unwrap();
}

#[test]
fn created_sessions_cannot_exceed_total_scope_limit() {
    let mut db = database();
    let who = identity(ConversationKind::User);
    let principal = active(&mut db, &who);
    let tx = db.transaction().unwrap();
    for n in 0..31 {
        grant_session(&tx, &principal.principal_id, &format!("created-{n}"), NOW).unwrap();
    }
    assert!(grant_session(&tx, &principal.principal_id, "overflow", NOW).is_err());
    grant_session(&tx, &principal.principal_id, "created-0", NOW).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM qq_session_acl", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        31
    );
    tx.commit().unwrap();
}

#[test]
fn development_ignored_watermark_migration_preserves_old_replay_barrier() {
    let db = database();
    db.execute_batch("DROP TABLE qq_ignored_inputs; CREATE TABLE qq_ignored_inputs(singleton INTEGER PRIMARY KEY,count INTEGER NOT NULL,latest_platform_ms INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL);").unwrap();
    db.execute(
        "INSERT INTO qq_ignored_inputs VALUES(1,2,?1,?2)",
        params![NOW + 60_000, NOW],
    )
    .unwrap();
    migrate(&db).unwrap();
    assert_eq!(
        db.query_row("SELECT clock_fault_ms FROM qq_ignored_inputs", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        NOW + 60_000
    );
    migrate(&db).unwrap();
    assert_eq!(
        public_snapshot(&db, 10).unwrap()["ignored_inputs"]["count"],
        2
    );
}

#[test]
fn legacy_clock_fence_migration_failure_rolls_back_column_and_can_retry() {
    let db = database();
    db.execute_batch("DROP TABLE qq_ignored_inputs; CREATE TABLE qq_ignored_inputs(singleton INTEGER PRIMARY KEY,count INTEGER NOT NULL,latest_platform_ms INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL);").unwrap();
    db.execute(
        "INSERT INTO qq_ignored_inputs VALUES(1,2,?1,?2)",
        params![NOW + 60_000, NOW],
    )
    .unwrap();
    db.execute_batch("CREATE TRIGGER reject_clock_backfill BEFORE UPDATE ON qq_ignored_inputs BEGIN SELECT RAISE(ABORT,'backfill fault'); END;").unwrap();
    assert!(migrate(&db).is_err());
    let mut stmt = db.prepare("PRAGMA table_info(qq_ignored_inputs)").unwrap();
    let columns = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    drop(stmt);
    assert!(!columns.iter().any(|name| name == "clock_fault_ms"));
    assert_eq!(
        db.query_row(
            "SELECT latest_platform_ms FROM qq_ignored_inputs",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        NOW + 60_000
    );
    db.execute_batch("DROP TRIGGER reject_clock_backfill; BEGIN IMMEDIATE;")
        .unwrap();
    migrate(&db).unwrap(); // compatible with the later outer migration ledger
    db.execute_batch("COMMIT;").unwrap();
    assert_eq!(
        db.query_row("SELECT clock_fault_ms FROM qq_ignored_inputs", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        NOW + 60_000
    );
}
