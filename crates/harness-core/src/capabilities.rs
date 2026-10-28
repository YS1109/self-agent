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
    /// Compatibility input only; nonempty legacy references are rejected.
    #[serde(default)]
    pub credential_refs: BTreeMap<String, String>,
    /// Declared environment variable names, never values or vault references.
    #[serde(default)]
    pub credential_slots: Vec<String>,
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
    #[serde(default)]
    pub credential_states: Vec<McpCredentialSlotState>,
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
    #[serde(default)]
    pub credential_operations: Vec<McpCredentialOperationView>,
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

/// Public enrollment metadata is not evidence that an OS vault is unlocked.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpCredentialSlotState {
    pub slot: String,
    pub configured: bool,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpCredentialOperationView {
    #[serde(flatten)]
    pub operation: crate::CredentialOperationView,
    pub slot: String,
}

/// Shared enrollment policy; transport may have additional internal-only environment rules.
pub fn validate_mcp_credential_slot(slot: &str) -> Result<(), &'static str> {
    let bytes = slot.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_uppercase()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
    {
        return Err("MCP 凭据槽必须为 1–64 位大写 ASCII 变量名，首位为字母");
    }
    // Explicit defense against startup, code-loading, ambient configuration,
    // transport interception and credential-file discovery. This is NOT an OS
    // sandbox or proof that an arbitrary trusted program ignores other options.
    let exact = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "TMPDIR",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "PSMODULEPATH",
        "GLIBC_TUNABLES",
        "LOCPATH",
        "NLSPATH",
        "BASH_XTRACEFD",
        "PROMPT_COMMAND",
        "PWD",
        "OLDPWD",
        "CWD",
        "SHLVL",
        "IFS",
        "ENV",
        "BASH_ENV",
        "BASHOPTS",
        "SHELLOPTS",
        "ZDOTDIR",
        "FPATH",
        "CDPATH",
        "CLASSPATH",
        "JAVA_HOME",
        "JAVA_TOOL_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "JAVA_OPTS",
        "KUBECONFIG",
        "DOCKER_CONFIG",
        "PHPRC",
        "PHP_INI_SCAN_DIR",
        "LUA_PATH",
        "LUA_CPATH",
        "TCL_LIBRARY",
        "TCLLIBPATH",
        "R_PROFILE",
        "R_PROFILE_USER",
        "R_ENVIRON",
        "R_ENVIRON_USER",
        "R_LIBS",
        "R_LIBS_USER",
        "GIO_EXTRA_MODULES",
        "GI_TYPELIB_PATH",
        "YARN_RC_FILENAME",
        "NODE_OPTIONS",
        "NODE_PATH",
        "NODE_EXTRA_CA_CERTS",
        "NODE_TLS_REJECT_UNAUTHORIZED",
        "SSLKEYLOGFILE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "GCONV_PATH",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSH_ASKPASS",
        "SSH_ASKPASS_REQUIRE",
        "GOPATH",
        "GOROOT",
        "GOFLAGS",
        "GOTOOLCHAIN",
        "RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "AWS_CONFIG_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "GH_CONFIG_DIR",
    ];
    let prefixes = [
        "LD_",
        "DYLD_",
        "PYTHON",
        "RUBY",
        "PERL",
        "DOTNET_",
        "COMPLUS_",
        "CORECLR_",
        "COR_",
        "MONO_",
        "GIT_",
        "CARGO_",
        "NPM_CONFIG_",
        "RUSTUP_",
        "OPENSSL_",
        "XDG_",
        "GTK_",
        "QT_",
        "DBUS_",
        "HARNESS_",
        "CODEX_",
        "AWS_ENDPOINT_URL",
        "AWS_CONTAINER_CREDENTIALS_",
        "AWS_EC2_METADATA_",
    ];
    let suffixes = [
        "PROXY",
        "_CA_BUNDLE",
        "_CERT_FILE",
        "_CERT_DIR",
        "_CONFIG_FILE",
        "_TOKEN_FILE",
        "_PATH",
    ];
    if exact.contains(&slot)
        || prefixes.iter().any(|prefix| slot.starts_with(prefix))
        || suffixes.iter().any(|suffix| slot.ends_with(suffix))
    {
        return Err("MCP 凭据槽不能覆盖启动、代码加载、代理、证书或宿主配置变量");
    }
    Ok(())
}
