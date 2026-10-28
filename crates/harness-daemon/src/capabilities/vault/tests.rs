use super::*;
use std::{
    collections::HashMap,
    sync::{atomic::AtomicUsize, Condvar, Mutex},
};

const OWNER: &str = "94a846e4-7ac2-46d7-ac1d-fde573fd6e12";
const CANARY: &str = "vault-fixture-secret-never-public";

fn binding() -> VaultBinding {
    VaultBinding::provider(OWNER, "api_key", "https://example.invalid/v1").unwrap()
}
fn secret() -> SecretBytes {
    SecretBytes::new(CANARY.as_bytes().to_vec()).unwrap()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}
fn fresh(binding: &VaultBinding) -> (FreshCredential, CredentialRef) {
    let fresh = FreshCredential::new(binding, secret());
    let reference = fresh.reference().clone();
    (fresh, reference)
}
fn error<T>(result: Result<T, VaultError>) -> VaultError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected safe vault error"),
    }
}

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    released: Mutex<bool>,
    ready: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.ready.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.ready.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !self.entered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[derive(Default)]
struct MockState {
    entries: Mutex<HashMap<String, Vec<u8>>>,
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    gate: Mutex<Option<Arc<Gate>>>,
    reject: Mutex<Option<BackendError>>,
    panic: AtomicBool,
}
struct Mock(Arc<MockState>);
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Mock {
    fn begin(&self) -> Result<Active<'_>, BackendError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if self.0.panic.load(Ordering::SeqCst) {
            panic!("fixture backend failure");
        }
        if let Some(error) = *self.0.reject.lock().unwrap() {
            return Err(error);
        }
        let active = self.0.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.max_active.fetch_max(active, Ordering::SeqCst);
        Ok(Active(&self.0.active))
    }
    fn block_once(&self) {
        let gate = self.0.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.block();
        }
    }
}
impl Backend for Mock {
    fn set(&mut self, reference: &CredentialRef, secret: &SecretBytes) -> Result<(), BackendError> {
        let _active = self.begin()?;
        self.0.entries.lock().unwrap().insert(
            reference.as_str().to_owned(),
            secret.expose_secret().to_vec(),
        );
        self.block_once();
        Ok(())
    }
    fn get(&mut self, reference: &CredentialRef) -> Result<SecretBytes, BackendError> {
        let _active = self.begin()?;
        self.block_once();
        match self.0.entries.lock().unwrap().get(reference.as_str()) {
            Some(bytes) => Ok(SecretBytes::new(bytes.clone()).unwrap()),
            None => Err(BackendError {
                reason: VaultReason::Missing,
                uncertain_mutation: false,
            }),
        }
    }
    fn delete(&mut self, reference: &CredentialRef) -> Result<(), BackendError> {
        let _active = self.begin()?;
        self.0.entries.lock().unwrap().remove(reference.as_str());
        self.block_once();
        Ok(())
    }
}
fn mock(capacity: usize) -> (Arc<Vault>, Arc<MockState>) {
    let state = Arc::new(MockState::default());
    (
        Arc::new(Vault::start(Mock(state.clone()), capacity).unwrap()),
        state,
    )
}

#[tokio::test]
async fn mock_roundtrip_rotation_delete_and_restart_reference() {
    let (vault, state) = mock(4);
    let binding = binding();
    let cancel = VaultCancellation::default();
    let (first, reference) = fresh(&binding);
    vault
        .set(first, &binding, deadline(), &cancel)
        .await
        .unwrap();
    let restored = CredentialRef::parse(reference.as_str(), &binding).unwrap();
    let bytes = vault
        .get(&restored, &binding, deadline(), &cancel)
        .await
        .unwrap();
    assert_eq!(bytes.expose_secret(), CANARY.as_bytes());
    let second = FreshCredential::new(
        &binding,
        SecretBytes::new(b"rotated-fixture".to_vec()).unwrap(),
    );
    let next = second.reference().clone();
    assert!(next != reference);
    vault
        .set(second, &binding, deadline(), &cancel)
        .await
        .unwrap();
    assert_eq!(
        vault
            .get(&reference, &binding, deadline(), &cancel)
            .await
            .unwrap()
            .expose_secret(),
        CANARY.as_bytes()
    );
    vault
        .delete(&reference, &binding, deadline(), &cancel)
        .await
        .unwrap();
    assert_eq!(
        error(vault.get(&reference, &binding, deadline(), &cancel).await).reason,
        VaultReason::Missing
    );
    assert_eq!(state.entries.lock().unwrap().len(), 1);
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
    let restarted = Vault::start(Mock(state.clone()), 2).unwrap();
    let restored = CredentialRef::parse(next.as_str(), &binding).unwrap();
    assert_eq!(
        restarted
            .get(&restored, &binding, deadline(), &cancel)
            .await
            .unwrap()
            .expose_secret(),
        b"rotated-fixture"
    );
    assert_eq!(
        restarted.shutdown(deadline()).await,
        ShutdownStatus::Stopped
    );
}

#[tokio::test]
async fn wrong_endpoint_owner_purpose_or_reference_never_enters_backend() {
    let (vault, state) = mock(4);
    let binding = binding();
    let cancel = VaultCancellation::default();
    let (fresh, reference) = fresh(&binding);
    vault
        .set(fresh, &binding, deadline(), &cancel)
        .await
        .unwrap();
    let variants = [
        VaultBinding::provider(OWNER, "api_key", "https://different.invalid/v1").unwrap(),
        VaultBinding::provider(OWNER, "api_key", "http://example.invalid/v1").unwrap(),
        VaultBinding::provider(OWNER, "api_key", "https://example.invalid:444/v1").unwrap(),
        VaultBinding::provider(OWNER, "api_key", "https://example.invalid/v2").unwrap(),
        VaultBinding::provider(OWNER, "header:x-key", "https://example.invalid/v1").unwrap(),
        VaultBinding::provider(
            "52b26327-70de-4f75-becf-cfe5ce34fcea",
            "api_key",
            "https://example.invalid/v1",
        )
        .unwrap(),
        VaultBinding::capability(OWNER, "api_key", &"a".repeat(64)).unwrap(),
    ];
    for wrong in variants {
        assert!(CredentialRef::parse(reference.as_str(), &wrong).is_err());
        assert_eq!(
            error(vault.get(&reference, &wrong, deadline(), &cancel).await).certainty,
            VaultCertainty::NotAttempted
        );
        assert!(vault
            .delete(&reference, &wrong, deadline(), &cancel)
            .await
            .is_err());
    }
    assert!(CredentialRef::parse(&":".repeat(1024 * 1024), &binding).is_err());
    let mut tampered = reference.as_str().to_owned();
    tampered.replace_range(..1, "x");
    assert!(CredentialRef::parse(&tampered, &binding).is_err());
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
}

#[test]
fn bindings_are_canonical_and_inputs_bounded() {
    assert!(
        binding()
            == VaultBinding::provider(OWNER, "api_key", "https://EXAMPLE.invalid:443/v1/").unwrap()
    );
    for url in [
        "https://user:secret@example.invalid/v1",
        "https://example.invalid/v1?q=x",
        "https://example.invalid/v1#x",
        "file:///tmp/secret",
        " https://example.invalid",
        "https://example.invalid\n",
    ] {
        assert!(VaultBinding::provider(OWNER, "api_key", url).is_err());
    }
    for purpose in ["", "../key", "password\n", &"x".repeat(65)] {
        assert!(VaultBinding::provider(OWNER, purpose, "https://example.invalid").is_err());
    }
    assert!(VaultBinding::provider("not-a-uuid", "key", "https://example.invalid").is_err());
    assert!(VaultBinding::capability(OWNER, "TOKEN", &"A".repeat(64)).is_err());
    assert!(SecretBytes::new(vec![]).is_err());
    assert!(SecretBytes::new(vec![1; MAX_SECRET_BYTES + 1]).is_err());
    assert!(SecretBytes::new(vec![1; MAX_SECRET_BYTES]).is_ok());
}

#[tokio::test]
async fn rejected_unavailable_and_unexpected_are_safe_without_fallback() {
    for (reason, uncertain) in [
        (VaultReason::AccessDeniedOrLocked, false),
        (VaultReason::Unavailable, false),
        (VaultReason::Unexpected, true),
    ] {
        let (vault, state) = mock(2);
        *state.reject.lock().unwrap() = Some(BackendError {
            reason,
            uncertain_mutation: uncertain,
        });
        let binding = binding();
        let (fresh, reference) = fresh(&binding);
        let cancel = VaultCancellation::default();
        let result = error(vault.set(fresh, &binding, deadline(), &cancel).await);
        assert_eq!(result.reason, reason);
        assert_eq!(
            result.certainty,
            if uncertain {
                VaultCertainty::Unknown
            } else {
                VaultCertainty::Rejected
            }
        );
        assert_eq!(
            error(vault.get(&reference, &binding, deadline(), &cancel).await).reason,
            reason
        );
        assert_eq!(
            error(
                vault
                    .delete(&reference, &binding, deadline(), &cancel)
                    .await
            )
            .reason,
            reason
        );
        assert!(state.entries.lock().unwrap().is_empty());
        assert_eq!(state.calls.load(Ordering::SeqCst), 3);
        assert!(!format!("{result:?} {result}").contains(CANARY));
        assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_after_write_is_unknown_no_retry_and_worker_drains() {
    let (vault, state) = mock(2);
    let gate = Arc::new(Gate::default());
    *state.gate.lock().unwrap() = Some(gate.clone());
    let binding = binding();
    let (fresh, reference) = fresh(&binding);
    let task = {
        let vault = vault.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            vault
                .set(
                    fresh,
                    &binding,
                    Instant::now() + Duration::from_secs(1),
                    &VaultCancellation::default(),
                )
                .await
        })
    };
    gate.entered().await;
    assert_eq!(state.entries.lock().unwrap().len(), 1);
    let result = error(task.await.unwrap());
    assert_eq!(
        result,
        VaultError {
            reason: VaultReason::Timeout,
            certainty: VaultCertainty::Unknown
        }
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    gate.release();
    let read = vault
        .get(
            &reference,
            &binding,
            deadline(),
            &VaultCancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(read.expose_secret(), CANARY.as_bytes());
    assert_eq!(state.calls.load(Ordering::SeqCst), 2); // one write, one explicit reconciliation read
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_after_delete_is_unknown_and_never_replayed() {
    let (vault, state) = mock(2);
    let binding = binding();
    let (fresh, reference) = fresh(&binding);
    vault
        .set(fresh, &binding, deadline(), &VaultCancellation::default())
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    *state.gate.lock().unwrap() = Some(gate.clone());
    let cancel = VaultCancellation::default();
    let task = {
        let vault = vault.clone();
        let binding = binding.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            vault
                .delete(&reference, &binding, deadline(), &cancel)
                .await
        })
    };
    gate.entered().await;
    cancel.cancel();
    assert_eq!(
        error(task.await.unwrap()).certainty,
        VaultCertainty::Unknown
    );
    assert!(state.entries.lock().unwrap().is_empty());
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    gate.release();
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_queue_rejects_overflow_and_cancelled_waiter_never_executes() {
    let (vault, state) = mock(1);
    let binding = binding();
    let gate = Arc::new(Gate::default());
    *state.gate.lock().unwrap() = Some(gate.clone());
    let first = {
        let vault = vault.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            let (fresh, _) = fresh(&binding);
            vault
                .set(fresh, &binding, deadline(), &VaultCancellation::default())
                .await
        })
    };
    gate.entered().await;
    let cancel = VaultCancellation::default();
    let queued = {
        let vault = vault.clone();
        let binding = binding.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let (fresh, _) = fresh(&binding);
            vault.set(fresh, &binding, deadline(), &cancel).await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while vault.sender.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (extra, _) = fresh(&binding);
    assert_eq!(
        error(
            vault
                .set(extra, &binding, deadline(), &VaultCancellation::default())
                .await
        ),
        VaultError::not_attempted(VaultReason::Busy)
    );
    cancel.cancel();
    assert_eq!(
        error(queued.await.unwrap()),
        VaultError::not_attempted(VaultReason::Cancelled)
    );
    gate.release();
    first.await.unwrap().unwrap();
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.entries.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_calls_remain_serial_and_shutdown_rejects_new_work() {
    let (vault, state) = mock(32);
    let binding = binding();
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let vault = vault.clone();
        let binding = binding.clone();
        tasks.push(tokio::spawn(async move {
            let (fresh, _) = fresh(&binding);
            vault
                .set(fresh, &binding, deadline(), &VaultCancellation::default())
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(state.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(state.entries.lock().unwrap().len(), 20);
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
    let (fresh, _) = fresh(&binding);
    assert_eq!(
        error(
            vault
                .set(fresh, &binding, deadline(), &VaultCancellation::default())
                .await
        )
        .certainty,
        VaultCertainty::NotAttempted
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_shutdown_reports_draining_then_stopped_without_killing_os_call() {
    let (vault, state) = mock(2);
    let binding = binding();
    let gate = Arc::new(Gate::default());
    *state.gate.lock().unwrap() = Some(gate.clone());
    let task = {
        let vault = vault.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            let (fresh, _) = fresh(&binding);
            vault
                .set(fresh, &binding, deadline(), &VaultCancellation::default())
                .await
        })
    };
    gate.entered().await;
    assert_eq!(
        vault
            .shutdown(Instant::now() + Duration::from_millis(20))
            .await,
        ShutdownStatus::Draining
    );
    let (fresh, _) = fresh(&binding);
    assert_eq!(
        error(
            vault
                .set(fresh, &binding, deadline(), &VaultCancellation::default())
                .await
        )
        .reason,
        VaultReason::ShuttingDown
    );
    gate.release();
    task.await.unwrap().unwrap();
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn worker_panic_is_unknown_and_future_operations_fail_closed() {
    let (vault, state) = mock(2);
    state.panic.store(true, Ordering::SeqCst);
    let binding = binding();
    let (fresh, reference) = fresh(&binding);
    let result = error(
        vault
            .set(fresh, &binding, deadline(), &VaultCancellation::default())
            .await,
    );
    assert_eq!(
        result,
        VaultError {
            reason: VaultReason::WorkerFailed,
            certainty: VaultCertainty::Unknown
        }
    );
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Failed);
    assert_eq!(
        error(
            vault
                .get(
                    &reference,
                    &binding,
                    deadline(),
                    &VaultCancellation::default()
                )
                .await
        )
        .reason,
        VaultReason::WorkerFailed
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_queued_future_erases_it_without_executing_the_write() {
    let (vault, state) = mock(2);
    let binding = binding();
    let gate = Arc::new(Gate::default());
    *state.gate.lock().unwrap() = Some(gate.clone());
    let (first, reference) = fresh(&binding);
    let running = {
        let vault = vault.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            vault
                .set(first, &binding, deadline(), &VaultCancellation::default())
                .await
        })
    };
    gate.entered().await;
    let queued = {
        let vault = vault.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            let (next, _) = fresh(&binding);
            vault
                .set(next, &binding, deadline(), &VaultCancellation::default())
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while vault.sender.capacity() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    queued.abort();
    assert!(queued.await.unwrap_err().is_cancelled());
    gate.release();
    running.await.unwrap().unwrap();
    // This read is a barrier after the abandoned queued write.
    assert!(vault
        .get(
            &reference,
            &binding,
            deadline(),
            &VaultCancellation::default()
        )
        .await
        .is_ok());
    assert_eq!(state.entries.lock().unwrap().len(), 1);
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_queued_operations_as_not_attempted() {
    let (vault, state) = mock(1);
    let binding = binding();
    let gate = Arc::new(Gate::default());
    *state.gate.lock().unwrap() = Some(gate.clone());
    let running = {
        let vault = vault.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            let (first, _) = fresh(&binding);
            vault
                .set(first, &binding, deadline(), &VaultCancellation::default())
                .await
        })
    };
    gate.entered().await;
    let queued = {
        let vault = vault.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            let (next, _) = fresh(&binding);
            vault
                .set(next, &binding, deadline(), &VaultCancellation::default())
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while vault.sender.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        vault
            .shutdown(Instant::now() + Duration::from_millis(20))
            .await,
        ShutdownStatus::Draining
    );
    gate.release();
    running.await.unwrap().unwrap();
    assert_eq!(
        error(queued.await.unwrap()),
        VaultError::not_attempted(VaultReason::ShuttingDown)
    );
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn already_cancelled_or_expired_never_enters_backend() {
    let (vault, state) = mock(1);
    let binding = binding();
    let cancel = VaultCancellation::default();
    cancel.cancel();
    let (first, _) = fresh(&binding);
    assert_eq!(
        error(vault.set(first, &binding, deadline(), &cancel).await),
        VaultError::not_attempted(VaultReason::Cancelled)
    );
    let (second, _) = fresh(&binding);
    assert_eq!(
        error(
            vault
                .set(
                    second,
                    &binding,
                    Instant::now(),
                    &VaultCancellation::default()
                )
                .await
        ),
        VaultError::not_attempted(VaultReason::Timeout)
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
}

#[test]
fn secret_types_cannot_debug_serialize_or_clone_and_error_payload_is_discarded() {
    // Negative compile-time trait assertions: adding any of these implementations
    // makes trait selection ambiguous and fails the test build.
    macro_rules! not_impl {
        ($type:ty, $bound:path) => {{
            trait Ambiguous<A> {
                fn check() {}
            }
            impl<T: ?Sized> Ambiguous<()> for T {}
            struct Implemented;
            impl<T: ?Sized + $bound> Ambiguous<Implemented> for T {}
            let _ = <$type as Ambiguous<_>>::check;
        }};
    }
    not_impl!(SecretBytes, std::fmt::Debug);
    not_impl!(SecretBytes, serde::Serialize);
    not_impl!(SecretBytes, Clone);
    not_impl!(FreshCredential, std::fmt::Debug);
    not_impl!(FreshCredential, serde::Serialize);
    not_impl!(FreshCredential, Clone);
    for platform_error in [
        keyring_core::Error::PlatformFailure(Box::new(std::io::Error::other(CANARY))),
        keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other(CANARY))),
        keyring_core::Error::BadEncoding(CANARY.as_bytes().to_vec()),
        keyring_core::Error::BadDataFormat(
            CANARY.as_bytes().to_vec(),
            Box::new(std::io::Error::other(CANARY)),
        ),
        keyring_core::Error::Invalid(CANARY.into(), CANARY.into()),
        keyring_core::Error::BadStoreFormat(CANARY.into()),
    ] {
        let classified = system::classify(platform_error);
        let public = VaultError {
            reason: classified.reason,
            certainty: VaultCertainty::Rejected,
        };
        let rendered = format!("{public:?} {public} {}", public.reason.code());
        assert!(!rendered.contains(CANARY));
        assert!(std::error::Error::source(&public).is_none());
    }
    let binding = binding();
    let (_, reference) = fresh(&binding);
    assert!(!reference.as_str().contains(CANARY));
    assert!(!reference.account().contains("example.invalid"));
    assert!(!reference.account().contains(OWNER));
}

#[tokio::test]
async fn system_constructor_and_shutdown_do_not_open_any_os_store() {
    // Only constructs SystemBackend::default; no get/set/delete/health is called.
    let vault = Vault::system(1).unwrap();
    assert_eq!(vault.shutdown(deadline()).await, ShutdownStatus::Stopped);
    assert!(Vault::system(0).is_err());
    assert!(Vault::system(MAX_QUEUE + 1).is_err());
}
