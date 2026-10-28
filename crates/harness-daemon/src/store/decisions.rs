//! Durable user questions and action-scoped approvals. A decision is not an
//! execution: only the still-live worker may consume it and leave waiting_input.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_REQUESTS_PER_TASK: u64 = 16;

fn milliseconds() -> Result<u64> {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .context("系统时间超出支持范围")
}

fn save_request(db: &Connection, request: &Value) -> Result<()> {
    db.execute(
        "INSERT INTO execution_requests(id,task_id,body) VALUES(?1,?2,?3) \
         ON CONFLICT(id) DO UPDATE SET body=excluded.body",
        params![
            text(request, "id")?,
            text(request, "task_id")?,
            serde_json::to_string(request)?
        ],
    )?;
    Ok(())
}

fn request_event(db: &Connection, request: &Value, message: &str) -> Result<RunEvent> {
    let mut item = event(
        Some(text(request, "session_id")?.into()),
        Some(text(request, "task_id")?.into()),
        &format!("request_{}", text(request, "status")?),
        message,
    );
    persist_event(db, &item)?;
    item.sequence = db.query_row(
        "SELECT sequence FROM events WHERE id=?1",
        [&item.id],
        |row| row.get(0),
    )?;
    Ok(item)
}

fn pending_task(db: &Connection, request: &Value) -> Result<Value> {
    let task = record(db, "tasks", text(request, "task_id")?)?;
    if task["status"] != "waiting_input" || task["waiting_request_id"] != request["id"] {
        bail!("请求已不属于当前等待任务，不能授权或恢复执行");
    }
    Ok(task)
}

fn is_expired(request: &Value, time: u64) -> Result<bool> {
    Ok(time
        >= request["expires_at_ms"]
            .as_u64()
            .context("请求过期时间无效")?)
}

fn finish_request(request: &mut Value, status: &str, reason: &str) {
    request["status"] = json!(status);
    request["reason"] = json!(reason);
    request["revision"] = json!(request["revision"].as_u64().unwrap_or(0) + 1);
    request["updated_at"] = json!(now());
}

impl Store {
    /// `action` is a safe display descriptor plus hashes of exact private
    /// arguments, not raw credentials or write-file content. The caller must
    /// redact its prompt/descriptor before crossing this persistence boundary.
    pub(crate) fn create_decision_request(
        &mut self,
        task_id: &str,
        kind: &str,
        prompt: &str,
        action: Option<Value>,
        timeout_seconds: u64,
    ) -> Result<Value> {
        if !matches!(kind, "question" | "approval") {
            bail!("不支持的交互请求类型");
        }
        if prompt.trim().is_empty() || prompt.len() > 4096 || prompt.contains('\0') {
            bail!("交互问题必须为非空文本，最多 4096 字节");
        }
        if !(1..=900).contains(&timeout_seconds) {
            bail!("交互等待时间必须在 1 至 900 秒之间");
        }
        match (kind, action.as_ref()) {
            ("question", None) => {}
            ("approval", Some(action))
                if action.is_object() && serde_json::to_vec(action)?.len() <= 16_384 => {}
            _ => bail!("问题不能附带授权动作；授权必须提供不超过 16 KiB 的动作对象"),
        }
        let time = milliseconds()?;
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if task["status"] != "running"
            || task["task_kind"] == "plan"
            || task["execution_mode"] == "demo"
        {
            bail!("只有实际运行中的执行任务可以请求用户回答或授权");
        }
        if !task["waiting_request_id"].is_null() {
            bail!("任务仍有未消费的交互请求");
        }
        let count: u64 = tx.query_row(
            "SELECT COUNT(*) FROM execution_requests WHERE task_id=?1",
            [task_id],
            |row| row.get(0),
        )?;
        if count >= MAX_REQUESTS_PER_TASK {
            bail!("单任务交互请求已达到 16 条上限");
        }
        let digest = action.as_ref().map(|action| {
            crate::continuation::fingerprint(&json!({"task_id":task_id,"action":action}))
        });
        let request = json!({
            "id":new_id(), "task_id":task_id, "session_id":task["session_id"],
            "kind":kind, "prompt":prompt, "action":action, "action_digest":digest,
            "revision":0, "status":"pending", "answer":null,"decision_actor":null,
            "decision":null,"decided_at":null,
            "created_at":now(), "updated_at":now(), "expires_at_ms":time + timeout_seconds * 1000,
            "consumed_at":null,"reason":"等待用户明确决定；尚未授权或执行"
        });
        save_request(&tx, &request)?;
        task["status"] = json!("waiting_input");
        task["waiting_request_id"] = request["id"].clone();
        task["updated_at"] = json!(now());
        put(&tx, "tasks", &task)?;
        sync_task_node(&tx, &task)?;
        request_event(
            &tx,
            &request,
            "任务等待用户明确回答或授权，未执行待确认动作",
        )?;
        tx.commit()?;
        Ok(request)
    }

    pub(crate) fn decision_request(&self, id: &str) -> Result<Value> {
        record(&self.db, "execution_requests", id)
    }

    pub(crate) fn expire_decision_request(&mut self, task_id: &str, id: &str) -> Result<Value> {
        let tx = self.db.transaction()?;
        let mut request = record(&tx, "execution_requests", id)?;
        if request["task_id"] != task_id {
            bail!("交互请求不属于指定任务");
        }
        if matches!(request["status"].as_str(), Some("pending" | "approved"))
            && request["consumed_at"].is_null()
            && is_expired(&request, milliseconds()?)?
        {
            finish_request(
                &mut request,
                "expired",
                "等待或授权有效期已超时，待确认动作没有执行",
            );
            save_request(&tx, &request)?;
            request_event(&tx, &request, "用户交互等待超时，待确认动作没有执行")?;
        }
        tx.commit()?;
        Ok(request)
    }

    /// This is an internal worker operation, never an HTTP management command.
    /// Denied/expired/cancelled outcomes must terminate the caller, not retry the
    /// rejected action. A restarted worker has no right to consume this request.
    pub(crate) fn resume_decision_request(&mut self, task_id: &str, id: &str) -> Result<Value> {
        let tx = self.db.transaction()?;
        let mut request = record(&tx, "execution_requests", id)?;
        if request["task_id"] != task_id {
            bail!("交互请求不属于指定任务");
        }
        let mut task = pending_task(&tx, &request)?;
        if matches!(request["status"].as_str(), Some("pending" | "approved"))
            && request["consumed_at"].is_null()
            && is_expired(&request, milliseconds()?)?
        {
            finish_request(
                &mut request,
                "expired",
                "等待或授权有效期已超时，待确认动作没有执行",
            );
            request_event(&tx, &request, "用户交互等待超时，待确认动作没有执行")?;
        }
        if !matches!(
            request["status"].as_str(),
            Some("answered" | "approved" | "denied" | "expired" | "cancelled")
        ) || !request["consumed_at"].is_null()
        {
            bail!("请求尚未决定或已经消费，不能重复恢复执行");
        }
        request["consumed_at"] = json!(now());
        request["updated_at"] = json!(now());
        request["revision"] = json!(request["revision"].as_u64().unwrap_or(0) + 1);
        save_request(&tx, &request)?;
        task["status"] = json!("running");
        task["waiting_request_id"] = Value::Null;
        task["updated_at"] = json!(now());
        put(&tx, "tasks", &task)?;
        sync_task_node(&tx, &task)?;
        let item = event(
            Some(text(&task, "session_id")?.into()),
            Some(task_id.into()),
            "request_consumed",
            "原执行器已读取用户决定；这不表示动作已经完成",
        );
        persist_event(&tx, &item)?;
        tx.commit()?;
        Ok(request)
    }
}

/// `actor` is derived by the authenticated server, never CommandEnvelope.source
/// or a model-supplied field. Outer Store::command owns command-ID deduplication.
pub(super) fn decide(tx: &Transaction<'_>, command: &Value, actor: &str) -> Result<RunEvent> {
    if actor.trim().is_empty() || actor.len() > 200 || actor.contains('\0') {
        bail!("缺少有效的已认证决定者");
    }
    let id = bounded_text(command, "request_id", 128)?;
    let mut request = record(tx, "execution_requests", id)?;
    let revision = command["expected_revision"]
        .as_u64()
        .context("交互决定必须提供 expected_revision")?;
    if request["revision"].as_u64() != Some(revision) || request["status"] != "pending" {
        bail!("交互请求已改变或已决定，请刷新后检查；未重复执行");
    }
    let mut task = pending_task(tx, &request)?;
    if is_expired(&request, milliseconds()?)? {
        bail!("交互请求已过期，未授予权限或替用户回答");
    }
    let (status, reason) = match (command["type"].as_str(), request["kind"].as_str()) {
        (Some("answer_request"), Some("question")) => {
            request["answer"] = json!(bounded_text(command, "answer", 8192)?);
            // Preserve the answer in the task's authoritative conversation
            // reconstruction even if the worker dies before consuming it.
            // Approval decisions never enter this history collection.
            let mut answers = match task.get("question_answers") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(answers)) => answers.clone(),
                _ => bail!("任务问题历史格式无效，未覆盖已有记录"),
            };
            if answers.len() >= MAX_REQUESTS_PER_TASK as usize
                || answers
                    .iter()
                    .any(|answer| answer["request_id"] == request["id"])
            {
                bail!("任务问题历史达到上限或已有此回答，未重复保存");
            }
            answers.push(json!({"request_id":request["id"],"prompt":request["prompt"],"answer":request["answer"]}));
            task["question_answers"] = json!(answers);
            task["updated_at"] = json!(now());
            put(tx, "tasks", &task)?;
            ("answered", "用户已回答；回答不产生新的执行权限")
        }
        (Some("decide_request"), Some("approval")) => {
            let digest = bounded_text(command, "action_digest", 64)?;
            let expected = crate::continuation::fingerprint(
                &json!({"task_id":request["task_id"],"action":request["action"]}),
            );
            if request["action_digest"].as_str() != Some(digest) || digest != expected {
                bail!("授权动作摘要不匹配，未授予权限");
            }
            if command["approve"]
                .as_bool()
                .context("approve 必须是布尔值")?
            {
                ("approved", "用户仅授权此任务的当前动作；尚未执行")
            } else {
                ("denied", "用户拒绝此动作；不能自行重试或提升权限")
            }
        }
        _ => bail!("回答与授权请求类型不匹配，不能将回答当作授权"),
    };
    finish_request(&mut request, status, reason);
    request["decision_actor"] = json!(actor);
    request["decision"] = json!(status);
    request["decided_at"] = json!(now());
    save_request(tx, &request)?;
    request_event(tx, &request, reason)
}

/// Called inside the task terminal transaction. Existing explicit decisions are
/// retained as audit history; none can be consumed once the task is terminal.
pub(super) fn cancel_for_task(tx: &Transaction<'_>, task_id: &str, reason: &str) -> Result<()> {
    let mut statement = tx.prepare("SELECT body FROM execution_requests WHERE task_id=?1")?;
    let requests = statement
        .query_map([task_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    for body in requests {
        let mut request: Value = serde_json::from_str(&body)?;
        if request["status"] == "pending" {
            finish_request(&mut request, "cancelled", reason);
            save_request(tx, &request)?;
            request_event(tx, &request, "任务已停止，待回答或授权请求已取消")?;
        }
    }
    Ok(())
}

/// No executor survives daemon recovery. Even a recorded approval must not
/// revive a paused effect: fail the interrupted task using the usual outbox.
pub(super) fn recover_pending(tx: &Transaction<'_>) -> Result<()> {
    for mut task in rows(tx, "tasks")? {
        if task["status"] == "waiting_input" && task["task_kind"] != "plan" {
            let task_id = text(&task, "id")?.to_owned();
            cancel_for_task(tx, &task_id, "后台重启，原执行器已失效；没有重放待确认动作")?;
            task["status"] = json!("failed");
            task["waiting_request_id"] = Value::Null;
            if task["stop_reason"].is_null() {
                task["stop_reason"] = json!("interrupted_wait");
            }
            task["error"] = json!("后台在等待用户时重启。原执行器已停止，待确认动作没有自动重放；请检查已有结果后明确继续。");
            terminal_steps(&mut task, false);
            complete_transaction(tx, &mut task)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Store {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.db.execute_batch("CREATE TABLE IF NOT EXISTS execution_requests(id TEXT PRIMARY KEY,task_id TEXT NOT NULL,body TEXT NOT NULL);").unwrap();
        put(
            &store.db,
            "sessions",
            &json!({"id":"session","messages":[],"updated_at":now()}),
        )
        .unwrap();
        add_task(&mut store, "task");
        store
    }

    fn add_task(store: &mut Store, id: &str) {
        let mut task = worker_task(
            "session",
            "测试",
            "native",
            &json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"desktop"}),
            None,
            None,
            None,
        );
        task["id"] = json!(id);
        task["status"] = json!("running");
        put(&store.db, "tasks", &task).unwrap();
    }

    fn request(store: &mut Store, kind: &str) -> Value {
        store.create_decision_request("task",kind,"确认？",(kind == "approval").then(||json!({"tool":"write_file","path":"file.txt","content_sha256":"fixture-hash"})),60).unwrap()
    }

    fn command(request: &Value, approve: bool) -> Value {
        json!({"type":"decide_request","request_id":request["id"],"expected_revision":0,"approve":approve,"action_digest":request["action_digest"]})
    }

    fn apply(store: &mut Store, command: Value) -> Result<RunEvent> {
        let tx = store.db.transaction()?;
        let event = decide(&tx, &command, "local_owner")?;
        tx.commit()?;
        Ok(event)
    }

    #[test]
    fn question_answer_is_durable_but_only_live_worker_resumes() {
        let mut store = fixture();
        let request = request(&mut store, "question");
        let id = request["id"].as_str().unwrap();
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["status"],
            "waiting_input"
        );
        assert!(store
            .create_decision_request("task", "question", "第二个？", None, 60)
            .is_err());
        assert!(store.resume_decision_request("task", id).is_err());
        apply(&mut store,json!({"type":"answer_request","request_id":id,"expected_revision":0,"answer":"保留现有文件"})).unwrap();
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["status"],
            "waiting_input"
        );
        let result = store.resume_decision_request("task", id).unwrap();
        assert_eq!(result["status"], "answered");
        assert_eq!(result["answer"], "保留现有文件");
        assert_eq!(result["decision_actor"], "local_owner");
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["status"],
            "running"
        );
        assert!(store.resume_decision_request("task", id).is_err());
        assert!(apply(&mut store, json!({"type":"answer_request","request_id":id,"expected_revision":0,"answer":"覆盖旧回答"})).is_err());
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["question_answers"],
            json!([
                {"request_id":id,"prompt":"确认？","answer":"保留现有文件"}
            ])
        );
    }

    #[test]
    fn exact_action_digest_kind_and_revision_are_required() {
        let mut store = fixture();
        let request = request(&mut store, "approval");
        let mut wrong = command(&request, true);
        wrong["action_digest"] = json!("wrong");
        assert!(apply(&mut store, wrong).is_err());
        assert!(apply(&mut store,json!({"type":"answer_request","request_id":request["id"],"expected_revision":0,"answer":"yes"})).is_err());
        let mut stale = command(&request, true);
        stale["expected_revision"] = json!(1);
        assert!(apply(&mut store, stale).is_err());
        apply(&mut store, command(&request, true)).unwrap();
        assert!(apply(&mut store, command(&request, true)).is_err());
        assert!(store
            .resume_decision_request("another-task", request["id"].as_str().unwrap())
            .is_err());
        assert_eq!(
            store
                .resume_decision_request("task", request["id"].as_str().unwrap())
                .unwrap()["status"],
            "approved"
        );
        assert!(record(&store.db, "tasks", "task").unwrap()["question_answers"].is_null());
    }

    #[test]
    fn changed_action_and_failed_decision_commit_never_grant_permission() {
        let mut store = fixture();
        let req = request(&mut store, "approval");
        let mut changed = req.clone();
        changed["action"]["path"] = json!("different.txt");
        save_request(&store.db, &changed).unwrap();
        assert!(apply(&mut store, command(&req, true)).is_err());
        save_request(&store.db, &req).unwrap();
        store.db.execute_batch("CREATE TRIGGER reject_decision_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(apply(&mut store, command(&req, true)).is_err());
        let stored = store.decision_request(req["id"].as_str().unwrap()).unwrap();
        assert_eq!(stored["status"], "pending");
        assert!(stored["decision_actor"].is_null());
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["status"],
            "waiting_input"
        );
    }

    #[test]
    fn denial_and_expiration_never_become_approval() {
        let mut store = fixture();
        let first = request(&mut store, "approval");
        apply(&mut store, command(&first, false)).unwrap();
        assert_eq!(
            store
                .resume_decision_request("task", first["id"].as_str().unwrap())
                .unwrap()["status"],
            "denied"
        );
        let mut expired = request(&mut store, "approval");
        expired["expires_at_ms"] = json!(0);
        save_request(&store.db, &expired).unwrap();
        assert!(apply(&mut store, command(&expired, true)).is_err());
        let id = expired["id"].as_str().unwrap();
        assert_eq!(
            store.expire_decision_request("task", id).unwrap()["status"],
            "expired"
        );
        assert_eq!(
            store.resume_decision_request("task", id).unwrap()["status"],
            "expired"
        );
    }

    #[test]
    fn approval_expiring_before_consumption_does_not_authorize_execution() {
        let mut store = fixture();
        let req = request(&mut store, "approval");
        apply(&mut store, command(&req, true)).unwrap();
        let id = req["id"].as_str().unwrap();
        let mut stored = store.decision_request(id).unwrap();
        stored["expires_at_ms"] = json!(0);
        save_request(&store.db, &stored).unwrap();
        let result = store.resume_decision_request("task", id).unwrap();
        assert_eq!(result["status"], "expired");
        assert_eq!(result["decision"], "approved");
        assert_eq!(result["decision_actor"], "local_owner");
    }

    #[test]
    fn cancel_after_approval_prevents_consumption() {
        let mut store = fixture();
        let request = request(&mut store, "approval");
        apply(&mut store, command(&request, true)).unwrap();
        let mut task = record(&store.db, "tasks", "task").unwrap();
        task["status"] = json!("cancelled");
        put(&store.db, "tasks", &task).unwrap();
        assert!(store
            .resume_decision_request("task", request["id"].as_str().unwrap())
            .is_err());
        assert_eq!(
            store
                .decision_request(request["id"].as_str().unwrap())
                .unwrap()["status"],
            "approved"
        );
    }

    #[test]
    fn recovery_fails_waiting_tasks_without_replaying_decided_or_pending_actions() {
        for approved in [false, true] {
            let mut store = fixture();
            let request = request(&mut store, "approval");
            if approved {
                apply(&mut store, command(&request, true)).unwrap();
            }
            let tx = store.db.transaction().unwrap();
            recover_pending(&tx).unwrap();
            tx.commit().unwrap();
            assert_eq!(
                record(&store.db, "tasks", "task").unwrap()["status"],
                "failed"
            );
            assert_eq!(rows(&store.db, "deliveries").unwrap().len(), 1);
            let status = store
                .decision_request(request["id"].as_str().unwrap())
                .unwrap();
            assert_eq!(
                status["status"],
                if approved { "approved" } else { "cancelled" }
            );
            assert!(store
                .resume_decision_request("task", request["id"].as_str().unwrap())
                .is_err());
            let tx = store.db.transaction().unwrap();
            recover_pending(&tx).unwrap();
            tx.commit().unwrap();
            assert_eq!(rows(&store.db, "deliveries").unwrap().len(), 1);
        }
    }

    #[test]
    fn recovery_preserves_unconsumed_answer_but_never_resumes_its_worker() {
        let mut store = fixture();
        let req = request(&mut store, "question");
        let id = req["id"].as_str().unwrap();
        apply(&mut store, json!({"type":"answer_request","request_id":id,"expected_revision":0,"answer":"已明确回答"})).unwrap();
        let tx = store.db.transaction().unwrap();
        recover_pending(&tx).unwrap();
        tx.commit().unwrap();
        let stored = store.decision_request(id).unwrap();
        assert_eq!(stored["status"], "answered");
        assert_eq!(stored["answer"], "已明确回答");
        assert!(stored["consumed_at"].is_null());
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["question_answers"],
            json!([
                {"request_id":id,"prompt":"确认？","answer":"已明确回答"}
            ])
        );
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["status"],
            "failed"
        );
        assert!(store.resume_decision_request("task", id).is_err());
        assert_eq!(rows(&store.db, "deliveries").unwrap().len(), 1);
    }

    #[test]
    fn failed_answer_commit_does_not_append_question_history() {
        let mut store = fixture();
        let req = request(&mut store, "question");
        store.db.execute_batch("CREATE TRIGGER reject_answer_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(apply(&mut store, json!({"type":"answer_request","request_id":req["id"],"expected_revision":0,"answer":"未提交回答"})).is_err());
        assert!(record(&store.db, "tasks", "task").unwrap()["question_answers"].is_null());
        assert_eq!(
            store.decision_request(req["id"].as_str().unwrap()).unwrap()["status"],
            "pending"
        );
    }

    #[test]
    fn bounded_requests_reject_invalid_payloads_and_excess_count() {
        let mut store = fixture();
        for (kind, prompt, action, timeout) in [
            ("other", "确认？", None, 60),
            ("question", "", None, 60),
            ("question", "确认？", Some(json!({})), 60),
            ("approval", "确认？", None, 60),
            ("question", "确认？", None, 0),
            ("question", "确认？", None, 901),
        ] {
            assert!(store
                .create_decision_request("task", kind, prompt, action, timeout)
                .is_err());
        }
        assert!(store
            .create_decision_request("task", "question", &"x".repeat(4097), None, 60)
            .is_err());
        assert!(store
            .create_decision_request(
                "task",
                "approval",
                "确认？",
                Some(json!({"data":"x".repeat(16384)})),
                60
            )
            .is_err());
        for _ in 0..16 {
            let req = request(&mut store, "question");
            apply(&mut store,json!({"type":"answer_request","request_id":req["id"],"expected_revision":0,"answer":"答复"})).unwrap();
            store
                .resume_decision_request("task", req["id"].as_str().unwrap())
                .unwrap();
        }
        assert!(store
            .create_decision_request("task", "question", "第十七个？", None, 60)
            .is_err());
    }

    #[test]
    fn failed_event_write_rolls_back_request_and_waiting_transition() {
        let mut store = fixture();
        store.db.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(store
            .create_decision_request("task", "question", "确认？", None, 60)
            .is_err());
        assert!(rows(&store.db, "execution_requests").unwrap().is_empty());
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["status"],
            "running"
        );
    }
}
