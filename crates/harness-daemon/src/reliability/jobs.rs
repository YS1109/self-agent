//! Authoritative backup intents. All writes require the caller's transaction.
//! Acceptance must commit before starting filesystem work; cancellation of the
//! HTTP waiter does not cancel the accepted intent. No automatic restart replay.
use super::storage::{BackupManifest, MAX_DATABASE_BYTES, SCHEMA_VERSION};
use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub(crate) const MAX_BACKUP_JOBS: usize = 256;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackupState {
    Pending,
    Running,
    Completed,
    Failed,
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackupReason {
    ExportFailed,
    Cancelled,
    Deadline,
    Restarted,
    OutcomeUncertain,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackupReceipt {
    pub id: String,
    pub state: BackupState,
    pub reason: Option<BackupReason>,
    pub manifest: Option<BackupManifest>,
}
/// Private worker input: deliberately not Serialize/Debug. Never expose paths
/// or the ownership token in receipts, events, diagnostics or remote responses.
pub(crate) struct ClaimedBackup {
    pub id: String,
    pub token: String,
    pub parent: PathBuf,
    pub name: String,
}
pub(crate) enum BackupOutcome {
    Completed(BackupManifest),
    Failed(BackupReason),
    Unknown,
}
fn safe<T>(value: Result<T>) -> Result<T> {
    value.map_err(|_| anyhow::anyhow!("备份操作记录无效、冲突或不可用；未扩大操作范围"))
}
fn canonical_id(id: &str) -> Result<()> {
    if Uuid::parse_str(id)?.to_string() != id {
        bail!("操作标识必须是规范 UUID");
    }
    Ok(())
}
fn fingerprint(id: &str, parent: &Path, name: &str) -> Result<(String, String)> {
    canonical_id(id)?;
    let parent = parent
        .to_str()
        .filter(|s| s.len() <= 4096 && !s.contains('\0'))
        .ok_or_else(|| anyhow::anyhow!("目录格式无效"))?;
    let path = Path::new(parent);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        bail!("目录必须是绝对路径");
    }
    if name.is_empty()
        || name.len() > 80
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        bail!("备份名称无效");
    }
    let bytes = serde_json::to_vec(&(1u8, id, parent, name))?;
    Ok((format!("{:x}", Sha256::digest(bytes)), parent.to_owned()))
}
/// Only call inside the fixed trusted schema migration/ledger transaction.
pub(crate) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    safe((|| {
        tx.execute_batch("CREATE TABLE backup_jobs(id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, parent TEXT NOT NULL, name TEXT NOT NULL, receipt TEXT NOT NULL, claim_token TEXT);")?;
        Ok(())
    })())
}
fn get_inner(db: &Connection, id: &str) -> Result<Option<BackupReceipt>> {
    canonical_id(id)?;
    let raw: Option<String> = db
        .query_row("SELECT receipt FROM backup_jobs WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    raw.map(|raw| {
        if raw.len() > 4096 {
            bail!("记录过大");
        }
        let receipt: BackupReceipt = serde_json::from_str(&raw)?;
        let valid = match receipt.state {
            BackupState::Pending | BackupState::Running => {
                receipt.reason.is_none() && receipt.manifest.is_none()
            }
            BackupState::Completed => {
                receipt.reason.is_none() && receipt.manifest.as_ref().is_some_and(valid_manifest)
            }
            BackupState::Failed => {
                receipt.manifest.is_none()
                    && matches!(
                        receipt.reason,
                        Some(
                            BackupReason::ExportFailed
                                | BackupReason::Cancelled
                                | BackupReason::Deadline
                        )
                    )
            }
            BackupState::Unknown => {
                receipt.manifest.is_none()
                    && matches!(
                        receipt.reason,
                        Some(BackupReason::Restarted | BackupReason::OutcomeUncertain)
                    )
            }
        };
        if receipt.id != id || !valid {
            bail!("操作记录不一致");
        }
        Ok(receipt)
    })
    .transpose()
}
pub(crate) fn get(db: &Connection, id: &str) -> Result<Option<BackupReceipt>> {
    safe((|| {
        canonical_id(id)?;
        get_inner(db, id)
    })())
}
pub(crate) fn list(db: &Connection) -> Result<Vec<BackupReceipt>> {
    safe((|| {
        let mut stmt = db.prepare("SELECT id FROM backup_jobs ORDER BY rowid LIMIT 257")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len() > MAX_BACKUP_JOBS {
            bail!("记录容量超限");
        }
        ids.iter()
            .map(|id| get_inner(db, id)?.ok_or_else(|| anyhow::anyhow!("记录缺失")))
            .collect()
    })())
}
pub(crate) fn accept(
    tx: &Transaction<'_>,
    id: &str,
    parent: &Path,
    name: &str,
) -> Result<BackupReceipt> {
    safe((|| {
        let (digest, parent) = fingerprint(id, parent, name)?;
        let prior: Option<String> = tx
            .query_row(
                "SELECT fingerprint FROM backup_jobs WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            if prior != digest {
                bail!("同一操作标识的参数发生变化");
            }
            return get_inner(tx, id)?.ok_or_else(|| anyhow::anyhow!("记录缺失"));
        }
        let count: usize = tx.query_row("SELECT count(*) FROM backup_jobs", [], |r| r.get(0))?;
        if count >= MAX_BACKUP_JOBS {
            bail!("备份记录已满；保留所有历史及未决操作");
        }
        let receipt = BackupReceipt {
            id: id.to_owned(),
            state: BackupState::Pending,
            reason: None,
            manifest: None,
        };
        tx.execute(
            "INSERT INTO backup_jobs(id,fingerprint,parent,name,receipt) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![id, digest, parent, name, serde_json::to_string(&receipt)?],
        )?;
        Ok(receipt)
    })())
}
pub(crate) fn claim(tx: &Transaction<'_>, id: &str) -> Result<Option<ClaimedBackup>> {
    safe((|| {
        canonical_id(id)?;
        let Some(mut receipt) = get_inner(tx, id)? else {
            return Ok(None);
        };
        if receipt.state != BackupState::Pending {
            return Ok(None);
        }
        let (parent, name, digest): (String, String, String) = tx.query_row(
            "SELECT parent,name,fingerprint FROM backup_jobs WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if fingerprint(id, Path::new(&parent), &name)?.0 != digest {
            bail!("私有参数校验失败");
        }
        let token = Uuid::new_v4().to_string();
        receipt.state = BackupState::Running;
        tx.execute(
            "UPDATE backup_jobs SET receipt=?2,claim_token=?3 WHERE id=?1",
            rusqlite::params![id, serde_json::to_string(&receipt)?, token],
        )?;
        Ok(Some(ClaimedBackup {
            id: id.to_owned(),
            token,
            parent: PathBuf::from(parent),
            name,
        }))
    })())
}
fn valid_manifest(m: &BackupManifest) -> bool {
    m.format == 1
        && m.schema <= SCHEMA_VERSION
        && (100..=MAX_DATABASE_BYTES).contains(&m.bytes)
        && m.sha256.len() == 64
        && m.sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn finish(
    tx: &Transaction<'_>,
    id: &str,
    token: &str,
    outcome: BackupOutcome,
) -> Result<BackupReceipt> {
    safe((|| {
        canonical_id(id)?;
        canonical_id(token)?;
        let mut receipt = get_inner(tx, id)?.ok_or_else(|| anyhow::anyhow!("记录缺失"))?;
        let current: Option<String> = tx.query_row(
            "SELECT claim_token FROM backup_jobs WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        if receipt.state != BackupState::Running || current.as_deref() != Some(token) {
            bail!("工作所有权已失效；不覆盖终态");
        }
        match outcome {
            BackupOutcome::Completed(manifest) => {
                if !valid_manifest(&manifest) {
                    bail!("完整备份元数据无效");
                }
                receipt.state = BackupState::Completed;
                receipt.manifest = Some(manifest);
            }
            BackupOutcome::Failed(reason) => {
                if !matches!(
                    reason,
                    BackupReason::ExportFailed | BackupReason::Cancelled | BackupReason::Deadline
                ) {
                    bail!("失败原因无效");
                }
                receipt.state = BackupState::Failed;
                receipt.reason = Some(reason);
            }
            BackupOutcome::Unknown => {
                receipt.state = BackupState::Unknown;
                receipt.reason = Some(BackupReason::OutcomeUncertain);
            }
        }
        tx.execute(
            "UPDATE backup_jobs SET receipt=?2,claim_token=NULL WHERE id=?1",
            rusqlite::params![id, serde_json::to_string(&receipt)?],
        )?;
        Ok(receipt)
    })())
}
/// Startup/restore only: retain intent and uncertainty; never schedule old jobs.
pub(crate) fn recover_interrupted(tx: &Transaction<'_>) -> Result<usize> {
    safe((|| {
        let mut changed = 0;
        for mut receipt in list(tx)? {
            if matches!(receipt.state, BackupState::Pending | BackupState::Running) {
                receipt.state = BackupState::Unknown;
                receipt.reason = Some(BackupReason::Restarted);
                tx.execute(
                    "UPDATE backup_jobs SET receipt=?2,claim_token=NULL WHERE id=?1",
                    rusqlite::params![receipt.id, serde_json::to_string(&receipt)?],
                )?;
                changed += 1;
            }
        }
        Ok(changed)
    })())
}
#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
