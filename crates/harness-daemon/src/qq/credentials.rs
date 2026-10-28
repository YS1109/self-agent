//! QQ-only one-shot vault intents. No secret material or secret fingerprint is
//! accepted. Caller commits claim before OS work, then finalizes in a fresh tx.
//! Every error requires whole-transaction rollback. Recovery never replays OS work.
use super::configuration::{self, Config};
use anyhow::{bail, Context, Result};
use harness_core::CredentialOperationView;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
const NORMAL_LIMIT: usize = 128;
const ORPHAN_LIMIT: usize = 64;
const CLEANUP_LIMIT: usize = 128;
const MAX_REV: u64 = 9_007_199_254_740_991;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkKind {
    Set,
    Delete,
}
pub(crate) struct Work {
    #[cfg(test)]
    pub operation_id: String,
    pub config_id: String,
    #[cfg(test)]
    pub app_id: String,
    pub binding_digest: String,
    pub reference: String,
    pub kind: WorkKind,
}
#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Succeeded,
    NotApplied,
    Unknown,
}
#[derive(Clone, Serialize, Deserialize)]
struct Operation {
    id: String,
    owner_id: String,
    app_id: String,
    expected_revision: u64,
    kind: String,
    status: String,
    reason: Option<String>,
    reference: String,
    metadata: Value,
    orphan: bool,
    can_cleanup: bool,
    target: Option<String>,
    cleanup_operation: Option<String>,
    cleanup_status: Option<String>,
    created_at: String,
    updated_at: String,
}
fn uuid(id: &str) -> Result<()> {
    if !uuid::Uuid::parse_str(id).is_ok_and(|v| v.to_string() == id) {
        bail!("QQ 凭据操作标识必须为规范 UUID");
    }
    Ok(())
}
fn revision(rev: u64) -> Result<()> {
    if rev >= MAX_REV {
        bail!("QQ 配置修订已达到凭据操作上限");
    }
    Ok(())
}
pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS qq_credential_operations(id TEXT PRIMARY KEY,body TEXT NOT NULL);")?;
    Ok(())
}
fn get(db: &Connection, id: &str) -> Result<Option<Operation>> {
    uuid(id)?;
    let raw: Option<String> = db
        .query_row(
            "SELECT body FROM qq_credential_operations WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
fn all(db: &Connection) -> Result<Vec<Operation>> {
    let mut stmt = db.prepare("SELECT body FROM qq_credential_operations ORDER BY rowid")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|v| Ok(serde_json::from_str(&v?)?)).collect()
}
fn put(tx: &Transaction<'_>, op: &Operation) -> Result<()> {
    tx.execute("INSERT INTO qq_credential_operations VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![op.id,serde_json::to_string(op)?])?;
    Ok(())
}
fn view(op: &Operation) -> CredentialOperationView {
    CredentialOperationView {
        id: op.id.clone(),
        owner_kind: "qq".into(),
        owner_id: op.owner_id.clone(),
        kind: op.kind.clone(),
        status: op.status.clone(),
        reason: op.reason.clone(),
        can_cleanup: op.can_cleanup,
        cleanup_status: op.cleanup_status.clone(),
        created_at: op.created_at.clone(),
        updated_at: op.updated_at.clone(),
    }
}
pub(crate) fn operation(db: &Connection, id: &str) -> Result<CredentialOperationView> {
    Ok(view(&get(db, id)?.context("QQ 凭据操作不存在")?))
}
pub(crate) fn operations(db: &Connection) -> Result<Vec<CredentialOperationView>> {
    Ok(all(db)?.iter().map(view).collect())
}
fn receipt(op: &Operation, replayed: bool) -> Value {
    json!({"operation":view(op),"replayed":replayed,"no_secret_applied":replayed})
}
fn metadata(kind: &str, config: &str, rev: u64) -> Result<Value> {
    uuid(config)?;
    revision(rev)?;
    Ok(json!({"kind":kind,"owner_id":config,"expected_revision":rev}))
}
fn existing(db: &Connection, id: &str, metadata: &Value) -> Result<Option<Value>> {
    get(db, id)?
        .map(|op| {
            if op.metadata != *metadata {
                bail!("QQ 凭据操作标识已用于不同元数据");
            }
            Ok(receipt(&op, true))
        })
        .transpose()
}
pub(crate) fn existing_set(
    db: &Connection,
    id: &str,
    config: &str,
    rev: u64,
) -> Result<Option<Value>> {
    existing(db, id, &metadata("set", config, rev)?)
}
fn active(db: &Connection, config: &str) -> Result<()> {
    if all(db)?
        .iter()
        .any(|o| o.owner_id == config && matches!(o.status.as_str(), "pending" | "inflight"))
    {
        bail!("QQ 配置已有未完成凭据操作");
    }
    Ok(())
}
fn capacity(db: &Connection, slots: usize) -> Result<()> {
    let ops = all(db)?;
    let normal = ops.iter().filter(|o| o.target.is_none()).count();
    let reserved = ops
        .iter()
        .filter(|o| o.kind == "set" && matches!(o.status.as_str(), "pending" | "inflight"))
        .count();
    let orphan = ops.iter().filter(|o| o.orphan).count();
    let reserved_orphans = ops
        .iter()
        .filter(|o| o.target.is_none() && matches!(o.status.as_str(), "pending" | "inflight"))
        .count();
    if normal + reserved + slots > NORMAL_LIMIT || orphan + reserved_orphans + 1 > ORPHAN_LIMIT {
        bail!("QQ 凭据审计或待清理容量已满，未遗忘凭据");
    }
    Ok(())
}
fn bound(db: &Connection, reference: &str) -> Result<bool> {
    Ok(configuration::current(db)?.is_some_and(|c| c.credential_ref() == Some(reference)))
}
fn new(id: &str, c: &Config, kind: &str, reference: &str, metadata: Value) -> Operation {
    let now = harness_core::now();
    Operation {
        id: id.into(),
        owner_id: c.id.clone(),
        app_id: c.app_id.clone(),
        expected_revision: c.revision,
        kind: kind.into(),
        status: "pending".into(),
        reason: None,
        reference: reference.into(),
        metadata,
        orphan: false,
        can_cleanup: false,
        target: None,
        cleanup_operation: None,
        cleanup_status: None,
        created_at: now.clone(),
        updated_at: now,
    }
}
fn validate_ref(
    c: &Config,
    reference: &str,
    validate: impl FnOnce(&str, &str, &str) -> Result<()>,
) -> Result<()> {
    if reference.is_empty() || reference.len() > 512 || reference.chars().any(char::is_control) {
        bail!("QQ 凭据引用格式无效");
    }
    validate(
        &c.id,
        &configuration::credential_binding_digest(&c.app_id)?,
        reference,
    )
}
pub(crate) fn begin_set(
    tx: &Transaction<'_>,
    id: &str,
    config: &str,
    rev: u64,
    reference: &str,
    validate: impl FnOnce(&str, &str, &str) -> Result<()>,
) -> Result<Value> {
    let meta = metadata("set", config, rev)?;
    if let Some(r) = existing(tx, id, &meta)? {
        return Ok(r);
    }
    let c = configuration::load(tx, config, rev)?;
    active(tx, config)?;
    capacity(tx, 2)?;
    validate_ref(&c, reference, validate)?;
    if bound(tx, reference)? || all(tx)?.iter().any(|o| o.reference == reference) {
        bail!("QQ 凭据引用必须全新，不能重用历史引用");
    }
    let op = new(id, &c, "set", reference, meta);
    put(tx, &op)?;
    Ok(receipt(&op, false))
}
pub(crate) fn begin_delete(
    tx: &Transaction<'_>,
    id: &str,
    config: &str,
    rev: u64,
    now: u64,
    validate: impl FnOnce(&str, &str, &str) -> Result<()>,
) -> Result<Value> {
    let meta = metadata("delete", config, rev)?;
    if let Some(r) = existing(tx, id, &meta)? {
        return Ok(r);
    }
    let c = configuration::load(tx, config, rev)?;
    active(tx, config)?;
    capacity(tx, 1)?;
    let reference = c.credential_ref().context("QQ 尚未绑定凭据")?;
    validate_ref(&c, reference, validate)?;
    let op = new(id, &c, "delete", reference, meta);
    configuration::disconnect_credential(tx, config, rev, now)?;
    put(tx, &op)?;
    Ok(receipt(&op, false))
}
pub(crate) fn begin_cleanup(tx: &Transaction<'_>, id: &str, target_id: &str) -> Result<Value> {
    uuid(target_id)?;
    let meta = json!({"kind":"cleanup","target":target_id});
    if let Some(r) = existing(tx, id, &meta)? {
        return Ok(r);
    }
    let mut target = get(tx, target_id)?.context("QQ 待清理记录不存在")?;
    if !target.can_cleanup || !target.orphan || target.target.is_some() {
        bail!("QQ 此凭据操作不可清理");
    }
    if bound(tx, &target.reference)? {
        bail!("QQ 凭据仍被绑定，不能清理");
    }
    active(tx, &target.owner_id)?;
    if all(tx)?.iter().filter(|o| o.target.is_some()).count() >= CLEANUP_LIMIT {
        bail!("QQ 显式清理审计容量已满，未自动重试或遗忘引用");
    }
    let mut op = target.clone();
    op.id = id.into();
    op.kind = "cleanup".into();
    op.status = "pending".into();
    op.reason = None;
    op.metadata = meta;
    op.orphan = false;
    op.can_cleanup = false;
    op.target = Some(target_id.into());
    op.cleanup_operation = None;
    op.cleanup_status = None;
    op.created_at = harness_core::now();
    op.updated_at = op.created_at.clone();
    target.can_cleanup = false;
    target.cleanup_operation = Some(id.into());
    target.cleanup_status = Some("pending".into());
    target.updated_at = op.created_at.clone();
    put(tx, &target)?;
    put(tx, &op)?;
    Ok(receipt(&op, false))
}
pub(crate) fn claim(
    tx: &Transaction<'_>,
    id: &str,
    validate: impl FnOnce(&str, &str, &str) -> Result<()>,
) -> Result<Option<Work>> {
    let Some(mut op) = get(tx, id)? else {
        return Ok(None);
    };
    if op.status != "pending" {
        return Ok(None);
    }
    if op.kind == "set" {
        let current = configuration::current(tx)?;
        if current.is_none_or(|c| {
            c.id != op.owner_id || c.revision != op.expected_revision || c.app_id != op.app_id
        }) {
            op.status = "rejected".into();
            op.reason = Some("config_changed_before_vault".into());
            op.updated_at = harness_core::now();
            put(tx, &op)?;
            return Ok(None);
        }
    }
    let digest = configuration::credential_binding_digest(&op.app_id)?;
    validate(&op.owner_id, &digest, &op.reference)?;
    let kind = if op.kind == "set" {
        WorkKind::Set
    } else {
        WorkKind::Delete
    };
    if kind == WorkKind::Delete && bound(tx, &op.reference)? {
        bail!("QQ 凭据仍被绑定，不能删除");
    }
    op.status = "inflight".into();
    op.updated_at = harness_core::now();
    put(tx, &op)?;
    Ok(Some(Work {
        #[cfg(test)]
        operation_id: op.id,
        config_id: op.owner_id,
        #[cfg(test)]
        app_id: op.app_id,
        binding_digest: digest,
        reference: op.reference,
        kind,
    }))
}
pub(crate) fn finish(
    tx: &Transaction<'_>,
    id: &str,
    outcome: Outcome,
    validate: impl FnOnce(&str, &str, &str) -> Result<()>,
) -> Result<bool> {
    let Some(mut op) = get(tx, id)? else {
        return Ok(false);
    };
    if op.status != "inflight" {
        return Ok(false);
    }
    let success = matches!(outcome, Outcome::Succeeded);
    let unknown = matches!(outcome, Outcome::Unknown);
    op.status = if success {
        "completed"
    } else if unknown {
        "unknown"
    } else {
        "rejected"
    }
    .into();
    op.reason = if success {
        None
    } else {
        Some(
            if unknown {
                "vault_outcome_unknown"
            } else {
                "vault_not_applied"
            }
            .into(),
        )
    };
    if op.kind == "set" {
        if success {
            if let Some(c) = configuration::current(tx)?.filter(|c| {
                c.id == op.owner_id && c.revision == op.expected_revision && c.app_id == op.app_id
            }) {
                validate(
                    &c.id,
                    &configuration::credential_binding_digest(&c.app_id)?,
                    &op.reference,
                )?;
                if let Some(old) = c.credential_ref() {
                    let mut audit = new(
                        &uuid::Uuid::new_v4().to_string(),
                        &c,
                        "cleanup",
                        old,
                        Value::Null,
                    );
                    audit.status = "cleanup_required".into();
                    audit.reason = Some("credential_detached".into());
                    audit.orphan = true;
                    audit.can_cleanup = true;
                    put(tx, &audit)?;
                }
                configuration::attach_credential(tx, &c.id, c.revision, &op.reference, |_, _| {
                    Ok(())
                })?;
            } else {
                op.status = "orphaned".into();
                op.reason = Some("config_changed_after_vault".into());
                op.orphan = true;
                op.can_cleanup = true;
            }
        } else if unknown {
            op.orphan = true;
            op.can_cleanup = true;
        }
    } else if let Some(target_id) = &op.target {
        let mut target = get(tx, target_id)?.context("QQ 清理关联缺失")?;
        if target.cleanup_operation.as_deref() != Some(id) {
            bail!("QQ 清理关联已改变");
        }
        target.can_cleanup = !success;
        target.orphan = !success;
        target.cleanup_status = Some(op.status.clone());
        target.updated_at = harness_core::now();
        if success && target.status == "cleanup_required" {
            target.status = "cleaned".into();
            target.reason = Some("credential_deleted".into());
        }
        put(tx, &target)?;
    } else if !success {
        op.orphan = true;
        op.can_cleanup = true;
    }
    op.updated_at = harness_core::now();
    put(tx, &op)?;
    Ok(true)
}
pub(crate) fn recover(tx: &Transaction<'_>) -> Result<()> {
    for mut op in all(tx)? {
        if !matches!(op.status.as_str(), "pending" | "inflight") {
            continue;
        }
        op.status = "unknown".into();
        op.reason = Some("daemon_interrupted_no_replay".into());
        op.updated_at = harness_core::now();
        if let Some(target_id) = &op.target {
            let mut target = get(tx, target_id)?.context("QQ 清理关联缺失")?;
            if target.cleanup_operation.as_deref() != Some(&op.id) {
                bail!("QQ 清理关联已改变");
            }
            target.can_cleanup = true;
            target.cleanup_status = Some("unknown".into());
            target.updated_at = op.updated_at.clone();
            put(tx, &target)?;
        } else {
            op.orphan = true;
            op.can_cleanup = true;
        }
        put(tx, &op)?;
    }
    Ok(())
}
#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
