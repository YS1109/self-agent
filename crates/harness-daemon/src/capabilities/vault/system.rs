//! Platform backends are selected explicitly. Never use keyring's global/default
//! store (which another library could replace with an insecure mock or file store).

use super::{Backend, BackendError, CredentialRef, SecretBytes, VaultReason, NAMESPACE};
use keyring_core::{CredentialStore, Entry};
use std::sync::Arc;
use zeroize::Zeroize;

#[derive(Default)]
pub(super) struct SystemBackend {
    store: Option<Arc<CredentialStore>>,
}

impl SystemBackend {
    fn entry(&mut self, reference: &CredentialRef) -> Result<Entry, BackendError> {
        if self.store.is_none() {
            self.store = Some(open_store().map_err(|_| BackendError {
                reason: VaultReason::Unavailable,
                uncertain_mutation: false,
            })?);
        }
        let store = self.store.as_ref().ok_or(BackendError {
            reason: VaultReason::Unavailable,
            uncertain_mutation: false,
        })?;
        let account = reference.account();
        #[cfg(target_os = "windows")]
        let modifiers = Some(std::collections::HashMap::from([("persistence", "Local")]));
        #[cfg(target_os = "macos")]
        let modifiers = Some(std::collections::HashMap::from([("keychain", "User")]));
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let modifiers = None;
        store
            .build(NAMESPACE, &account, modifiers.as_ref())
            .map_err(classify)
    }
}

impl Backend for SystemBackend {
    fn set(&mut self, reference: &CredentialRef, secret: &SecretBytes) -> Result<(), BackendError> {
        self.entry(reference)?
            .set_secret(secret.expose_secret())
            .map_err(classify)
    }
    fn get(&mut self, reference: &CredentialRef) -> Result<SecretBytes, BackendError> {
        let bytes = self.entry(reference)?.get_secret().map_err(classify)?;
        SecretBytes::new(bytes).map_err(|_| BackendError {
            reason: VaultReason::InvalidStoredData,
            uncertain_mutation: false,
        })
    }
    fn delete(&mut self, reference: &CredentialRef) -> Result<(), BackendError> {
        match self.entry(reference)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(classify(error)),
        }
    }
}

#[cfg(target_os = "linux")]
fn open_store() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(zbus_secret_service_keyring_store::Store::new()?)
}
#[cfg(target_os = "macos")]
fn open_store() -> keyring_core::Result<Arc<CredentialStore>> {
    let config = std::collections::HashMap::from([("keychain", "User")]);
    Ok(apple_native_keyring_store::keychain::Store::new_with_configuration(&config)?)
}
#[cfg(target_os = "windows")]
fn open_store() -> keyring_core::Result<Arc<CredentialStore>> {
    // Fixed selectors have no '@'; reject delimiter ambiguity as defense in depth.
    let config =
        std::collections::HashMap::from([("divider", "@"), ("service_no_divider", "true")]);
    Ok(windows_native_keyring_store::Store::new_with_configuration(
        &config,
    )?)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn open_store() -> keyring_core::Result<Arc<CredentialStore>> {
    Err(keyring_core::Error::NoDefaultStore)
}

// Never format an OS error or retain it in the public error chain. Some variants
// contain actual secret bytes. Erase those variants before discarding them.
pub(super) fn classify(error: keyring_core::Error) -> BackendError {
    use keyring_core::Error;
    let (reason, uncertain_mutation) = match error {
        Error::NoEntry => (VaultReason::Missing, false),
        Error::NoStorageAccess(_) => (VaultReason::AccessDeniedOrLocked, false),
        Error::NoDefaultStore | Error::NotSupportedByStore(_) => (VaultReason::Unavailable, false),
        Error::Ambiguous(_) => (VaultReason::Ambiguous, false),
        Error::Invalid(_, _) | Error::TooLong(_, _) => (VaultReason::InvalidStoredData, false),
        Error::BadEncoding(mut bytes) => {
            bytes.zeroize();
            (VaultReason::InvalidStoredData, true)
        }
        Error::BadDataFormat(mut bytes, _) => {
            bytes.zeroize();
            (VaultReason::InvalidStoredData, true)
        }
        Error::BadStoreFormat(_) | Error::PlatformFailure(_) => (VaultReason::Unexpected, true),
        _ => (VaultReason::Unexpected, true),
    };
    BackendError {
        reason,
        uncertain_mutation,
    }
}
