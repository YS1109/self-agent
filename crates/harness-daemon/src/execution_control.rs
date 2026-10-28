//! Runtime control is a thin bridge to durable task state. It never grants
//! permissions, replays failed tools, or treats a question answer as approval.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use harness_core::{PermissionMode, TaskControls};
use serde_json::{json, Value};

use crate::{continuation::fingerprint, store::execution::Charge, AppState};

pub(crate) struct RunControl {
    state: Arc<AppState>,
    task_id: String,
    pub controls: TaskControls,
    executor: String,
    started: Instant,
}

impl RunControl {
    pub fn start(state: Arc<AppState>, task_id: &str, executor: &str) -> Result<Self> {
        let controls = state.store.lock().unwrap().start_execution(task_id)?;
        Ok(Self {
            state,
            task_id: task_id.into(),
            controls,
            executor: executor.into(),
            started: Instant::now(),
        })
    }

    pub fn stop(&self, code: &str) {
        let _ = self
            .state
            .store
            .lock()
            .unwrap()
            .set_stop_reason(&self.task_id, code);
    }

    pub fn stopped(&self) -> bool {
        // Codex cannot translate an application file/tool restriction mid-turn.
        // Stop it instead of pretending the new owner policy is enforced.
        let current = self
            .state
            .settings
            .read()
            .unwrap()
            .execution_controls
            .clone();
        let mut builtin_permissions = current.permissions.clone();
        builtin_permissions.managed_tools = PermissionMode::Deny;
        if self.executor == "codex"
            && builtin_permissions != harness_core::TaskPermissions::default()
        {
            self.stop("permission_revoked");
            return true;
        }
        if self
            .controls
            .capabilities
            .mcp_servers
            .iter()
            .any(|id| !current.capabilities.mcp_servers.contains(id))
            || self
                .controls
                .capabilities
                .skills
                .iter()
                .any(|id| !current.capabilities.skills.contains(id))
            || (!self.controls.capabilities.mcp_servers.is_empty()
                && (current.permissions.managed_tools == PermissionMode::Deny
                    || !current.permissions.read_files
                    || current.permissions.write_files == PermissionMode::Deny))
            || self
                .state
                .store
                .lock()
                .unwrap()
                .check_task_capabilities(&self.task_id)
                .is_err()
        {
            self.stop("capability_revoked");
            return true;
        }
        if self.started.elapsed()
            >= Duration::from_secs(self.controls.budget.max_runtime_seconds.into())
        {
            self.stop("runtime_exhausted");
            return true;
        }
        self.state
            .store
            .lock()
            .unwrap()
            .execution_stopped(&self.task_id)
            .unwrap_or(true)
    }

    pub fn check(&self) -> Result<()> {
        if self.stopped() {
            bail!("任务已取消或运行时长预算已耗尽");
        }
        Ok(())
    }

    pub fn charge(&self, charge: Charge) -> Result<()> {
        self.check()?;
        self.state
            .store
            .lock()
            .unwrap()
            .charge_execution(&self.task_id, charge)
    }

    pub(crate) fn capability_state(
        &self,
    ) -> Result<crate::store::capabilities::TaskCapabilityState> {
        self.check()?;
        self.state
            .store
            .lock()
            .unwrap()
            .task_capability_state(&self.task_id)
    }

    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::from_std(
            self.started + Duration::from_secs(self.controls.budget.max_runtime_seconds.into()),
        )
    }

    pub(crate) fn begin_capability_call(
        &self,
        server: &str,
        revision: u64,
        tool: &str,
        digest: &str,
    ) -> Result<String> {
        self.check()?;
        self.state.store.lock().unwrap().begin_capability_call(
            &self.task_id,
            server,
            revision,
            tool,
            digest,
        )
    }

    pub(crate) fn finish_capability_call(
        &self,
        id: &str,
        status: &'static str,
        reason: &'static str,
    ) -> Result<()> {
        self.state
            .store
            .lock()
            .unwrap()
            .finish_capability_call(id, status, Some(reason))
            .map(|_| ())
    }

    /// MCP is arbitrary owner-selected local code. Read-only server annotations
    /// are not permissions, and a file-scoped policy cannot constrain this process.
    pub(crate) async fn authorize_managed(
        &self,
        binding: &Value,
        safe_action: Value,
    ) -> Result<PermissionMode> {
        self.check()?;
        let mode = self.controls.permissions.managed_tools.intersect(
            self.state
                .settings
                .read()
                .unwrap()
                .execution_controls
                .permissions
                .managed_tools,
        );
        if mode == PermissionMode::Deny {
            self.stop("permission_denied");
            bail!("任务未获得托管外部工具权限，未启动服务");
        }
        if mode == PermissionMode::Ask {
            let request = self.state.store.lock().unwrap().create_decision_request(
                &self.task_id, "approval",
                "此操作会启动受信任的本机 MCP 程序并调用所列工具。该程序没有文件或网络沙箱；工具只读提示不构成限制。批准仅适用于本次参数和修订。",
                Some(json!({"tool":"managed_mcp","arguments_digest":fingerprint(binding),"details":safe_action})),
                self.controls.budget.max_runtime_seconds.min(300).into(),
            )?;
            let decided = self.wait_for_request(request).await?;
            if decided["status"] != "approved" {
                self.stop("approval_denied");
                bail!("MCP 操作未获有效授权，未启动服务");
            }
        }
        self.check()?;
        if self
            .state
            .settings
            .read()
            .unwrap()
            .execution_controls
            .permissions
            .managed_tools
            == PermissionMode::Deny
        {
            self.stop("permission_revoked");
            bail!("MCP 权限已撤销，未启动服务");
        }
        self.recheck_managed_authorization(mode)?;
        Ok(mode)
    }

    pub(crate) fn recheck_managed_authorization(&self, granted: PermissionMode) -> Result<()> {
        self.check()?;
        let current = self
            .state
            .settings
            .read()
            .unwrap()
            .execution_controls
            .permissions
            .clone();
        let effective = self
            .controls
            .permissions
            .managed_tools
            .intersect(current.managed_tools)
            .intersect(self.controls.permissions.write_files)
            .intersect(current.write_files);
        if !current.read_files
            || effective == PermissionMode::Deny
            || (effective == PermissionMode::Ask && granted != PermissionMode::Ask)
        {
            self.stop("permission_revoked");
            bail!("MCP 权限在服务准备期间收紧，工具请求未发送；请建立新任务重新授权");
        }
        Ok(())
    }

    pub fn read_files_allowed(&self) -> bool {
        self.controls.permissions.read_files
            && self
                .state
                .settings
                .read()
                .unwrap()
                .execution_controls
                .permissions
                .read_files
    }

    pub fn set_cleanup(&self, pending: bool) -> Result<()> {
        self.state
            .store
            .lock()
            .unwrap()
            .set_execution_cleanup(&self.task_id, pending)
    }

    pub(crate) fn cleanup_unknown(&self) -> Result<()> {
        self.state
            .store
            .lock()
            .unwrap()
            .mark_execution_cleanup_unknown(&self.task_id)
    }

    pub fn verification_complete(&self) -> Result<bool> {
        self.state
            .store
            .lock()
            .unwrap()
            .verification_complete(&self.task_id)
    }

    pub fn verification_feedback(&self) -> Result<String> {
        self.state
            .store
            .lock()
            .unwrap()
            .verification_feedback(&self.task_id)
    }

    pub fn record_verification(
        &self,
        command: &str,
        exit_code: Option<i32>,
        output: &str,
    ) -> Result<()> {
        self.state.store.lock().unwrap().record_verification(
            &self.task_id,
            command,
            exit_code,
            output,
        )?;
        Ok(())
    }

    /// A live waiter owns resuming the task. HTTP only records a decision; it
    /// cannot start an executor or resurrect a cancelled task.
    async fn wait_for_request(&self, request: Value) -> Result<Value> {
        let id = request["id"].as_str().context("交互请求缺少标识")?;
        loop {
            self.check()?;
            let current = self.state.store.lock().unwrap().decision_request(id)?;
            if current["status"] != "pending" {
                return self
                    .state
                    .store
                    .lock()
                    .unwrap()
                    .resume_decision_request(&self.task_id, id);
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis();
            if current["expires_at_ms"]
                .as_u64()
                .is_some_and(|expires| now_ms >= u128::from(expires))
            {
                self.state
                    .store
                    .lock()
                    .unwrap()
                    .expire_decision_request(&self.task_id, id)?;
                continue;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn ask_question(&self, prompt: &str, timeout_seconds: u32) -> Result<String> {
        self.check()?;
        let request = self.state.store.lock().unwrap().create_decision_request(
            &self.task_id,
            "question",
            prompt,
            None,
            timeout_seconds.into(),
        )?;
        let decided = self.wait_for_request(request).await?;
        if decided["status"] != "answered" {
            self.stop("input_expired");
            bail!("问题未得到有效回答，已停止依赖该回答的任务");
        }
        Ok(decided["answer"]
            .as_str()
            .context("问题回答格式无效")?
            .into())
    }

    pub async fn authorize_tool(
        &self,
        name: &str,
        arguments: &Value,
        safe_action: Value,
    ) -> Result<()> {
        self.check()?;
        // Settings changes may revoke an existing snapshot, never broaden it.
        let current = self
            .state
            .settings
            .read()
            .unwrap()
            .execution_controls
            .permissions
            .clone();
        let permissions = &self.controls.permissions;
        let mode = match name {
            "list_files" | "read_file" | "search_files" => {
                if permissions.read_files && current.read_files {
                    PermissionMode::Allow
                } else {
                    PermissionMode::Deny
                }
            }
            "write_file" => permissions.write_files.intersect(current.write_files),
            "run_command" => {
                if std::env::var("HARNESS_ALLOW_COMMANDS").as_deref() != Ok("1") {
                    PermissionMode::Deny
                } else {
                    permissions.run_commands.intersect(current.run_commands)
                }
            }
            _ => PermissionMode::Deny,
        };
        match mode {
            PermissionMode::Allow => Ok(()),
            PermissionMode::Deny => {
                self.stop("permission_denied");
                bail!("任务权限不允许使用此工具，未执行操作");
            }
            PermissionMode::Ask => {
                // Include an opaque digest of all actual arguments, even where
                // the review view intentionally omits file contents/secrets.
                let action = json!({"tool":name,"arguments_digest":fingerprint(arguments),"details":safe_action});
                let request = self.state.store.lock().unwrap().create_decision_request(
                    &self.task_id,
                    "approval",
                    "此任务请求执行以下具体操作。批准仅适用于本次操作；拒绝会停止任务。",
                    Some(action),
                    self.controls.budget.max_runtime_seconds.min(300).into(),
                )?;
                let decided = self.wait_for_request(request).await?;
                if decided["status"] != "approved" {
                    self.stop(if decided["status"] == "expired" {
                        "approval_expired"
                    } else {
                        "approval_denied"
                    });
                    bail!("操作未获有效授权，未执行操作");
                }
                self.check()?;
                let latest = self
                    .state
                    .settings
                    .read()
                    .unwrap()
                    .execution_controls
                    .permissions
                    .clone();
                let revoked = match name {
                    "write_file" => latest.write_files == PermissionMode::Deny,
                    "run_command" => {
                        latest.run_commands == PermissionMode::Deny
                            || std::env::var("HARNESS_ALLOW_COMMANDS").as_deref() != Ok("1")
                    }
                    _ => true,
                };
                if revoked {
                    self.stop("permission_denied");
                    bail!("授权期间权限已撤销，未执行操作");
                }
                Ok(())
            }
        }
    }
}

/// Codex delivers command evidence immediately before its tool event. Hold the
/// evidence privately until both representations can share one reservation.
/// Native reserves in the provider before invoking either callback.
pub(crate) struct ExecutionOutput {
    state: Arc<AppState>,
    task_id: String,
    control: Arc<RunControl>,
    codex: bool,
    secret: String,
    pending: std::sync::Mutex<Option<harness_core::CommandEvidence>>,
}

impl ExecutionOutput {
    pub fn new(
        state: Arc<AppState>,
        task_id: &str,
        control: Arc<RunControl>,
        codex: bool,
        secret: String,
    ) -> Self {
        Self {
            state,
            task_id: task_id.into(),
            control,
            codex,
            secret,
            pending: std::sync::Mutex::new(None),
        }
    }

    fn save(&self, item: harness_core::CommandEvidence) -> Result<()> {
        self.state
            .store
            .lock()
            .unwrap()
            .record_evidence(&self.task_id, item)
    }

    pub fn evidence(&self, mut item: harness_core::CommandEvidence) {
        item.command = crate::redact_secret(&item.command, &self.secret);
        item.output = crate::redact_secret(&item.output, &self.secret);
        let result = if self.codex {
            // At most one bounded command record is retained even if an
            // upstream item cannot be represented as a tool event.
            self.flush().map(|()| {
                *self.pending.lock().unwrap() = Some(item);
            })
        } else {
            self.save(item)
        };
        if let Err(error) = result {
            self.control.stop("evidence_persistence_failed");
            tracing::error!(%error, "无法保存预算内的命令证据");
        }
    }

    pub fn emit(&self, data: harness_core::ExecutionEvent) -> Result<()> {
        use harness_core::ExecutionEvent;
        let mut pending = self.pending.lock().unwrap().take();
        let bytes = match &data {
            ExecutionEvent::AssistantDelta { text } => text.len(),
            ExecutionEvent::ToolFinished { output, .. } if self.codex => output.len(),
            _ => 0,
        };
        let evidence_bytes = pending.as_ref().map_or(0, |item| item.output.len());
        let bytes = if matches!(&data, ExecutionEvent::ToolFinished { name, .. } if name == "command_execution")
        {
            bytes.max(evidence_bytes)
        } else {
            bytes.saturating_add(evidence_bytes)
        };
        let reservation = if bytes > 0 {
            self.control.charge(Charge::OutputBytes(bytes as u64))
        } else {
            self.control.check()
        };
        if reservation.is_err() {
            if let Some(item) = &mut pending {
                item.output.clear();
            }
        }
        if let Some(item) = pending {
            self.save(item)?;
        }
        reservation?;
        let event = self
            .state
            .store
            .lock()
            .unwrap()
            .execution_event(&self.task_id, data)?;
        self.state.publish(event);
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        let Some(mut item) = self.pending.lock().unwrap().take() else {
            return Ok(());
        };
        let reservation = self
            .control
            .charge(Charge::OutputBytes(item.output.len() as u64));
        if reservation.is_err() {
            item.output.clear();
        }
        self.save(item)?;
        reservation
    }
}

#[cfg(test)]
mod tests;
