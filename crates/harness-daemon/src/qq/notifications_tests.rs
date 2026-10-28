use super::super::{
    configuration::ConfigPatch,
    persistence::{ConversationKind, DeliveryOutcome, Identity, Inbound},
    policy::{conservative_controls, RemoteGrant, RemotePermissions},
};
use super::*;
const NOW: u64 = 1_800_000_000_000;
const SESSION: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3b";
const PLAN: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3c";
fn setup() -> Connection {
    let mut db = Connection::open_in_memory().unwrap();
    persistence::migrate(&db).unwrap();
    configuration::migrate(&db).unwrap();
    migrate(&db).unwrap();
    db.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE deliveries(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE plans(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE coordinator_jobs(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE qq_selections(scope TEXT PRIMARY KEY,session_id TEXT NOT NULL);").unwrap();
    let tx = db.transaction().unwrap();
    let c = configuration::create(&tx, "app_1").unwrap();
    configuration::update(
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
    tx.commit().unwrap();
    db
}
fn source(db: &mut Connection, peer: &str) -> Value {
    let tx = db.transaction().unwrap();
    let who = Identity {
        app_id: "app_1".into(),
        sender_open_id: peer.into(),
        kind: ConversationKind::User,
        target_id: peer.into(),
    };
    let ticket = persistence::issue_pairing(&tx, "app_1", NOW, 60).unwrap();
    let pending = persistence::redeem_pairing(&tx, &who, &ticket.code, NOW).unwrap();
    let grant = RemoteGrant {
        session_ids: vec![SESSION.into()],
        permissions: RemotePermissions {
            view: true,
            ..Default::default()
        },
        controls: conservative_controls(),
        expires_at_ms: NOW + 600000,
    };
    let binding = persistence::confirm_binding(
        &tx,
        &pending.principal_id,
        pending.revision,
        &who,
        serde_json::to_value(grant).unwrap(),
        NOW + 600000,
        NOW - 31_000,
    )
    .unwrap();
    let accepted = persistence::accept_input(
        &tx,
        &Inbound {
            identity: who,
            message_id: format!("message_{peer}"),
            event_aliases: vec![],
            canonical_input: json!({"content":"test"}),
            received_at_ms: NOW,
        },
        NOW,
    )
    .unwrap();
    tx.commit().unwrap();
    json!({"channel_id":"qq","actor_id":binding.principal_id,"conversation_id":accepted.route_id.unwrap()})
}
fn delivery(db: &Connection, id: &str, kind: &str, source: &Value) -> Value {
    let task = json!({"id":id,"session_id":SESSION,"task_kind":kind,"status":"completed","result":"CANARY_SECRET_RESULT","error":"CANARY_SECRET_ERROR","source":source});
    db.execute(
        "INSERT INTO tasks VALUES(?1,?2)",
        params![id, task.to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO deliveries VALUES(?1,?2)",
        params![
            id,
            json!({"id":id,"task_id":id,"session_id":SESSION,"status":"pending","target":source})
                .to_string()
        ],
    )
    .unwrap();
    task
}
fn run(db: &mut Connection, now: u64) {
    let tx = db.transaction().unwrap();
    bridge(&tx, now).unwrap();
    tx.commit().unwrap();
}
fn count(db: &Connection, table: &str) -> u64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn payloads(db: &Connection) -> String {
    let mut q = db
        .prepare("SELECT payload FROM qq_outbox ORDER BY rowid")
        .unwrap();
    q.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n")
}
#[test]
fn main_plan_notify_original_route_status_only_and_workers_skip() {
    let mut db = setup();
    let src = source(&mut db, "one");
    let other = source(&mut db, "two");
    db.execute(
        "INSERT INTO qq_selections VALUES('changed','other-session')",
        [],
    )
    .unwrap();
    for (id, kind) in [("main", "main"), ("plan", "plan"), ("worker", "worker")] {
        delivery(&db, id, kind, &src);
    }
    run(&mut db, NOW);
    assert_eq!(count(&db, "qq_outbox"), 2);
    assert_eq!(
        get(&db, "deliveries", "worker").unwrap()["status"],
        "not_notified"
    );
    let routes: Vec<String> = {
        let mut q = db.prepare("SELECT route_id FROM qq_outbox").unwrap();
        let rows = q.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    };
    assert!(routes
        .iter()
        .all(|r| r == src["conversation_id"].as_str().unwrap()));
    assert_ne!(src["conversation_id"], other["conversation_id"]);
    assert!(!payloads(&db).contains("CANARY"));
    run(&mut db, NOW);
    assert_eq!(count(&db, "qq_outbox"), 2);
}
#[test]
fn actor_or_task_source_mismatch_never_queues() {
    let mut db = setup();
    let src = source(&mut db, "one");
    let other = source(&mut db, "two");
    let mut wrong = src.clone();
    wrong["actor_id"] = other["actor_id"].clone();
    delivery(&db, "wrong_actor", "main", &wrong);
    delivery(&db, "wrong_target", "main", &src);
    let mut d = get(&db, "deliveries", "wrong_target").unwrap();
    d["target"] = other;
    db.execute(
        "UPDATE deliveries SET body=?1 WHERE id='wrong_target'",
        [d.to_string()],
    )
    .unwrap();
    run(&mut db, NOW);
    assert_eq!(count(&db, "qq_outbox"), 0);
    assert_eq!(count(&db, "qq_notifications"), 1);
    assert_eq!(
        get(&db, "deliveries", "wrong_target").unwrap()["status"],
        "not_notified"
    );
}
#[test]
fn expired_notification_never_changes_completed_task_or_replays() {
    let mut db = setup();
    let src = source(&mut db, "one");
    let original = delivery(&db, "expired", "main", &src);
    run(&mut db, NOW + 300001);
    assert_eq!(get(&db, "tasks", "expired").unwrap(), original);
    assert_eq!(count(&db, "qq_outbox"), 1);
    assert_eq!(
        get(&db, "deliveries", "expired").unwrap()["status"],
        "expired"
    );
    {
        let tx = db.transaction().unwrap();
        assert!(persistence::claim_outbox(&tx, NOW + 300001)
            .unwrap()
            .is_none());
    }
    run(&mut db, NOW + 300002);
    assert_eq!(count(&db, "qq_notifications"), 1);
}
#[test]
fn unknown_finalization_syncs_without_bridge_replay() {
    let mut db = setup();
    let src = source(&mut db, "one");
    let original = delivery(&db, "unknown", "main", &src);
    run(&mut db, NOW);
    let tx = db.transaction().unwrap();
    let claimed = persistence::claim_outbox(&tx, NOW).unwrap().unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    persistence::finalize_outbox(
        &tx,
        &claimed.record.id,
        claimed.record.claim_nonce.as_deref().unwrap(),
        DeliveryOutcome::Unknown,
        NOW + 1,
    )
    .unwrap();
    sync(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        get(&db, "deliveries", "unknown").unwrap()["status"],
        "unknown"
    );
    assert_eq!(get(&db, "tasks", "unknown").unwrap(), original);
    run(&mut db, NOW + 2);
    assert_eq!(count(&db, "qq_outbox"), 1);
    let tx = db.transaction().unwrap();
    assert!(persistence::claim_outbox(&tx, NOW + 3).unwrap().is_none());
}
#[test]
fn enqueue_fault_restores_sequence_and_records_not_queued() {
    let mut db = setup();
    let src = source(&mut db, "one");
    let original = delivery(&db, "fault", "main", &src);
    db.execute_batch("CREATE TRIGGER fail_notification BEFORE INSERT ON qq_outbox BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    run(&mut db, NOW);
    assert_eq!(get(&db, "tasks", "fault").unwrap(), original);
    assert_eq!(count(&db, "qq_outbox"), 0);
    let seq: u32 = db
        .query_row(
            "SELECT next_msg_seq FROM qq_reply_routes WHERE route_id=?1",
            [src["conversation_id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seq, 1);
    let state: String = db
        .query_row("SELECT state FROM qq_notifications", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "not_queued");
    db.execute_batch("DROP TRIGGER fail_notification").unwrap();
    run(&mut db, NOW + 1);
    assert_eq!(count(&db, "qq_outbox"), 0);
}
#[test]
fn coordinator_draft_summary_only_and_logical_reply_is_unique() {
    let mut db = setup();
    let src = source(&mut db, "one");
    let job = json!({"id":"coordinator-job","kind":"message","status":"completed","session_id":SESSION,"source":src,"result":"CANARY_ASSISTANT_SECRET","error":"CANARY_ERROR"});
    db.execute(
        "INSERT INTO coordinator_jobs VALUES('coordinator-job',?1)",
        [job.to_string()],
    )
    .unwrap();
    let plan = json!({"id":PLAN,"session_id":SESSION,"source":src,"status":"draft","revision":0,"objective":"CANARY_OBJECTIVE","nodes":[{"content":"CANARY_NODE"}]});
    db.execute(
        "INSERT INTO plans VALUES(?1,?2)",
        params![PLAN, plan.to_string()],
    )
    .unwrap();
    run(&mut db, NOW);
    let payload = payloads(&db);
    assert!(payload.contains(PLAN));
    assert!(payload.contains("/计划"));
    assert!(!payload.contains("CANARY"));
    run(&mut db, NOW);
    assert_eq!(count(&db, "qq_outbox"), 1);
    assert_eq!(count(&db, "qq_notifications"), 1);
    assert_eq!(
        get(&db, "coordinator_jobs", "coordinator-job").unwrap(),
        job
    );
}

#[test]
fn nonterminal_task_and_session_mismatch_are_not_completion_notifications() {
    let mut db = setup();
    let src = source(&mut db, "one");
    let mut task = delivery(&db, "running", "main", &src);
    task["status"] = json!("running");
    db.execute(
        "UPDATE tasks SET body=?1 WHERE id='running'",
        [task.to_string()],
    )
    .unwrap();
    delivery(&db, "session_mismatch", "main", &src);
    let mut d = get(&db, "deliveries", "session_mismatch").unwrap();
    d["session_id"] = json!(PLAN);
    db.execute(
        "UPDATE deliveries SET body=?1 WHERE id='session_mismatch'",
        [d.to_string()],
    )
    .unwrap();
    run(&mut db, NOW);
    assert_eq!(count(&db, "qq_outbox"), 0);
    assert_eq!(get(&db, "tasks", "running").unwrap()["status"], "running");
}
