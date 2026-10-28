use std::collections::HashSet;

use anyhow::{bail, Context, Result};

pub use harness_core::AgentProfile;

pub fn default_agents() -> Vec<AgentProfile> {
    vec![AgentProfile::default()]
}

pub fn default_agent_id() -> String {
    "coder".into()
}

pub fn validate_agents(agents: &[AgentProfile], default_id: &str) -> Result<()> {
    if agents.is_empty() || agents.len() > 16 {
        bail!("请配置 1 至 16 个子智能体角色");
    }
    let mut ids = HashSet::new();
    for agent in agents {
        if agent.id.is_empty()
            || agent.id.len() > 64
            || !agent
                .id
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
        {
            bail!("角色 ID 必须为 1 至 64 个英文字母、数字、下划线或连字符");
        }
        if !ids.insert(&agent.id) {
            bail!("角色 ID 重复：{}", agent.id);
        }
        if agent.name.trim().is_empty() || agent.name.len() > 200 {
            bail!("角色名称必须为 1 至 200 字节的文本");
        }
        if agent.instructions.len() > 32_000 || agent.model.len() > 200 {
            bail!("角色指令不能超过 32000 字节，模型名不能超过 200 字节");
        }
        if let Some(id) = &agent.provider_id {
            if agent.executor != "native" || uuid::Uuid::parse_str(id).is_err() {
                bail!("只有 Native 角色可选择有效的命名模型服务 UUID");
            }
        }
        crate::configuration::validate_model(&agent.model)?;
        crate::model_config::validate_overrides(agent)?;
        if let Some(controls) = &agent.controls {
            crate::store::execution::resolve_controls(controls, None, None, &agent.executor)?;
        }
        if !matches!(agent.executor.as_str(), "native" | "codex") {
            bail!("角色执行器仅支持 native 或 codex");
        }
    }
    resolve_agent(agents, default_id, None)?;
    Ok(())
}

pub fn resolve_agent(
    agents: &[AgentProfile],
    default_id: &str,
    requested: Option<&str>,
) -> Result<AgentProfile> {
    let id = requested.unwrap_or(default_id);
    agents
        .iter()
        .find(|agent| agent.id == id)
        .cloned()
        .with_context(|| format!("未找到子智能体角色：{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_missing_fields_are_compatible() {
        validate_agents(&default_agents(), &default_agent_id()).unwrap();
        let profile: AgentProfile = serde_json::from_str(r#"{"id":"reviewer"}"#).unwrap();
        assert_eq!(profile.executor, "native");
        assert_eq!(profile.id, "reviewer");
        assert!(resolve_agent(&default_agents(), "coder", Some("unknown")).is_err());
    }
}
