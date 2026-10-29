//! Trusted Store shutdown transaction, not proof that any process/OS operation
//! has stopped. Integration MUST close Lifecycle admission before entering this
//! method and arm Admission::begin_intent before its durable transaction. It may
//! settle that permit only after commit is confirmed. No new task/coordinator,
//! credential mutation, probe, QQ ingest, claim or reply bridge may be admitted
//! during draining. Registration of a tracked worker must precede releasing the
//! armed admission; commit alone does not establish physical cleanup. Every
//! durable entry point MUST recheck the closed gate/StopSignal while holding
//! the same Store mutex, even if it already reserved an Admission. Otherwise a
//! pre-admitted waiter could commit a new claim AFTER this transaction. Workers
//! registered late still check task cancellation before any external effect.
//!
//! Run this promptly after closing admission, before awaiting cleanup, and
//! again after workers settle to fence any late known-retry outbox result. Pending/unknown cleanup
//! ownership is preserved for real workers/recovery; SQL never clears it. This
//! helper neither calls advance_plans nor any model/tool/network/OS-vault API.
use super::*;
use serde::Serialize;

const STOP_REASON: &str = "daemon_shutdown";
const STOP_MESSAGE: &str = "后台正在关闭，任务已停止；已有修改和执行证据保留，进程及外部操作清理结果另行确认，不自动重放。";

#[derive(Default, Serialize)]
pub(crate) struct QuiescenceReport {
    pub cancelled_tasks: usize,
    pub cancelled_plans: usize,
    pub cancelled_coordinator_jobs: usize,
    pub discarded_plan_changes: usize,
    pub stopped_deliveries: usize,
    pub rejected_remote_inputs: usize,
    pub stopped_remote_outbox: usize,
    pub suppressed_remote_notifications: usize,
    pub probes_pending: u64,
    pub probes_inflight: u64,
    pub cleanup_pending: u64,
    pub cleanup_unknown: u64,
    pub external_calls_sending: u64,
    pub external_calls_unknown: u64,
    pub credential_operations_pending: u64,
    pub credential_operations_inflight: u64,
    pub credential_operations_unknown: u64,
    pub remote_sending: u64,
    pub remote_unknown: u64,
    /// Always false: the Lifecycle/worker/process backends, not this transaction,
    /// must establish termination and successful cleanup independently.
    pub process_cleanup_confirmed: bool,
    pub events: Vec<RunEvent>,
}
impl QuiescenceReport {
    fn changed(&self) -> bool {
        self.cancelled_tasks
            + self.cancelled_plans
            + self.cancelled_coordinator_jobs
            + self.discarded_plan_changes
            + self.stopped_deliveries
            + self.rejected_remote_inputs
            + self.stopped_remote_outbox
            + self.suppressed_remote_notifications
            > 0
    }
}
fn selected(db: &Connection, sql: &str) -> Result<Vec<Value>> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|raw| Ok(serde_json::from_str(&raw?)?)).collect()
}
fn count(db: &Connection, sql: &str) -> Result<u64> {
    Ok(db.query_row(sql, [], |r| r.get(0))?)
}

impl Store {
    pub(crate) fn quiesce_for_shutdown(&mut self) -> Result<QuiescenceReport> {
        let tx = self.db.transaction()?;
        let cursor: u64 =
            tx.query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |r| {
                r.get(0)
            })?;
        let mut report = QuiescenceReport::default();
        // Freeze plans before touching tasks. A terminal plan prevents any
        // sync/late finish path from advancing dependent children or reviews.
        let plans=selected(&tx,"SELECT body FROM plans WHERE json_extract(body,'$.status') IN('running','paused','waiting_input','reviewing')")?;
        let mut cancelled_plans = HashSet::new();
        for mut plan in plans {
            cancelled_plans.insert(text(&plan, "id")?.to_owned());
            plan["status"] = json!("cancelled");
            plan["error"] = json!(STOP_MESSAGE);
            if let Some(nodes) = plan["nodes"].as_array_mut() {
                for node in nodes {
                    if matches!(
                        node["status"].as_str(),
                        Some("pending" | "queued" | "running" | "waiting_input" | "paused")
                    ) {
                        node["status"] = json!("cancelled");
                        node["error"] = json!(STOP_MESSAGE);
                    }
                }
            }
            touch_plan(&mut plan);
            put(&tx, "plans", &plan)?;
            persist_event(
                &tx,
                &plan_event(
                    &plan,
                    "plan_cancelled",
                    "后台关闭已停止计划；未调度后继节点，清理仍由原执行器确认",
                ),
            )?;
            report.cancelled_plans += 1;
        }
        let tasks = selected(&tx,"SELECT body FROM tasks WHERE json_extract(body,'$.status') IN('queued','running','waiting_input','paused')")?;
        for mut task in tasks {
            task["status"] = json!("cancelled");
            if task["stop_reason"].is_null() {
                task["stop_reason"] = json!(STOP_REASON);
            }
            task["error"] = json!(STOP_MESSAGE);
            task["waiting_request_id"] = Value::Null;
            terminal_steps(&mut task, false);
            // Reuse the actual terminal semantics: pending approvals cancelled,
            // explicit decisions remain audit-only, pending supplements become
            // not_applied and delivering ones unknown, Codex context invalidates.
            // Newly made notification intents are stopped in this SAME commit.
            complete_transaction(&tx, &mut task)?;
            report.cancelled_tasks += 1;
        }
        let jobs = selected(&tx,"SELECT body FROM coordinator_jobs WHERE json_extract(body,'$.status') IN('pending','running')")?;
        for mut job in jobs {
            job["status"] = json!("cancelled");
            job["error"] = json!(STOP_MESSAGE);
            job["updated_at"] = json!(now());
            put(&tx, "coordinator_jobs", &job)?;
            persist_event(
                &tx,
                &event(
                    job["session_id"].as_str().map(str::to_owned),
                    None,
                    "coordinator_cancelled",
                    "后台关闭，编排作业已取消；迟到结果不会生成或恢复计划",
                ),
            )?;
            report.cancelled_coordinator_jobs += 1;
        }
        let changes = selected(&tx,"SELECT body FROM plan_changes WHERE json_extract(body,'$.status') IN('queued','running','ready')")?;
        for mut change in changes {
            // Preserve non-running draft edits unless their plan was executing.
            if change["status"] == "ready"
                && !change["plan_id"]
                    .as_str()
                    .is_some_and(|id| cancelled_plans.contains(id))
            {
                continue;
            }
            change["status"] = json!("discarded");
            change["error"] = json!(STOP_MESSAGE);
            change["updated_at"] = json!(now());
            put(&tx, "plan_changes", &change)?;
            report.discarded_plan_changes += 1;
        }
        // A pending callback has not been claimed. Do not claim/send it merely
        // to report shutdown. Already sending/unknown callbacks remain factual.
        report.stopped_deliveries=tx.execute("UPDATE deliveries SET body=json_set(body,'$.status','cancelled','$.error',?1,'$.updated_at',?2) WHERE json_extract(body,'$.status')='pending'",params![STOP_MESSAGE,now()])?;
        report.cleanup_pending = count(
            &tx,
            "SELECT count(*) FROM tasks WHERE json_extract(body,'$.execution_cleanup')='pending'",
        )?;
        report.cleanup_unknown = count(
            &tx,
            "SELECT count(*) FROM tasks WHERE json_extract(body,'$.execution_cleanup')='unknown'",
        )?;
        report.external_calls_sending = count(
            &tx,
            "SELECT count(*) FROM capability_calls WHERE json_extract(body,'$.status')='sending'",
        )?;
        report.external_calls_unknown = count(
            &tx,
            "SELECT count(*) FROM capability_calls WHERE json_extract(body,'$.status')='unknown'",
        )?;
        // Secret storage intent/cleanup semantics belong to their original
        // workers. Do not cancel or relabel OS writes based on an SQL snapshot.
        for table in [
            "credential_operations",
            "mcp_credential_operations",
            "qq_credential_operations",
            "gateway_credential_operations",
        ] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |r| r.get(0),
            )?;
            if !exists {
                continue;
            } // Gateway schema is a separately integrated slice.
            report.credential_operations_pending += count(
                &tx,
                &format!(
                    "SELECT count(*) FROM {table} WHERE json_extract(body,'$.status')='pending'"
                ),
            )?;
            report.credential_operations_inflight += count(
                &tx,
                &format!(
                    "SELECT count(*) FROM {table} WHERE json_extract(body,'$.status')='inflight'"
                ),
            )?;
            report.credential_operations_unknown += count(
                &tx,
                &format!(
                    "SELECT count(*) FROM {table} WHERE json_extract(body,'$.status')='unknown'"
                ),
            )?;
        }
        // Do not execute an accepted remote command or queue a shutdown ACK.
        // Preserve nonce/attempts for already claimed sends so their ORIGINAL
        // worker can report a true result. A known 429 may return to pending;
        // integration runs this transaction again AFTER those workers settle.
        let now_ms = crate::qq::runtime::now_ms();
        loop {
            let pending = crate::qq::persistence::pending_inputs(&tx, 128)?;
            if pending.is_empty() {
                break;
            }
            for input in pending {
                crate::qq::persistence::mark_handled(
                    &tx,
                    &input.inbox_id,
                    &input.command_id,
                    json!({"command_id":input.command_id,"status":"rejected","reason":STOP_REASON,"message":"后台关闭，远端指令没有执行；不会自动重放"}),
                    now_ms,
                )?;
                report.rejected_remote_inputs += 1;
            }
        }
        report.stopped_remote_outbox=tx.execute("UPDATE qq_outbox SET status='failed',reason=?1,updated_at_ms=?2 WHERE status='pending'",params![STOP_REASON,now_ms])?;
        // Suppress not-yet-bridged coordinator completion notices on restart,
        // including already terminal jobs. Existing sent/unknown links survive.
        report.suppressed_remote_notifications=tx.execute("INSERT OR IGNORE INTO qq_notifications(logical_key,source_kind,source_id,outbox_id,state) SELECT 'coordinator:'||id,'coordinator',id,NULL,'not_queued' FROM coordinator_jobs WHERE json_extract(body,'$.kind')='message' AND json_extract(body,'$.source.channel_id')='qq' AND json_extract(body,'$.status') IN('completed','failed','cancelled')",[])?;
        // Sync only existing links; unlike bridge(), this cannot enqueue sends.
        // sync processes 128 entries at a time. Stop once all authoritative
        // outbox statuses/attempts are reflected in existing delivery records.
        while count(&tx,"SELECT count(*) FROM qq_notifications n JOIN qq_outbox o ON o.id=n.outbox_id JOIN deliveries d ON d.id=n.source_id WHERE n.source_kind='delivery' AND (n.state!=o.status OR json_extract(d.body,'$.attempts')!=o.attempts)")? > 0 {
            crate::qq::notifications::sync(&tx)?;
        }
        report.probes_pending=count(&tx,"SELECT count(*) FROM managed_capabilities WHERE json_extract(body,'$.probe_state')='pending'")?;
        report.probes_inflight=count(&tx,"SELECT count(*) FROM managed_capabilities WHERE json_extract(body,'$.probe_state')='inflight'")?;
        report.remote_sending =
            count(&tx, "SELECT count(*) FROM qq_outbox WHERE status='sending'")?;
        report.remote_unknown =
            count(&tx, "SELECT count(*) FROM qq_outbox WHERE status='unknown'")?;
        if report.changed() {
            persist_event(
                &tx,
                &event(
                    None,
                    None,
                    "daemon_quiesced",
                    "已提交后台停止调度状态；尚未声明进程或外部副作用停止",
                ),
            )?;
        }
        {
            let mut statement =
                tx.prepare("SELECT sequence,body FROM events WHERE sequence>?1 ORDER BY sequence")?;
            report.events = statement
                .query_map([cursor], |r| {
                    Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?))
                })?
                .map(|raw| {
                    let (sequence, body) = raw?;
                    let mut item: RunEvent = serde_json::from_str(&body)?;
                    item.sequence = sequence;
                    Ok(item)
                })
                .collect::<Result<Vec<_>>>()?;
        }
        tx.commit()?;
        Ok(report)
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
