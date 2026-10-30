//! Trusted same-Store adapter. SQL quiescence is not physical stop evidence.
use super::lifecycle_runtime::{QuiesceStage, QuiescenceCheck, ShutdownBackend, ShutdownFault};
use crate::AppState;
use std::sync::Arc;

pub(crate) struct Backend {
    state: Arc<AppState>,
}
impl Backend {
    pub(crate) fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}
impl ShutdownBackend for Backend {
    fn stop_external_inputs(&self) {
        if let Some(updates) = &self.state.updates {
            updates.stop_active();
        }
        self.state.qq_stop.send_replace(true);
        self.state.gateway_stop.send_replace(true);
        self.state.wake.notify_waiters();
    }
    fn quiesce(&self, _: QuiesceStage) -> Result<QuiescenceCheck, ShutdownFault> {
        let report = self
            .state
            .store
            .lock()
            .map_err(|_| ShutdownFault)?
            .quiesce_for_shutdown()
            .map_err(|_| ShutdownFault)?;
        // Historical receipt/quarantine counters are audit facts, NOT current
        // process cleanup. The registered physical custodians supply WorkerExit.
        for event in report.events {
            self.state.publish(event);
        }
        self.state.wake.notify_waiters();
        Ok(QuiescenceCheck::default())
    }
}
