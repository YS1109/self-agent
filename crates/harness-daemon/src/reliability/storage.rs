//! Local storage reliability primitives. Callers must hold the daemon data-dir
//! lease before opening Store or restoring. Backup bundles contain private user
//! history; SHA-256 checks corruption, not provenance or authenticity.
//!
//! SQLite backup/serialization avoids copying a live main/WAL pair or opening a
//! writable SQLite connection through a staging pathname. The 128 MiB limit is
//! a database-image limit, not a claim about SQLite/process peak RSS.
use anyhow::{bail, Context, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use fs2::FileExt;
use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OptionalExtension, Transaction, TransactionBehavior, MAIN_DB,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) const SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_DATABASE_BYTES: usize = 128 * 1024 * 1024;
const LEDGER_ID: &str = "self-agent-schema-v1-atomic-baseline";
const MAIN: &str = "harness.sqlite";
const JOURNAL: &str = ".restore-journal.json";
const COMMIT: &str = ".restore-committed.json";
const NEW: &str = ".restore-new.sqlite";
const OLD: &str = ".restore-old.sqlite";
const ROLLBACK: &str = ".restore-rollback.sqlite";
const ORIGINAL: &str = ".restore-original.sqlite";
const ORIGINAL_WAL: &str = ".restore-original.sqlite-wal";
const ORIGINAL_SHM: &str = ".restore-original.sqlite-shm";
const SCRATCH: &[&str] = &[NEW, OLD, ROLLBACK, ORIGINAL, ORIGINAL_WAL, ORIGINAL_SHM];

/// Must precede journal_mode, schema creation, repair, and all data writes.
/// A legacy database has user_version=0 and no application migration ledger.
pub(crate) fn preflight(db: &Connection) -> Result<u32> {
    preflight_inner(db)
        .map_err(|_| anyhow::anyhow!("数据库版本不兼容、损坏或迁移记录无效；未改写数据"))
}
fn preflight_inner(db: &Connection) -> Result<u32> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("数据库来自更新版本；未执行迁移或修复");
    }
    let exists = table_exists(db, "storage_migrations")?;
    if version == 0 {
        if exists {
            bail!("数据库版本与迁移记录不一致；未修改原数据");
        }
    } else {
        if !exists {
            bail!("数据库迁移记录缺失；未修改原数据");
        }
        let records = db
            .prepare("SELECT version,identity FROM storage_migrations ORDER BY version")?
            .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if records != vec![(SCHEMA_VERSION, LEDGER_ID.into())] {
            bail!("数据库迁移记录不受支持；未修改原数据");
        }
    }
    Ok(version)
}

/// Only compiled, trusted schema code may be passed here. It must not BEGIN or
/// COMMIT itself. Version 1 is the atomic baseline of the pre-ledger schemas.
pub(crate) fn migrate(
    db: &mut Connection,
    schema: impl FnOnce(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    migrate_inner(db, schema)
        .map_err(|_| anyhow::anyhow!("数据库迁移失败；事务已回滚，未重建或删除原数据"))
}
fn migrate_inner(
    db: &mut Connection,
    schema: impl FnOnce(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    let version = preflight(db)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    preflight(&tx)?;
    schema(&tx)?;
    tx.execute_batch("CREATE TABLE storage_migrations(version INTEGER PRIMARY KEY,identity TEXT NOT NULL); PRAGMA user_version=1;")?;
    tx.execute(
        "INSERT INTO storage_migrations(version,identity) VALUES(?1,?2)",
        rusqlite::params![SCHEMA_VERSION, LEDGER_ID],
    )?;
    tx.commit()?;
    Ok(())
}
fn table_exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(db
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1",
            [name],
            |r| r.get::<_, i32>(0),
        )
        .optional()?
        .is_some())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn image_size(db: &Connection) -> Result<usize> {
    let pages: u64 = db.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let size: u64 = db.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    bounded_size(pages, size)
}
fn bounded_size(pages: u64, size: u64) -> Result<usize> {
    let bytes = pages.checked_mul(size).context("数据库大小无效")?;
    if size == 0 || size > 65536 || bytes > MAX_DATABASE_BYTES as u64 {
        bail!("数据库超过 128 MiB 备份恢复预算");
    }
    Ok(bytes as usize)
}
fn validate_import(db: &Connection) -> Result<()> {
    preflight(db)?;
    image_size(db)?;
    let unsupported:u32=db.query_row("SELECT COUNT(*) FROM pragma_table_list WHERE schema='main' AND type NOT IN ('table','view')",[],|r|r.get(0))?;
    if unsupported != 0 {
        bail!("备份包含不受支持的虚拟表或影子表");
    }
    let mut stmt =
        db.prepare("SELECT type,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (kind, sql) = row?;
        if !matches!(kind.as_str(), "table" | "index")
            || sql.is_some_and(|sql| sql.to_ascii_uppercase().contains("CREATE VIRTUAL TABLE"))
        {
            bail!("备份包含不受支持的触发器、视图或虚拟表；未恢复");
        }
    }
    let result: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if result != "ok" {
        bail!("备份数据库完整性校验失败");
    }
    // integrity_check does not validate referential integrity. Check the same
    // isolated image before migration, after invalidation and before publish.
    if db
        .prepare("PRAGMA foreign_key_check")?
        .query([])?
        .next()?
        .is_some()
    {
        bail!("备份数据库外键完整性校验失败");
    }
    Ok(())
}

/// Consistent online snapshot. The source connection must not hold a write
/// transaction. Each 64-page step rechecks SQLite's total page count, deadline,
/// and cancellation; growth can exceed the limit by at most one bounded step
/// before the temporary in-memory destination is discarded.
pub(crate) fn consistent_image(
    source: &Connection,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>> {
    consistent_image_inner(source, deadline, cancelled)
        .map_err(|_| anyhow::anyhow!("数据库快照失败、已取消或超过 128 MiB 预算；未发布不完整备份"))
}
fn check_backup(deadline: Instant, cancelled: &impl Fn() -> bool) -> Result<()> {
    if cancelled() || Instant::now() >= deadline {
        bail!("备份已取消或到期；未发布不完整备份");
    }
    Ok(())
}
fn consistent_image_inner(
    source: &Connection,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>> {
    check_backup(deadline, &cancelled)?;
    preflight(source)?;
    image_size(source)?;
    let page_size: u64 = source.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let mut memory = Connection::open_in_memory()?;
    memory.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA cache_size=-2048;")?;
    {
        let backup = Backup::new(source, &mut memory)?;
        loop {
            if cancelled() || Instant::now() >= deadline {
                bail!("备份已取消或到期；未发布不完整备份");
            }
            let step = backup.step(64)?;
            let progress = backup.progress();
            if progress.pagecount < 0
                || (progress.pagecount as u64).saturating_mul(page_size) > MAX_DATABASE_BYTES as u64
            {
                bail!("数据库超过 128 MiB 备份预算");
            }
            match step {
                StepResult::Done => break,
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                StepResult::More => {}
                _ => bail!("备份状态不受支持"),
            }
        }
    }
    // A standalone image must not request the source's live WAL sidecar.
    memory.execute_batch("PRAGMA journal_mode=MEMORY;")?;
    check_backup(deadline, &cancelled)?;
    validate_import(&memory)?;
    check_backup(deadline, &cancelled)?;
    let data = memory.serialize(MAIN_DB)?;
    if data.len() > MAX_DATABASE_BYTES {
        bail!("数据库超过 128 MiB 备份预算");
    }
    check_backup(deadline, &cancelled)?;
    let mut bytes = data.to_vec();
    check_backup(deadline, &cancelled)?;
    if bytes.len() < 100 || &bytes[..16] != b"SQLite format 3\0" {
        bail!("数据库映像格式无效");
    }
    // SQLite memory serialization may retain WAL read/write-version bytes from
    // the source header. The completed backup contains all pages; mark it as a
    // standalone rollback-journal image, never associate external WAL contents.
    bytes[18] = 1;
    bytes[19] = 1;
    Ok(bytes)
}
fn deserialize(bytes: &[u8]) -> Result<Connection> {
    if bytes.len() < 100
        || bytes.len() > MAX_DATABASE_BYTES
        || &bytes[..16] != b"SQLite format 3\0"
        || bytes[18] != 1
        || bytes[19] != 1
    {
        bail!("备份必须是有界独立 SQLite 映像");
    }
    let mut db = Connection::open_in_memory()?;
    // Cursor length is exactly the validated size, so read_exact cannot produce
    // partial buffers. rusqlite owns/frees SQLite's allocated deserialization.
    db.deserialize_read_exact(MAIN_DB, std::io::Cursor::new(bytes), bytes.len(), false)?;
    db.execute_batch(
        "PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA cache_size=-2048;",
    )?;
    validate_import(&db)?;
    Ok(db)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackupManifest {
    pub format: u32,
    pub schema: u32,
    pub bytes: usize,
    pub sha256: String,
}
impl BackupManifest {
    fn for_bytes(bytes: &[u8], schema: u32) -> Self {
        Self {
            format: 1,
            schema,
            bytes: bytes.len(),
            sha256: digest(bytes),
        }
    }
    fn verify(&self, bytes: &[u8]) -> Result<()> {
        if self.format != 1
            || self.schema > SCHEMA_VERSION
            || self.bytes > MAX_DATABASE_BYTES
            || self.bytes != bytes.len()
            || !valid_digest(&self.sha256)
            || digest(bytes) != self.sha256
        {
            bail!("备份摘要、格式或大小校验失败");
        }
        Ok(())
    }
}

fn open_directory(path: &Path) -> Result<Dir> {
    open_directory_inner(path, false)
}
fn open_directory_inner(path: &Path, create_missing: bool) -> Result<Dir> {
    if !path.is_absolute() {
        bail!("存储目录必须为绝对路径");
    }
    #[cfg(windows)]
    {
        let drive = match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => d,
                _ => bail!("存储仅支持本地固定磁盘"),
            },
            _ => bail!("存储仅支持本地固定磁盘"),
        };
        let root = [drive as u16, b':' as u16, b'\\' as u16, 0];
        // A mapped SMB drive has a disk-letter prefix too. Query the drive type
        // before any synchronous file open; unknown/removable/network fail closed.
        if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) } != 3 {
            bail!("存储拒绝网络、可移动或未知磁盘");
        }
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                if !matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                ) {
                    bail!("存储不接受远程或设备路径");
                }
                anchor.push(prefix.as_os_str());
            }
            Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => bail!("存储路径不能包含上级组件"),
        }
    }
    let mut dir = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?;
    for name in names {
        dir = match dir.open_dir_nofollow(&name) {
            Ok(child) => child,
            Err(error) if create_missing && error.kind() == std::io::ErrorKind::NotFound => {
                #[allow(unused_mut)]
                let mut builder = cap_std::fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use cap_std::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match dir.create_dir_with(&name, &builder) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                dir.open_dir_nofollow(&name)
                    .context("新建存储目录不可访问")?
            }
            Err(error) => return Err(error).context("存储目录含链接或不可访问"),
        };
    }
    Ok(dir)
}
fn file_identity(file: &cap_std::fs::File) -> Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: borrowed live file handle and valid writable output structure.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            bail!("无法核对存储文件身份");
        }
        Ok((
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        bail!("当前平台无法核对文件身份")
    }
}
fn directory_identity(dir: &Dir) -> Result<(u64, u64)> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    file_identity(&dir.open_with(".", &options)?)
}
fn read_file(dir: &Dir, name: &str, limit: usize) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        // Opening a FIFO must not wait for a writer before the descriptor's
        // type can be checked. Regular-file reads remain unchanged.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = dir.open_with(name, &options)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit as u64 {
        bail!("存储文件类型或大小不受支持");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("存储文件超过大小限制");
    }
    Ok(bytes)
}
fn write_private(dir: &Dir, name: &str, bytes: &[u8]) -> Result<()> {
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
    let mut file = dir.open_with(name, &options)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn sync_directory(dir: &Dir) -> Result<()> {
    // Unix supports directory fsync. Windows file contents are flushed and all
    // handles are closed before rename; no claim of power-loss directory flush.
    #[cfg(unix)]
    {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        dir.open_with(".", &options)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
fn exists(dir: &Dir, name: &str) -> Result<bool> {
    match dir.symlink_metadata(name) {
        Ok(meta) => {
            if !meta.is_file() {
                bail!("存储保留路径不是普通文件");
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn remove_if_present(dir: &Dir, name: &str) -> Result<()> {
    if exists(dir, name)? {
        dir.remove_file(name)?;
    }
    Ok(())
}

/// Creates a new owner-selected bundle directory; never overwrites an existing
/// bundle. Payload is private history, not a sanitized diagnostic export.
#[cfg(test)]
pub(crate) fn export_backup(
    source: &Connection,
    parent: &Path,
    name: &str,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<BackupManifest> {
    export_backup_inner(source, parent, name, deadline, cancelled)
        .map_err(|_| anyhow::anyhow!("备份导出失败；未覆盖现有备份或原数据库"))
}
#[cfg(test)]
fn export_backup_inner(
    source: &Connection,
    parent: &Path,
    name: &str,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<BackupManifest> {
    let bytes = consistent_image(source, deadline, &cancelled)?;
    let schema = preflight(source)?;
    publish_image(&bytes, schema, parent, name, deadline, &cancelled)
}
fn publish_image(
    bytes: &[u8],
    schema: u32,
    parent: &Path,
    name: &str,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<BackupManifest> {
    if name.is_empty()
        || name.len() > 80
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        bail!("备份名称仅允许字母、数字、连字符与下划线");
    }
    let manifest = BackupManifest::for_bytes(bytes, schema);
    let parent = open_directory(parent)?;
    check_backup(deadline, &cancelled)?;
    parent.create_dir(name)?;
    let result = (|| -> Result<()> {
        let dir = parent.open_dir_nofollow(name)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            dir.set_permissions(
                ".",
                cap_std::fs::Permissions::from_std(std::fs::Permissions::from_mode(0o700)),
            )?;
        }
        write_private(&dir, "database.sqlite", bytes)?;
        check_backup(deadline, &cancelled)?;
        write_private(&dir, "manifest.json", &serde_json::to_vec(&manifest)?)?;
        sync_directory(&dir)?;
        sync_directory(&parent)?;
        check_backup(deadline, &cancelled)?;
        Ok(())
    })();
    if result.is_err() {
        // Only this newly-created bundle is removed; existing bundles are never touched.
        let _ = parent.remove_dir_all(name);
        let _ = sync_directory(&parent);
    }
    result?;
    Ok(manifest)
}
pub(crate) fn inspect_backup(path: &Path) -> Result<BackupManifest> {
    inspect_backup_inner(path).map_err(|_| anyhow::anyhow!("备份格式、完整性、版本或路径检查失败"))
}
fn inspect_backup_inner(path: &Path) -> Result<BackupManifest> {
    let dir = open_directory(path)?;
    let manifest: BackupManifest =
        serde_json::from_slice(&read_file(&dir, "manifest.json", 4096)?)?;
    let bytes = read_file(&dir, "database.sqlite", MAX_DATABASE_BYTES)?;
    manifest.verify(&bytes)?;
    let db = deserialize(&bytes)?;
    if preflight(&db)? != manifest.schema {
        bail!("备份声明与数据库版本不一致");
    }
    Ok(manifest)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreJournal {
    format: u32,
    new: BackupManifest,
    old: Option<BackupManifest>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryOutcome {
    None,
    RolledBack,
    Completed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestorePoint {
    JournalDurable,
    OldImageDurable,
    NewImageDurable,
    OriginalMoved,
    WalMoved,
    ShmMoved,
    Installed,
    Committed,
}

/// Same lock as daemon startup. Keep this guard alive for the daemon lifetime;
/// call recover_pending BEFORE opening the writable Store. CLI restore cannot
/// acquire it while a cooperating daemon is alive.
pub(crate) struct StorageGuard {
    dir: Dir,
    root_path: PathBuf,
    _lock: std::fs::File,
}
impl StorageGuard {
    #[cfg(test)]
    pub(crate) fn acquire(directory: &Path) -> Result<Self> {
        Self::acquire_inner(directory, false)
            .map_err(|_| anyhow::anyhow!("无法取得本机数据目录独占锁；未开始恢复"))
    }
    /// Create missing absolute directory components without following links.
    /// New Unix directories are private (0700); existing permissions are not changed.
    pub(crate) fn create_and_acquire(directory: &Path) -> Result<Self> {
        Self::acquire_inner(directory, true)
            .map_err(|_| anyhow::anyhow!("无法创建或锁定本机数据目录；未打开数据库"))
    }
    pub(crate) fn database_path(&self) -> PathBuf {
        self.root_path.join(MAIN)
    }
    pub(crate) fn startup_identity(&self) -> Result<Option<(u64, u64)>> {
        self.check_source_identity(None)
    }
    pub(crate) fn validate_source_identity(&self, expected: (u64, u64)) -> Result<()> {
        self.check_source_identity(Some(&Some(expected)))?;
        Ok(())
    }
    /// Opens the writable handle only after a fresh read-only preflight and
    /// verifies the pinned namespace again before any Store migration/recovery.
    /// The caller must not reopen the returned path independently.
    pub(crate) fn open_database(
        &self,
        expected: &Option<(u64, u64)>,
    ) -> Result<(Connection, (u64, u64))> {
        self.check_source_identity(Some(expected))?;
        self.preflight_startup()?;
        self.check_source_identity(Some(expected))?;
        let db = Connection::open_with_flags(
            self.database_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let identity = if expected.is_some() {
            self.check_source_identity(Some(expected))?
        } else {
            // This handle created a new database, so a new ordinary file is
            // expected; the pinned directory must still be the same directory.
            self.check_source_identity(None)?
        }
        .context("新建数据库身份不可确认")?;
        preflight(&db)?;
        self.validate_source_identity(identity)?;
        Ok((db, identity))
    }
    pub(crate) fn probe_directory(&self) -> Result<PathBuf> {
        self.check_source_identity(None)?;
        match self.dir.create_dir("capability-probes") {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        self.dir.open_dir_nofollow("capability-probes")?;
        self.check_source_identity(None)?;
        Ok(self.root_path.join("capability-probes"))
    }
    pub(crate) fn write_daemon_metadata(&self, content: &[u8]) -> Result<()> {
        self.check_source_identity(None)?;
        let name = format!("daemon-{}.tmp", uuid::Uuid::new_v4());
        write_private(&self.dir, &name, content)?;
        #[cfg(windows)]
        if exists(&self.dir, "daemon.json")? {
            self.dir.remove_file("daemon.json")?;
        }
        self.dir.rename(&name, &self.dir, "daemon.json")?;
        sync_directory(&self.dir)?;
        self.check_source_identity(None)?;
        Ok(())
    }
    /// Uses a separate read-only connection, not the live Store mutex. Identity
    /// checks finish before publication; the source WAL remains authoritative.
    #[cfg(test)]
    pub(crate) fn export_backup(
        &self,
        parent: &Path,
        name: &str,
        deadline: Instant,
        cancelled: impl Fn() -> bool,
    ) -> Result<BackupManifest> {
        let identity = self.startup_identity()?.context("原数据库不存在")?;
        self.export_backup_from_identity(identity, parent, name, deadline, cancelled)
    }
    pub(crate) fn export_backup_from_identity(
        &self,
        expected: (u64, u64),
        parent: &Path,
        name: &str,
        deadline: Instant,
        cancelled: impl Fn() -> bool,
    ) -> Result<BackupManifest> {
        (|| -> Result<BackupManifest> {
            check_backup(deadline, &cancelled)?;
            let identity = Some(expected);
            self.check_source_identity(Some(&identity))?;
            let source = Connection::open_with_flags(
                self.database_path(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            self.check_source_identity(Some(&identity))?;
            let bytes = consistent_image(&source, deadline, &cancelled)?;
            let schema = preflight(&source)?;
            self.check_source_identity(Some(&identity))?;
            drop(source);
            check_backup(deadline, &cancelled)?;
            publish_image(&bytes, schema, parent, name, deadline, &cancelled)
        })()
        .map_err(|_| anyhow::anyhow!("在线备份失败、已取消或源目录发生变化；未确认完成"))
    }
    fn acquire_inner(directory: &Path, create_missing: bool) -> Result<Self> {
        let dir = open_directory_inner(directory, create_missing)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = dir.open_with("daemon.lock", &options)?.into_std();
        if !lock.metadata()?.is_file() {
            bail!("后台锁不是普通文件");
        }
        lock.try_lock_exclusive()
            .context("后台仍在使用数据目录；离线恢复未开始")?;
        Ok(Self {
            dir,
            root_path: directory.to_owned(),
            _lock: lock,
        })
    }
    pub(crate) fn recover_pending(&self) -> Result<RecoveryOutcome> {
        self.recover_pending_inner().map_err(|_| {
            anyhow::anyhow!("中断恢复材料不完整或不一致；保留原数据，未打开后台数据库")
        })
    }
    fn recover_pending_inner(&self) -> Result<RecoveryOutcome> {
        if !exists(&self.dir, JOURNAL)? {
            if exists(&self.dir, COMMIT)? {
                let marker = read_file(&self.dir, COMMIT, 128)?;
                let hash = std::str::from_utf8(&marker)?;
                if !valid_digest(hash)
                    || digest(&read_file(&self.dir, MAIN, MAX_DATABASE_BYTES)?) != hash
                    || SCRATCH
                        .iter()
                        .any(|name| exists(&self.dir, name).unwrap_or(true))
                {
                    bail!("恢复提交清理状态不一致；未修改数据库");
                }
                self.dir.remove_file(COMMIT)?;
                sync_directory(&self.dir)?;
                return Ok(RecoveryOutcome::Completed);
            }
            if SCRATCH
                .iter()
                .any(|name| exists(&self.dir, name).unwrap_or(true))
            {
                bail!("存在未识别的恢复材料；未打开或删除用户数据库");
            }
            return Ok(RecoveryOutcome::None);
        }
        let journal: RestoreJournal =
            serde_json::from_slice(&read_file(&self.dir, JOURNAL, 8192)?)?;
        if journal.format != 1
            || !valid_digest(&journal.new.sha256)
            || journal.new.bytes > MAX_DATABASE_BYTES
        {
            bail!("恢复日志格式无效；保留所有原始材料");
        }
        let committed = if exists(&self.dir, COMMIT)? {
            let marker = read_file(&self.dir, COMMIT, 128)?;
            let hash = std::str::from_utf8(&marker)?;
            if !valid_digest(hash) || hash != journal.new.sha256 {
                bail!("恢复提交标记与日志不一致；保留所有原始材料");
            }
            true
        } else {
            false
        };
        if committed {
            journal
                .new
                .verify(&read_file(&self.dir, MAIN, MAX_DATABASE_BYTES)?)?;
            if exists(&self.dir, "harness.sqlite-wal")? || exists(&self.dir, "harness.sqlite-shm")?
            {
                bail!("恢复后的数据库出现未预期边文件；保留恢复材料");
            }
            self.cleanup(true)?;
            return Ok(RecoveryOutcome::Completed);
        }
        let moved = exists(&self.dir, ORIGINAL)?;
        if !moved && journal.old.is_some() {
            // No original rename was attempted: the live original including its
            // WAL/SHM remains untouched. Partial private stages may be discarded.
            if !exists(&self.dir, MAIN)? {
                bail!("原数据库缺失；保留恢复材料供人工检查");
            }
            self.cleanup(false)?;
            return Ok(RecoveryOutcome::RolledBack);
        }
        if let Some(old) = &journal.old {
            if !exists(&self.dir, OLD)? {
                old.verify(&read_file(&self.dir, MAIN, MAX_DATABASE_BYTES)?)?;
                if exists(&self.dir, "harness.sqlite-wal")?
                    || exists(&self.dir, "harness.sqlite-shm")?
                {
                    bail!("回滚数据库出现未知边文件");
                }
                self.cleanup(false)?;
                return Ok(RecoveryOutcome::RolledBack);
            }
            let bytes = read_file(&self.dir, OLD, MAX_DATABASE_BYTES)?;
            old.verify(&bytes)?;
            // Never attach an old WAL to the replacement main. Keep consistent
            // OLD intact until rollback has itself reached a durable state.
            remove_if_present(&self.dir, ROLLBACK)?;
            write_private(&self.dir, ROLLBACK, &bytes)?;
            for name in [MAIN, "harness.sqlite-wal", "harness.sqlite-shm"] {
                remove_if_present(&self.dir, name)?;
            }
            self.dir.rename(ROLLBACK, &self.dir, MAIN)?;
            sync_directory(&self.dir)?;
        } else {
            if exists(&self.dir, MAIN)? {
                journal
                    .new
                    .verify(&read_file(&self.dir, MAIN, MAX_DATABASE_BYTES)?)?;
                self.dir.remove_file(MAIN)?;
            }
            for name in ["harness.sqlite-wal", "harness.sqlite-shm"] {
                if exists(&self.dir, name)? {
                    bail!("无原数据库的恢复出现边文件；未删除未知数据");
                }
            }
        }
        self.cleanup(false)?;
        Ok(RecoveryOutcome::RolledBack)
    }
    fn cleanup(&self, committed: bool) -> Result<()> {
        for name in SCRATCH {
            remove_if_present(&self.dir, name)?;
        }
        if committed {
            // A surviving valid commit marker allows restart to finish cleanup
            // after journal removal without mistaking the new image for old.
            self.dir.remove_file(JOURNAL)?;
            sync_directory(&self.dir)?;
            remove_if_present(&self.dir, COMMIT)?;
        } else {
            remove_if_present(&self.dir, COMMIT)?;
            self.dir.remove_file(JOURNAL)?;
        }
        sync_directory(&self.dir)
    }
    /// Call after recover_pending and BEFORE opening any read-write SQLite
    /// connection. A hot journal requiring writes causes read-only preflight
    /// to fail closed rather than running recovery on a possibly newer DB.
    pub(crate) fn preflight_startup(&self) -> Result<Option<u32>> {
        (|| -> Result<Option<u32>> {
            let identity = self.check_source_identity(None)?;
            if identity.is_none() {
                return Ok(None);
            }
            let db = Connection::open_with_flags(
                self.root_path.join(MAIN),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let version = preflight(&db)?;
            self.check_source_identity(Some(&identity))?;
            Ok(Some(version))
        })()
        .map_err(|_| anyhow::anyhow!("启动前只读版本检查失败；保留数据库及恢复材料"))
    }
    fn check_source_identity(
        &self,
        expected: Option<&Option<(u64, u64)>>,
    ) -> Result<Option<(u64, u64)>> {
        let current = open_directory(&self.root_path)?;
        if directory_identity(&self.dir)? != directory_identity(&current)? {
            bail!("数据目录身份已改变；未恢复数据库");
        }
        let metadata = if exists(&self.dir, MAIN)? {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let file = self.dir.open_with(MAIN, &options)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                bail!("原数据库不是普通文件");
            }
            Some(file_identity(&file)?)
        } else {
            None
        };
        if let Some(expected) = expected {
            let unchanged = match (expected, &metadata) {
                (None, None) => true,
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if !unchanged {
                bail!("原数据库身份已改变；未恢复数据库");
            }
        }
        Ok(metadata)
    }
    /// Opens and closes the original read-only connection internally, with
    /// pinned-directory/file identity fences before and after SQLite snapshot.
    /// Requires a private data directory and cooperating daemon.lock users;
    /// this is not a capability SQLite VFS or malicious same-user ABA defense.
    /// `prepare` is compiled migration code; `invalidate` must invalidate ALL
    /// execution/approval/continuation/remote-auth/vault-intent state, including
    /// adapters added after this module. Neither callback is user-supplied SQL.
    pub(crate) fn restore(
        &self,
        bundle: &Path,
        expected_sha256: &str,
        prepare: impl FnOnce(&mut Connection) -> Result<()>,
        invalidate: impl FnOnce(&Transaction<'_>) -> Result<()>,
    ) -> Result<BackupManifest> {
        self.restore_with_hook(bundle, expected_sha256, prepare, invalidate, |_| Ok(()))
            .map_err(|_| {
                anyhow::anyhow!(
                    "备份恢复未完成；原数据或一致回滚材料已保留，请执行中断恢复后重新确认"
                )
            })
    }
    fn restore_with_hook(
        &self,
        bundle: &Path,
        expected_sha256: &str,
        prepare: impl FnOnce(&mut Connection) -> Result<()>,
        invalidate: impl FnOnce(&Transaction<'_>) -> Result<()>,
        hook: impl Fn(RestorePoint) -> Result<()>,
    ) -> Result<BackupManifest> {
        if self.recover_pending()? != RecoveryOutcome::None {
            bail!("已恢复此前中断操作；请重新检查备份并确认恢复");
        }
        let original_identity = self.check_source_identity(None)?;
        let original = if original_identity.is_some() {
            Some(Connection::open_with_flags(
                self.root_path.join(MAIN),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?)
        } else {
            None
        };
        self.check_source_identity(Some(&original_identity))?;
        let source = open_directory(bundle)?;
        let manifest: BackupManifest =
            serde_json::from_slice(&read_file(&source, "manifest.json", 4096)?)?;
        if !valid_digest(expected_sha256) || expected_sha256 != manifest.sha256 {
            bail!("备份摘要与所有者确认不一致");
        }
        let bytes = read_file(&source, "database.sqlite", MAX_DATABASE_BYTES)?;
        manifest.verify(&bytes)?;
        let mut db = deserialize(&bytes)?;
        drop(bytes);
        if preflight(&db)? != manifest.schema {
            bail!("备份版本声明不一致");
        }
        prepare(&mut db)?;
        validate_import(&db)?;
        if preflight(&db)? != SCHEMA_VERSION {
            bail!("恢复迁移未达到当前版本");
        }
        {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            invalidate_core_for_restore(&tx)?;
            invalidate(&tx)?;
            tx.commit()?;
        }
        validate_import(&db)?;
        let new = consistent_image(&db, Instant::now() + Duration::from_secs(30), || false)?;
        drop(db);
        let old = original
            .as_ref()
            .map(|db| consistent_image(db, Instant::now() + Duration::from_secs(30), || false))
            .transpose()?;
        let old_schema = original.as_ref().map(preflight).transpose()?;
        self.check_source_identity(Some(&original_identity))?;
        drop(original); // Close all original SQLite handles before any Windows rename.
        let new_manifest = BackupManifest::for_bytes(&new, SCHEMA_VERSION);
        let journal = RestoreJournal {
            format: 1,
            new: new_manifest.clone(),
            old: old
                .as_ref()
                .map(|bytes| BackupManifest::for_bytes(bytes, old_schema.unwrap_or(0))),
        };
        write_private(&self.dir, JOURNAL, &serde_json::to_vec(&journal)?)?;
        sync_directory(&self.dir)?;
        hook(RestorePoint::JournalDurable)?;
        if let Some(old) = old {
            write_private(&self.dir, OLD, &old)?;
        }
        hook(RestorePoint::OldImageDurable)?;
        write_private(&self.dir, NEW, &new)?;
        sync_directory(&self.dir)?;
        hook(RestorePoint::NewImageDurable)?;
        if journal.old.is_some() {
            self.dir.rename(MAIN, &self.dir, ORIGINAL)?;
        }
        hook(RestorePoint::OriginalMoved)?;
        if exists(&self.dir, "harness.sqlite-wal")? {
            self.dir
                .rename("harness.sqlite-wal", &self.dir, ORIGINAL_WAL)?;
        }
        hook(RestorePoint::WalMoved)?;
        if exists(&self.dir, "harness.sqlite-shm")? {
            self.dir
                .rename("harness.sqlite-shm", &self.dir, ORIGINAL_SHM)?;
        }
        hook(RestorePoint::ShmMoved)?;
        self.dir.rename(NEW, &self.dir, MAIN)?;
        sync_directory(&self.dir)?;
        hook(RestorePoint::Installed)?;
        write_private(&self.dir, COMMIT, new_manifest.sha256.as_bytes())?;
        sync_directory(&self.dir)?;
        hook(RestorePoint::Committed)?;
        self.cleanup(true)?;
        Ok(new_manifest)
    }
}

/// Defense-in-depth default for current JSON records. The required adapter
/// callback must additionally revoke structured remote/gateway tables. Historic
/// results and conversations remain; active work is never enqueued on restore.
pub(crate) fn invalidate_core_for_restore(db: &Connection) -> Result<()> {
    for table in [
        "tasks",
        "plans",
        "coordinator_jobs",
        "plan_changes",
        "deliveries",
        "session_contexts",
        "execution_requests",
        "capability_calls",
        "credential_operations",
        "mcp_credential_operations",
        "qq_credential_operations",
        "managed_capabilities",
        "named_providers",
    ] {
        if !table_exists(db, table)? {
            continue;
        }
        let mut stmt = db.prepare(&format!("SELECT id,body FROM {table}"))?;
        let mut rows = stmt.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            if count > 500_000 {
                bail!("恢复记录超过有界处理上限");
            }
            let id: String = row.get(0)?;
            let body: String = row.get(1)?;
            if body.len() > 4 * 1024 * 1024 {
                bail!("恢复记录过大");
            }
            let mut value: Value = serde_json::from_str(&body)?;
            if !value.is_object() {
                bail!("恢复记录结构无效");
            }
            match table {
                "tasks" => {
                    if !matches!(
                        value["status"].as_str(),
                        Some("completed" | "failed" | "cancelled")
                    ) {
                        value["status"] = json!("failed");
                        value["stop_reason"] = json!("backup_restored");
                        value["error"] = json!("数据从备份恢复，原执行器与授权已失效；未重放任务");
                    }
                    value["waiting_request_id"] = Value::Null;
                    if value["execution_cleanup"] == "pending" {
                        value["execution_cleanup"] = json!("unknown");
                    }
                }
                "plans" => {
                    if !matches!(
                        value["status"].as_str(),
                        Some("completed" | "failed" | "cancelled" | "archived")
                    ) {
                        value["status"] = json!("paused");
                        value["pause_reason"] = json!("backup_restored");
                    }
                }
                "coordinator_jobs" | "plan_changes" => {
                    if matches!(
                        value["status"].as_str(),
                        Some("pending" | "queued" | "running")
                    ) {
                        value["status"] = json!("failed");
                        value["error"] = json!("备份恢复后没有自动重放规划请求");
                    }
                }
                "deliveries" => {
                    if matches!(value["status"].as_str(), Some("pending" | "sending")) {
                        value["status"] = json!("unknown");
                        value["reason"] = json!("backup_restored_no_replay");
                    }
                }
                "session_contexts" => {
                    value["state"] = json!("invalidated");
                    value["reason"] = json!("备份恢复，必须依据应用历史重建上下文");
                    for key in ["summary", "thread_id", "turn_id"] {
                        value.as_object_mut().unwrap().remove(key);
                    }
                }
                "execution_requests" => {
                    if matches!(value["status"].as_str(), Some("pending" | "approved")) {
                        value["status"] = json!("cancelled");
                        value["reason"] = json!("backup_restored");
                    }
                }
                "capability_calls" => {
                    if value["status"] == "sending" {
                        value["status"] = json!("unknown");
                        value["reason"] = json!("backup_restored_no_replay");
                    }
                }
                "credential_operations"
                | "mcp_credential_operations"
                | "qq_credential_operations" => {
                    value["can_cleanup"] = json!(false);
                    if value["cleanup_status"] == "pending" {
                        value["cleanup_status"] = json!("unknown");
                    }
                    if matches!(value["status"].as_str(), Some("pending" | "inflight")) {
                        value["status"] = json!("unknown");
                        value["reason"] = json!("backup_restored_no_replay");
                    }
                }
                "managed_capabilities" => {
                    value["enabled"] = json!(false);
                    if value["kind"] == "mcp" && value["deleted"] != true {
                        if !value["definition"].is_object() {
                            bail!("恢复能力定义结构无效；未安装备份");
                        }
                        value["definition"]["enabled"] = json!(false);
                        value["status"] = json!("configured");
                        value["catalog"] = json!([]);
                        value["catalog_digest"] = Value::Null;
                        value["probe_state"] = Value::Null;
                    }
                    value["revoke_epoch"] = json!(value["revoke_epoch"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_add(1));
                }
                "named_providers" => {
                    value["enabled"] = json!(false);
                    value["credential_ref"] = Value::Null;
                    value["revision"] =
                        json!(value["revision"].as_u64().unwrap_or(0).saturating_add(1));
                }
                _ => unreachable!(),
            }
            db.execute(
                &format!("UPDATE {table} SET body=?2 WHERE id=?1"),
                rusqlite::params![id, value.to_string()],
            )?;
        }
    }
    // Restored references may point at a different machine/user's vault. Never
    // read or delete them automatically. Audit rows remain for owner inspection.
    if table_exists(db, "mcp_credential_bindings")? {
        db.execute("DELETE FROM mcp_credential_bindings", [])?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
