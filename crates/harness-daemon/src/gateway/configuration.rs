//! Gateway-only local configuration. No secret bytes or process fixture mode
//! enter SQLite/public patches. Callers own transactions and vault I/O occurs
//! outside them; configuration validation does not prove runtime vault availability.
use super::{persistence, policy, wire::GATEWAY_PROTOCOL};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
pub(crate) const CREDENTIAL_PURPOSE: &str = "gateway_device_token";
const MAX_REV: u64 = 9_007_199_254_740_991;
#[derive(Clone)]
pub(crate) struct Config {
    pub id: String,
    pub revision: u64,
    pub device_id: String,
    pub origin: String,
    pub enabled: bool,
    /// Endpoint/protocol identity digest, never a digest of secret material.
    pub scope_digest: String,
    credential_ref: Option<String>,
}
impl Config {
    pub(crate) fn credential_ref(&self) -> Option<&str> {
        self.credential_ref.as_deref()
    }
    pub(crate) fn public_view(&self) -> Value {
        json!({"id":self.id,"revision":self.revision,"device_id":self.device_id,"origin":self.origin,"enabled":self.enabled,"credential_state":if self.credential_ref.is_some(){"configured"}else{"missing"}})
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigPatch {
    pub device_id: String,
    pub origin: String,
    pub enabled: bool,
}
pub(crate) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS gateway_configuration(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),id TEXT NOT NULL UNIQUE,
        revision INTEGER NOT NULL CHECK(revision>=1),device_id TEXT NOT NULL,
        origin TEXT NOT NULL,enabled INTEGER NOT NULL CHECK(enabled IN(0,1)),credential_ref TEXT);",
    )?;
    Ok(())
}
fn revision(value: u64) -> Result<()> {
    if value == 0 || value > MAX_REV {
        bail!("网关配置修订超出支持范围");
    }
    Ok(())
}
fn normalized_origin(origin: &str, device: &str) -> Result<String> {
    policy::identifier(device)?;
    let endpoint = super::transport::endpoint(origin, device, false)?;
    // Use the exact normalized HTTPS origin corresponding to the validated
    // WSS endpoint (default ports, host case and IDNA normalized by Url).
    let mut url = endpoint;
    url.set_scheme("https")
        .map_err(|_| anyhow::anyhow!("网关协议无效"))?;
    url.set_path("/");
    Ok(url.to_string())
}
pub(crate) fn credential_binding_digest(origin: &str, device: &str) -> Result<String> {
    let origin = normalized_origin(origin, device)?;
    let endpoint = super::transport::endpoint(&origin, device, false)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"origin":origin,"device_id":device,"endpoint":endpoint.as_str(),"protocol":GATEWAY_PROTOCOL,"purpose":CREDENTIAL_PURPOSE})
        )?)
    ))
}
pub(crate) fn validate_device_token(token: &[u8]) -> Result<()> {
    if token.len() != 64
        || !token
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        bail!("网关设备凭据必须为本机登记生成的 64 字符小写十六进制值");
    }
    Ok(())
}
pub(crate) fn current(db: &Connection) -> Result<Option<Config>> {
    type Row = (String, u64, String, String, bool, Option<String>);
    let row:Option<Row>=db.query_row("SELECT id,revision,device_id,origin,enabled,credential_ref FROM gateway_configuration WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    row.map(|(id, rev, device_id, origin, enabled, credential_ref)| {
        policy::identifier(&id)?;
        revision(rev)?;
        let normalized = normalized_origin(&origin, &device_id)?;
        if normalized != origin {
            bail!("网关持久地址未规范化");
        }
        if credential_ref
            .as_ref()
            .is_some_and(|r| r.is_empty() || r.len() > 512 || r.chars().any(char::is_control))
        {
            bail!("网关凭据引用无效");
        }
        let scope_digest = credential_binding_digest(&origin, &device_id)?;
        Ok(Config {
            id,
            revision: rev,
            device_id,
            origin,
            enabled,
            scope_digest,
            credential_ref,
        })
    })
    .transpose()
}
pub(crate) fn load(db: &Connection, id: &str, rev: u64) -> Result<Config> {
    policy::identifier(id)?;
    revision(rev)?;
    let config = current(db)?.context("网关尚未配置")?;
    if config.id != id || config.revision != rev {
        bail!("网关配置版本已改变");
    }
    Ok(config)
}
pub(crate) fn create(tx: &Transaction<'_>, origin: &str, device_id: &str) -> Result<Config> {
    if current(tx)?.is_some() {
        bail!("网关配置已存在");
    }
    let origin = normalized_origin(origin, device_id)?;
    let id = harness_core::new_id();
    tx.execute(
        "INSERT INTO gateway_configuration VALUES(1,?1,1,?2,?3,0,NULL)",
        params![id, device_id, origin],
    )?;
    load(tx, &id, 1)
}
fn revoke_device(tx: &Transaction<'_>, device: &str, now: u64) -> Result<()> {
    let rows: Vec<(String, u64)> = {
        let mut query=tx.prepare("SELECT principal_id,revision FROM gateway_bindings WHERE device_id=?1 AND status!='revoked'")?;
        let rows = query
            .query_map([device], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (id, rev) in rows {
        persistence::revoke_binding(tx, &id, rev, now)?;
    }
    tx.execute(
        "UPDATE gateway_pairings SET status='revoked' WHERE device_id=?1 AND status!='revoked'",
        [device],
    )?;
    Ok(())
}
fn save(tx: &Transaction<'_>, mut config: Config, now: u64) -> Result<Config> {
    let old = config.revision;
    if old >= MAX_REV {
        bail!("网关配置修订已耗尽");
    }
    // Every new configuration generation invalidates old browser authority.
    revoke_device(tx, &config.device_id, now)?;
    config.revision += 1;
    config.scope_digest = credential_binding_digest(&config.origin, &config.device_id)?;
    let changed=tx.execute("UPDATE gateway_configuration SET revision=?1,device_id=?2,origin=?3,enabled=?4,credential_ref=?5 WHERE singleton=1 AND id=?6 AND revision=?7",params![config.revision,config.device_id,config.origin,config.enabled,config.credential_ref,config.id,old])?;
    if changed != 1 {
        bail!("网关配置版本已改变");
    }
    Ok(config)
}
/// `ready` validates the locally scoped credential reference, not an untrusted
/// flag. It must not perform OS/network work inside this transaction. A configured
/// reference is not proof of vault availability: the supervisor reads the vault
/// before each connection generation and fails closed if unavailable. An explicit
/// process-only fixture may instead validate its isolated canary token; it never
/// fabricates a stored credential reference or changes public patches.
pub(crate) fn update(
    tx: &Transaction<'_>,
    id: &str,
    rev: u64,
    patch: ConfigPatch,
    now: u64,
    ready: impl FnOnce(&Config) -> Result<bool>,
) -> Result<Config> {
    let mut config = load(tx, id, rev)?;
    let origin = normalized_origin(&patch.origin, &patch.device_id)?;
    let identity_changed = config.origin != origin || config.device_id != patch.device_id;
    if identity_changed && config.credential_ref.is_some() {
        bail!("修改网关来源或设备前必须先断开旧凭据");
    }
    revoke_device(tx, &config.device_id, now)?;
    config.origin = origin;
    config.device_id = patch.device_id;
    config.enabled = patch.enabled;
    config.scope_digest = credential_binding_digest(&config.origin, &config.device_id)?;
    if config.enabled && !ready(&config)? {
        bail!("网关设备凭据尚未就绪");
    }
    save(tx, config, now)
}
pub(crate) fn attach_credential(
    tx: &Transaction<'_>,
    id: &str,
    rev: u64,
    reference: &str,
    validate: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<Config> {
    let mut config = load(tx, id, rev)?;
    if reference.is_empty() || reference.len() > 512 || reference.chars().any(char::is_control) {
        bail!("网关凭据引用格式无效");
    }
    validate(reference, &config.scope_digest)?;
    config.credential_ref = Some(reference.into());
    config.enabled = false;
    save(tx, config, current_time_ms())
}
pub(crate) fn disconnect_credential(
    tx: &Transaction<'_>,
    id: &str,
    rev: u64,
    now: u64,
) -> Result<Config> {
    let mut config = load(tx, id, rev)?;
    config.enabled = false;
    config.credential_ref = None;
    save(tx, config, now)
}
fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}
/// Trusted offline restore only. No OS deletion is authorized by historical
/// references. Preserve audit, disable connector, detach reference and fence all
/// old browser authority/receipts. No pending command or reconnect replay exists.
pub(crate) fn invalidate_restored(tx: &Transaction<'_>, now: u64) -> Result<()> {
    persistence::restore_invalidate(tx, now)?;
    super::credentials::invalidate_restored(tx)?;
    if let Some(config) = current(tx)? {
        let next = config.revision.saturating_add(1).min(MAX_REV);
        tx.execute("UPDATE gateway_configuration SET revision=?1,enabled=0,credential_ref=NULL WHERE id=?2",params![next,config.id])?;
    }
    Ok(())
}
#[cfg(test)]
#[path = "configuration_tests.rs"]
mod tests;
