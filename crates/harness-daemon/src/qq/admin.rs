//! Local-owner QQ metadata commands. Not reachable from QQ input dispatch. Raw
//! pairing codes occur only in the first response, never in replay receipts.
use super::{
    configuration::{self, Config, ConfigPatch},
    persistence::{self, Identity, ReplyPolicy},
    policy::{self, RemoteGrant},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    id: String,
    command: Command,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Configure {
        config_id: Option<String>,
        expected_revision: Option<u64>,
        app_id: String,
        enabled: bool,
        reply_policy: ReplyPolicy,
    },
    IssuePairing {
        ttl_seconds: u32,
    },
    ConfirmBinding {
        principal_id: String,
        expected_revision: u64,
        identity: Identity,
        grant: RemoteGrant,
    },
    RevokeBinding {
        principal_id: String,
        expected_revision: u64,
    },
}
pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS qq_admin_commands(id TEXT PRIMARY KEY,digest TEXT NOT NULL,receipt TEXT NOT NULL);")?;
    Ok(())
}
fn digest(command: &Command) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(command)?)
    ))
}
fn existing(db: &Connection, id: &str, digest: &str) -> Result<Option<Value>> {
    let found: Option<(String, String)> = db
        .query_row(
            "SELECT digest,receipt FROM qq_admin_commands WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    found
        .map(|(original, receipt)| {
            if original != digest {
                bail!("QQ 管理操作标识已对应另一项操作")
            }
            Ok(serde_json::from_str(&receipt)?)
        })
        .transpose()
}
fn save(tx: &Transaction<'_>, id: &str, digest: &str, receipt: &Value) -> Result<()> {
    let count: u64 = tx.query_row("SELECT COUNT(*) FROM qq_admin_commands", [], |r| r.get(0))?;
    if count >= 10000 {
        bail!("QQ 管理操作记录达到安全上限，未遗忘旧操作或重复执行")
    }
    tx.execute(
        "INSERT INTO qq_admin_commands VALUES(?1,?2,?3)",
        params![id, digest, serde_json::to_string(receipt)?],
    )?;
    Ok(())
}
pub(crate) fn receipt(db: &Connection, id: &str) -> Result<Option<Value>> {
    policy::validate_id(id)?;
    let body: Option<String> = db
        .query_row(
            "SELECT receipt FROM qq_admin_commands WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    body.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}

/// `credential_ready` is internal metadata/binding readiness, not a claim that
/// the platform accepted it. OS reads happen outside the transaction at startup.
/// A trusted process fixture may prove readiness without creating a real key.
pub(crate) fn apply(
    tx: &Transaction<'_>,
    envelope: &Value,
    now: u64,
    credential_ready: impl FnOnce(&Config) -> Result<bool>,
    validate_grant: impl FnOnce(&RemoteGrant) -> Result<()>,
) -> Result<Value> {
    let envelope: Envelope = serde_json::from_value(envelope.clone())
        .map_err(|_| anyhow::anyhow!("QQ 管理操作字段无效"))?;
    policy::validate_id(&envelope.id)?;
    let hash = digest(&envelope.command)?;
    if let Some(receipt) = existing(tx, &envelope.id, &hash)? {
        return Ok(receipt);
    }
    let mut receipt = json!({"command_id":envelope.id,"status":"completed"});
    let mut pairing_code = None;
    match envelope.command {
        Command::Configure {
            config_id,
            expected_revision,
            app_id,
            enabled,
            reply_policy,
        } => {
            let config = match (config_id, expected_revision) {
                (None, None) => {
                    if enabled {
                        bail!("首次 QQ 配置必须停用创建")
                    };
                    configuration::create(tx, &app_id)?
                }
                (Some(id), Some(revision)) => configuration::load(tx, &id, revision)?,
                _ => bail!("QQ 配置标识与预期修订必须成对提供"),
            };
            let config = configuration::update(
                tx,
                &config.id,
                config.revision,
                ConfigPatch {
                    app_id,
                    enabled,
                    reply_policy,
                },
                now,
                credential_ready,
            )?;
            receipt["config_id"] = json!(config.id);
            receipt["revision"] = json!(config.revision);
        }
        Command::IssuePairing { ttl_seconds } => {
            let config = configuration::current(tx)?.context("QQ 尚未配置")?;
            if !config.enabled {
                bail!("启用 QQ 独立配置后才能签发配对码")
            }
            let pairing = persistence::issue_pairing(tx, &config.app_id, now, ttl_seconds.into())?;
            receipt["pairing"] = json!({"id":pairing.id,"expires_at_ms":pairing.expires_at_ms});
            pairing_code = Some(pairing.code);
        }
        Command::ConfirmBinding {
            principal_id,
            expected_revision,
            identity,
            grant,
        } => {
            let config = configuration::current(tx)?.context("QQ 尚未配置")?;
            if !config.enabled || identity.app_id != config.app_id {
                bail!("QQ 应用或启用状态已改变")
            }
            policy::validate_id(&principal_id)?;
            grant.validate(now)?;
            validate_grant(&grant)?;
            for id in &grant.session_ids {
                if !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [id],
                    |r| r.get::<_, bool>(0),
                )? {
                    bail!("QQ 授权会话不存在")
                }
            }
            let expires = grant.expires_at_ms;
            let binding = persistence::confirm_binding(
                tx,
                &principal_id,
                expected_revision,
                &identity,
                serde_json::to_value(grant)?,
                expires,
                now,
            )?;
            receipt["principal_id"] = json!(binding.principal_id);
            receipt["revision"] = json!(binding.revision);
        }
        Command::RevokeBinding {
            principal_id,
            expected_revision,
        } => {
            policy::validate_id(&principal_id)?;
            let binding = persistence::revoke_binding(tx, &principal_id, expected_revision, now)?;
            receipt["principal_id"] = json!(binding.principal_id);
            receipt["revision"] = json!(binding.revision);
        }
    }
    save(tx, &envelope.id, &hash, &receipt)?;
    // Only the response gets the plaintext. A lost response is recovered via
    // GET metadata, never by replaying the code or silently issuing a new one.
    if let Some(code) = pairing_code {
        receipt["pairing"]["code"] = json!(code)
    }
    Ok(receipt)
}

/// Apply errors roll back completely; then this NEW transaction may durably
/// record a generic rejected receipt. An uncertain earlier successful commit is
/// returned unchanged, never overwritten by rejection. Unknown fields/secrets
/// are not fingerprinted into the database on parse failure.
pub(crate) fn reject(tx: &Transaction<'_>, envelope: &Value) -> Result<Value> {
    let id = envelope["id"].as_str().context("QQ 管理操作标识缺失")?;
    policy::validate_id(id)?;
    let hash = serde_json::from_value::<Envelope>(envelope.clone())
        .ok()
        .map(|v| digest(&v.command))
        .transpose()?
        .unwrap_or_else(|| "invalid_command".into());
    if let Some(receipt) = existing(tx, id, &hash)? {
        return Ok(receipt);
    }
    let receipt =
        json!({"command_id":id,"status":"rejected","reason":"scope_or_state_unavailable"});
    save(tx, id, &hash, &receipt)?;
    Ok(receipt)
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
