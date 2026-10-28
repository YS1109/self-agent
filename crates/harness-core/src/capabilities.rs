//! Application-managed capabilities are explicit snapshots, never ambient grants.
//! Credentials and skill bodies are deliberately absent from public observations.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpProtocolMode {
    #[default]
    Modern2026,
    Legacy2025,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilitySelection {
    pub mcp_servers: Vec<String>,
    pub skills: Vec<String>,
}

/// Owner-configured local process. Arguments must not contain credentials;
/// secrets are supplied only through vault-backed environment references.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerDefinition {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub protocol: McpProtocolMode,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub credential_refs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub schema_digest: String,
    /// Server assertions are display hints, not application authorization.
    #[serde(default)]
    pub annotations: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServerView {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub enabled: bool,
    pub program: String,
    pub args: Vec<String>,
    pub protocol: McpProtocolMode,
    pub credential_slots: Vec<String>,
    /// configured/checking/ready/unavailable; ready is discovery, not a tool call.
    pub status: String,
    pub reason: Option<String>,
    pub catalog_digest: Option<String>,
    pub tool_count: usize,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillView {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub enabled: bool,
    pub source: String,
    pub content_digest: String,
    pub content_bytes: usize,
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ManagedCapabilities {
    #[serde(default)]
    pub mcp_servers: Vec<McpServerView>,
    #[serde(default)]
    pub skills: Vec<SkillView>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityReference {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub digest: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskCapabilities {
    #[serde(default)]
    pub mcp_servers: Vec<CapabilityReference>,
    #[serde(default)]
    pub skills: Vec<CapabilityReference>,
}

/// Auditable call lifecycle only: arguments and outputs are never public here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityCallRecord {
    pub id: String,
    pub task_id: String,
    pub server_id: String,
    pub revision: u64,
    pub tool: String,
    pub digest: String,
    pub status: String,
    pub reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
