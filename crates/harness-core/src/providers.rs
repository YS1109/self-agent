//! Public provider metadata only. Vault references and secret values stay private.
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub models_endpoint: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub allow_insecure_http: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct NamedProviderView {
    pub id: String,
    pub revision: u64,
    pub definition: ProviderDefinition,
    pub enabled: bool,
    pub credential_configured: bool,
    /// Binding metadata is not proof that the OS vault is currently accessible.
    pub credential_status: String,
    pub orphaned_credentials: usize,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderReference {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub model: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CredentialOperationView {
    pub id: String,
    pub owner_kind: String,
    pub owner_id: String,
    pub kind: String,
    pub status: String,
    pub reason: Option<String>,
    pub can_cleanup: bool,
    #[serde(default)]
    pub cleanup_status: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct NamedProviders {
    pub providers: Vec<NamedProviderView>,
    pub credential_operations: Vec<CredentialOperationView>,
}
