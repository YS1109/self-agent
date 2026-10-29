//! Application integration for the versioned storage layer. No private backup
//! arguments or raw Store records are included in diagnostics/receipts.
use super::*;
use crate::reliability::{diagnostics, jobs, storage};

// Required application shape is compiled from the baseline in prepare_database and
// capabilities/provider/QQ/backup migrations. Never derive requirements from an imported DB.
// The drift test compares this list to a freshly migrated actual Store.
type ColumnSpec = (&'static str, &'static str, bool, i64);
const REQUIRED_SCHEMA: &[(&str, &[ColumnSpec])] = &[
    (
        "backup_jobs",
        &[
            ("id", "TEXT", false, 1),
            ("fingerprint", "TEXT", true, 0),
            ("parent", "TEXT", true, 0),
            ("name", "TEXT", true, 0),
            ("receipt", "TEXT", true, 0),
            ("claim_token", "TEXT", false, 0),
        ],
    ),
    (
        "capability_calls",
        &[
            ("id", "TEXT", false, 1),
            ("task_id", "TEXT", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "capability_commands",
        &[
            ("id", "TEXT", false, 1),
            ("digest", "TEXT", true, 0),
            ("receipt", "TEXT", true, 0),
        ],
    ),
    (
        "commands",
        &[
            ("id", "TEXT", false, 1),
            ("envelope", "TEXT", true, 0),
            ("receipt", "TEXT", true, 0),
        ],
    ),
    (
        "coordinator_jobs",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "coordinator_providers",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "credential_operations",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "deliveries",
        &[
            ("id", "TEXT", false, 1),
            ("task_id", "TEXT", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "events",
        &[
            ("sequence", "INTEGER", false, 1),
            ("id", "TEXT", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "execution_requests",
        &[
            ("id", "TEXT", false, 1),
            ("task_id", "TEXT", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "managed_capabilities",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "mcp_credential_bindings",
        &[
            ("id", "TEXT", false, 1),
            ("server_id", "TEXT", true, 0),
            ("slot", "TEXT", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "mcp_credential_operations",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "named_providers",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "plan_changes",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "plan_providers",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "plan_role_providers",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "plans",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "provider_commands",
        &[
            ("id", "TEXT", false, 1),
            ("digest", "TEXT", true, 0),
            ("receipt", "TEXT", true, 0),
        ],
    ),
    (
        "qq_admin_commands",
        &[
            ("id", "TEXT", false, 1),
            ("digest", "TEXT", true, 0),
            ("receipt", "TEXT", true, 0),
        ],
    ),
    (
        "qq_bindings",
        &[
            ("principal_id", "TEXT", false, 1),
            ("identity_key", "TEXT", true, 0),
            ("app_id", "TEXT", true, 0),
            ("revision", "INTEGER", true, 0),
            ("status", "TEXT", true, 0),
            ("expires_at_ms", "INTEGER", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "qq_checkpoint",
        &[
            ("singleton", "INTEGER", false, 1),
            ("config_id", "TEXT", true, 0),
            ("config_revision", "INTEGER", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "qq_configuration",
        &[
            ("singleton", "INTEGER", false, 1),
            ("id", "TEXT", true, 0),
            ("revision", "INTEGER", true, 0),
            ("app_id", "TEXT", true, 0),
            ("enabled", "INTEGER", true, 0),
            ("mode", "TEXT", true, 0),
            ("reply_policy", "TEXT", true, 0),
            ("credential_ref", "TEXT", false, 0),
        ],
    ),
    (
        "qq_connection_state",
        &[
            ("singleton", "INTEGER", false, 1),
            ("config_id", "TEXT", true, 0),
            ("config_revision", "INTEGER", true, 0),
            ("state", "TEXT", true, 0),
            ("reason", "TEXT", false, 0),
            ("updated_at_ms", "INTEGER", false, 0),
        ],
    ),
    (
        "qq_credential_operations",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "qq_event_aliases",
        &[
            ("app_id", "TEXT", true, 1),
            ("alias", "TEXT", true, 2),
            ("inbox_id", "TEXT", true, 0),
        ],
    ),
    (
        "qq_ignored_inputs",
        &[
            ("singleton", "INTEGER", false, 1),
            ("count", "INTEGER", true, 0),
            ("latest_platform_ms", "INTEGER", true, 0),
            ("updated_at_ms", "INTEGER", true, 0),
            ("clock_fault_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "qq_inbox",
        &[
            ("inbox_id", "TEXT", false, 1),
            ("app_id", "TEXT", true, 0),
            ("message_id", "TEXT", true, 0),
            ("canonical_hash", "TEXT", true, 0),
            ("command_id", "TEXT", true, 0),
            ("status", "TEXT", true, 0),
            ("route_id", "TEXT", false, 0),
            ("body", "TEXT", true, 0),
            ("receipt", "TEXT", false, 0),
            ("received_at_ms", "INTEGER", true, 0),
            ("updated_at_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "qq_notifications",
        &[
            ("logical_key", "TEXT", false, 1),
            ("source_kind", "TEXT", true, 0),
            ("source_id", "TEXT", true, 0),
            ("outbox_id", "TEXT", false, 0),
            ("state", "TEXT", true, 0),
        ],
    ),
    (
        "qq_outbox",
        &[
            ("id", "TEXT", false, 1),
            ("route_id", "TEXT", true, 0),
            ("logical_key", "TEXT", true, 0),
            ("kind", "TEXT", true, 0),
            ("payload", "TEXT", true, 0),
            ("content_hash", "TEXT", true, 0),
            ("msg_seq", "INTEGER", true, 0),
            ("status", "TEXT", true, 0),
            ("attempts", "INTEGER", true, 0),
            ("retry_at_ms", "INTEGER", true, 0),
            ("claim_nonce", "TEXT", false, 0),
            ("platform_id", "TEXT", false, 0),
            ("reason", "TEXT", false, 0),
            ("created_at_ms", "INTEGER", true, 0),
            ("updated_at_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "qq_pairing_admissions",
        &[
            ("code_hash", "TEXT", false, 1),
            ("inbox_id", "TEXT", true, 0),
        ],
    ),
    (
        "qq_pairings",
        &[
            ("id", "TEXT", false, 1),
            ("app_id", "TEXT", true, 0),
            ("code_hash", "TEXT", true, 0),
            ("expires_at_ms", "INTEGER", true, 0),
            ("claimed_binding_id", "TEXT", false, 0),
            ("created_at_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "qq_reply_routes",
        &[
            ("route_id", "TEXT", false, 1),
            ("principal_id", "TEXT", true, 0),
            ("binding_revision", "INTEGER", true, 0),
            ("expires_at_ms", "INTEGER", true, 0),
            ("next_msg_seq", "INTEGER", true, 0),
            ("max_messages", "INTEGER", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "qq_selections",
        &[("scope", "TEXT", false, 1), ("session_id", "TEXT", true, 0)],
    ),
    (
        "qq_session_acl",
        &[
            ("principal_id", "TEXT", true, 1),
            ("session_id", "TEXT", true, 2),
            ("created_at_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "selections",
        &[("source", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "session_contexts",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "sessions",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "settings",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "task_capabilities",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "task_changes",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "task_mcp_credentials",
        &[
            ("id", "TEXT", false, 1),
            ("task_id", "TEXT", true, 0),
            ("server_id", "TEXT", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "task_providers",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "tasks",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "gateway_admin_commands",
        &[
            ("id", "TEXT", false, 1),
            ("digest", "TEXT", true, 0),
            ("receipt", "TEXT", true, 0),
        ],
    ),
    (
        "gateway_bindings",
        &[
            ("principal_id", "TEXT", false, 1),
            ("device_id", "TEXT", true, 0),
            ("config_revision", "INTEGER", true, 0),
            ("revision", "INTEGER", true, 0),
            ("status", "TEXT", true, 0),
            ("expires_at_ms", "INTEGER", true, 0),
            ("body", "TEXT", true, 0),
        ],
    ),
    (
        "gateway_configuration",
        &[
            ("singleton", "INTEGER", false, 1),
            ("id", "TEXT", true, 0),
            ("revision", "INTEGER", true, 0),
            ("device_id", "TEXT", true, 0),
            ("origin", "TEXT", true, 0),
            ("enabled", "INTEGER", true, 0),
            ("credential_ref", "TEXT", false, 0),
        ],
    ),
    (
        "gateway_connection",
        &[
            ("singleton", "INTEGER", false, 1),
            ("config_id", "TEXT", true, 0),
            ("revision", "INTEGER", true, 0),
            ("state", "TEXT", true, 0),
            ("updated_at_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "gateway_credential_operations",
        &[("id", "TEXT", false, 1), ("body", "TEXT", true, 0)],
    ),
    (
        "gateway_pairings",
        &[
            ("id", "TEXT", false, 1),
            ("device_id", "TEXT", true, 0),
            ("config_revision", "INTEGER", true, 0),
            ("code_hash", "TEXT", true, 0),
            ("expires_at_ms", "INTEGER", true, 0),
            ("status", "TEXT", true, 0),
            ("claimed_principal", "TEXT", false, 0),
            ("created_at_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "gateway_receipts",
        &[
            ("command_id", "TEXT", false, 1),
            ("device_id", "TEXT", true, 0),
            ("config_revision", "INTEGER", true, 0),
            ("principal_id", "TEXT", true, 0),
            ("binding_revision", "INTEGER", true, 0),
            ("session_id", "TEXT", true, 0),
            ("request_hash", "TEXT", true, 0),
            ("receipt", "TEXT", true, 0),
            ("invalidated", "INTEGER", true, 0),
            ("created_at_ms", "INTEGER", true, 0),
        ],
    ),
    (
        "gateway_text_resources",
        &[
            ("id", "TEXT", false, 1),
            ("task_id", "TEXT", true, 0),
            ("session_id", "TEXT", true, 0),
            ("kind", "TEXT", true, 0),
            ("name", "TEXT", true, 0),
            ("body", "TEXT", true, 0),
            ("sha256", "TEXT", true, 0),
            ("truncated", "INTEGER", true, 0),
        ],
    ),
];
const REQUIRED_UNIQUE_KEYS: &[(&str, &[&str])] = &[
    ("gateway_configuration", &["id"]),
    ("gateway_pairings", &["code_hash"]),
    ("gateway_text_resources", &["task_id", "kind"]),
    ("deliveries", &["task_id"]),
    ("events", &["id"]),
    ("mcp_credential_bindings", &["server_id", "slot"]),
    ("qq_bindings", &["identity_key"]),
    ("qq_configuration", &["id"]),
    ("qq_inbox", &["app_id", "message_id"]),
    ("qq_inbox", &["command_id"]),
    ("qq_outbox", &["logical_key"]),
    ("qq_outbox", &["route_id", "logical_key"]),
    ("qq_pairings", &["code_hash"]),
    ("task_mcp_credentials", &["task_id", "server_id"]),
];

/// The event cursor depends on SQLite's non-reusing AUTOINCREMENT allocator.
/// Compare tokens to compiled DDL rather than searching an attacker-controlled
/// SQL substring (a comment or another identifier could contain the keyword).
fn event_schema_tokens(sql: &str) -> Result<Vec<String>> {
    if sql.len() > 4096 {
        bail!("事件表定义超出支持的长度");
    }
    let bytes = sql.as_bytes();
    let mut offset = 0;
    let mut tokens = Vec::new();
    while offset < bytes.len() {
        let ch = bytes[offset];
        if ch.is_ascii_whitespace() {
            offset += 1;
            continue;
        }
        if matches!(ch, b'(' | b')' | b',') {
            tokens.push(char::from(ch).to_string());
            offset += 1;
            continue;
        }
        if matches!(ch, b'"' | b'`' | b'[') {
            let close = if ch == b'[' { b']' } else { ch };
            offset += 1;
            let start = offset;
            while offset < bytes.len() && bytes[offset] != close {
                if !bytes[offset].is_ascii_alphanumeric() && bytes[offset] != b'_' {
                    bail!("事件表定义包含不支持的标识符");
                }
                offset += 1;
            }
            if offset == bytes.len() {
                bail!("事件表定义引号不完整");
            }
            let identifier = sql[start..offset].to_ascii_lowercase();
            if !matches!(identifier.as_str(), "events" | "sequence" | "id" | "body") {
                bail!("事件表定义标识符无效");
            }
            tokens.push(identifier);
            offset += 1;
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == b'_' {
            let start = offset;
            offset += 1;
            while offset < bytes.len()
                && (bytes[offset].is_ascii_alphanumeric() || bytes[offset] == b'_')
            {
                offset += 1;
            }
            tokens.push(sql[start..offset].to_ascii_lowercase());
            continue;
        }
        // Comments, string literals, semicolons and expression tokens are not
        // part of either current or supported legacy ALTER-generated DDL.
        bail!("事件表定义包含不支持的 SQL 标记");
    }
    Ok(tokens)
}
fn validate_event_allocator(db: &Connection) -> Result<()> {
    const TRUSTED: &str = "CREATE TABLE events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,body TEXT NOT NULL)";
    let actual: String = db.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='events'",
        [],
        |r| r.get(0),
    )?;
    if event_schema_tokens(&actual)? != event_schema_tokens(TRUSTED)? {
        bail!("事件表不具备受支持的持久游标分配结构，未安装备份");
    }
    Ok(())
}

fn validate_schema_version(db: &Connection, gateway: bool) -> Result<()> {
    validate_event_allocator(db)?;
    for (table, expected) in REQUIRED_SCHEMA {
        if !gateway && table.starts_with("gateway_") {
            continue;
        }
        let kind: Option<String> = db
            .query_row(
                "SELECT type FROM sqlite_schema WHERE name=?1",
                [table],
                |r| r.get(0),
            )
            .optional()?;
        if kind.as_deref() != Some("table") {
            bail!("数据库缺少必需应用表，未安装备份");
        }
        let mut query = db.prepare(&format!("PRAGMA table_xinfo({table})"))?;
        let columns = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns.len() != expected.len()
            || !columns.iter().zip(*expected).all(
                |((actual, ty, required, pk, hidden), (name, kind, not_null, primary_key))| {
                    actual == name
                        && ty.eq_ignore_ascii_case(kind)
                        && required == not_null
                        && pk == primary_key
                        && *hidden == 0
                },
            )
        {
            bail!("数据库字段集合、顺序、类型或约束不匹配，未安装备份");
        }
    }
    for (table, expected) in REQUIRED_UNIQUE_KEYS {
        if !gateway && table.starts_with("gateway_") {
            continue;
        }
        let mut query = db.prepare(&format!("PRAGMA index_list({table})"))?;
        let indexes = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut found = false;
        for (name, unique, partial) in indexes {
            if !unique || partial {
                continue;
            }
            // Parameterized pragma prevents imported index names becoming SQL text.
            let mut query = db.prepare(
                "SELECT name,coll,desc FROM pragma_index_xinfo(?1) WHERE key=1 ORDER BY seqno",
            )?;
            let columns = query
                .query_map([name], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if columns.len() == expected.len()
                && columns
                    .iter()
                    .zip(*expected)
                    .all(|((name, coll, descending), expected)| {
                        name.as_deref() == Some(*expected) && coll == "BINARY" && !descending
                    })
            {
                found = true;
                break;
            }
        }
        if !found {
            bail!("数据库缺少必需唯一约束，未安装备份");
        }
    }
    Ok(())
}

fn validate_public_record(table: &str, body: &str) -> Result<()> {
    // Use the real shared types, including their explicit serde legacy defaults.
    // No blanket repair/default is introduced for malformed required fields.
    match table {
        "sessions" => {
            serde_json::from_str::<harness_core::Session>(body)?;
        }
        "tasks" => {
            serde_json::from_str::<harness_core::TaskRun>(body)?;
        }
        "plans" => {
            serde_json::from_str::<harness_core::Plan>(body)?;
        }
        "plan_changes" => {
            serde_json::from_str::<harness_core::PlanChange>(body)?;
        }
        "deliveries" => {
            serde_json::from_str::<harness_core::Delivery>(body)?;
        }
        "events" => {
            serde_json::from_str::<RunEvent>(body)?;
        }
        _ => {}
    }
    Ok(())
}

fn nested<T: serde::de::DeserializeOwned>(value: &Value) -> Result<()> {
    serde_json::from_value::<T>(value.clone())
        .map(|_| ())
        .map_err(|_| anyhow!("恢复嵌套记录结构无效"))
}

fn validate_private_record(table: &str, value: &Value) -> Result<()> {
    match table {
        "managed_capabilities" if value["deleted"] != true => match value["kind"].as_str() {
            Some("mcp") => {
                nested::<harness_core::McpServerDefinition>(&value["definition"])?;
                if let Some(catalog) = value.get("catalog") {
                    nested::<Vec<harness_core::McpToolDefinition>>(catalog)?;
                }
            }
            Some("skill") => {
                for field in ["name", "source", "content", "content_digest"] {
                    if !value[field].is_string() {
                        bail!("恢复技能字段无效");
                    }
                }
            }
            _ => bail!("恢复能力类型无效"),
        },
        "named_providers" | "task_providers" | "coordinator_providers" | "plan_providers" => {
            nested::<harness_core::ProviderDefinition>(&value["definition"])?;
        }
        "plan_role_providers" => {
            for role in value["roles"]
                .as_object()
                .context("恢复角色服务快照无效")?
                .values()
            {
                nested::<harness_core::ProviderDefinition>(&role["definition"])?;
            }
        }
        "task_capabilities" => {
            for server in value["servers"].as_array().context("恢复 MCP 快照无效")? {
                nested::<harness_core::CapabilityReference>(&server["reference"])?;
                nested::<harness_core::McpServerDefinition>(&server["definition"])?;
                nested::<Vec<harness_core::McpToolDefinition>>(&server["catalog"])?;
            }
            for skill in value["skills"].as_array().context("恢复技能快照无效")? {
                nested::<harness_core::CapabilityReference>(&skill["reference"])?;
                if !skill["source"].is_string() || !skill["content"].is_string() {
                    bail!("恢复技能快照字段无效");
                }
            }
        }
        "credential_operations" => {
            nested::<harness_core::CredentialOperationView>(value)?;
            nested::<harness_core::ProviderDefinition>(&value["definition"])?;
        }
        "mcp_credential_operations" => {
            nested::<harness_core::CredentialOperationView>(value)?;
        }
        "capability_calls" => nested::<harness_core::CapabilityCallRecord>(value)?,
        "execution_requests" => nested::<harness_core::ExecutionRequest>(value)?,
        "coordinator_jobs" => {
            if let Some(agents) = value.get("agents") {
                nested::<Vec<harness_core::AgentProfile>>(agents)?;
            }
            if let Some(controls) = value.get("controls").filter(|v| !v.is_null()) {
                nested::<harness_core::TaskControls>(controls)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn validate_records(db: &Connection) -> Result<()> {
    validate_v2_records(db)?;
    scheduling::validate(db)
}
pub(crate) fn validate_v2_records(db: &Connection) -> Result<()> {
    validate_records_version(db, true)?;
    validate_gateway_records(db)
}
pub(crate) fn validate_v1_records(db: &Connection) -> Result<()> {
    let unexpected: u64 = db.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name LIKE 'gateway_%'",
        [],
        |r| r.get(0),
    )?;
    if unexpected != 0 {
        bail!("旧版本包含未登记的网关结构，未执行升级");
    }
    validate_records_version(db, false)
}
pub(crate) fn restore_gateway(tx: &Transaction<'_>) -> Result<()> {
    super::gateway::restore(tx)
}
fn validate_records_version(db: &Connection, gateway: bool) -> Result<()> {
    validate_schema_version(db, gateway).context("数据库应用结构无效")?;
    for table in [
        "sessions",
        "tasks",
        "plans",
        "plan_changes",
        "coordinator_jobs",
        "events",
        "deliveries",
        "settings",
        "session_contexts",
        "task_changes",
        "execution_requests",
        "managed_capabilities",
        "task_capabilities",
        "capability_calls",
        "named_providers",
        "task_providers",
        "coordinator_providers",
        "plan_providers",
        "plan_role_providers",
        "credential_operations",
        "mcp_credential_bindings",
        "mcp_credential_operations",
        "qq_credential_operations",
        "task_mcp_credentials",
    ] {
        let mut statement = db.prepare(&format!("SELECT id,body FROM {table}"))?;
        let records =
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for record in records {
            let (id, body) = record?;
            let value: Value = serde_json::from_str(&body).context("数据库记录不是有效 JSON")?;
            if !value.is_object() || value.get("id").is_some_and(|v| v.as_str() != Some(&id)) {
                bail!("数据库记录结构或标识不一致，未安装备份");
            }
            validate_private_record(table, &value)?;
            validate_public_record(table, &body).map_err(|_| anyhow!("恢复业务记录结构无效"))?;
            if table == "settings" && id == "provider" {
                serde_json::from_value::<Settings>(value).context("恢复配置结构无效")?;
            }
        }
    }
    // QQ stores its own typed private Operation (owner_kind is added only in its public view).
    // This decoder reads metadata only and never opens or probes the OS vault.
    crate::qq::credentials::operations(db).map_err(|_| anyhow!("恢复 QQ 凭据审计结构无效"))?;
    jobs::list(db)?;
    Ok(())
}

fn gateway_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// Read-only structural validation; never probes/deletes an OS credential or
/// grants access to an imported resource. Runtime rechecks task/session ACLs.
fn validate_gateway_records(db: &Connection) -> Result<()> {
    use crate::gateway::{configuration, persistence::Binding, policy};
    for (table, limit) in [
        ("gateway_configuration", 1),
        ("gateway_connection", 1),
        ("gateway_bindings", 4096),
        ("gateway_pairings", 4096),
        ("gateway_receipts", 10000),
        ("gateway_admin_commands", 10000),
        ("gateway_credential_operations", 512),
        ("gateway_text_resources", 1024),
    ] {
        let count: u64 =
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        if count > limit {
            bail!("网关恢复记录超过容量上限");
        }
    }
    if let Some(c) = configuration::current(db)? {
        if let Some(reference) = c.credential_ref() {
            crate::capabilities::vault::CredentialRef::parse(
                reference,
                &crate::gateway::credential_binding(&c.id, &c.scope_digest)?,
            )?;
        }
    }
    let mut query=db.prepare("SELECT principal_id,device_id,config_revision,revision,status,expires_at_ms,CASE WHEN length(CAST(body AS BLOB))<=262144 THEN body END FROM gateway_bindings")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let device: String = row.get(1)?;
        let config: u64 = row.get(2)?;
        let revision: u64 = row.get(3)?;
        let status: String = row.get(4)?;
        let expires: u64 = row.get(5)?;
        let body: String = row.get(6)?;
        let b: Binding = serde_json::from_str(&body)?;
        policy::identifier(&id)?;
        policy::identifier(&device)?;
        policy::identifier(&b.pairing_id)?;
        if b.principal_id != id
            || b.device_id != device
            || b.config_revision != config
            || b.revision != revision
            || b.status != status
            || b.expires_at_ms != expires
            || config == 0
            || config > 9_007_199_254_740_991
            || revision > 9_007_199_254_740_991
            || !matches!(status.as_str(), "pending" | "active" | "revoked")
            || (status == "pending" && revision != 0)
            || (status == "active" && revision == 0)
            || b.label.trim().is_empty()
            || b.label.len() > 128
        {
            bail!("网关身份索引或记录不一致");
        }
        if let Some(grant) = &b.grant {
            policy::validate_grant(grant, expires.saturating_sub(1))?;
            if grant.expires_at_ms != expires {
                bail!("网关授权期限不一致");
            }
        } else if status == "active" {
            bail!("网关有效身份缺少授权");
        }
    }
    let mut query=db.prepare("SELECT id,CASE WHEN length(CAST(body AS BLOB))<=262144 THEN body END FROM gateway_credential_operations")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let body: String = row.get(1)?;
        let op: Value = serde_json::from_str(&body)?;
        policy::identifier(&id)?;
        let owner = text(&op, "owner_id")?;
        policy::identifier(owner)?;
        let digest = text(&op, "scope_digest")?;
        if op["id"] != id
            || !gateway_digest(digest)
            || !matches!(op["kind"].as_str(), Some("set" | "delete" | "cleanup"))
            || !matches!(
                op["status"].as_str(),
                Some(
                    "pending"
                        | "inflight"
                        | "completed"
                        | "rejected"
                        | "unknown"
                        | "orphaned"
                        | "cleanup_required"
                        | "cleaned"
                )
            )
            || !op["expected_revision"]
                .as_u64()
                .is_some_and(|v| v > 0 && v <= 9_007_199_254_740_991)
            || !op["can_cleanup"].is_boolean()
        {
            bail!("网关凭据审计无效");
        }
        crate::capabilities::vault::CredentialRef::parse(
            text(&op, "reference")?,
            &crate::gateway::credential_binding(owner, digest)?,
        )?;
    }
    crate::gateway::credentials::operations(db)?;
    for (table, id_column, json_column) in [
        ("gateway_receipts", "command_id", "receipt"),
        ("gateway_admin_commands", "id", "receipt"),
    ] {
        let mut q=db.prepare(&format!("SELECT {id_column},CASE WHEN length(CAST({json_column} AS BLOB))<=65536 THEN {json_column} END FROM {table}"))?;
        let mut rows = q.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            policy::identifier(&id)?;
            let body: String = row.get(1)?;
            let value: Value = serde_json::from_str(&body)?;
            if !value.is_object() {
                bail!("网关收据结构无效");
            }
        }
    }
    let mut q=db.prepare("SELECT id,device_id,code_hash,status,claimed_principal,config_revision FROM gateway_pairings")?;
    let mut rows = q.query([])?;
    while let Some(row) = rows.next()? {
        for i in [0, 1] {
            policy::identifier(&row.get::<_, String>(i)?)?;
        }
        let hash: String = row.get(2)?;
        let status: String = row.get(3)?;
        let principal: Option<String> = row.get(4)?;
        let revision: u64 = row.get(5)?;
        if !gateway_digest(&hash)
            || !matches!(status.as_str(), "issued" | "claimed" | "revoked")
            || revision == 0
            || revision > 9_007_199_254_740_991
            || (status == "claimed" && principal.is_none())
        {
            bail!("网关配对记录无效");
        }
        if let Some(id) = principal {
            policy::identifier(&id)?;
        }
    }
    let invalid_singletons:u64=db.query_row("SELECT (SELECT COUNT(*) FROM gateway_configuration WHERE singleton<>1)+(SELECT COUNT(*) FROM gateway_connection WHERE singleton<>1)",[],|r|r.get(0))?;
    if invalid_singletons != 0 {
        bail!("网关单例记录无效");
    }
    let mut q=db.prepare("SELECT command_id,device_id,config_revision,principal_id,binding_revision,session_id,request_hash,invalidated,created_at_ms FROM gateway_receipts")?;
    let mut rows = q.query([])?;
    while let Some(row) = rows.next()? {
        for i in [0, 1, 3, 5] {
            policy::identifier(&row.get::<_, String>(i)?)?;
        }
        for i in [2, 4] {
            let revision: u64 = row.get(i)?;
            if revision == 0 || revision > 9_007_199_254_740_991 {
                bail!("网关收据修订无效");
            }
        }
        if !gateway_digest(&row.get::<_, String>(6)?)
            || !matches!(row.get::<_, i64>(7)?, 0 | 1)
            || row.get::<_, i64>(8)? < 0
        {
            bail!("网关收据索引无效");
        }
    }
    let mut q =
        db.prepare("SELECT config_id,revision,state,updated_at_ms FROM gateway_connection")?;
    let mut rows = q.query([])?;
    while let Some(row) = rows.next()? {
        policy::identifier(&row.get::<_, String>(0)?)?;
        let revision: u64 = row.get(1)?;
        let state: String = row.get(2)?;
        if revision == 0
            || revision > 9_007_199_254_740_991
            || state.len() > 64
            || row.get::<_, i64>(3)? < 0
        {
            bail!("网关连接观察记录无效");
        }
    }
    let mut q=db.prepare("SELECT id,task_id,session_id,kind,name,CASE WHEN length(CAST(body AS BLOB))<=65536 THEN body END,sha256,truncated FROM gateway_text_resources")?;
    let mut rows = q.query([])?;
    while let Some(row) = rows.next()? {
        for i in [0, 1, 2] {
            policy::identifier(&row.get::<_, String>(i)?)?;
        }
        let kind: String = row.get(3)?;
        let name: String = row.get(4)?;
        let body: String = row.get(5)?;
        let hash: String = row.get(6)?;
        let truncated: i64 = row.get(7)?;
        use sha2::{Digest, Sha256};
        if kind.is_empty()
            || kind.len() > 40
            || !kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            || name.is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
            || !matches!(truncated, 0 | 1)
            || format!("{:x}", Sha256::digest(body.as_bytes())) != hash
        {
            bail!("网关文本产物记录无效");
        }
    }
    Ok(())
}

impl Store {
    pub(crate) fn backup_receipt(&self, id: &str) -> Result<Option<jobs::BackupReceipt>> {
        jobs::get(&self.db, id)
    }
    pub(crate) fn backup_receipts(&self) -> Result<Vec<jobs::BackupReceipt>> {
        jobs::list(&self.db)
    }
    pub(crate) fn accept_backup(
        &mut self,
        id: &str,
        parent: &Path,
        name: &str,
    ) -> Result<jobs::BackupReceipt> {
        let tx = self.db.transaction()?;
        let receipt = jobs::accept(&tx, id, parent, name)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn claim_backup(&mut self, id: &str) -> Result<Option<jobs::ClaimedBackup>> {
        let tx = self.db.transaction()?;
        let work = jobs::claim(&tx, id)?;
        tx.commit()?;
        Ok(work)
    }
    pub(crate) fn finish_backup(
        &mut self,
        id: &str,
        token: &str,
        outcome: jobs::BackupOutcome,
    ) -> Result<jobs::BackupReceipt> {
        let tx = self.db.transaction()?;
        let receipt = jobs::finish(&tx, id, token, outcome)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn diagnostic_export(&self) -> Result<Vec<u8>> {
        let count = |sql: &str| -> Result<u64> { Ok(self.db.query_row(sql, [], |r| r.get(0))?) };
        let tasks = |status: &str| -> Result<u64> {
            Ok(self.db.query_row(
                "SELECT count(*) FROM tasks WHERE json_extract(body,'$.status')=?1",
                [status],
                |r| r.get(0),
            )?)
        };
        let counts = diagnostics::SafeCounts {
            sessions: count("SELECT count(*) FROM sessions")?,
            tasks_pending: tasks("queued")?, tasks_running: tasks("running")? + tasks("waiting_input")?,
            tasks_completed: tasks("completed")?, tasks_failed: tasks("failed")?, tasks_cancelled: tasks("cancelled")?,
            tasks_unknown: count("SELECT count(*) FROM tasks WHERE json_extract(body,'$.execution_cleanup')='unknown'")?,
            events_retained: count("SELECT count(*) FROM events")?,
            providers_configured: count("SELECT count(*) FROM named_providers WHERE json_extract(body,'$.credential_ref') IS NOT NULL AND coalesce(json_extract(body,'$.deleted'),0)=0")?,
            // Connection checks are currently ephemeral, not an audit of task success.
            providers_connection_verified: 0,
            provider_tasks_succeeded: count("SELECT count(*) FROM tasks t JOIN task_providers p ON p.id=t.id WHERE json_extract(t.body,'$.status')='completed'")?,
            credentials_unknown: count("SELECT count(*) FROM credential_operations WHERE json_extract(body,'$.status')='unknown'")?
                + count("SELECT count(*) FROM mcp_credential_operations WHERE json_extract(body,'$.status')='unknown'")?
                + count("SELECT count(*) FROM qq_credential_operations WHERE json_extract(body,'$.status')='unknown'")?,
            external_calls_unknown: count("SELECT count(*) FROM capability_calls WHERE json_extract(body,'$.status')='unknown'")?,
        };
        Ok(diagnostics::export(&diagnostics::SafeDiagnostics {
            database_schema: storage::preflight(&self.db)?,
            storage: diagnostics::StorageHealth::Compatible,
            vault: diagnostics::VaultHealth::NotChecked,
            updates: diagnostics::UpdateHealth::DisabledNoTrustedReleaseConfiguration,
            counts,
        })?)
    }
}

#[cfg(test)]
#[path = "reliability_tests.rs"]
mod tests;
