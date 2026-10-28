use super::super::{
    configuration::{Checkpoint, Config, ConfigPatch},
    persistence::{Identity, Inbound},
    policy::{conservative_controls, RemotePermissions},
};
use super::*;
const NOW: u64 = 1_800_000_000_000;
const SESSION: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3b";
const OTHER: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3c";
const TASK: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3d";
const REQUEST: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3e";
fn setup() -> (Connection, Config) {
    let mut db = Connection::open_in_memory().unwrap();
    persistence::migrate(&db).unwrap();
    configuration::migrate(&db).unwrap();
    migrate(&db).unwrap();
    db.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE tasks(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE plans(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE execution_requests(id TEXT PRIMARY KEY,task_id TEXT NOT NULL,body TEXT NOT NULL);").unwrap();
    for id in [SESSION, OTHER] {
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2)",
            params![
                id,
                json!({"id":id,"project_path":"/approved/root","title":"测试"}).to_string()
            ],
        )
        .unwrap();
    }
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
    tx.commit().unwrap();
    (db, c)
}
fn identity(sender: &str) -> Identity {
    Identity {
        app_id: "app_1".into(),
        sender_open_id: sender.into(),
        kind: ConversationKind::User,
        target_id: sender.into(),
    }
}
fn bind(db: &mut Connection, sender: &str) -> Binding {
    let tx = db.transaction().unwrap();
    let who = identity(sender);
    let code = persistence::issue_pairing(&tx, "app_1", NOW, 60).unwrap();
    let b = persistence::redeem_pairing(&tx, &who, &code.code, NOW).unwrap();
    let grant = RemoteGrant {
        session_ids: vec![SESSION.into()],
        permissions: RemotePermissions {
            view: true,
            run: true,
            answer: true,
            create_session: true,
            ..Default::default()
        },
        controls: conservative_controls(),
        expires_at_ms: NOW + 600000,
    };
    let b = persistence::confirm_binding(
        &tx,
        &b.principal_id,
        b.revision,
        &who,
        serde_json::to_value(grant).unwrap(),
        NOW + 600000,
        NOW - 31_000,
    )
    .unwrap();
    tx.commit().unwrap();
    b
}
fn input(db: &mut Connection, b: &Binding, id: &str, content: &str) -> persistence::AcceptedInput {
    let tx = db.transaction().unwrap();
    let a = persistence::accept_input(
        &tx,
        &Inbound {
            identity: b.identity.clone(),
            message_id: id.into(),
            event_aliases: vec![],
            canonical_input: json!({"kind":"command","content":content}),
            received_at_ms: NOW,
        },
        NOW,
    )
    .unwrap();
    tx.commit().unwrap();
    a
}
fn dispatch(tx: &Transaction<'_>, e: Execution<'_>) -> Result<(Value, Option<RunEvent>)> {
    assert_eq!(e.envelope["source"]["channel_id"], "qq");
    assert!(e.decision_actor.starts_with("qq:"));
    assert_eq!(e.grant.controls, conservative_controls());
    let command = &e.envelope["command"];
    let mut receipt = json!({"command_id":e.envelope["command_id"]});
    match command["type"].as_str().unwrap() {
        "select_session" => receipt["session_id"] = command["session_id"].clone(),
        "create_session" => {
            assert_eq!(e.copied_project_root.as_deref(), Some("/approved/root"));
            receipt["session_id"] = json!(REQUEST);
            tx.execute(
                "INSERT INTO sessions VALUES(?1,?2)",
                params![
                    REQUEST,
                    json!({"id":REQUEST,"project_path":e.copied_project_root}).to_string()
                ],
            )?;
        }
        "run_agent" => {
            receipt["task_id"] = json!(TASK);
            tx.execute("INSERT INTO tasks VALUES(?1,?2)",params![TASK,json!({"id":TASK,"session_id":command["session_id"],"source":e.envelope["source"]}).to_string()])?;
        }
        "answer_request" => {
            receipt["request_id"] = command["request_id"].clone();
        }
        _ => bail!("意外的夹具指令"),
    }
    Ok((receipt, None))
}
fn run(db: &mut Connection, c: &Config) {
    let tx = db.transaction().unwrap();
    assert!(process(&tx, &c.id, c.revision, NOW, dispatch)
        .unwrap()
        .is_some());
    tx.commit().unwrap();
}
fn count(db: &Connection, table: &str) -> u64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
#[test]
fn pairing_checkpoint_and_current_config_are_atomic_and_fenced() {
    let (mut db, c) = setup();
    let tx = db.transaction().unwrap();
    let code = persistence::issue_pairing(&tx, "app_1", NOW, 60).unwrap();
    let a = persistence::accept_input(
        &tx,
        &Inbound {
            identity: identity("new"),
            message_id: "pair".into(),
            event_aliases: vec![],
            canonical_input: persistence::pairing_canonical("app_1", &code.code).unwrap(),
            received_at_ms: NOW,
        },
        NOW,
    )
    .unwrap();
    configuration::ready_checkpoint(
        &tx,
        &Checkpoint {
            config_id: c.id.clone(),
            config_revision: c.revision,
            session_id: "session".into(),
            sequence: 5,
            bot_user_id: "bot".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(process(&tx, &c.id, c.revision + 1, NOW, |_, _| panic!(
            "must not dispatch"
        ))
        .is_err());
    }
    run(&mut db, &c);
    assert_eq!(
        persistence::inbox(&db, &a.inbox_id).unwrap().status,
        "handled"
    );
    assert_eq!(count(&db, "qq_bindings"), 1);
    assert_eq!(count(&db, "qq_reply_routes"), 0);
    assert_eq!(count(&db, "qq_outbox"), 0);
    assert_eq!(
        configuration::load_checkpoint(&db, &c.id, c.revision)
            .unwrap()
            .unwrap()
            .sequence,
        5
    );
    let tx = db.transaction().unwrap();
    assert!(
        process(&tx, &c.id, c.revision, NOW, |_, _| panic!("replayed"))
            .unwrap()
            .is_none()
    );
}
#[test]
fn stable_selection_different_binding_and_immutable_source_route() {
    let (mut db, c) = setup();
    let b = bind(&mut db, "one");
    let other = bind(&mut db, "two");
    let selected = input(&mut db, &b, "select", &format!("/选择 {SESSION}"));
    run(&mut db, &c);
    let started = input(&mut db, &b, "run", "执行测试");
    run(&mut db, &c);
    let task = record(&db, "tasks", TASK).unwrap();
    assert_eq!(task["source"]["conversation_id"], started.route_id.unwrap());
    assert_ne!(
        task["source"]["conversation_id"],
        selected.route_id.unwrap()
    );
    assert_eq!(task["source"]["actor_id"], b.principal_id);
    input(&mut db, &other, "other", "执行测试");
    let tx = db.transaction().unwrap();
    assert!(process(&tx, &c.id, c.revision, NOW, dispatch).is_err());
    tx.rollback().unwrap();
    assert_eq!(count(&db, "tasks"), 1);
}
#[test]
fn cross_session_and_local_task_steering_never_dispatch() {
    let (mut db, c) = setup();
    let b = bind(&mut db, "one");
    for (idx, session) in [OTHER, SESSION].iter().enumerate() {
        db.execute(
            "INSERT OR REPLACE INTO tasks VALUES(?1,?2)",
            params![
                TASK,
                json!({"id":TASK,"session_id":session,"source":{"channel_id":"desktop"}})
                    .to_string()
            ],
        )
        .unwrap();
        let a = input(
            &mut db,
            &b,
            &format!("m{idx}"),
            &format!("/补充 {TASK} 注入"),
        );
        let tx = db.transaction().unwrap();
        assert!(process(&tx, &c.id, c.revision, NOW, |_, _| panic!(
            "unauthorized dispatch"
        ))
        .is_err());
        tx.rollback().unwrap();
        let tx = db.transaction().unwrap();
        reject_pending(&tx, &a.inbox_id, NOW).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            persistence::inbox(&db, &a.inbox_id)
                .unwrap()
                .receipt
                .unwrap()["status"],
            "rejected"
        );
    }
}
#[test]
fn create_acl_selection_ack_receipt_commit_together() {
    let (mut db, c) = setup();
    let b = bind(&mut db, "one");
    let a = input(&mut db, &b, "new", &format!("/新建 {SESSION} 新会话"));
    run(&mut db, &c);
    assert!(persistence::allowed_session(&db, &b.principal_id, REQUEST, NOW).unwrap());
    assert_eq!(
        record(&db, "sessions", REQUEST).unwrap()["project_path"],
        "/approved/root"
    );
    assert_eq!(count(&db, "qq_selections"), 1);
    assert_eq!(count(&db, "qq_outbox"), 1);
    assert_eq!(
        persistence::inbox(&db, &a.inbox_id)
            .unwrap()
            .receipt
            .unwrap()["session_id"],
        REQUEST
    );
    let tx = db.transaction().unwrap();
    assert!(
        process(&tx, &c.id, c.revision, NOW, |_, _| panic!("duplicate"))
            .unwrap()
            .is_none()
    );
    reject_pending(&tx, &a.inbox_id, NOW).unwrap();
    tx.commit().unwrap();
    assert_eq!(count(&db, "qq_outbox"), 1);
}
#[test]
fn failed_dispatch_rolls_back_all_then_rejection_is_separate() {
    let (mut db, c) = setup();
    let b = bind(&mut db, "one");
    let a = input(&mut db, &b, "new", &format!("/新建 {SESSION} 新会话"));
    let tx = db.transaction().unwrap();
    assert!(process(&tx, &c.id, c.revision, NOW, |tx, e| {
        dispatch(tx, e)?;
        bail!("模拟提交前失败")
    })
    .is_err());
    tx.rollback().unwrap();
    assert_eq!(count(&db, "sessions"), 2);
    assert_eq!(count(&db, "qq_session_acl"), 0);
    assert_eq!(count(&db, "qq_selections"), 0);
    assert_eq!(count(&db, "qq_outbox"), 0);
    assert_eq!(
        persistence::inbox(&db, &a.inbox_id).unwrap().status,
        "pending"
    );
    let tx = db.transaction().unwrap();
    reject_pending(&tx, &a.inbox_id, NOW).unwrap();
    reject_pending(&tx, &a.inbox_id, NOW).unwrap();
    tx.commit().unwrap();
    assert_eq!(count(&db, "qq_outbox"), 1);
    assert_eq!(
        persistence::inbox(&db, &a.inbox_id).unwrap().status,
        "handled"
    );
}
#[test]
fn optional_rejection_send_failure_rolls_back_sequence_but_records_receipt() {
    let (mut db, c) = setup();
    let b = bind(&mut db, "one");
    let a = input(&mut db, &b, "bad", "/未知");
    let tx = db.transaction().unwrap();
    assert!(process(&tx, &c.id, c.revision, NOW, dispatch).is_err());
    tx.rollback().unwrap();
    db.execute_batch("CREATE TRIGGER reject_outbox BEFORE INSERT ON qq_outbox BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let tx = db.transaction().unwrap();
    reject_pending(&tx, &a.inbox_id, NOW).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        persistence::inbox(&db, &a.inbox_id).unwrap().status,
        "handled"
    );
    assert_eq!(count(&db, "qq_outbox"), 0);
    let seq: u32 = db
        .query_row(
            "SELECT next_msg_seq FROM qq_reply_routes WHERE route_id=?1",
            [a.route_id.unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seq, 1);
}
#[test]
fn answer_requires_owned_question_never_approval() {
    let (mut db, c) = setup();
    let b = bind(&mut db, "one");
    let source = input(&mut db, &b, "source", "/帮助");
    run(&mut db, &c);
    db.execute("INSERT INTO tasks VALUES(?1,?2)",params![TASK,json!({"id":TASK,"session_id":SESSION,"status":"waiting_input","waiting_request_id":REQUEST,"source":{"channel_id":"qq","actor_id":b.principal_id,"conversation_id":source.route_id}}).to_string()]).unwrap();
    for (index, kind) in ["approval", "question"].iter().enumerate() {
        db.execute(
            "INSERT OR REPLACE INTO execution_requests VALUES(?1,?2,?3)",
            params![
                REQUEST,
                TASK,
                json!({"id":REQUEST,"task_id":TASK,"kind":kind,"status":"pending","revision":0})
                    .to_string()
            ],
        )
        .unwrap();
        let a = input(
            &mut db,
            &b,
            &format!("answer{index}"),
            &format!("/回答 {REQUEST} 0 同意"),
        );
        let tx = db.transaction().unwrap();
        if *kind == "approval" {
            assert!(process(&tx, &c.id, c.revision, NOW, |_, _| panic!(
                "approval dispatch"
            ))
            .is_err());
            tx.rollback().unwrap();
            let tx = db.transaction().unwrap();
            reject_pending(&tx, &a.inbox_id, NOW).unwrap();
            tx.commit().unwrap();
        } else {
            assert!(process(&tx, &c.id, c.revision, NOW, dispatch)
                .unwrap()
                .is_some());
            tx.commit().unwrap();
        }
    }
}

#[test]
fn ack_failure_rolls_back_successful_create_acl_selection_and_receipt() {
    let (mut db, c) = setup();
    let b = bind(&mut db, "one");
    let a = input(&mut db, &b, "new", &format!("/新建 {SESSION} 新会话"));
    db.execute_batch("CREATE TRIGGER fail_ack BEFORE INSERT ON qq_outbox BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let tx = db.transaction().unwrap();
    assert!(process(&tx, &c.id, c.revision, NOW, dispatch).is_err());
    tx.rollback().unwrap();
    assert_eq!(count(&db, "sessions"), 2);
    assert_eq!(count(&db, "qq_session_acl"), 0);
    assert_eq!(count(&db, "qq_selections"), 0);
    assert_eq!(count(&db, "qq_outbox"), 0);
    assert_eq!(
        persistence::inbox(&db, &a.inbox_id).unwrap().status,
        "pending"
    );
    let seq: u32 = db
        .query_row(
            "SELECT next_msg_seq FROM qq_reply_routes WHERE route_id=?1",
            [a.route_id.unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seq, 1);
    let tx = db.transaction().unwrap();
    reject_pending(&tx, &a.inbox_id, NOW).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        persistence::inbox(&db, &a.inbox_id).unwrap().status,
        "handled"
    );
}
