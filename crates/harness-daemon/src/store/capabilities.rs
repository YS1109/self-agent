//! Durable owner-managed metadata and immutable task snapshots. No process is
//! started here. Skill bodies/catalogs live only in private tables, never in
//! public tasks, receipts or event payloads. This slice rejects credential refs.
use super::*;
use harness_core::{
    CapabilityReference, ManagedCapabilities, McpServerDefinition, McpServerView,
    McpToolDefinition, SkillView, TaskCapabilities, TaskControls,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_MCP: usize = 32;
const MAX_SKILLS: usize = 64;
const MAX_CALLS: usize = 128;
const MAX_BODY: usize = 64 * 1024;

pub(crate) struct PreparedSkillImport {
    pub content: String,
    pub source: String,
}
pub(crate) struct ProbeWork {
    pub id: String,
    pub revision: u64,
    pub definition: McpServerDefinition,
}
pub(crate) struct PinnedMcp {
    pub reference: CapabilityReference,
    pub definition: McpServerDefinition,
    pub catalog: Vec<McpToolDefinition>,
}
pub(crate) struct PinnedSkill {
    pub reference: CapabilityReference,
    pub source: String,
    pub content: String,
}
pub(crate) struct TaskCapabilityState {
    pub servers: Vec<PinnedMcp>,
    pub skills: Vec<PinnedSkill>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    id: String,
    command: CapabilityCommand,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum CapabilityCommand {
    SaveMcpServer {
        id: Option<String>,
        expected_revision: Option<u64>,
        definition: McpServerDefinition,
    },
    SetCapabilityEnabled {
        kind: String,
        id: String,
        expected_revision: u64,
        enabled: bool,
    },
    ProbeMcpServer {
        id: String,
        expected_revision: u64,
        confirm_start: bool,
    },
    SaveSkill {
        id: Option<String>,
        expected_revision: Option<u64>,
        name: String,
        content: String,
    },
    ImportSkill {
        id: Option<String>,
        expected_revision: Option<u64>,
        name: String,
        source_path: String,
    },
    DeleteCapability {
        kind: String,
        id: String,
        expected_revision: u64,
    },
}

pub(super) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS managed_capabilities(id TEXT PRIMARY KEY, body TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS capability_commands(id TEXT PRIMARY KEY, digest TEXT NOT NULL, receipt TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS task_capabilities(id TEXT PRIMARY KEY, body TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS capability_calls(id TEXT PRIMARY KEY, task_id TEXT NOT NULL, body TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS capability_calls_task ON capability_calls(task_id);",
    )?;
    Ok(())
}

fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn uuid(value: &str) -> Result<()> {
    if value.len() != 36
        || !value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
    {
        bail!("能力标识必须是规范 UUID");
    }
    Ok(())
}
fn safe_text(value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        bail!("能力文本为空、过长或包含无效字符");
    }
    Ok(())
}
fn name(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > 80 || value.chars().any(char::is_control)
    {
        bail!("能力名称必须为 1–80 个字符且不能包含控制字符");
    }
    Ok(())
}
fn private_text(value: &Value, known: &[String]) -> Result<()> {
    match value {
        Value::String(value) => {
            if known
                .iter()
                .any(|key| !key.is_empty() && value.contains(key))
                || value
                    .split_whitespace()
                    .any(crate::configuration::looks_like_credential)
                || crate::configuration::looks_like_credential(value)
            {
                bail!("能力配置不能包含访问凭据，请使用受支持的安全凭据入口");
            }
        }
        Value::Array(values) => {
            for value in values {
                private_text(value, known)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                private_text(&json!(key), known)?;
                private_text(value, known)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn parse(envelope: &Value) -> Result<Envelope> {
    if envelope.to_string().len() > 256 * 1024 {
        bail!("能力管理指令过大");
    }
    let parsed: Envelope = serde_json::from_value(envelope.clone())
        .map_err(|_| anyhow!("能力管理指令字段或类型无效"))?;
    uuid(&parsed.id)?;
    Ok(parsed)
}
fn lookup(db: &Connection, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT body FROM managed_capabilities WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|v| serde_json::from_str(&v).map_err(Into::into))
        .transpose()
}
fn active(db: &Connection, id: &str, kind: &str) -> Result<Value> {
    uuid(id)?;
    let value = lookup(db, id)?.context("能力不存在或已经删除")?;
    if value["deleted"] == true || value["kind"] != kind {
        bail!("能力不存在或已经删除");
    }
    Ok(value)
}
fn check_revision(value: &Value, expected: u64) -> Result<()> {
    if value["revision"].as_u64() != Some(expected) {
        bail!("能力修订已改变，请刷新后重新操作");
    }
    Ok(())
}
fn next_revision(value: &Value) -> Result<u64> {
    value["revision"]
        .as_u64()
        .and_then(|v| v.checked_add(1))
        .filter(|v| *v <= 9_007_199_254_740_991)
        .context("能力修订已达到限制")
}
fn kind(value: &str) -> Result<()> {
    if !matches!(value, "mcp" | "skill") {
        bail!("能力类型无效");
    }
    Ok(())
}
fn prepare_save(
    db: &Connection,
    id: Option<String>,
    expected: Option<u64>,
    kind: &str,
) -> Result<Value> {
    let id = id.unwrap_or_else(new_id);
    uuid(&id)?;
    if let Some(value) = lookup(db, &id)? {
        if value["deleted"] == true || value["kind"] != kind {
            bail!("能力标识已经使用，不能重新创建");
        }
        check_revision(&value, expected.context("更新能力必须提供预期修订")?)?;
        Ok(value)
    } else {
        if expected.unwrap_or(0) != 0 {
            bail!("能力不存在或已经删除");
        }
        let count = rows(db, "managed_capabilities")?
            .iter()
            .filter(|v| v["kind"] == kind && v["deleted"] != true)
            .count();
        if count >= if kind == "mcp" { MAX_MCP } else { MAX_SKILLS } {
            bail!("能力注册数量已达到限制");
        }
        Ok(
            json!({"id":id,"kind":kind,"revision":0,"enabled":false,"deleted":false,"revoke_epoch":0}),
        )
    }
}
fn validate_definition(definition: &McpServerDefinition) -> Result<()> {
    name(&definition.name)?;
    safe_text(&definition.program, 4096)?;
    if !Path::new(&definition.program).is_absolute() {
        bail!("MCP 程序必须是本机绝对路径");
    }
    if definition.args.len() > 64
        || definition
            .args
            .iter()
            .any(|a| a.len() > 8192 || a.contains('\0'))
        || definition.args.iter().map(String::len).sum::<usize>() > 65536
    {
        bail!("MCP 参数超出数量或长度限制");
    }
    if !definition.credential_refs.is_empty() {
        bail!("当前切片尚未开放 MCP 凭据绑定，请勿改用明文参数");
    }
    Ok(())
}

impl Store {
    pub(crate) fn existing_capability_receipt(&self, envelope: &Value) -> Result<Option<Value>> {
        let parsed = parse(envelope)?;
        let stored: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT digest,receipt FROM capability_commands WHERE id=?1",
                [parsed.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        stored
            .map(|(original, receipt)| {
                if original != digest(envelope) {
                    bail!("能力指令标识已被另一条指令使用");
                }
                Ok(serde_json::from_str(&receipt)?)
            })
            .transpose()
    }
    pub(crate) fn capability_command(
        &mut self,
        envelope: &Value,
        known_secrets: &[String],
    ) -> Result<Value> {
        self.apply_capability_command(envelope, None, known_secrets)
    }
    pub(crate) fn capability_import_command(
        &mut self,
        envelope: &Value,
        prepared: &PreparedSkillImport,
        known_secrets: &[String],
    ) -> Result<Value> {
        self.apply_capability_command(envelope, Some(prepared), known_secrets)
    }
    fn apply_capability_command(
        &mut self,
        envelope: &Value,
        prepared: Option<&PreparedSkillImport>,
        known_secrets: &[String],
    ) -> Result<Value> {
        // Check before inspecting import contents: an accepted import is a
        // snapshot, not an instruction to reread the same mutable file.
        if let Some(receipt) = self.existing_capability_receipt(envelope)? {
            return Ok(receipt);
        }
        private_text(envelope, known_secrets)?;
        let parsed = parse(envelope)?;
        let tx = self.db.transaction()?;
        let mut checking = false;
        let mut value = match parsed.command {
            CapabilityCommand::SaveMcpServer {
                id,
                expected_revision,
                mut definition,
            } => {
                if prepared.is_some() {
                    bail!("导入准备数据只能用于技能导入");
                }
                validate_definition(&definition)?;
                let mut value = prepare_save(&tx, id, expected_revision, "mcp")?;
                if definition.enabled != value["enabled"].as_bool().unwrap_or(false) {
                    bail!("启用状态只能通过独立启用指令修改，新配置默认禁用");
                }
                definition.enabled = value["enabled"].as_bool().unwrap_or(false);
                value["definition"] = json!(definition);
                value["status"] = json!("configured");
                value["reason"] = Value::Null;
                value["catalog"] = json!([]);
                value["catalog_digest"] = Value::Null;
                value["probe_state"] = Value::Null;
                value
            }
            CapabilityCommand::SaveSkill {
                id,
                expected_revision,
                name: skill_name,
                content,
            } => {
                if prepared.is_some() {
                    bail!("导入准备数据只能用于技能导入");
                }
                save_skill(
                    &tx,
                    id,
                    expected_revision,
                    &skill_name,
                    &content,
                    "manual",
                    known_secrets,
                )?
            }
            CapabilityCommand::ImportSkill {
                id,
                expected_revision,
                name: skill_name,
                source_path,
            } => {
                safe_text(&source_path, 4096)?;
                if !Path::new(&source_path).is_absolute()
                    || Path::new(&source_path)
                        .file_name()
                        .is_none_or(|n| n != "SKILL.md")
                {
                    bail!("导入来源必须是本机 SKILL.md 绝对路径");
                }
                let prepared = prepared.context("技能导入尚未准备安全读取的正文")?;
                safe_text(&prepared.source, 4096)?;
                if prepared.source != source_path {
                    bail!("技能导入来源与已准备文件不一致");
                }
                save_skill(
                    &tx,
                    id,
                    expected_revision,
                    &skill_name,
                    &prepared.content,
                    &prepared.source,
                    known_secrets,
                )?
            }
            CapabilityCommand::SetCapabilityEnabled {
                kind: cap_kind,
                id,
                expected_revision,
                enabled,
            } => {
                if prepared.is_some() {
                    bail!("导入准备数据只能用于技能导入");
                }
                kind(&cap_kind)?;
                let mut value = active(&tx, &id, &cap_kind)?;
                check_revision(&value, expected_revision)?;
                if !enabled && value["enabled"] == true {
                    value["revoke_epoch"] = json!(value["revoke_epoch"].as_u64().unwrap_or(0) + 1);
                }
                value["enabled"] = json!(enabled);
                if cap_kind == "mcp" {
                    value["definition"]["enabled"] = json!(enabled);
                    // An in-progress probe belongs to the former revision.
                    if value["status"] == "checking" {
                        value["status"] = json!("configured");
                        value["probe_state"] = Value::Null;
                    }
                }
                value
            }
            CapabilityCommand::ProbeMcpServer {
                id,
                expected_revision,
                confirm_start,
            } => {
                if prepared.is_some() {
                    bail!("导入准备数据只能用于技能导入");
                }
                if !confirm_start {
                    bail!("验证 MCP 会启动可信本机进程，必须明确确认");
                }
                let mut value = active(&tx, &id, "mcp")?;
                check_revision(&value, expected_revision)?;
                if value["status"] == "checking" {
                    bail!("MCP 验证已经在进行中，请等待结果");
                }
                value["status"] = json!("checking");
                value["probe_state"] = json!("pending");
                value["catalog"] = json!([]);
                value["catalog_digest"] = Value::Null;
                value["reason"] = Value::Null;
                checking = true;
                value
            }
            CapabilityCommand::DeleteCapability {
                kind: cap_kind,
                id,
                expected_revision,
            } => {
                if prepared.is_some() {
                    bail!("导入准备数据只能用于技能导入");
                }
                kind(&cap_kind)?;
                let mut value = active(&tx, &id, &cap_kind)?;
                check_revision(&value, expected_revision)?;
                // Retain only a tombstone; a UUID can never acquire new authority.
                value = json!({"id":id,"kind":cap_kind,"revision":value["revision"],"deleted":true,"enabled":false});
                value
            }
        };
        value["revision"] = json!(next_revision(&value)?);
        value["updated_at"] = json!(now());
        put(&tx, "managed_capabilities", &value)?;
        let receipt = json!({"command_id":parsed.id,"status":if checking {"checking"}else{"completed"},"capability_id":value["id"],"revision":value["revision"]});
        tx.execute(
            "INSERT INTO capability_commands(id,digest,receipt) VALUES(?1,?2,?3)",
            params![parsed.id, digest(envelope), receipt.to_string()],
        )?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn capabilities_registry(&self) -> Result<ManagedCapabilities> {
        let mut result = ManagedCapabilities::default();
        for value in rows(&self.db, "managed_capabilities")? {
            if value["deleted"] == true {
                continue;
            }
            if value["kind"] == "mcp" {
                let definition: McpServerDefinition =
                    serde_json::from_value(value["definition"].clone())?;
                result.mcp_servers.push(McpServerView {
                    id: text(&value, "id")?.into(),
                    revision: value["revision"].as_u64().context("能力修订无效")?,
                    name: definition.name,
                    enabled: value["enabled"] == true,
                    program: definition.program,
                    args: definition.args,
                    protocol: definition.protocol,
                    credential_slots: definition.credential_refs.into_keys().collect(),
                    status: text(&value, "status")?.into(),
                    reason: value["reason"].as_str().map(str::to_owned),
                    catalog_digest: value["catalog_digest"].as_str().map(str::to_owned),
                    tool_count: value["catalog"].as_array().map_or(0, Vec::len),
                    updated_at: text(&value, "updated_at")?.into(),
                });
            } else {
                result.skills.push(SkillView {
                    id: text(&value, "id")?.into(),
                    revision: value["revision"].as_u64().context("能力修订无效")?,
                    name: text(&value, "name")?.into(),
                    enabled: value["enabled"] == true,
                    source: text(&value, "source")?.into(),
                    content_digest: text(&value, "content_digest")?.into(),
                    content_bytes: text(&value, "content")?.len(),
                    updated_at: text(&value, "updated_at")?.into(),
                });
            }
        }
        Ok(result)
    }
    pub(crate) fn claim_pending_probe(
        &mut self,
        id: &str,
        revision: u64,
    ) -> Result<Option<ProbeWork>> {
        let tx = self.db.transaction()?;
        let Some(mut value) = lookup(&tx, id)? else {
            return Ok(None);
        };
        if value["deleted"] == true
            || value["kind"] != "mcp"
            || value["revision"].as_u64() != Some(revision)
            || value["status"] != "checking"
            || value["probe_state"] != "pending"
        {
            return Ok(None);
        }
        let work = ProbeWork {
            id: id.into(),
            revision,
            definition: serde_json::from_value(value["definition"].clone())?,
        };
        value["probe_state"] = json!("inflight");
        put(&tx, "managed_capabilities", &value)?;
        tx.commit()?;
        Ok(Some(work))
    }
    pub(crate) fn finish_probe(
        &mut self,
        id: &str,
        revision: u64,
        result: std::result::Result<Vec<McpToolDefinition>, &'static str>,
        known_secrets: &[String],
    ) -> Result<bool> {
        let tx = self.db.transaction()?;
        let Some(mut value) = lookup(&tx, id)? else {
            return Ok(false);
        };
        if value["deleted"] == true
            || value["kind"] != "mcp"
            || value["revision"].as_u64() != Some(revision)
            || value["status"] != "checking"
            || value["probe_state"] != "inflight"
        {
            return Ok(false);
        }
        match result {
            Ok(mut catalog) => {
                if validate_catalog(&mut catalog, known_secrets).is_err() {
                    value["status"] = json!("unavailable");
                    value["reason"] = json!("invalid_catalog");
                } else {
                    let catalog = json!(catalog);
                    value["catalog_digest"] = json!(digest(&catalog));
                    value["catalog"] = catalog;
                    value["status"] = json!("ready");
                    value["reason"] = Value::Null;
                }
            }
            Err(_reason) => {
                value["status"] = json!("unavailable");
                value["reason"] = json!("server_unavailable");
            }
        }
        value["probe_state"] = Value::Null;
        value["updated_at"] = json!(now());
        put(&tx, "managed_capabilities", &value)?;
        tx.commit()?;
        Ok(true)
    }
    pub(crate) fn task_capability_state(&self, task_id: &str) -> Result<TaskCapabilityState> {
        let _ = task_record(&self.db, task_id)?;
        let raw = task_private(&self.db, task_id)?;
        let mut result = TaskCapabilityState {
            servers: Vec::new(),
            skills: Vec::new(),
        };
        for server in raw["servers"].as_array().into_iter().flatten() {
            result.servers.push(PinnedMcp {
                reference: serde_json::from_value(server["reference"].clone())?,
                definition: serde_json::from_value(server["definition"].clone())?,
                catalog: serde_json::from_value(server["catalog"].clone())?,
            });
        }
        for skill in raw["skills"].as_array().into_iter().flatten() {
            result.skills.push(PinnedSkill {
                reference: serde_json::from_value(skill["reference"].clone())?,
                source: text(skill, "source")?.into(),
                content: text(skill, "content")?.into(),
            });
        }
        Ok(result)
    }
    pub(crate) fn check_task_capabilities(&self, task_id: &str) -> Result<()> {
        check_pinned(&self.db, task_id)
    }
    pub(crate) fn begin_capability_call(
        &mut self,
        task_id: &str,
        server_id: &str,
        revision: u64,
        tool: &str,
        safe_digest: &str,
    ) -> Result<String> {
        if !valid_digest(safe_digest) {
            bail!("能力调用摘要无效");
        }
        let tx = self.db.transaction()?;
        let mut task = task_record(&tx, task_id)?;
        if task["status"] != "running" || task["task_kind"] == "plan" {
            bail!("任务当前不能调用能力");
        }
        check_pinned(&tx, task_id)?;
        let pinned = task_private(&tx, task_id)?;
        let server = pinned["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|v| {
                v["reference"]["id"] == server_id
                    && v["reference"]["revision"].as_u64() == Some(revision)
            })
            .context("能力没有固定在此任务中")?;
        if !server["catalog"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v["name"] == tool)
        {
            bail!("能力工具不在固定目录中");
        }
        let controls: TaskControls = serde_json::from_value(task["controls"].clone())
            .map_err(|_| anyhow!("任务能力权限数据无效"))?;
        if controls.permissions.managed_tools == harness_core::PermissionMode::Deny {
            bail!("任务禁止应用管理的工具");
        }
        let count: usize = tx.query_row(
            "SELECT COUNT(*) FROM capability_calls WHERE task_id=?1",
            [task_id],
            |r| r.get(0),
        )?;
        if count >= MAX_CALLS {
            bail!("任务能力调用记录已达到限制");
        }
        let id = new_id();
        let value = json!({"id":id,"task_id":task_id,"server_id":server_id,"revision":revision,"tool":tool,"digest":safe_digest,"status":"sending","reason":Value::Null,"created_at":now(),"updated_at":now()});
        tx.execute(
            "INSERT INTO capability_calls(id,task_id,body) VALUES(?1,?2,?3)",
            params![id, task_id, value.to_string()],
        )?;
        refresh_calls(&tx, &mut task)?;
        // Call intent and ownership of possible process effects are one commit.
        // Cancellation cannot leave a sending record without a cleanup owner.
        task["execution_cleanup"] = json!("pending");
        task["execution_cleanup_updated_at"] = json!(now());
        put(&tx, "tasks", &task)?;
        tx.commit()?;
        Ok(id)
    }
    pub(crate) fn finish_capability_call(
        &mut self,
        call_id: &str,
        status: &'static str,
        safe_reason: Option<&'static str>,
    ) -> Result<bool> {
        if !matches!(status, "completed" | "tool_error" | "not_sent" | "unknown") {
            bail!("能力调用终态无效");
        }
        if safe_reason.is_some_and(|v| v.len() > 256 || v.chars().any(char::is_control)) {
            bail!("能力调用原因无效");
        }
        let tx = self.db.transaction()?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT body FROM capability_calls WHERE id=?1",
                [call_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            bail!("能力调用记录不存在");
        };
        let mut value: Value = serde_json::from_str(&raw)?;
        if value["status"] != "sending" {
            return Ok(false);
        }
        value["status"] = json!(status);
        value["reason"] = json!(safe_reason);
        value["updated_at"] = json!(now());
        tx.execute(
            "UPDATE capability_calls SET body=?2 WHERE id=?1",
            params![call_id, value.to_string()],
        )?;
        let mut task = task_record(&tx, text(&value, "task_id")?)?;
        refresh_calls(&tx, &mut task)?;
        put(&tx, "tasks", &task)?;
        tx.commit()?;
        Ok(true)
    }
}

fn save_skill(
    db: &Connection,
    id: Option<String>,
    expected: Option<u64>,
    skill_name: &str,
    content: &str,
    source: &str,
    known: &[String],
) -> Result<Value> {
    name(skill_name)?;
    safe_text(content, MAX_BODY)?;
    private_text(
        &json!({"name":skill_name,"content":content,"source":source}),
        known,
    )?;
    let mut value = prepare_save(db, id, expected, "skill")?;
    value["name"] = json!(skill_name);
    value["content"] = json!(content);
    value["source"] = json!(source);
    value["content_digest"] = json!(digest(&json!(content)));
    Ok(value)
}
fn validate_catalog(catalog: &mut [McpToolDefinition], known: &[String]) -> Result<()> {
    if catalog.len() > 128 || json!(catalog).to_string().len() > 1024 * 1024 {
        bail!("MCP 目录超过限制");
    }
    private_text(&json!(catalog), known)?;
    let mut unique = HashSet::new();
    for tool in catalog.iter_mut() {
        safe_text(&tool.name, 128)?;
        if tool.name.chars().any(char::is_control)
            || tool.description.len() > 4096
            || !unique.insert(tool.name.clone())
            || tool.input_schema.to_string().len() > 32 * 1024
            || tool.annotations.to_string().len() > 16 * 1024
            || !tool.input_schema.is_object()
            || !json_depth(&tool.input_schema, 0)
        {
            bail!("MCP 工具定义无效或超过限制");
        }
        crate::capabilities::schema::check_schema(&tool.input_schema)?;
        tool.schema_digest = digest(&tool.input_schema);
    }
    catalog.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}
fn json_depth(value: &Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        Value::Array(v) => v.iter().all(|v| json_depth(v, depth + 1)),
        Value::Object(v) => v.values().all(|v| json_depth(v, depth + 1)),
        _ => true,
    }
}
fn task_record(db: &Connection, id: &str) -> Result<Value> {
    record(db, "tasks", id).map_err(|_| anyhow!("任务不存在或任务记录无效"))
}
fn task_private(db: &Connection, id: &str) -> Result<Value> {
    let raw: Option<String> = db
        .query_row(
            "SELECT body FROM task_capabilities WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        Some(raw) => Ok(serde_json::from_str(&raw)?),
        None => {
            let task = task_record(db, id)?;
            for selection in [
                &task["managed_capabilities"],
                &task["controls"]["capabilities"],
            ] {
                for key in ["mcp_servers", "skills"] {
                    if selection[key].as_array().is_some_and(|v| !v.is_empty()) {
                        bail!("任务能力私有快照缺失，不能自动重新解析配置");
                    }
                }
            }
            Ok(json!({"id":id,"servers":[],"skills":[]}))
        }
    }
}
pub(super) fn snapshot_task_capabilities(db: &Connection, task: &mut Value) -> Result<()> {
    let controls: TaskControls = serde_json::from_value(task["controls"].clone())
        .map_err(|_| anyhow!("任务能力选择无效"))?;
    let selection = controls.capabilities;
    if task["agent"]["executor"] == "codex" && !selection.mcp_servers.is_empty() {
        bail!("应用管理的 MCP 仅可用于 Native");
    }
    if selection.mcp_servers.len() > 8 || selection.skills.len() > 8 {
        bail!("每类任务能力最多选择 8 个");
    }
    let mut refs = TaskCapabilities::default();
    let mut servers = Vec::new();
    let mut skills = Vec::new();
    let mut seen = HashSet::new();
    let mut total_tools = 0usize;
    let mut total_skill_bytes = 0usize;
    for id in selection.mcp_servers {
        if !seen.insert(id.clone()) {
            bail!("任务能力选择不能重复");
        }
        let value = active(db, &id, "mcp")?;
        if value["enabled"] != true || value["status"] != "ready" {
            bail!("所选 MCP 尚未启用或验证就绪");
        }
        total_tools += value["catalog"].as_array().map_or(0, Vec::len);
        if total_tools > 128 {
            bail!("任务所选 MCP 工具总数不能超过 128 个");
        }
        let reference = CapabilityReference {
            id,
            revision: value["revision"].as_u64().context("能力修订无效")?,
            name: text(&value["definition"], "name")?.into(),
            digest: text(&value, "catalog_digest")?.into(),
        };
        servers.push(json!({"reference":reference,"definition":value["definition"],"catalog":value["catalog"]}));
        refs.mcp_servers.push(reference);
    }
    for id in selection.skills {
        if !seen.insert(id.clone()) {
            bail!("任务能力选择不能重复");
        }
        let value = active(db, &id, "skill")?;
        if value["enabled"] != true {
            bail!("所选技能尚未启用");
        }
        total_skill_bytes += text(&value, "content")?.len();
        if total_skill_bytes > 128 * 1024 {
            bail!("任务所选技能正文总量不能超过 128 KiB");
        }
        let reference = CapabilityReference {
            id,
            revision: value["revision"].as_u64().context("能力修订无效")?,
            name: text(&value, "name")?.into(),
            digest: text(&value, "content_digest")?.into(),
        };
        skills.push(json!({"reference":reference,"source":value["source"],"content":value["content"],"revoke_epoch":value["revoke_epoch"]}));
        refs.skills.push(reference);
    }
    let id = text(task, "id")?;
    if db
        .query_row("SELECT 1 FROM task_capabilities WHERE id=?1", [id], |r| {
            r.get::<_, i32>(0)
        })
        .optional()?
        .is_some()
    {
        bail!("任务能力快照已经固定，不能替换");
    }
    put(
        db,
        "task_capabilities",
        &json!({"id":id,"servers":servers,"skills":skills}),
    )?;
    task["managed_capabilities"] = json!(refs);
    task["capability_calls"] = json!([]);
    Ok(())
}
fn check_pinned(db: &Connection, task_id: &str) -> Result<()> {
    let _ = task_record(db, task_id)?;
    let private = task_private(db, task_id)?;
    for pinned in private["servers"].as_array().into_iter().flatten() {
        let reference = &pinned["reference"];
        let value = active(db, text(reference, "id")?, "mcp")?;
        if value["enabled"] != true
            || value["status"] != "ready"
            || value["revision"] != reference["revision"]
            || value["catalog_digest"] != reference["digest"]
        {
            bail!("任务固定的 MCP 已被撤销或修改");
        }
    }
    for pinned in private["skills"].as_array().into_iter().flatten() {
        let value = active(db, text(&pinned["reference"], "id")?, "skill")?;
        if value["enabled"] != true || value["revoke_epoch"] != pinned["revoke_epoch"] {
            bail!("任务固定的技能已经撤销");
        }
    }
    Ok(())
}
fn refresh_calls(db: &Connection, task: &mut Value) -> Result<()> {
    let mut stmt =
        db.prepare("SELECT body FROM capability_calls WHERE task_id=?1 ORDER BY rowid LIMIT 128")?;
    let values = stmt.query_map([text(task, "id")?], |r| r.get::<_, String>(0))?;
    let values = values
        .map(|v| Ok(serde_json::from_str::<Value>(&v?)?))
        .collect::<Result<Vec<_>>>()?;
    task["capability_calls"] = json!(values);
    Ok(())
}
pub(super) fn recover(db: &Connection) -> Result<()> {
    for mut value in rows(db, "managed_capabilities")? {
        if value["deleted"] != true && value["kind"] == "mcp" && value["status"] == "checking" {
            value["status"] = json!("unavailable");
            value["reason"] = json!("probe_interrupted");
            value["probe_state"] = Value::Null;
            value["updated_at"] = json!(now());
            put(db, "managed_capabilities", &value)?;
        }
    }
    let mut affected = HashSet::new();
    for mut value in rows(db, "capability_calls")? {
        if value["status"] == "sending" {
            value["status"] = json!("unknown");
            value["reason"] = json!("后台重启，调用结果未知；没有自动重放");
            value["updated_at"] = json!(now());
            db.execute(
                "UPDATE capability_calls SET body=?2 WHERE id=?1",
                params![text(&value, "id")?, value.to_string()],
            )?;
            affected.insert(text(&value, "task_id")?.to_owned());
        }
    }
    for id in affected {
        let mut task = task_record(db, &id)?;
        refresh_calls(db, &mut task)?;
        put(db, "tasks", &task)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }
    fn envelope(command: Value) -> Value {
        json!({"id":new_id(),"command":command})
    }
    fn send(store: &mut Store, command: Value) -> Value {
        store.capability_command(&envelope(command), &[]).unwrap()
    }
    fn program() -> String {
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
    fn definition() -> Value {
        json!({"name":"夹具服务","program":program(),"args":[],"protocol":"modern2026","enabled":false,"credential_refs":{}})
    }
    fn skill(store: &mut Store, content: &str) -> Value {
        send(
            store,
            json!({"type":"save_skill","name":"只读技能","content":content}),
        )
    }
    fn enable(store: &mut Store, receipt: &Value, kind: &str, enabled: bool) -> Value {
        send(
            store,
            json!({"type":"set_capability_enabled","kind":kind,"id":receipt["capability_id"],"expected_revision":receipt["revision"],"enabled":enabled}),
        )
    }
    fn tool() -> McpToolDefinition {
        McpToolDefinition {
            name: "fixture_echo".into(),
            description: "仅测试工具".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            schema_digest: "UNTRUSTED".into(),
            annotations: json!({"readOnlyHint":true}),
        }
    }
    fn mcp(store: &mut Store) -> Value {
        send(
            store,
            json!({"type":"save_mcp_server","definition":definition()}),
        )
    }
    fn probe(store: &mut Store, receipt: &Value) -> Value {
        send(
            store,
            json!({"type":"probe_mcp_server","id":receipt["capability_id"],"expected_revision":receipt["revision"],"confirm_start":true}),
        )
    }
    fn ready(store: &mut Store) -> Value {
        let receipt = mcp(store);
        let receipt = enable(store, &receipt, "mcp", true);
        let receipt = probe(store, &receipt);
        let id = receipt["capability_id"].as_str().unwrap();
        let revision = receipt["revision"].as_u64().unwrap();
        assert!(store.claim_pending_probe(id, revision).unwrap().is_some());
        assert!(store
            .finish_probe(id, revision, Ok(vec![tool()]), &[])
            .unwrap());
        receipt
    }
    fn task(store: &mut Store, servers: Vec<String>, skills: Vec<String>) -> String {
        let id = new_id();
        let mut controls = TaskControls::default();
        controls.capabilities.mcp_servers = servers;
        controls.capabilities.skills = skills;
        controls.permissions.managed_tools = harness_core::PermissionMode::Allow;
        let mut value = json!({"id":id,"session_id":"fixture-session","status":"running","task_kind":"main","agent":{"executor":"native"},"controls":controls,"supplements":[]});
        let tx = store.db.transaction().unwrap();
        put(&tx,"sessions",&json!({"id":"fixture-session","title":"fixture","project_path":std::env::temp_dir(),"messages":[],"created_at":now(),"updated_at":now()})).unwrap();
        snapshot_task_capabilities(&tx, &mut value).unwrap();
        put(&tx, "tasks", &value).unwrap();
        tx.commit().unwrap();
        id
    }
    #[test]
    fn command_dedup_and_revision_conflict_are_durable() {
        let dir = std::env::temp_dir().join(format!("capability-{}.db", new_id()));
        let request =
            envelope(json!({"type":"save_skill","name":"技能","content":"private first body"}));
        let first;
        {
            let mut store = Store::open(&dir).unwrap();
            first = store.capability_command(&request, &[]).unwrap();
            assert_eq!(first, store.capability_command(&request, &[]).unwrap());
            let mut conflict = request.clone();
            conflict["command"]["content"] = json!("other");
            assert!(store.capability_command(&conflict, &[]).is_err());
            let edit = envelope(
                json!({"type":"save_skill","id":first["capability_id"],"expected_revision":0,"name":"技能","content":"bad stale"}),
            );
            assert!(store.capability_command(&edit, &[]).is_err());
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            store.existing_capability_receipt(&request).unwrap(),
            Some(first)
        );
        assert_eq!(store.capabilities_registry().unwrap().skills.len(), 1);
        drop(store);
        let _ = std::fs::remove_file(dir);
    }
    #[test]
    fn imports_are_private_pinned_snapshots_not_repeat_reads() {
        let mut store = store();
        let source = std::env::temp_dir()
            .join("SKILL.md")
            .to_string_lossy()
            .into_owned();
        let request =
            envelope(json!({"type":"import_skill","name":"导入技能","source_path":source}));
        assert!(store.capability_command(&request, &[]).is_err());
        let prepared = PreparedSkillImport {
            content: "PRIVATE-SKILL-CANARY".into(),
            source: source.clone(),
        };
        let receipt = store
            .capability_import_command(&request, &prepared, &[])
            .unwrap();
        let changed = PreparedSkillImport {
            content: "changed external file".into(),
            source,
        };
        assert_eq!(
            receipt,
            store
                .capability_import_command(&request, &changed, &[])
                .unwrap()
        );
        assert_eq!(
            store.existing_capability_receipt(&request).unwrap(),
            Some(receipt.clone())
        );
        let receipt = enable(&mut store, &receipt, "skill", true);
        let id = task(
            &mut store,
            vec![],
            vec![receipt["capability_id"].as_str().unwrap().into()],
        );
        let private = store.task_capability_state(&id).unwrap();
        assert_eq!(private.skills[0].content, "PRIVATE-SKILL-CANARY");
        assert!(!store
            .snapshot()
            .unwrap()
            .to_string()
            .contains("PRIVATE-SKILL-CANARY"));
        let commands: String = store
            .db
            .query_row(
                "SELECT group_concat(digest || receipt) FROM capability_commands",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!commands.contains("PRIVATE-SKILL-CANARY"));
    }
    #[test]
    fn skill_edits_keep_old_body_but_revocation_is_permanent_for_task() {
        let mut store = store();
        let receipt = skill(&mut store, "old body");
        let receipt = enable(&mut store, &receipt, "skill", true);
        let id = receipt["capability_id"].as_str().unwrap().to_owned();
        let task_id = task(&mut store, vec![], vec![id.clone()]);
        let edited = send(
            &mut store,
            json!({"type":"save_skill","id":id,"expected_revision":receipt["revision"],"name":"新名称","content":"new body"}),
        );
        store.check_task_capabilities(&task_id).unwrap();
        assert_eq!(
            store.task_capability_state(&task_id).unwrap().skills[0].content,
            "old body"
        );
        let newer_task = task(&mut store, vec![], vec![id]);
        assert_eq!(
            store.task_capability_state(&newer_task).unwrap().skills[0].content,
            "new body"
        );
        let disabled = enable(&mut store, &edited, "skill", false);
        assert!(store.check_task_capabilities(&task_id).is_err());
        enable(&mut store, &disabled, "skill", true);
        assert!(store.check_task_capabilities(&task_id).is_err());
        assert!(store.check_task_capabilities(&newer_task).is_err());
    }
    #[test]
    fn probe_is_once_and_revision_fenced_and_never_resurrects_deleted() {
        let mut store = store();
        let receipt = mcp(&mut store);
        assert!(!store.capabilities_registry().unwrap().mcp_servers[0].enabled);
        assert!(store
            .claim_pending_probe(receipt["capability_id"].as_str().unwrap(), 1)
            .unwrap()
            .is_none());
        let denied = envelope(
            json!({"type":"probe_mcp_server","id":receipt["capability_id"],"expected_revision":receipt["revision"],"confirm_start":false}),
        );
        assert!(store.capability_command(&denied, &[]).is_err());
        let receipt = probe(&mut store, &receipt);
        let id = receipt["capability_id"].as_str().unwrap();
        let revision = receipt["revision"].as_u64().unwrap();
        let work = store.claim_pending_probe(id, revision).unwrap().unwrap();
        assert_eq!(work.id, id);
        assert_eq!(work.revision, revision);
        assert_eq!(work.definition.name, "夹具服务");
        assert!(store.claim_pending_probe(id, revision).unwrap().is_none());
        send(
            &mut store,
            json!({"type":"delete_capability","id":id,"kind":"mcp","expected_revision":revision}),
        );
        assert!(!store
            .finish_probe(id, revision, Ok(vec![tool()]), &[])
            .unwrap());
        assert!(store
            .capabilities_registry()
            .unwrap()
            .mcp_servers
            .is_empty());
        assert!(store
            .capability_command(
                &envelope(json!({"type":"save_mcp_server","id":id,"definition":definition()})),
                &[]
            )
            .is_err());
    }
    #[test]
    fn catalog_is_validated_and_digest_recomputed_before_ready() {
        let mut store = store();
        let receipt = ready(&mut store);
        let id = receipt["capability_id"].as_str().unwrap().to_owned();
        let task_id = task(&mut store, vec![id], vec![]);
        let pinned = store.task_capability_state(&task_id).unwrap();
        assert_eq!(
            pinned.servers[0].catalog[0].schema_digest,
            digest(&tool().input_schema)
        );
        assert_eq!(pinned.servers[0].definition.name, "夹具服务");
        assert_eq!(
            pinned.servers[0].reference.revision,
            receipt["revision"].as_u64().unwrap()
        );
        {
            let mut invalid = tool();
            let r = probe(&mut store, &receipt);
            let id = r["capability_id"].as_str().unwrap();
            let rev = r["revision"].as_u64().unwrap();
            store.claim_pending_probe(id, rev).unwrap();
            invalid.input_schema = json!({"type":"object","$ref":"https://example.invalid/schema"});
            assert!(store.finish_probe(id, rev, Ok(vec![invalid]), &[]).unwrap());
            assert_eq!(
                store.capabilities_registry().unwrap().mcp_servers[0].status,
                "unavailable"
            );
        }
        assert!(store.check_task_capabilities(&task_id).is_err());
    }
    #[test]
    fn unknown_writes_and_pending_probes_are_not_replayed_on_restart() {
        let dir = std::env::temp_dir().join(format!("capability-recovery-{}.db", new_id()));
        let task_id;
        let call_id;
        let pending;
        {
            let mut store = Store::open(&dir).unwrap();
            let receipt = ready(&mut store);
            let server_id = receipt["capability_id"].as_str().unwrap().to_owned();
            task_id = task(&mut store, vec![server_id.clone()], vec![]);
            call_id = store
                .begin_capability_call(
                    &task_id,
                    &server_id,
                    receipt["revision"].as_u64().unwrap(),
                    "fixture_echo",
                    &"a".repeat(64),
                )
                .unwrap();
            let p = mcp(&mut store);
            pending = probe(&mut store, &p);
        }
        let mut store = Store::open(&dir).unwrap();
        let observed = task_record(&store.db, &task_id).unwrap();
        assert_eq!(observed["capability_calls"][0]["status"], "unknown");
        assert!(!store
            .finish_capability_call(&call_id, "completed", None)
            .unwrap());
        assert!(store
            .claim_pending_probe(
                pending["capability_id"].as_str().unwrap(),
                pending["revision"].as_u64().unwrap()
            )
            .unwrap()
            .is_none());
        assert_eq!(
            store.capabilities_registry().unwrap().mcp_servers[1].status,
            "unavailable"
        );
        drop(store);
        let _ = std::fs::remove_file(dir);
    }
    #[test]
    fn mcp_revisions_revoke_and_call_records_are_bounded_terminal() {
        let mut store = store();
        let receipt = ready(&mut store);
        let id = receipt["capability_id"].as_str().unwrap().to_owned();
        let revision = receipt["revision"].as_u64().unwrap();
        let task_id = task(&mut store, vec![id.clone()], vec![]);
        assert!(store
            .begin_capability_call(&task_id, &id, revision, "not_registered", &"a".repeat(64))
            .is_err());
        assert!(store
            .begin_capability_call(&task_id, &id, revision, "fixture_echo", "raw-arguments")
            .is_err());
        for _ in 0..128 {
            let call = store
                .begin_capability_call(&task_id, &id, revision, "fixture_echo", &"b".repeat(64))
                .unwrap();
            assert!(store
                .finish_capability_call(&call, "completed", None)
                .unwrap());
            assert!(!store
                .finish_capability_call(&call, "unknown", None)
                .unwrap());
        }
        assert!(store
            .begin_capability_call(&task_id, &id, revision, "fixture_echo", &"a".repeat(64))
            .is_err());
        assert_eq!(
            task_record(&store.db, &task_id).unwrap()["capability_calls"]
                .as_array()
                .unwrap()
                .len(),
            128
        );
        enable(&mut store, &receipt, "mcp", false);
        assert!(store.check_task_capabilities(&task_id).is_err());
    }
    #[test]
    fn call_intent_and_cleanup_ownership_commit_before_cancellation() {
        let mut store = store();
        let receipt = ready(&mut store);
        let id = receipt["capability_id"].as_str().unwrap();
        let task_id = task(&mut store, vec![id.into()], vec![]);
        let call = store
            .begin_capability_call(
                &task_id,
                id,
                receipt["revision"].as_u64().unwrap(),
                "fixture_echo",
                &"b".repeat(64),
            )
            .unwrap();
        let mut value = task_record(&store.db, &task_id).unwrap();
        assert_eq!(value["execution_cleanup"], "pending");
        assert_eq!(value["capability_calls"][0]["status"], "sending");
        value["status"] = json!("cancelled");
        put(&store.db, "tasks", &value).unwrap();
        // A pre-spawn cancelled scope reports NotStarted and may settle both
        // records without reviving the cancelled task.
        store.set_execution_cleanup(&task_id, false).unwrap();
        store
            .finish_capability_call(&call, "not_sent", None)
            .unwrap();
        let value = task_record(&store.db, &task_id).unwrap();
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["execution_cleanup"], "settled");
        assert_eq!(value["capability_calls"][0]["status"], "not_sent");
    }

    #[test]
    fn invalid_secrets_unknown_fields_and_limits_never_persist() {
        let mut store = store();
        let known = vec!["fixture-canary-key".to_owned()];
        for command in [
            json!({"type":"save_skill","name":"test","content":"fixture-canary-key"}),
            json!({"type":"save_mcp_server","definition":{"name":"test","program":program(),"args":["--key=fixture-canary-key"]}}),
            json!({"type":"save_skill","name":"test","content":"valid","unexpected":"ignored?"}),
        ] {
            let error = store
                .capability_command(&envelope(command), &known)
                .unwrap_err()
                .to_string();
            assert!(!error.contains(&known[0]));
        }
        let mut def = definition();
        def["enabled"] = json!(true);
        assert!(store
            .capability_command(
                &envelope(json!({"type":"save_mcp_server","definition":def})),
                &[]
            )
            .is_err());
        let mut def = definition();
        def["credential_refs"] = json!({"TOKEN":"not-yet-supported"});
        assert!(store
            .capability_command(
                &envelope(json!({"type":"save_mcp_server","definition":def})),
                &[]
            )
            .is_err());
        assert!(store
            .capability_command(
                &envelope(
                    json!({"type":"save_skill","name":"test","content":"x".repeat(MAX_BODY+1)})
                ),
                &[]
            )
            .is_err());
        assert!(store.capabilities_registry().unwrap().skills.is_empty());
        assert!(store
            .capabilities_registry()
            .unwrap()
            .mcp_servers
            .is_empty());
        for _ in 0..MAX_SKILLS {
            skill(&mut store, "bounded body");
        }
        assert!(store
            .capability_command(
                &envelope(json!({"type":"save_skill","name":"full","content":"body"})),
                &[]
            )
            .is_err());
        for _ in 0..MAX_MCP {
            mcp(&mut store);
        }
        assert!(store
            .capability_command(
                &envelope(json!({"type":"save_mcp_server","definition":definition()})),
                &[]
            )
            .is_err());
    }
    #[test]
    fn missing_private_snapshot_and_codex_mcp_fail_closed() {
        let mut store = store();
        let receipt = ready(&mut store);
        let id = receipt["capability_id"].as_str().unwrap().to_owned();
        let mut value = json!({"id":new_id(),"agent":{"executor":"codex"},"controls":{"capabilities":{"mcp_servers":[id],"skills":[]}}});
        assert!(snapshot_task_capabilities(&store.db, &mut value).is_err());
        let task_id = task(&mut store, vec![id], vec![]);
        store
            .db
            .execute("DELETE FROM task_capabilities WHERE id=?1", [&task_id])
            .unwrap();
        assert!(store.check_task_capabilities(&task_id).is_err());
        assert!(store.task_capability_state(&task_id).is_err());
    }
    #[test]
    fn probe_secret_echo_and_duplicate_catalog_are_rejected_without_public_payload() {
        let mut store = store();
        let receipt = mcp(&mut store);
        let receipt = probe(&mut store, &receipt);
        let id = receipt["capability_id"].as_str().unwrap();
        let rev = receipt["revision"].as_u64().unwrap();
        store.claim_pending_probe(id, rev).unwrap();
        let mut t = tool();
        t.description = "fixture-known-key".into();
        store
            .finish_probe(id, rev, Ok(vec![t]), &["fixture-known-key".into()])
            .unwrap();
        assert!(!store
            .snapshot()
            .unwrap()
            .to_string()
            .contains("fixture-known-key"));
        assert_eq!(
            store.capabilities_registry().unwrap().mcp_servers[0].status,
            "unavailable"
        );
        let receipt = probe(&mut store, &receipt);
        let rev = receipt["revision"].as_u64().unwrap();
        store.claim_pending_probe(id, rev).unwrap();
        store
            .finish_probe(id, rev, Ok(vec![tool(), tool()]), &[])
            .unwrap();
        assert_eq!(
            store.capabilities_registry().unwrap().mcp_servers[0].status,
            "unavailable"
        );
    }
    #[test]
    fn escaped_known_secrets_are_rejected_in_raw_keys_values_and_imports() {
        let mut store = store();
        for secret in ["canary\"quoted", "canary\\backslash", "canary\nnewline"] {
            let known = vec![secret.to_owned()];
            for value in [
                json!({"nested":[{"content":secret}]}),
                json!({secret:"innocent"}),
            ] {
                assert!(private_text(&value, &known).is_err());
            }
            let command = envelope(
                json!({"type":"save_skill","name":"fixture","content":format!("prefix {secret} suffix")}),
            );
            assert!(store.capability_command(&command, &known).is_err());
            let mut def = definition();
            def["args"] = json!([format!("--option={secret}")]);
            assert!(store
                .capability_command(
                    &envelope(json!({"type":"save_mcp_server","definition":def})),
                    &known
                )
                .is_err());
            let source = std::env::temp_dir()
                .join("SKILL.md")
                .to_string_lossy()
                .into_owned();
            let import =
                envelope(json!({"type":"import_skill","name":"fixture","source_path":source}));
            assert!(store
                .capability_import_command(
                    &import,
                    &PreparedSkillImport {
                        source,
                        content: secret.into()
                    },
                    &known
                )
                .is_err());
        }
        assert!(store.capabilities_registry().unwrap().skills.is_empty());
        assert!(store
            .capabilities_registry()
            .unwrap()
            .mcp_servers
            .is_empty());
        assert_eq!(
            store
                .db
                .query_row("SELECT COUNT(*) FROM capability_commands", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn task_aggregate_limits_and_legacy_empty_snapshots() {
        let mut store = store();
        let mut skills = Vec::new();
        for _ in 0..3 {
            let r = skill(&mut store, &"x".repeat(64 * 1024));
            let r = enable(&mut store, &r, "skill", true);
            skills.push(r["capability_id"].as_str().unwrap().to_owned());
        }
        let mut value = json!({"id":new_id(),"agent":{"executor":"native"},"controls":{"capabilities":{"skills":skills}}});
        assert!(snapshot_task_capabilities(&store.db, &mut value).is_err());
        value["controls"]["capabilities"]["skills"]
            .as_array_mut()
            .unwrap()
            .pop();
        snapshot_task_capabilities(&store.db, &mut value).unwrap();
        let mut servers = Vec::new();
        for _ in 0..2 {
            let r = mcp(&mut store);
            let r = enable(&mut store, &r, "mcp", true);
            let r = probe(&mut store, &r);
            let id = r["capability_id"].as_str().unwrap();
            let rev = r["revision"].as_u64().unwrap();
            store.claim_pending_probe(id, rev).unwrap();
            let tools = (0..65)
                .map(|i| {
                    let mut t = tool();
                    t.name = format!("fixture_{i}");
                    t
                })
                .collect();
            store.finish_probe(id, rev, Ok(tools), &[]).unwrap();
            servers.push(id.to_owned());
        }
        let mut value = json!({"id":new_id(),"agent":{"executor":"native"},"controls":{"capabilities":{"mcp_servers":servers}}});
        assert!(snapshot_task_capabilities(&store.db, &mut value).is_err());
        let legacy_id = new_id();
        put(&store.db, "tasks", &json!({"id":legacy_id})).unwrap();
        store.check_task_capabilities(&legacy_id).unwrap();
        let legacy = store.task_capability_state(&legacy_id).unwrap();
        assert!(legacy.servers.is_empty() && legacy.skills.is_empty());
    }
}
