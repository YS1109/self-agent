//! One physical OS-vault custodian per AppState. A caller deadline is an
//! observation only; it must never detach a still-running OS worker as Stopped.
use super::vault::ShutdownStatus;
use crate::{
    reliability::lifecycle::{AdmissionError, WorkerExit, WorkerKind},
    AppState,
};
use std::time::{Duration, Instant};

pub(crate) fn register_shutdown(state: &AppState) -> Result<(), AdmissionError> {
    let mut registered = state.cleanup_owner.lock().unwrap();
    if *registered {
        return Ok(());
    }
    let vault = state.vault.clone();
    let signal = state.lifecycle.signal();
    let lease = state.storage_lease.clone();
    state
        .lifecycle
        .spawn(WorkerKind::BlockingCleanup, async move {
            let _lease = lease;
            signal.cancelled().await;
            loop {
                match vault
                    .shutdown(Instant::now() + Duration::from_millis(250))
                    .await
                {
                    ShutdownStatus::Stopped => return WorkerExit::Settled,
                    ShutdownStatus::Failed => return WorkerExit::Unknown,
                    ShutdownStatus::Draining => {}
                }
            }
        })?;
    *registered = true;
    Ok(())
}
