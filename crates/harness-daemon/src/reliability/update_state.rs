//! Transaction-only local-owner verification state, NEVER an installer.
//! The caller supplies already verified manifest metadata and a trusted witness
//! only after append+fsync outside the database backup. Errors require rollback.
//! No callback here reads files, downloads, executes, or accesses a secret vault.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
const MAX_REV: u64 = 9_007_199_254_740_991;
const MAX_JOBS: u64 = 256;
const MAX_RECORD: usize = 128 * 1024;
const MAX_ARTIFACT: u64 = 256 * 1024 * 1024;
const APPLICATION: &str = "self-agent";

#[derive(Clone)]
pub(crate) struct TrustInput {
    pub key_id: String,
    pub source: String,
    pub public_key: [u8; 32],
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrustConfig {
    pub revision: u64,
    pub key_id: String,
    pub source: String,
    pub public_key: [u8; 32],
    pub fingerprint: String,
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Metadata {
    pub version: String,
    pub sequence: u64,
    pub target: String,
    pub bytes: u64,
    pub sha256: String,
    pub manifest_sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestEvidence {
    pub raw_manifest: String,
    pub signature_hex: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Pending,
    Verifying,
    Verified,
    Failed,
    Interrupted,
    Unknown,
}
impl Status {
    fn text(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verifying => "verifying",
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub id: String,
    pub config_revision: u64,
    pub metadata: Metadata,
    pub status: Status,
    /// Private staging eligibility only; never installation/download permission.
    pub stage_authority: bool,
    pub reason: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    receipt: Receipt,
    request_digest: String,
    nonce: Option<String>,
    evidence: ManifestEvidence,
}
/// Not deserializable: only the claim transaction creates this one-shot work.
pub(crate) struct Claim {
    pub id: String,
    pub nonce: String,
    pub config: TrustConfig,
    pub metadata: Metadata,
    pub evidence: ManifestEvidence,
}
/// Trusted filesystem adapter input, NOT an HTTP/remote payload. Construct only
/// after verifying the append-only witness and fsyncing this exact record.
pub(crate) struct TrustedWitness {
    pub target: String,
    pub version: String,
    pub sequence: u64,
    pub manifest_sha256: String,
    pub operation_id: String,
    pub claim_nonce: String,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Floor {
    pub target: String,
    pub version: String,
    pub sequence: u64,
    pub manifest_sha256: Option<String>,
    pub sha256: Option<String>,
}
#[derive(Clone, Copy)]
pub(crate) enum Failure {
    Rejected,
    Cancelled,
    Unknown,
}

pub(crate) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS update_trust(singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision INTEGER NOT NULL,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS update_jobs(id TEXT PRIMARY KEY,config_revision INTEGER NOT NULL,status TEXT NOT NULL,nonce TEXT,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS update_floors(application TEXT NOT NULL,target TEXT NOT NULL,version TEXT NOT NULL,sequence INTEGER NOT NULL,manifest_sha256 TEXT,sha256 TEXT,PRIMARY KEY(application,target));")?;
    Ok(())
}
fn uuid(value: &str) -> Result<()> {
    if !uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value) {
        bail!("更新操作标识必须为规范 UUID");
    }
    Ok(())
}
fn digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("更新摘要无效");
    }
    Ok(())
}
fn revision(value: u64) -> Result<()> {
    if value == 0 || value > MAX_REV {
        bail!("更新配置修订无效");
    }
    Ok(())
}
fn time(value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        bail!("更新时间无效");
    }
    Ok(())
}
fn target(value: &str) -> Result<()> {
    if !matches!(
        value,
        "linux-x86_64"
            | "linux-aarch64"
            | "windows-x86_64"
            | "windows-aarch64"
            | "macos-x86_64"
            | "macos-aarch64"
    ) {
        bail!("更新目标不受支持");
    }
    Ok(())
}
fn version(value: &str) -> Result<Version> {
    if value.len() > 64 {
        bail!("更新版本过长");
    }
    let parsed = Version::parse(value)?;
    if !parsed.pre.is_empty() || !parsed.build.is_empty() || parsed.to_string() != value {
        bail!("仅支持规范正式版本");
    }
    Ok(parsed)
}
fn source(value: &str) -> Result<String> {
    if value.len() > 2048 || value.chars().any(char::is_control) {
        bail!("更新来源无效");
    }
    let url = reqwest::Url::parse(value)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("更新来源必须是固定 HTTPS 地址，不包含认证、参数或片段");
    }
    Ok(url.to_string())
}
fn input(value: TrustInput, rev: u64) -> Result<TrustConfig> {
    revision(rev)?;
    if value.key_id.is_empty()
        || value.key_id.len() > 64
        || !value
            .key_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        bail!("更新签名标识无效");
    }
    Ok(TrustConfig {
        revision: rev,
        key_id: value.key_id,
        source: source(&value.source)?,
        fingerprint: format!("{:x}", Sha256::digest(value.public_key)),
        public_key: value.public_key,
        enabled: value.enabled,
    })
}
fn metadata(value: &Metadata) -> Result<()> {
    version(&value.version)?;
    target(&value.target)?;
    if value.sequence == 0
        || value.sequence > MAX_REV
        || value.bytes == 0
        || value.bytes > MAX_ARTIFACT
    {
        bail!("更新序号或大小无效");
    }
    digest(&value.sha256)?;
    digest(&value.manifest_sha256)
}
fn evidence(value: &ManifestEvidence, m: &Metadata) -> Result<()> {
    if value.raw_manifest.is_empty()
        || value.raw_manifest.len() > 16 * 1024
        || value.signature_hex.len() != 128
        || !value
            .signature_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("更新清单或签名超出明确上限");
    }
    if format!("{:x}", Sha256::digest(value.raw_manifest.as_bytes())) != m.manifest_sha256 {
        bail!("更新清单原文摘要不一致");
    }
    Ok(())
}
/// Stable request identity includes exact original UTF-8 manifest/signature and
/// the separately confirmed trust revision; receipts do not expose raw evidence.
pub(crate) fn request_digest(
    config_revision: u64,
    m: &Metadata,
    raw: &ManifestEvidence,
) -> Result<String> {
    revision(config_revision)?;
    metadata(m)?;
    evidence(raw, m)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(config_revision, m, raw))?)
    ))
}
pub(crate) fn config(db: &Connection) -> Result<Option<TrustConfig>> {
    let row:Option<(u64,String)>=db.query_row("SELECT revision,CASE WHEN length(CAST(body AS BLOB))<=16384 THEN body END FROM update_trust WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(rev, body)| {
        let c: TrustConfig = serde_json::from_str(&body)?;
        let checked = input(
            TrustInput {
                key_id: c.key_id.clone(),
                source: c.source.clone(),
                public_key: c.public_key,
                enabled: c.enabled,
            },
            rev,
        )?;
        if checked != c {
            bail!("更新信任配置索引不一致");
        }
        Ok(c)
    })
    .transpose()
}
pub(crate) fn configure(
    tx: &Transaction<'_>,
    expected_revision: Option<u64>,
    value: TrustInput,
    now: u64,
) -> Result<TrustConfig> {
    time(now)?;
    let current = config(tx)?;
    if current.as_ref().map(|c| c.revision) != expected_revision {
        bail!("更新信任配置版本已改变");
    }
    let next = current.map_or(1, |c| c.revision.saturating_add(1));
    let configured = input(value, next)?;
    invalidate_jobs(tx, now, "trust_changed", false)?;
    tx.execute("INSERT INTO update_trust VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,body=excluded.body",params![configured.revision,serde_json::to_string(&configured)?])?;
    Ok(configured)
}
fn job(db: &Connection, id: &str) -> Result<Option<Job>> {
    uuid(id)?;
    let row:Option<(u64,String,Option<String>,String)>=db.query_row("SELECT config_revision,status,nonce,CASE WHEN length(CAST(body AS BLOB))<=131072 THEN body END FROM update_jobs WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    row.map(|(rev, status, nonce, body)| {
        let job: Job = serde_json::from_str(&body)?;
        let r = &job.receipt;
        revision(rev)?;
        metadata(&r.metadata)?;
        digest(&job.request_digest)?;
        if request_digest(rev, &r.metadata, &job.evidence)? != job.request_digest {
            bail!("更新请求摘要不一致");
        }
        time(r.created_at_ms)?;
        time(r.updated_at_ms)?;
        if r.id != id
            || r.config_revision != rev
            || r.status.text() != status
            || job.nonce != nonce
            || r.updated_at_ms < r.created_at_ms
            || (r.stage_authority && r.status != Status::Verified)
            || (r.status == Status::Verifying && nonce.is_none())
        {
            bail!("更新操作索引不一致");
        }
        if let Some(n) = nonce {
            uuid(&n)?;
        }
        Ok(job)
    })
    .transpose()
}
fn put(tx: &Transaction<'_>, job: &Job) -> Result<()> {
    let body = serde_json::to_string(job)?;
    if body.len() > MAX_RECORD {
        bail!("更新操作记录过大");
    }
    let r = &job.receipt;
    tx.execute("INSERT INTO update_jobs VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET status=excluded.status,nonce=excluded.nonce,body=excluded.body",params![r.id,r.config_revision,r.status.text(),job.nonce,body])?;
    Ok(())
}
pub(crate) fn receipt(db: &Connection, id: &str) -> Result<Option<Receipt>> {
    Ok(job(db, id)?.map(|j| j.receipt))
}
/// Private signed evidence for current-policy revalidation. Never returned by
/// public receipt/status projection.
pub(crate) fn signed_evidence(db: &Connection, id: &str) -> Result<Option<ManifestEvidence>> {
    Ok(job(db, id)?.map(|j| j.evidence))
}
pub(crate) fn receipts(db: &Connection) -> Result<Vec<Receipt>> {
    let ids = job_ids(db)?;
    ids.iter()
        .map(|id| receipt(db, id)?.context("更新审计缺失"))
        .collect()
}
fn job_ids(db: &Connection) -> Result<Vec<String>> {
    let count: u64 = db.query_row("SELECT COUNT(*) FROM update_jobs", [], |r| r.get(0))?;
    if count > MAX_JOBS {
        bail!("更新记录超过容量");
    }
    let mut q = db.prepare("SELECT id FROM update_jobs ORDER BY rowid")?;
    let ids = q
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}
pub(crate) fn floor(db: &Connection, wanted: &str) -> Result<Floor> {
    target(wanted)?;
    let row:Option<(String,u64,Option<String>,Option<String>)>=db.query_row("SELECT version,sequence,manifest_sha256,sha256 FROM update_floors WHERE application=?1 AND target=?2",params![APPLICATION,wanted],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    if row
        .as_ref()
        .is_some_and(|(_, sequence, manifest, artifact)| {
            *sequence == 0 || manifest.is_none() || artifact.is_none()
        })
    {
        bail!("持久更新下限缺少见证摘要");
    }
    let f = match row {
        Some((version, sequence, manifest_sha256, sha256)) => Floor {
            target: wanted.into(),
            version,
            sequence,
            manifest_sha256,
            sha256,
        },
        None => Floor {
            target: wanted.into(),
            version: "0.0.0".into(),
            sequence: 0,
            manifest_sha256: None,
            sha256: None,
        },
    };
    version(&f.version)?;
    if f.sequence > MAX_REV {
        bail!("更新防回退序号无效");
    }
    if let Some(hash) = &f.manifest_sha256 {
        digest(hash)?;
    }
    if let Some(hash) = &f.sha256 {
        digest(hash)?;
    }
    Ok(f)
}
fn witness(w: &TrustedWitness) -> Result<()> {
    target(&w.target)?;
    version(&w.version)?;
    digest(&w.manifest_sha256)?;
    digest(&w.sha256)?;
    uuid(&w.operation_id)?;
    uuid(&w.claim_nonce)?;
    if w.sequence == 0 || w.sequence > MAX_REV {
        bail!("更新见证序号无效");
    }
    Ok(())
}
/// Read-side maximum includes the running application even if its trusted
/// release sequence is unavailable. Caller must load/validate witness first.
pub(crate) fn effective_floor(
    db: &Connection,
    wanted: &str,
    current_version: &str,
    w: Option<&TrustedWitness>,
) -> Result<Floor> {
    let mut f = floor(db, wanted)?;
    let current = version(current_version)?;
    if current > version(&f.version)? {
        f.version = current.to_string();
        f.manifest_sha256 = None;
        f.sha256 = None;
    }
    if let Some(w) = w {
        witness(w)?;
        if w.target != wanted {
            bail!("更新见证目标不一致");
        }
        if version(&w.version)? > version(&f.version)? {
            f.version = w.version.clone();
        }
        f.sequence = f.sequence.max(w.sequence);
        f.manifest_sha256 = None;
        f.sha256 = None;
    }
    Ok(f)
}
/// Only a validated append+fsync witness may reach this API. Strictly monotonic
/// in BOTH dimensions; dominated historical records are harmless no-ops. A
/// same-floor different manifest/artifact is not an idempotent replay.
pub(crate) fn merge_witness(tx: &Transaction<'_>, w: &TrustedWitness) -> Result<Floor> {
    witness(w)?;
    let old = floor(tx, &w.target)?;
    let order = version(&w.version)?.cmp(&version(&old.version)?);
    if (order.is_gt() && w.sequence < old.sequence) || (order.is_lt() && w.sequence > old.sequence)
    {
        bail!("更新见证版本和序号交叉回退");
    }
    if order.is_eq()
        && w.sequence == old.sequence
        && (old
            .manifest_sha256
            .as_ref()
            .is_some_and(|h| h != &w.manifest_sha256)
            || old.sha256.as_ref().is_some_and(|h| h != &w.sha256))
    {
        bail!("相同更新下限的摘要冲突");
    }
    if !order.is_gt() && w.sequence <= old.sequence {
        return Ok(old);
    }
    tx.execute("INSERT INTO update_floors VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(application,target) DO UPDATE SET version=excluded.version,sequence=excluded.sequence,manifest_sha256=excluded.manifest_sha256,sha256=excluded.sha256",params![APPLICATION,w.target,w.version,w.sequence,w.manifest_sha256,w.sha256])?;
    floor(tx, &w.target)
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn begin(
    tx: &Transaction<'_>,
    id: &str,
    request_digest: &str,
    config_revision: u64,
    m: Metadata,
    raw: ManifestEvidence,
    now: u64,
) -> Result<Receipt> {
    uuid(id)?;
    digest(request_digest)?;
    revision(config_revision)?;
    metadata(&m)?;
    time(now)?;
    if self::request_digest(config_revision, &m, &raw)? != request_digest {
        bail!("更新请求摘要与原文不匹配");
    }
    if let Some(old) = job(tx, id)? {
        if old.request_digest != request_digest
            || old.receipt.config_revision != config_revision
            || old.receipt.metadata != m
            || old.evidence != raw
        {
            bail!("更新操作标识已用于其他请求");
        }
        return Ok(old.receipt);
    }
    let c = config(tx)?.context("尚未配置本机更新验证信任")?;
    if !c.enabled || c.revision != config_revision {
        bail!("更新验证信任已停用或版本改变");
    }
    let f = floor(tx, &m.target)?;
    if version(&m.version)? <= version(&f.version)? || m.sequence <= f.sequence {
        bail!("更新版本或序号不高于防回退下限");
    }
    if job_ids(tx)?.len() >= MAX_JOBS as usize {
        bail!("更新审计容量已满，未删除历史记录");
    }
    let job = Job {
        receipt: Receipt {
            id: id.into(),
            config_revision,
            metadata: m,
            status: Status::Pending,
            stage_authority: false,
            reason: None,
            created_at_ms: now,
            updated_at_ms: now,
        },
        request_digest: request_digest.into(),
        nonce: None,
        evidence: raw,
    };
    put(tx, &job)?;
    Ok(job.receipt)
}
pub(crate) fn claim(tx: &Transaction<'_>, id: &str, now: u64) -> Result<Option<Claim>> {
    time(now)?;
    let Some(mut j) = job(tx, id)? else {
        return Ok(None);
    };
    if j.receipt.status != Status::Pending {
        return Ok(None);
    }
    let c = config(tx)?.context("更新信任配置缺失")?;
    if !c.enabled || c.revision != j.receipt.config_revision {
        bail!("更新配置已改变，未获取验证权限");
    }
    let f = floor(tx, &j.receipt.metadata.target)?;
    if version(&j.receipt.metadata.version)? <= version(&f.version)?
        || j.receipt.metadata.sequence <= f.sequence
    {
        bail!("更新下限已前进，未获取旧操作权限");
    }
    let nonce = uuid::Uuid::new_v4().to_string();
    j.nonce = Some(nonce.clone());
    j.receipt.status = Status::Verifying;
    j.receipt.updated_at_ms = now.max(j.receipt.updated_at_ms);
    put(tx, &j)?;
    Ok(Some(Claim {
        id: id.into(),
        nonce,
        config: c,
        metadata: j.receipt.metadata,
        evidence: j.evidence,
    }))
}
fn matching(tx: &Transaction<'_>, c: &Claim) -> Result<Job> {
    let j = job(tx, &c.id)?.context("更新操作不存在")?;
    if j.receipt.config_revision != c.config.revision
        || j.receipt.metadata != c.metadata
        || j.nonce.as_deref() != Some(&c.nonce)
    {
        bail!("更新执行声明不匹配");
    }
    Ok(j)
}
pub(crate) fn complete_verified(
    tx: &Transaction<'_>,
    c: &Claim,
    w: &TrustedWitness,
    now: u64,
) -> Result<Receipt> {
    time(now)?;
    witness(w)?;
    let mut j = matching(tx, c)?;
    if w.operation_id != c.id
        || w.claim_nonce != c.nonce
        || w.target != c.metadata.target
        || w.version != c.metadata.version
        || w.sequence != c.metadata.sequence
        || w.manifest_sha256 != c.metadata.manifest_sha256
        || w.sha256 != c.metadata.sha256
    {
        bail!("更新持久见证与已验证操作不一致");
    }
    if j.receipt.status == Status::Verified {
        return Ok(j.receipt);
    }
    let current = config(tx)?.context("更新信任配置缺失")?;
    if j.receipt.status != Status::Verifying
        || j.receipt.reason.as_deref() == Some("cancellation_requested")
        || !current.enabled
        || current != c.config
    {
        bail!("更新执行权限已失效，不能完成暂存");
    }
    let latest = floor(tx, &c.metadata.target)?;
    if version(&c.metadata.version)? < version(&latest.version)?
        || c.metadata.sequence < latest.sequence
    {
        bail!("更新下限已前进，旧验证不能产生暂存权限");
    }
    merge_witness(tx, w)?;
    j.receipt.status = Status::Verified;
    j.receipt.stage_authority = true;
    j.receipt.reason = None;
    j.receipt.updated_at_ms = now.max(j.receipt.updated_at_ms);
    put(tx, &j)?;
    Ok(j.receipt)
}
pub(crate) fn fail(tx: &Transaction<'_>, c: &Claim, failure: Failure, now: u64) -> Result<Receipt> {
    time(now)?;
    let mut j = matching(tx, c)?;
    if j.receipt.status != Status::Verifying {
        return Ok(j.receipt);
    }
    j.receipt.status = if matches!(failure, Failure::Unknown) {
        Status::Unknown
    } else {
        Status::Failed
    };
    j.receipt.reason = Some(
        match failure {
            Failure::Rejected => "verification_rejected",
            Failure::Cancelled => "verification_cancelled",
            Failure::Unknown => "verification_outcome_unknown",
        }
        .into(),
    );
    j.receipt.stage_authority = false;
    j.receipt.updated_at_ms = now.max(j.receipt.updated_at_ms);
    put(tx, &j)?;
    Ok(j.receipt)
}
fn invalidate_jobs(tx: &Transaction<'_>, now: u64, reason: &str, restart: bool) -> Result<()> {
    time(now)?;
    for id in job_ids(tx)? {
        let mut j = job(tx, &id)?.context("更新操作缺失")?;
        let changed = matches!(j.receipt.status, Status::Pending | Status::Verifying)
            || (!restart && j.receipt.stage_authority);
        if j.receipt.status == Status::Pending {
            j.receipt.status = Status::Interrupted;
            j.receipt.reason = Some(reason.into());
        } else if j.receipt.status == Status::Verifying {
            j.receipt.status = Status::Unknown;
            j.receipt.reason = Some(reason.into());
        }
        if !restart || j.receipt.status != Status::Verified {
            j.receipt.stage_authority = false;
        }
        if changed {
            j.receipt.updated_at_ms = now.max(j.receipt.updated_at_ms);
            put(tx, &j)?;
        }
    }
    Ok(())
}
pub(crate) fn recover(tx: &Transaction<'_>, now: u64) -> Result<()> {
    invalidate_jobs(tx, now, "verification_interrupted_no_replay", true)
}
pub(crate) fn restore(tx: &Transaction<'_>, now: u64) -> Result<()> {
    invalidate_jobs(tx, now, "backup_restored_no_authority", false)?;
    if let Some(mut c) = config(tx)? {
        c.enabled = false;
        c.revision = c.revision.saturating_add(1).min(MAX_REV);
        tx.execute(
            "UPDATE update_trust SET revision=?1,body=?2 WHERE singleton=1",
            params![c.revision, serde_json::to_string(&c)?],
        )?;
    }
    // Floors are deliberately untouched. Before accepting any new operation,
    // caller MUST reconcile the external witness, which is not restored here.
    Ok(())
}
pub(crate) fn validate(db: &Connection) -> Result<()> {
    let singleton: u64 = db.query_row(
        "SELECT COUNT(*) FROM update_trust WHERE singleton<>1",
        [],
        |r| r.get(0),
    )?;
    if singleton != 0 {
        bail!("更新信任单例无效");
    }
    config(db)?;
    receipts(db)?;
    let mut q = db.prepare("SELECT application,target FROM update_floors")?;
    let rows = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > 6 {
        bail!("更新下限目标过多");
    }
    for (app, target) in rows {
        if app != APPLICATION {
            bail!("更新下限应用身份不一致");
        }
        floor(db, &target)?;
    }
    Ok(())
}
#[cfg(test)]
#[path = "update_state_tests.rs"]
mod tests;

/// Digest-matched private staging removal revokes only stage eligibility, never
/// rewrites the historical verification fact or monotonic acceptance floor.
pub(crate) fn discarded(
    tx: &Transaction<'_>,
    id: &str,
    expected_sha256: &str,
    now: u64,
) -> Result<Receipt> {
    time(now)?;
    digest(expected_sha256)?;
    let mut j = job(tx, id)?.context("更新操作不存在")?;
    if j.receipt.metadata.sha256 != expected_sha256
        || matches!(j.receipt.status, Status::Pending | Status::Verifying)
    {
        bail!("暂存摘要或操作状态已改变");
    }
    j.receipt.stage_authority = false;
    j.receipt.reason = Some("staging_discarded".into());
    j.receipt.updated_at_ms = now.max(j.receipt.updated_at_ms);
    put(tx, &j)?;
    Ok(j.receipt)
}

/// Cancellation is a durable request, not evidence that filesystem work stopped.
pub(crate) fn cancel(tx: &Transaction<'_>, id: &str, now: u64) -> Result<Receipt> {
    time(now)?;
    let mut j = job(tx, id)?.context("更新操作不存在")?;
    match j.receipt.status {
        Status::Pending => {
            j.receipt.status = Status::Failed;
            j.receipt.reason = Some("verification_cancelled".into());
        }
        Status::Verifying => j.receipt.reason = Some("cancellation_requested".into()),
        _ => return Ok(j.receipt),
    }
    j.receipt.stage_authority = false;
    j.receipt.updated_at_ms = now.max(j.receipt.updated_at_ms);
    put(tx, &j)?;
    Ok(j.receipt)
}
