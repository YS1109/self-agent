//! QQ is an authenticated remote principal, never a local-owner command proxy.
//! This module contains only bounded parsing and explicit scope decisions; the
//! store must apply them inside the same transaction as the actual command.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

const MAX_INPUT_BYTES: usize = 8192;
const MAX_GRANTED_SESSIONS: usize = 32;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RemotePermissions {
    pub view: bool,
    pub read_results: bool,
    pub run: bool,
    pub cancel: bool,
    pub answer: bool,
    pub orchestrate: bool,
    pub create_session: bool,
    /// Group audiences are broader than the authenticated sender. This is a
    /// distinct owner choice, not implied by permission to read a result in DM.
    pub share_group_results: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteGrant {
    pub session_ids: Vec<String>,
    pub permissions: RemotePermissions,
    pub controls: Value,
    pub expires_at_ms: u64,
}

impl RemoteGrant {
    pub(crate) fn validate(&self, now_ms: u64) -> Result<()> {
        if self.session_ids.is_empty() || self.session_ids.len() > MAX_GRANTED_SESSIONS {
            bail!("QQ 绑定需要明确选择 1 至 32 个会话");
        }
        let mut seen = std::collections::HashSet::new();
        for id in &self.session_ids {
            validate_id(id)?;
            if !seen.insert(id) {
                bail!("QQ 授权会话不能重复");
            }
        }
        if self.expires_at_ms <= now_ms
            || self.expires_at_ms.saturating_sub(now_ms) > 30 * 24 * 60 * 60 * 1000
        {
            bail!("QQ 绑定有效期必须在未来且不超过 30 天");
        }
        if !self.permissions.view {
            bail!("QQ 绑定必须明确授权查看所选会话");
        }
        if !self.controls.is_object() {
            bail!("QQ 任务限制必须是明确配置对象");
        }
        // Range and executor support are additionally checked by the daemon's
        // existing control resolver when the owner confirms and tasks enqueue.
        serde_json::from_value::<harness_core::TaskControls>(self.controls.clone())
            .context("QQ 任务限制字段无效")?;
        Ok(())
    }

    pub(crate) fn permits(&self, command: &RemoteCommand, group: bool) -> bool {
        let p = &self.permissions;
        match command {
            RemoteCommand::Help => true,
            RemoteCommand::Sessions
            | RemoteCommand::Select { .. }
            | RemoteCommand::Status { .. }
            | RemoteCommand::Plan { .. } => p.view,
            RemoteCommand::Result { .. } => {
                p.view && p.read_results && (!group || p.share_group_results)
            }
            RemoteCommand::Questions { .. } => {
                p.view && p.answer && (!group || p.share_group_results)
            }
            RemoteCommand::Create { .. } => p.view && p.create_session,
            RemoteCommand::Run { .. }
            | RemoteCommand::Continue { .. }
            | RemoteCommand::Steer { .. } => p.run,
            RemoteCommand::Cancel { .. } => p.cancel,
            RemoteCommand::Answer { .. } => p.answer,
            RemoteCommand::Orchestrate { .. }
            | RemoteCommand::StartPlan { .. }
            | RemoteCommand::ResumePlan { .. } => p.orchestrate && p.run,
            RemoteCommand::PausePlan { .. } => p.orchestrate,
            RemoteCommand::CancelPlan { .. } => p.orchestrate && p.cancel,
            // Pairing is processed before authority lookup, and only creates a
            // pending claim requiring independent local confirmation.
            RemoteCommand::Pair { .. } => false,
        }
    }
}

/// Default offered by the owner UI, not an implicit grant. The owner must still
/// choose sessions, actions, duration, group visibility and confirm the peer.
#[cfg(test)]
pub(crate) fn conservative_controls() -> Value {
    json!({"permissions":{"read_files":true,"write_files":"ask","run_commands":"deny","managed_tools":"deny"},
        "budget":{"max_runtime_seconds":300,"max_output_bytes":32768},
        "capabilities":{"mcp_servers":[],"skills":[]}})
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum RemoteCommand {
    Help,
    Pair {
        code: String,
    },
    Sessions,
    Select {
        session_id: String,
    },
    Create {
        from_session_id: String,
        title: String,
    },
    Run {
        content: String,
    },
    Status {
        task_id: Option<String>,
    },
    Result {
        task_id: String,
    },
    Cancel {
        task_id: String,
    },
    Steer {
        task_id: String,
        content: String,
    },
    Continue {
        task_id: String,
        content: Option<String>,
    },
    Questions {
        task_id: String,
    },
    Answer {
        request_id: String,
        revision: u32,
        answer: String,
    },
    Orchestrate {
        content: String,
    },
    Plan {
        plan_id: Option<String>,
    },
    StartPlan {
        plan_id: String,
        revision: u32,
    },
    PausePlan {
        plan_id: String,
        revision: u32,
    },
    ResumePlan {
        plan_id: String,
        revision: u32,
    },
    CancelPlan {
        plan_id: String,
    },
}

pub(crate) fn parse(input: &str) -> Result<RemoteCommand> {
    if input.len() > MAX_INPUT_BYTES || input.contains('\0') {
        bail!("QQ 指令最多 8192 UTF-8 字节且不能包含空字符");
    }
    let input = input.trim();
    if input.is_empty() {
        bail!("QQ 消息为空，请输入 /帮助 查看指令");
    }
    if !input.starts_with('/') {
        return Ok(RemoteCommand::Run {
            content: input.into(),
        });
    }
    let (head, rest) = split(input);
    Ok(match head {
        "/帮助" if rest.is_empty() => RemoteCommand::Help,
        "/配对" => {
            if rest.len() != 32 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("配对码格式无效；请使用本机签发的短时一次性配对码");
            }
            RemoteCommand::Pair {
                code: rest.to_ascii_lowercase(),
            }
        }
        "/会话" if rest.is_empty() => RemoteCommand::Sessions,
        "/选择" => RemoteCommand::Select {
            session_id: id_only(rest)?,
        },
        "/新建" => {
            let (id, title) = split(rest);
            validate_id(id)?;
            if title.is_empty() || title.len() > 200 {
                bail!("用法：/新建 已授权会话ID 新会话标题（最多 200 字节）；项目根不能远程指定");
            }
            RemoteCommand::Create {
                from_session_id: id.into(),
                title: title.into(),
            }
        }
        "/运行" => RemoteCommand::Run {
            content: required(rest)?.into(),
        },
        "/状态" => RemoteCommand::Status {
            task_id: if rest.is_empty() {
                None
            } else {
                Some(id_only(rest)?)
            },
        },
        "/结果" => RemoteCommand::Result {
            task_id: id_only(rest)?,
        },
        "/取消" => RemoteCommand::Cancel {
            task_id: id_only(rest)?,
        },
        "/补充" => {
            let (id, content) = split(rest);
            validate_id(id)?;
            RemoteCommand::Steer {
                task_id: id.into(),
                content: required(content)?.into(),
            }
        }
        "/继续" => {
            let (id, content) = split(rest);
            validate_id(id)?;
            RemoteCommand::Continue {
                task_id: id.into(),
                content: (!content.is_empty()).then(|| content.into()),
            }
        }
        "/问题" => RemoteCommand::Questions {
            task_id: id_only(rest)?,
        },
        "/回答" => {
            let (id, tail) = split(rest);
            let (revision, answer) = split(tail);
            validate_id(id)?;
            RemoteCommand::Answer {
                request_id: id.into(),
                revision: revision_number(revision)?,
                answer: required(answer)?.into(),
            }
        }
        "/编排" => RemoteCommand::Orchestrate {
            content: required(rest)?.into(),
        },
        "/计划" => RemoteCommand::Plan {
            plan_id: if rest.is_empty() {
                None
            } else {
                Some(id_only(rest)?)
            },
        },
        "/启动计划" | "/暂停计划" | "/恢复计划" => {
            let (id, revision) = split(rest);
            validate_id(id)?;
            let revision = revision_number(revision)?;
            match head {
                "/启动计划" => RemoteCommand::StartPlan {
                    plan_id: id.into(),
                    revision,
                },
                "/暂停计划" => RemoteCommand::PausePlan {
                    plan_id: id.into(),
                    revision,
                },
                _ => RemoteCommand::ResumePlan {
                    plan_id: id.into(),
                    revision,
                },
            }
        }
        "/取消计划" => RemoteCommand::CancelPlan {
            plan_id: id_only(rest)?,
        },
        _ => bail!("未知或不允许的 QQ 指令；发送 /帮助。动作授权和配置管理只能在本机完成"),
    })
}

fn split(input: &str) -> (&str, &str) {
    input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(a, b)| (a, b.trim()))
}
fn required(input: &str) -> Result<&str> {
    if input.is_empty() {
        bail!("指令缺少必要文本，请输入 /帮助");
    }
    Ok(input)
}
fn id_only(input: &str) -> Result<String> {
    validate_id(input)?;
    Ok(input.into())
}
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.len() != 36
        || !uuid::Uuid::parse_str(id).is_ok_and(|value| value.hyphenated().to_string() == id)
    {
        bail!("任务、会话或请求 ID 必须是完整规范 UUID");
    }
    Ok(())
}
fn revision_number(input: &str) -> Result<u32> {
    if input.is_empty() || !input.bytes().all(|b| b.is_ascii_digit()) {
        bail!("修订号必须是非负整数");
    }
    input.parse().context("修订号超出范围")
}

pub(crate) const HELP: &str = "QQ 指令：\n/会话；/选择 会话ID；/新建 已授权会话ID 标题\n直接发送目标，或 /运行 目标\n/状态 [任务ID]；/结果 任务ID；/取消 任务ID\n/补充 任务ID 内容；/继续 任务ID [要求]\n/问题 任务ID；/回答 请求ID 修订号 内容\n/编排 目标；/计划 [计划ID]\n/启动计划、/暂停计划、/恢复计划 计划ID 修订号；/取消计划 计划ID\n操作依赖本机确认的范围。动作审批、凭据和项目设置只能在本机操作。长任务可能超过 QQ 被动回复窗口，可发送新消息查询。";

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3b";

    #[test]
    fn commands_are_small_allowlist_not_json_or_owner_proxy() {
        assert!(
            parse("修复测试").unwrap()
                == RemoteCommand::Run {
                    content: "修复测试".into()
                }
        );
        assert!(parse("/settings {}").is_err());
        assert!(parse("/授权 request 0 digest").is_err());
        assert!(parse("/回答 owner 0 yes").is_err());
        assert!(parse(&format!("/选择 {ID} arbitrary")).is_err());
        assert!(parse(&format!("/启动计划 {ID} -1")).is_err());
        assert!(parse(&format!("/启动计划 {ID} 4294967296")).is_err());
        assert!(parse("/新建 /outside path").is_err());
        assert!(matches!(
            parse("/计划").unwrap(),
            RemoteCommand::Plan { plan_id: None }
        ));
        assert!(matches!(
            parse(&format!("/计划 {ID}")).unwrap(),
            RemoteCommand::Plan { plan_id: Some(_) }
        ));
        assert!(matches!(
            parse(&format!("/新建 {ID} 中文会话")).unwrap(),
            RemoteCommand::Create { .. }
        ));
        assert!(matches!(
            parse(&format!("/继续 {ID}")).unwrap(),
            RemoteCommand::Continue { content: None, .. }
        ));
        assert!(matches!(
            parse(&format!("/回答 {ID} 2 实际回答")).unwrap(),
            RemoteCommand::Answer { revision: 2, .. }
        ));
        assert!(parse(&"中".repeat(2731)).is_err());
    }

    #[test]
    fn permissions_default_deny_and_group_result_is_separate() {
        let mut grant = RemoteGrant {
            session_ids: vec![ID.into()],
            permissions: RemotePermissions::default(),
            controls: conservative_controls(),
            expires_at_ms: 600_000,
        };
        assert!(grant.validate(0).is_err());
        assert!(!grant.permits(
            &RemoteCommand::Run {
                content: "read".into()
            },
            false
        ));
        grant.permissions.view = true;
        grant.validate(0).unwrap();
        assert!(grant.permits(&RemoteCommand::Sessions, false));
        grant.permissions.read_results = true;
        let result = RemoteCommand::Result { task_id: ID.into() };
        assert!(grant.permits(&result, false));
        assert!(!grant.permits(&result, true));
        grant.permissions.share_group_results = true;
        assert!(grant.permits(&result, true));
        grant.permissions.orchestrate = true;
        assert!(!grant.permits(
            &RemoteCommand::Orchestrate {
                content: "执行计划".into()
            },
            false
        ));
        grant.permissions.run = true;
        assert!(grant.permits(
            &RemoteCommand::Orchestrate {
                content: "执行计划".into()
            },
            false
        ));
        assert!(!grant.permits(
            &RemoteCommand::Pair {
                code: "a".repeat(32)
            },
            false
        ));
        assert!(grant.validate(600_000).is_err());
        grant.expires_at_ms = u64::MAX;
        assert!(grant.validate(0).is_err());
    }

    #[test]
    fn scope_rejects_unknown_fields_duplicate_sessions_and_nonobject_controls() {
        let value = json!({"session_ids":[ID],"permissions":{"view":true,"owner":true},"controls":{},"expires_at_ms":123});
        assert!(serde_json::from_value::<RemoteGrant>(value).is_err());
        let mut grant = RemoteGrant {
            session_ids: vec![ID.into(), ID.into()],
            permissions: RemotePermissions {
                view: true,
                ..Default::default()
            },
            controls: conservative_controls(),
            expires_at_ms: 100_000,
        };
        assert!(grant.validate(0).is_err());
        grant.session_ids.pop();
        grant.controls = Value::Null;
        assert!(grant.validate(0).is_err());
    }
}
