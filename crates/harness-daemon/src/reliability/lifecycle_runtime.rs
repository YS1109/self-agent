//! Runtime ownership for actual daemon integration (not wired by this module).
//!
//! New work: reserve capacity BEFORE a durable intent. With the authoritative
//! Store mutex held, begin_intent/check_locked takes intent_gate -> Lifecycle's
//! lock. Keep that SAME Store guard through the transaction. Shutdown takes
//! intent_gate -> Lifecycle to close admission, releases both, THEN acquires
//! Store inside the awaited quiesce worker. Never acquire Store while holding
//! intent_gate, supervisor state, or Lifecycle's lock. This orders an already
//! admitted transaction before first quiesce without a check/commit race.
//!
//! Existing settlement is not new admission: registered workers must finish
//! original cleanup/receipts even while draining. Their nested blocking/process
//! workers must be awaited, or independently registered. Returning Settled is a
//! trusted assertion, not something inferred from an HTTP future completing.
//!
//! The separate, retained shutdown supervisor must NOT register in the worker
//! set it drains. Public Stopped additionally requires the FINAL quiesce check;
//! the inner Lifecycle reaching Stopped alone is insufficient. No timeout or
//! dropped observer aborts cleanup. Runtime teardown itself is not safe shutdown.
use super::lifecycle::{
    Admission, AdmissionError, DrainResult, Lifecycle, LifecycleSnapshot, LifecycleState,
    StopSignal, WorkerExit, WorkerKind,
};
use serde::Serialize;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{oneshot, Notify},
    task::JoinHandle,
};

const OBSERVE_MAX: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);
const DRAIN_TICK: Duration = Duration::from_millis(250);
const RUNNING_REAP_TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuiesceStage {
    Initial,
    Final,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QuiescenceCheck {
    /// CURRENT process cleanup ownership still needing its ORIGINAL worker's
    /// confirmation, not a count of historical pending/sending receipt rows.
    pub pending: u64,
    /// CURRENT process physical cleanup / owned-work certainty gap. This is
    /// sticky for this drain. NEVER map historical remote, credential or tool
    /// outcome-unknown audit rows (including previous-process quarantine) here:
    /// an unknown result can coexist with a confirmed closed connection/process.
    /// Such audit facts remain in Store even after a successful shutdown.
    pub unknown: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShutdownFault;
/// Implemented by a trusted AppState adapter, not by remote/tool input. Do not
/// return raw DB errors, paths, tokens or worker identifiers across this trait.
pub(crate) trait ShutdownBackend: Send + Sync + 'static {
    /// Fast local signals only: QQ/Gateway watches, wake loops, update cancel.
    /// Vault OS shutdown itself belongs in a registered, awaited cleanup worker.
    fn stop_external_inputs(&self);
    /// Blocking Store transaction plus CURRENT process cleanup-ownership check.
    /// A successful transaction normally returns default(): actual registered
    /// workers carry cleanup certainty via WorkerExit. Historical receipt/audit
    /// counts stay in Store, not in this check. An adapter may report a current
    /// ownership gap not otherwise represented by a tracked custodian. Never
    /// infer physical cleanup from SQL, invoke network/OS-vault APIs, or enqueue
    /// new work here. Settled workers may preserve an audited Unknown result
    /// once their actual process/connection/OS operation is confirmed stopped.
    fn quiesce(&self, stage: QuiesceStage) -> Result<QuiescenceCheck, ShutdownFault>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShutdownPhase {
    NotRequested,
    InitialQuiesce,
    Workers,
    FinalQuiesce,
    Finished,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShutdownOutcome {
    Stopped,
    Unknown,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct RuntimeSnapshot {
    /// Public generation binding, never a credential or persistence key.
    pub instance_id: String,
    pub state: LifecycleState,
    pub phase: ShutdownPhase,
    pub outcome: Option<ShutdownOutcome>,
    pub workers: LifecycleSnapshot,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShutdownObservation {
    pub snapshot: RuntimeSnapshot,
    /// Timeout is only an observer deadline. Owned cleanup is still running.
    pub deadline_reached: bool,
}
struct Supervisor {
    phase: ShutdownPhase,
    handle: Option<JoinHandle<ShutdownOutcome>>,
    outcome: Option<ShutdownOutcome>,
}
pub(crate) struct LifecycleRuntime {
    instance_id: String,
    workers: Arc<Lifecycle>,
    intent_gate: Mutex<()>,
    supervisor: Mutex<Supervisor>,
    changed: Notify,
}
impl LifecycleRuntime {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            workers: Lifecycle::new(),
            intent_gate: Mutex::new(()),
            supervisor: Mutex::new(Supervisor {
                phase: ShutdownPhase::NotRequested,
                handle: None,
                outcome: None,
            }),
            changed: Notify::new(),
        })
    }
    pub(crate) fn instance_id(&self) -> &str {
        &self.instance_id
    }
    pub(crate) fn signal(&self) -> StopSignal {
        self.workers.signal()
    }
    pub(crate) fn admit(self: &Arc<Self>) -> Result<WorkAdmission, AdmissionError> {
        let _gate = self.intent_gate.lock().unwrap();
        let permit = self.workers.admit()?;
        Ok(WorkAdmission {
            owner: self.clone(),
            permit: Some(permit),
        })
    }
    /// For an ALREADY registered worker, while holding the real Store guard.
    /// This is not permission to spawn detached work or to bypass admission.
    /// Do not use for original-result finalization: that must remain permitted.
    pub(crate) fn check_locked<S>(&self, _store: &MutexGuard<'_, S>) -> Result<(), AdmissionError> {
        let _gate = self.intent_gate.lock().unwrap();
        self.check_gate()
    }
    fn check_gate(&self) -> Result<(), AdmissionError> {
        if self.workers.snapshot().accepting_work {
            Ok(())
        } else {
            Err(AdmissionError::Draining)
        }
    }
    /// Startup fixed loops are registered BEFORE their first claim/effect.
    pub(crate) fn spawn<F>(
        self: &Arc<Self>,
        kind: WorkerKind,
        worker: F,
    ) -> Result<(), AdmissionError>
    where
        F: Future<Output = WorkerExit> + Send + 'static,
    {
        self.admit()?
            .spawn(kind, async move { (worker.await, ()) })?;
        Ok(())
    }
    /// Idempotent, synchronous admission closure + supervisor registration.
    /// This call is never made while holding the Store guard: its backend will
    /// subsequently acquire Store only after every runtime lock is released.
    pub(crate) fn request_shutdown<B: ShutdownBackend>(
        self: &Arc<Self>,
        backend: Arc<B>,
    ) -> Result<RuntimeSnapshot, AdmissionError> {
        let runtime = Handle::try_current().map_err(|_| AdmissionError::RuntimeUnavailable)?;
        let mut supervisor = self.supervisor.lock().unwrap();
        if supervisor.phase == ShutdownPhase::NotRequested {
            {
                let _gate = self.intent_gate.lock().unwrap();
                self.workers.begin_draining();
            }
            supervisor.phase = ShutdownPhase::InitialQuiesce;
            let owner = self.clone();
            supervisor.handle = Some(runtime.spawn(async move { owner.supervise(backend).await }));
        }
        drop(supervisor);
        self.changed.notify_waiters();
        Ok(self.snapshot())
    }
    fn phase(&self, phase: ShutdownPhase) {
        self.supervisor.lock().unwrap().phase = phase;
        self.changed.notify_waiters();
    }
    async fn quiesce<B: ShutdownBackend>(
        backend: Arc<B>,
        stage: QuiesceStage,
    ) -> Result<QuiescenceCheck, ShutdownFault> {
        // Retain/await the blocking handle. A disconnected HTTP observer cannot
        // drop it, and no deadline here aborts a filesystem/SQLite operation.
        tokio::task::spawn_blocking(move || backend.quiesce(stage))
            .await
            .map_err(|_| ShutdownFault)?
    }
    async fn supervise<B: ShutdownBackend>(self: Arc<Self>, backend: Arc<B>) -> ShutdownOutcome {
        backend.stop_external_inputs();
        let initial = Self::quiesce(backend.clone(), QuiesceStage::Initial).await;
        self.phase(ShutdownPhase::Workers);
        let worker_outcome = loop {
            match self.workers.drain(Instant::now() + DRAIN_TICK).await {
                DrainResult::Pending(_) => continue,
                result => break result,
            }
        };
        self.phase(ShutdownPhase::FinalQuiesce);
        let final_check = Self::quiesce(backend, QuiesceStage::Final).await;
        // Unknown is sticky for this drain. A final projection disappearing or
        // being cleared cannot retroactively prove an initially uncertain effect.
        // Initial pending is different: its ORIGINAL registered worker may settle.
        if initial.is_ok_and(|check| check.unknown == 0)
            && matches!(worker_outcome, DrainResult::Stopped(_))
            && final_check == Ok(QuiescenceCheck::default())
        {
            ShutdownOutcome::Stopped
        } else {
            ShutdownOutcome::Unknown
        }
    }
    pub(crate) fn snapshot(&self) -> RuntimeSnapshot {
        let mut supervisor = self.supervisor.lock().unwrap();
        if let Some(handle) = supervisor.handle.as_mut() {
            let mut cx = Context::from_waker(Waker::noop());
            if let Poll::Ready(result) = Pin::new(handle).poll(&mut cx) {
                // Only a completed handle is removed. Never abort or format a
                // JoinError/panic payload; it may contain private worker data.
                supervisor.handle = None;
                supervisor.outcome = Some(result.unwrap_or(ShutdownOutcome::Unknown));
                supervisor.phase = ShutdownPhase::Finished;
            }
        }
        // Snapshot can reap an Unknown worker and close Lifecycle itself. Use
        // the same gate as explicit shutdown and armed Drop for that transition.
        let workers = {
            let _gate = self.intent_gate.lock().unwrap();
            self.workers.snapshot()
        };
        let state = if supervisor.outcome == Some(ShutdownOutcome::Stopped) {
            LifecycleState::Stopped
        } else if supervisor.phase != ShutdownPhase::NotRequested
            || workers.state != LifecycleState::Running
        {
            LifecycleState::Draining
        } else {
            LifecycleState::Running
        };
        RuntimeSnapshot {
            instance_id: self.instance_id.clone(),
            state,
            phase: supervisor.phase,
            outcome: supervisor.outcome,
            workers,
        }
    }
    /// The daemon owner MUST drive this from startup alongside Ctrl-C/serve.
    /// Merely awaiting StopSignal is insufficient: a failed JoinHandle becomes
    /// Unknown only when reaped, even if no HTTP observer/admission arrives.
    /// This is an owner observer, never a member of the worker set being drained.
    /// It does no SQL/network work. On return the owner starts the SAME trusted
    /// request_shutdown backend. Dropping this future does not stop any worker.
    pub(crate) async fn monitor_until_draining(&self) -> RuntimeSnapshot {
        let signal = self.signal();
        loop {
            let snapshot = self.snapshot();
            if snapshot.state != LifecycleState::Running {
                return snapshot;
            }
            tokio::select! {
                _=signal.cancelled()=>{},
                _=tokio::time::sleep(RUNNING_REAP_TICK)=>{},
            }
        }
    }

    /// Independent repeatable observers. Cancelling this future does not cancel
    /// the supervisor, detach a worker, release its admission, or erase Unknown.
    pub(crate) async fn observe(&self, deadline: Instant) -> ShutdownObservation {
        let deadline = deadline.min(Instant::now() + OBSERVE_MAX);
        loop {
            let snapshot = self.snapshot();
            if snapshot.outcome.is_some() || snapshot.phase == ShutdownPhase::NotRequested {
                return ShutdownObservation {
                    snapshot,
                    deadline_reached: false,
                };
            }
            if Instant::now() >= deadline {
                return ShutdownObservation {
                    snapshot,
                    deadline_reached: true,
                };
            }
            tokio::select! {
                _=self.changed.notified()=>{},
                _=tokio::time::sleep_until((Instant::now()+POLL).min(deadline).into())=>{},
            }
        }
    }
}
/// Non-cloneable capacity reservation. An armed drop inherits reviewed
/// Lifecycle Unknown semantics, including after observer cancellation/unwind.
pub(crate) struct WorkAdmission {
    owner: Arc<LifecycleRuntime>,
    permit: Option<Admission>,
}
impl WorkAdmission {
    pub(crate) fn begin_intent<S>(
        &mut self,
        _store: &MutexGuard<'_, S>,
    ) -> Result<(), AdmissionError> {
        let _gate = self.owner.intent_gate.lock().unwrap();
        self.owner.check_gate()?;
        self.permit.as_mut().unwrap().begin_intent();
        Ok(())
    }
    /// Only a confirmed synchronous result/rollback, or transfer to a worker
    /// already registered before the intent, permits this explicit release.
    pub(crate) fn settle(mut self) {
        let _gate = self.owner.intent_gate.lock().unwrap();
        self.permit.take().unwrap().settle();
    }
    pub(crate) fn spawn<F, T>(
        mut self,
        kind: WorkerKind,
        worker: F,
    ) -> Result<oneshot::Receiver<T>, AdmissionError>
    where
        F: Future<Output = (WorkerExit, T)> + Send + 'static,
        T: Send + 'static,
    {
        let (send, observe) = oneshot::channel();
        let _gate = self.owner.intent_gate.lock().unwrap();
        self.permit.take().unwrap().spawn(kind, async move {
            let (outcome, result) = worker.await;
            let _ = send.send(result); // observer ownership is intentionally separate
            outcome
        })?;
        Ok(observe)
    }
}
impl Drop for WorkAdmission {
    fn drop(&mut self) {
        if self.permit.is_some() {
            let _gate = self.owner.intent_gate.lock().unwrap();
            drop(self.permit.take());
        }
    }
}

#[cfg(test)]
#[path = "lifecycle_runtime_tests.rs"]
mod tests;
