use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use harness_core::{AgentProfile, ModelContextConfig};

pub(crate) fn validate_registry(registry: &BTreeMap<String, ModelContextConfig>) -> Result<()> {
    if registry.len() > 2000 {
        bail!("模型上下文配置不能超过 2000 项");
    }
    for (model, config) in registry {
        crate::configuration::validate_model(model)?;
        if model.trim().is_empty() || model.trim() != model {
            bail!("上下文配置的模型 ID 不能为空或包含首尾空白");
        }
        validate_values(
            Some(config.context_window),
            Some(config.compression_threshold),
        )
        .context("模型上下文配置无效")?;
    }
    Ok(())
}

pub(crate) fn validate_overrides(agent: &AgentProfile) -> Result<()> {
    validate_values(agent.context_window, agent.compression_threshold)
        .context("子智能体上下文配置无效")
}

fn validate_values(context_window: Option<u32>, compression_threshold: Option<u8>) -> Result<()> {
    if context_window.is_some_and(|value| !(1024..=2_000_000).contains(&value)) {
        bail!("上下文窗口必须为 1024 至 2000000 token");
    }
    if compression_threshold.is_some_and(|value| !(10..=95).contains(&value)) {
        bail!("上下文压缩阈值必须为 10 至 95 的百分比");
    }
    Ok(())
}

// Resolve only after the executor has selected its effective model ID.
pub(crate) fn resolve(
    registry: &BTreeMap<String, ModelContextConfig>,
    model: &str,
    agent: Option<&AgentProfile>,
) -> Result<ModelContextConfig> {
    crate::configuration::validate_model(model)?;
    let mut config = registry.get(model).cloned().unwrap_or_default();
    if let Some(agent) = agent {
        validate_overrides(agent)?;
        if let Some(value) = agent.context_window {
            config.context_window = value;
        }
        if let Some(value) = agent.compression_threshold {
            config.compression_threshold = value;
        }
    }
    validate_values(
        Some(config.context_window),
        Some(config.compression_threshold),
    )?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_policy_is_shared_and_role_overrides_each_value_independently() {
        let registry = BTreeMap::from([(
            "fixture-model".into(),
            ModelContextConfig {
                context_window: 64_000,
                compression_threshold: 70,
            },
        )]);
        validate_registry(&registry).unwrap();
        let policy = resolve(&registry, "fixture-model", None).unwrap();
        assert_eq!(policy.context_window, 64_000);
        assert_eq!(policy.compression_threshold, 70);
        let mut agent = AgentProfile {
            context_window: Some(32_000),
            ..Default::default()
        };
        let policy = resolve(&registry, "fixture-model", Some(&agent)).unwrap();
        assert_eq!(policy.context_window, 32_000);
        assert_eq!(policy.compression_threshold, 70);
        agent.context_window = None;
        agent.compression_threshold = Some(50);
        let policy = resolve(&registry, "fixture-model", Some(&agent)).unwrap();
        assert_eq!(policy.context_window, 64_000);
        assert_eq!(policy.compression_threshold, 50);
        let fallback = resolve(&registry, "unknown-model", None).unwrap();
        assert_eq!(fallback.context_window, 128_000);
        assert_eq!(fallback.compression_threshold, 80);
    }

    #[test]
    fn context_validation_rejects_invalid_values_and_secret_model_ids() {
        for window in [0, 1023, 2_000_001] {
            assert!(validate_overrides(&AgentProfile {
                context_window: Some(window),
                ..Default::default()
            })
            .is_err());
        }
        for threshold in [0, 9, 96, 255] {
            assert!(validate_overrides(&AgentProfile {
                compression_threshold: Some(threshold),
                ..Default::default()
            })
            .is_err());
        }
        for id in ["", " model", "sk-fixture-secret-model-name"] {
            assert!(validate_registry(&BTreeMap::from([(
                id.into(),
                ModelContextConfig::default()
            )]))
            .is_err());
        }
        for (window, threshold) in [(1024, 10), (2_000_000, 95)] {
            validate_overrides(&AgentProfile {
                context_window: Some(window),
                compression_threshold: Some(threshold),
                ..Default::default()
            })
            .unwrap();
        }
    }
}
