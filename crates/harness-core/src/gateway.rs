//! Versioned, deliberately narrow gateway wire protocol. This is not the local
//! owner command API. The authenticated daemon still checks every principal,
//! binding revision and resource in its authoritative transaction.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GATEWAY_PROTOCOL: u32 = 1;
pub const GATEWAY_MAX_FRAME_BYTES: usize = 256 * 1024;
pub const GATEWAY_MAX_INPUT_BYTES: usize = 8192;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayCommand {
    Run {
        content: String,
    },
    Cancel {
        task_id: String,
    },
    Answer {
        request_id: String,
        expected_revision: u32,
        answer: String,
    },
    Steer {
        task_id: String,
        content: String,
    },
    Continue {
        task_id: String,
        content: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayRpc {
    PairClaim {
        code: String,
        label: String,
    },
    PairStatus,
    Sessions,
    Session {
        session_id: String,
    },
    Task {
        task_id: String,
    },
    Events {
        session_id: String,
        after: u64,
    },
    Changes {
        task_id: String,
    },
    Artifacts {
        task_id: String,
    },
    Artifact {
        resource_id: String,
    },
    Receipt {
        command_id: String,
    },
    Command {
        command_id: String,
        session_id: String,
        command: GatewayCommand,
    },
}

impl GatewayRpc {
    pub fn is_mutation(&self) -> bool {
        matches!(self, Self::Command { .. } | Self::PairClaim { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayRequest {
    pub request_id: String,
    pub principal_id: String,
    pub binding_revision: u64,
    pub rpc: GatewayRpc,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayCode {
    Ok,
    DeviceOffline,
    OutcomeUnknown,
    Unauthorized,
    Forbidden,
    InvalidRequest,
    Conflict,
    Busy,
    NotFound,
    CursorExpired,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayResult {
    pub code: GatewayCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
impl GatewayResult {
    pub fn ok(data: Value) -> Self {
        Self {
            code: GatewayCode::Ok,
            data: Some(data),
        }
    }
    pub fn error(code: GatewayCode) -> Self {
        Self { code, data: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayDaemonFrame {
    Ready {
        protocol: u32,
        device_id: String,
    },
    Reply {
        request_id: String,
        result: GatewayResult,
    },
    Revoked {
        principal_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayServerFrame {
    Request { request: GatewayRequest },
}

/// Serde's internally tagged unit variants can ignore extra fields even with
/// deny_unknown_fields. A canonical round-trip rejects those extra authority
/// fields as well as omitted nullable fields rather than silently discarding
/// them. Browser and daemon boundaries must use these parsers.
pub fn parse_gateway_rpc(value: Value) -> Result<GatewayRpc, &'static str> {
    let parsed: GatewayRpc =
        serde_json::from_value(value.clone()).map_err(|_| "网关请求字段无效")?;
    if serde_json::to_value(&parsed).map_err(|_| "网关请求字段无效")? != value {
        return Err("网关请求包含未支持或不完整字段");
    }
    Ok(parsed)
}

pub fn parse_gateway_server_frame(raw: &str) -> Result<GatewayServerFrame, &'static str> {
    if raw.len() > GATEWAY_MAX_FRAME_BYTES {
        return Err("网关请求超过帧限制");
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| "网关请求 JSON 无效")?;
    let parsed: GatewayServerFrame = serde_json::from_str(raw).map_err(|_| "网关请求字段无效")?;
    if serde_json::to_value(&parsed).map_err(|_| "网关请求字段无效")? != value {
        return Err("网关请求包含未支持或不完整字段");
    }
    Ok(parsed)
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayPermissions {
    pub view: bool,
    pub read_results: bool,
    pub read_artifacts: bool,
    pub run: bool,
    pub cancel: bool,
    pub answer: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayGrant {
    pub session_ids: Vec<String>,
    pub permissions: GatewayPermissions,
    /// Must be an explicit complete restrictive configuration. The daemon
    /// validates it and intersects it with local/role/task controls; no remote
    /// ability to approve an action or expand its own ceiling exists.
    pub controls: Value,
    pub expires_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn unit_rpc_cannot_discard_owner_or_principal_fields() {
        for value in [
            json!({"type":"sessions","owner":true}),
            json!({"type":"pair_status","principal_id":"other"}),
        ] {
            assert!(parse_gateway_rpc(value).is_err());
        }
        assert!(matches!(
            parse_gateway_rpc(json!({"type":"sessions"})),
            Ok(GatewayRpc::Sessions)
        ));
    }
    #[test]
    fn frames_require_complete_narrow_commands_and_reject_authority_injection() {
        let id = "11111111-1111-4111-8111-111111111111";
        let mut value = json!({"type":"request","request":{"request_id":id,"principal_id":id,"binding_revision":1,"rpc":{"type":"sessions","admin":true}}});
        assert!(parse_gateway_server_frame(&value.to_string()).is_err());
        value["request"]["rpc"] = json!({"type":"command","command_id":id,"session_id":id,"command":{"type":"continue","task_id":id,"content":null}});
        assert!(parse_gateway_server_frame(&value.to_string()).is_ok());
        value["request"]["rpc"]["command"]["type"] = json!("approve");
        assert!(parse_gateway_server_frame(&value.to_string()).is_err());
    }
}
