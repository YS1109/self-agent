//! Update state uses the SAME authoritative Store transaction and retains the
//! data-directory lease through detached uploads/finalization. No second DB.
use super::Store;
use crate::{
    maintenance::DataLease,
    reliability::update_runtime::{UpdateDatabase, UpdateRuntime},
};
use anyhow::{Context, Result};
use rusqlite::Transaction;
use std::sync::{Arc, Mutex};
pub(crate) struct UpdateStore {
    store: Arc<Mutex<Store>>,
    _lease: Arc<DataLease>,
    lifecycle: Arc<crate::reliability::lifecycle_runtime::LifecycleRuntime>,
}
impl UpdateDatabase for UpdateStore {
    fn new_transaction<T>(
        &self,
        lifecycle: &crate::reliability::lifecycle_runtime::LifecycleRuntime,
        action: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        if !std::ptr::eq(lifecycle, self.lifecycle.as_ref()) {
            anyhow::bail!("更新生命周期不一致");
        }
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("更新状态锁不可用"))?;
        self.lifecycle.check_locked(&store)?;
        let tx = store.db.transaction()?;
        let value = action(&tx)?;
        tx.commit()
            .map_err(|_| crate::reliability::update_runtime::UpdateCommitUncertain)?;
        Ok(value)
    }
    fn transaction<T>(&self, action: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("更新状态锁不可用"))?;
        let tx = store.db.transaction()?;
        let value = action(&tx)?;
        tx.commit()
            .map_err(|_| crate::reliability::update_runtime::UpdateCommitUncertain)?;
        Ok(value)
    }
}
pub(crate) type Runtime = UpdateRuntime<UpdateStore>;
pub(crate) fn runtime(
    store: Arc<Mutex<Store>>,
    lease: Arc<DataLease>,
    lifecycle: Arc<crate::reliability::lifecycle_runtime::LifecycleRuntime>,
) -> Result<Arc<Runtime>> {
    let (stage, witness) = lease
        .guard
        .update_directories()
        .context("私有更新目录不可用")?;
    UpdateRuntime::new(
        Arc::new(UpdateStore {
            store,
            _lease: lease,
            lifecycle: lifecycle.clone(),
        }),
        stage,
        witness,
        env!("CARGO_PKG_VERSION").into(),
        crate::reliability::storage::SCHEMA_VERSION,
        lifecycle,
    )
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
