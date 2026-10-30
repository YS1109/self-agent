use super::super::persistence::ConversationKind;
use super::*;
fn db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    persistence::migrate(&db).unwrap();
    configuration::migrate(&db).unwrap();
    migrate(&db).unwrap();
    db.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY,body TEXT NOT NULL);")
        .unwrap();
    db
}
fn envelope(command: Value) -> Value {
    json!({"id":harness_core::new_id(),"command":command})
}
fn configure(db: &mut Connection) -> Config {
    let tx = db.transaction().unwrap();
    let config = configuration::create(&tx, "app_1").unwrap();
    let config = configuration::update(
        &tx,
        &config.id,
        0,
        ConfigPatch {
            app_id: "app_1".into(),
            enabled: true,
            reply_policy: ReplyPolicy::default(),
        },
        100,
        |_| Ok(true),
    )
    .unwrap();
    tx.commit().unwrap();
    config
}
fn send(db: &mut Connection, value: &Value) -> Value {
    let tx = db.transaction().unwrap();
    let response = apply(&tx, value, 100, |_| Ok(true), |_| Ok(())).unwrap();
    tx.commit().unwrap();
    response
}
#[test]
fn pairing_is_first_response_only_and_idempotency_never_issues_another_code() {
    let mut db = db();
    configure(&mut db);
    let request = envelope(json!({"type":"issue_pairing","ttl_seconds":300}));
    let first = send(&mut db, &request);
    let code = first["pairing"]["code"].as_str().unwrap();
    assert_eq!(code.len(), 32);
    let replay = send(&mut db, &request);
    assert!(replay["pairing"]["code"].is_null());
    assert_eq!(
        replay,
        receipt(&db, request["id"].as_str().unwrap())
            .unwrap()
            .unwrap()
    );
    for sql in [
        "SELECT receipt FROM qq_admin_commands",
        "SELECT code_hash FROM qq_pairings",
    ] {
        let body: String = db.query_row(sql, [], |r| r.get(0)).unwrap();
        assert!(!body.contains(code));
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM qq_pairings", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    let mut conflict = request;
    conflict["command"]["ttl_seconds"] = json!(600);
    let tx = db.transaction().unwrap();
    assert!(apply(&tx, &conflict, 100, |_| Ok(true), |_| Ok(())).is_err());
}
#[test]
fn configuration_create_is_disabled_cas_and_failure_rolls_back_then_rejects() {
    let mut db = db();
    let bad = envelope(
        json!({"type":"configure","config_id":null,"expected_revision":null,"app_id":"app_1","enabled":false,"reply_policy":{"window_ms":300001,"max_user":4,"max_group":5}}),
    );
    {
        let tx = db.transaction().unwrap();
        assert!(apply(&tx, &bad, 100, |_| Ok(true), |_| Ok(())).is_err());
    }
    assert!(configuration::current(&db).unwrap().is_none());
    let tx = db.transaction().unwrap();
    let response = reject(&tx, &bad).unwrap();
    assert_eq!(response["status"], "rejected");
    tx.commit().unwrap();
    assert_eq!(send(&mut db, &bad)["status"], "rejected");
    assert!(configuration::current(&db).unwrap().is_none());
    let good = envelope(
        json!({"type":"configure","config_id":null,"expected_revision":null,"app_id":"app_1","enabled":false,"reply_policy":ReplyPolicy::default()}),
    );
    let first = send(&mut db, &good);
    assert!(first["config_id"].is_string());
    assert!(!configuration::current(&db).unwrap().unwrap().enabled);
    let conflict = envelope(
        json!({"type":"configure","config_id":first["config_id"],"expected_revision":99,"app_id":"app_1","enabled":false,"reply_policy":ReplyPolicy::default()}),
    );
    let tx = db.transaction().unwrap();
    assert!(apply(&tx, &conflict, 100, |_| Ok(true), |_| Ok(())).is_err());
}
#[test]
fn invalid_fields_are_not_persisted_or_secret_fingerprinted() {
    let mut db = db();
    let request = envelope(
        json!({"type":"issue_pairing","ttl_seconds":300,"secret":"do-not-store-or-fingerprint-this"}),
    );
    {
        let tx = db.transaction().unwrap();
        assert!(apply(&tx, &request, 100, |_| Ok(true), |_| Ok(())).is_err());
    }
    let tx = db.transaction().unwrap();
    let response = reject(&tx, &request).unwrap();
    assert_eq!(response["status"], "rejected");
    tx.commit().unwrap();
    let (digest, body): (String, String) = db
        .query_row("SELECT digest,receipt FROM qq_admin_commands", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(digest, "invalid_command");
    assert!(!body.contains("do-not-store"));
}
#[test]
fn confirm_requires_exact_pending_identity_existing_sessions_and_runtime_policy() {
    let mut db = db();
    configure(&mut db);
    let identity = Identity {
        app_id: "app_1".into(),
        sender_open_id: "user".into(),
        kind: ConversationKind::User,
        target_id: "user".into(),
    };
    let binding = {
        let tx = db.transaction().unwrap();
        let code = persistence::issue_pairing(&tx, "app_1", 100, 300).unwrap();
        let binding = persistence::redeem_pairing(&tx, &identity, &code.code, 100).unwrap();
        tx.commit().unwrap();
        binding
    };
    let session = harness_core::new_id();
    let grant = json!({"session_ids":[session],"permissions":{"view":true},"controls":policy::conservative_controls(),"expires_at_ms":100000});
    let request = envelope(
        json!({"type":"confirm_binding","principal_id":binding.principal_id,"expected_revision":binding.revision,"identity":identity,"grant":grant}),
    );
    {
        let tx = db.transaction().unwrap();
        assert!(apply(&tx, &request, 100, |_| Ok(true), |_| Ok(())).is_err());
    }
    db.execute("INSERT INTO sessions VALUES(?1,'{}')", [&session])
        .unwrap();
    {
        let tx = db.transaction().unwrap();
        assert!(apply(
            &tx,
            &request,
            100,
            |_| Ok(true),
            |_| anyhow::bail!("policy denied")
        )
        .is_err());
    }
    assert_eq!(
        persistence::binding(&db, &binding.principal_id)
            .unwrap()
            .status,
        "pending"
    );
    let response = send(&mut db, &request);
    assert_eq!(response["revision"], binding.revision + 1);
    assert_eq!(
        persistence::binding(&db, &binding.principal_id)
            .unwrap()
            .status,
        "active"
    );
    let mut wrong = request;
    wrong["id"] = json!(harness_core::new_id());
    wrong["command"]["identity"]["sender_open_id"] = json!("other");
    let tx = db.transaction().unwrap();
    assert!(apply(&tx, &wrong, 100, |_| Ok(true), |_| Ok(())).is_err());
}
#[test]
fn uncertain_success_cannot_be_overwritten_by_rejection() {
    let mut db = db();
    configure(&mut db);
    let request = envelope(json!({"type":"issue_pairing","ttl_seconds":300}));
    let first = send(&mut db, &request);
    let tx = db.transaction().unwrap();
    let recovered = reject(&tx, &request).unwrap();
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["pairing"]["id"], first["pairing"]["id"]);
    assert!(recovered["pairing"]["code"].is_null());
    tx.commit().unwrap();
}
