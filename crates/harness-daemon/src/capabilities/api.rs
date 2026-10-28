//! Local-owner registry commands. No remote channel or model tool can enroll
//! capabilities; accepting an owner command does not grant task permissions.
use super::{mcp, runtime};
use crate::{store::capabilities::PreparedSkillImport, ApiError, ApiResult, AppState};
use anyhow::{bail, Result};
use axum::{extract::State, Json};
use serde_json::Value;
use std::{path::Path, sync::Arc};

fn invalid(_: impl std::fmt::Display) -> ApiError {
    ApiError(axum::http::StatusCode::BAD_REQUEST, "invalid".into())
}

pub(crate) async fn registry(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<harness_core::ManagedCapabilities>> {
    state
        .store
        .lock()
        .unwrap()
        .capabilities_registry()
        .map(Json)
        .map_err(invalid)
}

pub(crate) async fn command(
    State(state): State<Arc<AppState>>,
    Json(envelope): Json<Value>,
) -> ApiResult<Json<Value>> {
    if let Some(receipt) = state
        .store
        .lock()
        .unwrap()
        .existing_capability_receipt(&envelope)
        .map_err(invalid)?
    {
        return Ok(Json(receipt));
    }
    let settings = state.settings.read().unwrap().clone();
    let settings = crate::configuration::resolve(&settings).unwrap_or(settings);
    let secrets = crate::continuation::known_secrets(&settings);
    let receipt = if envelope["command"]["type"] == "import_skill" {
        let source = envelope["command"]["source_path"]
            .as_str()
            .ok_or_else(|| invalid("path"))?
            .to_owned();
        let prepared = tokio::task::spawn_blocking(move || read_skill(&source))
            .await
            .map_err(invalid)?
            .map_err(invalid)?;
        state
            .store
            .lock()
            .unwrap()
            .capability_import_command(&envelope, &prepared, &secrets)
            .map_err(invalid)?
    } else {
        state
            .store
            .lock()
            .unwrap()
            .capability_command(&envelope, &secrets)
            .map_err(invalid)?
    };
    if receipt["status"] == "checking" {
        let id = receipt["capability_id"]
            .as_str()
            .ok_or_else(|| invalid("id"))?
            .to_owned();
        let revision = receipt["revision"]
            .as_u64()
            .ok_or_else(|| invalid("revision"))?;
        let state = state.clone();
        tokio::spawn(async move {
            probe(state, id, revision, secrets).await;
        });
    }
    Ok(Json(receipt))
}

fn read_skill(source: &str) -> Result<PreparedSkillImport> {
    let path = Path::new(source);
    if source.len() > 4096
        || !path.is_absolute()
        || path.file_name().and_then(|s| s.to_str()) != Some("SKILL.md")
    {
        bail!("技能导入路径必须指向绝对 SKILL.md");
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("技能路径缺少父目录"))?;
    let project = crate::project_files::ScopedProject::open(parent)?;
    let content = project.read("SKILL.md")?;
    if content.is_empty() || content.len() > 64 * 1024 {
        bail!("技能正文超过限制");
    }
    Ok(PreparedSkillImport {
        content,
        source: source.into(),
    })
}

async fn probe(state: Arc<AppState>, id: String, revision: u64, mut secrets: Vec<String>) {
    let work = match state
        .store
        .lock()
        .unwrap()
        .claim_pending_probe(&id, revision)
    {
        Ok(Some(work)) => work,
        _ => return,
    };
    if work.id != id || work.revision != revision {
        return;
    }
    let permit = state.capability_probe_gate.clone().try_acquire_owned();
    let result = if let Ok(_permit) = permit {
        let cancelled = || {
            state
                .store
                .lock()
                .unwrap()
                .capabilities_registry()
                .map_or(true, |registry| {
                    !registry.mcp_servers.iter().any(|entry| {
                        entry.id == id && entry.revision == revision && entry.status == "checking"
                    })
                })
        };
        let scope = mcp::Control {
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(25),
            cancelled: &cancelled,
        };
        let private =
            super::credentials::probe(&state, &id, revision, scope.deadline, &cancelled).await;
        let config = match private {
            Ok(private) => {
                secrets.extend(private.secrets());
                if runtime::has_secret(&serde_json::json!(work.definition), &secrets) {
                    Err(anyhow::anyhow!("MCP 配置包含已知凭据"))
                } else {
                    private.check(&state).and_then(|()| {
                        runtime::config_with_environment(&work.definition, private.environment())
                    })
                }
            }
            Err(error) => Err(error),
        };
        match config {
            Err(_) => Err("unavailable"),
            Ok(config) => match mcp::McpSession::open(&config, &state.capability_probe_root, scope)
                .await
            {
                Err(_) => Err("unavailable"),
                Ok(mut session) => {
                    let result = async {
                        let discovery = session.discover(scope).await.map_err(|_| "unavailable")?;
                        if discovery.protocol != config.mode || !discovery.tools {
                            return Err("unavailable");
                        }
                        drop(discovery.metadata);
                        runtime::catalog(
                            session.list_tools(scope).await.map_err(|_| "unavailable")?,
                        )
                        .map_err(|_| "unavailable")
                    }
                    .await;
                    let cleanup = session.close(scope).await;
                    if cleanup == mcp::CleanupStatus::Unknown {
                        Err("unavailable")
                    } else {
                        result
                    }
                }
            },
        }
    } else {
        Err("unavailable")
    };
    // Revision fencing makes an edited/deleted entry a no-op, not a resurrection.
    let _ = state
        .store
        .lock()
        .unwrap()
        .finish_probe(&id, revision, result, &secrets);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skill_import_is_explicit_bounded_and_nofollow() {
        let root = std::env::temp_dir().join(format!("skill-import-{}", harness_core::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: fixture\ndescription: fixture skill\n---\nUse a bounded fixture.",
        )
        .unwrap();
        let path = root.join("SKILL.md");
        assert!(read_skill(path.to_str().unwrap())
            .unwrap()
            .content
            .contains("bounded fixture"));
        assert!(read_skill("SKILL.md").is_err());
        std::fs::write(&path, vec![b'x'; 65537]).unwrap();
        assert!(read_skill(path.to_str().unwrap()).is_err());
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(root.join("other"), &path).unwrap();
            std::fs::write(root.join("other"), "secret source").unwrap();
            assert!(read_skill(path.to_str().unwrap()).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
