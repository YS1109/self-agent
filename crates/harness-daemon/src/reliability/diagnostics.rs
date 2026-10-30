//! Allowlisted, aggregate diagnostics. This is deliberately not a sanitizer for
//! a Store snapshot: arbitrary strings, paths, URLs, identifiers, digests,
//! prompts, tool output and logs cannot be supplied to the export API.
use serde::Serialize;

pub(crate) const MAX_EXPORT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Default)]
pub(crate) struct SafeCounts {
    pub sessions: u64,
    pub tasks_pending: u64,
    pub tasks_running: u64,
    pub tasks_completed: u64,
    pub tasks_failed: u64,
    pub tasks_cancelled: u64,
    pub tasks_unknown: u64,
    pub events_retained: u64,
    pub providers_configured: u64,
    pub providers_connection_verified: u64,
    pub provider_tasks_succeeded: u64,
    pub credentials_unknown: u64,
    pub external_calls_unknown: u64,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
// Reserved allowlisted states are exercised offline; the live endpoint only runs after a compatible Store opens.
#[allow(dead_code)]
pub(crate) enum StorageHealth {
    NotChecked,
    Compatible,
    Unsupported,
    RecoveryRequired,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
// Diagnostic export deliberately does not probe or unlock the OS credential store.
#[allow(dead_code)]
pub(crate) enum VaultHealth {
    NotChecked,
    Available,
    Unavailable,
    Locked,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
// Production trust remains disabled; verification-only is reserved for authorized test policy.
#[allow(dead_code)]
pub(crate) enum UpdateHealth {
    DisabledNoTrustedReleaseConfiguration,
    VerificationOnly,
}

/// Trusted integration computes these aggregates without fetching secret values
/// or forwarding arbitrary DB columns. Connection health is not task success.
pub(crate) struct SafeDiagnostics {
    pub database_schema: u32,
    pub storage: StorageHealth,
    pub vault: VaultHealth,
    pub updates: UpdateHealth,
    pub counts: SafeCounts,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticError {
    Encoding,
    TooLarge,
}
impl std::fmt::Display for DiagnosticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("诊断导出失败；未包含原始状态或错误详情")
    }
}
impl std::error::Error for DiagnosticError {}

pub(crate) fn export(input: &SafeDiagnostics) -> Result<Vec<u8>, DiagnosticError> {
    // Spell out every field; do not serialize a caller-owned struct generically.
    // Saturate counts to JSON's interoperable integer range.
    let count = |value: u64| value.min(9_007_199_254_740_991);
    let c = &input.counts;
    let document = serde_json::json!({
        "diagnostic_schema": 1,
        "application": "self-agent",
        "application_version": env!("CARGO_PKG_VERSION"),
        "platform": platform(),
        "database_schema": input.database_schema,
        "connection_check_history": "not_recorded",
        "storage": input.storage,
        "vault": input.vault,
        "updates": input.updates,
        "counts": {
            "sessions": count(c.sessions),
            "tasks_pending": count(c.tasks_pending),
            "tasks_running": count(c.tasks_running),
            "tasks_completed": count(c.tasks_completed),
            "tasks_failed": count(c.tasks_failed),
            "tasks_cancelled": count(c.tasks_cancelled),
            "tasks_unknown": count(c.tasks_unknown),
            "events_retained": count(c.events_retained),
            "providers_configured": count(c.providers_configured),
            "providers_connection_verified": count(c.providers_connection_verified),
            "provider_tasks_succeeded": count(c.provider_tasks_succeeded),
            "credentials_unknown": count(c.credentials_unknown),
            "external_calls_unknown": count(c.external_calls_unknown)
        },
        "excludes": ["credentials", "private_references", "private_digests", "identifiers", "prompts", "tool_output", "paths", "urls", "logs"]
    });
    let bytes = serde_json::to_vec_pretty(&document).map_err(|_| DiagnosticError::Encoding)?;
    if bytes.len() > MAX_EXPORT_BYTES {
        return Err(DiagnosticError::TooLarge);
    }
    Ok(bytes)
}
fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_has_exact_allowlist_and_no_private_canaries() {
        let private = serde_json::json!({
            "token":"fixture-token-canary", "credential_ref":"self-agent.vault.v1:fixture-ref",
            "digest":"fixture-private-digest", "prompt":"fixture-raw-prompt", "tool_output":"fixture-tool-output",
            "path":"/private/fixture-user/project", "url":"https://fixture-secret.invalid/private", "log":"fixture-raw-log"
        });
        let export = export(&SafeDiagnostics {
            database_schema: 1,
            storage: StorageHealth::Compatible,
            vault: VaultHealth::NotChecked,
            updates: UpdateHealth::DisabledNoTrustedReleaseConfiguration,
            counts: SafeCounts {
                providers_configured: 2,
                providers_connection_verified: 0,
                provider_tasks_succeeded: 0,
                ..SafeCounts::default()
            },
        })
        .unwrap();
        let text = String::from_utf8(export).unwrap();
        for value in private.as_object().unwrap().values() {
            assert!(!text.contains(value.as_str().unwrap()));
        }
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 11);
        assert_eq!(value["counts"].as_object().unwrap().len(), 13);
        assert_eq!(value["counts"]["providers_configured"], 2);
        assert_eq!(value["counts"]["providers_connection_verified"], 0);
        assert_eq!(value["counts"]["provider_tasks_succeeded"], 0);
        assert!(text.len() < MAX_EXPORT_BYTES);
    }
    #[test]
    fn all_states_and_largest_counts_remain_bounded_and_static() {
        for storage in [
            StorageHealth::NotChecked,
            StorageHealth::Compatible,
            StorageHealth::Unsupported,
            StorageHealth::RecoveryRequired,
        ] {
            for vault in [
                VaultHealth::NotChecked,
                VaultHealth::Available,
                VaultHealth::Unavailable,
                VaultHealth::Locked,
            ] {
                for updates in [
                    UpdateHealth::DisabledNoTrustedReleaseConfiguration,
                    UpdateHealth::VerificationOnly,
                ] {
                    let bytes = export(&SafeDiagnostics {
                        database_schema: u32::MAX,
                        storage,
                        vault,
                        updates,
                        counts: SafeCounts {
                            sessions: u64::MAX,
                            ..SafeCounts::default()
                        },
                    })
                    .unwrap();
                    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(json["counts"]["sessions"], 9_007_199_254_740_991u64);
                    assert!(bytes.len() < MAX_EXPORT_BYTES);
                }
            }
        }
    }
}
