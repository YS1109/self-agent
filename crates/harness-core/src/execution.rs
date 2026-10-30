//! Durable, executor-independent task controls. These are limits, never grants
//! beyond the daemon's configured execution capabilities.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Allow,
    Ask,
    Deny,
}

impl PermissionMode {
    pub fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Ask, _) | (_, Self::Ask) => Self::Ask,
            _ => Self::Allow,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskPermissions {
    pub read_files: bool,
    pub write_files: PermissionMode,
    /// Trusted local shell, NOT a filesystem or network sandbox. The daemon's
    /// HARNESS_ALLOW_COMMANDS opt-in remains a separate mandatory ceiling.
    pub run_commands: PermissionMode,
    /// Application-managed MCP only; never a promise about external Codex tools.
    pub managed_tools: PermissionMode,
}

impl Default for TaskPermissions {
    fn default() -> Self {
        Self {
            read_files: true,
            write_files: PermissionMode::Allow,
            run_commands: PermissionMode::Allow,
            managed_tools: PermissionMode::Deny,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionBudget {
    /// Native model requests, including context-compaction requests. Codex
    /// cannot expose this counter and rejects an explicit restriction.
    pub max_model_requests: u32,
    pub max_runtime_seconds: u32,
    pub max_retries: u32,
    pub max_repair_attempts: u32,
    pub max_output_bytes: u32,
    pub max_external_calls: u32,
}

impl Default for ExecutionBudget {
    fn default() -> Self {
        Self {
            max_model_requests: 32,
            max_runtime_seconds: 900,
            max_retries: 0,
            max_repair_attempts: 2,
            max_output_bytes: 256 * 1024,
            max_external_calls: 32,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskControls {
    pub budget: ExecutionBudget,
    pub permissions: TaskPermissions,
    /// These exact commands must have successful post-mutation evidence before
    /// a Native task can be marked completed. Empty means no declared test gate.
    pub acceptance_commands: Vec<String>,
    pub capabilities: crate::CapabilitySelection,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct BudgetUsage {
    pub model_requests: u32,
    pub retries: u32,
    pub repair_attempts: u32,
    pub output_bytes: u64,
    pub external_calls: u32,
    pub mutation_generation: u64,
    pub started_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerificationAttempt {
    pub id: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub output: String,
    pub mutation_generation: u64,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionRequest {
    pub id: String,
    pub task_id: String,
    pub session_id: String,
    pub kind: String,
    pub prompt: String,
    pub action: Option<serde_json::Value>,
    pub action_digest: Option<String>,
    pub revision: u64,
    pub status: String,
    pub answer: Option<String>,
    pub decision_actor: Option<String>,
    pub expires_at_ms: u64,
    pub created_at: String,
    pub updated_at: String,
    pub consumed_at: Option<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restrictions_only_narrow() {
        for mode in [
            PermissionMode::Allow,
            PermissionMode::Ask,
            PermissionMode::Deny,
        ] {
            assert_eq!(mode.intersect(PermissionMode::Deny), PermissionMode::Deny);
            assert_eq!(PermissionMode::Deny.intersect(mode), PermissionMode::Deny);
        }
        assert_eq!(
            PermissionMode::Allow.intersect(PermissionMode::Ask),
            PermissionMode::Ask
        );
    }

    #[test]
    fn controls_have_explicit_compatible_defaults_and_reject_typos() {
        let controls: TaskControls = serde_json::from_str("{}").unwrap();
        assert_eq!(controls, TaskControls::default());
        assert!(
            serde_json::from_str::<TaskControls>(r#"{"budget":{"max_runtime_second":1}}"#).is_err()
        );
    }
}

/// Explicit local owner attestation; not a backend process-exit observation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CleanupOwnerAcknowledgement {
    pub expected_updated_at: String,
    pub confirmed_at: String,
    pub source: String,
}
