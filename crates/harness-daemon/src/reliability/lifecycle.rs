//! Cooperative daemon shutdown and explicit wire-protocol compatibility.
//!
//! Integration contract: gate every execution/enqueue/credential/probe admission
//! BEFORE its durable claim. Call Admission::begin_intent BEFORE starting the
//! durable transaction, then transfer into a tracked worker or explicitly settle
//! confirmed synchronous completion/rollback. Dropping an armed permit is Unknown.
//! Keep the Lifecycle owner alive through draining. Workers must await process
//! tree / blocking OS / durable-result cleanup before returning Settled. An async
//! task finishing alone does not prove its external effects have stopped.
//!
//! This helper never aborts workers. A timed-out or cancelled drain retains their
//! JoinHandles so a later drain can observe completion. Dropping the owner or the
//! Tokio runtime is NOT a successful drain and cannot produce a Stopped report.
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{watch, Notify},
    task::JoinHandle,
};

const MAX_TRACKED: usize = 256;
const MAX_DRAIN: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecycleState {
    Running,
    Draining,
    Stopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerKind {
    Task,
    Coordinator,
    Callback,
    Channel,
    Probe,
    Credential,
    BlockingCleanup,
}
/// Settled is an explicit assertion by trusted integration: all owned work and
/// cleanup has been awaited and uncertain external results recorded durably.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerExit {
    Settled,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    Draining,
    Capacity,
    RuntimeUnavailable,
}
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Draining => "后台正在停止接收新工作",
            Self::Capacity => "后台在途工作达到上限，未接收新工作",
            Self::RuntimeUnavailable => "后台异步运行时不可用，未启动工作",
        })
    }
}
impl std::error::Error for AdmissionError {}

#[derive(Clone)]
pub(crate) struct StopSignal {
    receiver: watch::Receiver<bool>,
}
impl StopSignal {
    pub(crate) fn is_cancelled(&self) -> bool {
        *self.receiver.borrow() || self.receiver.has_changed().is_err()
    }
    pub(crate) async fn cancelled(&self) {
        let mut receiver = self.receiver.clone();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}
struct Worker {
    _kind: WorkerKind,
    handle: JoinHandle<WorkerExit>,
}
struct Inner {
    state: LifecycleState,
    admissions: usize,
    workers: Vec<Worker>,
    settled: u64,
    unknown: u64,
    unknown_admissions: u64,
}
pub(crate) struct Lifecycle {
    inner: Mutex<Inner>,
    stop: watch::Sender<bool>,
    changed: Notify,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct LifecycleSnapshot {
    pub state: LifecycleState,
    pub accepting_work: bool,
    pub active_admissions: usize,
    pub pending_workers: usize,
    pub settled_workers: u64,
    pub unknown_workers: u64,
    pub unknown_admissions: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DrainResult {
    Stopped(LifecycleSnapshot),
    /// Pending futures are still owned/tracked, not aborted or detached.
    Pending(LifecycleSnapshot),
    /// No futures/admissions remain, but a worker or started durable admission
    /// failed to confirm cleanup (including panic/cancelled JoinError).
    /// State remains Draining.
    Unknown(LifecycleSnapshot),
}
impl Lifecycle {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                state: LifecycleState::Running,
                admissions: 0,
                workers: vec![],
                settled: 0,
                unknown: 0,
                unknown_admissions: 0,
            }),
            stop: watch::channel(false).0,
            changed: Notify::new(),
        })
    }
    pub(crate) fn signal(&self) -> StopSignal {
        StopSignal {
            receiver: self.stop.subscribe(),
        }
    }
    /// RAII reservation is intentionally not Clone. Admission and begin_draining
    /// use the same lock, so an accepted request is always counted before drain.
    pub(crate) fn admit(self: &Arc<Self>) -> Result<Admission, AdmissionError> {
        // Reserve an actual runtime before callers create a durable intent.
        // The permit carries it, so registration also works if moved to another
        // thread; runtime shutdown is subsequently observed as Unknown on join.
        let runtime = Handle::try_current().map_err(|_| AdmissionError::RuntimeUnavailable)?;
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner);
        if inner.state != LifecycleState::Running {
            return Err(AdmissionError::Draining);
        }
        if inner.admissions + inner.workers.len() >= MAX_TRACKED {
            return Err(AdmissionError::Capacity);
        }
        inner.admissions += 1;
        Ok(Admission {
            lifecycle: self.clone(),
            runtime,
            intent_started: false,
        })
    }
    #[cfg(test)]
    pub(crate) fn spawn<F>(
        self: &Arc<Self>,
        kind: WorkerKind,
        future: F,
    ) -> Result<(), AdmissionError>
    where
        F: Future<Output = WorkerExit> + Send + 'static,
    {
        self.admit()?.spawn(kind, future)
    }
    /// Stop admission first, then signal all already registered/accepted work.
    /// Repeated calls are idempotent and can never reopen the gate.
    pub(crate) fn begin_draining(&self) -> LifecycleSnapshot {
        let mut inner = self.inner.lock().unwrap();
        if inner.state == LifecycleState::Running {
            inner.state = LifecycleState::Draining;
        }
        self.stop.send_replace(true);
        self.reap(&mut inner);
        self.changed.notify_waiters();
        Self::view(&inner)
    }
    pub(crate) fn snapshot(&self) -> LifecycleSnapshot {
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner);
        Self::view(&inner)
    }
    fn view(inner: &Inner) -> LifecycleSnapshot {
        LifecycleSnapshot {
            state: inner.state,
            accepting_work: inner.state == LifecycleState::Running,
            active_admissions: inner.admissions,
            pending_workers: inner.workers.len(),
            settled_workers: inner.settled,
            unknown_workers: inner.unknown,
            unknown_admissions: inner.unknown_admissions,
        }
    }
    /// Poll JoinHandles themselves to completion while retaining every Pending
    /// handle in place. JoinHandle polling does not poll the worker's user code.
    /// A noop waker is safe because drain also polls at a bounded interval.
    fn reap(&self, inner: &mut Inner) {
        let mut cx = Context::from_waker(Waker::noop());
        let mut index = 0;
        while index < inner.workers.len() {
            match Pin::new(&mut inner.workers[index].handle).poll(&mut cx) {
                Poll::Pending => index += 1,
                Poll::Ready(result) => {
                    // Only a completed handle is removed/dropped. Never format
                    // JoinError/panic payloads or expose worker identifiers.
                    drop(inner.workers.swap_remove(index));
                    match result {
                        Ok(WorkerExit::Settled) => inner.settled = inner.settled.saturating_add(1),
                        _ => {
                            inner.unknown = inner.unknown.saturating_add(1);
                            inner.state = LifecycleState::Draining;
                            self.stop.send_replace(true);
                        }
                    }
                }
            }
        }
    }
    /// Cancellation-safe and repeatable. Timeout leaves handles tracked; it does
    /// not terminate a side effect, detach a task, or erase Unknown evidence.
    pub(crate) async fn drain(&self, deadline: Instant) -> DrainResult {
        self.begin_draining();
        let deadline = deadline.min(Instant::now() + MAX_DRAIN);
        loop {
            let snapshot = {
                let mut inner = self.inner.lock().unwrap();
                self.reap(&mut inner);
                if inner.admissions == 0
                    && inner.workers.is_empty()
                    && inner.unknown == 0
                    && inner.unknown_admissions == 0
                {
                    inner.state = LifecycleState::Stopped;
                }
                Self::view(&inner)
            };
            if snapshot.state == LifecycleState::Stopped {
                return DrainResult::Stopped(snapshot);
            }
            if snapshot.pending_workers == 0 && snapshot.active_admissions == 0 {
                return DrainResult::Unknown(snapshot);
            }
            if Instant::now() >= deadline {
                return DrainResult::Pending(snapshot);
            }
            tokio::select! {
                _=self.changed.notified()=>{},
                _=tokio::time::sleep_until((Instant::now()+POLL_INTERVAL).min(deadline).into())=>{},
            }
        }
    }
}
impl Drop for Lifecycle {
    fn drop(&mut self) {
        // This only requests cancellation. Pending JoinHandles detach on drop;
        // there is deliberately no state/report claiming successful shutdown.
        self.stop.send_replace(true);
    }
}
/// Hold across the short durable admission/claim transaction, then either finish
/// synchronous work or transfer it into a tracked worker. Never launch a detached
/// task first and acquire the permit afterwards.
pub(crate) struct Admission {
    lifecycle: Arc<Lifecycle>,
    runtime: Handle,
    intent_started: bool,
}
impl Admission {
    /// MUST precede the durable transaction, not follow its commit: cancellation
    /// or unwinding may happen between commit and worker registration. Calling
    /// this twice is harmless; an armed Drop conservatively closes admission.
    pub(crate) fn begin_intent(&mut self) {
        self.intent_started = true;
    }
    /// Explicit trusted assertion: synchronous work/rollback is confirmed, or
    /// responsibility was transferred to an already registered worker. Never use
    /// this to hide uncertain effects or detach pending cleanup.
    pub(crate) fn settle(mut self) {
        self.intent_started = false;
    }

    pub(crate) fn spawn<F>(mut self, kind: WorkerKind, future: F) -> Result<(), AdmissionError>
    where
        F: Future<Output = WorkerExit> + Send + 'static,
    {
        let weak = Arc::downgrade(&self.lifecycle);
        let mut inner = self.lifecycle.inner.lock().unwrap();
        // This reservation predates draining and already occupies capacity.
        // Do not reject it after a durable claim: register cleanup instead.
        let handle = self.runtime.spawn(async move {
            let result = future.await;
            if let Some(lifecycle) = weak.upgrade() {
                lifecycle.changed.notify_waiters();
            }
            result
        });
        inner.workers.push(Worker {
            _kind: kind,
            handle,
        });
        // Clear only after the worker handle is durably owned by this in-memory
        // tracker; its eventual outcome now determines cleanup certainty.
        self.intent_started = false;
        drop(inner);
        Ok(())
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        let mut inner = self.lifecycle.inner.lock().unwrap();
        inner.admissions -= 1;
        if self.intent_started {
            inner.unknown_admissions = inner.unknown_admissions.saturating_add(1);
            inner.state = LifecycleState::Draining;
            self.lifecycle.stop.send_replace(true);
        }
        self.lifecycle.changed.notify_waiters();
    }
}

// Historical isolated-helper compatibility regressions only. Production wire
// negotiation is the shared harness_core::compatibility module (v15).
#[cfg(test)]
mod isolated_compatibility {
    use super::*;
    // This is a new wire-compatibility generation, NOT a docs/protocol-vN number or
    // CARGO_PKG_VERSION. Missing metadata is not silently treated as compatible.
    pub(crate) const PROTOCOL_CURRENT: u16 = 1;
    pub(crate) const PROTOCOL_MIN: u16 = 1;
    pub(super) const MAX_HANDSHAKE_BYTES: usize = 512;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct ProtocolRange {
        current: u16,
        minimum: u16,
    }
    impl ProtocolRange {
        pub(crate) fn new(current: u16, minimum: u16) -> Result<Self, CompatibilityError> {
            if minimum == 0 || current < minimum {
                return Err(CompatibilityError::Invalid);
            }
            Ok(Self { current, minimum })
        }
        pub(crate) fn local() -> Self {
            Self {
                current: PROTOCOL_CURRENT,
                minimum: PROTOCOL_MIN,
            }
        }
    }
    #[derive(Serialize, Deserialize)]
    enum Application {
        #[serde(rename = "self-agent")]
        SelfAgent,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Handshake {
        handshake_schema: u16,
        application: Application,
        protocol_current: u16,
        protocol_min: u16,
        lifecycle: LifecycleState,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum CompatibilityError {
        Invalid,
        ClientTooOld,
        ServerTooOld,
    }
    impl std::fmt::Display for CompatibilityError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(match self {
                Self::Invalid => "后台协议元数据缺失或无效；未继续执行",
                Self::ClientTooOld => "客户端协议版本过旧；请升级客户端",
                Self::ServerTooOld => "后台协议版本过旧；请升级后台",
            })
        }
    }
    impl std::error::Error for CompatibilityError {}
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Negotiated {
        pub protocol: u16,
        pub state: LifecycleState,
        pub accepts_new_work: bool,
    }
    pub(crate) fn handshake(state: LifecycleState) -> Result<Vec<u8>, CompatibilityError> {
        serde_json::to_vec(&Handshake {
            handshake_schema: 1,
            application: Application::SelfAgent,
            protocol_current: PROTOCOL_CURRENT,
            protocol_min: PROTOCOL_MIN,
            lifecycle: state,
        })
        .map_err(|_| CompatibilityError::Invalid)
    }
    /// Pure compatibility, NOT authentication or authority. Callers must first bind
    /// the response to the authenticated loopback daemon they intend to control.
    pub(crate) fn negotiate(
        raw: &[u8],
        client: ProtocolRange,
    ) -> Result<Negotiated, CompatibilityError> {
        if raw.is_empty() || raw.len() > MAX_HANDSHAKE_BYTES {
            return Err(CompatibilityError::Invalid);
        }
        let peer: Handshake =
            serde_json::from_slice(raw).map_err(|_| CompatibilityError::Invalid)?;
        if peer.handshake_schema != 1 {
            return Err(CompatibilityError::Invalid);
        }
        let server = ProtocolRange::new(peer.protocol_current, peer.protocol_min)?;
        if client.current < server.minimum {
            return Err(CompatibilityError::ClientTooOld);
        }
        if server.current < client.minimum {
            return Err(CompatibilityError::ServerTooOld);
        }
        Ok(Negotiated {
            protocol: client.current.min(server.current),
            state: peer.lifecycle,
            accepts_new_work: peer.lifecycle == LifecycleState::Running,
        })
    }
}
#[cfg(test)]
use isolated_compatibility::*;

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
