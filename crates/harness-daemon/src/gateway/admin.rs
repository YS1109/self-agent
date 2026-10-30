//! Local-owner metadata management only. This API is never a GatewayRpc.
//! Every effect and receipt is committed by the caller's one transaction; a
//! one-time pairing secret appears only in the first response, never SQLite.
use super::{
    configuration::{self, Config, ConfigPatch},
    persistence::{self, TrustedChannel},
    policy::{self, ResourceContext},
    wire::GatewayGrant,
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
        origin: String,
        device_id: String,
        enabled: bool,
    },
    IssuePairing {
        ttl_seconds: u64,
    },
    ConfirmBinding {
        principal_id: String,
        expected_revision: u64,
        expected_label: String,
        expected_pairing_id: String,
        grant: GatewayGrant,
    },
    RevokeBinding {
        principal_id: String,
        expected_revision: u64,
    },
}
pub(crate) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS gateway_admin_commands(id TEXT PRIMARY KEY,digest TEXT NOT NULL,receipt TEXT NOT NULL);")?;
    Ok(())
}
fn digest(command: &Command) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(command)?)
    ))
}
fn existing(db: &Connection, id: &str, digest: &str) -> Result<Option<Value>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT digest,receipt FROM gateway_admin_commands WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(original, receipt)| {
        if original != digest {
            bail!("网关管理操作标识已用于其他参数");
        }
        Ok(serde_json::from_str(&receipt)?)
    })
    .transpose()
}
fn save(tx: &Transaction<'_>, id: &str, digest: &str, receipt: &Value) -> Result<()> {
    let count: u64 = tx.query_row("SELECT COUNT(*) FROM gateway_admin_commands", [], |r| {
        r.get(0)
    })?;
    if count >= 10000 {
        bail!("网关管理审计达到上限，未清理或重放旧操作");
    }
    tx.execute(
        "INSERT INTO gateway_admin_commands VALUES(?1,?2,?3)",
        params![id, digest, serde_json::to_string(receipt)?],
    )?;
    Ok(())
}
pub(crate) fn receipt(db: &Connection, id: &str) -> Result<Option<Value>> {
    policy::identifier(id)?;
    let raw: Option<String> = db
        .query_row(
            "SELECT receipt FROM gateway_admin_commands WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|text| Ok(serde_json::from_str(&text)?)).transpose()
}
fn channel(tx: &Transaction<'_>) -> Result<TrustedChannel> {
    let c = configuration::current(tx)?.context("网关尚未配置")?;
    if !c.enabled {
        bail!("网关配置已停用");
    }
    Ok(TrustedChannel {
        device_id: c.device_id,
        config_revision: c.revision,
    })
}
pub(crate) fn apply(
    tx: &Transaction<'_>,
    value: &Value,
    now: u64,
    ready: impl FnOnce(&Config) -> Result<bool>,
    validate: impl FnOnce(&GatewayGrant) -> Result<()>,
    context: &impl ResourceContext,
) -> Result<Value> {
    let envelope: Envelope =
        serde_json::from_value(value.clone()).map_err(|_| anyhow::anyhow!("网关管理字段无效"))?;
    policy::identifier(&envelope.id)?;
    let hash = digest(&envelope.command)?;
    if let Some(receipt) = existing(tx, &envelope.id, &hash)? {
        return Ok(receipt);
    }
    let mut receipt = json!({"command_id":envelope.id,"status":"completed"});
    let mut code = None;
    match envelope.command {
        Command::Configure {
            config_id,
            expected_revision,
            origin,
            device_id,
            enabled,
        } => {
            let c = match (config_id, expected_revision) {
                (None, None) => {
                    if enabled {
                        bail!("首次网关配置必须停用创建");
                    }
                    configuration::create(tx, &origin, &device_id)?
                }
                (Some(id), Some(rev)) => configuration::update(
                    tx,
                    &id,
                    rev,
                    ConfigPatch {
                        origin,
                        device_id,
                        enabled,
                    },
                    now,
                    ready,
                )?,
                _ => bail!("网关配置标识和预期修订必须成对提供"),
            };
            receipt["config_id"] = json!(c.id);
            receipt["revision"] = json!(c.revision);
        }
        Command::IssuePairing { ttl_seconds } => {
            let pair = persistence::issue_pairing(tx, &channel(tx)?, now, ttl_seconds)?;
            receipt["pairing"] = json!({"id":pair.id,"expires_at_ms":pair.expires_at_ms});
            code = Some(pair.code);
        }
        Command::ConfirmBinding {
            principal_id,
            expected_revision,
            expected_label,
            expected_pairing_id,
            grant,
        } => {
            policy::validate_grant(&grant, now)?;
            validate(&grant)?;
            let b = persistence::confirm_binding(
                tx,
                &channel(tx)?,
                &principal_id,
                expected_revision,
                &expected_label,
                &expected_pairing_id,
                grant,
                now,
                context,
            )?;
            receipt["principal_id"] = json!(b.principal_id);
            receipt["revision"] = json!(b.revision);
        }
        Command::RevokeBinding {
            principal_id,
            expected_revision,
        } => {
            let b = persistence::revoke_binding(tx, &principal_id, expected_revision, now)?;
            receipt["principal_id"] = json!(b.principal_id);
            receipt["revision"] = json!(b.revision);
        }
    }
    save(tx, &envelope.id, &hash, &receipt)?;
    if let Some(code) = code {
        receipt["pairing"]["code"] = json!(code);
    }
    Ok(receipt)
}
/// Only after rollback of the failed apply transaction. Existing successful
/// commits remain authoritative; parsing failures never persist caller secrets.
pub(crate) fn reject(tx: &Transaction<'_>, value: &Value) -> Result<Value> {
    let id = value["id"].as_str().context("网关操作标识缺失")?;
    policy::identifier(id)?;
    let hash = serde_json::from_value::<Envelope>(value.clone())
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
