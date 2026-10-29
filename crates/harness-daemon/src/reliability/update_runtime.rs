//! Verification/staging coordinator. All authority is local-owner committed
//! state; no network, extraction, execution, installation or automatic download.
use super::{update, update_state as state, update_witness as witness};
use anyhow::{bail, Context, Result};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use rusqlite::Transaction;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncRead, sync::Semaphore};
const QUOTA: u64 = 512 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(120);
/// Implement with the authoritative Store mutex/connection. This callback must
/// commit on Ok and roll back on Err; no separate runtime SQLite database.
pub(crate) trait UpdateDatabase: Send + Sync + 'static {
    fn transaction<T>(&self, action: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T>;
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct StageStatus {
    pub files: usize,
    pub bytes: u64,
}
#[derive(Serialize)]
pub(crate) struct RuntimeStatus {
    pub mode: &'static str,
    pub installed: bool,
    pub automatic_update_enabled: bool,
    pub configuration: Option<state::TrustConfig>,
    pub floors: Vec<state::Floor>,
    pub receipts: Vec<state::Receipt>,
    pub staging: Option<StageStatus>,
    pub blocked_reason: Option<&'static str>,
}
#[derive(Serialize)]
pub(crate) struct RuntimeReceipt {
    pub receipt: state::Receipt,
    /// True only after checking current policy AND reading actual staged bytes.
    pub current_stage_verified: bool,
    pub installed: bool,
    pub automatic_update_enabled: bool,
}
pub(crate) struct UpdateRuntime<D: UpdateDatabase> {
    db: Arc<D>,
    stage: Arc<Dir>,
    witness: Arc<Dir>,
    current_version: String,
    schema: u32,
    target: update::Target,
    admission: Arc<Semaphore>,
    upload_gate: Arc<Semaphore>,
    active: Mutex<Option<(String, Arc<AtomicBool>)>>,
    #[cfg(test)]
    test_now: Option<Arc<std::sync::atomic::AtomicU64>>,
    #[cfg(test)]
    test_after_hash: Option<Arc<dyn Fn() + Send + Sync>>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn millis() -> u64 {
    now().saturating_mul(1000)
}
fn target_name(t: update::Target) -> String {
    serde_json::to_value(t).unwrap().as_str().unwrap().into()
}
fn basename(id: &str) -> Result<String> {
    if !uuid::Uuid::parse_str(id).is_ok_and(|v| v.to_string() == id) {
        bail!("更新操作标识无效");
    }
    Ok(format!(".update-{id}.verified"))
}
fn trusted(record: &witness::FloorRecord) -> state::TrustedWitness {
    state::TrustedWitness {
        target: record.target.clone(),
        version: record.version.clone(),
        sequence: record.sequence,
        manifest_sha256: record.manifest_sha256.clone(),
        sha256: record.artifact_sha256.clone(),
        operation_id: record.operation_id.clone(),
        claim_nonce: record.claim_nonce.clone(),
    }
}
fn import<D: UpdateDatabase>(db: &D, dir: &Dir) -> Result<()> {
    let records = witness::load(dir)?;
    let floors = witness::floors(&records)?;
    db.transaction(|tx| {
        for record in floors.values() {
            state::merge_witness(tx, &trusted(record))?;
        }
        Ok(())
    })
}
fn scan(dir: &Dir) -> Result<StageStatus> {
    let mut status = StageStatus { files: 0, bytes: 0 };
    for entry in dir.entries()? {
        let entry = entry?;
        status.files += 1;
        if status.files > 512 {
            bail!("更新暂存记录超过上限");
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("更新暂存名称无效"))?;
        let id = name
            .strip_prefix(".update-")
            .and_then(|s| {
                s.strip_suffix(".verified")
                    .or_else(|| s.strip_suffix(".part"))
            })
            .context("更新暂存目录包含未知记录")?;
        basename(id)?;
        if entry.file_type()?.is_symlink() {
            bail!("更新暂存拒绝链接");
        }
        let m = entry.metadata()?;
        if !m.is_file() {
            bail!("更新暂存拒绝链接或特殊文件");
        }
        status.bytes = status
            .bytes
            .checked_add(m.len())
            .context("更新暂存大小溢出")?;
        if status.bytes > QUOTA {
            bail!("更新暂存超过 512 MiB 配额");
        }
    }
    Ok(status)
}
fn matches_file(dir: &Dir, id: &str, m: &state::Metadata) -> Result<bool> {
    let name = basename(id)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = match dir.open_with(&name, &options) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != m.bytes || m.bytes > update::MAX_ARTIFACT_BYTES {
        bail!("暂存文件类型或大小不匹配");
    }
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > m.bytes {
            bail!("暂存文件已改变");
        }
        hash.update(&buffer[..n]);
    }
    if bytes != m.bytes || format!("{:x}", hash.finalize()) != m.sha256 {
        bail!("暂存摘要已改变");
    }
    Ok(true)
}
fn verify(
    raw: &state::ManifestEvidence,
    c: &state::TrustConfig,
    f: &state::Floor,
    current: &str,
    schema: u32,
    target: update::Target,
    time: u64,
) -> Result<(update::VerifiedManifest, state::Metadata)> {
    if !c.enabled {
        bail!("本机更新验证信任已停用");
    }
    if raw.signature_hex.len() != 128
        || !raw
            .signature_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || raw.raw_manifest.len() > update::MAX_MANIFEST_BYTES
    {
        bail!("更新输入超过限制");
    }
    let signature: Vec<u8> = (0..128)
        .step_by(2)
        .map(|i| u8::from_str_radix(&raw.signature_hex[i..i + 2], 16))
        .collect::<std::result::Result<_, _>>()?;
    let policy = update::UpdatePolicy::verification_only(c.public_key, &c.key_id, &c.source)?;
    let context =
        update::UpdateContext::new(current, &f.version, f.sequence, schema, target, time)?;
    let verified =
        update::verify_manifest(raw.raw_manifest.as_bytes(), &signature, &policy, &context)?;
    let m = verified.metadata();
    let metadata = state::Metadata {
        version: m.version,
        sequence: m.sequence,
        target: target_name(m.target),
        bytes: m.bytes,
        sha256: m.sha256,
        manifest_sha256: format!("{:x}", Sha256::digest(raw.raw_manifest.as_bytes())),
    };
    Ok((verified, metadata))
}
impl<D: UpdateDatabase> UpdateRuntime<D> {
    pub(crate) fn new(
        db: Arc<D>,
        stage: Dir,
        witness: Dir,
        current_version: String,
        schema: u32,
    ) -> Result<Arc<Self>> {
        if update::production_policy().status()
            != update::UpdateStatus::DisabledNoTrustedReleaseConfiguration
        {
            bail!("生产自动更新必须保持禁用");
        }
        let target = update::Target::current()?;
        update::UpdateContext::new(&current_version, &current_version, 0, schema, target, now())?;
        Ok(Arc::new(Self {
            db,
            stage: Arc::new(stage),
            witness: Arc::new(witness),
            current_version,
            schema,
            target,
            admission: Arc::new(Semaphore::new(8)),
            upload_gate: Arc::new(Semaphore::new(1)),
            active: Mutex::new(None),
            #[cfg(test)]
            test_now: None,
            #[cfg(test)]
            test_after_hash: None,
        }))
    }
    fn current_time(&self) -> u64 {
        #[cfg(test)]
        if let Some(time) = &self.test_now {
            return time.load(Ordering::Acquire);
        }
        now()
    }
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        action: impl FnOnce(Arc<Self>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("更新验证操作繁忙，未接收新操作"))?;
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            action(runtime)
        })
        .await?
    }
    #[cfg(test)]
    pub(crate) async fn recover(self: &Arc<Self>) -> Result<RuntimeStatus> {
        self.blocking(|r| r.db.transaction(|tx| state::recover(tx, millis())))
            .await?;
        self.status().await
    }
    pub(crate) async fn status(self: &Arc<Self>) -> Result<RuntimeStatus> {
        self.blocking(|r| {
            let witness_ok = import(r.db.as_ref(), &r.witness).is_ok();
            let staging = scan(&r.stage).ok();
            let (configuration, floors, mut receipts) = r.db.transaction(|tx| {
                let config = state::config(tx)?;
                let mut floors = Vec::new();
                for t in [
                    "linux-x86_64",
                    "linux-aarch64",
                    "windows-x86_64",
                    "windows-aarch64",
                    "macos-x86_64",
                    "macos-aarch64",
                ] {
                    floors.push(state::floor(tx, t)?);
                }
                Ok((config, floors, state::receipts(tx)?))
            })?;
            receipts.reverse();
            receipts.truncate(32);
            for receipt in &mut receipts {
                receipt.stage_authority = false;
            }
            let blocked_reason = if !witness_ok {
                Some("witness_unavailable")
            } else if staging.is_none() {
                Some("staging_unavailable")
            } else {
                None
            };
            Ok(RuntimeStatus {
                mode: if blocked_reason.is_some() {
                    "verification_blocked"
                } else if configuration.as_ref().is_some_and(|c| c.enabled) {
                    "verification_only"
                } else {
                    "verification_disabled"
                },
                installed: false,
                automatic_update_enabled: false,
                configuration,
                floors,
                receipts,
                staging,
                blocked_reason,
            })
        })
        .await
    }
    pub(crate) async fn configure(
        self: &Arc<Self>,
        expected: Option<u64>,
        input: state::TrustInput,
    ) -> Result<state::TrustConfig> {
        self.blocking(move |r| {
            import(r.db.as_ref(), &r.witness)?;
            let c =
                r.db.transaction(|tx| state::configure(tx, expected, input, millis()))?;
            if let Some((_, cancel)) = r.active.lock().unwrap().as_ref() {
                cancel.store(true, Ordering::Release);
            }
            Ok(c)
        })
        .await
    }
    pub(crate) async fn begin(
        self: &Arc<Self>,
        id: String,
        revision: u64,
        raw: state::ManifestEvidence,
    ) -> Result<state::Receipt> {
        self.blocking(move |r| {
            import(r.db.as_ref(), &r.witness)?;
            r.db.transaction(|tx| {
                let c = state::config(tx)?.context("尚未配置本机更新验证信任")?;
                if c.revision != revision {
                    bail!("更新信任版本已改变");
                }
                // Existing exact requests may return historical receipts without
                // re-verifying against the now-advanced floor or re-enqueueing.
                if let Some(old) = state::receipt(tx, &id)? {
                    let digest = state::request_digest(revision, &old.metadata, &raw)?;
                    let mut receipt =
                        state::begin(tx, &id, &digest, revision, old.metadata, raw, millis())?;
                    receipt.stage_authority = false;
                    return Ok(receipt);
                }
                let f =
                    state::effective_floor(tx, &target_name(r.target), &r.current_version, None)?;
                let (_, m) = verify(
                    &raw,
                    &c,
                    &f,
                    &r.current_version,
                    r.schema,
                    r.target,
                    r.current_time(),
                )?;
                let digest = state::request_digest(revision, &m, &raw)?;
                state::begin(tx, &id, &digest, revision, m, raw, millis())
            })
        })
        .await
    }
    fn current_authority(&self, tx: &Transaction<'_>, receipt: &state::Receipt) -> Result<bool> {
        let floor = state::floor(tx, &receipt.metadata.target)?;
        let Some(config) = state::config(tx)? else {
            return Ok(false);
        };
        if !config.enabled
            || config.revision != receipt.config_revision
            || receipt.status != state::Status::Verified
            || !receipt.stage_authority
            || floor.sequence != receipt.metadata.sequence
            || floor.version != receipt.metadata.version
        {
            return Ok(false);
        }
        let Some(evidence) = state::signed_evidence(tx, &receipt.id)? else {
            return Ok(false);
        };
        // An already accepted floor must equal this receipt. Signature/time/schema
        // are independently rechecked against the installed application baseline.
        let baseline = state::Floor {
            target: receipt.metadata.target.clone(),
            version: self.current_version.clone(),
            sequence: 0,
            manifest_sha256: None,
            sha256: None,
        };
        Ok(verify(
            &evidence,
            &config,
            &baseline,
            &self.current_version,
            self.schema,
            self.target,
            self.current_time(),
        )
        .is_ok_and(|(_, metadata)| metadata == receipt.metadata))
    }
    pub(crate) async fn receipt(self: &Arc<Self>, id: String) -> Result<Option<RuntimeReceipt>> {
        self.blocking(move |r| {
            let witness_ok = import(r.db.as_ref(), &r.witness).is_ok();
            let Some((mut receipt, current)) = r.db.transaction(|tx| {
                let Some(receipt) = state::receipt(tx, &id)? else {
                    return Ok(None);
                };
                let current = r.current_authority(tx, &receipt)?;
                Ok(Some((receipt, current)))
            })?
            else {
                return Ok(None);
            };
            let bytes_verified = witness_ok
                && current
                && matches_file(&r.stage, &id, &receipt.metadata).unwrap_or(false);
            #[cfg(test)]
            if bytes_verified {
                if let Some(hook) = &r.test_after_hash {
                    hook();
                }
            }
            // A bounded file hash can be slow. Revoke/config/floor changes during
            // it must not be projected as current stage authority afterward.
            let current_stage_verified = bytes_verified
                && import(r.db.as_ref(), &r.witness).is_ok()
                && r.db.transaction(|tx| {
                    let Some(latest) = state::receipt(tx, &id)? else {
                        return Ok(false);
                    };
                    Ok(latest == receipt && r.current_authority(tx, &latest)?)
                })?;
            receipt.stage_authority = current_stage_verified;
            Ok(Some(RuntimeReceipt {
                receipt,
                current_stage_verified,
                installed: false,
                automatic_update_enabled: false,
            }))
        })
        .await
    }
    pub(crate) async fn cancel(self: &Arc<Self>, id: String) -> Result<state::Receipt> {
        self.blocking(move |r| {
            let mut receipt = r.db.transaction(|tx| state::cancel(tx, &id, millis()))?;
            receipt.stage_authority = false;
            if let Some((active, cancel)) = r.active.lock().unwrap().as_ref() {
                if active == &id {
                    cancel.store(true, Ordering::Release);
                }
            }
            Ok(receipt)
        })
        .await
    }
    pub(crate) async fn discard(
        self: &Arc<Self>,
        id: String,
        expected_sha256: String,
    ) -> Result<state::Receipt> {
        self.blocking(move |r| {
            // Never remove an active writer's file or claim cancellation cleanup.
            if r.active
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(active, _)| active == &id)
            {
                bail!("验证仍在进行，尚不能清理暂存");
            }
            let receipt =
                r.db.transaction(|tx| state::receipt(tx, &id)?.context("更新操作不存在"))?;
            if receipt.metadata.sha256 != expected_sha256
                || matches!(
                    receipt.status,
                    state::Status::Pending | state::Status::Verifying
                )
            {
                bail!("暂存摘要或操作状态已改变");
            }
            if matches_file(&r.stage, &id, &receipt.metadata)? {
                r.stage.remove_file(basename(&id)?)?;
            }
            r.db.transaction(|tx| state::discarded(tx, &id, &expected_sha256, millis()))
        })
        .await
    }
    pub(crate) async fn upload<R: AsyncRead + Unpin + Send + 'static>(
        self: &Arc<Self>,
        id: String,
        source: R,
        cancel: Arc<AtomicBool>,
    ) -> Result<state::Receipt> {
        self.upload_until(id, source, cancel, Instant::now() + DEADLINE)
            .await
    }
    async fn upload_until<R: AsyncRead + Unpin + Send + 'static>(
        self: &Arc<Self>,
        id: String,
        source: R,
        cancel: Arc<AtomicBool>,
        deadline: Instant,
    ) -> Result<state::Receipt> {
        let deadline = deadline.min(Instant::now() + DEADLINE);
        basename(&id)?;
        let permit = self
            .upload_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("已有更新暂存正在验证，未接收第二份上传"))?;
        let runtime = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            *runtime.active.lock().unwrap() = Some((id.clone(), cancel.clone()));
            let result = runtime.upload_inner(id, source, cancel, deadline).await;
            *runtime.active.lock().unwrap() = None;
            result
        })
        .await?
    }
    async fn upload_inner<R: AsyncRead + Unpin + Send + 'static>(
        self: &Arc<Self>,
        id: String,
        source: R,
        cancel: Arc<AtomicBool>,
        deadline: Instant,
    ) -> Result<state::Receipt> {
        let op = id.clone();
        let claim = self
            .blocking(move |r| {
                import(r.db.as_ref(), &r.witness)?;
                r.db.transaction(|tx| {
                    state::claim(tx, &op, millis())?.context("更新操作不可重放或已终止")
                })
            })
            .await?;
        let work = self.clone();
        let stage_cancel = cancel.clone();
        let stage_id = id.clone();
        let revision = claim.config.revision;
        let expected = claim.metadata.clone();
        let evidence = claim.evidence.clone();
        let prepared = self
            .blocking(move |r| {
                import(r.db.as_ref(), &r.witness)?;
                let (config, floor) = r.db.transaction(|tx| {
                    Ok((
                        state::config(tx)?.context("更新信任配置缺失")?,
                        state::effective_floor(
                            tx,
                            &target_name(r.target),
                            &r.current_version,
                            None,
                        )?,
                    ))
                })?;
                if config.revision != revision {
                    bail!("更新信任版本已改变");
                }
                let (verified, metadata) = verify(
                    &evidence,
                    &config,
                    &floor,
                    &r.current_version,
                    r.schema,
                    r.target,
                    r.current_time(),
                )?;
                if metadata != expected {
                    bail!("更新清单与原意图不一致");
                }
                let quota = scan(&r.stage)?;
                // Publication briefly uses both .part and .verified hard links.
                if quota.files > 510 {
                    bail!("更新暂存名称槽不足，未创建新文件");
                }
                if quota
                    .bytes
                    .checked_add(metadata.bytes)
                    .is_none_or(|n| n > QUOTA)
                {
                    bail!("更新暂存超过 512 MiB 配额");
                }
                Ok(verified)
            })
            .await;
        let verified = match prepared {
            Ok(v) => v,
            Err(error) => {
                let db = self.db.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    db.transaction(|tx| state::fail(tx, &claim, state::Failure::Rejected, millis()))
                })
                .await;
                return Err(error);
            }
        };
        let handle = tokio::runtime::Handle::current();
        let mut stage = tokio::task::spawn_blocking(move || {
            handle.block_on(update::stage_artifact_for(
                &stage_id,
                source,
                verified,
                &work.stage,
                deadline,
                &|| stage_cancel.load(Ordering::Acquire),
            ))
        });
        let staged = loop {
            tokio::select! {
                result=&mut stage=>break result,
                _=tokio::time::sleep(Duration::from_millis(100))=>{
                    let current_id=id.clone();let valid=self.blocking(move|r|r.db.transaction(|tx|Ok(state::config(tx)?.is_some_and(|c|c.enabled&&c.revision==revision)&&state::receipt(tx,&current_id)?.is_some_and(|r|r.status==state::Status::Verifying&&r.reason.as_deref()!=Some("cancellation_requested"))))).await;
                    if !matches!(valid,Ok(true))||Instant::now()>=deadline{cancel.store(true,Ordering::Release);}
                }
            }
        };
        let staged = match staged {
            Ok(Ok(s)) => s,
            result => {
                let worker_failed = result.is_err();
                let error = match result {
                    Ok(Err(error)) => anyhow::Error::from(error),
                    Err(_) => anyhow::anyhow!("更新文件验证工作未确认"),
                    Ok(Ok(_)) => unreachable!(),
                };
                let db = self.db.clone();
                let reason = if worker_failed {
                    state::Failure::Unknown
                } else if cancel.load(Ordering::Acquire) {
                    state::Failure::Cancelled
                } else {
                    state::Failure::Rejected
                };
                let _ = tokio::task::spawn_blocking(move || {
                    db.transaction(|tx| state::fail(tx, &claim, reason, millis()))
                })
                .await;
                return Err(error);
            }
        };
        // Filesystem finalization stays on a blocking worker; the staged guard's
        // destructor also runs here on any failure, never on a Tokio worker.
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || -> Result<state::Receipt> {
            let result = (|| {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    bail!("更新验证已取消或超时");
                }
                staged.valid_at(runtime.current_time())?;
                runtime.db.transaction(|tx| {
                    let current = state::config(tx)?.context("更新信任配置缺失")?;
                    let receipt = state::receipt(tx, &id)?.context("更新操作缺失")?;
                    if current != claim.config
                        || !current.enabled
                        || receipt.reason.as_deref() == Some("cancellation_requested")
                    {
                        bail!("更新验证权限已撤销");
                    }
                    Ok(())
                })?;
                let record = witness::FloorRecord {
                    schema: 1,
                    operation_id: id,
                    claim_nonce: claim.nonce.clone(),
                    target: claim.metadata.target.clone(),
                    version: claim.metadata.version.clone(),
                    sequence: claim.metadata.sequence,
                    manifest_sha256: claim.metadata.manifest_sha256.clone(),
                    artifact_sha256: claim.metadata.sha256.clone(),
                };
                witness::append(&runtime.witness, &record)?;
                let receipt = runtime.db.transaction(|tx| {
                    if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                        bail!("更新验证已取消或超时");
                    }
                    staged.valid_at(runtime.current_time())?;
                    state::complete_verified(tx, &claim, &trusted(&record), millis())
                })?;
                staged.retain();
                Ok(receipt)
            })();
            if result.is_err() {
                let _ = runtime
                    .db
                    .transaction(|tx| state::fail(tx, &claim, state::Failure::Unknown, millis()));
            }
            result
        })
        .await?
    }
}
#[cfg(test)]
#[path = "update_runtime_tests.rs"]
mod tests;
