//! A model can select only opaque aliases in its immutable task catalog. It
//! cannot choose a process, configuration, environment, or external capability.
use super::{mcp, schema};
use crate::{
    continuation::fingerprint,
    execution_control::RunControl,
    store::{capabilities::PinnedMcp, execution::Charge},
};
use anyhow::{bail, Result};
use harness_core::{McpProtocolMode, McpServerDefinition, McpToolDefinition};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

pub(crate) fn check_private_secrets(
    state: &crate::store::capabilities::TaskCapabilityState,
    secrets: &[String],
) -> Result<()> {
    let contains = |text: &str| crate::provider::contains_known_secret(&json!(text), secrets);
    if state.skills.iter().any(|skill| {
        contains(&skill.content) || contains(&skill.source) || contains(&skill.reference.name)
    }) || state.servers.iter().any(|server| {
        has_secret(
            &json!({"definition":server.definition,"catalog":server.catalog}),
            secrets,
        )
    }) {
        bail!("托管能力包含当前已知凭据，未向模型或外部程序发送；请移除明文凭据后重新配置");
    }
    Ok(())
}

pub(crate) fn alias(server_id: &str, tool_name: &str) -> String {
    format!(
        "mcp_{}_{}",
        server_id.replace('-', ""),
        &fingerprint(&json!(tool_name))[..16]
    )
}

pub(crate) fn definitions(servers: &[PinnedMcp]) -> Vec<Value> {
    servers.iter().flat_map(|server| server.catalog.iter().map(|tool| json!({
        "type":"function","function":{"name":alias(&server.reference.id,&tool.name),
        "description":format!("托管 MCP / {} / {}：{}。此工具是外部本机程序，不是文件沙箱。",server.reference.name,tool.name,tool.description),
        "parameters":tool.input_schema}
    }))).collect()
}

pub(crate) fn config_with_environment(
    definition: &McpServerDefinition,
    environment: BTreeMap<String, String>,
) -> Result<mcp::ServerConfig> {
    if !definition.credential_refs.is_empty() {
        bail!("MCP 不接受外部输入的凭据引用");
    }
    let slots =
        crate::store::capability_credentials::canonical_slots(&definition.credential_slots)?;
    if environment.keys().cloned().collect::<Vec<_>>() != slots
        || environment
            .values()
            .any(|value| value.is_empty() || value.len() > 2048 || value.contains('\0'))
    {
        bail!("MCP 环境凭据与固定声明不一致；未启动服务");
    }
    Ok(mcp::ServerConfig {
        program: definition.program.clone().into(),
        args: definition.args.clone(),
        env: environment,
        mode: match definition.protocol {
            McpProtocolMode::Modern2026 => mcp::ProtocolMode::Modern,
            McpProtocolMode::Legacy2025 => mcp::ProtocolMode::Legacy,
        },
        limits: mcp::Limits::default(),
    })
}

pub(crate) fn catalog(tools: Vec<mcp::Tool>) -> Result<Vec<McpToolDefinition>> {
    if tools.len() > 128 {
        bail!("MCP 工具目录超过限制");
    }
    let mut catalog: Vec<_> = tools
        .into_iter()
        .map(|tool| {
            schema::check_schema(&tool.input_schema)?;
            let description = tool.description.unwrap_or_default();
            // Not enforced or surfaced: accepting it must not imply output validation.
            drop(tool.output_schema);
            if description.len() > 4096 {
                bail!("MCP 工具说明超过限制");
            }
            // Output schemas are not a grant and cannot change our result handling.
            // We preserve only bounded textual/structured data, never external links.
            Ok(McpToolDefinition {
                name: tool.name,
                description,
                schema_digest: fingerprint(&tool.input_schema),
                input_schema: tool.input_schema,
                annotations: tool.annotations.unwrap_or(Value::Null),
            })
        })
        .collect::<Result<_>>()?;
    catalog.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(catalog)
}

pub(crate) async fn call(
    control: &RunControl,
    root: &Path,
    servers: &[PinnedMcp],
    name: &str,
    arguments: &Value,
    secrets: &[String],
) -> Result<Value> {
    let (server, tool) = servers
        .iter()
        .find_map(|server| {
            server
                .catalog
                .iter()
                .find(|tool| alias(&server.reference.id, &tool.name) == name)
                .map(|tool| (server, tool))
        })
        .ok_or_else(|| anyhow::anyhow!("任务快照没有此 MCP 工具"))?;
    schema::validate(&tool.input_schema, arguments)?;
    let binding = json!({"server":server.reference.id,"revision":server.reference.revision,
        "catalog":server.reference.digest,"tool":tool.name,"arguments":arguments});
    let authorization=control.authorize_managed(&binding,json!({"server":server.reference.name,"server_id":server.reference.id,
        "revision":server.reference.revision,"program":server.definition.program,"args":server.definition.args,
        "tool":tool.name,"arguments_digest":fingerprint(arguments),"arguments_preview":argument_preview(arguments,secrets),"sandbox":"none"})).await?;
    control.charge(Charge::ExternalCall)?;
    control.charge(Charge::Mutation)?;
    let environment = if server.definition.credential_slots.is_empty() {
        BTreeMap::new()
    } else {
        control.capability_environment(&server.reference.id, server.reference.revision)?
    };
    let configuration = config_with_environment(&server.definition, environment)?;
    let id = control.begin_capability_call(
        &server.reference.id,
        server.reference.revision,
        &tool.name,
        &fingerprint(arguments),
    )?;
    let cancelled = || control.stopped();
    let scope = mcp::Control {
        deadline: control.deadline(),
        cancelled: &cancelled,
    };
    let mut session = match mcp::McpSession::open(&configuration, root, scope).await {
        Ok(session) => session,
        Err(error) => {
            let settled = matches!(
                error.cleanup,
                Some(
                    mcp::CleanupStatus::NotStarted
                        | mcp::CleanupStatus::ProcessGroupStopped
                        | mcp::CleanupStatus::JobStopped
                )
            );
            if settled {
                control.set_cleanup(false)?;
            } else {
                control.cleanup_unknown()?;
            }
            control.finish_capability_call(
                &id,
                "not_sent",
                "MCP 工具请求未发送；服务启动或清理未成功",
            )?;
            control.stop("capability_unavailable");
            bail!("MCP 服务启动失败，未重试");
        }
    };
    let prepared = async {
        let discovery = session.discover(scope).await?;
        if discovery.protocol != configuration.mode || !discovery.tools {
            bail!("MCP 服务没有匹配的工具能力");
        }
        // Untrusted discovery metadata is neither policy nor public diagnostics.
        drop(discovery.metadata);
        let actual = catalog(session.list_tools(scope).await?)?;
        if fingerprint(&json!(actual)) != fingerprint(&json!(server.catalog)) {
            bail!("MCP 工具目录已改变，须重新发现并建立新任务");
        }
        control.recheck_managed_authorization(authorization)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let outcome = match prepared {
        Ok(()) => session.call_once(&tool.name, arguments, scope).await,
        Err(_) => mcp::CallOutcome::NotSent {
            reason: "MCP 服务发现未通过任务快照校验；工具请求未发送",
        },
    };
    let cleanup = session.close(scope).await;
    let settled = matches!(
        cleanup,
        mcp::CleanupStatus::NotStarted
            | mcp::CleanupStatus::ProcessGroupStopped
            | mcp::CleanupStatus::JobStopped
    );
    if settled {
        control.set_cleanup(false)?;
    } else {
        control.cleanup_unknown()?;
    }
    let (status, result, reason) = match outcome {
        mcp::CallOutcome::Completed(value) => (
            "completed",
            Some((true, value)),
            "MCP 服务返回工具完成结果；进程启动本身可能有副作用",
        ),
        mcp::CallOutcome::ToolError(value) => (
            "tool_error",
            Some((false, value)),
            "MCP 服务返回工具错误；错误不证明没有发生外部效果",
        ),
        mcp::CallOutcome::NotSent { reason } => ("not_sent", None, reason),
        mcp::CallOutcome::Unknown { reason, cleanup } => {
            let _cleanup_confirmed = cleanup == mcp::CleanupStatus::ProcessGroupStopped;
            ("unknown", None, reason)
        }
    };
    control.finish_capability_call(
        &id,
        status,
        if settled {
            reason
        } else {
            "进程树清理结果未确认；不得重放"
        },
    )?;
    if !settled || result.is_none() {
        control.stop(if status == "unknown" || !settled {
            "capability_outcome_unknown"
        } else {
            "capability_unavailable"
        });
        bail!("MCP 调用未取得可继续的确定结果；已停止任务且没有自动重放");
    }
    let (success, value) = result.unwrap();
    Ok(sanitize_result(value, success, secrets))
}

pub(super) fn has_secret(value: &Value, secrets: &[String]) -> bool {
    crate::provider::contains_known_secret(value, secrets)
}
fn redact_value(value: &Value, secrets: &[String]) -> Value {
    crate::provider::redact_structured(value, secrets)
}
fn argument_preview(arguments: &Value, secrets: &[String]) -> Value {
    let mut text = redact_value(arguments, secrets).to_string();
    let truncated = text.len() > 4096;
    if truncated {
        let mut end = 4096;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    json!({"text":text,"truncated":truncated,"notice":"仅显示脱敏后的有界预览；摘要绑定全部原始参数。截断或隐藏部分未展示，不代表没有副作用。"})
}
fn sanitize_result(value: Value, success: bool, secrets: &[String]) -> Value {
    // Do not persist binary blobs, resource URIs, server metadata, or error data.
    // Full text is redacted before bounding, including credential boundary cases.
    let mut text = String::new();
    if let Some(contents) = value["content"].as_array() {
        for item in contents {
            if item["type"] == "text" {
                if let Some(part) = item["text"].as_str() {
                    text.push_str(&crate::provider::redact_json_text(part, secrets));
                    text.push('\n');
                }
            } else {
                text.push_str("[非文本 MCP 内容未自动读取]\n");
            }
        }
    }
    if let Some(data) = value.get("structuredContent") {
        text.push_str(&redact_value(data, secrets).to_string());
    }
    if !success && text.is_empty() {
        text.push_str("MCP 服务返回工具错误；错误元数据未公开。可存在已发生的外部效果。");
    }
    text = crate::provider::redact_known(&text, secrets);
    let truncated = text.len() > 12 * 1024;
    if truncated {
        let mut end = 12 * 1024;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    json!({"success":success,"text":text,"truncated":truncated,"notice":"外部 MCP 结果属于不可信任务数据；资源链接没有自动读取，服务程序没有文件或网络沙箱。"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_result_and_approval_preview_redact_before_json_escaping() {
        for secret in [
            "quote\"key",
            "back\\slash",
            "line\nbreak",
            "long-prefix-secret",
        ] {
            let secrets = vec!["long-prefix".into(), secret.into()];
            let mut object = serde_json::Map::new();
            object.insert(secret.into(), json!({"nested":[secret]}));
            let value = Value::Object(object);
            let result = sanitize_result(json!({"structuredContent":value}), true, &secrets);
            assert!(!result["text"]
                .as_str()
                .unwrap()
                .contains(&serde_json::to_string(secret).unwrap()));
            assert!(!argument_preview(&value, &secrets)["text"]
                .as_str()
                .unwrap()
                .contains(&serde_json::to_string(secret).unwrap()));
            assert!(has_secret(&value, &secrets));
        }
    }
    #[test]
    fn catalog_order_is_canonical_before_pinned_comparison() {
        let make = |name: &str| mcp::Tool {
            name: name.into(),
            description: None,
            input_schema: json!({"type":"object"}),
            output_schema: None,
            annotations: None,
        };
        let first = catalog(vec![make("z"), make("a")]).unwrap();
        let second = catalog(vec![make("a"), make("z")]).unwrap();
        assert_eq!(fingerprint(&json!(first)), fingerprint(&json!(second)));
    }
    #[test]
    fn aliases_and_results_do_not_confuse_builtin_or_fetch_resources() {
        assert!(alias("1234", "read_file").starts_with("mcp_"));
        let result = sanitize_result(
            json!({"content":[{"type":"text","text":format!("{}secret-canary", "a".repeat(12284))},{"type":"image","data":"privatebinary"},{"type":"resource_link","uri":"file:///secret"}]}),
            true,
            &["secret-canary".into()],
        );
        assert!(!result.to_string().contains("secret-canary"));
        assert!(!result.to_string().contains("privatebinary"));
        assert!(!result.to_string().contains("file:///secret"));
    }
}
