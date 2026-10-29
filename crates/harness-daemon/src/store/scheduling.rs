//! Indexed scheduling reads. Historical JSON is not a scheduler work queue.
//! These indexes must be installed by an explicit application schema migration;
//! this module never performs an opportunistic migration from a read path.
use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::Value;

pub(super) const INDEXES: &[(&str, &str)] = &[
    ("scheduler_tasks_status_created", "CREATE INDEX scheduler_tasks_status_created ON tasks(json_extract(body,'$.status'),json_extract(body,'$.created_at'))"),
    ("scheduler_plans_status_created", "CREATE INDEX scheduler_plans_status_created ON plans(json_extract(body,'$.status'),json_extract(body,'$.created_at'))"),
    ("scheduler_jobs_status_created", "CREATE INDEX scheduler_jobs_status_created ON coordinator_jobs(json_extract(body,'$.status'),json_extract(body,'$.created_at'))"),
    ("scheduler_deliveries_status_created", "CREATE INDEX scheduler_deliveries_status_created ON deliveries(json_extract(body,'$.status'),json_extract(body,'$.created_at'))"),
    ("scheduler_tasks_source_status", "CREATE INDEX scheduler_tasks_source_status ON tasks(json_extract(body,'$.source.channel_id'),json_extract(body,'$.status'))"),
    ("scheduler_plans_source_status", "CREATE INDEX scheduler_plans_source_status ON plans(json_extract(body,'$.source.channel_id'),json_extract(body,'$.status'))"),
    ("scheduler_jobs_source_status", "CREATE INDEX scheduler_jobs_source_status ON coordinator_jobs(json_extract(body,'$.source.channel_id'),json_extract(body,'$.status'))"),
];

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    for (_, sql) in INDEXES {
        // Do not accept an existing same-name index with a different expression.
        tx.execute_batch(sql)?;
    }
    validate(tx)
}

pub(super) fn validate(db: &Connection) -> Result<()> {
    for (name, expected) in INDEXES {
        let sql: Option<String> = db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='index' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        // These indexes have no published legacy spellings or user-authored
        // equivalents. Accept only the exact trusted DDL emitted by migrate.
        if sql.as_deref() != Some(expected) {
            bail!("历史调度索引缺失或定义不兼容");
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum RemoteTable {
    Tasks,
    Plans,
    Jobs,
}
impl RemoteTable {
    fn name(self) -> &'static str {
        match self {
            Self::Tasks => "tasks",
            Self::Plans => "plans",
            Self::Jobs => "coordinator_jobs",
        }
    }
}

fn collect(db: &Connection, sql: &str) -> Result<Vec<Value>> {
    let mut statement = db.prepare(sql)?;
    let records = statement.query_map([], |row| row.get::<_, String>(0))?;
    records
        .map(|record| Ok(serde_json::from_str(&record?)?))
        .collect()
}

pub(super) const QUEUED_TASKS: &str = "SELECT body FROM tasks WHERE json_extract(body,'$.status')='queued' AND COALESCE(json_extract(body,'$.task_kind'),'')<>'plan' ORDER BY json_extract(body,'$.created_at'),rowid";
pub(super) fn queued_tasks(db: &Connection) -> Result<Vec<Value>> {
    collect(db, QUEUED_TASKS)
}

pub(super) fn has_active_main_predecessor(
    db: &Connection,
    session_id: &str,
    sequence: u64,
) -> Result<bool> {
    // Filter on the trusted status index before decoding. Keep sequence
    // comparisons in serde/u64 rather than SQLite floating-point conversion,
    // preserving exact legacy values even above 2^53.
    let mut statement = db.prepare("SELECT body FROM tasks WHERE json_extract(body,'$.status') IN ('queued','running','waiting_input','paused') AND json_extract(body,'$.task_kind')='main' AND json_extract(body,'$.session_id')=?1")?;
    let rows = statement.query_map([session_id], |row| row.get::<_, String>(0))?;
    for body in rows {
        let task: Value = serde_json::from_str(&body?)?;
        if task["main_sequence"].as_u64().unwrap_or(0) < sequence {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn pending_jobs(db: &Connection) -> Result<Vec<Value>> {
    collect(db, "SELECT body FROM coordinator_jobs WHERE json_extract(body,'$.status')='pending' ORDER BY json_extract(body,'$.created_at'),rowid")
}

pub(super) fn running_plans(db: &Connection) -> Result<Vec<Value>> {
    collect(
        db,
        "SELECT body FROM plans WHERE json_extract(body,'$.status')='running' ORDER BY rowid",
    )
}

pub(super) fn pending_delivery(db: &Connection) -> Result<Option<Value>> {
    let body: Option<String> = db.query_row("SELECT body FROM deliveries WHERE json_extract(body,'$.status')='pending' AND COALESCE(json_extract(body,'$.target.channel_id'),'')<>'qq' ORDER BY rowid LIMIT 1", [], |row| row.get(0)).optional()?;
    body.map(|body| Ok(serde_json::from_str(&body)?))
        .transpose()
}

pub(super) fn active_remote(
    db: &Connection,
    table: RemoteTable,
    channel: &str,
) -> Result<Vec<Value>> {
    // No hidden LIMIT: every active remote record still receives the same live
    // revocation check. Fixed enums, not request strings, select the SQL table.
    let sql = format!("SELECT body FROM {} WHERE json_extract(body,'$.source.channel_id')=?1 AND json_extract(body,'$.status') NOT IN ('completed','failed','cancelled') ORDER BY rowid", table.name());
    let mut statement = db.prepare(&sql)?;
    let records = statement.query_map([channel], |row| row.get::<_, String>(0))?;
    records
        .map(|record| Ok(serde_json::from_str(&record?)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, StatementStatus};
    use serde_json::json;

    fn fixture() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        for table in ["tasks", "plans", "coordinator_jobs", "deliveries"] {
            db.execute_batch(&format!(
                "CREATE TABLE {table}(id TEXT PRIMARY KEY, body TEXT NOT NULL)"
            ))
            .unwrap();
        }
        let tx = db.transaction().unwrap();
        migrate(&tx).unwrap();
        tx.commit().unwrap();
        db
    }
    fn put(db: &Connection, table: &str, id: &str, status: &str, kind: &str, channel: &str) {
        db.execute(&format!("INSERT OR REPLACE INTO {table}(id,body) VALUES(?1,?2)"), params![id, json!({"id":id,"created_at":"same-time","status":status,"task_kind":kind,"source":{"channel_id":channel},"target":{"channel_id":channel},"result":"历史".repeat(512)}).to_string()]).unwrap();
    }
    #[test]
    fn empty_queue_uses_index_without_visiting_completed_history() {
        let mut db = fixture();
        let tx = db.transaction().unwrap();
        for i in 0..5000 {
            put(&tx, "tasks", &i.to_string(), "completed", "main", "desktop");
        }
        tx.commit().unwrap();
        let mut statement = db.prepare(QUEUED_TASKS).unwrap();
        assert!(statement.query([]).unwrap().next().unwrap().is_none());
        assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
        assert!(statement.get_status(StatementStatus::VmStep) < 100);
        assert!(queued_tasks(&db).unwrap().is_empty());
    }
    #[test]
    fn serial_predecessor_ignores_history_and_preserves_exact_large_sequences() {
        let db = fixture();
        let put_task = |id: &str, status: &str, session: &str, sequence: u64| {
            let body = json!({"id":id,"status":status,"task_kind":"main","session_id":session,"main_sequence":sequence,"created_at":"same"});
            db.execute(
                "INSERT OR REPLACE INTO tasks(id,body) VALUES(?1,?2)",
                params![id, body.to_string()],
            )
            .unwrap();
        };
        for n in 0..5000 {
            put_task(&n.to_string(), "completed", "current", 0);
        }
        put_task("other", "running", "other", 0);
        assert!(!has_active_main_predecessor(&db, "current", u64::MAX).unwrap());
        put_task("same", "waiting_input", "current", u64::MAX - 1);
        assert!(!has_active_main_predecessor(&db, "current", u64::MAX - 1).unwrap());
        assert!(has_active_main_predecessor(&db, "current", u64::MAX).unwrap());
        put_task("same", "failed", "current", u64::MAX - 1);
        assert!(!has_active_main_predecessor(&db, "current", u64::MAX).unwrap());
    }

    #[test]
    fn selection_order_and_terminal_filters_match_scheduler_semantics() {
        let db = fixture();
        put(&db, "tasks", "z", "queued", "main", "desktop");
        put(&db, "tasks", "a", "queued", "main", "desktop");
        put(&db, "tasks", "parent", "queued", "plan", "desktop");
        put(&db, "tasks", "done", "completed", "main", "desktop");
        assert_eq!(
            queued_tasks(&db)
                .unwrap()
                .iter()
                .map(|v| v["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["z", "a"]
        );
        put(
            &db,
            "coordinator_jobs",
            "pending",
            "pending",
            "message",
            "desktop",
        );
        put(
            &db,
            "coordinator_jobs",
            "done",
            "completed",
            "message",
            "desktop",
        );
        assert_eq!(pending_jobs(&db).unwrap().len(), 1);
        put(&db, "plans", "active", "running", "plan", "desktop");
        put(&db, "plans", "paused", "paused", "plan", "desktop");
        assert_eq!(running_plans(&db).unwrap()[0]["id"], "active");
        put(&db, "deliveries", "qq", "pending", "", "qq");
        put(&db, "deliveries", "local", "pending", "", "desktop");
        put(&db, "deliveries", "sent", "sent", "", "desktop");
        assert_eq!(pending_delivery(&db).unwrap().unwrap()["id"], "local");
    }
    #[test]
    fn remote_checks_do_not_scan_local_history_or_cap_active_authority() {
        let db = fixture();
        for (table, kind) in [
            ("tasks", RemoteTable::Tasks),
            ("plans", RemoteTable::Plans),
            ("coordinator_jobs", RemoteTable::Jobs),
        ] {
            put(&db, table, "local", "running", "main", "desktop");
            put(&db, table, "old", "completed", "main", "qq");
            for i in 0..140 {
                put(&db, table, &format!("qq-{i}"), "queued", "main", "qq");
            }
            put(&db, table, "gateway", "running", "main", "gateway");
            assert_eq!(active_remote(&db, kind, "qq").unwrap().len(), 140);
            assert_eq!(
                active_remote(&db, kind, "gateway").unwrap()[0]["id"],
                "gateway"
            );
        }
    }
    #[test]
    fn index_updates_are_atomic_and_wrong_definitions_are_rejected() {
        let mut db = fixture();
        put(&db, "tasks", "task", "completed", "main", "desktop");
        let tx = db.transaction().unwrap();
        put(&tx, "tasks", "task", "queued", "main", "desktop");
        assert_eq!(queued_tasks(&tx).unwrap().len(), 1);
        tx.rollback().unwrap();
        assert!(queued_tasks(&db).unwrap().is_empty());
        db.execute_batch("DROP INDEX scheduler_tasks_status_created; CREATE INDEX scheduler_tasks_status_created ON tasks(id)").unwrap();
        assert!(validate(&db).is_err());
    }
}
