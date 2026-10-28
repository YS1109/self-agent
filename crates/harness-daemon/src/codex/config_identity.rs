//! Effective configuration identity and private redaction material.
//! Credentials are retained only in memory; public identity contains digests.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, ffi::OsString};

pub(super) struct PrivateConfig {
    identity: Value,
    pub secrets: Vec<String>,
    custom_credentials: bool,
    model: Option<String>,
    provider: String,
}

impl PrivateConfig {
    pub fn verify_thread_identity(&self, result: &Value, requested_model: &str) -> Result<()> {
        let actual_model = result["model"]
            .as_str()
            .filter(|value| !value.trim().is_empty() && value.len() <= 256)
            .ok_or_else(|| anyhow::anyhow!("Codex 未返回有效模型标识，未开始新回合"))?;
        let expected_model = if requested_model.trim().is_empty() {
            self.model.as_deref()
        } else {
            Some(requested_model)
        };
        if expected_model.is_some_and(|expected| expected != actual_model) {
            bail!("Codex 有效模型与任务快照不一致，未开始新回合");
        }
        if result["modelProvider"].as_str() != Some(self.provider.as_str()) {
            bail!("Codex 有效模型服务与已核对配置不一致，未开始新回合");
        }
        Ok(())
    }

    pub fn with_account(&self, account: &Value) -> Result<Value> {
        let Some(requires_auth) = account["requiresOpenaiAuth"].as_bool() else {
            bail!("Codex 账户状态响应无效，未启动任务");
        };
        let kind = account["account"]["type"].as_str();
        let reuse_safe = self.custom_credentials
            && !requires_auth
            && (account["account"].is_null() || kind == Some("apiKey"));
        let mut identity = self.identity.clone();
        identity["fingerprint"] = json!(crate::continuation::fingerprint(
            &json!({"version":3,"config":self.identity["fingerprint"],"account":account})
        ));
        // Email/plan are not reliable workspace identity. Hidden credentials,
        // ChatGPT and externally managed cloud auth never qualify for reuse.
        identity["reuse_safe"] = json!(reuse_safe);
        Ok(identity)
    }
}

pub(super) fn from_config(config: &Value) -> Result<PrivateConfig> {
    with_environment(config, |name| std::env::var_os(name))
}

fn with_environment(
    config: &Value,
    read: impl Fn(&str) -> Option<OsString>,
) -> Result<PrivateConfig> {
    if !config.is_object() {
        bail!("Codex 有效配置缺少配置对象");
    }
    let provider = match config
        .get("model_provider")
        .filter(|value| !value.is_null())
    {
        Some(Value::String(value)) if !value.trim().is_empty() && value.len() <= 256 => {
            value.clone()
        }
        None => "openai".to_owned(),
        _ => bail!("Codex 有效配置的模型服务标识无效"),
    };
    let model = match config.get("model").filter(|value| !value.is_null()) {
        Some(Value::String(value)) if !value.trim().is_empty() && value.len() <= 256 => {
            Some(value.clone())
        }
        None => None,
        _ => bail!("Codex 有效配置的模型标识无效"),
    };
    let mut environment = BTreeMap::new();
    let mut secrets = Vec::new();
    let mut custom_credentials = false;
    let builtin = read("OPENAI_API_KEY");
    if let Some(value) = &builtin {
        if let Some(value) = value.to_str() {
            add_secret(&mut secrets, value);
        }
    }
    environment.insert(
        "OPENAI_API_KEY",
        builtin.map(|value| value.as_encoded_bytes().to_vec()),
    );
    if let Some(providers) = config["model_providers"].as_object() {
        for (provider_name, provider) in providers {
            let mut provider_secrets = Vec::new();
            for value in provider["http_headers"]
                .as_object()
                .into_iter()
                .flat_map(|headers| headers.values().filter_map(Value::as_str))
            {
                add_secret(&mut provider_secrets, value);
            }
            if let Some(value) = provider["experimental_bearer_token"].as_str() {
                add_secret(&mut provider_secrets, value);
            }
            let names = provider["env_key"].as_str().into_iter().chain(
                provider["env_http_headers"]
                    .as_object()
                    .into_iter()
                    .flat_map(|headers| headers.values().filter_map(Value::as_str)),
            );
            for name in names {
                if name.is_empty() || name.contains(['=', '\0']) || name.len() > 1024 {
                    bail!("Codex 配置包含无效的环境变量引用");
                }
                let value = read(name);
                if let Some(value) = &value {
                    if let Some(value) = value.to_str() {
                        add_secret(&mut provider_secrets, value);
                    }
                }
                environment.insert(name, value.map(|value| value.as_encoded_bytes().to_vec()));
            }
            if config["model_provider"].as_str() == Some(provider_name)
                && provider["requires_openai_auth"] == false
                // Helper and gateway credentials are opaque/dynamic even when
                // other static headers happen to be present in this provider.
                && provider["auth"].is_null()
                && provider["gateway_oauth"].is_null()
                && !provider_secrets.is_empty()
            {
                custom_credentials = true;
            }
            secrets.extend(provider_secrets);
        }
    }
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    let mut identity = json!({"fingerprint":crate::continuation::fingerprint(
        &json!({"version":3,"config":config,"environment":environment})
    )});
    for field in [
        "model",
        "model_provider",
        "model_context_window",
        "model_auto_compact_token_limit",
        "sandbox_mode",
        "approval_policy",
    ] {
        if let Some(value) = config
            .get(field)
            .filter(|value| value.is_string() || value.is_number() || value.is_null())
        {
            if value.as_str().is_none_or(|text| {
                text.len() <= 256
                    && !crate::configuration::looks_like_credential(text)
                    && !secrets.iter().any(|secret| text.contains(secret))
            }) {
                identity[field] = value.clone();
            }
        }
    }
    Ok(PrivateConfig {
        identity,
        secrets,
        custom_credentials,
        model,
        provider,
    })
}

fn add_secret(secrets: &mut Vec<String>, value: &str) {
    if !value.is_empty() {
        secrets.push(value.into());
        if let Some(token) = value
            .strip_prefix("Bearer ")
            .filter(|token| !token.is_empty())
        {
            secrets.push(token.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn referenced_provider_environment_changes_identity_without_exposing_values() {
        let config = json!({"model_provider":"custom", "model_providers":{"custom":{
            "requires_openai_auth":false,"env_key":"EXAMPLE_KEY", "env_http_headers":{"Authorization":"EXAMPLE_HEADER"}
        }}});
        let make = |key: &str, header: &str, builtin: &str| {
            with_environment(&config, |name| match name {
                "OPENAI_API_KEY" => Some(OsString::from(builtin)),
                "EXAMPLE_KEY" => Some(OsString::from(key)),
                "EXAMPLE_HEADER" => Some(OsString::from(header)),
                _ => panic!("unreferenced environment read"),
            })
            .unwrap()
            .with_account(&json!({"account":null,"requiresOpenaiAuth":false}))
            .unwrap()
        };
        let identity = make("private-key-one", "private-header-one", "builtin-one");
        assert_ne!(
            identity,
            make("private-key-two", "private-header-one", "builtin-one")
        );
        assert_ne!(
            identity,
            make("private-key-one", "private-header-two", "builtin-one")
        );
        assert_ne!(
            identity,
            make("private-key-one", "private-header-one", "builtin-two")
        );
        assert!(!identity.to_string().contains("private-"));
        assert!(!identity.to_string().contains("EXAMPLE_"));
        assert_eq!(identity["reuse_safe"], true);
    }
    #[test]
    fn default_model_and_provider_must_match_effective_config() {
        let config = with_environment(&json!({"model":"configured-model"}), |_| None).unwrap();
        config
            .verify_thread_identity(
                &json!({"model":"configured-model","modelProvider":"openai"}),
                "",
            )
            .unwrap();
        assert!(config
            .verify_thread_identity(
                &json!({"model":"old-saved-model","modelProvider":"openai"}),
                ""
            )
            .is_err());
        assert!(config
            .verify_thread_identity(
                &json!({"model":"configured-model","modelProvider":"other"}),
                ""
            )
            .is_err());
        config
            .verify_thread_identity(
                &json!({"model":"explicit-model","modelProvider":"openai"}),
                "explicit-model",
            )
            .unwrap();
    }

    #[test]
    fn dynamic_helper_or_gateway_auth_never_qualifies_for_thread_reuse() {
        for field in ["auth", "gateway_oauth"] {
            for dynamic in [
                json!({"command":"must-not-execute"}),
                json!({}),
                json!(false),
                json!("malformed"),
            ] {
                let mut config = json!({"model_provider":"custom","model_providers":{"custom":{
                    "requires_openai_auth":false,"http_headers":{"Authorization":"Bearer known-static-value"}
                }}});
                config["model_providers"]["custom"][field] = dynamic;
                let identity = with_environment(&config, |_| None)
                    .unwrap()
                    .with_account(&json!({"account":null,"requiresOpenaiAuth":false}))
                    .unwrap();
                assert_eq!(identity["reuse_safe"], false, "{field}");
                assert!(!identity.to_string().contains("known-static-value"));
            }
        }
    }

    #[test]
    fn private_credentials_are_collected_and_unknown_accounts_never_reuse() {
        let config = json!({"model_provider":"custom","model_providers":{"custom":{
            "requires_openai_auth":false,"http_headers":{"Authorization":"Bearer private-header"},
            "experimental_bearer_token":"private-token","env_key":"KEY","env_http_headers":{"X-Token":"HEADER"}
        }}});
        let private = with_environment(&config, |name| {
            Some(OsString::from(format!("private-{name}")))
        })
        .unwrap();
        for secret in [
            "Bearer private-header",
            "private-header",
            "private-token",
            "private-KEY",
            "private-HEADER",
            "private-OPENAI_API_KEY",
        ] {
            assert!(private.secrets.contains(&secret.to_owned()));
        }
        for account in [
            json!({"type":"chatgpt","email":"user@example.test","planType":"team"}),
            json!({"type":"gcp"}),
            json!({"type":"amazonBedrock"}),
            json!({"type":"unknown"}),
        ] {
            let identity = private
                .with_account(&json!({"account":account,"requiresOpenaiAuth":false}))
                .unwrap();
            assert_eq!(identity["reuse_safe"], false);
            assert!(!identity.to_string().contains("user@example"));
        }
    }
}
