//! Outbound-only self-hosted gateway. Remote browser RPC is not owner authority.
pub(crate) use harness_core::gateway as wire;
pub(crate) mod admin;
pub(crate) mod api;
pub(crate) mod configuration;
pub(crate) mod credentials;
pub(crate) mod daemon;
pub(crate) mod persistence;
pub(crate) mod policy;
pub(crate) mod resources;
pub(crate) mod transport;

pub(crate) fn credential_binding(
    owner: &str,
    digest: &str,
) -> anyhow::Result<crate::capabilities::vault::VaultBinding> {
    Ok(crate::capabilities::vault::VaultBinding::service(
        owner,
        configuration::CREDENTIAL_PURPOSE,
        digest,
    )?)
}
