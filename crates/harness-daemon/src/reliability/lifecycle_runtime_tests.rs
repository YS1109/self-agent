use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Condvar,
};

#[derive(Default)]
struct Block {
    open: Mutex<bool>,
    changed: Condvar,
    entered: Notify,
}
impl Block {
    fn wait(&self) {
        self.entered.notify_one();
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ReleaseOnDrop(Arc<Block>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[derive(Default)]
struct Backend {
    store: Arc<Mutex<Vec<&'static str>>>,
    stops: AtomicUsize,
    calls: AtomicUsize,
    fail_first: AtomicBool,
    unknown: AtomicBool,
    initial_unknown: AtomicBool,
    initial_pending: AtomicBool,
    historical_outcome_unknown: AtomicUsize,
    final_block: Option<Arc<Block>>,
}
impl ShutdownBackend for Backend {
    fn stop_external_inputs(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn quiesce(&self, stage: QuiesceStage) -> Result<QuiescenceCheck, ShutdownFault> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if stage == QuiesceStage::Final {
            if let Some(block) = &self.final_block {
                block.wait();
            }
        }
        self.store
            .lock()
            .unwrap()
            .push(if stage == QuiesceStage::Initial {
                "initial"
            } else {
                "final"
            });
        if stage == QuiesceStage::Initial && self.fail_first.load(Ordering::SeqCst) {
            return Err(ShutdownFault);
        }
        Ok(QuiescenceCheck {
            pending: u64::from(
                stage == QuiesceStage::Initial && self.initial_pending.load(Ordering::SeqCst),
            ),
            unknown: u64::from(
                self.unknown.load(Ordering::SeqCst)
                    || (stage == QuiesceStage::Initial
                        && self.initial_unknown.load(Ordering::SeqCst)),
            ),
        })
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}
async fn observed(runtime: &LifecycleRuntime) -> RuntimeSnapshot {
    let report = runtime.observe(deadline()).await;
    assert!(!report.deadline_reached);
    report.snapshot
}
#[tokio::test]
async fn idle_shutdown_is_idempotent_and_performs_two_quiescence_barriers() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let other = Arc::new(Backend::default());
    runtime.request_shutdown(backend.clone()).unwrap();
    runtime.request_shutdown(other.clone()).unwrap();
    let result = observed(&runtime).await;
    assert_eq!(result.state, LifecycleState::Stopped);
    assert_eq!(result.outcome, Some(ShutdownOutcome::Stopped));
    assert_eq!(backend.stops.load(Ordering::SeqCst), 1);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    assert_eq!(other.calls.load(Ordering::SeqCst), 0);
    assert_eq!(*backend.store.lock().unwrap(), ["initial", "final"]);
    assert_eq!(runtime.admit().err(), Some(AdmissionError::Draining));
    assert_eq!(
        runtime.request_shutdown(other).unwrap().outcome,
        result.outcome
    );
}
#[tokio::test]
async fn preadmitted_store_waiter_cannot_begin_intent_after_gate_closes() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let mut permit = runtime.admit().unwrap();
    runtime.request_shutdown(backend.clone()).unwrap();
    {
        let guard = backend.store.lock().unwrap();
        assert_eq!(permit.begin_intent(&guard), Err(AdmissionError::Draining));
    }
    drop(permit);
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
    assert_eq!(*backend.store.lock().unwrap(), ["initial", "final"]);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intent_linearized_before_close_commits_before_first_quiesce_without_lock_inversion() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let mut permit = runtime.admit().unwrap();
    let (ready, wait) = oneshot::channel();
    let release = Arc::new(Block::default());
    let _cleanup = ReleaseOnDrop(release.clone());
    let worker_release = release.clone();
    let db = backend.store.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let mut guard = db.lock().unwrap();
        permit.begin_intent(&guard).unwrap();
        ready.send(()).unwrap();
        worker_release.wait();
        guard.push("intent");
        drop(guard);
        permit.settle();
    });
    wait.await.unwrap();
    // This must not wait for the Store mutex. Its later quiesce does wait.
    runtime.request_shutdown(backend.clone()).unwrap();
    assert!(runtime.signal().is_cancelled());
    assert_eq!(runtime.snapshot().state, LifecycleState::Draining);
    release.release();
    worker.await.unwrap();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
    assert_eq!(
        *backend.store.lock().unwrap(),
        ["intent", "initial", "final"]
    );
}
#[tokio::test]
async fn dirty_drop_is_unknown_and_never_becomes_stopped() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let mut permit = runtime.admit().unwrap();
    {
        let mut db = backend.store.lock().unwrap();
        permit.begin_intent(&db).unwrap();
        db.push("uncertain_intent");
    }
    drop(permit);
    assert!(runtime.signal().is_cancelled());
    runtime.request_shutdown(backend).unwrap();
    let report = observed(&runtime).await;
    assert_eq!(report.outcome, Some(ShutdownOutcome::Unknown));
    assert_eq!(report.state, LifecycleState::Draining);
    assert_eq!(report.workers.unknown_admissions, 1);
}
#[tokio::test]
async fn confirmed_rollback_can_release_armed_admission() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let mut permit = runtime.admit().unwrap();
    {
        let guard = backend.store.lock().unwrap();
        permit.begin_intent(&guard).unwrap();
    }
    permit.settle();
    runtime.request_shutdown(backend).unwrap();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
}
#[tokio::test]
async fn dropped_http_receiver_does_not_cancel_registered_effect_or_cleanup() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let mut permit = runtime.admit().unwrap();
    {
        let guard = backend.store.lock().unwrap();
        permit.begin_intent(&guard).unwrap();
    }
    let (release, wait) = oneshot::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let marker = finished.clone();
    let observe = permit
        .spawn(WorkerKind::Credential, async move {
            wait.await.unwrap();
            marker.store(true, Ordering::SeqCst);
            (WorkerExit::Settled, "private result")
        })
        .unwrap();
    drop(observe);
    runtime.request_shutdown(backend).unwrap();
    let pending = runtime
        .observe(Instant::now() + Duration::from_millis(30))
        .await;
    assert!(pending.deadline_reached);
    assert_eq!(pending.snapshot.workers.pending_workers, 1);
    assert!(!finished.load(Ordering::SeqCst));
    release.send(()).unwrap();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
    assert!(finished.load(Ordering::SeqCst));
}
#[tokio::test]
async fn dropped_shutdown_observer_does_not_drop_supervisor() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let (release, wait) = oneshot::channel();
    runtime
        .spawn(WorkerKind::Task, async move {
            wait.await.unwrap();
            WorkerExit::Settled
        })
        .unwrap();
    runtime.request_shutdown(backend.clone()).unwrap();
    let observed_runtime = runtime.clone();
    let observer = tokio::spawn(async move { observed_runtime.observe(deadline()).await });
    tokio::task::yield_now().await;
    observer.abort();
    let _ = observer.await;
    assert_eq!(runtime.snapshot().workers.pending_workers, 1);
    release.send(()).unwrap();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn final_store_check_must_finish_even_when_inner_workers_are_stopped() {
    let runtime = LifecycleRuntime::new();
    let block = Arc::new(Block::default());
    let _cleanup = ReleaseOnDrop(block.clone());
    let backend = Arc::new(Backend {
        final_block: Some(block.clone()),
        ..Default::default()
    });
    runtime.request_shutdown(backend).unwrap();
    tokio::time::timeout(Duration::from_secs(3), block.entered.notified())
        .await
        .expect("final quiesce worker must enter its fixture gate");
    let pending = runtime
        .observe(Instant::now() + Duration::from_millis(30))
        .await;
    assert!(pending.deadline_reached);
    assert_eq!(pending.snapshot.phase, ShutdownPhase::FinalQuiesce);
    assert_eq!(pending.snapshot.workers.state, LifecycleState::Stopped);
    assert_eq!(pending.snapshot.state, LifecycleState::Draining);
    block.release();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
}
#[tokio::test]
async fn worker_uncertainty_and_final_durable_uncertainty_cannot_be_erased() {
    for unknown_worker in [false, true] {
        let runtime = LifecycleRuntime::new();
        let backend = Arc::new(Backend::default());
        backend.unknown.store(!unknown_worker, Ordering::SeqCst);
        if unknown_worker {
            runtime
                .spawn(WorkerKind::Probe, async { WorkerExit::Unknown })
                .unwrap();
        }
        runtime.request_shutdown(backend.clone()).unwrap();
        let report = observed(&runtime).await;
        assert_eq!(report.state, LifecycleState::Draining);
        assert_eq!(report.outcome, Some(ShutdownOutcome::Unknown));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn initial_transaction_failure_stays_unknown_but_still_awaits_workers_and_final_barrier() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    backend.fail_first.store(true, Ordering::SeqCst);
    let (release, wait) = oneshot::channel();
    runtime
        .spawn(WorkerKind::BlockingCleanup, async move {
            wait.await.unwrap();
            WorkerExit::Settled
        })
        .unwrap();
    runtime.request_shutdown(backend.clone()).unwrap();
    assert!(
        runtime
            .observe(Instant::now() + Duration::from_millis(30))
            .await
            .deadline_reached
    );
    release.send(()).unwrap();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Unknown)
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn panic_is_unknown_not_success_and_snapshot_contains_only_allowlisted_state() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    runtime
        .spawn(WorkerKind::Callback, async {
            panic!("fixture worker panic");
            #[allow(unreachable_code)]
            WorkerExit::Settled
        })
        .unwrap();
    runtime.request_shutdown(backend).unwrap();
    let report = observed(&runtime).await;
    assert_eq!(report.outcome, Some(ShutdownOutcome::Unknown));
    assert_eq!(report.workers.unknown_workers, 1);
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("fixture worker panic"));
}
#[tokio::test]
async fn registered_blocking_operation_keeps_ownership_through_observer_timeout() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let block = Arc::new(Block::default());
    let _cleanup = ReleaseOnDrop(block.clone());
    let owned = block.clone();
    let (started, wait) = oneshot::channel();
    runtime
        .spawn(WorkerKind::BlockingCleanup, async move {
            let result = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                owned.wait();
            })
            .await;
            if result.is_ok() {
                WorkerExit::Settled
            } else {
                WorkerExit::Unknown
            }
        })
        .unwrap();
    wait.await.unwrap();
    runtime.request_shutdown(backend).unwrap();
    assert!(
        runtime
            .observe(Instant::now() + Duration::from_millis(30))
            .await
            .deadline_reached
    );
    assert_eq!(runtime.snapshot().workers.pending_workers, 1);
    block.release();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
}
#[tokio::test]
async fn capacity_is_reserved_before_intent_and_existing_settlement_is_not_readmitted() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let mut reservations = Vec::new();
    for _ in 0..256 {
        reservations.push(runtime.admit().unwrap());
    }
    assert_eq!(runtime.admit().err(), Some(AdmissionError::Capacity));
    runtime.request_shutdown(backend.clone()).unwrap();
    assert_eq!(
        runtime.check_locked(&backend.store.lock().unwrap()),
        Err(AdmissionError::Draining)
    );
    drop(reservations);
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
}
#[test]
fn missing_tokio_runtime_fails_before_closing_gate_or_side_effects() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    assert_eq!(
        runtime.request_shutdown(backend.clone()).err(),
        Some(AdmissionError::RuntimeUnavailable)
    );
    assert_eq!(runtime.snapshot().state, LifecycleState::Running);
    assert_eq!(backend.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn armed_preexisting_intent_can_register_original_cleanup_after_gate_closure() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    let mut permit = runtime.admit().unwrap();
    {
        let mut store = backend.store.lock().unwrap();
        permit.begin_intent(&store).unwrap();
        store.push("intent");
    }
    runtime.request_shutdown(backend.clone()).unwrap();
    let output = permit
        .spawn(WorkerKind::Credential, async {
            (WorkerExit::Settled, "original settlement")
        })
        .unwrap();
    assert_eq!(output.await.unwrap(), "original settlement");
    let final_state = observed(&runtime).await;
    assert_eq!(final_state.outcome, Some(ShutdownOutcome::Stopped));
    assert_eq!(final_state.workers.settled_workers, 1);
    assert_eq!(
        *backend.store.lock().unwrap(),
        ["intent", "initial", "final"]
    );
}

#[tokio::test]
async fn initially_unknown_effect_cannot_be_erased_by_final_projection() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    backend.initial_unknown.store(true, Ordering::SeqCst);
    runtime.request_shutdown(backend.clone()).unwrap();
    let result = observed(&runtime).await;
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.outcome, Some(ShutdownOutcome::Unknown));
    assert_eq!(result.state, LifecycleState::Draining);
}
#[tokio::test]
async fn initially_pending_effect_can_settle_only_after_original_worker_finishes() {
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    backend.initial_pending.store(true, Ordering::SeqCst);
    let (release, wait) = oneshot::channel();
    runtime
        .spawn(WorkerKind::BlockingCleanup, async move {
            wait.await.unwrap();
            WorkerExit::Settled
        })
        .unwrap();
    runtime.request_shutdown(backend.clone()).unwrap();
    assert!(
        runtime
            .observe(Instant::now() + Duration::from_millis(30))
            .await
            .deadline_reached
    );
    release.send(()).unwrap();
    assert_eq!(
        observed(&runtime).await.outcome,
        Some(ShutdownOutcome::Stopped)
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn historical_unknown_results_remain_audit_not_current_cleanup_ownership() {
    // This is the adapter contract fixture, not proof of production Store wiring:
    // historical unknown sends/credential results persist independently from
    // current worker/OS/process cleanup certainty.
    let runtime = LifecycleRuntime::new();
    let backend = Arc::new(Backend::default());
    backend
        .historical_outcome_unknown
        .store(7, Ordering::SeqCst);
    runtime
        .spawn(WorkerKind::Channel, async { WorkerExit::Settled })
        .unwrap();
    runtime.request_shutdown(backend.clone()).unwrap();
    let result = observed(&runtime).await;
    assert_eq!(result.outcome, Some(ShutdownOutcome::Stopped));
    assert_eq!(result.state, LifecycleState::Stopped);
    assert_eq!(backend.historical_outcome_unknown.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn owner_monitor_reaps_unknown_and_panic_without_external_clients_or_admissions() {
    for panic_worker in [false, true] {
        let runtime = LifecycleRuntime::new();
        let backend = Arc::new(Backend::default());
        let (release, wait) = oneshot::channel();
        runtime
            .spawn(WorkerKind::Task, async move {
                wait.await.unwrap();
                assert!(!panic_worker, "fixture background worker panic");
                WorkerExit::Unknown
            })
            .unwrap();
        let signal = runtime.signal();
        let owner = runtime.clone();
        let observed_backend = backend.clone();
        // The owner driver is the ONLY reaper after this point: no client poll,
        // new admission, request_shutdown, or snapshot is performed by the test.
        let driver = tokio::spawn(async move {
            let detected = owner.monitor_until_draining().await;
            assert_eq!(detected.state, LifecycleState::Draining);
            owner.request_shutdown(observed_backend).unwrap();
            owner.observe(deadline()).await
        });
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), signal.cancelled())
            .await
            .expect("owner monitor must detect failure without any HTTP activity");
        let result = driver.await.unwrap();
        assert_eq!(result.snapshot.outcome, Some(ShutdownOutcome::Unknown));
        assert_eq!(result.snapshot.workers.unknown_workers, 1);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }
}
