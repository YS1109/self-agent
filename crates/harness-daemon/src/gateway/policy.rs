//! Narrow browser-principal policy. Resource IDs are resolved by authoritative
//! daemon state, never by browser-provided paths or owner API envelopes.
use super::wire::{GatewayCommand, GatewayGrant, GatewayRpc};
use anyhow::{bail, Context, Result};
use harness_core::{PermissionMode, TaskControls};
use serde_json::Value;
use std::collections::HashSet;

pub(crate) trait ResourceContext {
    /// Live private transport-generation fence. Fail closed when disconnected,
    /// disabled, superseded or unable to establish the current configuration.
    fn channel_current(&self) -> bool;
    fn session_exists(&self, session_id: &str) -> Result<bool>;
    fn task_session(&self, task_id: &str) -> Result<String>;
    /// Immutable effective controls of the target task, from authoritative state.
    fn task_controls(&self, task_id: &str) -> Result<TaskControls>;
    /// Returns (task ID, is pending question). Approval requests MUST be false.
    fn question_task(&self, request_id: &str, expected_revision: u32) -> Result<(String, bool)>;
    fn artifact_task(&self, resource_id: &str) -> Result<String>;
}

pub(crate) fn identifier(value: &str) -> Result<()> {
    if value.len() != 36
        || value.bytes().enumerate().any(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte != b'-'
            } else {
                !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte)
            }
        })
    {
        bail!("远程资源标识必须是规范 UUID");
    }
    Ok(())
}
fn exact_fields(value: &Value, fields: &[&str]) -> Result<()> {
    let object = value.as_object().context("远程任务限制必须是完整对象")?;
    if object.len() != fields.len() || fields.iter().any(|key| !object.contains_key(*key)) {
        bail!("远程任务限制必须明确填写全部字段，不能依赖默认授权");
    }
    Ok(())
}
pub(crate) fn validate_grant(grant: &GatewayGrant, now_ms: u64) -> Result<TaskControls> {
    if grant.permissions.read_artifacts && !grant.permissions.read_results {
        bail!("文本产物包含任务结果，必须同时明确授权读取结果");
    }
    if grant.session_ids.is_empty() || grant.session_ids.len() > 32 {
        bail!("远程授权必须选择 1 至 32 个会话");
    }
    let mut sessions = HashSet::new();
    for id in &grant.session_ids {
        identifier(id)?;
        if !sessions.insert(id) {
            bail!("远程授权会话不能重复");
        }
    }
    if grant.expires_at_ms <= now_ms
        || grant.expires_at_ms.saturating_sub(now_ms) > 30 * 24 * 60 * 60 * 1000
        || grant.expires_at_ms > i64::MAX as u64
    {
        bail!("远程授权有效期必须在未来且不超过 30 天");
    }
    if serde_json::to_vec(&grant.controls)?.len() > 256 * 1024 {
        bail!("远程任务限制超过 256 KiB 上限");
    }
    exact_fields(
        &grant.controls,
        &[
            "budget",
            "permissions",
            "acceptance_commands",
            "capabilities",
        ],
    )?;
    exact_fields(
        &grant.controls["budget"],
        &[
            "max_model_requests",
            "max_runtime_seconds",
            "max_retries",
            "max_repair_attempts",
            "max_output_bytes",
            "max_external_calls",
        ],
    )?;
    exact_fields(
        &grant.controls["permissions"],
        &["read_files", "write_files", "run_commands", "managed_tools"],
    )?;
    exact_fields(&grant.controls["capabilities"], &["mcp_servers", "skills"])?;
    let controls: TaskControls =
        serde_json::from_value(grant.controls.clone()).context("远程任务限制字段无效")?;
    let budget = &controls.budget;
    if !(1..=128).contains(&budget.max_model_requests)
        || !(1..=900).contains(&budget.max_runtime_seconds)
        || budget.max_retries > 3
        || budget.max_repair_attempts > 8
        || !(1024..=262144).contains(&budget.max_output_bytes)
        || budget.max_external_calls > 128
    {
        bail!("远程任务预算超出本机支持范围");
    }
    let p = &controls.permissions;
    if (!p.read_files || p.write_files == PermissionMode::Deny)
        && (p.run_commands != PermissionMode::Deny || p.managed_tools != PermissionMode::Deny)
    {
        bail!("禁止文件读写时必须同时禁用可信命令和托管工具");
    }
    if p.write_files == PermissionMode::Ask
        && (p.run_commands == PermissionMode::Allow || p.managed_tools == PermissionMode::Allow)
    {
        bail!("文件写入需要授权时，命令和工具也必须受限");
    }
    if controls.acceptance_commands.len() > 8 {
        bail!("验收命令最多 8 条");
    }
    let mut unique = HashSet::new();
    for command in &controls.acceptance_commands {
        if command.trim().is_empty()
            || command.len() > 16000
            || command.contains('\0')
            || !unique.insert(command)
        {
            bail!("远程验收命令无效或重复");
        }
    }
    for ids in [
        &controls.capabilities.mcp_servers,
        &controls.capabilities.skills,
    ] {
        let mut seen = HashSet::new();
        if ids.len() > 8 {
            bail!("每类远程任务能力最多 8 项");
        }
        for id in ids {
            identifier(id)?;
            if !seen.insert(id) {
                bail!("远程任务能力不能重复");
            }
        }
    }
    Ok(controls)
}
fn content(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 8192 || value.contains('\0') {
        bail!("远程输入必须是非空且不超过 8192 UTF-8 字节的文本");
    }
    Ok(())
}
fn session(grant: &GatewayGrant, id: &str, context: &impl ResourceContext) -> Result<()> {
    identifier(id)?;
    if !grant.session_ids.iter().any(|allowed| allowed == id) || !context.session_exists(id)? {
        bail!("远程身份未获该会话授权");
    }
    Ok(())
}
fn scoped_task(
    grant: &GatewayGrant,
    task: &str,
    selected: &str,
    context: &impl ResourceContext,
) -> Result<()> {
    identifier(task)?;
    let actual = context.task_session(task)?;
    session(grant, &actual, context)?;
    if actual != selected {
        bail!("任务必须属于明确授权的所选会话");
    }
    Ok(())
}
/// Existing tasks cannot silently gain the grant's restrictions. Refuse input
/// unless their immutable effective controls already satisfy every ceiling.
fn compatible_task(grant: &GatewayGrant, task: &str, context: &impl ResourceContext) -> Result<()> {
    let ceiling = validate_grant(grant, grant.expires_at_ms.saturating_sub(1))?;
    let target = context.task_controls(task)?;
    let a = &target.budget;
    let b = &ceiling.budget;
    let p = &target.permissions;
    let q = &ceiling.permissions;
    if a.max_model_requests > b.max_model_requests
        || a.max_runtime_seconds > b.max_runtime_seconds
        || a.max_retries > b.max_retries
        || a.max_repair_attempts > b.max_repair_attempts
        || a.max_output_bytes > b.max_output_bytes
        || a.max_external_calls > b.max_external_calls
        || (p.read_files && !q.read_files)
        || p.write_files.intersect(q.write_files) != p.write_files
        || p.run_commands.intersect(q.run_commands) != p.run_commands
        || p.managed_tools.intersect(q.managed_tools) != p.managed_tools
        || target
            .capabilities
            .mcp_servers
            .iter()
            .any(|id| !ceiling.capabilities.mcp_servers.contains(id))
        || target
            .capabilities
            .skills
            .iter()
            .any(|id| !ceiling.capabilities.skills.contains(id))
        || ceiling
            .acceptance_commands
            .iter()
            .any(|command| !target.acceptance_commands.contains(command))
    {
        bail!("现有任务的执行权限或预算高于远程授权，不能注入输入");
    }
    Ok(())
}
/// Returns the exact session for a scoped operation; Sessions has no single
/// target and the caller must filter to grant.session_ids. Pairing and receipts
/// have dedicated transactional persistence entry points, never generic access.
pub(crate) fn authorize_rpc(
    grant: &GatewayGrant,
    principal: &str,
    _revision: u64,
    rpc: &GatewayRpc,
    context: &impl ResourceContext,
) -> Result<Option<String>> {
    if !context.channel_current() {
        bail!("远程连接已失效，未执行请求");
    }
    identifier(principal)?;
    let permissions = &grant.permissions;
    let target = match rpc {
        GatewayRpc::Sessions => {
            if !permissions.view {
                bail!("未授权查看会话");
            }
            None
        }
        GatewayRpc::Session { session_id } | GatewayRpc::Events { session_id, .. } => {
            if !permissions.view {
                bail!("未授权查看会话");
            }
            Some(session_id.clone())
        }
        GatewayRpc::Task { task_id } => {
            if !permissions.view {
                bail!("未授权查看任务");
            }
            identifier(task_id)?;
            Some(context.task_session(task_id)?)
        }
        GatewayRpc::Changes { task_id } => {
            if !permissions.read_results {
                bail!("未授权查看任务改动");
            }
            identifier(task_id)?;
            Some(context.task_session(task_id)?)
        }
        GatewayRpc::Artifacts { task_id } => {
            if !permissions.read_artifacts {
                bail!("未授权查看任务产物");
            }
            identifier(task_id)?;
            Some(context.task_session(task_id)?)
        }
        GatewayRpc::Artifact { resource_id } => {
            if !permissions.read_artifacts {
                bail!("未授权查看任务产物");
            }
            identifier(resource_id)?;
            let task = context.artifact_task(resource_id)?;
            identifier(&task)?;
            Some(context.task_session(&task)?)
        }
        GatewayRpc::Command {
            command_id,
            session_id,
            command,
        } => {
            identifier(command_id)?;
            session(grant, session_id, context)?;
            match command {
                GatewayCommand::Run { content: input } => {
                    if !permissions.run {
                        bail!("未授权运行任务");
                    }
                    content(input)?;
                }
                GatewayCommand::Cancel { task_id } => {
                    if !permissions.cancel {
                        bail!("未授权取消任务");
                    }
                    scoped_task(grant, task_id, session_id, context)?;
                }
                GatewayCommand::Steer {
                    task_id,
                    content: input,
                } => {
                    if !permissions.run {
                        bail!("未授权补充任务");
                    }
                    content(input)?;
                    scoped_task(grant, task_id, session_id, context)?;
                    compatible_task(grant, task_id, context)?;
                }
                GatewayCommand::Continue {
                    task_id,
                    content: input,
                } => {
                    if !permissions.run {
                        bail!("未授权继续任务");
                    }
                    if let Some(input) = input {
                        content(input)?;
                    }
                    scoped_task(grant, task_id, session_id, context)?;
                }
                GatewayCommand::Answer {
                    request_id,
                    expected_revision,
                    answer,
                } => {
                    if !permissions.answer {
                        bail!("未授权回答问题");
                    }
                    identifier(request_id)?;
                    content(answer)?;
                    let (task, is_question) =
                        context.question_task(request_id, *expected_revision)?;
                    if !is_question {
                        bail!("远程只能回答当前问题，不能批准权限请求");
                    }
                    scoped_task(grant, &task, session_id, context)?;
                    compatible_task(grant, &task, context)?;
                }
            }
            Some(session_id.clone())
        }
        _ => bail!("该请求必须使用独立的配对或收据入口"),
    };
    if let Some(id) = &target {
        session(grant, id, context)?;
    }
    if !context.channel_current() {
        bail!("远程连接已失效，未执行请求");
    }
    Ok(target)
}
