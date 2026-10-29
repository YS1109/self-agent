use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
async fn until(mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn drain_waits_for_confirmed_cleanup_then_stops_and_cannot_reopen() {
    let lifecycle = Lifecycle::new();
    let signal = lifecycle.signal();
    let settled = Arc::new(AtomicBool::new(false));
    let done = settled.clone();
    lifecycle
        .spawn(WorkerKind::Task, async move {
            signal.cancelled().await;
            tokio::time::sleep(Duration::from_millis(30)).await;
            done.store(true, Ordering::SeqCst);
            WorkerExit::Settled
        })
        .unwrap();
    let report = lifecycle.drain(deadline()).await;
    assert!(matches!(report, DrainResult::Stopped(_)));
    assert!(settled.load(Ordering::SeqCst));
    assert_eq!(lifecycle.snapshot().settled_workers, 1);
    assert_eq!(lifecycle.snapshot().state, LifecycleState::Stopped);
    assert_eq!(lifecycle.admit().err(), Some(AdmissionError::Draining));
    assert_eq!(lifecycle.begin_draining().state, LifecycleState::Stopped);
    assert_eq!(lifecycle.drain(deadline()).await, report);
}
#[tokio::test]
async fn admission_closes_before_cancellation_is_observable() {
    let lifecycle = Lifecycle::new();
    let before = lifecycle.signal();
    assert!(!before.is_cancelled());
    lifecycle.begin_draining();
    assert!(before.is_cancelled());
    assert!(lifecycle.signal().is_cancelled());
    assert_eq!(lifecycle.admit().err(), Some(AdmissionError::Draining));
    tokio::time::timeout(Duration::from_millis(100), lifecycle.signal().cancelled())
        .await
        .unwrap();
}
#[tokio::test]
async fn accepted_claim_reservation_can_register_cleanup_after_drain_begins() {
    let lifecycle = Lifecycle::new();
    let admission = lifecycle.admit().unwrap();
    assert_eq!(lifecycle.begin_draining().active_admissions, 1);
    assert!(matches!(
        lifecycle.drain(Instant::now()).await,
        DrainResult::Pending(_)
    ));
    let signal = lifecycle.signal();
    admission
        .spawn(WorkerKind::Credential, async move {
            assert!(signal.is_cancelled());
            WorkerExit::Settled
        })
        .unwrap();
    let report = lifecycle.drain(deadline()).await;
    assert!(matches!(report, DrainResult::Stopped(_)));
    assert_eq!(lifecycle.snapshot().active_admissions, 0);
    assert_eq!(lifecycle.snapshot().settled_workers, 1);
}
#[tokio::test]
async fn timeout_keeps_running_worker_handle_and_later_drain_can_join_it() {
    let lifecycle = Lifecycle::new();
    let (release, wait) = oneshot::channel();
    let completed = Arc::new(AtomicBool::new(false));
    let done = completed.clone();
    lifecycle
        .spawn(WorkerKind::Probe, async move {
            let _ = wait.await;
            done.store(true, Ordering::SeqCst);
            WorkerExit::Settled
        })
        .unwrap();
    let first = lifecycle
        .drain(Instant::now() + Duration::from_millis(30))
        .await;
    assert!(matches!(
        first,
        DrainResult::Pending(LifecycleSnapshot {
            pending_workers: 1,
            ..
        })
    ));
    assert!(!completed.load(Ordering::SeqCst));
    assert_eq!(lifecycle.snapshot().state, LifecycleState::Draining);
    release.send(()).unwrap();
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Stopped(_)
    ));
    assert!(completed.load(Ordering::SeqCst));
}
#[tokio::test]
async fn cancelling_outer_drain_future_does_not_abort_or_detach_cleanup() {
    let lifecycle = Lifecycle::new();
    let (release, wait) = oneshot::channel();
    lifecycle
        .spawn(WorkerKind::Channel, async move {
            let _ = wait.await;
            WorkerExit::Settled
        })
        .unwrap();
    let owner = lifecycle.clone();
    let outer = tokio::spawn(async move { owner.drain(deadline()).await });
    until(|| lifecycle.snapshot().state == LifecycleState::Draining).await;
    outer.abort();
    let _ = outer.await;
    assert_eq!(lifecycle.snapshot().pending_workers, 1);
    assert_eq!(lifecycle.snapshot().unknown_workers, 0);
    release.send(()).unwrap();
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Stopped(_)
    ));
    assert_eq!(lifecycle.snapshot().settled_workers, 1);
}
#[tokio::test]
async fn concurrent_and_repeated_drains_observe_one_completion_without_double_join() {
    let lifecycle = Lifecycle::new();
    let signal = lifecycle.signal();
    lifecycle
        .spawn(WorkerKind::Coordinator, async move {
            signal.cancelled().await;
            tokio::task::yield_now().await;
            WorkerExit::Settled
        })
        .unwrap();
    let first = lifecycle.clone();
    let second = lifecycle.clone();
    let (a, b) = tokio::join!(first.drain(deadline()), second.drain(deadline()));
    assert_eq!(a, b);
    assert!(matches!(a, DrainResult::Stopped(_)));
    assert_eq!(lifecycle.snapshot().settled_workers, 1);
}
#[tokio::test]
async fn explicit_unknown_and_worker_panics_never_become_stopped() {
    for panic in [false, true] {
        let lifecycle = Lifecycle::new();
        lifecycle
            .spawn(WorkerKind::BlockingCleanup, async move {
                if panic {
                    panic!("fixture cleanup failure");
                }
                WorkerExit::Unknown
            })
            .unwrap();
        let report = lifecycle.drain(deadline()).await;
        assert!(matches!(
            report,
            DrainResult::Unknown(LifecycleSnapshot {
                state: LifecycleState::Draining,
                pending_workers: 0,
                unknown_workers: 1,
                ..
            })
        ));
        assert_eq!(lifecycle.drain(deadline()).await, report);
        assert_eq!(lifecycle.admit().err(), Some(AdmissionError::Draining));
    }
}
#[tokio::test]
async fn externally_cancelled_worker_is_unknown_not_confirmed_cleanup() {
    let lifecycle = Lifecycle::new();
    lifecycle
        .spawn(WorkerKind::Callback, async {
            std::future::pending::<()>().await;
            WorkerExit::Settled
        })
        .unwrap();
    // Simulate an external runtime cancellation. Production API deliberately
    // does not expose an abort method or hand out the registered JoinHandle.
    lifecycle.inner.lock().unwrap().workers[0].handle.abort();
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Unknown(_)
    ));
}
#[tokio::test]
async fn capacity_is_reserved_before_claim_and_rejected_future_never_runs() {
    let lifecycle = Lifecycle::new();
    let admissions = (0..MAX_TRACKED)
        .map(|_| lifecycle.admit().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lifecycle.admit().err(), Some(AdmissionError::Capacity));
    let effect = Arc::new(AtomicBool::new(false));
    let seen = effect.clone();
    assert_eq!(
        lifecycle.spawn(WorkerKind::Task, async move {
            seen.store(true, Ordering::SeqCst);
            WorkerExit::Settled
        }),
        Err(AdmissionError::Capacity)
    );
    assert!(!effect.load(Ordering::SeqCst));
    lifecycle.begin_draining();
    drop(admissions);
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Stopped(_)
    ));
}
#[test]
fn missing_runtime_never_starts_work_or_leaks_admission() {
    let lifecycle = Lifecycle::new();
    let effect = Arc::new(AtomicBool::new(false));
    let seen = effect.clone();
    assert_eq!(
        lifecycle.spawn(WorkerKind::Task, async move {
            seen.store(true, Ordering::SeqCst);
            WorkerExit::Settled
        }),
        Err(AdmissionError::RuntimeUnavailable)
    );
    assert!(!effect.load(Ordering::SeqCst));
    assert_eq!(lifecycle.snapshot().active_admissions, 0);
    assert_eq!(lifecycle.snapshot().pending_workers, 0);
}
#[tokio::test]
async fn completed_slots_are_reaped_without_unbounded_history() {
    let lifecycle = Lifecycle::new();
    for _ in 0..MAX_TRACKED * 2 {
        lifecycle
            .spawn(WorkerKind::Probe, async { WorkerExit::Settled })
            .unwrap();
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Stopped(_)
    ));
    assert_eq!(lifecycle.snapshot().settled_workers, MAX_TRACKED as u64 * 2);
    assert_eq!(lifecycle.snapshot().pending_workers, 0);
}
#[tokio::test]
async fn blocking_cleanup_is_awaited_not_assumed_cancelled_at_timeout() {
    let lifecycle = Lifecycle::new();
    let release = Arc::new(AtomicBool::new(false));
    let stop = release.clone();
    let (started, seen) = oneshot::channel();
    lifecycle
        .spawn(WorkerKind::BlockingCleanup, async move {
            let cleanup = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let deadline = Instant::now() + Duration::from_secs(3);
                while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
                if stop.load(Ordering::SeqCst) {
                    WorkerExit::Settled
                } else {
                    WorkerExit::Unknown
                }
            });
            cleanup.await.unwrap_or(WorkerExit::Unknown)
        })
        .unwrap();
    seen.await.unwrap();
    assert!(matches!(
        lifecycle
            .drain(Instant::now() + Duration::from_millis(20))
            .await,
        DrainResult::Pending(_)
    ));
    release.store(true, Ordering::SeqCst);
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Stopped(_)
    ));
}
#[tokio::test]
async fn dropping_owner_only_signals_and_does_not_claim_cleanup_stopped() {
    let lifecycle = Lifecycle::new();
    let signal = lifecycle.signal();
    let (stopping, seen) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    let (done, completed) = oneshot::channel();
    lifecycle
        .spawn(WorkerKind::Channel, async move {
            signal.cancelled().await;
            let _ = stopping.send(());
            let _ = wait.await;
            let _ = done.send(());
            WorkerExit::Settled
        })
        .unwrap();
    drop(lifecycle);
    tokio::time::timeout(Duration::from_secs(1), seen)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), completed)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn handshake_reports_wire_generation_independently_of_package_version() {
    for state in [
        LifecycleState::Running,
        LifecycleState::Draining,
        LifecycleState::Stopped,
    ] {
        let raw = handshake(state).unwrap();
        assert!(raw.len() <= MAX_HANDSHAKE_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(value.get("version").is_none());
        assert!(value.get("token").is_none());
        assert_eq!(value["protocol_current"], PROTOCOL_CURRENT);
        let result = negotiate(&raw, ProtocolRange::local()).unwrap();
        assert_eq!(result.state, state);
        assert_eq!(result.protocol, 1);
        assert_eq!(result.accepts_new_work, state == LifecycleState::Running);
    }
}
fn peer(current: u16, minimum: u16) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"handshake_schema":1,"application":"self-agent","protocol_current":current,"protocol_min":minimum,"lifecycle":"running"})).unwrap()
}
#[test]
fn protocol_negotiates_only_valid_intersection_and_distinguishes_old_peers() {
    assert_eq!(
        negotiate(&peer(4, 2), ProtocolRange::new(3, 1).unwrap())
            .unwrap()
            .protocol,
        3
    );
    assert_eq!(
        negotiate(&peer(3, 1), ProtocolRange::new(4, 2).unwrap())
            .unwrap()
            .protocol,
        3
    );
    assert_eq!(
        negotiate(&peer(4, 2), ProtocolRange::local()),
        Err(CompatibilityError::ClientTooOld)
    );
    assert_eq!(
        negotiate(&peer(1, 1), ProtocolRange::new(4, 2).unwrap()),
        Err(CompatibilityError::ServerTooOld)
    );
    for (current, minimum) in [(0, 0), (1, 0), (1, 2)] {
        assert_eq!(
            ProtocolRange::new(current, minimum),
            Err(CompatibilityError::Invalid)
        );
        assert_eq!(
            negotiate(&peer(current, minimum), ProtocolRange::local()),
            Err(CompatibilityError::Invalid)
        );
    }
}
#[test]
fn malformed_or_secret_bearing_handshake_fails_closed_without_echoing_input() {
    let base: serde_json::Value = serde_json::from_slice(&peer(1, 1)).unwrap();
    for (field, value) in [
        ("handshake_schema", serde_json::json!(2)),
        ("application", serde_json::json!("other")),
        ("lifecycle", serde_json::json!("ready")),
        ("token", serde_json::json!("fixture-secret-canary")),
    ] {
        let mut bad = base.clone();
        bad[field] = value;
        let error =
            negotiate(&serde_json::to_vec(&bad).unwrap(), ProtocolRange::local()).unwrap_err();
        assert_eq!(error, CompatibilityError::Invalid);
        assert!(!format!("{error} {error:?}").contains("canary"));
    }
    for field in [
        "handshake_schema",
        "application",
        "protocol_current",
        "protocol_min",
        "lifecycle",
    ] {
        let mut bad = base.clone();
        bad.as_object_mut().unwrap().remove(field);
        assert_eq!(
            negotiate(&serde_json::to_vec(&bad).unwrap(), ProtocolRange::local()),
            Err(CompatibilityError::Invalid)
        );
    }
    assert_eq!(
        negotiate(&vec![b'x'; MAX_HANDSHAKE_BYTES + 1], ProtocolRange::local()),
        Err(CompatibilityError::Invalid)
    );
    let duplicate =
        String::from_utf8(peer(1, 1))
            .unwrap()
            .replacen('{', "{\"protocol_current\":999,", 1);
    assert_eq!(
        negotiate(duplicate.as_bytes(), ProtocolRange::local()),
        Err(CompatibilityError::Invalid)
    );
    assert_eq!(
        negotiate(b"{}", ProtocolRange::local()),
        Err(CompatibilityError::Invalid)
    );
}

#[tokio::test]
async fn preclaimed_admission_registers_using_its_original_runtime_from_another_thread() {
    let lifecycle = Lifecycle::new();
    let permit = lifecycle.admit().unwrap();
    lifecycle.begin_draining();
    std::thread::spawn(move || permit.spawn(WorkerKind::Credential, async { WorkerExit::Settled }))
        .join()
        .unwrap()
        .unwrap();
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Stopped(_)
    ));
    assert_eq!(lifecycle.snapshot().settled_workers, 1);
}

#[tokio::test]
async fn cancelled_after_durable_intent_before_registration_remains_unknown() {
    let lifecycle = Lifecycle::new();
    let owner = lifecycle.clone();
    let committed = Arc::new(AtomicBool::new(false));
    let wrote = committed.clone();
    let (ready, seen) = oneshot::channel();
    let caller = tokio::spawn(async move {
        let mut admission = owner.admit().unwrap();
        admission.begin_intent();
        // Fixture equivalent of a committed durable claim, before registration.
        wrote.store(true, Ordering::SeqCst);
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
        admission.settle();
    });
    seen.await.unwrap();
    assert!(committed.load(Ordering::SeqCst));
    caller.abort();
    let _ = caller.await;
    let report = lifecycle.drain(deadline()).await;
    assert!(matches!(
        report,
        DrainResult::Unknown(LifecycleSnapshot {
            active_admissions: 0,
            pending_workers: 0,
            unknown_admissions: 1,
            ..
        })
    ));
    assert_eq!(lifecycle.snapshot().state, LifecycleState::Draining);
    assert!(lifecycle.signal().is_cancelled());
    assert_eq!(lifecycle.admit().err(), Some(AdmissionError::Draining));
}

#[tokio::test]
async fn panic_after_intent_never_loses_uncertain_admission() {
    let lifecycle = Lifecycle::new();
    let mut admission = lifecycle.admit().unwrap();
    admission.begin_intent();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _keep_until_unwind = admission;
        panic!("fixture interruption after durable intent");
    }));
    assert!(unwind.is_err());
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Unknown(LifecycleSnapshot {
            unknown_admissions: 1,
            ..
        })
    ));
}

#[tokio::test]
async fn unstarted_or_explicitly_rolled_back_admission_can_settle_cleanly() {
    let lifecycle = Lifecycle::new();
    drop(lifecycle.admit().unwrap()); // No transaction/effect was started.
    let mut admission = lifecycle.admit().unwrap();
    admission.begin_intent();
    admission.begin_intent(); // Idempotent marking before fallible transaction.
                              // Integration confirms transaction rollback or synchronous completion.
    admission.settle();
    assert_eq!(lifecycle.snapshot().unknown_admissions, 0);
    assert!(matches!(
        lifecycle.drain(deadline()).await,
        DrainResult::Stopped(_)
    ));
}

#[tokio::test]
async fn registered_worker_takes_armed_admission_cleanup_responsibility() {
    for result in [WorkerExit::Settled, WorkerExit::Unknown] {
        let lifecycle = Lifecycle::new();
        let mut admission = lifecycle.admit().unwrap();
        admission.begin_intent();
        lifecycle.begin_draining();
        admission
            .spawn(WorkerKind::Credential, async move { result })
            .unwrap();
        let report = lifecycle.drain(deadline()).await;
        assert_eq!(lifecycle.snapshot().unknown_admissions, 0);
        match result {
            WorkerExit::Settled => assert!(matches!(report, DrainResult::Stopped(_))),
            WorkerExit::Unknown => assert!(matches!(
                report,
                DrainResult::Unknown(LifecycleSnapshot {
                    unknown_workers: 1,
                    ..
                })
            )),
        }
    }
}
