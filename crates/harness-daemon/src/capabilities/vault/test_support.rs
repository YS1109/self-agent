//! Test-build-only in-memory backend for real daemon/router unit tests. It is
//! not a Cargo feature, environment switch, production constructor or file store.
//! Tests use normal Vault APIs to enroll/read/delete; this control never exposes
//! stored secret bytes and deliberately has no Debug/Serialize implementation.
use super::*;
use std::{
    collections::HashMap,
    sync::{Condvar, Mutex},
};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct CallCounts {
    pub sets: usize,
    pub gets: usize,
    pub deletes: usize,
}
#[derive(Default)]
struct State {
    values: HashMap<String, Zeroizing<Vec<u8>>>,
    counts: CallCounts,
    reject_next: Option<BackendError>,
    gate_next: Option<Arc<GateState>>,
}
#[derive(Clone, Default)]
pub(crate) struct MockVaultControl(Arc<Mutex<State>>);
struct MemoryBackend(MockVaultControl);

/// Fresh fixture storage. Construction performs no OS access. Reuse the control
/// with `open` to simulate a daemon restart against the same credential service.
pub(crate) fn memory(queue_capacity: usize) -> Result<(Arc<Vault>, MockVaultControl), VaultError> {
    let control = MockVaultControl::default();
    Ok((control.open(queue_capacity)?, control))
}
impl MockVaultControl {
    pub(crate) fn open(&self, queue_capacity: usize) -> Result<Arc<Vault>, VaultError> {
        Ok(Arc::new(Vault::start(
            MemoryBackend(self.clone()),
            queue_capacity,
        )?))
    }
    pub(crate) fn counts(&self) -> CallCounts {
        self.0.lock().unwrap().counts
    }
    pub(crate) fn entry_count(&self) -> usize {
        self.0.lock().unwrap().values.len()
    }
    /// Reject exactly the next attempted OS operation. No string/raw-error
    /// injection: the test and production surfaces share safe reason enums.
    pub(crate) fn reject_next(&self, reason: VaultReason, uncertain_mutation: bool) {
        self.0.lock().unwrap().reject_next = Some(BackendError {
            reason,
            uncertain_mutation,
        });
    }
    /// Block after the next operation's effect. A mutation deadline can therefore
    /// become Unknown despite a completed write. Dropping the handle releases it.
    pub(crate) fn block_next_after_effect(&self) -> MockGate {
        let state = Arc::new(GateState::default());
        let old = self.0.lock().unwrap().gate_next.replace(state.clone());
        assert!(old.is_none(), "fixture gate already pending");
        MockGate(state)
    }
}
#[derive(Default)]
struct GateState {
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}
pub(crate) struct MockGate(Arc<GateState>);
impl MockGate {
    pub(crate) async fn entered(&self) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.0.entered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .is_ok()
    }
    pub(crate) fn release(&self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}
impl Drop for MockGate {
    fn drop(&mut self) {
        self.release();
    }
}
fn block(gate: Option<Arc<GateState>>) {
    if let Some(gate) = gate {
        gate.entered.store(true, Ordering::SeqCst);
        let mut released = gate.released.lock().unwrap();
        while !*released {
            released = gate.wake.wait(released).unwrap();
        }
    }
}
impl Backend for MemoryBackend {
    fn set(&mut self, reference: &CredentialRef, secret: &SecretBytes) -> Result<(), BackendError> {
        let gate = {
            let mut state = self.0 .0.lock().unwrap();
            state.counts.sets += 1;
            if let Some(error) = state.reject_next.take() {
                return Err(error);
            }
            state.values.insert(
                reference.as_str().into(),
                Zeroizing::new(secret.expose_secret().to_vec()),
            );
            state.gate_next.take()
        };
        block(gate);
        Ok(())
    }
    fn get(&mut self, reference: &CredentialRef) -> Result<SecretBytes, BackendError> {
        let (gate, secret) = {
            let mut state = self.0 .0.lock().unwrap();
            state.counts.gets += 1;
            if let Some(error) = state.reject_next.take() {
                return Err(error);
            }
            let secret = state.values.get(reference.as_str()).ok_or(BackendError {
                reason: VaultReason::Missing,
                uncertain_mutation: false,
            })?;
            let secret = SecretBytes::new(secret.to_vec()).map_err(|_| BackendError {
                reason: VaultReason::InvalidStoredData,
                uncertain_mutation: false,
            })?;
            (state.gate_next.take(), secret)
        };
        block(gate);
        Ok(secret)
    }
    fn delete(&mut self, reference: &CredentialRef) -> Result<(), BackendError> {
        let gate = {
            let mut state = self.0 .0.lock().unwrap();
            state.counts.deletes += 1;
            if let Some(error) = state.reject_next.take() {
                return Err(error);
            }
            state.values.remove(reference.as_str());
            state.gate_next.take()
        };
        block(gate);
        Ok(())
    }
}

#[tokio::test]
async fn exposed_mock_factory_supports_restart_and_unknown_without_replay() {
    let (vault, control) = memory(4).unwrap();
    let binding = VaultBinding::provider(
        "94a846e4-7ac2-46d7-ac1d-fde573fd6e12",
        "api_key",
        "https://example.invalid/v1",
    )
    .unwrap();
    let fresh = FreshCredential::new(
        &binding,
        SecretBytes::new(b"test-only-canary".to_vec()).unwrap(),
    );
    let reference = fresh.reference().clone();
    let gate = control.block_next_after_effect();
    let worker = vault.clone();
    let bound = binding.clone();
    let write = tokio::spawn(async move {
        worker
            .set(
                fresh,
                &bound,
                Instant::now() + Duration::from_secs(1),
                &VaultCancellation::default(),
            )
            .await
    });
    assert!(gate.entered().await);
    assert_eq!(control.entry_count(), 1);
    let error = write.await.unwrap().err().unwrap();
    assert_eq!(error.certainty, VaultCertainty::Unknown);
    assert_eq!(control.counts().sets, 1);
    gate.release();
    assert_eq!(
        vault
            .shutdown(Instant::now() + Duration::from_secs(3))
            .await,
        ShutdownStatus::Stopped
    );
    let restarted = control.open(4).unwrap();
    let secret = restarted
        .get(
            &reference,
            &binding,
            Instant::now() + Duration::from_secs(3),
            &VaultCancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(secret.expose_secret(), b"test-only-canary");
    assert_eq!(control.counts().sets, 1);
    control.reject_next(VaultReason::AccessDeniedOrLocked, false);
    let rejected = restarted
        .delete(
            &reference,
            &binding,
            Instant::now() + Duration::from_secs(3),
            &VaultCancellation::default(),
        )
        .await;
    assert_eq!(rejected.unwrap_err().certainty, VaultCertainty::Rejected);
    assert_eq!(control.entry_count(), 1);
    restarted
        .delete(
            &reference,
            &binding,
            Instant::now() + Duration::from_secs(3),
            &VaultCancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(control.entry_count(), 0);
    assert_eq!(
        control.counts(),
        CallCounts {
            sets: 1,
            gets: 1,
            deletes: 2
        }
    );
    assert_eq!(
        restarted
            .shutdown(Instant::now() + Duration::from_secs(3))
            .await,
        ShutdownStatus::Stopped
    );
}
