//! MCP-only credential authority. Client input never chooses owner_kind or an OS
//! reference. Secret bytes and secret-derived hashes never enter this Store.
//! Exact metadata, enqueue snapshots and one-shot OS intentions are durable;
//! actual OS work happens on Vault's worker only after releasing the Store lock.
pub(crate) use super::providers::{CredentialOutcome, CredentialWorkKind};
use super::*;
use crate::capabilities::vault::{CredentialRef, VaultBinding};
use harness_core::{
    CredentialOperationView, McpCredentialOperationView, McpCredentialSlotState,
    McpServerDefinition,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MAX_OPERATIONS: usize = 1024;
const MAX_CLEANUPS: usize = 256;
const MAX_ORPHANS: usize = 256;
const MAX_SLOTS: usize = 8;
const CWD_POLICY: &str = "isolated_probe_and_pinned_task_project_v1";
const ENV_POLICY: &str = "clear_declared_slots_only_v1";

pub(crate) struct McpCredentialRecord {
    pub server_id: String,
    pub revision: u64,
    pub config_digest: String,
    pub declared_slots: Vec<String>,
    pub bindings: BTreeMap<String, CredentialRef>,
}
pub(crate) struct McpCredentialWork {
    pub operation_id: String,
    pub server_id: String,
    pub slot: String,
    pub kind: CredentialWorkKind,
    pub binding: VaultBinding,
    pub reference: CredentialRef,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Task,
    Probe,
}
pub(crate) struct McpCredentialSnapshot {
    pub server_id: String,
    pub revision: u64,
    pub config_digest: String,
    pub declared_slots: Vec<String>,
    pub bindings: BTreeMap<String, CredentialRef>,
    scope: Scope,
}

pub(super) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS mcp_credential_bindings(id TEXT PRIMARY KEY,server_id TEXT NOT NULL,slot TEXT NOT NULL,body TEXT NOT NULL,UNIQUE(server_id,slot));
      CREATE TABLE IF NOT EXISTS mcp_credential_operations(id TEXT PRIMARY KEY,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS task_mcp_credentials(id TEXT PRIMARY KEY,task_id TEXT NOT NULL,server_id TEXT NOT NULL,body TEXT NOT NULL,UNIQUE(task_id,server_id));")?;
    // No supported predecessor accepted nonempty credential_refs. Never adopt
    // them as OS selectors. Keep the raw legacy body private for owner repair,
    // but revoke execution and expose only static unavailability metadata.
    for mut server in rows(db, "managed_capabilities")? {
        if server["kind"] != "mcp" || server["deleted"] == true {
            continue;
        }
        if server["definition"]["credential_slots"].is_null() {
            server["definition"]["credential_slots"] = json!([]);
        }
        if server["definition"]["credential_refs"]
            .as_object()
            .is_some_and(|refs| !refs.is_empty())
        {
            server["enabled"] = json!(false);
            server["definition"]["enabled"] = json!(false);
            server["status"] = json!("unavailable");
            server["reason"] = json!("legacy_credentials_unsupported");
            server["catalog"] = json!([]);
            server["catalog_digest"] = Value::Null;
            server["probe_state"] = Value::Null;
        }
        put(db, "managed_capabilities", &server)?;
    }
    Ok(())
}
fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn uuid(value: &str) -> Result<()> {
    if value.len() != 36
        || !uuid::Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
    {
        bail!("MCP 服务或凭据操作标识必须是规范 UUID");
    }
    Ok(())
}
pub(crate) fn validate_slot(slot: &str) -> Result<()> {
    harness_core::validate_mcp_credential_slot(slot).map_err(anyhow::Error::msg)
}
pub(crate) fn canonical_slots(slots: &[String]) -> Result<Vec<String>> {
    if slots.len() > MAX_SLOTS {
        bail!("每个 MCP 服务最多声明 8 个凭据槽");
    }
    let mut seen = HashSet::new();
    for slot in slots {
        validate_slot(slot)?;
        if !seen.insert(slot) {
            bail!("MCP 凭据槽不能重复");
        }
    }
    let mut slots = slots.to_vec();
    slots.sort();
    Ok(slots)
}
pub(crate) fn config_digest(definition: &McpServerDefinition) -> Result<String> {
    if !definition.credential_refs.is_empty() {
        bail!("不能提交 MCP 凭据引用，请使用独立凭据入口");
    }
    if definition.program.len() > 4096
        || definition.program.contains('\0')
        || !Path::new(&definition.program).is_absolute()
        || definition.args.len() > 64
        || definition
            .args
            .iter()
            .any(|arg| arg.len() > 8192 || arg.contains('\0'))
        || definition.args.iter().map(String::len).sum::<usize>() > 65536
    {
        bail!("MCP 程序或参数配置无效");
    }
    let slots = canonical_slots(&definition.credential_slots)?;
    Ok(hash(
        &json!({"domain":"self-agent.mcp-vault-config.v1","program":definition.program,"args":definition.args,"protocol":definition.protocol,"credential_slots":slots,"cwd_policy":CWD_POLICY,"env_policy":ENV_POLICY}),
    ))
}
pub(crate) fn mcp_binding(
    server_id: &str,
    slot: &str,
    config_digest: &str,
) -> Result<VaultBinding> {
    uuid(server_id)?;
    validate_slot(slot)?;
    Ok(VaultBinding::capability(server_id, slot, config_digest)?)
}
fn get(db: &Connection, table: &str, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = db
        .query_row(
            &format!("SELECT body FROM {table} WHERE id=?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(|_| anyhow!("MCP 凭据私有记录无效")))
        .transpose()
}
fn server(db: &Connection, id: &str) -> Result<Value> {
    uuid(id)?;
    let value = get(db, "managed_capabilities", id)?.context("MCP 服务不存在或已经删除")?;
    if value["id"] != id || value["kind"] != "mcp" || value["deleted"] == true {
        bail!("MCP 服务不存在或已经删除");
    }
    Ok(value)
}
fn revision(value: &Value) -> Result<u64> {
    value["revision"].as_u64().context("MCP 修订无效")
}
fn next_revision(value: &Value) -> Result<u64> {
    revision(value)?
        .checked_add(1)
        .filter(|v| *v <= 9_007_199_254_740_991)
        .context("MCP 修订已达到限制")
}
fn check_revision(value: &Value, expected: u64) -> Result<()> {
    if revision(value)? != expected {
        bail!("MCP 修订已改变，请刷新后重新操作");
    }
    Ok(())
}
fn definition(value: &Value) -> Result<McpServerDefinition> {
    serde_json::from_value(value["definition"].clone()).map_err(|_| anyhow!("MCP 服务配置无效"))
}
fn bindings(db: &Connection, server_id: &str) -> Result<Vec<Value>> {
    let mut stmt =
        db.prepare("SELECT body FROM mcp_credential_bindings WHERE server_id=?1 ORDER BY slot")?;
    let values = stmt.query_map([server_id], |r| r.get::<_, String>(0))?;
    values
        .map(|raw| serde_json::from_str(&raw?).map_err(|_| anyhow!("MCP 凭据私有记录无效")))
        .collect()
}
fn load_record(db: &Connection, value: &Value) -> Result<McpCredentialRecord> {
    let definition = definition(value)?;
    let config_digest = config_digest(&definition)?;
    let slots = canonical_slots(&definition.credential_slots)?;
    let id = text(value, "id")?;
    let mut refs = BTreeMap::new();
    for row in bindings(db, id)? {
        let slot = text(&row, "slot")?;
        if !slots.iter().any(|name| name == slot) || row["config_digest"] != config_digest {
            bail!("MCP 凭据绑定与当前配置不一致，未回退到其它凭据");
        }
        let reference = CredentialRef::parse(
            text(&row, "reference")?,
            &mcp_binding(id, slot, &config_digest)?,
        )?;
        refs.insert(slot.into(), reference);
    }
    Ok(McpCredentialRecord {
        server_id: id.into(),
        revision: revision(value)?,
        config_digest,
        declared_slots: slots,
        bindings: refs,
    })
}
fn operations(db: &Connection) -> Result<Vec<Value>> {
    rows(db, "mcp_credential_operations")
}
fn capacity(db: &Connection, new_operations: usize, new_orphans: usize) -> Result<()> {
    let ops = operations(db)?;
    let pending = |op: &&Value| matches!(op["status"].as_str(), Some("pending" | "inflight"));
    let reserved_operations = ops
        .iter()
        .filter(pending)
        .filter(|op| op["kind"] == "set")
        .count();
    let reserved_orphans = ops
        .iter()
        .filter(pending)
        .filter(|op| op["target_operation_id"].is_null())
        .count();
    let normal = ops
        .iter()
        .filter(|op| op["target_operation_id"].is_null())
        .count();
    let orphans = ops.iter().filter(|op| op["orphan"] == true).count();
    if normal + reserved_operations + new_operations > MAX_OPERATIONS
        || orphans + reserved_orphans + new_orphans > MAX_ORPHANS
    {
        bail!("MCP 凭据操作或待清理记录已达上限，未遗忘已有引用");
    }
    Ok(())
}
fn cleanup_capacity(db: &Connection) -> Result<()> {
    if operations(db)?
        .iter()
        .filter(|op| !op["target_operation_id"].is_null())
        .count()
        >= MAX_CLEANUPS
    {
        bail!("MCP 显式凭据清理已达 256 次审计上限，未自动重试或遗忘引用");
    }
    Ok(())
}
fn current_reference(db: &Connection, reference: &str) -> Result<bool> {
    // A private MCP ref can never be parsed as a Provider binding. Scanning all
    // MCP slots still prevents accidental deletion of any active slot pointer.
    Ok(rows(db, "mcp_credential_bindings")?
        .iter()
        .any(|row| row["reference"] == reference))
}
fn orphan(
    db: &Connection,
    server_id: &str,
    slot: &str,
    digest: &str,
    reference: &str,
) -> Result<()> {
    if operations(db)?
        .iter()
        .any(|op| op["reference"] == reference && op["orphan"] == true)
    {
        return Ok(());
    }
    put(
        db,
        "mcp_credential_operations",
        &json!({"id":new_id(),"owner_kind":"mcp","owner_id":server_id,"slot":slot,"config_digest":digest,"reference":reference,"kind":"cleanup","status":"cleanup_required","reason":"mcp_credential_detached","can_cleanup":true,"orphan":true,"cleanup_status":Value::Null,"created_at":now(),"updated_at":now()}),
    )
}
fn public_operation(op: &Value) -> Result<McpCredentialOperationView> {
    Ok(McpCredentialOperationView {
        slot: text(op, "slot")?.into(),
        operation: CredentialOperationView {
            id: text(op, "id")?.into(),
            owner_kind: "mcp".into(),
            owner_id: text(op, "owner_id")?.into(),
            kind: text(op, "kind")?.into(),
            status: text(op, "status")?.into(),
            reason: op["reason"].as_str().map(str::to_owned),
            can_cleanup: op["can_cleanup"] == true,
            cleanup_status: op["cleanup_status"].as_str().map(str::to_owned),
            created_at: text(op, "created_at")?.into(),
            updated_at: text(op, "updated_at")?.into(),
        },
    })
}
fn receipt(op: &Value, replayed: bool) -> Result<Value> {
    Ok(json!({"operation":public_operation(op)?,"replayed":replayed,"no_secret_applied":replayed}))
}
fn metadata(kind: &str, id: &str, slot: &str, revision: u64) -> Value {
    json!({"owner_kind":"mcp","owner_id":id,"slot":slot,"expected_revision":revision,"kind":kind})
}
fn existing(db: &Connection, operation_id: &str, metadata: &Value) -> Result<Option<Value>> {
    uuid(operation_id)?;
    get(db, "mcp_credential_operations", operation_id)?
        .map(|op| {
            if op["metadata_digest"] != hash(metadata) {
                bail!("MCP 凭据操作标识已被不同元数据使用");
            }
            receipt(&op, true)
        })
        .transpose()
}
fn active_operation(db: &Connection, server_id: &str) -> Result<()> {
    if operations(db)?.iter().any(|op| {
        op["owner_id"] == server_id && matches!(op["status"].as_str(), Some("pending" | "inflight"))
    }) {
        bail!("此 MCP 服务已有待确认凭据操作，请先查询结果");
    }
    Ok(())
}
fn new_operation(
    id: &str,
    record: &McpCredentialRecord,
    slot: &str,
    kind: &str,
    metadata: &Value,
    reference: &str,
) -> Value {
    json!({"id":id,"owner_kind":"mcp","owner_id":record.server_id,"slot":slot,"kind":kind,"status":"pending","expected_revision":record.revision,"config_digest":record.config_digest,"reference":reference,"metadata_digest":hash(metadata),"reason":Value::Null,"can_cleanup":false,"orphan":false,"cleanup_status":Value::Null,"created_at":now(),"updated_at":now()})
}
fn invalidate_catalog(value: &mut Value) -> Result<()> {
    value["revision"] = json!(next_revision(value)?);
    value["status"] = json!("configured");
    value["catalog"] = json!([]);
    value["catalog_digest"] = Value::Null;
    value["probe_state"] = Value::Null;
    value["reason"] = Value::Null;
    value["updated_at"] = json!(now());
    Ok(())
}
fn save_binding(
    db: &Connection,
    server_id: &str,
    slot: &str,
    digest: &str,
    reference: &str,
) -> Result<()> {
    let id = format!("{server_id}:{slot}");
    let body = json!({"id":id,"server_id":server_id,"slot":slot,"config_digest":digest,"reference":reference,"updated_at":now()});
    db.execute("INSERT INTO mcp_credential_bindings(id,server_id,slot,body) VALUES(?1,?2,?3,?4) ON CONFLICT(server_id,slot) DO UPDATE SET body=excluded.body",params![id,server_id,slot,body.to_string()])?;
    Ok(())
}

impl Store {
    /// Private admission check material only. Never include existing vault refs,
    /// and never accept or persist the prospective secret in this Store method.
    pub(crate) fn metadata_for_mcp_credential_check(&self, server_id: &str) -> Result<Value> {
        let value = server(&self.db, server_id)?;
        let mut definition = value["definition"].clone();
        definition
            .as_object_mut()
            .context("MCP 服务配置无效")?
            .remove("credential_refs");
        Ok(json!({"definition":definition,"catalog":value["catalog"]}))
    }
    pub(crate) fn mcp_credential_record(&self, id: &str) -> Result<McpCredentialRecord> {
        load_record(&self.db, &server(&self.db, id)?)
    }
    pub(crate) fn mcp_credential_states(&self, id: &str) -> Result<Vec<McpCredentialSlotState>> {
        let value = server(&self.db, id)?;
        let definition = definition(&value)?;
        if !definition.credential_refs.is_empty() {
            return Ok(canonical_slots(&definition.credential_slots)?
                .into_iter()
                .map(|slot| McpCredentialSlotState {
                    slot,
                    configured: false,
                    status: "legacy_unsupported".into(),
                })
                .collect());
        }
        let record = self.mcp_credential_record(id)?;
        Ok(record
            .declared_slots
            .into_iter()
            .map(|slot| {
                let configured = record.bindings.contains_key(&slot);
                McpCredentialSlotState {
                    slot,
                    configured,
                    status: if configured {
                        "bound_not_checked"
                    } else {
                        "not_configured"
                    }
                    .into(),
                }
            })
            .collect())
    }
    pub(crate) fn mcp_credential_operations(&self) -> Result<Vec<McpCredentialOperationView>> {
        operations(&self.db)?.iter().map(public_operation).collect()
    }
    pub(crate) fn mcp_credential_operation(&self, id: &str) -> Result<McpCredentialOperationView> {
        uuid(id)?;
        public_operation(
            &get(&self.db, "mcp_credential_operations", id)?.context("MCP 凭据操作不存在")?,
        )
    }
    pub(crate) fn existing_mcp_credential_set(
        &self,
        operation_id: &str,
        server_id: &str,
        slot: &str,
        expected_revision: u64,
    ) -> Result<Option<Value>> {
        existing(
            &self.db,
            operation_id,
            &metadata("set", server_id, slot, expected_revision),
        )
    }
    pub(crate) fn begin_mcp_credential_set(
        &mut self,
        operation_id: &str,
        server_id: &str,
        slot: &str,
        expected_revision: u64,
        reference: &CredentialRef,
    ) -> Result<Value> {
        let metadata = metadata("set", server_id, slot, expected_revision);
        if let Some(receipt) = existing(&self.db, operation_id, &metadata)? {
            return Ok(receipt);
        }
        validate_slot(slot)?;
        let tx = self.db.transaction()?;
        let value = server(&tx, server_id)?;
        check_revision(&value, expected_revision)?;
        next_revision(&value)?;
        let record = load_record(&tx, &value)?;
        if !record
            .declared_slots
            .iter()
            .any(|declared| declared == slot)
        {
            bail!("MCP 凭据槽尚未声明");
        }
        CredentialRef::parse(
            reference.as_str(),
            &mcp_binding(server_id, slot, &record.config_digest)?,
        )?;
        active_operation(&tx, server_id)?;
        capacity(&tx, 2, 1)?;
        if current_reference(&tx, reference.as_str())?
            || operations(&tx)?
                .iter()
                .any(|op| op["reference"] == reference.as_str())
        {
            bail!("MCP 凭据写入必须使用全新引用，不能重用旧操作");
        }
        let op = new_operation(
            operation_id,
            &record,
            slot,
            "set",
            &metadata,
            reference.as_str(),
        );
        put(&tx, "mcp_credential_operations", &op)?;
        tx.commit()?;
        receipt(&op, false)
    }
    pub(crate) fn begin_mcp_credential_delete(
        &mut self,
        operation_id: &str,
        server_id: &str,
        slot: &str,
        expected_revision: u64,
    ) -> Result<Value> {
        let metadata = metadata("delete", server_id, slot, expected_revision);
        if let Some(receipt) = existing(&self.db, operation_id, &metadata)? {
            return Ok(receipt);
        }
        validate_slot(slot)?;
        let tx = self.db.transaction()?;
        let mut value = server(&tx, server_id)?;
        check_revision(&value, expected_revision)?;
        let record = load_record(&tx, &value)?;
        let reference = record
            .bindings
            .get(slot)
            .context("MCP 凭据槽没有已绑定凭据")?;
        active_operation(&tx, server_id)?;
        capacity(&tx, 1, 1)?;
        let op = new_operation(
            operation_id,
            &record,
            slot,
            "delete",
            &metadata,
            reference.as_str(),
        );
        tx.execute(
            "DELETE FROM mcp_credential_bindings WHERE server_id=?1 AND slot=?2",
            params![server_id, slot],
        )?;
        invalidate_catalog(&mut value)?;
        put(&tx, "managed_capabilities", &value)?;
        put(&tx, "mcp_credential_operations", &op)?;
        tx.commit()?;
        receipt(&op, false)
    }
    pub(crate) fn begin_mcp_credential_cleanup(
        &mut self,
        operation_id: &str,
        target_operation_id: &str,
    ) -> Result<Value> {
        uuid(target_operation_id)?;
        let metadata =
            json!({"owner_kind":"mcp","kind":"cleanup","target_operation_id":target_operation_id});
        if let Some(receipt) = existing(&self.db, operation_id, &metadata)? {
            return Ok(receipt);
        }
        let tx = self.db.transaction()?;
        let mut target = get(&tx, "mcp_credential_operations", target_operation_id)?
            .context("待清理 MCP 凭据操作不存在")?;
        if target["can_cleanup"] != true || target["orphan"] != true {
            bail!("此 MCP 凭据操作当前不可清理");
        }
        if current_reference(&tx, text(&target, "reference")?)? {
            bail!("MCP 凭据仍被服务引用，不能清理");
        }
        cleanup_capacity(&tx)?;
        let record = McpCredentialRecord {
            server_id: text(&target, "owner_id")?.into(),
            revision: 0,
            config_digest: text(&target, "config_digest")?.into(),
            declared_slots: Vec::new(),
            bindings: BTreeMap::new(),
        };
        let mut op = new_operation(
            operation_id,
            &record,
            text(&target, "slot")?,
            "cleanup",
            &metadata,
            text(&target, "reference")?,
        );
        op["target_operation_id"] = json!(target_operation_id);
        target["can_cleanup"] = json!(false);
        target["cleanup_operation_id"] = json!(operation_id);
        target["cleanup_status"] = json!("pending");
        target["updated_at"] = json!(now());
        put(&tx, "mcp_credential_operations", &target)?;
        put(&tx, "mcp_credential_operations", &op)?;
        tx.commit()?;
        receipt(&op, false)
    }
    pub(crate) fn claim_mcp_credential_operation(
        &mut self,
        id: &str,
    ) -> Result<Option<McpCredentialWork>> {
        uuid(id)?;
        let tx = self.db.transaction()?;
        let Some(mut op) = get(&tx, "mcp_credential_operations", id)? else {
            return Ok(None);
        };
        if op["status"] != "pending" {
            return Ok(None);
        }
        let server_id = text(&op, "owner_id")?.to_owned();
        let slot = text(&op, "slot")?.to_owned();
        if op["kind"] == "set" {
            let current = server(&tx, &server_id).and_then(|value| load_record(&tx, &value));
            if !current.is_ok_and(|record| {
                Some(record.revision) == op["expected_revision"].as_u64()
                    && record.config_digest == op["config_digest"]
                    && record.declared_slots.contains(&slot)
            }) {
                op["status"] = json!("rejected");
                op["reason"] = json!("mcp_changed_before_vault_call");
                op["updated_at"] = json!(now());
                put(&tx, "mcp_credential_operations", &op)?;
                tx.commit()?;
                return Ok(None);
            }
        }
        let binding = mcp_binding(&server_id, &slot, text(&op, "config_digest")?)?;
        let reference = CredentialRef::parse(text(&op, "reference")?, &binding)?;
        let kind = match op["kind"].as_str() {
            Some("set") => CredentialWorkKind::Set,
            Some("delete" | "cleanup") => CredentialWorkKind::Delete,
            _ => bail!("MCP 凭据操作类型无效"),
        };
        if kind == CredentialWorkKind::Delete && current_reference(&tx, reference.as_str())? {
            bail!("MCP 凭据仍被服务引用，不能删除");
        }
        op["status"] = json!("inflight");
        op["updated_at"] = json!(now());
        put(&tx, "mcp_credential_operations", &op)?;
        tx.commit()?;
        Ok(Some(McpCredentialWork {
            operation_id: id.into(),
            server_id,
            slot,
            kind,
            binding,
            reference,
        }))
    }
    pub(crate) fn finish_mcp_credential_operation(
        &mut self,
        id: &str,
        outcome: CredentialOutcome,
    ) -> Result<bool> {
        let tx = self.db.transaction()?;
        let Some(mut op) = get(&tx, "mcp_credential_operations", id)? else {
            return Ok(false);
        };
        if op["status"] != "inflight" {
            return Ok(false);
        }
        let success = matches!(outcome, CredentialOutcome::Succeeded);
        let unknown = matches!(outcome, CredentialOutcome::Unknown);
        op["status"] = json!(if success {
            "completed"
        } else if unknown {
            "unknown"
        } else {
            "rejected"
        });
        op["reason"] = json!(if success {
            None
        } else if unknown {
            Some("vault_outcome_unknown")
        } else {
            Some("vault_rejected")
        });
        if op["kind"] == "set" {
            if success {
                let current = server(&tx, text(&op, "owner_id")?).ok().filter(|value| {
                    value["revision"] == op["expected_revision"]
                        && definition(value)
                            .and_then(|def| config_digest(&def))
                            .is_ok_and(|digest| digest == op["config_digest"])
                });
                if let Some(mut value) = current {
                    let record = load_record(&tx, &value)?;
                    let slot = text(&op, "slot")?;
                    if !record.declared_slots.iter().any(|name| name == slot) {
                        bail!("MCP 凭据槽配置已改变");
                    }
                    if let Some(old) = record.bindings.get(slot) {
                        orphan(
                            &tx,
                            &record.server_id,
                            slot,
                            &record.config_digest,
                            old.as_str(),
                        )?;
                    }
                    save_binding(
                        &tx,
                        &record.server_id,
                        slot,
                        &record.config_digest,
                        text(&op, "reference")?,
                    )?;
                    invalidate_catalog(&mut value)?;
                    put(&tx, "managed_capabilities", &value)?;
                } else {
                    op["status"] = json!("orphaned");
                    op["reason"] = json!("mcp_revision_changed");
                    op["orphan"] = json!(true);
                    op["can_cleanup"] = json!(true);
                }
            } else if unknown {
                op["orphan"] = json!(true);
                op["can_cleanup"] = json!(true);
            }
        } else if let Some(target_id) = op["target_operation_id"].as_str() {
            let mut target = get(&tx, "mcp_credential_operations", target_id)?
                .context("待清理 MCP 凭据操作不存在")?;
            if target["cleanup_operation_id"] != op["id"] {
                bail!("MCP 凭据清理关联已改变");
            }
            target["can_cleanup"] = json!(!success);
            target["orphan"] = json!(!success);
            target["cleanup_status"] = json!(if success {
                "completed"
            } else if unknown {
                "unknown"
            } else {
                "rejected"
            });
            target["updated_at"] = json!(now());
            if success && target["status"] == "cleanup_required" {
                target["status"] = json!("cleaned");
                target["reason"] = json!("credential_deleted");
            }
            put(&tx, "mcp_credential_operations", &target)?;
        } else if !success {
            op["orphan"] = json!(true);
            op["can_cleanup"] = json!(true);
        }
        op["updated_at"] = json!(now());
        put(&tx, "mcp_credential_operations", &op)?;
        tx.commit()?;
        Ok(true)
    }
    pub(crate) fn task_mcp_credentials(&self, task_id: &str) -> Result<Vec<McpCredentialSnapshot>> {
        task_snapshots(&self.db, task_id)
    }
    pub(crate) fn check_task_mcp_credentials(&self, task_id: &str) -> Result<()> {
        for snapshot in task_snapshots(&self.db, task_id)? {
            check_snapshot(&self.db, &snapshot)?;
        }
        Ok(())
    }
    pub(crate) fn probe_mcp_credentials(
        &self,
        server_id: &str,
        revision: u64,
    ) -> Result<McpCredentialSnapshot> {
        let value = server(&self.db, server_id)?;
        check_revision(&value, revision)?;
        if value["status"] != "checking" || value["probe_state"] != "inflight" {
            bail!("MCP 验证意图已失效");
        }
        complete_snapshot(load_record(&self.db, &value)?, Scope::Probe)
    }
    pub(crate) fn check_mcp_credential_snapshot(
        &self,
        snapshot: &McpCredentialSnapshot,
    ) -> Result<()> {
        check_snapshot(&self.db, snapshot)
    }
}

/// Call inside the same metadata-save/delete transaction, BEFORE replacing the
/// server definition. No partial detach is committed if any quota/check fails.
pub(super) fn detach_mcp_credentials(
    db: &Connection,
    old: &Value,
    next: Option<&McpServerDefinition>,
) -> Result<()> {
    let server_id = text(old, "id")?;
    let bindings = bindings(db, server_id)?;
    let next_digest = next.map(config_digest).transpose()?;
    if bindings.is_empty()
        || next_digest
            .as_ref()
            .is_some_and(|digest| bindings.iter().all(|row| row["config_digest"] == *digest))
    {
        return Ok(());
    }
    capacity(db, bindings.len(), bindings.len())?;
    for row in bindings {
        let slot = text(&row, "slot")?;
        let digest = text(&row, "config_digest")?;
        let reference = text(&row, "reference")?;
        CredentialRef::parse(reference, &mcp_binding(server_id, slot, digest)?)?;
        orphan(db, server_id, slot, digest, reference)?;
    }
    db.execute(
        "DELETE FROM mcp_credential_bindings WHERE server_id=?1",
        [server_id],
    )?;
    Ok(())
}
fn complete_snapshot(record: McpCredentialRecord, scope: Scope) -> Result<McpCredentialSnapshot> {
    if record
        .declared_slots
        .iter()
        .any(|slot| !record.bindings.contains_key(slot))
    {
        bail!("MCP 声明的凭据槽尚未全部配置，未读取其它服务凭据或启动进程");
    }
    Ok(McpCredentialSnapshot {
        server_id: record.server_id,
        revision: record.revision,
        config_digest: record.config_digest,
        declared_slots: record.declared_slots,
        bindings: record.bindings,
        scope,
    })
}
fn serialize_snapshot(snapshot: &McpCredentialSnapshot) -> Value {
    json!({"server_id":snapshot.server_id,"revision":snapshot.revision,"config_digest":snapshot.config_digest,"declared_slots":snapshot.declared_slots,"bindings":snapshot.bindings.iter().map(|(slot,reference)|(slot,reference.as_str())).collect::<BTreeMap<_,_>>()})
}
pub(super) fn snapshot_task_mcp_credentials(
    db: &Connection,
    task_id: &str,
    server_id: &str,
    revision: u64,
) -> Result<()> {
    let value = server(db, server_id)?;
    check_revision(&value, revision)?;
    if value["enabled"] != true || value["status"] != "ready" {
        bail!("MCP 服务尚未启用或验证就绪");
    }
    let snapshot = complete_snapshot(load_record(db, &value)?, Scope::Task)?;
    let body = serialize_snapshot(&snapshot);
    let id = format!("{task_id}:{server_id}");
    db.execute(
        "INSERT INTO task_mcp_credentials(id,task_id,server_id,body) VALUES(?1,?2,?3,?4)",
        params![id, task_id, server_id, body.to_string()],
    )?;
    Ok(())
}
fn decode_snapshot(value: &Value, scope: Scope) -> Result<McpCredentialSnapshot> {
    let server_id = text(value, "server_id")?.to_owned();
    let config_digest = text(value, "config_digest")?.to_owned();
    let declared_slots: Vec<String> = serde_json::from_value(value["declared_slots"].clone())
        .map_err(|_| anyhow!("MCP 凭据任务快照无效"))?;
    let declared_slots = canonical_slots(&declared_slots)?;
    let raw = value["bindings"]
        .as_object()
        .context("MCP 凭据任务快照无效")?;
    if raw.len() != declared_slots.len() {
        bail!("MCP 凭据任务快照缺少固定引用");
    }
    let mut bindings = BTreeMap::new();
    for slot in &declared_slots {
        let reference = raw
            .get(slot)
            .and_then(Value::as_str)
            .context("MCP 凭据任务快照无效")?;
        bindings.insert(
            slot.clone(),
            CredentialRef::parse(reference, &mcp_binding(&server_id, slot, &config_digest)?)?,
        );
    }
    complete_snapshot(
        McpCredentialRecord {
            server_id,
            revision: revision(value)?,
            config_digest,
            declared_slots,
            bindings,
        },
        scope,
    )
}
fn task_snapshots(db: &Connection, task_id: &str) -> Result<Vec<McpCredentialSnapshot>> {
    let task = record(db, "tasks", task_id).map_err(|_| anyhow!("任务不存在"))?;
    let Some(private) = get(db, "task_capabilities", task_id)? else {
        if [
            &task["managed_capabilities"]["mcp_servers"],
            &task["controls"]["capabilities"]["mcp_servers"],
        ]
        .iter()
        .any(|v| v.as_array().is_some_and(|v| !v.is_empty()))
        {
            bail!("任务 MCP 私有快照缺失，不能重新选择服务");
        }
        return Ok(Vec::new());
    };
    let pinned_servers = private["servers"]
        .as_array()
        .context("任务 MCP 私有快照结构无效，不能跳过凭据检查")?;
    if pinned_servers.len() > 8 {
        bail!("任务 MCP 私有快照超出服务数量限制");
    }
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for pinned in pinned_servers {
        let id = text(&pinned["reference"], "id")?;
        if !seen.insert(id) {
            bail!("任务 MCP 私有快照包含重复服务");
        }
        let definition: McpServerDefinition = serde_json::from_value(pinned["definition"].clone())
            .map_err(|_| anyhow!("任务 MCP 配置无效"))?;
        let digest = config_digest(&definition)?;
        let slots = canonical_slots(&definition.credential_slots)?;
        let raw: Option<String> = db
            .query_row(
                "SELECT body FROM task_mcp_credentials WHERE task_id=?1 AND server_id=?2",
                params![task_id, id],
                |row| row.get(0),
            )
            .optional()?;
        let snapshot = if let Some(raw) = raw {
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| anyhow!("MCP 凭据任务快照无效"))?;
            decode_snapshot(&value, Scope::Task)?
        } else if slots.is_empty() {
            complete_snapshot(
                McpCredentialRecord {
                    server_id: id.into(),
                    revision: revision(&pinned["reference"])?,
                    config_digest: digest.clone(),
                    declared_slots: Vec::new(),
                    bindings: BTreeMap::new(),
                },
                Scope::Task,
            )?
        } else {
            bail!("MCP 凭据任务快照缺失，不能从当前服务补取新引用");
        };
        if snapshot.server_id != id
            || Some(snapshot.revision) != pinned["reference"]["revision"].as_u64()
            || snapshot.config_digest != digest
            || snapshot.declared_slots != slots
        {
            bail!("MCP 凭据快照与任务固定配置不一致");
        }
        output.push(snapshot);
    }
    Ok(output)
}
fn check_snapshot(db: &Connection, snapshot: &McpCredentialSnapshot) -> Result<()> {
    let value = server(db, &snapshot.server_id)?;
    if revision(&value)? != snapshot.revision
        || match snapshot.scope {
            Scope::Task => value["enabled"] != true || value["status"] != "ready",
            Scope::Probe => value["status"] != "checking" || value["probe_state"] != "inflight",
        }
    {
        bail!("MCP 凭据所属服务已改变或撤销，未启动或继续调用");
    }
    let current = load_record(db, &value)?;
    if current.config_digest != snapshot.config_digest
        || current.declared_slots != snapshot.declared_slots
        || current.bindings != snapshot.bindings
    {
        bail!("MCP 凭据绑定已改变，未切换到新凭据");
    }
    Ok(())
}
pub(super) fn recover(db: &Connection) -> Result<()> {
    for mut op in operations(db)? {
        if !matches!(op["status"].as_str(), Some("pending" | "inflight")) {
            continue;
        }
        op["status"] = json!("unknown");
        op["reason"] = json!("daemon_interrupted_no_replay");
        op["updated_at"] = json!(now());
        if let Some(target_id) = op["target_operation_id"].as_str() {
            if let Some(mut target) = get(db, "mcp_credential_operations", target_id)? {
                if target["cleanup_operation_id"] == op["id"] {
                    target["can_cleanup"] = json!(true);
                    target["cleanup_status"] = json!("unknown");
                    target["updated_at"] = json!(now());
                    put(db, "mcp_credential_operations", &target)?;
                }
            }
        } else {
            op["orphan"] = json!(true);
            op["can_cleanup"] = json!(true);
        }
        put(db, "mcp_credential_operations", &op)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::vault::{FreshCredential, SecretBytes};
    const SECRET: &str = "mcp-fixture-secret-never-stored";
    fn store() -> Store {
        let store = Store::open(Path::new(":memory:")).unwrap();
        migrate(&store.db).unwrap();
        store
    }
    fn definition(slots: &[&str]) -> McpServerDefinition {
        McpServerDefinition {
            name: "fixture server".into(),
            program: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec!["fixture-mode".into()],
            protocol: harness_core::McpProtocolMode::Modern2026,
            enabled: true,
            credential_refs: BTreeMap::new(),
            credential_slots: slots.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
    fn create(store: &Store, slots: &[&str]) -> String {
        let id = new_id();
        put(&store.db,"managed_capabilities",&json!({"id":id,"kind":"mcp","revision":1,"enabled":true,"deleted":false,"definition":definition(slots),"status":"configured","catalog":[],"catalog_digest":Value::Null,"updated_at":now()})).unwrap();
        id
    }
    fn material(store: &Store, id: &str, slot: &str) -> FreshCredential {
        let record = store.mcp_credential_record(id).unwrap();
        FreshCredential::new(
            &mcp_binding(id, slot, &record.config_digest).unwrap(),
            SecretBytes::new(SECRET.as_bytes().to_vec()).unwrap(),
        )
    }
    fn install(store: &mut Store, id: &str, slot: &str) -> (String, CredentialRef) {
        let record = store.mcp_credential_record(id).unwrap();
        let fresh = material(store, id, slot);
        let reference = fresh.reference().clone();
        let op = new_id();
        store
            .begin_mcp_credential_set(&op, id, slot, record.revision, &reference)
            .unwrap();
        let work = store.claim_mcp_credential_operation(&op).unwrap().unwrap();
        assert_eq!(work.operation_id, op);
        assert_eq!(work.server_id, id);
        assert_eq!(work.slot, slot);
        assert!(matches!(work.kind, CredentialWorkKind::Set));
        assert!(CredentialRef::parse(reference.as_str(), &work.binding).is_ok());
        store
            .finish_mcp_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap();
        (op, reference)
    }
    fn ready(store: &Store, id: &str) {
        let mut value = server(&store.db, id).unwrap();
        value["status"] = json!("ready");
        value["catalog_digest"] = json!("fixture-catalog-digest");
        put(&store.db, "managed_capabilities", &value).unwrap();
    }
    fn task(store: &Store, id: &str) -> String {
        let value = server(&store.db, id).unwrap();
        let task_id = new_id();
        snapshot_task_mcp_credentials(&store.db, &task_id, id, revision(&value).unwrap()).unwrap();
        let reference = json!({"id":id,"revision":value["revision"],"digest":"fixture-catalog-digest","name":"fixture server"});
        put(
            &store.db,
            "tasks",
            &json!({"id":task_id,"managed_capabilities":{"mcp_servers":[reference]}}),
        )
        .unwrap();
        put(&store.db,"task_capabilities",&json!({"id":task_id,"servers":[{"reference":reference,"definition":value["definition"]}]})).unwrap();
        task_id
    }
    fn edit(store: &mut Store, id: &str, next: McpServerDefinition) {
        let tx = store.db.transaction().unwrap();
        let mut value = server(&tx, id).unwrap();
        detach_mcp_credentials(&tx, &value, Some(&next)).unwrap();
        value["definition"] = json!(next);
        invalidate_catalog(&mut value).unwrap();
        put(&tx, "managed_capabilities", &value).unwrap();
        tx.commit().unwrap();
    }
    #[test]
    fn slots_are_bounded_canonical_and_cannot_control_child_startup_or_transport() {
        assert_eq!(
            canonical_slots(&["Z_TOKEN".into(), "API_KEY".into()]).unwrap(),
            vec!["API_KEY", "Z_TOKEN"]
        );
        for slot in [
            "",
            "api_key",
            "9TOKEN",
            "_JAVA_OPTIONS",
            "A=B",
            "TOKEN\0",
            "PATH",
            "HOME",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "GIT_CONFIG_COUNT",
            "HTTPS_PROXY",
            "SSLKEYLOGFILE",
            "AWS_CA_BUNDLE",
            "OPENSSL_CONF",
            "DBUS_SESSION_BUS_ADDRESS",
            "HARNESS_API_KEY",
            "CODEX_HOME",
            "BASH_ENV",
            "ENV",
            "CLASSPATH",
            "CARGO_HOME",
        ] {
            assert!(validate_slot(slot).is_err(), "unexpected slot accepted");
        }
        assert!(canonical_slots(&["TOKEN".into(), "TOKEN".into()]).is_err());
        assert!(
            canonical_slots(&(0..9).map(|n| format!("TOKEN_{n}")).collect::<Vec<_>>()).is_err()
        );
        assert!(validate_slot(&"A".repeat(65)).is_err());
        for slot in [
            "API_KEY",
            "GITHUB_TOKEN",
            "SLACK_BOT_TOKEN",
            "DATABASE_PASSWORD",
            "ACCESS_TOKEN",
            "AUTHORIZATION",
        ] {
            validate_slot(slot).unwrap();
        }
    }
    #[test]
    fn digest_binds_execution_and_slot_set_not_cosmetic_name_or_references() {
        let original = definition(&["B_KEY", "A_KEY"]);
        let digest = config_digest(&original).unwrap();
        let mut cosmetic = original.clone();
        cosmetic.name = "renamed".into();
        cosmetic.enabled = false;
        cosmetic.credential_slots.reverse();
        assert_eq!(digest, config_digest(&cosmetic).unwrap());
        for changed in [0, 1, 2, 3] {
            let mut changed_def = original.clone();
            match changed {
                0 => changed_def.program.push_str("-other"),
                1 => changed_def.args.push("different".into()),
                2 => changed_def.protocol = harness_core::McpProtocolMode::Legacy2025,
                _ => changed_def.credential_slots.pop().map(drop).unwrap(),
            };
            assert_ne!(digest, config_digest(&changed_def).unwrap());
        }
        let mut forbidden = original;
        forbidden
            .credential_refs
            .insert("API_KEY".into(), "opaque-client-reference".into());
        assert!(config_digest(&forbidden).is_err());
    }
    #[test]
    fn set_is_one_shot_and_bindings_are_server_slot_configuration_specific() {
        let mut store = store();
        let id = create(&store, &["API_KEY", "OTHER_TOKEN"]);
        let other = create(&store, &["API_KEY"]);
        let record = store.mcp_credential_record(&id).unwrap();
        let fresh = material(&store, &id, "API_KEY");
        let op = new_id();
        let accepted = store
            .begin_mcp_credential_set(&op, &id, "API_KEY", record.revision, fresh.reference())
            .unwrap();
        assert_eq!(accepted["replayed"], false);
        let second = material(&store, &id, "API_KEY");
        let replay = store
            .begin_mcp_credential_set(&op, &id, "API_KEY", record.revision, second.reference())
            .unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["no_secret_applied"], true);
        assert!(store
            .existing_mcp_credential_set(&op, &id, "OTHER_TOKEN", record.revision)
            .is_err());
        assert!(store
            .existing_mcp_credential_set(&op, &other, "API_KEY", record.revision)
            .is_err());
        assert!(store
            .existing_mcp_credential_set(&op, &id, "API_KEY", record.revision + 1)
            .is_err());
        assert!(!store
            .finish_mcp_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap());
        let work = store.claim_mcp_credential_operation(&op).unwrap().unwrap();
        assert_eq!(work.reference.as_str(), fresh.reference().as_str());
        assert!(store.claim_mcp_credential_operation(&op).unwrap().is_none());
        store
            .finish_mcp_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap();
        let record = store.mcp_credential_record(&id).unwrap();
        assert_eq!(record.revision, 2);
        assert!(record.bindings.contains_key("API_KEY"));
        assert!(store
            .begin_mcp_credential_set(
                &new_id(),
                &id,
                "OTHER_TOKEN",
                record.revision,
                fresh.reference()
            )
            .is_err());
        let foreign = material(&store, &other, "API_KEY");
        assert!(store
            .begin_mcp_credential_set(
                &new_id(),
                &id,
                "API_KEY",
                record.revision,
                foreign.reference()
            )
            .is_err());
        let provider_binding =
            VaultBinding::provider(&id, "api_key", "https://example.invalid/v1").unwrap();
        let provider = FreshCredential::new(
            &provider_binding,
            SecretBytes::new(SECRET.as_bytes().to_vec()).unwrap(),
        );
        assert!(store
            .begin_mcp_credential_set(
                &new_id(),
                &id,
                "API_KEY",
                record.revision,
                provider.reference()
            )
            .is_err());
    }
    #[test]
    fn all_slots_are_required_and_success_clears_catalog_until_explicit_probe() {
        let mut store = store();
        let id = create(&store, &["API_KEY", "SECOND_TOKEN"]);
        install(&mut store, &id, "API_KEY");
        ready(&store, &id);
        assert!(snapshot_task_mcp_credentials(&store.db, &new_id(), &id, 2).is_err());
        install(&mut store, &id, "SECOND_TOKEN");
        let value = server(&store.db, &id).unwrap();
        assert_eq!(value["status"], "configured");
        assert!(value["catalog_digest"].is_null());
        ready(&store, &id);
        let task_id = task(&store, &id);
        let snapshots = store.task_mcp_credentials(&task_id).unwrap();
        assert_eq!(snapshots[0].bindings.len(), 2);
        store.check_task_mcp_credentials(&task_id).unwrap();
        install(&mut store, &id, "API_KEY");
        assert!(store.check_task_mcp_credentials(&task_id).is_err());
        assert!(store.check_mcp_credential_snapshot(&snapshots[0]).is_err());
        let states = store.mcp_credential_states(&id).unwrap();
        assert!(states.iter().all(|slot| slot.configured));
        let public=json!({"states":states,"operations":store.mcp_credential_operations().unwrap(),"task":record(&store.db,"tasks",&task_id).unwrap()}).to_string();
        assert!(!public.contains("self-agent.vault"));
        assert!(!public.contains(SECRET));
        for table in [
            "mcp_credential_bindings",
            "mcp_credential_operations",
            "task_mcp_credentials",
        ] {
            assert!(!json!(rows(&store.db, table).unwrap())
                .to_string()
                .contains(SECRET));
        }
    }
    #[test]
    fn config_change_detaches_every_slot_and_cas_failure_tracks_fresh_orphan() {
        let mut store = store();
        let id = create(&store, &["API_KEY", "SECOND_TOKEN"]);
        install(&mut store, &id, "API_KEY");
        install(&mut store, &id, "SECOND_TOKEN");
        let record = store.mcp_credential_record(&id).unwrap();
        let fresh = material(&store, &id, "API_KEY");
        let op = new_id();
        store
            .begin_mcp_credential_set(&op, &id, "API_KEY", record.revision, fresh.reference())
            .unwrap();
        store.claim_mcp_credential_operation(&op).unwrap();
        let mut next = definition(&["API_KEY", "SECOND_TOKEN"]);
        next.args.push("changed-program-input".into());
        edit(&mut store, &id, next);
        assert!(store
            .mcp_credential_record(&id)
            .unwrap()
            .bindings
            .is_empty());
        store
            .finish_mcp_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap();
        assert_eq!(
            store
                .mcp_credential_operation(&op)
                .unwrap()
                .operation
                .status,
            "orphaned"
        );
        assert!(store
            .mcp_credential_record(&id)
            .unwrap()
            .bindings
            .is_empty());
        assert_eq!(
            store
                .mcp_credential_operations()
                .unwrap()
                .iter()
                .filter(|op| op.operation.can_cleanup)
                .count(),
            3
        );
    }
    #[test]
    fn cleanup_does_not_rewrite_historical_unknown_and_never_deletes_current_slot() {
        let mut store = store();
        let id = create(&store, &["API_KEY"]);
        let (installed, _) = install(&mut store, &id, "API_KEY");
        let mut fake = get(&store.db, "mcp_credential_operations", &installed)
            .unwrap()
            .unwrap();
        fake["orphan"] = json!(true);
        fake["can_cleanup"] = json!(true);
        put(&store.db, "mcp_credential_operations", &fake).unwrap();
        assert!(store
            .begin_mcp_credential_cleanup(&new_id(), &installed)
            .is_err());
        fake["orphan"] = json!(false);
        fake["can_cleanup"] = json!(false);
        put(&store.db, "mcp_credential_operations", &fake).unwrap();
        let revision = store.mcp_credential_record(&id).unwrap().revision;
        let op = new_id();
        store
            .begin_mcp_credential_delete(&op, &id, "API_KEY", revision)
            .unwrap();
        assert!(store
            .mcp_credential_record(&id)
            .unwrap()
            .bindings
            .is_empty());
        store.claim_mcp_credential_operation(&op).unwrap();
        store
            .finish_mcp_credential_operation(&op, CredentialOutcome::Unknown)
            .unwrap();
        let cleanup = new_id();
        store.begin_mcp_credential_cleanup(&cleanup, &op).unwrap();
        assert!(store.begin_mcp_credential_cleanup(&new_id(), &op).is_err());
        let work = store
            .claim_mcp_credential_operation(&cleanup)
            .unwrap()
            .unwrap();
        assert!(matches!(work.kind, CredentialWorkKind::Delete));
        store
            .finish_mcp_credential_operation(&cleanup, CredentialOutcome::Succeeded)
            .unwrap();
        let historical = store.mcp_credential_operation(&op).unwrap();
        assert_eq!(historical.operation.status, "unknown");
        assert_eq!(
            historical.operation.cleanup_status.as_deref(),
            Some("completed")
        );
        assert!(!historical.operation.can_cleanup);
        let receipt = store.begin_mcp_credential_cleanup(&cleanup, &op).unwrap();
        assert_eq!(receipt["replayed"], true);
        assert_eq!(receipt["no_secret_applied"], true);
    }
    #[test]
    fn restart_and_deleted_server_replay_do_not_reissue_os_intents() {
        let path = std::env::temp_dir().join(format!("mcp-credential-{}.db", new_id()));
        let id;
        let op;
        let before;
        {
            let mut store = Store::open(&path).unwrap();
            migrate(&store.db).unwrap();
            id = create(&store, &["API_KEY"]);
            before = store.mcp_credential_record(&id).unwrap().revision;
            let fresh = material(&store, &id, "API_KEY");
            op = new_id();
            store
                .begin_mcp_credential_set(&op, &id, "API_KEY", before, fresh.reference())
                .unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        migrate(&store.db).unwrap();
        recover(&store.db).unwrap();
        assert_eq!(
            store
                .mcp_credential_operation(&op)
                .unwrap()
                .operation
                .status,
            "unknown"
        );
        assert!(store.claim_mcp_credential_operation(&op).unwrap().is_none());
        assert!(!store
            .finish_mcp_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap());
        let mut value = server(&store.db, &id).unwrap();
        value["deleted"] = json!(true);
        put(&store.db, "managed_capabilities", &value).unwrap();
        let replay = store
            .existing_mcp_credential_set(&op, &id, "API_KEY", before)
            .unwrap()
            .unwrap();
        assert_eq!(replay["replayed"], true);
        let cleanup = new_id();
        store.begin_mcp_credential_cleanup(&cleanup, &op).unwrap();
        recover(&store.db).unwrap();
        let target = store.mcp_credential_operation(&op).unwrap();
        assert_eq!(target.operation.status, "unknown");
        assert_eq!(target.operation.cleanup_status.as_deref(), Some("unknown"));
        assert!(target.operation.can_cleanup);
        assert!(store
            .claim_mcp_credential_operation(&cleanup)
            .unwrap()
            .is_none());
        drop(store);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn queued_set_is_rejected_after_revision_change_without_os_claim() {
        let mut store = store();
        let id = create(&store, &["API_KEY"]);
        let record = store.mcp_credential_record(&id).unwrap();
        let fresh = material(&store, &id, "API_KEY");
        let op = new_id();
        store
            .begin_mcp_credential_set(&op, &id, "API_KEY", record.revision, fresh.reference())
            .unwrap();
        let mut def = definition(&["API_KEY"]);
        def.name = "cosmetic edit still revokes intent".into();
        edit(&mut store, &id, def);
        assert!(store.claim_mcp_credential_operation(&op).unwrap().is_none());
        let view = store.mcp_credential_operation(&op).unwrap();
        assert_eq!(view.operation.status, "rejected");
        assert!(!view.operation.can_cleanup);
    }
    #[test]
    fn probe_is_fenced_to_its_server_and_missing_private_snapshots_fail_closed() {
        let mut store = store();
        let id = create(&store, &["API_KEY"]);
        install(&mut store, &id, "API_KEY");
        let other = create(&store, &["OTHER_KEY"]);
        install(&mut store, &other, "OTHER_KEY");
        let mut value = server(&store.db, &id).unwrap();
        value["status"] = json!("checking");
        value["probe_state"] = json!("inflight");
        put(&store.db, "managed_capabilities", &value).unwrap();
        let revision = revision(&value).unwrap();
        let snapshot = store.probe_mcp_credentials(&id, revision).unwrap();
        assert_eq!(snapshot.bindings.len(), 1);
        assert!(snapshot.bindings.contains_key("API_KEY"));
        store.check_mcp_credential_snapshot(&snapshot).unwrap();
        value["probe_state"] = Value::Null;
        put(&store.db, "managed_capabilities", &value).unwrap();
        assert!(store.check_mcp_credential_snapshot(&snapshot).is_err());
        ready(&store, &id);
        let task_id = task(&store, &id);
        store
            .db
            .execute(
                "DELETE FROM task_mcp_credentials WHERE task_id=?1",
                [&task_id],
            )
            .unwrap();
        assert!(store.task_mcp_credentials(&task_id).is_err());
        let empty = create(&store, &[]);
        ready(&store, &empty);
        let legacy = task(&store, &empty);
        store
            .db
            .execute(
                "DELETE FROM task_mcp_credentials WHERE task_id=?1",
                [&legacy],
            )
            .unwrap();
        assert!(store.task_mcp_credentials(&legacy).unwrap()[0]
            .bindings
            .is_empty());
        store.check_task_mcp_credentials(&legacy).unwrap();
    }
    #[test]
    fn malformed_or_duplicate_task_metadata_cannot_skip_credential_checks() {
        let mut store = store();
        let id = create(&store, &["API_KEY"]);
        install(&mut store, &id, "API_KEY");
        ready(&store, &id);
        let task_id = task(&store, &id);
        let mut private = get(&store.db, "task_capabilities", &task_id)
            .unwrap()
            .unwrap();
        let original = private["servers"][0].clone();
        private["servers"] = Value::Null;
        put(&store.db, "task_capabilities", &private).unwrap();
        assert!(store.task_mcp_credentials(&task_id).is_err());
        private["servers"] = json!([original, original]);
        put(&store.db, "task_capabilities", &private).unwrap();
        assert!(store.task_mcp_credentials(&task_id).is_err());
    }
    #[test]
    fn legacy_ref_migration_is_quarantined_not_interpreted_as_vault_authority() {
        let store = store();
        let id = create(&store, &[]);
        let mut value = server(&store.db, &id).unwrap();
        value["definition"]
            .as_object_mut()
            .unwrap()
            .remove("credential_slots");
        value["definition"]["credential_refs"] =
            json!({"API_KEY":"legacy-private-selector-canary"});
        value["status"] = json!("ready");
        put(&store.db, "managed_capabilities", &value).unwrap();
        migrate(&store.db).unwrap();
        let migrated = server(&store.db, &id).unwrap();
        assert_eq!(migrated["enabled"], false);
        assert_eq!(migrated["status"], "unavailable");
        assert!(store.mcp_credential_record(&id).is_err());
        assert!(store.mcp_credential_states(&id).unwrap().is_empty());
        assert!(operations(&store.db).unwrap().is_empty());
        assert!(bindings(&store.db, &id).unwrap().is_empty());
    }
    #[test]
    fn quotas_reserve_pending_fates_and_cleanup_tail_without_losing_rows() {
        let store = store();
        for _ in 0..MAX_OPERATIONS {
            put(
                &store.db,
                "mcp_credential_operations",
                &json!({"id":new_id(),"status":"completed"}),
            )
            .unwrap();
        }
        assert!(capacity(&store.db, 1, 0).is_err());
        cleanup_capacity(&store.db).unwrap();
        for _ in 0..MAX_CLEANUPS {
            put(
                &store.db,
                "mcp_credential_operations",
                &json!({"id":new_id(),"status":"unknown","target_operation_id":new_id()}),
            )
            .unwrap();
        }
        assert!(cleanup_capacity(&store.db).is_err());
        assert_eq!(
            operations(&store.db).unwrap().len(),
            MAX_OPERATIONS + MAX_CLEANUPS
        );
        store
            .db
            .execute("DELETE FROM mcp_credential_operations", [])
            .unwrap();
        for _ in 0..MAX_ORPHANS {
            put(
                &store.db,
                "mcp_credential_operations",
                &json!({"id":new_id(),"kind":"delete","status":"pending"}),
            )
            .unwrap();
        }
        assert!(capacity(&store.db, 1, 1).is_err());
    }
    #[test]
    fn multi_slot_detach_is_atomic_when_orphan_budget_is_exhausted() {
        let mut store = store();
        let id = create(&store, &["API_KEY", "OTHER_KEY"]);
        install(&mut store, &id, "API_KEY");
        install(&mut store, &id, "OTHER_KEY");
        for _ in 0..MAX_ORPHANS - 1 {
            put(
                &store.db,
                "mcp_credential_operations",
                &json!({"id":new_id(),"orphan":true}),
            )
            .unwrap();
        }
        {
            let tx = store.db.transaction().unwrap();
            let old = server(&tx, &id).unwrap();
            let mut next = definition(&["API_KEY", "OTHER_KEY"]);
            next.args.push("edited".into());
            assert!(detach_mcp_credentials(&tx, &old, Some(&next)).is_err());
        }
        assert_eq!(bindings(&store.db, &id).unwrap().len(), 2);
    }
    #[test]
    fn prospective_secret_check_metadata_and_receipts_never_expose_references() {
        let mut store = store();
        let id = create(&store, &["API_KEY"]);
        let (op, reference) = install(&mut store, &id, "API_KEY");
        let mut value = server(&store.db, &id).unwrap();
        value["catalog"] = json!([{"name":"fixture","input_schema":{"type":"object","properties":{"quoted\"metadata":{"type":"string"}}}}]);
        put(&store.db, "managed_capabilities", &value).unwrap();
        let metadata = store.metadata_for_mcp_credential_check(&id).unwrap();
        assert!(metadata.get("definition").is_some() && metadata.get("catalog").is_some());
        assert!(metadata["definition"].get("credential_refs").is_none());
        assert!(!metadata.to_string().contains(reference.as_str()));
        assert!(metadata["catalog"][0]["input_schema"]["properties"]
            .get("quoted\"metadata")
            .is_some());
        let replay = store
            .existing_mcp_credential_set(&op, &id, "API_KEY", 1)
            .unwrap()
            .unwrap();
        assert_eq!(replay["operation"]["slot"], "API_KEY");
        assert_eq!(replay["operation"]["owner_kind"], "mcp");
        assert!(replay["operation"].get("operation").is_none());
        assert!(!replay.to_string().contains(reference.as_str()));
    }
}
