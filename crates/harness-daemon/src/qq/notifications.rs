//! Local delivery projections into QQ's durable passive outbox. Task outcome is
//! never rewritten because a notification expired, failed or became unknown.
use super::{
    configuration,
    persistence::{self, OutboxKind},
    presentation,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS qq_notifications(logical_key TEXT PRIMARY KEY,source_kind TEXT NOT NULL,source_id TEXT NOT NULL,outbox_id TEXT,state TEXT NOT NULL);")?;
    Ok(())
}

/// Call before claim_outbox, in the same short transaction. Main deliver_one
/// must leave QQ sources to this adapter rather than treating them as a local
/// sink. Only main/parent completions notify; worker nodes never consume quota.
pub(crate) fn bridge(tx: &Transaction<'_>, now: u64) -> Result<()> {
    let deliveries=rows(tx,"SELECT body FROM deliveries WHERE json_extract(body,'$.status')='pending' AND json_extract(body,'$.target.channel_id')='qq' ORDER BY rowid LIMIT 128")?;
    for mut delivery in deliveries {
        let task_id = delivery["task_id"].as_str().context("QQ 通知任务缺失")?;
        let task = get(tx, "tasks", task_id)?;
        let key = format!("completion:{task_id}");
        let outbox = if matches!(task["task_kind"].as_str(), Some("main" | "plan"))
            && matches!(
                task["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            )
            && task["source"] == delivery["target"]
            && task["session_id"] == delivery["session_id"]
        {
            queue(
                tx,
                &key,
                "delivery",
                delivery["id"].as_str().context("QQ 回调标识缺失")?,
                &delivery["target"],
                &presentation::task_status(&task),
                now,
            )?
        } else {
            None
        };
        delivery["status"] =
            json!(outbox
                .as_ref()
                .map_or("not_notified", |(_, state)| if state == "pending" {
                    "queued_remote"
                } else {
                    state
                }));
        delivery["error"] = if outbox.is_some() {
            Value::Null
        } else {
            json!("未发送 QQ 通知；子任务不单独通知，或固定来源/配额不可用")
        };
        delivery["updated_at"] = json!(harness_core::now());
        save_delivery(tx, &delivery)?;
    }
    // Draft generation can finish without creating a task. It needs its own
    // status-only notification; no assistant body, project path or raw error is
    // copied. /计划 without ID recovers drafts after this reply window expires.
    let jobs=rows(tx,"SELECT j.body FROM coordinator_jobs j LEFT JOIN qq_notifications n ON n.logical_key='coordinator:'||j.id WHERE n.logical_key IS NULL AND json_extract(j.body,'$.kind')='message' AND json_extract(j.body,'$.source.channel_id')='qq' AND json_extract(j.body,'$.status') IN('completed','failed','cancelled') ORDER BY j.rowid LIMIT 128")?;
    for job in jobs {
        let id = job["id"].as_str().context("QQ 编排作业标识缺失")?;
        let source = &job["source"];
        let mut reply = format!(
            "编排作业 {id}\n{}",
            if job["status"] == "completed" {
                "处理已完成；没有自动批准或启动计划。"
            } else {
                "处理未完成；具体错误保留在本机。"
            }
        );
        if let (Some(route), Some(session)) = (
            source["conversation_id"].as_str(),
            job["session_id"].as_str(),
        ) {
            let body:Option<String>=tx.query_row("SELECT body FROM plans WHERE json_extract(body,'$.source.channel_id')='qq' AND json_extract(body,'$.source.conversation_id')=?1 AND json_extract(body,'$.session_id')=?2 AND json_extract(body,'$.source.actor_id')=?3 ORDER BY rowid DESC LIMIT 1",params![route,session,source["actor_id"].as_str().unwrap_or("")],|r|r.get(0)).optional()?;
            if let Some(body) = body {
                reply.push_str(&format!(
                    "\n{}",
                    presentation::plan_status(&serde_json::from_str(&body)?)
                ));
            }
        }
        reply.push_str("\n可发送 /计划 查询当前会话的计划 ID 与状态。");
        queue(
            tx,
            &format!("coordinator:{id}"),
            "coordinator",
            id,
            source,
            &reply,
            now,
        )?;
    }
    sync(tx)
}

fn queue(
    tx: &Transaction<'_>,
    key: &str,
    kind: &str,
    id: &str,
    source: &Value,
    payload: &str,
    now: u64,
) -> Result<Option<(String, String)>> {
    let existing: Option<(Option<String>, String)> = tx
        .query_row(
            "SELECT outbox_id,state FROM qq_notifications WHERE logical_key=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((id, state)) = existing {
        return Ok(id.map(|id| (id, state)));
    }
    let route_id = source["conversation_id"].as_str().unwrap_or("");
    let authorized = configuration::live_task_route(tx, route_id, now).is_ok_and(|route| {
        source["channel_id"] == "qq" && source["actor_id"] == route.principal_id
    });
    let mut outbox = None;
    if authorized {
        // A notification failure is not task failure. Roll back its whole write
        // set before persisting a local not_queued marker, including msg_seq.
        tx.execute_batch("SAVEPOINT qq_terminal_notification")?;
        match persistence::enqueue_outbox(tx, route_id, key, OutboxKind::Completion, payload, now) {
            Ok(record) => outbox = Some((record.id, record.status)),
            Err(_) => tx.execute_batch("ROLLBACK TO qq_terminal_notification")?,
        }
        tx.execute_batch("RELEASE qq_terminal_notification")?;
    }
    tx.execute("INSERT INTO qq_notifications(logical_key,source_kind,source_id,outbox_id,state) VALUES(?1,?2,?3,?4,?5)",params![key,kind,id,outbox.as_ref().map(|v|&v.0),outbox.as_ref().map_or("not_queued",|v|v.1.as_str())])?;
    Ok(outbox)
}

/// Run after outbox finalization as well as before claiming. A failed local
/// finalization can be recovered later from the authoritative outbox state.
pub(crate) fn sync(tx: &Transaction<'_>) -> Result<()> {
    let links = {
        let mut stmt=tx.prepare("SELECT n.logical_key,n.source_id,o.id FROM qq_notifications n JOIN qq_outbox o ON o.id=n.outbox_id JOIN deliveries d ON d.id=n.source_id WHERE n.source_kind='delivery' AND (n.state!=o.status OR json_extract(d.body,'$.attempts')!=o.attempts) LIMIT 128")?;
        let values = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        values.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (key, delivery_id, outbox_id) in links {
        let record = persistence::outbox(tx, &outbox_id)?;
        let mut delivery = get(tx, "deliveries", &delivery_id)?;
        delivery["status"] = json!(if record.status == "pending" {
            "queued_remote"
        } else {
            &record.status
        });
        delivery["attempts"] = json!(record.attempts);
        delivery["error"] = json!(match record.status.as_str() {
            "unknown" => Some("QQ 投递结果未知，不自动重发"),
            "expired" => Some("QQ 被动回复窗口已过期；任务结果仍在本机"),
            "revoked" => Some("QQ 来源授权已撤销"),
            "failed" => Some("QQ 通知未确认送达，请在本机查看投递记录"),
            _ => None,
        });
        delivery["updated_at"] = json!(harness_core::now());
        save_delivery(tx, &delivery)?;
        tx.execute(
            "UPDATE qq_notifications SET state=?1 WHERE logical_key=?2",
            params![record.status, key],
        )?;
    }
    Ok(())
}
fn rows(db: &Connection, sql: &str) -> Result<Vec<Value>> {
    let mut statement = db.prepare(sql)?;
    let values = statement.query_map([], |r| r.get::<_, String>(0))?;
    values.map(|v| Ok(serde_json::from_str(&v?)?)).collect()
}
fn get(db: &Connection, table: &str, id: &str) -> Result<Value> {
    let body: String = db.query_row(
        &format!("SELECT body FROM {table} WHERE id=?1"),
        [id],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&body)?)
}
fn save_delivery(tx: &Transaction<'_>, value: &Value) -> Result<()> {
    tx.execute(
        "UPDATE deliveries SET body=?1 WHERE id=?2",
        params![
            serde_json::to_string(value)?,
            value["id"].as_str().context("QQ 回调标识缺失")?
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "notifications_tests.rs"]
mod tests;
