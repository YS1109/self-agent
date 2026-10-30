//! Offline signed-update verification and private staging, NOT an installer.
//! There is intentionally no production trust root or update source yet: the
//! production automatic-update policy is Disabled. A separate, explicitly
//! owner-confirmed policy can verify/stage bytes only; it cannot install them.
//! No network, process execution, extraction or installation occurs here.
//!
//! Signatures cover DOMAIN || the exact bounded manifest bytes (not reserialized
//! JSON). Verification policy is loaded from durable owner configuration, never
//! from the manifest, upload request, environment or downloaded artifact.
//! Staging uses an already authorized/pinned directory capability. OS filesystem
//! calls run synchronously inside the runtime-owned blocking staging worker.
//! Input waits are cancellable; filesystem operations remain awaited/owned until
//! they return. We do not claim preemptible syscalls or power-loss installation.
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use ring::signature::{UnparsedPublicKey, ED25519};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    future::Future,
    io::{self, Read, Seek, Write},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) const MAX_MANIFEST_BYTES: usize = 16 * 1024;
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const DOMAIN: &[u8] = b"self-agent-update-manifest-v1\0";
const CHUNK_BYTES: usize = 64 * 1024;
const MAX_STAGE_TIME: Duration = Duration::from_secs(120);
const MAX_MANIFEST_LIFETIME: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Target {
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "linux-aarch64")]
    LinuxAarch64,
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
    #[serde(rename = "windows-aarch64")]
    WindowsAarch64,
    #[serde(rename = "macos-x86_64")]
    MacosX86_64,
    #[serde(rename = "macos-aarch64")]
    MacosAarch64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateStatus {
    DisabledNoTrustedReleaseConfiguration,
    VerificationOnly,
}

struct Trust {
    public_key: [u8; 32],
    key_id: String,
    source: String,
}
/// No Deserialize: the trusted runtime constructs this only from the previously
/// confirmed owner record. A verification request cannot supply its own key.
pub(crate) struct UpdatePolicy {
    trust: Option<Trust>,
}
pub(crate) fn production_policy() -> UpdatePolicy {
    UpdatePolicy { trust: None }
}
impl UpdatePolicy {
    /// This does NOT enable production automatic updates. Call only with a
    /// committed local-owner trust record; never with current upload fields.
    pub(crate) fn verification_only(
        public_key: [u8; 32],
        key_id: &str,
        source: &str,
    ) -> Result<Self, UpdateError> {
        if key_id.is_empty()
            || key_id.len() > 64
            || !key_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || source.len() > 2048
        {
            return Err(UpdateError::Source);
        }
        let url = url::Url::parse(source).map_err(|_| UpdateError::Source)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.as_str() != source
        {
            return Err(UpdateError::Source);
        }
        Ok(Self {
            trust: Some(Trust {
                public_key,
                key_id: key_id.into(),
                source: source.into(),
            }),
        })
    }
    pub(crate) fn status(&self) -> UpdateStatus {
        if self.trust.is_some() {
            UpdateStatus::VerificationOnly
        } else {
            UpdateStatus::DisabledNoTrustedReleaseConfiguration
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateError {
    Disabled,
    ManifestTooLarge,
    Signature,
    Format,
    Source,
    Platform,
    Schema,
    Version,
    Downgrade,
    Expired,
    Size,
    Digest,
    Cancelled,
    Deadline,
    Io,
}
impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "更新已禁用：尚未配置受信任的生产签名与更新源",
            Self::ManifestTooLarge => "更新清单超过大小限制",
            Self::Signature => "更新签名校验失败；未接受更新",
            Self::Format => "更新清单格式不受支持",
            Self::Source => "更新来源或签名标识不匹配",
            Self::Platform => "更新平台不匹配",
            Self::Schema => "更新不兼容当前数据库版本",
            Self::Version => "更新版本格式不受支持",
            Self::Downgrade => "更新版本或发布序号不是严格升级；拒绝回退与重放",
            Self::Expired => "更新清单未生效或已过期",
            Self::Size => "更新产物大小不匹配或超出限制",
            Self::Digest => "更新产物摘要校验失败",
            Self::Cancelled => "更新校验已取消；未发布暂存产物",
            Self::Deadline => "更新校验已到期；未发布暂存产物",
            Self::Io => "更新暂存读写失败；未安装更新",
        })
    }
}
impl std::error::Error for UpdateError {}

/// Caller loads the anti-rollback floor from durable application state plus the
/// independent floor witness, never from the uploaded manifest. This primitive
/// does not itself persist authority; the runtime must publish its witness before
/// the successful SQLite receipt. No installer consumes this capability.
pub(crate) struct UpdateContext {
    current: Version,
    highest_accepted: Version,
    sequence_floor: u64,
    schema: u32,
    target: Target,
    now: u64,
}
impl UpdateContext {
    pub(crate) fn new(
        current: &str,
        highest_accepted: &str,
        sequence_floor: u64,
        schema: u32,
        target: Target,
        now: u64,
    ) -> Result<Self, UpdateError> {
        let current = stable_version(current)?;
        let highest_accepted = stable_version(highest_accepted)?;
        if highest_accepted.cmp_precedence(&current) == Ordering::Less {
            return Err(UpdateError::Downgrade);
        }
        Ok(Self {
            current,
            highest_accepted,
            sequence_floor,
            schema,
            target,
            now,
        })
    }
}
fn stable_version(text: &str) -> Result<Version, UpdateError> {
    if text.len() > 64 {
        return Err(UpdateError::Version);
    }
    let version = Version::parse(text).map_err(|_| UpdateError::Version)?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(UpdateError::Version);
    }
    Ok(version)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    manifest_schema: u32,
    application: String,
    channel: String,
    source: String,
    key_id: String,
    version: String,
    sequence: u64,
    target: Target,
    database_schema_min: u32,
    database_schema_max: u32,
    issued_at: u64,
    expires_at: u64,
    artifact: Artifact,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    name: String,
    bytes: u64,
    sha256: String,
}
/// Fields stay private: unverified JSON cannot construct a staging capability.
#[derive(Clone)]
pub(crate) struct VerifiedManifest {
    version: Version,
    sequence: u64,
    target: Target,
    bytes: u64,
    sha256: [u8; 32],
    issued_at: u64,
    expires_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifiedMetadata {
    pub version: String,
    pub sequence: u64,
    pub target: Target,
    pub bytes: u64,
    pub sha256: String,
}
impl VerifiedManifest {
    fn valid_at(&self, now: u64) -> Result<(), UpdateError> {
        if self.issued_at > now.saturating_add(300) || self.expires_at <= now {
            Err(UpdateError::Expired)
        } else {
            Ok(())
        }
    }
    pub(crate) fn metadata(&self) -> VerifiedMetadata {
        VerifiedMetadata {
            version: self.version.to_string(),
            sequence: self.sequence,
            target: self.target,
            bytes: self.bytes,
            sha256: self.sha256.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}
impl Target {
    pub(crate) fn current() -> Result<Self, UpdateError> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Ok(Self::LinuxX86_64),
            ("linux", "aarch64") => Ok(Self::LinuxAarch64),
            ("windows", "x86_64") => Ok(Self::WindowsX86_64),
            ("windows", "aarch64") => Ok(Self::WindowsAarch64),
            ("macos", "x86_64") => Ok(Self::MacosX86_64),
            ("macos", "aarch64") => Ok(Self::MacosAarch64),
            _ => Err(UpdateError::Platform),
        }
    }
}

pub(crate) fn verify_manifest(
    raw: &[u8],
    signature: &[u8],
    policy: &UpdatePolicy,
    context: &UpdateContext,
) -> Result<VerifiedManifest, UpdateError> {
    let trust = policy.trust.as_ref().ok_or(UpdateError::Disabled)?;
    if raw.is_empty() || raw.len() > MAX_MANIFEST_BYTES {
        return Err(UpdateError::ManifestTooLarge);
    }
    if signature.len() != 64 {
        return Err(UpdateError::Signature);
    }
    let mut message = Vec::with_capacity(DOMAIN.len() + raw.len());
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(raw);
    UnparsedPublicKey::new(&ED25519, trust.public_key)
        .verify(&message, signature)
        .map_err(|_| UpdateError::Signature)?;
    let manifest: Manifest = serde_json::from_slice(raw).map_err(|_| UpdateError::Format)?;
    if manifest.manifest_schema != 1
        || manifest.application != "self-agent"
        || manifest.channel != "stable"
    {
        return Err(UpdateError::Format);
    }
    if manifest.source != trust.source || manifest.key_id != trust.key_id {
        return Err(UpdateError::Source);
    }
    if manifest.target != context.target {
        return Err(UpdateError::Platform);
    }
    if manifest.database_schema_min > manifest.database_schema_max
        || context.schema < manifest.database_schema_min
        || context.schema > manifest.database_schema_max
    {
        return Err(UpdateError::Schema);
    }
    let version = stable_version(&manifest.version)?;
    if version.cmp_precedence(&context.current) != Ordering::Greater
        || version.cmp_precedence(&context.highest_accepted) != Ordering::Greater
        || manifest.sequence <= context.sequence_floor
    {
        return Err(UpdateError::Downgrade);
    }
    if manifest.issued_at > context.now.saturating_add(300)
        || manifest.expires_at <= context.now
        || manifest.expires_at <= manifest.issued_at
        || manifest.expires_at - manifest.issued_at > MAX_MANIFEST_LIFETIME
    {
        return Err(UpdateError::Expired);
    }
    let artifact = manifest.artifact;
    if artifact.bytes == 0 || artifact.bytes > MAX_ARTIFACT_BYTES {
        return Err(UpdateError::Size);
    }
    // The signed remote basename is validated but NEVER used as a local path.
    if artifact.name.is_empty()
        || artifact.name.len() > 128
        || !artifact
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || artifact.name.starts_with('.')
        || artifact.name.contains("..")
    {
        return Err(UpdateError::Format);
    }
    let sha256 = parse_digest(&artifact.sha256)?;
    Ok(VerifiedManifest {
        version,
        sequence: manifest.sequence,
        target: manifest.target,
        bytes: artifact.bytes,
        sha256,
        issued_at: manifest.issued_at,
        expires_at: manifest.expires_at,
    })
}
fn parse_digest(text: &str) -> Result<[u8; 32], UpdateError> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(UpdateError::Digest);
    }
    let mut bytes = [0; 32];
    for (i, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |b: u8| {
            if b.is_ascii_digit() {
                b - b'0'
            } else {
                b - b'a' + 10
            }
        };
        bytes[i] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(bytes)
}
fn check(deadline: Instant, cancelled: &(impl Fn() -> bool + Sync)) -> Result<(), UpdateError> {
    if cancelled() {
        Err(UpdateError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(UpdateError::Deadline)
    } else {
        Ok(())
    }
}
async fn io_wait<F: Future<Output = io::Result<T>>, T>(
    future: F,
    deadline: Instant,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<T, UpdateError> {
    check(deadline, cancelled)?;
    tokio::pin!(future);
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! { biased;
            _ = tokio::time::sleep_until(deadline.into()) => return Err(UpdateError::Deadline),
            _ = tick.tick() => { check(deadline,cancelled)?; },
            result = &mut future => return result.map_err(|_|UpdateError::Io),
        }
    }
}
struct Pending {
    dir: Dir,
    part: String,
    ready: String,
    part_owned: bool,
    published: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        // All file operations have returned before cleanup; no Tokio filesystem
        // job is detached by cancellation. Removal can still fail, so the owner
        // verifies absence before asserting physical settlement. Orphans are
        // never installable and no recovery path promotes them.
        if self.part_owned {
            let _ = self.dir.remove_file(&self.part);
        }
        if self.published {
            let _ = self.dir.remove_file(&self.ready);
        }
    }
}
/// Private verified bytes retained only for this process. Drop discards the
/// staged file. There is deliberately no public path/installer or auto-recovery
/// method. Crash leftovers require a fresh signed verification, never promotion.
pub(crate) struct StagedArtifact {
    dir: Dir,
    name: String,
    manifest: VerifiedManifest,
    retained: bool,
}
#[cfg(test)]
#[derive(Serialize)]
pub(crate) struct StagedSummary {
    pub version: String,
    pub sequence: u64,
    pub target: Target,
    pub bytes: u64,
    pub installed: bool,
}
impl StagedArtifact {
    pub(crate) fn valid_at(&self, now: u64) -> Result<(), UpdateError> {
        self.manifest.valid_at(now)
    }
    /// Private basename only, never a remote-supplied path or installer command.
    #[cfg(test)]
    pub(crate) fn private_name(&self) -> &str {
        &self.name
    }
    /// Runtime may retain bytes only after floor witness + authoritative receipt
    /// have committed. This changes file retention, not install/execution rights.
    pub(crate) fn retain(mut self) {
        self.retained = true;
    }
    #[cfg(test)]
    pub(crate) fn summary(&self) -> StagedSummary {
        StagedSummary {
            version: self.manifest.version.to_string(),
            sequence: self.manifest.sequence,
            target: self.manifest.target,
            bytes: self.manifest.bytes,
            installed: false,
        }
    }
}
impl Drop for StagedArtifact {
    fn drop(&mut self) {
        if !self.retained {
            let _ = self.dir.remove_file(&self.name);
        }
    }
}

/// Only authorized, already nofollow-opened directory capabilities may be
/// supplied. Local writes are randomized create_new files, never server names.
/// An untrusted downloader may feed the reader: exact signed length and hash are
/// both checked, including one byte beyond the announced length and EOF.
#[cfg(test)]
pub(crate) async fn stage_artifact<R: AsyncRead + Unpin>(
    source: R,
    manifest: VerifiedManifest,
    staging: &Dir,
    deadline: Instant,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<StagedArtifact, UpdateError> {
    stage_artifact_for(
        &uuid::Uuid::new_v4().to_string(),
        source,
        manifest,
        staging,
        deadline,
        cancelled,
    )
    .await
}
/// An operation UUID fixes the private name; retries cannot replace existing
/// bytes. Only the previously admitted runtime operation may supply this ID.
pub(crate) async fn stage_artifact_for<R: AsyncRead + Unpin>(
    operation_id: &str,
    source: R,
    manifest: VerifiedManifest,
    staging: &Dir,
    deadline: Instant,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<StagedArtifact, UpdateError> {
    stage_artifact_inner(
        operation_id,
        source,
        manifest,
        staging,
        deadline,
        cancelled,
        #[cfg(test)]
        None,
    )
    .await
}
/// Must run in the caller's retained blocking worker; never detach this future.
async fn stage_artifact_inner<R: AsyncRead + Unpin>(
    operation_id: &str,
    mut source: R,
    manifest: VerifiedManifest,
    staging: &Dir,
    deadline: Instant,
    cancelled: &(impl Fn() -> bool + Sync),
    #[cfg(test)] after_write: Option<&(dyn Fn() + Sync)>,
) -> Result<StagedArtifact, UpdateError> {
    let id = uuid::Uuid::parse_str(operation_id).map_err(|_| UpdateError::Format)?;
    if id.to_string() != operation_id {
        return Err(UpdateError::Format);
    }
    let deadline = deadline.min(Instant::now() + MAX_STAGE_TIME);
    check(deadline, cancelled)?;
    let mut pending = Pending {
        dir: staging.try_clone().map_err(|_| UpdateError::Io)?,
        part: format!(".update-{id}.part"),
        ready: format!(".update-{id}.verified"),
        part_owned: false,
        published: false,
    };
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = pending
        .dir
        .open_with(&pending.part, &options)
        .map_err(|_| UpdateError::Io)?;
    pending.part_owned = true;
    let mut file = file;
    let mut buffer = vec![0; CHUNK_BYTES];
    let mut received = 0u64;
    let mut digest = Sha256::new();
    loop {
        check(deadline, cancelled)?;
        let allowed = (manifest.bytes - received + 1).min(CHUNK_BYTES as u64) as usize;
        let count = io_wait(source.read(&mut buffer[..allowed]), deadline, cancelled).await?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or(UpdateError::Size)?;
        if received > manifest.bytes {
            return Err(UpdateError::Size);
        }
        digest.update(&buffer[..count]);
        check(deadline, cancelled)?;
        file.write_all(&buffer[..count])
            .map_err(|_| UpdateError::Io)?;
        #[cfg(test)]
        if let Some(hook) = after_write {
            hook();
        }
        check(deadline, cancelled)?;
    }
    if received != manifest.bytes {
        return Err(UpdateError::Size);
    }
    if <[u8; 32]>::from(digest.finalize()) != manifest.sha256 {
        return Err(UpdateError::Digest);
    }
    check(deadline, cancelled)?;
    file.flush().map_err(|_| UpdateError::Io)?;
    check(deadline, cancelled)?;
    file.sync_all().map_err(|_| UpdateError::Io)?;
    check(deadline, cancelled)?;
    // Verify the actual staged file through the same open handle, not only the
    // bytes delivered by the source. No ambient path is reopened for readback.
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|_| UpdateError::Io)?;
    let mut stored = 0u64;
    let mut stored_digest = Sha256::new();
    loop {
        let allowed = (manifest.bytes - stored + 1).min(CHUNK_BYTES as u64) as usize;
        check(deadline, cancelled)?;
        let count = file
            .read(&mut buffer[..allowed])
            .map_err(|_| UpdateError::Io)?;
        if count == 0 {
            break;
        }
        stored += count as u64;
        if stored > manifest.bytes {
            return Err(UpdateError::Size);
        }
        stored_digest.update(&buffer[..count]);
    }
    if stored != manifest.bytes {
        return Err(UpdateError::Size);
    }
    if <[u8; 32]>::from(stored_digest.finalize()) != manifest.sha256 {
        return Err(UpdateError::Digest);
    }
    drop(file);
    check(deadline, cancelled)?;
    // Atomic no-replace publication. A collision cannot overwrite another file.
    pending
        .dir
        .hard_link(&pending.part, &pending.dir, &pending.ready)
        .map_err(|_| UpdateError::Io)?;
    pending.published = true;
    pending
        .dir
        .remove_file(&pending.part)
        .map_err(|_| UpdateError::Io)?;
    #[cfg(unix)]
    {
        // cap_std's directory capability can be O_PATH on Linux; fsync needs
        // a readable fd opened relative to that same pinned capability.
        let directory = pending.dir.open(".").map_err(|_| UpdateError::Io)?;
        check(deadline, cancelled)?;
        directory.sync_all().map_err(|_| UpdateError::Io)?;
    }
    check(deadline, cancelled)?;
    let artifact = StagedArtifact {
        dir: pending.dir.try_clone().map_err(|_| UpdateError::Io)?,
        name: pending.ready.clone(),
        manifest,
        retained: false,
    };
    pending.published = false;
    Ok(artifact)
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
