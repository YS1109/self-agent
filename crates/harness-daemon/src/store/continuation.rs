use super::*;
use crate::{
    codex::CodexLifecycle,
    continuation::{fingerprint, NativeSummary},
};

fn context_id(session_id: &str, executor: &str) -> String {
    format!("{session_id}:{executor}")
}

fn context_record(db: &Connection, session_id: &str, executor: &str) -> Result<Option<Value>> {
    let body: Option<String> = db
        .query_row(
            "SELECT body FROM session_contexts WHERE id=?1",
            [context_id(session_id, executor)],
            |row| row.get(0),
        )
        .optional()?;
    body.map(|body| serde_json::from_str(&body).map_err(Into::into))
        .transpose()
}

fn active_main(db: &Connection, task_id: &str) -> Result<Value> {
    let task = record(db, "tasks", task_id)?;
    if task["task_kind"] != "main" || task["status"] != "running" {
        bail!("只有正在执行的主任务可以更新会话续接状态");
    }
    Ok(task)
}

fn invalidate_other(db: &Connection, session_id: &str, executor: &str) -> Result<()> {
    let other = if executor == "native" {
        "codex"
    } else {
        "native"
    };
    if let Some(mut state) = context_record(db, session_id, other)? {
        state["state"] = json!("invalidated");
        state["reason"] = json!("已切换执行器，下一次将依据完整应用历史重新建立上下文");
        state["updated_at"] = json!(now());
        put(db, "session_contexts", &state)?;
    }
    Ok(())
}

impl Store {
    pub(super) fn public_context_states(&self) -> Result<Vec<Value>> {
        // No summary text, provider/config digest, local paths or credentials.
        Ok(rows(&self.db, "session_contexts")?.into_iter().map(|state| json!({
            "session_id":state["session_id"], "executor":state["executor"],
            "state":state["state"], "reason":state["reason"],
            "covered_messages":state["summary"]["covered_messages"],
            "source_task_id":if state["executor"] == "native" { &state["summary_source_task_id"] } else { &state["task_id"] },
            "updated_at":state["updated_at"],
        })).collect())
    }

    pub(crate) fn prepare_native_summary(
        &mut self,
        task_id: &str,
        config: &str,
    ) -> Result<Option<NativeSummary>> {
        let history = self.execution_history(task_id)?;
        let tx = self.db.transaction()?;
        let task = active_main(&tx, task_id)?;
        let session_id = text(&task, "session_id")?;
        invalidate_other(&tx, session_id, "native")?;
        let mut state = context_record(&tx, session_id, "native")?.unwrap_or_else(|| json!({
            "id":context_id(session_id,"native"),"session_id":session_id,"executor":"native", "state":"empty"
        }));
        let cached = if state["state"] == "available" && state["summary_config"] == config {
            serde_json::from_value::<NativeSummary>(state["summary"].clone())
                .ok()
                .filter(|summary| {
                    summary.covered_messages > 0
                        && summary.covered_messages <= history.len()
                        && !summary.summary.trim().is_empty()
                        && summary.summary.len() <= 64 * 1024
                        && summary.prefix_fingerprint
                            == fingerprint(&json!(&history[..summary.covered_messages]))
                })
        } else {
            None
        };
        if cached.is_none() && state.get("summary").is_some() {
            state["state"] = json!("invalidated");
            state["reason"] = json!("模型、服务、项目、策略或历史边界已改变，保留原始历史重新处理");
        }
        state["config"] = json!(config);
        state["task_id"] = json!(task_id);
        state["updated_at"] = json!(now());
        put(&tx, "session_contexts", &state)?;
        tx.commit()?;
        Ok(cached)
    }

    pub(crate) fn save_native_summary(
        &mut self,
        task_id: &str,
        config: &str,
        summary: NativeSummary,
    ) -> Result<()> {
        let history = self.execution_history(task_id)?;
        if summary.covered_messages == 0
            || summary.covered_messages != history.len()
            || summary.summary.trim().is_empty()
            || summary.summary.len() > 64 * 1024
            || summary.prefix_fingerprint != fingerprint(&json!(history))
        {
            bail!("摘要覆盖边界无效，保留原始历史");
        }
        let tx = self.db.transaction()?;
        let task = active_main(&tx, task_id)?;
        let session_id = text(&task, "session_id")?;
        let mut state = context_record(&tx, session_id, "native")?.context("摘要上下文尚未准备")?;
        if state["task_id"] != task_id || state["config"] != config {
            bail!("摘要已过期，未替换当前上下文");
        }
        state["summary"] = json!(summary);
        state["summary_config"] = json!(config);
        state["summary_source_task_id"] = json!(task_id);
        state["summary_created_at"] = json!(now());
        state["state"] = json!("available");
        state["reason"] = json!("已保存较早对话摘要；原始历史和新增消息继续保留");
        state["updated_at"] = json!(now());
        put(&tx, "session_contexts", &state)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn prepare_codex_continuation(
        &mut self,
        task_id: &str,
        config: &str,
    ) -> Result<Option<(String, String)>> {
        let history = self.execution_history(task_id)?;
        let tx = self.db.transaction()?;
        let task = active_main(&tx, task_id)?;
        let session_id = text(&task, "session_id")?;
        invalidate_other(&tx, session_id, "codex")?;
        let mut state = context_record(&tx, session_id, "codex")?.unwrap_or_else(|| json!({
            "id":context_id(session_id,"codex"),"session_id":session_id,"executor":"codex","state":"empty"
        }));
        // Resume only when the app transcript is exactly the previous thread's
        // known input plus its successful result. Unrelated discussion or an
        // executor switch forces a fresh history bootstrap instead of omitting it.
        let continuous = if let Some(previous_id) = state["task_id"].as_str() {
            if let Ok(previous) = record(&tx, "tasks", previous_id) {
                let len = history.len();
                len >= 2
                    && previous["status"] == "completed"
                    && previous["main_sequence"].as_u64().map(|n| n + 1)
                        == task["main_sequence"].as_u64()
                    && history[len - 2] == json!({"role":"user","content":previous["objective"]})
                    && history[len - 1] == json!({"role":"assistant","content":previous["result"]})
                    && state["history_fingerprint"] == fingerprint(&json!(&history[..len - 2]))
            } else {
                false
            }
        } else {
            false
        };
        let thread = if state["state"] == "idle" && state["config"] == config && continuous {
            state["thread_id"]
                .as_str()
                .zip(state["turn_id"].as_str())
                .filter(|(thread, turn)| !thread.is_empty() && !turn.is_empty())
                .map(|(thread, turn)| (thread.to_owned(), turn.to_owned()))
        } else {
            None
        };
        state["reason"] = json!(if thread.is_some() {
            "已准备恢复已完成的 Codex 会话"
        } else {
            "未找到可安全复用的会话，或有效配置/历史已改变；将新建并导入应用历史一次"
        });
        if thread.is_none() {
            state["thread_id"] = Value::Null;
        }
        state["config"] = json!(config);
        state["task_id"] = json!(task_id);
        state["state"] = json!("ready");
        state["turn_id"] = Value::Null;
        state["history_fingerprint"] = json!(fingerprint(&json!(history)));
        state["updated_at"] = json!(now());
        put(&tx, "session_contexts", &state)?;
        tx.commit()?;
        Ok(thread)
    }

    pub(crate) fn codex_lifecycle(&mut self, task_id: &str, update: CodexLifecycle) -> Result<()> {
        let tx = self.db.transaction()?;
        let task = active_main(&tx, task_id)?;
        let session_id = text(&task, "session_id")?;
        let mut state =
            context_record(&tx, session_id, "codex")?.context("Codex 续接状态尚未准备")?;
        if state["task_id"] != task_id {
            bail!("Codex 回合关联已过期");
        }
        match &update {
            CodexLifecycle::ThreadReady { .. } | CodexLifecycle::TurnStarting { .. }
                if state["state"] != "ready" =>
            {
                bail!("Codex 会话状态不允许开始新回合")
            }
            CodexLifecycle::TurnStarted { .. } if state["state"] != "inflight" => {
                bail!("Codex 回合尚未登记启动")
            }
            CodexLifecycle::TurnCompleted { turn_id, .. }
                if state["state"] != "inflight" || state["turn_id"] != *turn_id =>
            {
                bail!("Codex 终态与在途回合不一致")
            }
            _ => {}
        }
        let (thread_id, turn_id, stage) = match update {
            CodexLifecycle::ThreadReady { thread_id } => (thread_id, None, "ready"),
            CodexLifecycle::TurnStarting { thread_id } => (thread_id, None, "inflight"),
            CodexLifecycle::TurnStarted { thread_id, turn_id } => {
                (thread_id, Some(turn_id), "inflight")
            }
            CodexLifecycle::TurnCompleted { thread_id, turn_id } => {
                (thread_id, Some(turn_id), "idle")
            }
        };
        if thread_id.is_empty()
            || thread_id.len() > 256
            || turn_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 256)
        {
            bail!("Codex 会话或回合标识无效");
        }
        if stage != "ready" && state["thread_id"] != thread_id {
            bail!("Codex 会话关联不一致");
        }
        state["thread_id"] = json!(thread_id);
        if let Some(turn_id) = turn_id {
            state["turn_id"] = json!(turn_id);
        }
        state["state"] = json!(stage);
        state["reason"] = json!(match stage {
            "idle" => "回合已明确完成，可在后续任务恢复",
            "inflight" => "回合执行中；未知写入不会自动重放",
            _ => "持久会话已建立",
        });
        state["updated_at"] = json!(now());
        put(&tx, "session_contexts", &state)?;
        tx.commit()?;
        Ok(())
    }
}

pub(super) fn complete_context_transaction(db: &Connection, task: &Value) -> Result<()> {
    if task["task_kind"] == "main"
        && task["agent"]["executor"] == "codex"
        && task["status"] != "completed"
    {
        let session_id = text(task, "session_id")?;
        if let Some(mut state) = context_record(db, session_id, "codex")? {
            if state["task_id"] == task["id"] {
                state["state"] = json!("invalidated");
                state["reason"] = json!("任务失败或取消；保留项目修改和应用历史，不自动重放回合");
                state["updated_at"] = json!(now());
                put(db, "session_contexts", &state)?;
            }
        }
    }
    Ok(())
}
