//! Private continuation metadata. Fingerprints are never credentials or a reason
//! to trust unknown in-flight writes; they only decide when to discard caches.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

// Bump when effective tool/approval policy changes; cache reuse never grants authority.
const CONTINUATION_POLICY_REVISION: &str = "v0.8-managed-capabilities-1";

pub(crate) fn fingerprint(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("JSON value serializes"))
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct NativeSummary {
    pub summary: String,
    pub covered_messages: usize,
    pub prefix_fingerprint: String,
}

pub(crate) fn known_secrets(settings: &crate::Settings) -> Vec<String> {
    let mut secrets = vec![settings.api_key.clone()];
    for value in settings.provider_headers.values() {
        if let Ok(value) = value.to_str() {
            secrets.push(value.to_owned());
            if let Some(token) = value.strip_prefix("Bearer ") {
                secrets.push(token.to_owned());
            }
        }
    }
    for name in ["HARNESS_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(value) = std::env::var(name) {
            secrets.push(value);
        }
    }
    secrets.retain(|value| !value.is_empty());
    secrets
}

pub(crate) fn native_identity(
    settings: &crate::Settings,
    root: &std::path::Path,
    agent: &crate::profiles::AgentProfile,
    context: &harness_core::ModelContextConfig,
) -> anyhow::Result<String> {
    let settings = crate::configuration::resolve(settings)?;
    let headers: std::collections::BTreeMap<_, _> = settings
        .provider_headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or_default()))
        .collect();
    // Secret-bearing fields are digested in memory, never stored or returned.
    Ok(fingerprint(
        &serde_json::json!({"version":1,"policy_revision":CONTINUATION_POLICY_REVISION,"project":root,
        "provider_source":settings.provider_source,"endpoint":settings.base_url,"model":settings.model,
        "api_key":settings.api_key,"headers":headers,"role":agent,"context":context,
        "commands":std::env::var("HARNESS_ALLOW_COMMANDS").as_deref()==Ok("1") }),
    ))
}

pub(crate) async fn codex_identity(
    binary: &str,
    root: &std::path::Path,
    agent: &crate::profiles::AgentProfile,
    context: &harness_core::ModelContextConfig,
    cancelled: &impl Fn() -> bool,
) -> anyhow::Result<(String, Value)> {
    let identity =
        crate::codex::probe_config_identity_with_control(binary, root, cancelled).await?;
    let version = crate::codex::probe_version_with_control(binary, root, cancelled).await?;
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|path| path.join(".codex")));
    let binary_path = if std::path::Path::new(binary).components().count() > 1 {
        std::fs::canonicalize(binary).ok()
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(binary))
                .find_map(|candidate| std::fs::canonicalize(candidate).ok())
        })
    };
    let digest = fingerprint(
        &serde_json::json!({"version":1,"policy_revision":CONTINUATION_POLICY_REVISION,"binary":binary,"binary_path":binary_path,
        "binary_version":version,"home":home,
        "project":root,"role":agent,"context":context,"effective":identity,
        "sandbox":"workspace-write","network":false,"approval":"never"}),
    );
    Ok((digest, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{AgentProfile, ModelContextConfig};

    #[test]
    fn native_identity_changes_with_each_effective_configuration_boundary() {
        let root =
            std::env::temp_dir().join(format!("self-agent-identity-{}", harness_core::new_id()));
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let settings = crate::Settings {
            base_url: "http://127.0.0.1:1234/v1".into(),
            model: "fixture-model".into(),
            api_key: "private-fixture-key".into(),
            ..Default::default()
        };
        let agent = AgentProfile::default();
        let context = ModelContextConfig::default();
        let original = native_identity(&settings, &root, &agent, &context).unwrap();
        assert_eq!(original.len(), 64);
        assert!(!original.contains(&settings.api_key));
        for field in ["key", "endpoint", "model", "header"] {
            let mut changed = settings.clone();
            match field {
                "key" => changed.api_key = "rotated-private-key".into(),
                "endpoint" => changed.base_url = "http://127.0.0.1:4321/v1".into(),
                "model" => changed.model = "other-model".into(),
                _ => {
                    changed
                        .provider_headers
                        .insert("x-fixture-key", "private-header".parse().unwrap());
                }
            }
            assert_ne!(
                original,
                native_identity(&changed, &root, &agent, &context).unwrap(),
                "{field}"
            );
        }
        assert_ne!(
            original,
            native_identity(&settings, &other, &agent, &context).unwrap()
        );
        let mut role = agent.clone();
        role.instructions = "Changed role policy".into();
        assert_ne!(
            original,
            native_identity(&settings, &root, &role, &context).unwrap()
        );
        let mut policy = context.clone();
        policy.context_window += 1;
        assert_ne!(
            original,
            native_identity(&settings, &root, &agent, &policy).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
