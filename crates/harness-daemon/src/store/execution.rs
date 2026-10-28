//! Immutable enqueue controls and transactional execution accounting.
use super::*;
use harness_core::{
    BudgetUsage, PermissionMode, TaskControls, TaskPermissions, VerificationAttempt,
};

const MAX_ACCEPTANCE_COMMANDS: usize = 8;
const MAX_VERIFICATIONS: usize = 256;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Charge {
    ModelRequest,
    Retry,
    RepairAttempt,
    OutputBytes(u64),
    Mutation,
}

pub(crate) fn validate_controls(controls: &TaskControls) -> Result<()> {
    let budget = &controls.budget;
    if !(1..=128).contains(&budget.max_model_requests)
        || !(1..=900).contains(&budget.max_runtime_seconds)
        || budget.max_retries > 3
        || budget.max_repair_attempts > 8
        || !(1024..=262_144).contains(&budget.max_output_bytes)
    {
        bail!("执行预算无效：模型请求 1–128 次、运行 1–900 秒、重试 0–3 次、修复 0–8 次、输出 1024–262144 字节");
    }
    if controls.acceptance_commands.len() > MAX_ACCEPTANCE_COMMANDS {
        bail!("验收命令最多 8 条");
    }
    if (!controls.permissions.read_files
        || controls.permissions.write_files == PermissionMode::Deny)
        && controls.permissions.run_commands != PermissionMode::Deny
    {
        bail!("禁止文件读取或写入时也必须禁止可信本机命令；命令不是项目沙箱");
    }
    if controls.permissions.write_files == PermissionMode::Ask
        && controls.permissions.run_commands == PermissionMode::Allow
    {
        bail!("文件写入需要授权时，可信本机命令也必须逐次授权或禁用");
    }
    let mut unique = HashSet::new();
    for command in &controls.acceptance_commands {
        if command.trim().is_empty()
            || command.len() > 16_000
            || command.contains('\0')
            || !unique.insert(command)
        {
            bail!("验收命令必须是非空、不重复且不超过 16000 字节的命令文本");
        }
    }
    Ok(())
}

fn intersect(target: &mut TaskControls, restriction: &TaskControls) -> Result<()> {
    validate_controls(restriction)?;
    target.budget.max_model_requests = target
        .budget
        .max_model_requests
        .min(restriction.budget.max_model_requests);
    target.budget.max_runtime_seconds = target
        .budget
        .max_runtime_seconds
        .min(restriction.budget.max_runtime_seconds);
    target.budget.max_retries = target
        .budget
        .max_retries
        .min(restriction.budget.max_retries);
    target.budget.max_repair_attempts = target
        .budget
        .max_repair_attempts
        .min(restriction.budget.max_repair_attempts);
    target.budget.max_output_bytes = target
        .budget
        .max_output_bytes
        .min(restriction.budget.max_output_bytes);
    target.permissions.read_files &= restriction.permissions.read_files;
    target.permissions.write_files = target
        .permissions
        .write_files
        .intersect(restriction.permissions.write_files);
    target.permissions.run_commands = target
        .permissions
        .run_commands
        .intersect(restriction.permissions.run_commands);
    for command in &restriction.acceptance_commands {
        if !target.acceptance_commands.contains(command) {
            target.acceptance_commands.push(command.clone());
        }
    }
    validate_controls(target)
}

/// Partial task JSON inherits the configured limits. A task never removes
/// existing acceptance requirements or raises settings/role ceilings.
pub(crate) fn resolve_controls(
    settings_default: &TaskControls,
    role: Option<&TaskControls>,
    requested: Option<&Value>,
    executor: &str,
) -> Result<TaskControls> {
    validate_controls(settings_default)?;
    let mut effective = settings_default.clone();
    if let Some(role) = role {
        intersect(&mut effective, role)?;
    }
    if let Some(requested) = requested {
        if !requested.is_object() {
            bail!("任务执行控制必须为对象");
        }
        // Validate the input independently, including unknown nested fields.
        let _: TaskControls = serde_json::from_value(requested.clone())
            .map_err(|_| anyhow!("任务执行控制字段或类型无效"))?;
        let mut merged = serde_json::to_value(&effective)?;
        for (key, value) in requested.as_object().unwrap() {
            if matches!(key.as_str(), "budget" | "permissions") {
                let fields = value.as_object().context("预算和权限必须为对象")?;
                for (field, value) in fields {
                    merged[key][field] = value.clone();
                }
            } else {
                merged[key] = value.clone();
            }
        }
        let restriction: TaskControls =
            serde_json::from_value(merged).map_err(|_| anyhow!("任务执行控制字段或类型无效"))?;
        intersect(&mut effective, &restriction)?;
    }
    match executor {
        "native" => {}
        "codex" => {
            // Canonical defaults here are compatibility sentinels, NOT an
            // enforceable count of Codex's internal model/repair requests.
            // Explicit task limits for those dimensions are always refused.
            let defaults = TaskControls::default();
            if effective.budget.max_model_requests != defaults.budget.max_model_requests
                || effective.budget.max_repair_attempts != defaults.budget.max_repair_attempts
                || effective.budget.max_retries != defaults.budget.max_retries
                || effective.permissions != TaskPermissions::default()
                || !effective.acceptance_commands.is_empty()
                || requested.is_some_and(|value| {
                    value["budget"].get("max_model_requests").is_some()
                        || value["budget"].get("max_repair_attempts").is_some()
                        || value["budget"].get("max_retries").is_some()
                })
            {
                bail!("Codex 无法执行本任务要求的内部模型预算、修复闭环或逐工具权限；请选择 Native，未启动执行器");
            }
        }
        _ => bail!("执行控制使用了未知执行器"),
    }
    Ok(effective)
}

fn task_controls(task: &Value) -> Result<TaskControls> {
    let controls = task
        .get("controls")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<TaskControls>(value.clone()))
        .transpose()
        .map_err(|_| anyhow!("持久化任务执行控制无效"))?
        .unwrap_or_default();
    validate_controls(&controls)?;
    Ok(controls)
}

fn usage(task: &Value) -> Result<BudgetUsage> {
    task.get("budget_usage")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|_| anyhow!("持久化任务预算计数无效"))
        .map(Option::unwrap_or_default)
}

fn running(task: &Value) -> Result<()> {
    if task["status"] != "running" || task["task_kind"] == "plan" {
        bail!("任务未在执行，不能消耗预算或更新验收记录");
    }
    Ok(())
}

fn save_control(tx: &Transaction<'_>, task: &mut Value, message: &str) -> Result<()> {
    task["updated_at"] = json!(now());
    put(tx, "tasks", task)?;
    persist_event(
        tx,
        &event(
            Some(text(task, "session_id")?.into()),
            Some(text(task, "id")?.into()),
            "task_control_updated",
            message,
        ),
    )
}

impl Store {
    /// Cancellation changes the public task state immediately, but an already
    /// started filesystem operation retains its writer lease until settled.
    /// Updating this marker must never create a second terminal callback.
    pub(crate) fn set_execution_cleanup(&mut self, task_id: &str, pending: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if pending {
            running(&task)?;
        } else if !matches!(
            task["status"].as_str(),
            Some("running" | "cancelled" | "failed")
        ) {
            bail!("任务状态不允许确认执行清理结果");
        }
        let next = if pending { "pending" } else { "settled" };
        if task["execution_cleanup"] == next {
            return Ok(());
        }
        if !pending && task["execution_cleanup"] != "pending" {
            bail!("没有本进程可确认的在途文件操作");
        }
        task["execution_cleanup"] = json!(next);
        task["execution_cleanup_updated_at"] = json!(now());
        save_control(
            &tx,
            &mut task,
            if pending {
                "文件操作已开始；即使取消，项目执行权也保留到该操作清理完成"
            } else {
                "在途文件操作已清理完成；任务原终态和回调保持不变"
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn execution_controls(&self, task_id: &str) -> Result<TaskControls> {
        task_controls(&record(&self.db, "tasks", task_id)?)
    }

    /// Repeated calls never reset usage or its original execution start.
    pub(crate) fn start_execution(&mut self, task_id: &str) -> Result<TaskControls> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        running(&task)?;
        let controls = task_controls(&task)?;
        let mut counters = usage(&task)?;
        if counters.started_at.is_none() {
            counters.started_at = Some(now());
            task["budget_usage"] = json!(counters);
            task["controls"] = json!(controls);
            save_control(&tx, &mut task, "任务执行预算已开始计时")?;
        }
        tx.commit()?;
        Ok(controls)
    }

    /// Reserve before requesting/effecting/publishing. Failure persists its
    /// reason without consuming the rejected increment or discarding results.
    pub(crate) fn charge_execution(&mut self, task_id: &str, charge: Charge) -> Result<()> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        running(&task)?;
        let controls = task_controls(&task)?;
        let mut counters = usage(&task)?;
        if counters.started_at.is_none() {
            bail!("任务尚未开始执行预算");
        }
        if task["stop_reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
        {
            bail!("任务已记录停止原因，不能继续消耗预算");
        }
        let exhaustion = match charge {
            Charge::ModelRequest
                if counters.model_requests >= controls.budget.max_model_requests =>
            {
                Some(("model_budget_exhausted", "模型请求预算已耗尽，保留已有结果"))
            }
            Charge::Retry if counters.retries >= controls.budget.max_retries => {
                Some(("retry_budget_exhausted", "安全重试预算已耗尽，未重放操作"))
            }
            Charge::RepairAttempt
                if counters.repair_attempts >= controls.budget.max_repair_attempts =>
            {
                Some((
                    "repair_budget_exhausted",
                    "修复预算已耗尽，保留失败和验收记录",
                ))
            }
            Charge::OutputBytes(bytes)
                if counters
                    .output_bytes
                    .checked_add(bytes)
                    .is_none_or(|total| total > u64::from(controls.budget.max_output_bytes)) =>
            {
                Some((
                    "output_budget_exhausted",
                    "任务输出预算已耗尽，保留已提交内容",
                ))
            }
            Charge::Mutation if counters.mutation_generation == u64::MAX => {
                Some(("mutation_budget_exhausted", "任务变更计数超过限制"))
            }
            _ => None,
        };
        if let Some((code, message)) = exhaustion {
            task["stop_reason"] = json!(code);
            save_control(&tx, &mut task, message)?;
            tx.commit()?;
            bail!("{message}");
        }
        match charge {
            Charge::ModelRequest => counters.model_requests += 1,
            Charge::Retry => counters.retries += 1,
            Charge::RepairAttempt => counters.repair_attempts += 1,
            Charge::OutputBytes(bytes) => counters.output_bytes += bytes,
            Charge::Mutation => counters.mutation_generation += 1,
        }
        task["budget_usage"] = json!(counters);
        save_control(&tx, &mut task, "任务执行预算已更新")?;
        tx.commit()?;
        Ok(())
    }

    /// Records are append-only; reaching the cap stops recording/execution
    /// rather than evicting the original failure. Callers redact first.
    pub(crate) fn record_verification(
        &mut self,
        task_id: &str,
        command: &str,
        exit_code: Option<i32>,
        output: &str,
    ) -> Result<VerificationAttempt> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        running(&task)?;
        let controls = task_controls(&task)?;
        if !controls
            .acceptance_commands
            .iter()
            .any(|expected| expected == command)
        {
            bail!("验收命令不属于任务固化的验收要求");
        }
        let mut attempts = task["verification_attempts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if attempts.len() >= MAX_VERIFICATIONS {
            bail!("任务验收记录已达到 256 条，原失败证据不会被删除");
        }
        let mut output = output.to_owned();
        if output.len() > 16_384 {
            let mut end = 16_384 - "\n[输出已截断]".len();
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            output.truncate(end);
            output.push_str("\n[输出已截断]");
        }
        let attempt = VerificationAttempt {
            id: new_id(),
            command: command.into(),
            exit_code,
            output,
            mutation_generation: usage(&task)?.mutation_generation,
            created_at: now(),
        };
        attempts.push(json!(attempt));
        task["verification_attempts"] = json!(attempts);
        save_control(&tx, &mut task, "真实验收命令结果已保存")?;
        tx.commit()?;
        Ok(attempt)
    }

    pub(crate) fn verification_complete(&self, task_id: &str) -> Result<bool> {
        verification_complete(&record(&self.db, "tasks", task_id)?)
    }

    /// The complete immutable commands remain in task.controls. Feedback is a
    /// bounded latest-result view, never an unbounded copy of task history.
    pub(crate) fn verification_feedback(&self, task_id: &str) -> Result<String> {
        let task = record(&self.db, "tasks", task_id)?;
        let controls = task_controls(&task)?;
        let attempts = task["verification_attempts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut feedback = Vec::new();
        for (index, command) in controls.acceptance_commands.iter().enumerate() {
            let attempt = attempts
                .iter()
                .rev()
                .find(|attempt| attempt["command"] == *command);
            let output = attempt
                .and_then(|attempt| attempt["output"].as_str())
                .unwrap_or("尚未取得真实执行结果");
            // Bound encoded JSON, including worst-case control-character
            // escaping; eight entries remain safely below the 64 KiB ceiling.
            let mut command_limit = command.len();
            let mut output_limit = output.len();
            let value = loop {
                let prefix = |text: &str, limit: usize| {
                    let mut end = limit.min(text.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text[..end].to_owned()
                };
                let value = json!({
                    "command_index":index,
                    "command":prefix(command,command_limit),
                    "command_truncated":command_limit<command.len(),
                    "exit_code":attempt.map(|attempt|attempt["exit_code"].clone()).unwrap_or(Value::Null),
                    "output":prefix(output,output_limit),
                    "output_truncated":output_limit<output.len(),
                });
                if serde_json::to_vec(&value)?.len() <= 8000 {
                    break value;
                }
                if output_limit >= command_limit && output_limit > 0 {
                    output_limit /= 2;
                } else {
                    command_limit /= 2;
                }
            };
            feedback.push(value);
        }
        Ok(serde_json::to_string(&feedback)?)
    }

    pub(crate) fn set_stop_reason(&mut self, task_id: &str, code: &str) -> Result<()> {
        if code.is_empty()
            || code.len() > 64
            || !code.bytes().all(|c| c.is_ascii_lowercase() || c == b'_')
        {
            bail!("任务停止原因标识无效");
        }
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if !matches!(task["status"].as_str(), Some("running" | "waiting_input")) {
            bail!("任务已结束，忽略迟到的停止原因");
        }
        if task["stop_reason"].is_null() {
            task["stop_reason"] = json!(code);
            save_control(&tx, &mut task, "任务停止原因已保存")?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// A process restart cannot establish whether its previous in-flight write
/// reached atomic replacement. Preserve that uncertainty; never replay it.
pub(super) fn recover_execution_cleanup(tx: &Transaction<'_>) -> Result<()> {
    for mut task in rows(tx, "tasks")? {
        if task["execution_cleanup"] == "pending" {
            task["execution_cleanup"] = json!("unknown");
            task["execution_cleanup_updated_at"] = json!(now());
            save_control(
                tx,
                &mut task,
                "后台重启时文件操作结果未知；请检查已有改动，未自动重放",
            )?;
        }
    }
    Ok(())
}

pub(super) fn verification_complete(task: &Value) -> Result<bool> {
    let controls = task_controls(task)?;
    let generation = usage(task)?.mutation_generation;
    let attempts = task["verification_attempts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(controls.acceptance_commands.iter().all(|command| {
        attempts
            .iter()
            .rev()
            .find(|attempt| attempt["command"] == *command)
            .is_some_and(|attempt| {
                attempt["exit_code"].as_i64() == Some(0)
                    && attempt["mutation_generation"].as_u64() == Some(generation)
            })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(controls: TaskControls) -> Store {
        let store = Store::open(Path::new(":memory:")).unwrap();
        put(&store.db, "tasks", &json!({"id":"task","session_id":"session","status":"running","task_kind":"main","controls":controls})).unwrap();
        store
    }

    #[test]
    fn cleanup_settles_cancelled_write_without_rewriting_terminal_result() {
        for status in ["cancelled", "failed"] {
            let mut store = setup(TaskControls::default());
            assert!(store.set_execution_cleanup("task", false).is_err());
            store.set_execution_cleanup("task", true).unwrap();
            let mut task = record(&store.db, "tasks", "task").unwrap();
            task["status"] = json!(status);
            task["result"] = json!("original terminal result");
            task["error"] = json!("original terminal error");
            put(&store.db, "tasks", &task).unwrap();
            let deliveries = rows(&store.db, "deliveries").unwrap();
            assert!(store.set_execution_cleanup("task", true).is_err());
            store.set_execution_cleanup("task", false).unwrap();
            let settled = record(&store.db, "tasks", "task").unwrap();
            assert_eq!(settled["execution_cleanup"], "settled");
            for field in ["status", "result", "error"] {
                assert_eq!(settled[field], task[field]);
            }
            assert_eq!(rows(&store.db, "deliveries").unwrap(), deliveries);
            store.set_execution_cleanup("task", false).unwrap();
            assert_eq!(record(&store.db, "tasks", "task").unwrap(), settled);
        }
    }

    #[test]
    fn restart_marks_pending_write_unknown_without_replay_or_terminal_rewrite() {
        let mut store = setup(TaskControls::default());
        store.set_execution_cleanup("task", true).unwrap();
        let mut task = record(&store.db, "tasks", "task").unwrap();
        task["status"] = json!("cancelled");
        task["result"] = json!("cancelled result");
        put(&store.db, "tasks", &task).unwrap();
        let tx = store.db.transaction().unwrap();
        recover_execution_cleanup(&tx).unwrap();
        tx.commit().unwrap();
        let recovered = record(&store.db, "tasks", "task").unwrap();
        assert_eq!(recovered["execution_cleanup"], "unknown");
        assert_eq!(recovered["status"], "cancelled");
        assert_eq!(recovered["result"], "cancelled result");
        assert!(store.set_execution_cleanup("task", false).is_err());
        let tx = store.db.transaction().unwrap();
        recover_execution_cleanup(&tx).unwrap();
        tx.commit().unwrap();
        assert_eq!(record(&store.db, "tasks", "task").unwrap(), recovered);
        assert!(rows(&store.db, "deliveries").unwrap().is_empty());
    }

    #[test]
    fn cancellation_and_restart_keep_first_reason_and_cleanup_uncertainty() {
        for restart in [false, true] {
            for previous_reason in [None, Some("output_budget_exhausted")] {
                let mut store = Store::open(Path::new(":memory:")).unwrap();
                let settings = Settings {
                    execution_mode: "demo".into(),
                    ..Settings::default()
                };
                let envelope = |command: Value| json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"fixture"},"command":command});
                let session = store.command(envelope(json!({"type":"create_session","title":"reason","project_path":std::env::temp_dir()})), &settings).unwrap().0;
                let receipt = store.command(envelope(json!({"type":"run_agent","session_id":session["session_id"],"content":"reason fixture"})), &settings).unwrap().0;
                let id = receipt["task_id"].as_str().unwrap();
                store.claim_task().unwrap().unwrap();
                store.set_execution_cleanup(id, true).unwrap();
                if let Some(reason) = previous_reason {
                    store.set_stop_reason(id, reason).unwrap();
                }
                if restart {
                    store.recover().unwrap();
                } else {
                    store
                        .command(
                            envelope(json!({"type":"cancel_task","task_id":id})),
                            &settings,
                        )
                        .unwrap();
                    store.set_execution_cleanup(id, false).unwrap();
                }
                let task = record(&store.db, "tasks", id).unwrap();
                assert_eq!(task["status"], if restart { "failed" } else { "cancelled" });
                assert_eq!(
                    task["stop_reason"],
                    previous_reason.unwrap_or(if restart {
                        "daemon_interrupted"
                    } else {
                        "user_cancelled"
                    })
                );
                assert_eq!(
                    task["execution_cleanup"],
                    if restart { "unknown" } else { "settled" }
                );
                assert_eq!(rows(&store.db, "deliveries").unwrap().len(), 1);
            }
        }
    }

    #[test]
    fn partial_requests_preserve_ceilings_and_requirements() {
        let mut settings = TaskControls::default();
        settings.budget.max_model_requests = 70;
        settings.acceptance_commands = vec!["original test".into()];
        let resolved = resolve_controls(
            &settings,
            None,
            Some(&json!({"budget":{"max_runtime_seconds":12},"acceptance_commands":[]})),
            "native",
        )
        .unwrap();
        assert_eq!(resolved.budget.max_model_requests, 70);
        assert_eq!(resolved.budget.max_runtime_seconds, 12);
        assert_eq!(resolved.acceptance_commands, ["original test"]);
        let mut role = settings.clone();
        role.permissions.run_commands = PermissionMode::Deny;
        role.budget.max_model_requests = 3;
        let resolved = resolve_controls(&settings, Some(&role), Some(&json!({"budget":{"max_model_requests":100},"permissions":{"run_commands":"allow"}})), "native").unwrap();
        assert_eq!(resolved.budget.max_model_requests, 3);
        assert_eq!(resolved.permissions.run_commands, PermissionMode::Deny);
    }

    #[test]
    fn invalid_values_and_codex_unsupported_limits_are_rejected() {
        let defaults = TaskControls::default();
        for request in [
            json!({"budget":{"max_model_requests":0}}),
            json!({"budget":{"max_runtime_seconds":901}}),
            json!({"budget":{"max_retries":4}}),
            json!({"budget":{"max_repair_attempts":9}}),
            json!({"budget":{"max_output_bytes":262145}}),
            json!({"permissions":{"write_files":"all"}}),
            json!({"unknown":true}),
        ] {
            assert!(resolve_controls(&defaults, None, Some(&request), "native").is_err());
        }
        assert!(resolve_controls(&defaults, None, None, "codex").is_ok());
        assert!(resolve_controls(
            &defaults,
            None,
            Some(&json!({"budget":{"max_runtime_seconds":1,"max_output_bytes":1024}})),
            "codex"
        )
        .is_ok());
        for request in [
            json!({"budget":{"max_model_requests":32}}),
            json!({"budget":{"max_repair_attempts":2}}),
            json!({"permissions":{"write_files":"ask"}}),
            json!({"acceptance_commands":["test"]}),
        ] {
            assert!(resolve_controls(&defaults, None, Some(&request), "codex").is_err());
        }
    }

    #[test]
    fn charges_are_bounded_persistent_and_never_reset() {
        let mut controls = TaskControls::default();
        controls.budget.max_model_requests = 1;
        let mut store = setup(controls);
        assert!(store
            .charge_execution("task", Charge::ModelRequest)
            .is_err());
        store.start_execution("task").unwrap();
        let started =
            record(&store.db, "tasks", "task").unwrap()["budget_usage"]["started_at"].clone();
        store
            .charge_execution("task", Charge::ModelRequest)
            .unwrap();
        store.start_execution("task").unwrap();
        assert!(store
            .charge_execution("task", Charge::ModelRequest)
            .is_err());
        let task = record(&store.db, "tasks", "task").unwrap();
        assert_eq!(task["budget_usage"]["model_requests"], 1);
        assert_eq!(task["budget_usage"]["started_at"], started);
        assert_eq!(task["stop_reason"], "model_budget_exhausted");
        assert!(store.charge_execution("task", Charge::Mutation).is_err());
        assert!(store.event_cursor().unwrap() >= 3);
    }

    #[test]
    fn output_overflow_and_terminal_charges_do_not_change_usage() {
        let mut controls = TaskControls::default();
        controls.budget.max_output_bytes = 1024;
        let mut store = setup(controls);
        store.start_execution("task").unwrap();
        store
            .charge_execution("task", Charge::OutputBytes(1024))
            .unwrap();
        assert!(store
            .charge_execution("task", Charge::OutputBytes(u64::MAX))
            .is_err());
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["budget_usage"]["output_bytes"],
            1024
        );
        let mut task = record(&store.db, "tasks", "task").unwrap();
        task["status"] = json!("cancelled");
        put(&store.db, "tasks", &task).unwrap();
        assert!(store.charge_execution("task", Charge::Mutation).is_err());
    }

    #[test]
    fn verification_requires_latest_exact_post_mutation_pass_and_preserves_failure() {
        let controls = TaskControls {
            acceptance_commands: vec!["test command".into()],
            ..TaskControls::default()
        };
        let mut store = setup(controls);
        store.start_execution("task").unwrap();
        assert!(!store.verification_complete("task").unwrap());
        assert!(store
            .record_verification("task", "other", Some(0), "")
            .is_err());
        store
            .record_verification("task", "test command", Some(1), "原失败")
            .unwrap();
        store.charge_execution("task", Charge::Mutation).unwrap();
        store
            .record_verification("task", "test command", Some(0), "真实通过")
            .unwrap();
        assert!(store.verification_complete("task").unwrap());
        store.charge_execution("task", Charge::Mutation).unwrap();
        assert!(!store.verification_complete("task").unwrap());
        store
            .record_verification("task", "test command", Some(0), "")
            .unwrap();
        store
            .record_verification("task", "test command", None, "中断")
            .unwrap();
        assert!(!store.verification_complete("task").unwrap());
        let task = record(&store.db, "tasks", "task").unwrap();
        assert_eq!(task["verification_attempts"][0]["output"], "原失败");
        assert_eq!(task["verification_attempts"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn budget_and_event_changes_roll_back_together() {
        let mut store = setup(TaskControls::default());
        store.start_execution("task").unwrap();
        store.db.execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(store
            .charge_execution("task", Charge::ModelRequest)
            .is_err());
        assert_eq!(
            record(&store.db, "tasks", "task").unwrap()["budget_usage"]["model_requests"],
            0
        );
        assert!(store.set_execution_cleanup("task", true).is_err());
        assert!(record(&store.db, "tasks", "task").unwrap()["execution_cleanup"].is_null());
    }

    #[test]
    fn file_restrictions_cannot_be_bypassed_with_trusted_shell() {
        let defaults = TaskControls::default();
        for permissions in [
            json!({"read_files":false}),
            json!({"write_files":"deny"}),
            json!({"write_files":"ask"}),
            json!({"read_files":false,"run_commands":"ask"}),
        ] {
            assert!(resolve_controls(
                &defaults,
                None,
                Some(&json!({"permissions":permissions})),
                "native"
            )
            .is_err());
        }
        for permissions in [
            json!({"read_files":false,"run_commands":"deny"}),
            json!({"write_files":"deny","run_commands":"deny"}),
            json!({"write_files":"ask","run_commands":"ask"}),
        ] {
            assert!(resolve_controls(
                &defaults,
                None,
                Some(&json!({"permissions":permissions})),
                "native"
            )
            .is_ok());
        }
    }

    #[test]
    fn retry_and_repair_budgets_are_distinct_and_first_stop_reason_wins() {
        let mut controls = TaskControls::default();
        controls.budget.max_retries = 1;
        controls.budget.max_repair_attempts = 1;
        let mut store = setup(controls);
        store.start_execution("task").unwrap();
        store.charge_execution("task", Charge::Retry).unwrap();
        store
            .charge_execution("task", Charge::RepairAttempt)
            .unwrap();
        assert!(store
            .charge_execution("task", Charge::RepairAttempt)
            .is_err());
        store.set_stop_reason("task", "later_reason").unwrap();
        let task = record(&store.db, "tasks", "task").unwrap();
        assert_eq!(task["budget_usage"]["retries"], 1);
        assert_eq!(task["budget_usage"]["repair_attempts"], 1);
        assert_eq!(task["stop_reason"], "repair_budget_exhausted");
        assert_eq!(
            store.execution_controls("task").unwrap().budget.max_retries,
            1
        );
    }

    #[test]
    fn verification_output_is_utf8_bounded_and_capped_without_evicting_failure() {
        let controls = TaskControls {
            acceptance_commands: vec!["check".into()],
            ..TaskControls::default()
        };
        let mut store = setup(controls);
        store.start_execution("task").unwrap();
        let first = store
            .record_verification("task", "check", Some(1), &"界".repeat(20_000))
            .unwrap();
        assert!(first.output.len() <= 16_384);
        assert!(first.output.ends_with("[输出已截断]"));
        for _ in 1..MAX_VERIFICATIONS {
            store
                .record_verification("task", "check", Some(0), "ok")
                .unwrap();
        }
        assert!(store
            .record_verification("task", "check", Some(0), "overflow")
            .is_err());
        let task = record(&store.db, "tasks", "task").unwrap();
        assert_eq!(
            task["verification_attempts"].as_array().unwrap().len(),
            MAX_VERIFICATIONS
        );
        assert_eq!(task["verification_attempts"][0]["id"], first.id);
        assert_eq!(task["verification_attempts"][0]["exit_code"], 1);
    }

    #[test]
    fn verification_feedback_bounds_encoded_json_and_keeps_latest_result() {
        let controls = TaskControls {
            acceptance_commands: (0..8)
                .map(|index| format!("command-{index}{}", "\u{1}".repeat(15_000)))
                .collect(),
            ..TaskControls::default()
        };
        let commands = controls.acceptance_commands.clone();
        let mut store = setup(controls);
        store.start_execution("task").unwrap();
        for command in &commands {
            store
                .record_verification("task", command, Some(1), &"\u{2}".repeat(16_384))
                .unwrap();
        }
        let feedback = store.verification_feedback("task").unwrap();
        assert!(feedback.len() <= 64 * 1024);
        let parsed: Value = serde_json::from_str(&feedback).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 8);
        assert_eq!(parsed[7]["command_index"], 7);
        assert_eq!(parsed[7]["exit_code"], 1);
        assert_eq!(parsed[7]["command_truncated"], true);
        store
            .record_verification("task", &commands[0], Some(0), "latest")
            .unwrap();
        let latest: Value =
            serde_json::from_str(&store.verification_feedback("task").unwrap()).unwrap();
        assert_eq!(latest[0]["exit_code"], 0);
        assert_eq!(latest[0]["output"], "latest");
    }

    #[test]
    fn explicit_continue_inherits_prior_limits_permissions_and_acceptance() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let settings = Settings {
            execution_mode: "demo".into(),
            ..Settings::default()
        };
        let envelope = |command: Value| json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"test"},"command":command});
        let session=store.command(envelope(json!({"type":"create_session","title":"controls","project_path":std::env::temp_dir()})),&settings).unwrap().0;
        let original=store.command(envelope(json!({"type":"run_agent","session_id":session["session_id"],"content":"original goal","controls":{
            "budget":{"max_model_requests":2},"permissions":{"write_files":"ask","run_commands":"ask"},"acceptance_commands":["python check.py"]
        }})),&settings).unwrap().0;
        let original_id = original["task_id"].as_str().unwrap();
        let mut previous = record(&store.db, "tasks", original_id).unwrap();
        previous["status"] = json!("failed");
        previous["error"] = json!("original failure");
        put(&store.db, "tasks", &previous).unwrap();
        let continued=store.command(envelope(json!({"type":"continue_task","task_id":original_id,"controls":{
            "budget":{"max_model_requests":128},"permissions":{"write_files":"allow","run_commands":"allow"},"acceptance_commands":[]
        }})),&settings).unwrap().0;
        let next = record(&store.db, "tasks", continued["task_id"].as_str().unwrap()).unwrap();
        assert_eq!(next["controls"], previous["controls"]);
        assert_eq!(next["continued_from"], original_id);
        assert_eq!(record(&store.db, "tasks", original_id).unwrap(), previous);
    }
}
