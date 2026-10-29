use super::*;
use harness_core::TaskControls;
struct Context;
impl ResourceContext for Context {
    fn channel_current(&self) -> bool {
        true
    }
    fn session_exists(&self, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn task_session(&self, _: &str) -> Result<String> {
        bail!("unused")
    }
    fn task_controls(&self, _: &str) -> Result<TaskControls> {
        bail!("unused")
    }
    fn question_task(&self, _: &str, _: u32) -> Result<(String, bool)> {
        bail!("unused")
    }
    fn artifact_task(&self, _: &str) -> Result<String> {
        bail!("unused")
    }
}
fn setup() -> rusqlite::Connection {
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    let tx = db.transaction().unwrap();
    persistence::migrate(&tx).unwrap();
    configuration::migrate(&tx).unwrap();
    migrate(&tx).unwrap();
    tx.commit().unwrap();
    db
}
fn command(value: Value) -> Value {
    json!({"id":harness_core::new_id(),"command":value})
}
fn apply_test(tx: &Transaction<'_>, value: &Value) -> Result<Value> {
    apply(tx, value, 1000, |_| Ok(true), |_| Ok(()), &Context)
}
fn enabled(tx: &Transaction<'_>) -> Config {
    let id = harness_core::new_id();
    let create = command(
        json!({"type":"configure","config_id":null,"expected_revision":null,"device_id":id,"origin":"https://fixture.invalid","enabled":false}),
    );
    let result = apply_test(tx, &create).unwrap();
    let update = command(
        json!({"type":"configure","config_id":result["config_id"],"expected_revision":result["revision"],"device_id":id,"origin":"https://fixture.invalid","enabled":true}),
    );
    apply_test(tx, &update).unwrap();
    configuration::current(tx).unwrap().unwrap()
}
#[test]
fn pairing_secret_is_first_response_only_and_not_in_database() {
    let mut db = setup();
    let tx = db.transaction().unwrap();
    enabled(&tx);
    let value = command(json!({"type":"issue_pairing","ttl_seconds":60}));
    let first = apply_test(&tx, &value).unwrap();
    let code = first["pairing"]["code"].as_str().unwrap();
    assert_eq!(code.len(), 64);
    let replay = apply_test(&tx, &value).unwrap();
    assert!(replay["pairing"]["code"].is_null());
    let raw: String = tx
        .query_row(
            "SELECT receipt FROM gateway_admin_commands WHERE id=?1",
            [value["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!raw.contains(code));
    tx.commit().unwrap();
    assert!(receipt(&db, value["id"].as_str().unwrap())
        .unwrap()
        .unwrap()["pairing"]["code"]
        .is_null());
}
#[test]
fn transaction_failure_rolls_back_pairing_and_success_cannot_be_rejected() {
    let mut db = setup();
    let tx = db.transaction().unwrap();
    enabled(&tx);
    tx.commit().unwrap();
    let value = command(json!({"type":"issue_pairing","ttl_seconds":60}));
    let tx = db.transaction().unwrap();
    apply_test(&tx, &value).unwrap();
    tx.rollback().unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM gateway_pairings", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    let tx = db.transaction().unwrap();
    let first = apply_test(&tx, &value).unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    let prior = reject(&tx, &value).unwrap();
    assert_eq!(prior["status"], first["status"]);
    assert!(prior["pairing"]["code"].is_null());
}
#[test]
fn duplicate_id_different_arguments_and_unknown_secret_fields_fail_closed() {
    let mut db = setup();
    let tx = db.transaction().unwrap();
    enabled(&tx);
    let mut value = command(json!({"type":"issue_pairing","ttl_seconds":60}));
    apply_test(&tx, &value).unwrap();
    value["command"]["ttl_seconds"] = json!(120);
    assert!(apply_test(&tx, &value).is_err());
    let invalid =
        command(json!({"type":"issue_pairing","ttl_seconds":60,"secret":"UNSUPPORTED_SECRET"}));
    assert!(apply_test(&tx, &invalid).is_err());
    let result = reject(&tx, &invalid).unwrap();
    assert_eq!(result["status"], "rejected");
    let stored: String = tx
        .query_row(
            "SELECT digest||receipt FROM gateway_admin_commands WHERE id=?1",
            [invalid["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!stored.contains("UNSUPPORTED_SECRET"));
}
#[test]
fn create_enabled_or_wrong_revision_never_issues_authority() {
    let mut db = setup();
    let tx = db.transaction().unwrap();
    let invalid = command(
        json!({"type":"configure","config_id":null,"expected_revision":null,"origin":"https://fixture.invalid","device_id":harness_core::new_id(),"enabled":true}),
    );
    assert!(apply_test(&tx, &invalid).is_err());
    assert!(configuration::current(&tx).unwrap().is_none());
    let c = enabled(&tx);
    let wrong = command(
        json!({"type":"configure","config_id":c.id,"expected_revision":c.revision-1,"origin":c.origin,"device_id":c.device_id,"enabled":false}),
    );
    assert!(apply_test(&tx, &wrong).is_err());
    assert!(configuration::current(&tx).unwrap().unwrap().enabled);
}
