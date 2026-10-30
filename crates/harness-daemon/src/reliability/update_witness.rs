//! Private anti-rollback high-watermarks outside the task SQLite backup.
//! These records authorize no execution or install. The trusted runtime writes
//! one only after signature + actual staged-byte verification, before committing
//! the success receipt. A crash can conservatively consume a version/sequence;
//! it can never lower the floor or promote an orphan artifact.
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

pub(crate) const MAX_WITNESSES: usize = 256;
const MAX_RECORD: u64 = 2048;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FloorRecord {
    pub schema: u32,
    pub operation_id: String,
    pub claim_nonce: String,
    pub target: String,
    pub version: String,
    pub sequence: u64,
    pub manifest_sha256: String,
    pub artifact_sha256: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WitnessError {
    Invalid,
    Capacity,
    Io,
}
impl std::fmt::Display for WitnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "更新高水位记录无效；校验已阻断，未降低回退保护",
            Self::Capacity => "更新高水位记录达到保留上限；未自动删除历史",
            Self::Io => "更新高水位读写未确认；未安装或执行产物",
        })
    }
}
impl std::error::Error for WitnessError {}
fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn stable(s: &str) -> Result<Version, WitnessError> {
    if s.len() > 64 {
        return Err(WitnessError::Invalid);
    }
    let v = Version::parse(s).map_err(|_| WitnessError::Invalid)?;
    if !v.pre.is_empty() || !v.build.is_empty() {
        return Err(WitnessError::Invalid);
    }
    Ok(v)
}
impl FloorRecord {
    pub(crate) fn validate(&self) -> Result<(), WitnessError> {
        if self.schema != 1
            || uuid::Uuid::parse_str(&self.operation_id)
                .map(|x| x.to_string() != self.operation_id)
                .unwrap_or(true)
            || uuid::Uuid::parse_str(&self.claim_nonce)
                .map(|x| x.to_string() != self.claim_nonce)
                .unwrap_or(true)
            || self.sequence == 0
            || !valid_digest(&self.manifest_sha256)
            || !valid_digest(&self.artifact_sha256)
            || !matches!(
                self.target.as_str(),
                "linux-x86_64"
                    | "linux-aarch64"
                    | "windows-x86_64"
                    | "windows-aarch64"
                    | "macos-x86_64"
                    | "macos-aarch64"
            )
        {
            return Err(WitnessError::Invalid);
        }
        stable(&self.version)?;
        Ok(())
    }
    fn name(&self) -> String {
        format!("floor-{}.json", self.operation_id)
    }
}
fn read(dir: &Dir, name: &str) -> Result<FloorRecord, WitnessError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = dir
        .open_with(name, &options)
        .map_err(|_| WitnessError::Io)?;
    if !file.metadata().map_err(|_| WitnessError::Io)?.is_file() {
        return Err(WitnessError::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| WitnessError::Io)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(WitnessError::Invalid);
    }
    let record: FloorRecord = serde_json::from_slice(&bytes).map_err(|_| WitnessError::Invalid)?;
    record.validate()?;
    if record.name() != name {
        return Err(WitnessError::Invalid);
    }
    Ok(record)
}
/// Invalid/partial/linked records block verification; startup does not silently
/// discard them or claim that the old DB floor is enough after a restore.
pub(crate) fn load(dir: &Dir) -> Result<Vec<FloorRecord>, WitnessError> {
    let mut records = Vec::new();
    for entry in dir.entries().map_err(|_| WitnessError::Io)? {
        if records.len() >= MAX_WITNESSES {
            return Err(WitnessError::Capacity);
        }
        let entry = entry.map_err(|_| WitnessError::Io)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| WitnessError::Invalid)?;
        if !name.starts_with("floor-") || !name.ends_with(".json") {
            return Err(WitnessError::Invalid);
        }
        records.push(read(dir, &name)?);
    }
    floors(&records)?;
    Ok(records)
}
/// Componentwise monotonicity prevents a larger sequence hiding a lower version
/// (or the reverse). Historical dominated records are normal, not corruption.
pub(crate) fn floors(
    records: &[FloorRecord],
) -> Result<BTreeMap<String, FloorRecord>, WitnessError> {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| r.sequence);
    let mut result: BTreeMap<String, FloorRecord> = BTreeMap::new();
    for record in sorted {
        record.validate()?;
        if let Some(prior) = result.get(&record.target) {
            let comparison = stable(&record.version)?.cmp_precedence(&stable(&prior.version)?);
            if record.sequence == prior.sequence {
                if comparison != std::cmp::Ordering::Equal
                    || record.manifest_sha256 != prior.manifest_sha256
                    || record.artifact_sha256 != prior.artifact_sha256
                {
                    return Err(WitnessError::Invalid);
                }
            } else if comparison != std::cmp::Ordering::Greater {
                return Err(WitnessError::Invalid);
            }
        }
        result.insert(record.target.clone(), record);
    }
    Ok(result)
}
/// No replace / no erase. Caller serializes this against all other update
/// finalizations using the application update gate and retains the data lease.
/// A partial write is deliberately left as a blocking recovery signal.
pub(crate) fn append(dir: &Dir, record: &FloorRecord) -> Result<(), WitnessError> {
    record.validate()?;
    let mut records = load(dir)?;
    if let Some(old) = records
        .iter()
        .find(|r| r.operation_id == record.operation_id)
    {
        return if old == record {
            Ok(())
        } else {
            Err(WitnessError::Invalid)
        };
    }
    if records.len() >= MAX_WITNESSES {
        return Err(WitnessError::Capacity);
    }
    records.push(record.clone());
    floors(&records)?;
    let bytes = serde_json::to_vec(record).map_err(|_| WitnessError::Invalid)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(WitnessError::Invalid);
    }
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = dir
        .open_with(record.name(), &options)
        .map_err(|_| WitnessError::Io)?;
    file.write_all(&bytes).map_err(|_| WitnessError::Io)?;
    file.sync_all().map_err(|_| WitnessError::Io)?;
    drop(file);
    #[cfg(unix)]
    {
        dir.open(".")
            .map_err(|_| WitnessError::Io)?
            .sync_all()
            .map_err(|_| WitnessError::Io)?;
    }
    if read(dir, &record.name())? != *record {
        return Err(WitnessError::Invalid);
    }
    Ok(())
}
#[cfg(test)]
#[path = "update_witness_tests.rs"]
mod tests;
