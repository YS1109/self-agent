//! Official QQ adapter components. The daemon integration owns configuration,
//! OS-credential intent lifecycle, SQLite transaction wiring and worker startup.
//! Source routing never confers the daemon's local-owner authority.
pub(crate) mod commands;
pub(crate) mod events;
pub(crate) mod persistence;
pub(crate) mod policy;
pub(crate) mod presentation;
pub(crate) mod transport;

pub(crate) mod admin;
pub(crate) mod configuration;
pub(crate) mod credentials;
pub(crate) mod engine;
pub(crate) mod notifications;
pub(crate) mod runtime;

pub(crate) fn credential_binding(
    owner: &str,
    digest: &str,
) -> anyhow::Result<crate::capabilities::vault::VaultBinding> {
    Ok(crate::capabilities::vault::VaultBinding::service(
        owner,
        "qq_client_secret",
        digest,
    )?)
}

pub(crate) mod api;
pub(crate) mod daemon;
