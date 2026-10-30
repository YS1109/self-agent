//! Atomic restricted command preparation. The main Store supplies its private
//! transaction dispatcher, never its authenticated owner HTTP entry. All errors
//! roll back the caller transaction; rejection is recorded in a separate one.
use super::{
    commands::{self, Prepared, RemoteContext},
    configuration,
    persistence::{self, Binding, ConversationKind, InboxEntry, OutboxKind},
    policy::{self, RemoteGrant},
};
use anyhow::{bail, Context, Result};
use harness_core::RunEvent;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS qq_selections(scope TEXT PRIMARY KEY,session_id TEXT NOT NULL);")?;
    Ok(())
}

/// The dispatcher must clamp *all* model/role/task snapshots with grant.controls
/// and use decision_actor instead of local_owner. For create_session it records
/// the exact already-authorized project root, without ambient re-canonicalizing.
pub(crate) struct Execution<'a> {
    pub envelope: Value,
    pub grant: &'a RemoteGrant,
    pub decision_actor: String,
    pub copied_project_root: Option<String>,
}

pub(crate) struct Processed {
    pub event: Option<RunEvent>,
}

pub(crate) fn next(db: &Connection) -> Result<Option<InboxEntry>> {
    Ok(persistence::pending_inputs(db, 1)?.into_iter().next())
}

pub(crate) fn process(
    tx: &Transaction<'_>,
    config_id: &str,
    config_revision: u64,
    now: u64,
    execute: impl FnOnce(&Transaction<'_>, Execution<'_>) -> Result<(Value, Option<RunEvent>)>,
) -> Result<Option<Processed>> {
    let config = configuration::load(tx, config_id, config_revision)?;
    if !config.enabled {
        bail!("QQ 配置已停用");
    }
    let Some(input) = next(tx)? else {
        return Ok(None);
    };
    let processed = Processed { event: None };
    if input.identity.app_id != config.app_id {
        reject(tx, &input, now)?;
        return Ok(Some(processed));
    }
    if input.canonical_input["kind"] == "pairing" {
        persistence::redeem_pairing_input(tx, &input.inbox_id, now)?;
        return Ok(Some(processed));
    }
    // Unknown identities are not replied to, and a later pairing cannot upgrade
    // their old messages. Expired/withdrawn inputs receive no proactive fallback.
    let binding = match persistence::guard_input(tx, &input.inbox_id, now) {
        Ok(value) => value,
        Err(_) => {
            reject(tx, &input, now)?;
            return Ok(Some(processed));
        }
    };
    let route_id = input.route_id.as_deref().context("QQ 输入来源缺失")?;
    configuration::live_task_route(tx, route_id, now)?;
    let grant: RemoteGrant = serde_json::from_value(binding.grant.clone())?;
    grant.validate(now)?;
    let content = input.canonical_input["content"]
        .as_str()
        .context("QQ 指令不受支持")?;
    let command = policy::parse(content)?;
    let context = SqlContext {
        db: tx,
        binding: &binding,
        now,
    };
    let prepared = commands::prepare(
        &context,
        &grant,
        input.identity.kind == ConversationKind::Group,
        &command,
    )?;
    let (receipt, reply, event) = match prepared {
        Prepared::Reply(reply) => (
            json!({"command_id":input.command_id,"status":"completed"}),
            reply,
            None,
        ),
        Prepared::Execute {
            command,
            select_receipt,
            grant_created_session,
        } => {
            let copied_project_root = grant_created_session
                .then(|| command["project_path"].as_str().map(str::to_owned))
                .flatten();
            let envelope = json!({"command_id":input.command_id,"source":{"channel_id":"qq","actor_id":binding.principal_id,"conversation_id":route_id},"command":command});
            let (receipt, event) = execute(
                tx,
                Execution {
                    envelope,
                    grant: &grant,
                    decision_actor: format!("qq:{}", binding.principal_id),
                    copied_project_root,
                },
            )?;
            if receipt["command_id"] != input.command_id {
                bail!("QQ 指令回执标识不匹配");
            }
            if select_receipt {
                let id = receipt["session_id"]
                    .as_str()
                    .context("QQ 会话指令回执缺少会话")?;
                policy::validate_id(id)?;
                if grant_created_session {
                    persistence::grant_session(tx, &binding.principal_id, id, now)?;
                }
                if !persistence::allowed_session(tx, &binding.principal_id, id, now)? {
                    bail!("QQ 选择的会话不在授权范围");
                }
                tx.execute("INSERT INTO qq_selections(scope,session_id) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET session_id=excluded.session_id",params![persistence::selection_key(&binding)?,id])?;
            }
            let reply = receipt_reply(&receipt);
            (receipt, reply, event)
        }
    };
    persistence::enqueue_outbox(
        tx,
        route_id,
        &format!("ack:{}", input.inbox_id),
        OutboxKind::Ack,
        &reply,
        now,
    )?;
    persistence::mark_handled(tx, &input.inbox_id, &input.command_id, receipt, now)?;
    Ok(Some(Processed { event }))
}

/// Call in a NEW transaction after process failed and its entire transaction was
/// rolled back. Already-committed input is left intact after an uncertain commit.
pub(crate) fn reject_pending(tx: &Transaction<'_>, inbox_id: &str, now: u64) -> Result<()> {
    let input = persistence::inbox(tx, inbox_id)?;
    if input.status != "pending" {
        return Ok(());
    }
    reject(tx, &input, now)
}

fn reject(tx: &Transaction<'_>, input: &InboxEntry, now: u64) -> Result<()> {
    // Do not retain/reflect raw engine errors, project paths or supplied content.
    let mut receipt = json!({"command_id":input.command_id,"status":"rejected","reason":"scope_or_state_unavailable"});
    if let Some(route_id) = &input.route_id {
        if configuration::live_task_route(tx, route_id, now).is_ok()
            && persistence::guard_route(tx, route_id, now).is_ok()
        {
            // A full outbox must not leave rejected input pending forever. The
            // OPTIONAL notification is its own rollback boundary; never catch
            // a partially written enqueue then commit a consumed msg_seq.
            tx.execute_batch("SAVEPOINT qq_rejection_reply")?;
            let notification = persistence::enqueue_outbox(
                tx,
                route_id,
                &format!("ack:{}", input.inbox_id),
                OutboxKind::Ack,
                "这条指令未执行：授权、修订、期限或当前状态不允许。可发送 /帮助，或在本机核对。",
                now,
            );
            if notification.is_err() {
                tx.execute_batch("ROLLBACK TO qq_rejection_reply")?;
                receipt["notification"] = json!("not_queued");
            }
            tx.execute_batch("RELEASE qq_rejection_reply")?;
        }
    }
    persistence::mark_handled(tx, &input.inbox_id, &input.command_id, receipt, now)
}

fn receipt_reply(receipt: &Value) -> String {
    let mut lines = vec!["指令已接受；任务执行与消息送达结果分别记录。".to_owned()];
    for (key, label) in [
        ("session_id", "会话"),
        ("task_id", "任务"),
        ("plan_id", "计划"),
        ("job_id", "编排作业"),
    ] {
        if let Some(id) = receipt[key]
            .as_str()
            .filter(|id| policy::validate_id(id).is_ok())
        {
            lines.push(format!("{label}：{id}"));
        }
    }
    lines.join("\n")
}

struct SqlContext<'a> {
    db: &'a Connection,
    binding: &'a Binding,
    now: u64,
}
impl RemoteContext for SqlContext<'_> {
    fn selected_session(&self) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT session_id FROM qq_selections WHERE scope=?1",
                [persistence::selection_key(self.binding)?],
                |r| r.get(0),
            )
            .optional()?)
    }
    fn session_allowed(&self, id: &str) -> Result<bool> {
        persistence::allowed_session(self.db, &self.binding.principal_id, id, self.now)
    }
    fn session(&self, id: &str) -> Result<Value> {
        record(self.db, "sessions", id)
    }
    fn sessions(&self) -> Result<Vec<Value>> {
        let mut ids: Vec<String> = self.binding.grant["session_ids"]
            .as_array()
            .context("QQ 授权会话列表无效")?
            .iter()
            .map(|id| id.as_str().context("QQ 授权会话无效").map(str::to_owned))
            .collect::<Result<_>>()?;
        let mut statement = self
            .db
            .prepare("SELECT session_id FROM qq_session_acl WHERE principal_id=?1 LIMIT 32")?;
        for id in statement.query_map([&self.binding.principal_id], |r| r.get::<_, String>(0))? {
            let id = id?;
            if !ids.contains(&id) {
                ids.push(id)
            }
        }
        if ids.len() > 32 {
            bail!("QQ 会话范围超过上限")
        }
        let mut result = Vec::new();
        for id in ids {
            if let Ok(session) = self.session(&id) {
                result.push(session)
            }
        }
        Ok(result)
    }
    fn task(&self, id: &str) -> Result<Value> {
        record(self.db, "tasks", id)
    }
    fn recent_tasks(&self, session_id: &str) -> Result<Vec<Value>> {
        query(self.db,"SELECT body FROM tasks WHERE json_extract(body,'$.session_id')=?1 ORDER BY rowid DESC LIMIT 5",session_id)
    }
    fn plan(&self, id: &str) -> Result<Value> {
        record(self.db, "plans", id)
    }
    fn recent_plans(&self, session_id: &str) -> Result<Vec<Value>> {
        query(self.db,"SELECT body FROM plans WHERE json_extract(body,'$.session_id')=?1 ORDER BY rowid DESC LIMIT 5",session_id)
    }
    fn request(&self, id: &str) -> Result<Value> {
        record(self.db, "execution_requests", id)
    }
    fn requests(&self, task_id: &str) -> Result<Vec<Value>> {
        query(
            self.db,
            "SELECT body FROM execution_requests WHERE task_id=?1 ORDER BY rowid DESC LIMIT 64",
            task_id,
        )
    }
    fn owns_work(&self, record: &Value) -> Result<bool> {
        let source = &record["source"];
        if source["channel_id"] != "qq" || source["actor_id"] != self.binding.principal_id {
            return Ok(false);
        }
        let Some(route) = source["conversation_id"].as_str() else {
            return Ok(false);
        };
        let Ok(route) = configuration::live_task_route(self.db, route, self.now) else {
            return Ok(false);
        };
        Ok(route.principal_id == self.binding.principal_id
            && route.binding_revision == self.binding.revision
            && route.identity == self.binding.identity)
    }
}
fn record(db: &Connection, table: &str, id: &str) -> Result<Value> {
    // `table` is a private fixed literal selected above, never remote text.
    let body: String = db
        .query_row(
            &format!("SELECT body FROM {table} WHERE id=?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?
        .context("QQ 指令关联记录不存在")?;
    Ok(serde_json::from_str(&body)?)
}
fn query(db: &Connection, sql: &str, id: &str) -> Result<Vec<Value>> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map([id], |r| r.get::<_, String>(0))?;
    rows.map(|body| Ok(serde_json::from_str(&body?)?)).collect()
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
