use std::{path::Path, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use harness_core::{new_id, now, RunEvent};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

use crate::{event, Settings};

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS commands(id TEXT PRIMARY KEY, envelope TEXT NOT NULL, receipt TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS selections(source TEXT PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS deliveries(id TEXT PRIMARY KEY, task_id TEXT UNIQUE NOT NULL, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS settings(id TEXT PRIMARY KEY, body TEXT NOT NULL);",
        )?;
        let mut store = Self { db };
        store.recover()?;
        Ok(store)
    }

    pub fn load_settings(&self) -> Result<Settings> {
        let stored: Option<String> = self
            .db
            .query_row("SELECT body FROM settings WHERE id='provider'", [], |row| {
                row.get(0)
            })
            .optional()?;
        let mut settings = stored
            .map(|body| serde_json::from_str::<Settings>(&body))
            .transpose()?
            .unwrap_or_default();
        if let Ok(value) = std::env::var("HARNESS_BASE_URL") {
            settings.base_url = value;
        }
        if let Ok(value) = std::env::var("HARNESS_MODEL") {
            settings.model = value;
        }
        settings.api_key = std::env::var("HARNESS_API_KEY").unwrap_or_default();
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        // Settings::api_key is explicitly excluded from serialization.
        self.db.execute(
            "INSERT INTO settings(id,body) VALUES('provider',?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
            [serde_json::to_string(settings)?],
        )?;
        Ok(())
    }

    fn recover(&mut self) -> Result<()> {
        let tasks = rows(&self.db, "tasks")?;
        for task in tasks {
            if task["status"] == "running" {
                self.finish(
                    text(&task, "id")?,
                    Err(anyhow!("Daemon restarted during execution. Inspect project changes before starting a new task; execution was not replayed.")),
                )?;
            }
        }
        for mut delivery in rows(&self.db, "deliveries")? {
            if delivery["status"] == "sending" {
                delivery["status"] = json!("pending");
                put(&self.db, "deliveries", &delivery)?;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Value> {
        let mut sessions = rows(&self.db, "sessions")?;
        sessions.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
        let mut tasks = rows(&self.db, "tasks")?;
        tasks.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
        let mut events = rows(&self.db, "events")?;
        events.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()));
        let events: Vec<_> = events
            .into_iter()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let shell_enabled = std::env::var("HARNESS_ALLOW_COMMANDS").as_deref() == Ok("1");
        Ok(json!({
            "sessions": sessions,
            "tasks": tasks,
            "events": events,
            "deliveries": rows(&self.db, "deliveries")?,
            "selections": rows(&self.db, "selections")?,
            "capabilities": [
                {"id":"main", "name":"Main agent", "kind":"agent", "description":"Responsive session manager and asynchronous task dispatcher", "enabled":true},
                {"id":"native", "name":"Native coding worker", "kind":"executor", "description":"Bounded Chat Completions tool loop; configure a compatible model to execute", "enabled":true},
                {"id":"workspace", "name":"Workspace files", "kind":"tool", "description":"Read, list, search and write files within the canonical project root", "enabled":true},
                {"id":"shell", "name":"Project commands", "kind":"tool", "description":"Trusted local shell execution; enable with HARNESS_ALLOW_COMMANDS=1 (not sandboxed)", "enabled":shell_enabled},
                {"id":"callback", "name":"Local callback sink", "kind":"tool", "description":"Durable desktop and simulator callbacks without a model round trip", "enabled":true},
                {"id":"codex", "name":"Codex executor", "kind":"executor", "description":"Planned adapter; not implemented in this milestone", "enabled":false},
                {"id":"mcp", "name":"MCP connections", "kind":"mcp", "description":"Planned capability registry integration", "enabled":false},
                {"id":"skills", "name":"Local skills", "kind":"skill", "description":"Planned scoped SKILL.md discovery", "enabled":false}
            ]
        }))
    }

    pub fn project_path(&self, session_id: &str) -> Result<String> {
        let session = record(&self.db, "sessions", session_id)?;
        Ok(text(&session, "project_path")?.into())
    }

    pub fn command(
        &mut self,
        envelope: Value,
        settings: &Settings,
    ) -> Result<(Value, Option<RunEvent>)> {
        let command_id = text(&envelope, "command_id")?.to_string();
        if command_id.len() > 128 {
            bail!("command_id is too long");
        }
        let serialized = serde_json::to_string(&envelope)?;
        let existing: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT envelope, receipt FROM commands WHERE id=?1",
                [&command_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((original, receipt)) = existing {
            if original != serialized {
                bail!("command_id already belongs to a different command");
            }
            return Ok((serde_json::from_str(&receipt)?, None));
        }
        let source = envelope.get("source").context("source is required")?;
        for field in ["channel_id", "actor_id", "conversation_id"] {
            text(source, field)?;
        }
        if text(source, "actor_id")? != "owner" {
            bail!("Only the local owner identity is supported in v0");
        }
        if !matches!(text(source, "channel_id")?, "desktop" | "simulator") {
            bail!("Channel is not configured; use desktop or simulator");
        }
        let command = envelope.get("command").context("command is required")?;
        let kind = text(command, "type")?;
        let tx = self.db.transaction()?;
        let mut receipt = json!({"command_id":command_id, "status":"accepted"});
        let produced;
        match kind {
            "create_session" => {
                let title = bounded_text(command, "title", 200)?;
                let project_path = std::fs::canonicalize(text(command, "project_path")?)
                    .context("Project directory must exist")?;
                if !project_path.is_dir() {
                    bail!("Project path must be a directory");
                }
                let id = new_id();
                let session = json!({
                    "id":id, "title":title, "project_path":project_path.to_string_lossy(),
                    "created_at":now(), "updated_at":now(), "messages":[]
                });
                put(&tx, "sessions", &session)?;
                select(&tx, source, &id)?;
                receipt["session_id"] = json!(id);
                produced = Some(event(Some(id), None, "session_created", "Session created"));
            }
            "select_session" => {
                let id = text(command, "session_id")?;
                record(&tx, "sessions", id)?;
                select(&tx, source, id)?;
                receipt["session_id"] = json!(id);
                produced = Some(event(
                    Some(id.into()),
                    None,
                    "session_selected",
                    "Session selected",
                ));
            }
            "send_message" => {
                let id = resolve_session(&tx, source, command)?;
                let content = bounded_text(command, "content", 100_000)?;
                let mut session = record(&tx, "sessions", &id)?;
                append_message(&mut session, "user", content, None)?;
                let active = rows(&tx, "tasks")?
                    .iter()
                    .filter(|task| {
                        task["session_id"] == id
                            && matches!(task["status"].as_str(), Some("running" | "queued"))
                    })
                    .count();
                append_message(&mut session, "assistant",
                    &format!("Received. {active} task(s) active in this session. Use Run task to dispatch coding work; you can keep managing sessions while workers execute."),
                    None)?;
                put(&tx, "sessions", &session)?;
                receipt["session_id"] = json!(id);
                produced = Some(event(
                    Some(id),
                    None,
                    "message_received",
                    "Main agent acknowledged your message",
                ));
            }
            "start_task" => {
                let session_id = resolve_session(&tx, source, command)?;
                let title = bounded_text(command, "title", 100_000)?;
                if settings.execution_mode == "native" && !settings.configured() {
                    bail!("Configure a Chat Completions-compatible model and provider URL in Settings before running a native task, or explicitly select Demo mode");
                }
                let active = rows(&tx, "tasks")?
                    .iter()
                    .filter(|task| matches!(task["status"].as_str(), Some("running" | "queued")))
                    .count();
                if active >= 32 {
                    bail!("Task queue is full (32 active tasks); cancel or finish a task first");
                }
                let id = new_id();
                let task = json!({
                    "id":id, "session_id":session_id, "title":title,
                    "status":"queued", "execution_mode":settings.execution_mode,
                    "created_at":now(), "updated_at":now(), "source":source,
                    "steps":[
                        {"id":new_id(),"title":"Understand the objective","status":"queued"},
                        {"id":new_id(),"title":"Execute with workspace tools","status":"queued"},
                        {"id":new_id(),"title":"Report evidence and limitations","status":"queued"}
                    ]
                });
                put(&tx, "tasks", &task)?;
                let mut session = record(&tx, "sessions", &session_id)?;
                append_message(&mut session, "user", title, Some(&id))?;
                append_message(
                    &mut session,
                    "assistant",
                    "Task dispatched to the background worker. This session remains available.",
                    Some(&id),
                )?;
                put(&tx, "sessions", &session)?;
                receipt["session_id"] = json!(session_id);
                receipt["task_id"] = json!(id);
                produced = Some(event(
                    Some(session_id),
                    Some(id),
                    "task_queued",
                    "Background task accepted",
                ));
            }
            "cancel_task" => {
                let id = text(command, "task_id")?;
                let mut task = record(&tx, "tasks", id)?;
                receipt["task_id"] = json!(id);
                receipt["session_id"] = task["session_id"].clone();
                if matches!(
                    task["status"].as_str(),
                    Some("queued" | "running" | "waiting_input")
                ) {
                    task["status"] = json!("cancelled");
                    task["error"] =
                        json!("Cancelled by user. Existing workspace changes are retained.");
                    terminal_steps(&mut task, false);
                    produced = Some(complete_transaction(&tx, &mut task)?);
                } else {
                    produced = None;
                }
            }
            "retry_delivery" => {
                let id = text(command, "delivery_id")?;
                let mut delivery = record(&tx, "deliveries", id)?;
                if delivery["status"] != "sent" {
                    delivery["status"] = json!("pending");
                    delivery["updated_at"] = json!(now());
                    delivery["error"] = Value::Null;
                    put(&tx, "deliveries", &delivery)?;
                }
                produced = Some(event(
                    None,
                    None,
                    "delivery_retry",
                    "Callback retry requested",
                ));
            }
            _ => bail!("Unsupported command type: {kind}"),
        }
        if let Some(ref event) = produced {
            persist_event(&tx, event)?;
        }
        tx.execute(
            "INSERT INTO commands(id,envelope,receipt) VALUES(?1,?2,?3)",
            params![command_id, serialized, serde_json::to_string(&receipt)?],
        )?;
        tx.commit()?;
        Ok((receipt, produced))
    }

    pub fn claim_task(&mut self) -> Result<Option<Value>> {
        let tx = self.db.transaction()?;
        let mut candidates = rows(&tx, "tasks")?;
        candidates.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()));
        let Some(mut task) = candidates
            .into_iter()
            .find(|task| task["status"] == "queued")
        else {
            return Ok(None);
        };
        task["status"] = json!("running");
        task["updated_at"] = json!(now());
        task["steps"][0]["status"] = json!("running");
        put(&tx, "tasks", &task)?;
        persist_event(
            &tx,
            &event(
                Some(text(&task, "session_id")?.into()),
                Some(text(&task, "id")?.into()),
                "task_started",
                "Worker claimed task",
            ),
        )?;
        tx.commit()?;
        Ok(Some(task))
    }

    pub fn is_cancelled(&self, task_id: &str) -> Result<bool> {
        Ok(record(&self.db, "tasks", task_id)?["status"] == "cancelled")
    }

    pub fn progress(&mut self, task_id: &str, detail: &str) -> Result<RunEvent> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if task["status"] == "running" {
            task["steps"][0]["status"] = json!("completed");
            task["steps"][1]["status"] = json!("running");
            task["steps"][1]["detail"] = json!(detail);
            task["updated_at"] = json!(now());
            put(&tx, "tasks", &task)?;
        }
        let event = event(
            Some(text(&task, "session_id")?.into()),
            Some(task_id.into()),
            "task_progress",
            detail,
        );
        persist_event(&tx, &event)?;
        tx.commit()?;
        Ok(event)
    }

    pub fn finish(&mut self, task_id: &str, result: Result<String>) -> Result<Option<RunEvent>> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if matches!(
            task["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            return Ok(None);
        }
        match result {
            Ok(result) => {
                task["status"] = json!("completed");
                task["result"] = json!(result);
                terminal_steps(&mut task, true);
            }
            Err(error) => {
                task["status"] = json!("failed");
                task["error"] = json!(error.to_string());
                terminal_steps(&mut task, false);
            }
        }
        let event = complete_transaction(&tx, &mut task)?;
        tx.commit()?;
        Ok(Some(event))
    }

    pub fn deliver_one(&mut self) -> Result<Option<RunEvent>> {
        let tx = self.db.transaction()?;
        let Some(mut delivery) = rows(&tx, "deliveries")?
            .into_iter()
            .find(|delivery| delivery["status"] == "pending")
        else {
            return Ok(None);
        };
        delivery["status"] = json!("sending");
        delivery["attempts"] = json!(delivery["attempts"].as_u64().unwrap_or(0) + 1);
        put(&tx, "deliveries", &delivery)?;
        // Both built-in sinks are durable local records, so recording their receipt
        // and marking sent in one transaction is sufficient. External adapters will
        // require a lease and provider-specific delivery idempotency.
        let channel = delivery["target"]["channel_id"]
            .as_str()
            .unwrap_or_default();
        if matches!(channel, "desktop" | "simulator") {
            delivery["status"] = json!("sent");
            delivery["error"] = Value::Null;
        } else {
            delivery["status"] = json!("failed");
            delivery["error"] = json!("Channel adapter not configured");
        }
        delivery["updated_at"] = json!(now());
        put(&tx, "deliveries", &delivery)?;
        let event = event(
            Some(text(&delivery, "session_id")?.into()),
            Some(text(&delivery, "task_id")?.into()),
            if delivery["status"] == "sent" {
                "callback_delivered"
            } else {
                "callback_failed"
            },
            if delivery["status"] == "sent" {
                "Result delivered to the durable local channel sink"
            } else {
                "Callback adapter unavailable"
            },
        );
        persist_event(&tx, &event)?;
        tx.commit()?;
        Ok(Some(event))
    }
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("{field} is required and must be nonempty text"))
}

fn bounded_text<'a>(value: &'a Value, field: &str, max: usize) -> Result<&'a str> {
    let value = text(value, field)?;
    if value.len() > max {
        bail!("{field} exceeds {max} bytes");
    }
    Ok(value)
}

fn rows(db: &Connection, table: &str) -> Result<Vec<Value>> {
    let mut statement = db.prepare(&format!("SELECT body FROM {table} ORDER BY rowid"))?;
    let values = statement.query_map([], |row| row.get::<_, String>(0))?;
    values
        .map(|value| Ok(serde_json::from_str(&value?)?))
        .collect()
}

fn record(db: &Connection, table: &str, id: &str) -> Result<Value> {
    let body = db
        .query_row(
            &format!("SELECT body FROM {table} WHERE id=?1"),
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("Unknown {}: {id}", table.trim_end_matches('s')))?;
    Ok(serde_json::from_str(&body)?)
}

fn put(db: &Connection, table: &str, value: &Value) -> Result<()> {
    let id = text(value, "id")?;
    let body = serde_json::to_string(value)?;
    if table == "deliveries" {
        db.execute(
            "INSERT INTO deliveries(id,task_id,body) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
            params![id, text(value, "task_id")?, body],
        )?;
    } else {
        db.execute(&format!("INSERT INTO {table}(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body"),
            params![id, body])?;
    }
    Ok(())
}

fn persist_event(db: &Connection, event: &RunEvent) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO events(id,body) VALUES(?1,?2)",
        params![event.id, serde_json::to_string(event)?],
    )?;
    Ok(())
}

fn select(db: &Connection, source: &Value, session_id: &str) -> Result<()> {
    db.execute("INSERT INTO selections(source,body) VALUES(?1,?2) ON CONFLICT(source) DO UPDATE SET body=excluded.body",
        params![serde_json::to_string(source)?, serde_json::to_string(&json!({"source":source,"session_id":session_id}))?])?;
    Ok(())
}

fn resolve_session(db: &Connection, source: &Value, command: &Value) -> Result<String> {
    if let Some(id) = command.get("session_id").and_then(Value::as_str) {
        record(db, "sessions", id)?;
        return Ok(id.into());
    }
    let selection = db
        .query_row(
            "SELECT body FROM selections WHERE source=?1",
            [serde_json::to_string(source)?],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .context("Select a session for this source or specify session_id")?;
    let selection: Value = serde_json::from_str(&selection)?;
    Ok(text(&selection, "session_id")?.into())
}

fn append_message(
    session: &mut Value,
    role: &str,
    content: &str,
    task_id: Option<&str>,
) -> Result<()> {
    session["messages"]
        .as_array_mut()
        .context("Invalid session messages")?
        .push(json!({
            "id":new_id(),"role":role,"content":content,"created_at":now(),"task_id":task_id
        }));
    session["updated_at"] = json!(now());
    Ok(())
}

fn terminal_steps(task: &mut Value, success: bool) {
    let terminal = if success {
        "completed"
    } else if task["status"] == "cancelled" {
        "cancelled"
    } else {
        "failed"
    };
    if let Some(steps) = task["steps"].as_array_mut() {
        for step in steps {
            if step["status"] != "completed" {
                step["status"] = json!(terminal);
            }
        }
    }
}

fn complete_transaction(tx: &Transaction<'_>, task: &mut Value) -> Result<RunEvent> {
    task["updated_at"] = json!(now());
    put(tx, "tasks", task)?;
    let task_id = text(task, "id")?;
    let session_id = text(task, "session_id")?;
    let status = text(task, "status")?;
    let result = task["result"]
        .as_str()
        .or_else(|| task["error"].as_str())
        .unwrap_or("Task finished");
    let event = event(
        Some(session_id.into()),
        Some(task_id.into()),
        &format!("task_{status}"),
        result,
    );
    persist_event(tx, &event)?;
    let payload = format!("Task {task_id} in session {session_id}: {status}\n{result}");
    let delivery = json!({
        "id":new_id(), "task_id":task_id,"session_id":session_id,"target":task["source"],
        "status":"pending","payload":payload,"attempts":0,"created_at":now(),"updated_at":now()
    });
    put(tx, "deliveries", &delivery)?;
    let mut session = record(tx, "sessions", session_id)?;
    append_message(&mut session, "assistant", result, Some(task_id))?;
    put(tx, "sessions", &session)?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> Value {
        json!({"channel_id":"simulator","actor_id":"owner","conversation_id":"remote-1"})
    }
    fn config() -> Settings {
        Settings {
            execution_mode: "demo".into(),
            ..Settings::default()
        }
    }
    fn send(store: &mut Store, command: Value) -> Value {
        store
            .command(
                json!({"command_id":new_id(),"source":source(),"command":command}),
                &config(),
            )
            .unwrap()
            .0
    }
    fn session(store: &mut Store) -> String {
        send(
            store,
            json!({"type":"create_session","title":"test","project_path":std::env::temp_dir()}),
        )["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    #[test]
    fn command_dedup_is_durable_and_conflicts_are_rejected() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let command = json!({"command_id":"stable", "source":source(),
            "command":{"type":"create_session","title":"test","project_path":std::env::temp_dir()}});
        let first = store.command(command.clone(), &config()).unwrap();
        let second = store.command(command.clone(), &config()).unwrap();
        assert_eq!(first.0, second.0);
        assert!(second.1.is_none());
        assert_eq!(
            store.snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut conflict = command;
        conflict["command"]["title"] = json!("other");
        assert!(store.command(conflict, &config()).is_err());
    }
    #[test]
    fn callback_target_sticks_to_dispatch_and_terminal_transition_is_atomic() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let a = session(&mut store);
        let task = send(&mut store, json!({"type":"start_task","title":"work"}))["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        let b = session(&mut store);
        assert_ne!(a, b);
        store.claim_task().unwrap().unwrap();
        store.finish(&task, Ok("done".into())).unwrap();
        assert!(store
            .finish(&task, Ok("duplicate".into()))
            .unwrap()
            .is_none());
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot["deliveries"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["deliveries"][0]["session_id"], a);
        assert_eq!(snapshot["deliveries"][0]["target"], source());
        assert_eq!(snapshot["tasks"][0]["status"], "completed");
        assert_eq!(
            snapshot["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["kind"] == "task_completed")
                .count(),
            1
        );
        store.deliver_one().unwrap();
        assert_eq!(store.snapshot().unwrap()["deliveries"][0]["status"], "sent");
    }
    #[test]
    fn outbox_survives_restart_without_repeating_completed_work() {
        let dir = std::env::temp_dir().join(new_id());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        {
            let mut store = Store::open(&path).unwrap();
            session(&mut store);
            let task = send(&mut store, json!({"type":"start_task","title":"work"}))["task_id"]
                .as_str()
                .unwrap()
                .to_string();
            store.claim_task().unwrap();
            store.finish(&task, Ok("done".into())).unwrap();
        }
        {
            let mut store = Store::open(&path).unwrap();
            assert!(store.claim_task().unwrap().is_none());
            assert_eq!(
                store.snapshot().unwrap()["deliveries"][0]["status"],
                "pending"
            );
            store.deliver_one().unwrap();
            assert_eq!(store.snapshot().unwrap()["deliveries"][0]["status"], "sent");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn outbox_insert_failure_rolls_back_terminal_task_and_event() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        session(&mut store);
        let task = send(&mut store, json!({"type":"start_task","title":"work"}))["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        store.claim_task().unwrap();
        store.db.execute_batch(
            "CREATE TRIGGER fail_outbox BEFORE INSERT ON deliveries BEGIN SELECT RAISE(ABORT,'simulated storage failure'); END;"
        ).unwrap();
        assert!(store.finish(&task, Ok("done".into())).is_err());
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot["tasks"][0]["status"], "running");
        assert!(snapshot["deliveries"].as_array().unwrap().is_empty());
        assert!(!snapshot["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "task_completed"));
        store.db.execute_batch("DROP TRIGGER fail_outbox;").unwrap();
        store.finish(&task, Ok("done".into())).unwrap();
        assert_eq!(
            store.snapshot().unwrap()["deliveries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn provider_preferences_persist_without_api_key() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let settings = Settings {
            base_url: "http://localhost:1234/v1".into(),
            model: "test-model".into(),
            execution_mode: "demo".into(),
            api_key: "never-store-this-secret".into(),
        };
        store.save_settings(&settings).unwrap();
        let body: String = store
            .db
            .query_row("SELECT body FROM settings WHERE id='provider'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!body.contains("never-store-this-secret"));
        assert!(!body.contains("api_key"));
        let decoded: Settings = serde_json::from_str(&body).unwrap();
        assert_eq!(decoded.model, "test-model");
        assert!(decoded.api_key.is_empty());
    }
}
