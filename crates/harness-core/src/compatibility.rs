//! Authenticated-daemon wire compatibility, independent of package versions.
//!
//! Range/lifecycle semantics are extracted from reliability::lifecycle. The
//! new application handshake also requires explicit bounded-history support. This pure
//! module neither authenticates a peer nor admits work. Fetch bounded metadata
//! from the exact authenticated daemon before invoking `negotiate`; never use
//! an unauthenticated health response or silently assume compatibility when the
//! handshake is missing. Revalidate on endpoint/token changes and reconnect.
//!
//! `accepts_new_work` is a reported lifecycle snapshot, not an admission permit:
//! the daemon must still atomically reject durable work after draining begins.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Running,
    Draining,
    Stopped,
}

// This is a new wire-compatibility generation, NOT a docs/protocol-vN number or
// CARGO_PKG_VERSION. Missing metadata is not silently treated as compatible.
pub const PROTOCOL_CURRENT: u16 = 1;
pub const PROTOCOL_MIN: u16 = 1;
pub const MAX_HANDSHAKE_BYTES: usize = 512;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolRange {
    current: u16,
    minimum: u16,
}
impl ProtocolRange {
    pub fn new(current: u16, minimum: u16) -> Result<Self, CompatibilityError> {
        if minimum == 0 || current < minimum {
            return Err(CompatibilityError::Invalid);
        }
        Ok(Self { current, minimum })
    }
    pub fn local() -> Self {
        Self {
            current: PROTOCOL_CURRENT,
            minimum: PROTOCOL_MIN,
        }
    }
}
#[derive(Serialize, Deserialize)]
enum Application {
    #[serde(rename = "self-agent")]
    SelfAgent,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handshake {
    capabilities: Capabilities,
    handshake_schema: u16,
    application: Application,
    protocol_current: u16,
    protocol_min: u16,
    lifecycle: LifecycleState,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    bounded_history: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatibilityError {
    Invalid,
    ClientTooOld,
    ServerTooOld,
}
impl std::fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "后台协议元数据缺失或无效；未继续执行",
            Self::ClientTooOld => "客户端协议版本过旧；请升级客户端",
            Self::ServerTooOld => "后台协议版本过旧；请升级后台",
        })
    }
}
impl std::error::Error for CompatibilityError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
    pub protocol: u16,
    pub bounded_history_schema: u16,
    pub state: LifecycleState,
    pub accepts_new_work: bool,
}
pub fn handshake(state: LifecycleState) -> Result<Vec<u8>, CompatibilityError> {
    serde_json::to_vec(&Handshake {
        capabilities: Capabilities { bounded_history: 1 },
        handshake_schema: 1,
        application: Application::SelfAgent,
        protocol_current: PROTOCOL_CURRENT,
        protocol_min: PROTOCOL_MIN,
        lifecycle: state,
    })
    .map_err(|_| CompatibilityError::Invalid)
}
/// Pure compatibility, NOT authentication or authority. Callers must first bind
/// the response to the authenticated loopback daemon they intend to control.
pub fn negotiate(raw: &[u8], client: ProtocolRange) -> Result<Negotiated, CompatibilityError> {
    if raw.is_empty() || raw.len() > MAX_HANDSHAKE_BYTES {
        return Err(CompatibilityError::Invalid);
    }
    let peer: Handshake = serde_json::from_slice(raw).map_err(|_| CompatibilityError::Invalid)?;
    if peer.handshake_schema != 1 || peer.capabilities.bounded_history != 1 {
        return Err(CompatibilityError::Invalid);
    }
    let server = ProtocolRange::new(peer.protocol_current, peer.protocol_min)?;
    if client.current < server.minimum {
        return Err(CompatibilityError::ClientTooOld);
    }
    if server.current < client.minimum {
        return Err(CompatibilityError::ServerTooOld);
    }
    Ok(Negotiated {
        protocol: client.current.min(server.current),
        bounded_history_schema: peer.capabilities.bounded_history,
        state: peer.lifecycle,
        accepts_new_work: peer.lifecycle == LifecycleState::Running,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn peer(current: u16, minimum: u16, state: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"capabilities":{"bounded_history":1},"handshake_schema":1,"application":"self-agent","protocol_current":current,"protocol_min":minimum,"lifecycle":state})).unwrap()
    }
    #[test]
    fn same_wire_generation_ignores_marketing_version_and_reports_lifecycle() {
        for state in [
            LifecycleState::Running,
            LifecycleState::Draining,
            LifecycleState::Stopped,
        ] {
            let raw = handshake(state).unwrap();
            assert!(raw.len() <= MAX_HANDSHAKE_BYTES);
            let actual = negotiate(&raw, ProtocolRange::local()).unwrap();
            assert_eq!(actual.protocol, 1);
            assert_eq!(actual.bounded_history_schema, 1);
            assert_eq!(actual.state, state);
            assert_eq!(actual.accepts_new_work, state == LifecycleState::Running);
            let wire: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(wire["application"], "self-agent");
            assert!(wire.get("package_version").is_none());
        }
    }
    #[test]
    fn overlapping_ranges_choose_highest_common_generation() {
        for server_min in 1..=4 {
            for server_current in server_min..=4 {
                for client_min in 1..=4 {
                    for client_current in client_min..=4 {
                        let result = negotiate(
                            &peer(server_current, server_min, "running"),
                            ProtocolRange::new(client_current, client_min).unwrap(),
                        );
                        if client_current < server_min {
                            assert_eq!(result, Err(CompatibilityError::ClientTooOld));
                        } else if server_current < client_min {
                            assert_eq!(result, Err(CompatibilityError::ServerTooOld));
                        } else {
                            assert_eq!(
                                result.unwrap().protocol,
                                server_current.min(client_current)
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn missing_unknown_duplicate_and_invalid_types_fail_closed_without_echo() {
        let base: Value =
            serde_json::from_slice(&handshake(LifecycleState::Running).unwrap()).unwrap();
        for field in [
            "capabilities",
            "handshake_schema",
            "application",
            "protocol_current",
            "protocol_min",
            "lifecycle",
        ] {
            let mut absent = base.clone();
            absent.as_object_mut().unwrap().remove(field);
            assert_eq!(
                negotiate(
                    &serde_json::to_vec(&absent).unwrap(),
                    ProtocolRange::local()
                ),
                Err(CompatibilityError::Invalid)
            );
            for value in [Value::Null, json!(true), json!({"private":"secret-canary"})] {
                let mut bad = base.clone();
                bad[field] = value;
                let error = negotiate(&serde_json::to_vec(&bad).unwrap(), ProtocolRange::local())
                    .unwrap_err();
                assert_eq!(error, CompatibilityError::Invalid);
                assert!(!error.to_string().contains("secret-canary"));
            }
        }
        for (field, value) in [
            ("capabilities", json!({})),
            ("capabilities", json!({"bounded_history":0})),
            ("capabilities", json!({"bounded_history":2})),
            ("capabilities", json!({"bounded_history":true})),
            (
                "capabilities",
                json!({"bounded_history":1,"all_history":true}),
            ),
            ("handshake_schema", json!(2)),
            ("application", json!("other")),
            ("lifecycle", json!("unknown")),
            ("protocol_current", json!(65536)),
            ("protocol_current", json!(-1)),
            ("protocol_current", json!(1.5)),
            ("protocol_min", json!(0)),
            ("unexpected_secret", json!("secret-canary")),
        ] {
            let mut bad = base.clone();
            bad[field] = value;
            assert_eq!(
                negotiate(&serde_json::to_vec(&bad).unwrap(), ProtocolRange::local()),
                Err(CompatibilityError::Invalid)
            );
        }
        let duplicate = String::from_utf8(handshake(LifecycleState::Running).unwrap())
            .unwrap()
            .replacen('{', "{\"lifecycle\":\"running\",", 1);
        assert_eq!(
            negotiate(duplicate.as_bytes(), ProtocolRange::local()),
            Err(CompatibilityError::Invalid)
        );
    }
    #[test]
    fn strict_byte_boundary_utf8_trailing_data_and_ranges() {
        let mut raw = handshake(LifecycleState::Running).unwrap();
        raw.resize(MAX_HANDSHAKE_BYTES, b' ');
        assert!(negotiate(&raw, ProtocolRange::local()).is_ok());
        raw.push(b' ');
        assert_eq!(
            negotiate(&raw, ProtocolRange::local()),
            Err(CompatibilityError::Invalid)
        );
        for raw in [
            vec![],
            vec![0xff],
            b"[]".to_vec(),
            b"null".to_vec(),
            b"{}{}".to_vec(),
        ] {
            assert_eq!(
                negotiate(&raw, ProtocolRange::local()),
                Err(CompatibilityError::Invalid)
            );
        }
        assert_eq!(ProtocolRange::new(0, 0), Err(CompatibilityError::Invalid));
        assert_eq!(ProtocolRange::new(1, 2), Err(CompatibilityError::Invalid));
        assert!(ProtocolRange::new(u16::MAX, 1).is_ok());
    }
}
