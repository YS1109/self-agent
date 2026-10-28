//! Turn authenticated QQ dispatches into bounded, identity-bound input. QQ
//! transport authority proves the app connection, not local-owner authority.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::persistence::{ConversationKind, Identity, Inbound};
use super::policy::{self, RemoteCommand};

pub(crate) enum Dispatch {
    Ready { bot_user_id: String },
    Input(Inbound),
    ClockSkew { timestamp_ms: u64 },
    Ignored,
}

/// Caller records the checkpoint in the same transaction as the input before
/// acknowledging it to the WS actor. Invalid input never becomes a command.
pub(crate) fn dispatch(
    app_id: &str,
    bot_user_id: Option<&str>,
    payload: &Value,
    now_ms: u64,
) -> Result<Dispatch> {
    identifier(app_id)?;
    if payload["op"] != 0 {
        bail!("QQ 事件不是 Dispatch");
    }
    let data = &payload["d"];
    let kind = payload["t"].as_str().context("QQ 事件类型缺失")?;
    if kind == "READY" {
        let bot_user_id = data["user"]["id"]
            .as_str()
            .context("QQ READY 缺少机器人身份")?;
        identifier(bot_user_id)?;
        return Ok(Dispatch::Ready {
            bot_user_id: bot_user_id.into(),
        });
    }
    let (kind, sender, target) = match kind {
        "C2C_MESSAGE_CREATE" => {
            let sender = data["author"]["user_openid"]
                .as_str()
                .context("QQ 单聊缺少 user_openid")?;
            (ConversationKind::User, sender, sender)
        }
        "GROUP_AT_MESSAGE_CREATE" => (
            ConversationKind::Group,
            data["author"]["member_openid"]
                .as_str()
                .context("QQ 群消息缺少 member_openid")?,
            data["group_openid"]
                .as_str()
                .context("QQ 群消息缺少 group_openid")?,
        ),
        _ => return Ok(Dispatch::Ignored),
    };
    identifier(sender)?;
    identifier(target)?;
    let message_id = data["id"].as_str().context("QQ 消息缺少稳定 ID")?;
    opaque_id(message_id)?;
    let mut event_aliases = Vec::new();
    if let Some(id) = payload.get("id") {
        let id = id.as_str().context("QQ 事件 ID 格式无效")?;
        opaque_id(id)?;
        event_aliases.push(id.to_owned());
    }
    let timestamp = data["timestamp"]
        .as_str()
        .context("QQ 消息缺少平台时间戳")?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp).context("QQ 消息时间戳无效")?;
    let timestamp_ms = u64::try_from(timestamp.timestamp_millis()).context("QQ 消息时间无效")?;
    if timestamp_ms > now_ms.saturating_add(super::persistence::PLATFORM_CLOCK_SKEW_MS) {
        return Ok(Dispatch::ClockSkew { timestamp_ms });
    }
    // Keep the platform timestamp stable across reconnects. Runtime age checks
    // happen after deduplication; time-dependent canonical input would conflict
    // with the very same message replayed after its reply window expires.
    let received_at_ms = timestamp_ms;
    let content = data["content"].as_str().context("QQ 消息只支持文本指令")?;
    let content = normalize_content(
        content,
        matches!(kind, ConversationKind::Group),
        bot_user_id,
    )?;
    let canonical_input = match policy::parse(&content) {
        Ok(RemoteCommand::Pair { code }) => super::persistence::pairing_canonical(app_id, &code)?,
        Ok(_) => json!({"kind":"command","content":content}),
        Err(_) => {
            json!({"kind":"invalid","reason":"unsupported_command","input_digest":format!("{:x}",Sha256::digest(content.as_bytes()))})
        }
    };
    Ok(Dispatch::Input(Inbound {
        identity: Identity {
            app_id: app_id.into(),
            sender_open_id: sender.into(),
            kind,
            target_id: target.into(),
        },
        message_id: message_id.into(),
        event_aliases,
        canonical_input,
        received_at_ms,
    }))
}

fn normalize_content(content: &str, group: bool, bot_user_id: Option<&str>) -> Result<String> {
    if content.len() > 8192 || content.contains('\0') {
        bail!("QQ 输入超过 8192 UTF-8 字节限制");
    }
    let mut text = content.trim();
    if group && text.starts_with("<@") {
        let end = text
            .find('>')
            .filter(|end| *end <= 140)
            .context("QQ 机器人提及格式无效")?;
        let mention = text[2..end].strip_prefix('!').unwrap_or(&text[2..end]);
        if bot_user_id != Some(mention) {
            bail!("QQ 提及与当前已验证机器人身份不一致");
        }
        text = text[end + 1..].trim();
    }
    Ok(text.into())
}

fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        bail!("QQ 身份字段格式无效");
    }
    Ok(())
}
fn opaque_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || !value.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("QQ 事件或消息 ID 格式无效");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message() -> Value {
        json!({"id":"event_1","op":0,"s":2,"t":"C2C_MESSAGE_CREATE", "d":{"id":"ROBOT1.0_message", "timestamp":"2026-10-28T00:00:00Z", "author":{"user_openid":"user_1"}, "content":"/会话"}})
    }
    fn time() -> u64 {
        chrono::DateTime::parse_from_rfc3339("2026-10-28T00:00:01Z")
            .unwrap()
            .timestamp_millis() as u64
    }
    #[test]
    fn identity_is_not_legacy_id_and_platform_time_bounds_replay() {
        let mut value = message();
        let Dispatch::Input(input) = dispatch("app_1", None, &value, time()).unwrap() else {
            panic!("input expected")
        };
        assert_eq!(input.identity.sender_open_id, "user_1");
        assert_eq!(input.identity.target_id, "user_1");
        assert_eq!(input.canonical_input["kind"], "command");
        let Dispatch::Input(expired) = dispatch("app_1", None, &value, time() + 300_000).unwrap()
        else {
            panic!("input expected")
        };
        assert_eq!(expired.canonical_input, input.canonical_input);
        assert_eq!(expired.received_at_ms, input.received_at_ms);
        assert!(time() + 300_000 - expired.received_at_ms >= 300_000);
        value["d"]["author"] = json!({"id":"owner"});
        assert!(dispatch("app_1", None, &value, time()).is_err());
        value = message();
        value["d"]["timestamp"] = json!("2026-10-29T00:00:00Z");
        assert!(matches!(
            dispatch("app_1", None, &value, time()).unwrap(),
            Dispatch::ClockSkew { .. }
        ));
    }
    #[test]
    fn group_requires_member_identity_and_exact_bot_mention() {
        let mut value = message();
        value["t"] = json!("GROUP_AT_MESSAGE_CREATE");
        value["d"]["author"] = json!({"member_openid":"member_1","role":"administrator"});
        value["d"]["group_openid"] = json!("group_1");
        value["d"]["content"] = json!("<@!bot_1> /会话");
        assert!(dispatch("app_1", Some("other_bot"), &value, time()).is_err());
        let Dispatch::Input(input) = dispatch("app_1", Some("bot_1"), &value, time()).unwrap()
        else {
            panic!("input expected")
        };
        assert_eq!(input.identity.sender_open_id, "member_1");
        assert_eq!(input.canonical_input["content"], "/会话");
        value["d"]["group_openid"] = json!("../other");
        assert!(dispatch("app_1", Some("bot_1"), &value, time()).is_err());
    }
    #[test]
    fn pairing_raw_code_never_enters_canonical_input() {
        let mut value = message();
        let code = "ab0123456789abcd0123456789abcdef";
        value["d"]["content"] = json!(format!("/配对 {code}"));
        let Dispatch::Input(input) = dispatch("app_1", None, &value, time()).unwrap() else {
            panic!("input expected")
        };
        assert!(!input.canonical_input.to_string().contains(code));
    }
}
