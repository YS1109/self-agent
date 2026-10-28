//! Named-provider metadata, immutable enqueue snapshots and one-shot vault
//! intents. SQLite never receives secret values or secret-derived fingerprints.
//! The caller performs OS work after releasing Store's lock; recovery never
//! replays an operation. Opaque vault references occur only in private tables.
use super::*;
use crate::capabilities::{
    providers::{self as validation, PinnedProvider, ValidatedProvider},
    vault::{CredentialRef, VaultBinding},
};
use harness_core::{
    CredentialOperationView, NamedProviderView, NamedProviders, ProviderDefinition,
    ProviderReference,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_PROVIDERS: usize = 32;
const MAX_OPERATIONS: usize = 1024;
const MAX_ORPHANS: usize = 256;
const MAX_CLEANUP_OPERATIONS: usize = 256;

pub(crate) struct ProviderRecord {
    pub id: String,
    pub revision: u64,
    pub enabled: bool,
    pub definition: ValidatedProvider,
    pub credential_ref: Option<CredentialRef>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialWorkKind {
    Set,
    Delete,
}
pub(crate) struct CredentialWork {
    pub operation_id: String,
    pub owner_id: String,
    pub kind: CredentialWorkKind,
    pub binding: VaultBinding,
    pub reference: CredentialRef,
}
#[derive(Clone, Copy)]
pub(crate) enum CredentialOutcome {
    Succeeded,
    Rejected,
    Unknown,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    id: String,
    command: Command,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    SaveProvider {
        id: Option<String>,
        expected_revision: Option<u64>,
        definition: ProviderDefinition,
    },
    SetProviderEnabled {
        id: String,
        expected_revision: u64,
        enabled: bool,
    },
    DeleteProvider {
        id: String,
        expected_revision: u64,
    },
}

pub(super) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS named_providers(id TEXT PRIMARY KEY,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS provider_commands(id TEXT PRIMARY KEY,digest TEXT NOT NULL,receipt TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS task_providers(id TEXT PRIMARY KEY,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS coordinator_providers(id TEXT PRIMARY KEY,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS plan_providers(id TEXT PRIMARY KEY,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS plan_role_providers(id TEXT PRIMARY KEY,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS credential_operations(id TEXT PRIMARY KEY,body TEXT NOT NULL);")?;
    Ok(())
}
fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn uuid(value: &str) -> Result<()> {
    if value.len() != 36
        || !uuid::Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
    {
        bail!("模型服务或操作标识必须是规范 UUID");
    }
    Ok(())
}
fn get(db: &Connection, table: &str, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = db
        .query_row(
            &format!("SELECT body FROM {table} WHERE id=?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(|_| anyhow!("模型服务私有记录无效")))
        .transpose()
}
fn provider(db: &Connection, id: &str) -> Result<Value> {
    uuid(id)?;
    let value = get(db, "named_providers", id)?.context("模型服务不存在或已经删除")?;
    if value["deleted"] == true {
        bail!("模型服务不存在或已经删除");
    }
    Ok(value)
}
fn revision(value: &Value) -> Result<u64> {
    value["revision"].as_u64().context("模型服务修订无效")
}
fn next_revision(value: &Value) -> Result<u64> {
    revision(value)?
        .checked_add(1)
        .filter(|v| *v <= 9_007_199_254_740_991)
        .context("模型服务修订已达到限制")
}
fn check_revision(value: &Value, expected: u64) -> Result<()> {
    if revision(value)? != expected {
        bail!("模型服务修订已改变，请刷新后重试");
    }
    Ok(())
}
fn parsed_record(value: &Value) -> Result<ProviderRecord> {
    let definition: ProviderDefinition = serde_json::from_value(value["definition"].clone())
        .map_err(|_| anyhow!("模型服务元数据无效"))?;
    let definition = validation::validate_definition(definition, &[])?;
    let id = text(value, "id")?.to_owned();
    let binding = validation::credential_binding(&id, &definition)?;
    let credential_ref = value["credential_ref"]
        .as_str()
        .map(|reference| CredentialRef::parse(reference, &binding))
        .transpose()?;
    Ok(ProviderRecord {
        id,
        revision: revision(value)?,
        enabled: value["enabled"] == true,
        definition,
        credential_ref,
    })
}
fn reject_known_metadata(value: &Value, known: &[String]) -> Result<()> {
    match value {
        Value::String(value)
            if known
                .iter()
                .any(|secret| !secret.is_empty() && value.contains(secret)) =>
        {
            bail!("模型服务元数据不能包含访问凭据")
        }
        Value::Array(values) => {
            for value in values {
                reject_known_metadata(value, known)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                reject_known_metadata(&json!(key), known)?;
                reject_known_metadata(value, known)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn operations(db: &Connection) -> Result<Vec<Value>> {
    rows(db, "credential_operations")
}
/// Pending sets reserve room for their possible orphan/old-key record so their
/// finalization can never be blocked by a later admission consuming that slot.
fn capacity(db: &Connection, new_operations: usize, new_orphans: usize) -> Result<()> {
    let ops = operations(db)?;
    let reserved = ops
        .iter()
        .filter(|op| {
            op["kind"] == "set" && matches!(op["status"].as_str(), Some("pending" | "inflight"))
        })
        .count();
    let reserved_orphans = ops
        .iter()
        .filter(|op| {
            matches!(op["status"].as_str(), Some("pending" | "inflight"))
                && op["target_operation_id"].is_null()
        })
        .count();
    let orphans = ops.iter().filter(|op| op["orphan"] == true).count();
    let normal_operations = ops
        .iter()
        .filter(|op| op["target_operation_id"].is_null())
        .count();
    if normal_operations + reserved + new_operations > MAX_OPERATIONS
        || orphans + reserved_orphans + new_orphans > MAX_ORPHANS
    {
        bail!("凭据操作或待清理记录已达到限制，请先处理已有记录；没有遗忘未清理凭据");
    }
    Ok(())
}
// Owner-requested cleanup has its own finite tail. It cannot consume an
// enrollment slot or disappear at the ordinary-operation ceiling. Repeated
// failed/Unknown cleanup can exhaust this explicit audit bound; never retry.
fn cleanup_capacity(db: &Connection) -> Result<()> {
    if operations(db)?
        .iter()
        .filter(|op| !op["target_operation_id"].is_null())
        .count()
        >= MAX_CLEANUP_OPERATIONS
    {
        bail!("显式凭据清理操作已达到 256 次审计上限；未遗忘待清理引用，也未自动重试");
    }
    Ok(())
}
fn current_reference(db: &Connection, reference: &str) -> Result<bool> {
    Ok(rows(db, "named_providers")?
        .iter()
        .any(|value| value["deleted"] != true && value["credential_ref"] == reference))
}
fn orphan(db: &Connection, owner: &Value, reference: &str) -> Result<()> {
    if operations(db)?
        .iter()
        .any(|op| op["reference"] == reference && op["orphan"] == true)
    {
        return Ok(());
    }
    put(
        db,
        "credential_operations",
        &json!({"id":new_id(),"owner_kind":"provider","owner_id":owner["id"],"kind":"cleanup","status":"cleanup_required","reason":"credential_detached","can_cleanup":true,"orphan":true,"reference":reference,"definition":owner["definition"],"created_at":now(),"updated_at":now()}),
    )
}
fn public_operation(value: &Value) -> Result<CredentialOperationView> {
    Ok(CredentialOperationView {
        id: text(value, "id")?.into(),
        owner_kind: "provider".into(),
        owner_id: text(value, "owner_id")?.into(),
        kind: text(value, "kind")?.into(),
        status: text(value, "status")?.into(),
        reason: value["reason"].as_str().map(str::to_owned),
        can_cleanup: value["can_cleanup"] == true,
        cleanup_status: value["cleanup_status"].as_str().map(str::to_owned),
        created_at: text(value, "created_at")?.into(),
        updated_at: text(value, "updated_at")?.into(),
    })
}
fn receipt(value: &Value, replayed: bool) -> Result<Value> {
    Ok(
        json!({"operation":public_operation(value)?,"replayed":replayed,"no_secret_applied":replayed}),
    )
}
fn existing_operation(db: &Connection, id: &str, metadata: &Value) -> Result<Option<Value>> {
    uuid(id)?;
    get(db, "credential_operations", id)?
        .map(|op| {
            if op["metadata_digest"] != hash(metadata) {
                bail!("凭据操作标识已被不同元数据使用");
            }
            receipt(&op, true)
        })
        .transpose()
}
fn active_operation(db: &Connection, owner_id: &str) -> Result<()> {
    if operations(db)?.iter().any(|op| {
        op["owner_id"] == owner_id && matches!(op["status"].as_str(), Some("pending" | "inflight"))
    }) {
        bail!("此模型服务已有待确认凭据操作，请先查询结果");
    }
    Ok(())
}
fn new_operation(id: &str, owner: &Value, kind: &str, metadata: &Value, reference: &str) -> Value {
    json!({"id":id,"owner_kind":"provider","owner_id":owner["id"],"kind":kind,"status":"pending","reason":Value::Null,"expected_revision":owner["revision"],"reference":reference,"definition":owner["definition"],"metadata_digest":hash(metadata),"orphan":false,"can_cleanup":false,"created_at":now(),"updated_at":now()})
}

impl Store {
    pub(crate) fn named_providers(&self) -> Result<NamedProviders> {
        let ops = operations(&self.db)?;
        let mut result = NamedProviders::default();
        for value in rows(&self.db, "named_providers")? {
            if value["deleted"] == true {
                continue;
            }
            let record = parsed_record(&value)?;
            result.providers.push(NamedProviderView {
                id: record.id.clone(),
                revision: record.revision,
                definition: record.definition.definition().clone(),
                enabled: record.enabled,
                credential_configured: record.credential_ref.is_some(),
                credential_status: if record.credential_ref.is_some() {
                    "configured_not_checked"
                } else {
                    "not_configured"
                }
                .into(),
                orphaned_credentials: ops
                    .iter()
                    .filter(|op| op["owner_id"] == record.id && op["orphan"] == true)
                    .count(),
                updated_at: text(&value, "updated_at")?.into(),
            });
        }
        result.credential_operations = ops.iter().map(public_operation).collect::<Result<_>>()?;
        Ok(result)
    }
    pub(crate) fn provider_record(&self, id: &str) -> Result<ProviderRecord> {
        parsed_record(&provider(&self.db, id)?)
    }
    pub(crate) fn provider_command(
        &mut self,
        envelope: &Value,
        known_secrets: &[String],
    ) -> Result<Value> {
        if envelope.to_string().len() > 16 * 1024 {
            bail!("模型服务管理指令过大");
        }
        reject_known_metadata(envelope, known_secrets)?;
        let parsed: Envelope = serde_json::from_value(envelope.clone())
            .map_err(|_| anyhow!("模型服务管理指令字段无效"))?;
        uuid(&parsed.id)?;
        let existing: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT digest,receipt FROM provider_commands WHERE id=?1",
                [&parsed.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((digest, receipt)) = existing {
            if digest != hash(envelope) {
                bail!("模型服务指令标识已被另一条指令使用");
            }
            return Ok(serde_json::from_str(&receipt)?);
        }
        let tx = self.db.transaction()?;
        let mut value = match parsed.command {
            Command::SaveProvider {
                id,
                expected_revision,
                definition,
            } => {
                let validated = validation::validate_definition(definition, known_secrets)?;
                let id = id.unwrap_or_else(new_id);
                uuid(&id)?;
                let mut value = if let Some(existing) = get(&tx, "named_providers", &id)? {
                    if existing["deleted"] == true {
                        bail!("模型服务标识已删除，不能重新创建");
                    }
                    check_revision(
                        &existing,
                        expected_revision.context("更新模型服务必须提供预期修订")?,
                    )?;
                    existing
                } else {
                    if expected_revision.unwrap_or(0) != 0 {
                        bail!("模型服务不存在或已经删除");
                    }
                    if rows(&tx, "named_providers")?
                        .iter()
                        .filter(|v| v["deleted"] != true)
                        .count()
                        >= MAX_PROVIDERS
                    {
                        bail!("模型服务最多注册 32 个");
                    }
                    json!({"id":id,"revision":0,"enabled":false,"credential_ref":Value::Null,"deleted":false})
                };
                if let Some(reference) = value["credential_ref"].as_str() {
                    let previous = parsed_record(&value)?;
                    if validation::retained_credential(
                        &id,
                        &previous.definition,
                        &validated,
                        previous.credential_ref.as_ref(),
                    )?
                    .is_none()
                    {
                        capacity(&tx, 1, 1)?;
                        orphan(&tx, &value, reference)?;
                        value["credential_ref"] = Value::Null;
                    }
                }
                value["definition"] = json!(validated.definition());
                value
            }
            Command::SetProviderEnabled {
                id,
                expected_revision,
                enabled,
            } => {
                let mut value = provider(&tx, &id)?;
                check_revision(&value, expected_revision)?;
                value["enabled"] = json!(enabled);
                value
            }
            Command::DeleteProvider {
                id,
                expected_revision,
            } => {
                let mut value = provider(&tx, &id)?;
                check_revision(&value, expected_revision)?;
                if let Some(reference) = value["credential_ref"].as_str() {
                    capacity(&tx, 1, 1)?;
                    orphan(&tx, &value, reference)?;
                }
                value["deleted"] = json!(true);
                value["enabled"] = json!(false);
                value["credential_ref"] = Value::Null;
                value
            }
        };
        value["revision"] = json!(next_revision(&value)?);
        value["updated_at"] = json!(now());
        put(&tx, "named_providers", &value)?;
        let receipt = json!({"command_id":parsed.id,"status":"completed","provider_id":value["id"],"revision":value["revision"]});
        tx.execute(
            "INSERT INTO provider_commands(id,digest,receipt) VALUES(?1,?2,?3)",
            params![parsed.id, hash(envelope), receipt.to_string()],
        )?;
        tx.commit()?;
        Ok(receipt)
    }
    /// Read receipt before resolving a possibly edited/deleted provider or
    /// creating new secret material. Replay compares metadata, never secret bytes.
    pub(crate) fn existing_provider_credential_set(
        &self,
        operation_id: &str,
        provider_id: &str,
        expected_revision: u64,
    ) -> Result<Option<Value>> {
        existing_operation(
            &self.db,
            operation_id,
            &json!({"owner_kind":"provider","owner_id":provider_id,"expected_revision":expected_revision,"kind":"set"}),
        )
    }
    pub(crate) fn begin_provider_credential_set(
        &mut self,
        operation_id: &str,
        provider_id: &str,
        expected_revision: u64,
        reference: &CredentialRef,
    ) -> Result<Value> {
        let metadata = json!({"owner_kind":"provider","owner_id":provider_id,"expected_revision":expected_revision,"kind":"set"});
        if let Some(receipt) = existing_operation(&self.db, operation_id, &metadata)? {
            return Ok(receipt);
        }
        let tx = self.db.transaction()?;
        let value = provider(&tx, provider_id)?;
        check_revision(&value, expected_revision)?;
        next_revision(&value)?; // Reserve a representable CAS revision before OS work.
        active_operation(&tx, provider_id)?;
        capacity(&tx, 2, 1)?;
        let record = parsed_record(&value)?;
        let binding = validation::credential_binding(provider_id, &record.definition)?;
        CredentialRef::parse(reference.as_str(), &binding)?;
        if current_reference(&tx, reference.as_str())?
            || operations(&tx)?
                .iter()
                .any(|op| op["reference"] == reference.as_str())
        {
            bail!("凭据轮换必须使用全新的引用，不能重用旧操作");
        }
        let operation = new_operation(operation_id, &value, "set", &metadata, reference.as_str());
        put(&tx, "credential_operations", &operation)?;
        tx.commit()?;
        receipt(&operation, false)
    }
    pub(crate) fn begin_provider_credential_delete(
        &mut self,
        operation_id: &str,
        provider_id: &str,
        expected_revision: u64,
    ) -> Result<Value> {
        let metadata = json!({"owner_kind":"provider","owner_id":provider_id,"expected_revision":expected_revision,"kind":"delete"});
        if let Some(receipt) = existing_operation(&self.db, operation_id, &metadata)? {
            return Ok(receipt);
        }
        let tx = self.db.transaction()?;
        let mut value = provider(&tx, provider_id)?;
        check_revision(&value, expected_revision)?;
        active_operation(&tx, provider_id)?;
        capacity(&tx, 1, 1)?;
        let reference = value["credential_ref"]
            .as_str()
            .context("模型服务没有已绑定凭据")?;
        let operation = new_operation(operation_id, &value, "delete", &metadata, reference);
        value["credential_ref"] = Value::Null;
        value["revision"] = json!(next_revision(&value)?);
        value["updated_at"] = json!(now());
        put(&tx, "named_providers", &value)?;
        put(&tx, "credential_operations", &operation)?;
        tx.commit()?;
        receipt(&operation, false)
    }
    pub(crate) fn begin_credential_cleanup(
        &mut self,
        operation_id: &str,
        target_operation_id: &str,
    ) -> Result<Value> {
        uuid(target_operation_id)?;
        let metadata = json!({"kind":"cleanup","target_operation_id":target_operation_id});
        if let Some(receipt) = existing_operation(&self.db, operation_id, &metadata)? {
            return Ok(receipt);
        }
        let tx = self.db.transaction()?;
        let mut target = get(&tx, "credential_operations", target_operation_id)?
            .context("待清理凭据操作不存在")?;
        if target["can_cleanup"] != true || target["orphan"] != true {
            bail!("此凭据操作当前不可清理");
        }
        let reference = text(&target, "reference")?;
        if current_reference(&tx, reference)? {
            bail!("凭据仍被模型服务引用，不能清理");
        }
        cleanup_capacity(&tx)?;
        let mut operation = new_operation(
            operation_id,
            &json!({"id":target["owner_id"],"revision":0,"definition":target["definition"]}),
            "cleanup",
            &metadata,
            reference,
        );
        operation["target_operation_id"] = json!(target_operation_id);
        target["can_cleanup"] = json!(false);
        target["cleanup_operation_id"] = json!(operation_id);
        target["cleanup_status"] = json!("pending");
        target["updated_at"] = json!(now());
        put(&tx, "credential_operations", &target)?;
        put(&tx, "credential_operations", &operation)?;
        tx.commit()?;
        receipt(&operation, false)
    }
    pub(crate) fn credential_operation(&self, id: &str) -> Result<CredentialOperationView> {
        uuid(id)?;
        public_operation(&get(&self.db, "credential_operations", id)?.context("凭据操作不存在")?)
    }
    pub(crate) fn claim_credential_operation(
        &mut self,
        id: &str,
    ) -> Result<Option<CredentialWork>> {
        uuid(id)?;
        let tx = self.db.transaction()?;
        let Some(mut op) = get(&tx, "credential_operations", id)? else {
            return Ok(None);
        };
        if op["status"] != "pending" {
            return Ok(None);
        }
        let owner_id = text(&op, "owner_id")?.to_owned();
        if op["kind"] == "set" {
            let current = get(&tx, "named_providers", &owner_id)?;
            if current
                .is_none_or(|v| v["deleted"] == true || v["revision"] != op["expected_revision"])
            {
                op["status"] = json!("rejected");
                op["reason"] = json!("provider_changed_before_vault_call");
                op["updated_at"] = json!(now());
                put(&tx, "credential_operations", &op)?;
                tx.commit()?;
                return Ok(None);
            }
        }
        let definition = validation::validate_definition(
            serde_json::from_value(op["definition"].clone())
                .map_err(|_| anyhow!("凭据操作元数据无效"))?,
            &[],
        )?;
        let binding = validation::credential_binding(&owner_id, &definition)?;
        let reference = CredentialRef::parse(text(&op, "reference")?, &binding)?;
        let kind = if op["kind"] == "set" {
            CredentialWorkKind::Set
        } else {
            CredentialWorkKind::Delete
        };
        if kind == CredentialWorkKind::Delete && current_reference(&tx, reference.as_str())? {
            bail!("凭据仍被模型服务引用，不能删除");
        }
        op["status"] = json!("inflight");
        op["updated_at"] = json!(now());
        put(&tx, "credential_operations", &op)?;
        tx.commit()?;
        Ok(Some(CredentialWork {
            operation_id: id.into(),
            owner_id,
            kind,
            binding,
            reference,
        }))
    }
    pub(crate) fn finish_credential_operation(
        &mut self,
        id: &str,
        outcome: CredentialOutcome,
    ) -> Result<bool> {
        let tx = self.db.transaction()?;
        let Some(mut op) = get(&tx, "credential_operations", id)? else {
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
                let current = get(&tx, "named_providers", text(&op, "owner_id")?)?;
                if let Some(mut current) = current
                    .filter(|v| v["deleted"] != true && v["revision"] == op["expected_revision"])
                {
                    if let Some(old) = current["credential_ref"].as_str() {
                        orphan(&tx, &current, old)?;
                    }
                    current["credential_ref"] = op["reference"].clone();
                    current["revision"] = json!(next_revision(&current)?);
                    current["updated_at"] = json!(now());
                    put(&tx, "named_providers", &current)?;
                } else {
                    op["status"] = json!("orphaned");
                    op["reason"] = json!("provider_revision_changed");
                    op["orphan"] = json!(true);
                    op["can_cleanup"] = json!(true);
                }
            } else if unknown {
                op["orphan"] = json!(true);
                op["can_cleanup"] = json!(true);
            }
        } else if let Some(target_id) = op["target_operation_id"].as_str() {
            let mut target =
                get(&tx, "credential_operations", target_id)?.context("待清理凭据操作不存在")?;
            if target["cleanup_operation_id"] != op["id"] {
                bail!("凭据清理操作关联已改变");
            }
            target["can_cleanup"] = json!(!success);
            target["orphan"] = json!(!success);
            target["updated_at"] = json!(now());
            target["cleanup_status"] = json!(if success {
                "completed"
            } else if unknown {
                "unknown"
            } else {
                "rejected"
            });
            // Cleanup confirms absence of the credential, not the historical
            // effect of a timed-out set/delete. Never erase that uncertainty.
            if success && target["status"] == "cleanup_required" {
                target["status"] = json!("cleaned");
                target["reason"] = json!("credential_deleted");
            }
            put(&tx, "credential_operations", &target)?;
        } else if !success {
            op["orphan"] = json!(true);
            op["can_cleanup"] = json!(true);
        }
        op["updated_at"] = json!(now());
        put(&tx, "credential_operations", &op)?;
        tx.commit()?;
        Ok(true)
    }
    pub(crate) fn coordinator_provider(&self, job_id: &str) -> Result<Option<PinnedProvider>> {
        let job =
            record(&self.db, "coordinator_jobs", job_id).map_err(|_| anyhow!("编排作业不存在"))?;
        private_snapshot(&self.db, "coordinator_providers", &job)
    }
    pub(crate) fn check_coordinator_provider(&self, job_id: &str) -> Result<()> {
        check_coordinator_provider(&self.db, job_id)
    }
    pub(crate) fn task_provider(&self, task_id: &str) -> Result<Option<PinnedProvider>> {
        task_provider(&self.db, task_id)
    }
    pub(crate) fn check_task_provider(&self, task_id: &str) -> Result<()> {
        if let Some(pinned) = task_provider(&self.db, task_id)? {
            let current = self.provider_record(pinned.id())?;
            pinned.check_current(
                &current.id,
                current.revision,
                current.enabled,
                &current.definition,
                current.credential_ref.as_ref(),
            )?;
        }
        Ok(())
    }
}

pub(super) fn check_coordinator_provider(db: &Connection, job_id: &str) -> Result<()> {
    let job = record(db, "coordinator_jobs", job_id)?;
    if let Some(pinned) = private_snapshot(db, "coordinator_providers", &job)? {
        let current = parsed_record(&provider(db, pinned.id())?)?;
        pinned.check_current(
            &current.id,
            current.revision,
            current.enabled,
            &current.definition,
            current.credential_ref.as_ref(),
        )?;
    }
    Ok(())
}

pub(super) fn snapshot_task_provider(db: &Connection, task: &mut Value) -> Result<()> {
    let Some(id) = task["agent"]["provider_id"].as_str() else {
        task["managed_provider"] = Value::Null;
        return Ok(());
    };
    if task["agent"]["executor"] != "native" {
        bail!("命名模型服务只适用于 Native 执行器");
    }
    let record = parsed_record(&provider(db, id)?)?;
    let model = task["agent"]["model"].as_str().unwrap_or_default();
    let pinned = PinnedProvider::new(
        id,
        record.revision,
        record.enabled,
        record.definition.clone(),
        record.credential_ref.clone(),
        model,
        &[],
    )?;
    let reference = ProviderReference {
        id: id.into(),
        revision: record.revision,
        name: record.definition.definition().name.clone(),
        model: pinned.model().into(),
    };
    let task_id = text(task, "id")?;
    if get(db, "task_providers", task_id)?.is_some() {
        bail!("任务模型服务快照已经固定，不能覆盖");
    }
    put(
        db,
        "task_providers",
        &json!({"id":task_id,"provider_id":id,"revision":record.revision,"definition":record.definition.definition(),"credential_ref":record.credential_ref.as_ref().map(CredentialRef::as_str),"model":pinned.model()}),
    )?;
    task["managed_provider"] = json!(reference);
    Ok(())
}
fn task_provider(db: &Connection, task_id: &str) -> Result<Option<PinnedProvider>> {
    let task = record(db, "tasks", task_id).map_err(|_| anyhow!("任务不存在"))?;
    let Some(snapshot) = get(db, "task_providers", task_id)? else {
        if !task["managed_provider"].is_null() || task["agent"]["provider_id"].as_str().is_some() {
            bail!("任务模型服务私有快照缺失，不能自动选择新服务");
        }
        return Ok(None);
    };
    Ok(Some(decode_snapshot(&snapshot)?))
}
fn decode_snapshot(snapshot: &Value) -> Result<PinnedProvider> {
    let definition = validation::validate_definition(
        serde_json::from_value(snapshot["definition"].clone())
            .map_err(|_| anyhow!("模型服务私有快照无效"))?,
        &[],
    )?;
    let id = text(snapshot, "provider_id")?;
    let binding = validation::credential_binding(id, &definition)?;
    let reference = snapshot["credential_ref"]
        .as_str()
        .map(|v| CredentialRef::parse(v, &binding))
        .transpose()?;
    Ok(PinnedProvider::new(
        id,
        revision(snapshot)?,
        true,
        definition,
        reference,
        text(snapshot, "model")?,
        &[],
    )?)
}
fn private_snapshot(
    db: &Connection,
    table: &str,
    entity: &Value,
) -> Result<Option<PinnedProvider>> {
    let Some(snapshot) = get(db, table, text(entity, "id")?)? else {
        if !entity["managed_provider"].is_null() {
            bail!("模型服务私有快照缺失，不能自动选择新服务");
        }
        return Ok(None);
    };
    Ok(Some(decode_snapshot(&snapshot)?))
}
fn snapshot_reference(pinned: &PinnedProvider) -> ProviderReference {
    ProviderReference {
        id: pinned.id().into(),
        revision: pinned.revision(),
        name: pinned.definition().definition().name.clone(),
        model: pinned.model().into(),
    }
}
pub(super) fn snapshot_coordinator_provider(
    db: &Connection,
    job: &mut Value,
    settings: &Settings,
) -> Result<()> {
    if settings.provider_source != "named" {
        job["managed_provider"] = Value::Null;
        return Ok(());
    }
    let mut agent = AgentProfile::default();
    let effective = effective_settings(db, settings, &mut agent)?;
    let id = agent
        .provider_id
        .as_deref()
        .context("尚未选择编排模型服务")?;
    let record = parsed_record(&provider(db, id)?)?;
    let pinned = PinnedProvider::new(
        id,
        record.revision,
        record.enabled,
        record.definition.clone(),
        record.credential_ref.clone(),
        &effective.model,
        &[],
    )?;
    let job_id = text(job, "id")?;
    if get(db, "coordinator_providers", job_id)?.is_some() {
        bail!("编排模型服务快照已经固定");
    }
    put(
        db,
        "coordinator_providers",
        &json!({"id":job_id,"provider_id":id,"revision":record.revision,"definition":record.definition.definition(),"credential_ref":record.credential_ref.as_ref().map(CredentialRef::as_str),"model":pinned.model()}),
    )?;
    job["managed_provider"] = json!(snapshot_reference(&pinned));
    Ok(())
}
fn copy_snapshot(
    db: &Connection,
    from_table: &str,
    source: &Value,
    to_table: &str,
    target: &mut Value,
) -> Result<()> {
    let pinned = private_snapshot(db, from_table, source)?;
    if let Some(pinned) = pinned {
        let mut snapshot =
            get(db, from_table, text(source, "id")?)?.context("模型服务私有快照缺失")?;
        let target_id = text(target, "id")?;
        if get(db, to_table, target_id)?.is_some() {
            bail!("模型服务快照已经固定，不能替换");
        }
        snapshot["id"] = json!(target_id);
        put(db, to_table, &snapshot)?;
        target["managed_provider"] = json!(snapshot_reference(&pinned));
    } else {
        target["managed_provider"] = Value::Null;
    }
    Ok(())
}
pub(super) fn validate_plan_role_providers(db: &Connection, agents: &Value) -> Result<()> {
    for agent in agents.as_array().into_iter().flatten() {
        if let Some(id) = agent["provider_id"].as_str() {
            let record = parsed_record(&provider(db, id)?)?;
            PinnedProvider::new(
                id,
                record.revision,
                record.enabled,
                record.definition,
                record.credential_ref,
                agent["model"].as_str().unwrap_or_default(),
                &[],
            )?;
        }
    }
    Ok(())
}

pub(super) fn inherit_plan_provider(db: &Connection, job: &Value, plan: &mut Value) -> Result<()> {
    copy_snapshot(db, "coordinator_providers", job, "plan_providers", plan)?;
    let mut roles = serde_json::Map::new();
    let mut references = serde_json::Map::new();
    for agent in plan["agents"].as_array().into_iter().flatten() {
        let Some(id) = agent["provider_id"].as_str() else {
            continue;
        };
        let record = parsed_record(&provider(db, id)?)?;
        let pinned = PinnedProvider::new(
            id,
            record.revision,
            record.enabled,
            record.definition.clone(),
            record.credential_ref.clone(),
            agent["model"].as_str().unwrap_or_default(),
            &[],
        )?;
        let role_id = text(agent, "id")?;
        roles.insert(role_id.into(),json!({"id":plan["id"],"provider_id":id,"revision":record.revision,"definition":record.definition.definition(),"credential_ref":record.credential_ref.as_ref().map(CredentialRef::as_str),"model":pinned.model()}));
        references.insert(role_id.into(), json!(snapshot_reference(&pinned)));
    }
    put(
        db,
        "plan_role_providers",
        &json!({"id":plan["id"],"roles":roles}),
    )?;
    plan["managed_role_providers"] = Value::Object(references);
    Ok(())
}

pub(super) fn inherit_task_provider(db: &Connection, plan: &Value, task: &mut Value) -> Result<()> {
    let Some(provider_id) = task["agent"]["provider_id"].as_str() else {
        task["managed_provider"] = Value::Null;
        return Ok(());
    };
    let stored = get(db, "plan_role_providers", text(plan, "id")?)?
        .context("计划角色的模型服务固定快照缺失")?;
    let mut snapshot = stored["roles"][text(&task["agent"], "id")?].clone();
    let pinned = decode_snapshot(&snapshot)?;
    if pinned.id() != provider_id
        || pinned.model() != task["agent"]["model"].as_str().unwrap_or_default()
    {
        bail!("计划角色模型服务身份不匹配");
    }
    let current = parsed_record(&provider(db, pinned.id())?)?;
    pinned.check_current(
        &current.id,
        current.revision,
        current.enabled,
        &current.definition,
        current.credential_ref.as_ref(),
    )?;
    let id = text(task, "id")?;
    if get(db, "task_providers", id)?.is_some() {
        bail!("任务模型服务快照已经存在");
    }
    snapshot["id"] = json!(id);
    put(db, "task_providers", &snapshot)?;
    task["managed_provider"] = json!(snapshot_reference(&pinned));
    Ok(())
}
pub(super) fn inherit_coordinator_provider(
    db: &Connection,
    plan: &Value,
    job: &mut Value,
) -> Result<()> {
    copy_snapshot(db, "plan_providers", plan, "coordinator_providers", job)
}
/// Resolve metadata only. No key/header inherited from manual/env/local Codex.
pub(super) fn effective_settings(
    db: &Connection,
    settings: &Settings,
    agent: &mut AgentProfile,
) -> Result<Settings> {
    if agent.executor == "codex" {
        if agent.provider_id.is_some() {
            bail!("Codex 角色不能选择应用管理的命名模型服务");
        }
        return Ok(settings.clone());
    }
    let id = agent.provider_id.clone().or_else(|| {
        if settings.provider_source == "named" {
            settings.provider_id.clone()
        } else {
            None
        }
    });
    if settings.provider_source == "named" && id.is_none() {
        bail!("尚未选择命名模型服务");
    }
    let Some(id) = id else {
        let mut effective = settings.clone();
        if !agent.model.trim().is_empty() {
            effective.model = agent.model.clone();
        }
        return Ok(effective);
    };
    let profile = parsed_record(&provider(db, &id)?)?;
    if !profile.enabled {
        bail!("所选命名模型服务尚未启用");
    }
    let mut effective = settings.clone();
    effective.provider_source = "manual".into();
    effective.provider_id = Some(id.clone());
    effective.base_url = profile.definition.base_url().into();
    effective.models_endpoint = profile.definition.models_url().into();
    effective.model = if agent.model.is_empty() {
        profile.definition.definition().model.clone()
    } else {
        agent.model.clone()
    };
    effective.api_key.clear();
    effective.provider_headers.clear();
    agent.provider_id = Some(id);
    Ok(effective)
}
pub(super) fn metadata_configured(db: &Connection, settings: &Settings) -> bool {
    let mut agent = AgentProfile {
        executor: "native".into(),
        ..AgentProfile::default()
    };
    effective_settings(db, settings, &mut agent).is_ok_and(|settings| settings.configured())
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
            if let Some(mut target) = get(db, "credential_operations", target_id)? {
                if target["cleanup_operation_id"] == op["id"] {
                    target["can_cleanup"] = json!(true);
                    target["cleanup_status"] = json!("unknown");
                    target["updated_at"] = json!(now());
                    put(db, "credential_operations", &target)?;
                }
            }
        } else {
            op["orphan"] = json!(true);
            op["can_cleanup"] = json!(true);
        }
        put(db, "credential_operations", &op)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::vault::{FreshCredential, SecretBytes};
    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }
    fn definition() -> ProviderDefinition {
        ProviderDefinition {
            name: "本地测试服务".into(),
            base_url: "https://example.invalid/v1".into(),
            models_endpoint: String::new(),
            model: "fixture-model".into(),
            allow_insecure_http: false,
        }
    }
    fn command(value: Value) -> Value {
        json!({"id":new_id(),"command":value})
    }
    fn send(store: &mut Store, value: Value) -> Value {
        store.provider_command(&command(value), &[]).unwrap()
    }
    fn profile(store: &mut Store) -> Value {
        send(
            store,
            json!({"type":"save_provider","definition":definition()}),
        )
    }
    fn enable(store: &mut Store, receipt: &Value, enabled: bool) -> Value {
        send(
            store,
            json!({"type":"set_provider_enabled","id":receipt["provider_id"],"expected_revision":receipt["revision"],"enabled":enabled}),
        )
    }
    fn fresh(record: &ProviderRecord) -> FreshCredential {
        FreshCredential::new(
            &validation::credential_binding(&record.id, &record.definition).unwrap(),
            SecretBytes::new(b"provider-fixture-secret-never-in-db".to_vec()).unwrap(),
        )
    }
    fn install(store: &mut Store, id: &str) -> String {
        let record = store.provider_record(id).unwrap();
        let fresh = fresh(&record);
        let op = new_id();
        store
            .begin_provider_credential_set(&op, id, record.revision, fresh.reference())
            .unwrap();
        let work = store.claim_credential_operation(&op).unwrap().unwrap();
        assert!(matches!(work.kind, CredentialWorkKind::Set));
        store
            .finish_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap();
        op
    }
    fn fixture_task(store: &mut Store, profile_id: &str) -> String {
        let mut agent = AgentProfile {
            provider_id: Some(profile_id.into()),
            ..AgentProfile::default()
        };
        let settings = Settings::default();
        let metadata = effective_settings(&store.db, &settings, &mut agent).unwrap();
        agent.model = metadata.model;
        let id = new_id();
        let mut task = json!({"id":id,"status":"completed","task_kind":"main","agent":agent});
        snapshot_task_provider(&store.db, &mut task).unwrap();
        put(&store.db, "tasks", &task).unwrap();
        id
    }
    #[test]
    fn provider_commands_are_idempotent_bounded_revisioned_tombstones() {
        let mut store = store();
        let request = command(json!({"type":"save_provider","definition":definition()}));
        let first = store.provider_command(&request, &[]).unwrap();
        assert_eq!(first, store.provider_command(&request, &[]).unwrap());
        assert!(!store.named_providers().unwrap().providers[0].enabled);
        let mut mismatch = request.clone();
        mismatch["command"]["definition"]["name"] = json!("other");
        assert!(store.provider_command(&mismatch, &[]).is_err());
        assert!(store.provider_command(&command(json!({"type":"set_provider_enabled","id":first["provider_id"],"expected_revision":0,"enabled":true})),&[]).is_err());
        let enabled = enable(&mut store, &first, true);
        send(
            &mut store,
            json!({"type":"delete_provider","id":enabled["provider_id"],"expected_revision":enabled["revision"]}),
        );
        assert!(store.named_providers().unwrap().providers.is_empty());
        assert!(store.provider_command(&command(json!({"type":"save_provider","id":first["provider_id"],"definition":definition()})),&[]).is_err());
        for _ in 0..MAX_PROVIDERS {
            profile(&mut store);
        }
        assert!(store
            .provider_command(
                &command(json!({"type":"save_provider","definition":definition()})),
                &[]
            )
            .is_err());
    }
    #[test]
    fn secret_values_never_enter_metadata_tables_or_public_views() {
        let mut store = store();
        for secret in ["quoted\"secret", "backslash\\secret", "newline\nsecret"] {
            let mut def = definition();
            def.name = secret.into();
            assert!(store
                .provider_command(
                    &command(json!({"type":"save_provider","definition":def})),
                    &[secret.into()]
                )
                .is_err());
        }
        let mut def = definition();
        def.model = "sk-123456789012345678901234567890".into();
        assert!(store
            .provider_command(
                &command(json!({"type":"save_provider","definition":def})),
                &[]
            )
            .is_err());
        let p = profile(&mut store);
        let id = p["provider_id"].as_str().unwrap();
        let op = install(&mut store, id);
        let all = [
            "named_providers",
            "provider_commands",
            "credential_operations",
        ]
        .iter()
        .map(|table| {
            let sql = if *table == "provider_commands" {
                "SELECT receipt FROM provider_commands".into()
            } else {
                format!("SELECT body FROM {table}")
            };
            let mut stmt = store.db.prepare(&sql).unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(Result::unwrap).collect::<Vec<_>>().join("")
        })
        .collect::<Vec<_>>()
        .join("");
        assert!(!all.contains("provider-fixture-secret-never-in-db"));
        let public = json!(store.named_providers().unwrap()).to_string();
        assert!(!public.contains("self-agent.vault"));
        assert!(!public.contains("credential_ref"));
        assert!(public.contains(&op));
    }
    #[test]
    fn set_is_one_shot_metadata_idempotent_and_rotation_revokes_tasks() {
        let mut store = store();
        let p = profile(&mut store);
        let p = enable(&mut store, &p, true);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let record = store.provider_record(&id).unwrap();
        let material = fresh(&record);
        let op = new_id();
        let first = store
            .begin_provider_credential_set(&op, &id, record.revision, material.reference())
            .unwrap();
        assert_eq!(first["replayed"], false);
        let changed_secret_material = fresh(&record);
        let replay = store
            .begin_provider_credential_set(
                &op,
                &id,
                record.revision,
                changed_secret_material.reference(),
            )
            .unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["no_secret_applied"], true);
        assert!(store
            .begin_provider_credential_set(&op, &id, record.revision + 1, material.reference())
            .is_err());
        assert!(!store
            .finish_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap());
        let work = store.claim_credential_operation(&op).unwrap().unwrap();
        assert_eq!(work.reference.as_str(), material.reference().as_str());
        assert!(store.claim_credential_operation(&op).unwrap().is_none());
        assert!(store
            .finish_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap());
        assert!(!store
            .finish_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap());
        let task_id = fixture_task(&mut store, &id);
        store.check_task_provider(&task_id).unwrap();
        install(&mut store, &id);
        assert!(store.check_task_provider(&task_id).is_err());
        let result = store.named_providers().unwrap();
        assert_eq!(result.providers[0].orphaned_credentials, 1);
        assert!(result.credential_operations.iter().any(|op| op.can_cleanup));
        assert!(!json!(store.snapshot().unwrap())
            .to_string()
            .contains("self-agent.vault"));
    }
    #[test]
    fn set_cas_race_or_unknown_retains_old_pointer_and_tracks_new_orphan() {
        let mut store = store();
        let p = profile(&mut store);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        install(&mut store, &id);
        let record = store.provider_record(&id).unwrap();
        let old = record.credential_ref.as_ref().unwrap().as_str().to_owned();
        let material = fresh(&record);
        let op = new_id();
        store
            .begin_provider_credential_set(&op, &id, record.revision, material.reference())
            .unwrap();
        store.claim_credential_operation(&op).unwrap();
        let mut changed = record.definition.definition().clone();
        changed.name = "改名但不换key".into();
        send(
            &mut store,
            json!({"type":"save_provider","id":id,"expected_revision":record.revision,"definition":changed}),
        );
        store
            .finish_credential_operation(&op, CredentialOutcome::Succeeded)
            .unwrap();
        assert_eq!(store.credential_operation(&op).unwrap().status, "orphaned");
        assert!(store.credential_operation(&op).unwrap().can_cleanup);
        assert_eq!(
            store
                .provider_record(&id)
                .unwrap()
                .credential_ref
                .unwrap()
                .as_str(),
            old
        );
        let record = store.provider_record(&id).unwrap();
        let material = fresh(&record);
        let op = new_id();
        store
            .begin_provider_credential_set(&op, &id, record.revision, material.reference())
            .unwrap();
        store.claim_credential_operation(&op).unwrap();
        store
            .finish_credential_operation(&op, CredentialOutcome::Unknown)
            .unwrap();
        assert_eq!(
            store
                .provider_record(&id)
                .unwrap()
                .credential_ref
                .unwrap()
                .as_str(),
            old
        );
        assert!(store.credential_operation(&op).unwrap().can_cleanup);
    }
    #[test]
    fn endpoint_changes_detach_and_cleanup_cannot_delete_a_current_reference() {
        let mut store = store();
        let p = profile(&mut store);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let initial = install(&mut store, &id);
        // Corruption/integration misuse cannot turn a currently referenced key
        // into an OS delete, even if an orphan flag were set accidentally.
        let mut operation = get(&store.db, "credential_operations", &initial)
            .unwrap()
            .unwrap();
        operation["orphan"] = json!(true);
        operation["can_cleanup"] = json!(true);
        put(&store.db, "credential_operations", &operation).unwrap();
        assert!(store.begin_credential_cleanup(&new_id(), &initial).is_err());
        operation["orphan"] = json!(false);
        operation["can_cleanup"] = json!(false);
        put(&store.db, "credential_operations", &operation).unwrap();
        for endpoint_change in [false, true] {
            let record = store.provider_record(&id).unwrap();
            let mut def = record.definition.definition().clone();
            if endpoint_change {
                def.models_endpoint = "/different-tenant/models".into();
            } else {
                def.base_url = "https://other.invalid/v2".into();
                def.models_endpoint = String::new();
            }
            send(
                &mut store,
                json!({"type":"save_provider","id":id,"expected_revision":record.revision,"definition":def}),
            );
            assert!(store.provider_record(&id).unwrap().credential_ref.is_none());
            let target = store
                .named_providers()
                .unwrap()
                .credential_operations
                .into_iter()
                .find(|op| op.can_cleanup)
                .unwrap();
            let cleanup = new_id();
            store
                .begin_credential_cleanup(&cleanup, &target.id)
                .unwrap();
            let work = store.claim_credential_operation(&cleanup).unwrap().unwrap();
            assert!(matches!(work.kind, CredentialWorkKind::Delete));
            store
                .finish_credential_operation(&cleanup, CredentialOutcome::Succeeded)
                .unwrap();
            assert!(!store.credential_operation(&target.id).unwrap().can_cleanup);
            if !endpoint_change {
                install(&mut store, &id);
            }
        }
    }
    #[test]
    fn delete_detaches_before_os_and_restart_never_replays_intents() {
        let path = std::env::temp_dir().join(format!("provider-store-{}.db", new_id()));
        let id;
        let op;
        let pending_set;
        {
            let mut store = Store::open(&path).unwrap();
            let p = profile(&mut store);
            id = p["provider_id"].as_str().unwrap().to_owned();
            install(&mut store, &id);
            let record = store.provider_record(&id).unwrap();
            op = new_id();
            store
                .begin_provider_credential_delete(&op, &id, record.revision)
                .unwrap();
            assert!(store.provider_record(&id).unwrap().credential_ref.is_none());
            assert_eq!(
                store.provider_record(&id).unwrap().revision,
                record.revision + 1
            );
            store.claim_credential_operation(&op).unwrap();
            let other = profile(&mut store);
            let other = store
                .provider_record(other["provider_id"].as_str().unwrap())
                .unwrap();
            pending_set = new_id();
            store
                .begin_provider_credential_set(
                    &pending_set,
                    &other.id,
                    other.revision,
                    fresh(&other).reference(),
                )
                .unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        for operation in [&op, &pending_set] {
            assert_eq!(
                store.credential_operation(operation).unwrap().status,
                "unknown"
            );
            assert!(store.credential_operation(operation).unwrap().can_cleanup);
            assert!(store
                .claim_credential_operation(operation)
                .unwrap()
                .is_none());
            assert!(!store
                .finish_credential_operation(operation, CredentialOutcome::Succeeded)
                .unwrap());
        }
        assert!(store.provider_record(&id).unwrap().credential_ref.is_none());
        drop(store);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn provider_selection_has_no_legacy_secret_fallback_and_legacy_tasks_still_work() {
        let mut store = store();
        let p = profile(&mut store);
        let p = enable(&mut store, &p, true);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let mut settings = Settings {
            provider_source: "named".into(),
            provider_id: Some(id.clone()),
            model: "wrong-legacy-model".into(),
            api_key: "legacy-env-secret".into(),
            ..Settings::default()
        };
        settings.provider_headers.insert(
            "x-secret",
            reqwest::header::HeaderValue::from_static("legacy-header"),
        );
        let mut agent = AgentProfile::default();
        let metadata = effective_settings(&store.db, &settings, &mut agent).unwrap();
        assert_eq!(metadata.model, "fixture-model");
        assert!(metadata.api_key.is_empty() && metadata.provider_headers.is_empty());
        assert_eq!(agent.provider_id.as_deref(), Some(id.as_str()));
        agent.model = "role-model".into();
        assert_eq!(
            effective_settings(&store.db, &settings, &mut agent)
                .unwrap()
                .model,
            "role-model"
        );
        agent.executor = "codex".into();
        assert!(effective_settings(&store.db, &settings, &mut agent).is_err());
        agent.provider_id = None;
        assert!(effective_settings(&store.db, &settings, &mut agent).is_ok());
        let task_id = fixture_task(&mut store, &id);
        let pinned = store.task_provider(&task_id).unwrap().unwrap();
        assert_eq!(pinned.id(), id);
        assert_eq!(pinned.model(), "fixture-model");
        store
            .db
            .execute("DELETE FROM task_providers WHERE id=?1", [&task_id])
            .unwrap();
        assert!(store.task_provider(&task_id).is_err());
        let legacy = new_id();
        put(&store.db, "tasks", &json!({"id":legacy})).unwrap();
        assert!(store.task_provider(&legacy).unwrap().is_none());
        store.check_task_provider(&legacy).unwrap();
    }
    #[test]
    fn denied_set_has_no_orphan_but_delete_failure_remains_explicitly_cleanable() {
        let mut store = store();
        let p = profile(&mut store);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let record = store.provider_record(&id).unwrap();
        let material = fresh(&record);
        let op = new_id();
        store
            .begin_provider_credential_set(&op, &id, record.revision, material.reference())
            .unwrap();
        store.claim_credential_operation(&op).unwrap();
        store
            .finish_credential_operation(&op, CredentialOutcome::Rejected)
            .unwrap();
        assert!(!store.credential_operation(&op).unwrap().can_cleanup);
        assert!(store.provider_record(&id).unwrap().credential_ref.is_none());
        install(&mut store, &id);
        let record = store.provider_record(&id).unwrap();
        let op = new_id();
        store
            .begin_provider_credential_delete(&op, &id, record.revision)
            .unwrap();
        store.claim_credential_operation(&op).unwrap();
        store
            .finish_credential_operation(&op, CredentialOutcome::Rejected)
            .unwrap();
        assert!(store.credential_operation(&op).unwrap().can_cleanup);
        assert!(store.provider_record(&id).unwrap().credential_ref.is_none());
    }
    #[test]
    fn capacity_preserves_all_unresolved_records_and_reserves_pending_finalization() {
        let store = store();
        for _ in 0..MAX_ORPHANS {
            put(
                &store.db,
                "credential_operations",
                &json!({"id":new_id(),"orphan":true}),
            )
            .unwrap();
        }
        assert!(capacity(&store.db, 1, 1).is_err());
        assert_eq!(operations(&store.db).unwrap().len(), MAX_ORPHANS);
        store
            .db
            .execute("DELETE FROM credential_operations", [])
            .unwrap();
        for _ in 0..MAX_ORPHANS {
            put(
                &store.db,
                "credential_operations",
                &json!({"id":new_id(),"kind":"delete","status":"pending"}),
            )
            .unwrap();
        }
        assert!(capacity(&store.db, 1, 1).is_err());
    }
    #[test]
    fn set_replay_is_available_after_provider_deleted_without_loading_secret() {
        let mut store = store();
        let p = profile(&mut store);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let record = store.provider_record(&id).unwrap();
        let material = fresh(&record);
        let op = new_id();
        store
            .begin_provider_credential_set(&op, &id, record.revision, material.reference())
            .unwrap();
        send(
            &mut store,
            json!({"type":"delete_provider","id":id,"expected_revision":record.revision}),
        );
        let replay = store
            .existing_provider_credential_set(&op, &id, record.revision)
            .unwrap()
            .unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["no_secret_applied"], true);
        assert!(store
            .existing_provider_credential_set(&op, &new_id(), record.revision)
            .is_err());
        assert!(store
            .existing_provider_credential_set(&op, &id, record.revision + 1)
            .is_err());
        assert!(store.claim_credential_operation(&op).unwrap().is_none());
        assert_eq!(store.credential_operation(&op).unwrap().status, "rejected");
        assert!(!store.credential_operation(&op).unwrap().can_cleanup);
    }
    #[test]
    fn cleanup_tail_survives_normal_operation_ceiling_without_forgetting_audit() {
        let store = store();
        for _ in 0..MAX_OPERATIONS {
            put(
                &store.db,
                "credential_operations",
                &json!({"id":new_id(),"status":"completed"}),
            )
            .unwrap();
        }
        assert!(capacity(&store.db, 1, 0).is_err());
        cleanup_capacity(&store.db).unwrap();
        for _ in 0..MAX_CLEANUP_OPERATIONS {
            put(
                &store.db,
                "credential_operations",
                &json!({"id":new_id(),"target_operation_id":new_id(),"status":"unknown"}),
            )
            .unwrap();
        }
        assert!(cleanup_capacity(&store.db).is_err());
        assert_eq!(
            operations(&store.db).unwrap().len(),
            MAX_OPERATIONS + MAX_CLEANUP_OPERATIONS
        );
    }
    #[test]
    fn native_main_worker_and_plan_enqueue_pin_effective_provider() {
        let mut store = store();
        let p = profile(&mut store);
        let p = enable(&mut store, &p, true);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let settings = Settings {
            provider_source: "named".into(),
            provider_id: Some(id.clone()),
            base_url: "not-the-legacy-host".into(),
            model: "old-global-model-must-not-override-named".into(),
            ..Settings::default()
        };
        let source =
            json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"provider-test"});
        let envelope = |command| json!({"command_id":new_id(),"source":source,"command":command});
        let session=store.command(envelope(json!({"type":"create_session","title":"fixture","project_path":std::env::temp_dir()})),&settings).unwrap().0["session_id"].as_str().unwrap().to_owned();
        for command in [
            json!({"type":"run_agent","session_id":session,"content":"main fixture"}),
            json!({"type":"start_task","session_id":session,"title":"worker fixture"}),
        ] {
            let receipt = store.command(envelope(command), &settings).unwrap().0;
            let task_id = receipt["task_id"].as_str().unwrap();
            let pinned = store.task_provider(task_id).unwrap().unwrap();
            assert_eq!(pinned.id(), id);
            assert_eq!(pinned.model(), "fixture-model");
        }
        let roles = super::super::snapshot_agents(&store.db, &settings).unwrap();
        assert_eq!(roles[0].provider_id.as_deref(), Some(id.as_str()));
        let mut plan_task =
            worker_task(&session, "plan child", "native", &source, None, None, None);
        plan_task["agent"] = json!(roles[0]);
        snapshot_task_provider(&store.db, &mut plan_task).unwrap();
        put(&store.db, "tasks", &plan_task).unwrap();
        assert_eq!(
            store
                .task_provider(plan_task["id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .id(),
            id
        );
        let legacy = Settings {
            provider_source: "manual".into(),
            base_url: "https://example.invalid/v1".into(),
            model: String::new(),
            ..Settings::default()
        };
        let mut role = AgentProfile {
            model: "only-role-model".into(),
            ..AgentProfile::default()
        };
        assert!(effective_settings(&store.db, &legacy, &mut role)
            .unwrap()
            .configured());
    }
    #[test]
    fn coordinator_message_draft_change_review_pin_one_immutable_provider() {
        let mut store = store();
        let p = profile(&mut store);
        let p = enable(&mut store, &p, true);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let settings = Settings {
            provider_source: "named".into(),
            provider_id: Some(id.clone()),
            model: "ignored-legacy-model".into(),
            ..Settings::default()
        };
        let mut message = json!({"id":new_id(),"kind":"message"});
        snapshot_coordinator_provider(&store.db, &mut message, &settings).unwrap();
        put(&store.db, "coordinator_jobs", &message).unwrap();
        assert_eq!(
            store
                .coordinator_provider(message["id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .model(),
            "fixture-model"
        );
        let mut plan = json!({"id":new_id()});
        inherit_plan_provider(&store.db, &message, &mut plan).unwrap();
        put(&store.db, "plans", &plan).unwrap();
        let record = store.provider_record(&id).unwrap();
        let mut changed = record.definition.definition().clone();
        changed.name = "after-enqueue".into();
        send(
            &mut store,
            json!({"type":"save_provider","id":id,"expected_revision":record.revision,"definition":changed}),
        );
        for kind in ["change", "review"] {
            let mut job = json!({"id":new_id(),"kind":kind});
            inherit_coordinator_provider(&store.db, &plan, &mut job).unwrap();
            put(&store.db, "coordinator_jobs", &job).unwrap();
            let job_id = job["id"].as_str().unwrap();
            assert_eq!(
                store
                    .coordinator_provider(job_id)
                    .unwrap()
                    .unwrap()
                    .revision(),
                record.revision
            );
            assert!(store.check_coordinator_provider(job_id).is_err());
        }
        assert!(store
            .check_coordinator_provider(message["id"].as_str().unwrap())
            .is_err());
        let legacy = json!({"id":new_id(),"kind":"message"});
        put(&store.db, "coordinator_jobs", &legacy).unwrap();
        assert!(store
            .coordinator_provider(legacy["id"].as_str().unwrap())
            .unwrap()
            .is_none());
        store
            .db
            .execute(
                "DELETE FROM coordinator_providers WHERE id=?1",
                [message["id"].as_str().unwrap()],
            )
            .unwrap();
        assert!(store
            .coordinator_provider(message["id"].as_str().unwrap())
            .is_err());
    }
    #[test]
    fn actual_coordinator_enqueue_paths_keep_original_plan_binding_after_edit() {
        let mut store = store();
        let p = profile(&mut store);
        let p = enable(&mut store, &p, true);
        let id = p["provider_id"].as_str().unwrap().to_owned();
        let settings = Settings {
            provider_source: "named".into(),
            provider_id: Some(id.clone()),
            model: "legacy-model-must-not-win".into(),
            ..Settings::default()
        };
        let source = json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"coordinator-provider"});
        let envelope = |command| json!({"command_id":new_id(),"source":source,"command":command});
        store.command(envelope(json!({"type":"create_session","title":"fixture","project_path":std::env::temp_dir()})),&settings).unwrap();
        let receipt = store
            .command(
                envelope(json!({"type":"send_message","content":"create a fixture plan"})),
                &settings,
            )
            .unwrap()
            .0;
        let message = store.claim_coordinator_job().unwrap().unwrap();
        assert_eq!(message["id"], receipt["job_id"]);
        assert_eq!(
            store
                .coordinator_provider(message["id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .model(),
            "fixture-model"
        );
        store.complete_coordinator_job(message["id"].as_str().unwrap(),Ok(json!({"action":"plan","message":"fixture only","title":"fixture plan","nodes":[{"id":"one","title":"fixture child","objective":"fixture only","depends_on":[]}]}))).unwrap();
        let plan = rows(&store.db, "plans").unwrap().remove(0);
        let change=store.command(envelope(json!({"type":"request_plan_change","plan_id":plan["id"],"expected_revision":plan["revision"],"content":"fixture change"})),&settings).unwrap().0;
        assert_eq!(
            store
                .coordinator_provider(change["job_id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .revision(),
            p["revision"].as_u64().unwrap()
        );
        let change_job = store.claim_coordinator_job().unwrap().unwrap();
        assert_eq!(change_job["kind"], "change");
        store
            .complete_coordinator_job(
                change_job["id"].as_str().unwrap(),
                Err(anyhow!("fixture cancellation")),
            )
            .unwrap();
        store.command(envelope(json!({"type":"start_plan","plan_id":plan["id"],"expected_revision":plan["revision"]})),&settings).unwrap();
        let child = store.claim_task().unwrap().unwrap();
        assert_eq!(
            store
                .task_provider(child["id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .id(),
            id
        );
        let record = store.provider_record(&id).unwrap();
        let mut def = record.definition.definition().clone();
        def.name = "edited while child finishing".into();
        send(
            &mut store,
            json!({"type":"save_provider","id":id,"expected_revision":record.revision,"definition":def}),
        );
        store
            .finish(
                child["id"].as_str().unwrap(),
                Ok("fixture child finished without new request".into()),
            )
            .unwrap();
        let review = store.claim_coordinator_job().unwrap().unwrap();
        assert_eq!(review["kind"], "review");
        assert_eq!(
            store
                .coordinator_provider(review["id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .revision(),
            record.revision
        );
        assert!(store
            .check_coordinator_provider(review["id"].as_str().unwrap())
            .is_err());
        assert_eq!(
            super::super::record(&store.db, "tasks", child["id"].as_str().unwrap()).unwrap()
                ["status"],
            "completed"
        );
        store
            .complete_coordinator_job(
                review["id"].as_str().unwrap(),
                Ok(json!({"action":"complete","message":"stale model response"})),
            )
            .unwrap();
        assert_eq!(
            super::super::record(
                &store.db,
                "coordinator_jobs",
                review["id"].as_str().unwrap()
            )
            .unwrap()["status"],
            "failed"
        );
        assert_eq!(
            super::super::record(&store.db, "plans", plan["id"].as_str().unwrap()).unwrap()
                ["status"],
            "waiting_input"
        );
    }
    #[test]
    fn downstream_revoked_provider_blocks_node_without_rolling_back_completed_predecessor() {
        for mode in ["disabled", "endpoint", "rotated"] {
            let mut store = store();
            let p = profile(&mut store);
            let p = enable(&mut store, &p, true);
            let id = p["provider_id"].as_str().unwrap().to_owned();
            let settings = Settings {
                provider_source: "named".into(),
                provider_id: Some(id.clone()),
                ..Settings::default()
            };
            let source = json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"downstream-provider"});
            let envelope =
                |command| json!({"command_id":new_id(),"source":source,"command":command});
            store.command(envelope(json!({"type":"create_session","title":"fixture","project_path":std::env::temp_dir()})),&settings).unwrap();
            store
                .command(
                    envelope(json!({"type":"send_message","content":"two sequential tasks"})),
                    &settings,
                )
                .unwrap();
            let message = store.claim_coordinator_job().unwrap().unwrap();
            store.complete_coordinator_job(message["id"].as_str().unwrap(),Ok(json!({"action":"plan","message":"fixture","title":"fixture","nodes":[{"id":"one","title":"one","objective":"one","depends_on":[]},{"id":"two","title":"two","objective":"two","depends_on":["one"]}]}))).unwrap();
            let plan = rows(&store.db, "plans").unwrap().remove(0);
            store.command(envelope(json!({"type":"start_plan","plan_id":plan["id"],"expected_revision":plan["revision"]})),&settings).unwrap();
            let first = store.claim_task().unwrap().unwrap();
            match mode {
                "disabled" => {
                    enable(&mut store, &p, false);
                }
                "endpoint" => {
                    let record = store.provider_record(&id).unwrap();
                    let mut definition = record.definition.definition().clone();
                    definition.base_url = "https://changed.invalid/v1".into();
                    definition.models_endpoint = String::new();
                    send(
                        &mut store,
                        json!({"type":"save_provider","id":id,"expected_revision":record.revision,"definition":definition}),
                    );
                }
                "rotated" => {
                    let record = store.provider_record(&id).unwrap();
                    let material = fresh(&record);
                    let op = new_id();
                    store
                        .begin_provider_credential_set(
                            &op,
                            &id,
                            record.revision,
                            material.reference(),
                        )
                        .unwrap();
                    store.claim_credential_operation(&op).unwrap();
                    store
                        .finish_credential_operation(&op, CredentialOutcome::Succeeded)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            store
                .finish(
                    first["id"].as_str().unwrap(),
                    Ok("completed before revocation was observed".into()),
                )
                .unwrap();
            let completed = record(&store.db, "tasks", first["id"].as_str().unwrap()).unwrap();
            assert_eq!(completed["status"], "completed");
            let plan = record(&store.db, "plans", plan["id"].as_str().unwrap()).unwrap();
            assert_eq!(plan["nodes"][0]["status"], "completed");
            assert_eq!(plan["nodes"][1]["status"], "blocked");
            assert!(plan["nodes"][1]["task_id"].is_null());
            assert_eq!(rows(&store.db, "task_providers").unwrap().len(), 1);
            assert_eq!(rows(&store.db, "task_capabilities").unwrap().len(), 1);
            let review = store.claim_coordinator_job().unwrap().unwrap();
            assert_eq!(review["kind"], "review");
            assert!(store
                .check_coordinator_provider(review["id"].as_str().unwrap())
                .is_err());
        }
    }
    #[test]
    fn coordinator_commit_fence_rejects_stale_message_and_change_responses() {
        for change in [false, true] {
            let mut store = store();
            let p = profile(&mut store);
            let p = enable(&mut store, &p, true);
            let id = p["provider_id"].as_str().unwrap().to_owned();
            let settings = Settings {
                provider_source: "named".into(),
                provider_id: Some(id.clone()),
                ..Settings::default()
            };
            let source =
                json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"fence"});
            let envelope =
                |command| json!({"command_id":new_id(),"source":source,"command":command});
            store.command(envelope(json!({"type":"create_session","title":"fixture","project_path":std::env::temp_dir()})),&settings).unwrap();
            store
                .command(
                    envelope(json!({"type":"send_message","content":"fixture"})),
                    &settings,
                )
                .unwrap();
            let mut job = store.claim_coordinator_job().unwrap().unwrap();
            let nodes = json!([{"id":"one","title":"one","objective":"one","depends_on":[]}]);
            if change {
                store.complete_coordinator_job(job["id"].as_str().unwrap(),Ok(json!({"action":"plan","title":"fixture","message":"fixture","nodes":nodes}))).unwrap();
                let plan = rows(&store.db, "plans").unwrap().remove(0);
                store.command(envelope(json!({"type":"request_plan_change","plan_id":plan["id"],"expected_revision":plan["revision"],"content":"change fixture"})),&settings).unwrap();
                job = store.claim_coordinator_job().unwrap().unwrap();
            }
            enable(&mut store, &p, false);
            let result = if change {
                json!({"action":"change","message":"stale","nodes":nodes})
            } else {
                json!({"action":"plan","message":"stale","title":"stale","nodes":nodes})
            };
            store
                .complete_coordinator_job(job["id"].as_str().unwrap(), Ok(result))
                .unwrap();
            assert_eq!(
                record(&store.db, "coordinator_jobs", job["id"].as_str().unwrap()).unwrap()
                    ["status"],
                "failed"
            );
            if change {
                assert_eq!(
                    rows(&store.db, "plan_changes").unwrap()[0]["status"],
                    "failed"
                );
            } else {
                assert!(rows(&store.db, "plans").unwrap().is_empty());
            }
        }
    }
    #[test]
    fn successful_orphan_cleanup_does_not_rewrite_unknown_set_outcome() {
        let mut store = store();
        let p = profile(&mut store);
        let id = p["provider_id"].as_str().unwrap();
        let record = store.provider_record(id).unwrap();
        let material = fresh(&record);
        let op = new_id();
        store
            .begin_provider_credential_set(&op, id, record.revision, material.reference())
            .unwrap();
        store.claim_credential_operation(&op).unwrap();
        store
            .finish_credential_operation(&op, CredentialOutcome::Unknown)
            .unwrap();
        let cleanup = new_id();
        store.begin_credential_cleanup(&cleanup, &op).unwrap();
        store.claim_credential_operation(&cleanup).unwrap();
        store
            .finish_credential_operation(&cleanup, CredentialOutcome::Succeeded)
            .unwrap();
        let original = store.credential_operation(&op).unwrap();
        assert_eq!(original.status, "unknown");
        assert_eq!(original.reason.as_deref(), Some("vault_outcome_unknown"));
        assert_eq!(original.cleanup_status.as_deref(), Some("completed"));
        assert!(!original.can_cleanup);
        let replay = store
            .existing_provider_credential_set(&op, id, record.revision)
            .unwrap()
            .unwrap();
        assert_eq!(replay["no_secret_applied"], true);
        assert_eq!(replay["operation"]["status"], "unknown");
    }
}
