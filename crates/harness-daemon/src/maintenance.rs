//! Owner-selected offline storage maintenance. This module never contacts a
//! model, adapter, update server or OS credential store.
use crate::reliability::storage;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Instant,
};

pub struct DataLease {
    pub(crate) guard: Arc<storage::StorageGuard>,
    initial_identity: Option<(u64, u64)>,
    online_identity: OnceLock<(u64, u64)>,
}
impl DataLease {
    pub fn acquire(directory: &Path) -> Result<Arc<Self>> {
        let guard = storage::StorageGuard::create_and_acquire(directory)?;
        guard.recover_pending()?;
        guard.preflight_startup()?;
        let initial_identity = guard.startup_identity()?;
        Ok(Arc::new(Self {
            guard: Arc::new(guard),
            initial_identity,
            online_identity: OnceLock::new(),
        }))
    }
    pub fn database_path(&self) -> PathBuf {
        self.guard.database_path()
    }
    pub(crate) fn open_store(&self) -> Result<crate::store::Store> {
        if self.online_identity.get().is_some() {
            anyhow::bail!("同一后台数据租约不能重复打开 Store");
        }
        let (connection, identity) = self.guard.open_database(&self.initial_identity)?;
        let store = crate::store::Store::from_connection(connection)?;
        self.guard.validate_source_identity(identity)?;
        self.online_identity
            .set(identity)
            .map_err(|_| anyhow::anyhow!("后台数据库身份已固定，不能重复打开"))?;
        Ok(store)
    }
    pub(crate) fn export_backup(
        &self,
        parent: &Path,
        name: &str,
        deadline: Instant,
    ) -> Result<storage::BackupManifest> {
        let identity = *self
            .online_identity
            .get()
            .context("在线 Store 身份尚未固定")?;
        self.guard
            .export_backup_from_identity(identity, parent, name, deadline, || false)
    }
    pub fn write_daemon_metadata(&self, info: &harness_core::DaemonInfo) -> Result<()> {
        self.guard.write_daemon_metadata(&serde_json::to_vec(info)?)
    }
}

pub fn inspect_backup(bundle: &Path) -> Result<Value> {
    Ok(serde_json::to_value(storage::inspect_backup(bundle)?)?)
}
/// This never stops a live daemon implicitly. Its exclusive data-directory lock
/// must be available, and the owner must provide the manifest digest explicitly.
pub fn restore_backup(directory: &Path, bundle: &Path, expected_sha256: &str) -> Result<Value> {
    let lease = DataLease::acquire(directory)?;
    let manifest = lease.guard.restore(
        bundle,
        expected_sha256,
        crate::store::prepare_database,
        |tx| {
            crate::reliability::jobs::recover_interrupted(tx)?;
            crate::qq::restore::invalidate(tx)?;
            crate::store::reliability::restore_gateway(tx)?;
            crate::store::reliability::validate_records(tx)?;
            Ok(())
        },
    )?;
    Ok(serde_json::to_value(manifest)?)
}
