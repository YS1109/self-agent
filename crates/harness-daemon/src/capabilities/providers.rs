//! Validation and immutable identity helpers for owner-managed providers. No
//! database, environment, auth-file or OS-vault access occurs in this module.
//! Production request builders must disable redirects and recheck the pinned
//! revision immediately before every model/discovery request, including retries.
use super::vault::{CredentialRef, SecretBytes, VaultBinding};
pub(crate) use harness_core::ProviderDefinition;
use reqwest::Url;
use std::net::IpAddr;
use uuid::Uuid;

const MAX_ENDPOINT: usize = 2048;
const MAX_REVISION: u64 = 9_007_199_254_740_991;

/// Canonical and already checked, but not an authority to send requests. Kept
/// non-Debug to discourage accidental configuration logging at call sites.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ValidatedProvider {
    definition: ProviderDefinition,
}
impl ValidatedProvider {
    pub(crate) fn definition(&self) -> &ProviderDefinition {
        &self.definition
    }
    pub(crate) fn base_url(&self) -> &str {
        &self.definition.base_url
    }
    pub(crate) fn models_url(&self) -> &str {
        &self.definition.models_endpoint
    }
    pub(crate) fn model_for_task(
        &self,
        role_override: &str,
        known_secrets: &[String],
    ) -> Result<String, ProviderError> {
        let model = if role_override.is_empty() {
            &self.definition.model
        } else {
            role_override
        };
        validate_model(model, known_secrets)?;
        if model.trim().is_empty() {
            return Err(ProviderError::InvalidModel);
        }
        Ok(model.to_owned())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderError {
    InvalidId,
    InvalidRevision,
    InvalidName,
    InvalidEndpoint,
    UnsafeHttp,
    CrossOrigin,
    InvalidModel,
    KnownCredential,
    BindingMismatch,
    Revoked,
    InvalidSecret,
}
impl ProviderError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidId => "invalid_provider_id",
            Self::InvalidRevision => "invalid_provider_revision",
            Self::InvalidName => "invalid_provider_name",
            Self::InvalidEndpoint => "invalid_provider_endpoint",
            Self::UnsafeHttp => "insecure_http_unconfirmed",
            Self::CrossOrigin => "cross_origin_models_endpoint",
            Self::InvalidModel => "invalid_provider_model",
            Self::KnownCredential => "credential_in_metadata",
            Self::BindingMismatch => "credential_binding_mismatch",
            Self::Revoked => "provider_revoked",
            Self::InvalidSecret => "invalid_provider_secret",
        }
    }
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::InvalidId => "模型服务标识必须是规范 UUID",
            Self::InvalidRevision => "模型服务修订无效",
            Self::InvalidName => "模型服务名称必须为 1–80 个字符且不能包含控制字符",
            Self::InvalidEndpoint => "模型服务地址或模型列表端点无效，不能包含凭证、查询参数或片段",
            Self::UnsafeHttp => "远程明文 HTTP 会暴露凭据和内容，必须由所有者显式确认",
            Self::CrossOrigin => "模型列表端点必须与模型服务使用相同协议、主机和端口",
            Self::InvalidModel => "模型名称无效或尚未选择模型",
            Self::KnownCredential => "模型服务元数据不能包含访问凭据",
            Self::BindingMismatch => "凭据引用与模型服务或地址不匹配",
            Self::Revoked => "任务固定的模型服务或凭据已改变，未发送请求且没有自动切换服务",
            Self::InvalidSecret => "模型服务凭据格式无效，未回显原始内容",
        }
    }
}
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}
impl std::error::Error for ProviderError {}

fn known_credential(text: &str, known: &[String]) -> bool {
    known
        .iter()
        .any(|secret| !secret.is_empty() && text.contains(secret))
        || text.split_whitespace().any(|part| {
            (part.starts_with("sk-") && part.len() >= 23)
                || part.starts_with("ghp_")
                || part.starts_with("github_pat_")
        })
        || text.trim().starts_with("Bearer ")
}
fn validate_model(value: &str, known: &[String]) -> Result<(), ProviderError> {
    if known_credential(value, known) {
        return Err(ProviderError::KnownCredential);
    }
    if value.len() > 200 || value.chars().any(char::is_control) {
        return Err(ProviderError::InvalidModel);
    }
    Ok(())
}
fn valid_id(value: &str) -> bool {
    value.len() == 36
        && Uuid::parse_str(value).is_ok_and(|uuid| uuid.hyphenated().to_string() == value)
}
fn endpoint_text(value: &str) -> Result<(), ProviderError> {
    if value.len() > MAX_ENDPOINT || value.trim() != value || value.chars().any(char::is_control) {
        return Err(ProviderError::InvalidEndpoint);
    }
    Ok(())
}
fn valid_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}
fn loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub(crate) fn validate_definition(
    mut definition: ProviderDefinition,
    known_secrets: &[String],
) -> Result<ValidatedProvider, ProviderError> {
    for value in [
        &definition.name,
        &definition.base_url,
        &definition.models_endpoint,
        &definition.model,
    ] {
        // Inspect raw strings, never their JSON-escaped representations.
        if known_credential(value, known_secrets) {
            return Err(ProviderError::KnownCredential);
        }
    }
    if definition.name.trim().is_empty()
        || definition.name.chars().count() > 80
        || definition.name.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidName);
    }
    endpoint_text(&definition.base_url)?;
    endpoint_text(&definition.models_endpoint)?;
    let mut base = Url::parse(&definition.base_url).map_err(|_| ProviderError::InvalidEndpoint)?;
    if !valid_url(&base) {
        return Err(ProviderError::InvalidEndpoint);
    }
    if base.scheme() == "http" && !loopback(&base) && !definition.allow_insecure_http {
        return Err(ProviderError::UnsafeHttp);
    }
    let path = base.path().trim_end_matches('/').to_owned();
    base.set_path(if path.is_empty() { "/" } else { &path });
    definition.base_url = base.as_str().trim_end_matches('/').to_owned();
    let directory = Url::parse(&format!("{}/", definition.base_url))
        .map_err(|_| ProviderError::InvalidEndpoint)?;
    let models = directory
        .join(if definition.models_endpoint.is_empty() {
            "models"
        } else {
            &definition.models_endpoint
        })
        .map_err(|_| ProviderError::InvalidEndpoint)?;
    if !valid_url(&models) || models.as_str().len() > MAX_ENDPOINT {
        return Err(ProviderError::InvalidEndpoint);
    }
    if models.origin() != base.origin() {
        return Err(ProviderError::CrossOrigin);
    }
    definition.models_endpoint = models.into();
    validate_model(&definition.model, known_secrets)?;
    Ok(ValidatedProvider { definition })
}

pub(crate) fn credential_binding(
    id: &str,
    provider: &ValidatedProvider,
) -> Result<VaultBinding, ProviderError> {
    if !valid_id(id) {
        return Err(ProviderError::InvalidId);
    }
    VaultBinding::provider(id, "api_key", provider.base_url())
        .map_err(|_| ProviderError::BindingMismatch)
}

/// Only unchanged canonical service AND discovery endpoints can retain a key.
/// Even same-origin model-list path changes detach it: paths may name different
/// tenants. Name/model-only edits preserve the key but still revoke old tasks.
pub(crate) fn retained_credential(
    id: &str,
    previous: &ValidatedProvider,
    next: &ValidatedProvider,
    reference: Option<&CredentialRef>,
) -> Result<Option<CredentialRef>, ProviderError> {
    let old_binding = credential_binding(id, previous)?;
    if let Some(reference) = reference {
        CredentialRef::parse(reference.as_str(), &old_binding)
            .map_err(|_| ProviderError::BindingMismatch)?;
    }
    Ok(
        if previous.base_url() == next.base_url() && previous.models_url() == next.models_url() {
            reference.cloned()
        } else {
            None
        },
    )
}

/// Non-serialized private enqueue snapshot. The selected model may come from a
/// role override; the definition remains pinned to detect all profile edits.
pub(crate) struct PinnedProvider {
    id: String,
    revision: u64,
    definition: ValidatedProvider,
    credential_ref: Option<CredentialRef>,
    model: String,
}
impl PinnedProvider {
    pub(crate) fn new(
        id: &str,
        revision: u64,
        enabled: bool,
        definition: ValidatedProvider,
        credential_ref: Option<CredentialRef>,
        role_model: &str,
        known_secrets: &[String],
    ) -> Result<Self, ProviderError> {
        if !valid_id(id) {
            return Err(ProviderError::InvalidId);
        }
        if revision == 0 || revision > MAX_REVISION {
            return Err(ProviderError::InvalidRevision);
        }
        if !enabled {
            return Err(ProviderError::Revoked);
        }
        let binding = credential_binding(id, &definition)?;
        if let Some(reference) = &credential_ref {
            CredentialRef::parse(reference.as_str(), &binding)
                .map_err(|_| ProviderError::BindingMismatch)?;
        }
        let model = definition.model_for_task(role_model, known_secrets)?;
        Ok(Self {
            id: id.into(),
            revision,
            definition,
            credential_ref,
            model,
        })
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn definition(&self) -> &ValidatedProvider {
        &self.definition
    }
    pub(crate) fn credential_ref(&self) -> Option<&CredentialRef> {
        self.credential_ref.as_ref()
    }
    pub(crate) fn model(&self) -> &str {
        &self.model
    }
    pub(crate) fn binding(&self) -> Result<VaultBinding, ProviderError> {
        credential_binding(&self.id, &self.definition)
    }
    pub(crate) fn check_current(
        &self,
        id: &str,
        revision: u64,
        enabled: bool,
        definition: &ValidatedProvider,
        reference: Option<&CredentialRef>,
    ) -> Result<(), ProviderError> {
        if !enabled
            || self.id != id
            || self.revision != revision
            || &self.definition != definition
            || self.credential_ref.as_ref() != reference
        {
            return Err(ProviderError::Revoked);
        }
        Ok(())
    }
}

/// Native Chat Completions uses bearer authentication. Reject invalid UTF-8 and
/// header bytes before converting a vault value into executor-private state.
pub(crate) fn validate_api_key(secret: &SecretBytes) -> Result<(), ProviderError> {
    let value =
        std::str::from_utf8(secret.expose_secret()).map_err(|_| ProviderError::InvalidSecret)?;
    if value.trim().is_empty()
        || value.trim() != value
        || !value.is_ascii()
        || value.bytes().any(|b| !b.is_ascii_graphic())
    {
        return Err(ProviderError::InvalidSecret);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::vault::FreshCredential;
    use super::*;
    const ID: &str = "94a846e4-7ac2-46d7-ac1d-fde573fd6e12";
    fn definition() -> ProviderDefinition {
        ProviderDefinition {
            name: "测试服务".into(),
            base_url: "https://example.invalid/v1".into(),
            models_endpoint: String::new(),
            model: "fixture-model".into(),
            allow_insecure_http: false,
        }
    }
    fn validated() -> ValidatedProvider {
        validate_definition(definition(), &[]).unwrap()
    }
    #[test]
    fn canonical_urls_are_same_origin_and_do_not_hide_unsafe_parts() {
        let mut input = definition();
        input.base_url = "HTTPS://EXAMPLE.INVALID:443/v1///".into();
        input.models_endpoint = "./models".into();
        let value = validate_definition(input, &[]).unwrap();
        assert_eq!(value.base_url(), "https://example.invalid/v1");
        assert_eq!(value.models_url(), "https://example.invalid/v1/models");
        for base in [
            "https://user:password@example.invalid/v1",
            "https://example.invalid/v1?",
            "https://example.invalid/v1#",
            "file:///tmp/model",
            "https://example.invalid/v1\n",
            " https://example.invalid/v1",
        ] {
            let mut input = definition();
            input.base_url = base.into();
            assert!(validate_definition(input, &[]).is_err());
        }
        for endpoint in [
            "https://other.invalid/models",
            "http://example.invalid/models",
            "https://example.invalid:444/models",
            "//other.invalid/models",
            "models?key=secret",
            "models#fragment",
            "https://user@example.invalid/models",
        ] {
            let mut input = definition();
            input.models_endpoint = endpoint.into();
            assert!(validate_definition(input, &[]).is_err());
        }
    }
    #[test]
    fn only_explicit_remote_http_preserves_existing_owner_boundary() {
        for host in ["127.0.0.1", "127.9.8.7", "[::1]", "localhost"] {
            let mut input = definition();
            input.base_url = format!("http://{host}:1234/v1");
            assert!(validate_definition(input, &[]).is_ok());
        }
        for host in [
            "example.invalid",
            "localhost.evil.invalid",
            "192.168.1.2",
            "[::]",
        ] {
            let mut input = definition();
            input.base_url = format!("http://{host}/v1");
            assert_eq!(
                validate_definition(input.clone(), &[]).err(),
                Some(ProviderError::UnsafeHttp)
            );
            input.allow_insecure_http = true;
            assert!(validate_definition(input, &[]).is_ok());
        }
    }
    #[test]
    fn metadata_secrets_and_unknown_fields_cannot_enter_profiles() {
        for secret in [
            "fixture-known-key",
            "quoted\"key",
            "backslash\\key",
            "newline\nkey",
        ] {
            for field in ["name", "base_url", "models_endpoint", "model"] {
                let mut input = serde_json::to_value(definition()).unwrap();
                input[field] = serde_json::json!(format!("prefix{secret}suffix"));
                let input = serde_json::from_value(input).unwrap();
                assert_eq!(
                    validate_definition(input, &[secret.into()]).err(),
                    Some(ProviderError::KnownCredential)
                );
            }
        }
        for field in ["api_key", "credential_ref", "provider_headers"] {
            let mut input = serde_json::to_value(definition()).unwrap();
            input[field] = serde_json::json!("not accepted");
            assert!(serde_json::from_value::<ProviderDefinition>(input).is_err());
        }
    }
    #[test]
    fn endpoint_changes_detach_keys_and_reference_binding_is_not_ambient() {
        let previous = validated();
        let binding = credential_binding(ID, &previous).unwrap();
        let fresh = FreshCredential::new(
            &binding,
            SecretBytes::new(b"test-fixture-key".to_vec()).unwrap(),
        );
        let reference = fresh.reference();
        assert!(
            retained_credential(ID, &previous, &previous, Some(reference))
                .unwrap()
                .is_some()
        );
        for base in [
            "https://other.invalid/v1",
            "http://example.invalid/v1",
            "https://example.invalid:444/v1",
            "https://example.invalid/v2",
        ] {
            let mut input = definition();
            input.base_url = base.into();
            input.allow_insecure_http = true;
            let next = validate_definition(input, &[]).unwrap();
            assert!(retained_credential(ID, &previous, &next, Some(reference))
                .unwrap()
                .is_none());
            assert!(
                PinnedProvider::new(ID, 1, true, next, Some(reference.clone()), "", &[]).is_err()
            );
        }
        let mut changed = previous.definition().clone();
        changed.models_endpoint = "/different-tenant/models".into();
        let changed = validate_definition(changed, &[]).unwrap();
        assert!(
            retained_credential(ID, &previous, &changed, Some(reference))
                .unwrap()
                .is_none()
        );
        let other = "c94e83b9-aa43-4ad8-925c-7255fbad1f73";
        assert!(retained_credential(other, &previous, &previous, Some(reference)).is_err());
    }
    #[test]
    fn pinned_profile_and_rotation_are_revoked_before_next_send() {
        let definition = validated();
        let binding = credential_binding(ID, &definition).unwrap();
        let fresh = FreshCredential::new(&binding, SecretBytes::new(b"key-one".to_vec()).unwrap());
        let reference = fresh.reference().clone();
        let pinned = PinnedProvider::new(
            ID,
            7,
            true,
            definition.clone(),
            Some(reference.clone()),
            "role-model",
            &[],
        )
        .unwrap();
        assert_eq!(pinned.model(), "role-model");
        pinned
            .check_current(ID, 7, true, &definition, Some(&reference))
            .unwrap();
        assert!(pinned
            .check_current(ID, 8, true, &definition, Some(&reference))
            .is_err());
        assert!(pinned
            .check_current(ID, 7, false, &definition, Some(&reference))
            .is_err());
        assert!(pinned
            .check_current(ID, 7, true, &definition, None)
            .is_err());
        let rotated =
            FreshCredential::new(&binding, SecretBytes::new(b"key-two".to_vec()).unwrap());
        assert!(pinned
            .check_current(ID, 7, true, &definition, Some(rotated.reference()))
            .is_err());
        let mut changed = definition.definition().clone();
        changed.models_endpoint = "/alternate/models".into();
        let changed = validate_definition(changed, &[]).unwrap();
        assert!(pinned
            .check_current(ID, 7, true, &changed, Some(&reference))
            .is_err());
    }
    #[test]
    fn bounded_models_names_ids_and_header_secrets_are_validated_without_echo() {
        let mut input = definition();
        input.name = "x".repeat(81);
        assert!(validate_definition(input, &[]).is_err());
        let mut input = definition();
        input.model = "x".repeat(201);
        assert!(validate_definition(input, &[]).is_err());
        let mut input = definition();
        input.model = String::new();
        let value = validate_definition(input, &[]).unwrap();
        assert!(value.model_for_task("", &[]).is_err());
        assert_eq!(
            value.model_for_task("role-model", &[]).unwrap(),
            "role-model"
        );
        assert!(PinnedProvider::new("invalid", 1, true, validated(), None, "", &[]).is_err());
        assert!(PinnedProvider::new(ID, 0, true, validated(), None, "", &[]).is_err());
        for secret in [
            b"line\nsecret".to_vec(),
            b" padded".to_vec(),
            vec![255],
            "非ASCII".as_bytes().to_vec(),
        ] {
            assert_eq!(
                validate_api_key(&SecretBytes::new(secret).unwrap()).unwrap_err(),
                ProviderError::InvalidSecret
            );
        }
        validate_api_key(&SecretBytes::new(b"test-fixture-key".to_vec()).unwrap()).unwrap();
        assert!(!ProviderError::InvalidSecret
            .to_string()
            .contains("test-fixture-key"));
    }
}
