use anyhow::{bail, ensure, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Directory(Arc<PathBuf>, Arc<tokio::sync::Semaphore>);
#[derive(Serialize, Clone)]
pub struct Device {
    pub device_id: String,
    pub label: String,
    pub revision: i64,
    pub revoked: bool,
}
pub fn canonical_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|v| v.to_string() == id)
}
pub fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
impl Directory {
    #[cfg(test)]
    pub(crate) fn hold_all_verification_slots(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.1.clone().try_acquire_many_owned(32).unwrap()
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "设备目录数据库必须为普通文件"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options.open(path)?;
            }
            Err(error) => return Err(error.into()),
        }
        let this = Self(
            Arc::new(path.to_path_buf()),
            Arc::new(tokio::sync::Semaphore::new(32)),
        );
        let db = this.db()?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS devices(device_id TEXT PRIMARY KEY,label TEXT NOT NULL,verifier TEXT NOT NULL UNIQUE,revision INTEGER NOT NULL,revoked INTEGER NOT NULL);")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(this)
    }
    fn db(&self) -> Result<Connection> {
        let metadata = std::fs::symlink_metadata(self.0.as_ref())?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "设备目录数据库必须为普通文件"
        );
        // A missing directory after startup must not silently create a fresh
        // authority database. The service account owns the private parent;
        // this is not a malicious same-user ABA-race capability SQLite VFS.
        let db = Connection::open_with_flags(
            self.0.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(db)
    }
    pub fn enroll(&self, label: &str) -> Result<(Device, String)> {
        ensure!(
            !label.is_empty() && label.len() <= 128 && !label.chars().any(char::is_control),
            "设备名称无效"
        );
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))?;
        ensure!(count < 1024, "设备目录已满");
        let id = Uuid::new_v4().to_string();
        let token = random_token();
        tx.execute(
            "INSERT INTO devices VALUES (?1,?2,?3,0,0)",
            params![id, label, digest(&token)],
        )?;
        tx.commit()?;
        Ok((
            Device {
                device_id: id,
                label: label.into(),
                revision: 0,
                revoked: false,
            },
            token,
        ))
    }
    pub fn list(&self) -> Result<Vec<Device>> {
        let db = self.db()?;
        let mut q =
            db.prepare("SELECT device_id,label,revision,revoked FROM devices ORDER BY device_id")?;
        let rows = q.query_map([], |r| {
            Ok(Device {
                device_id: r.get(0)?,
                label: r.get(1)?,
                revision: r.get(2)?,
                revoked: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn revoke(&self, id: &str) -> Result<()> {
        ensure!(canonical_id(id), "设备标识无效");
        let n=self.db()?.execute("UPDATE devices SET revoked=1,revision=CASE WHEN revision<9223372036854775807 THEN revision+1 ELSE revision END WHERE device_id=?1",[id])?;
        if n != 1 {
            bail!("设备不存在")
        }
        Ok(())
    }
    pub async fn verify(
        &self,
        id: String,
        token: Option<String>,
        revision: Option<i64>,
    ) -> Result<Option<Device>> {
        let permit = self
            .1
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("目录检查繁忙"))?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if !canonical_id(&id) {
                return Ok(None);
            }
            let row = this
                .db()?
                .query_row(
                    "SELECT label,verifier,revision,revoked FROM devices WHERE device_id=?1",
                    [&id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, bool>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((label, hash, rev, revoked)) = row else {
                return Ok(None);
            };
            if revoked || revision.is_some_and(|r| r != rev) {
                return Ok(None);
            }
            if let Some(token) = token {
                let got = digest(&token);
                if token.len() != 64
                    || hash.len() != got.len()
                    || hash
                        .bytes()
                        .zip(got.bytes())
                        .fold(0u8, |a, (x, y)| a | (x ^ y))
                        != 0
                {
                    return Ok(None);
                }
            }
            Ok(Some(Device {
                device_id: id,
                label,
                revision: rev,
                revoked: false,
            }))
        })
        .await?
    }
}
