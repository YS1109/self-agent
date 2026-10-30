//! Trusted offline-restore invalidation. Caller owns one atomic transaction and
//! MUST roll it back on error. No vault/network calls; historical routes and
//! accepted-message identities remain, but restored state cannot grant authority.
use super::{configuration, credentials, persistence};
use anyhow::{bail, Result};
use rusqlite::{params, Transaction};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
const MAX_SAFE_REVISION: u64 = 9_007_199_254_740_991;
fn next_revision(value: u64) -> u64 {
    if value < MAX_SAFE_REVISION {
        value + 1
    } else {
        value
    }
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-'))
}
fn identity(value: &persistence::Identity) -> Result<()> {
    if !identifier(&value.app_id)
        || !identifier(&value.sender_open_id)
        || !identifier(&value.target_id)
        || (value.kind == persistence::ConversationKind::User
            && value.sender_open_id != value.target_id)
    {
        bail!("QQ 恢复身份格式无效");
    }
    Ok(())
}
fn decode<T: serde::de::DeserializeOwned>(body: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|_| anyhow::anyhow!("QQ 恢复元数据格式无效"))
}
fn entries(tx: &Transaction<'_>, sql: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Invoke after compiled schema migration, before installing the restored DB.
/// Saturated revisions remain unchanged: disabled/revoked status is the fence,
/// never wrap a revision or fail open by making it a small valid revision again.
pub(crate) fn invalidate(tx: &Transaction<'_>) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let foreign_keys: bool = tx
        .prepare("PRAGMA foreign_key_check")?
        .query([])?
        .next()?
        .is_some();
    if foreign_keys {
        bail!("QQ 恢复数据库关联无效");
    }
    let config = configuration::current(tx)?;
    if let Some(c) = &config {
        super::policy::validate_id(&c.id)?;
        configuration::credential_binding_digest(&c.app_id)?;
        c.reply_policy.validate()?;
        if c.revision > i64::MAX as u64 {
            bail!("QQ 恢复配置修订无效");
        }
        let mode: String = tx.query_row(
            "SELECT mode FROM qq_configuration WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if mode != "official" {
            bail!("QQ 恢复配置模式无效");
        }
    }
    // Validate duplicated JSON/index metadata before updating it in lockstep.
    let bindings = entries(tx, "SELECT principal_id,body FROM qq_bindings")?;
    for (id, body) in bindings {
        let mut b: persistence::Binding = decode(&body)?;
        super::policy::validate_id(&id)?;
        identity(&b.identity)?;
        let (key,app,revision,status,expiry):(String,String,u64,String,u64)=tx.query_row("SELECT identity_key,app_id,revision,status,expires_at_ms FROM qq_bindings WHERE principal_id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let expected = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&serde_json::to_value(&b.identity)?)?)
        );
        if b.principal_id != id
            || key != expected
            || app != b.identity.app_id
            || revision != b.revision
            || status != b.status
            || expiry != b.expires_at_ms
            || !matches!(status.as_str(), "pending" | "active" | "revoked")
            || !(b.grant.is_null() || b.grant.is_object())
        {
            bail!("QQ 恢复绑定元数据不一致");
        }
        if b.status != "revoked" {
            b.revision = next_revision(b.revision);
        }
        b.status = "revoked".into();
        b.expires_at_ms = 0;
        b.updated_at_ms = now;
        tx.execute("UPDATE qq_bindings SET revision=?1,status='revoked',expires_at_ms=0,body=?2 WHERE principal_id=?3",params![b.revision,serde_json::to_string(&b)?,id])?;
    }
    for (id, body) in entries(tx, "SELECT route_id,body FROM qq_reply_routes")? {
        let mut route: persistence::ReplyRoute = decode(&body)?;
        super::policy::validate_id(&id)?;
        identity(&route.identity)?;
        let (principal,revision,expiry,quota):(String,u64,u64,u32)=tx.query_row("SELECT principal_id,binding_revision,expires_at_ms,max_messages FROM qq_reply_routes WHERE route_id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        if route.route_id != id
            || route.principal_id != principal
            || route.binding_revision != revision
            || route.expires_at_ms != expiry
            || route.max_messages != quota
            || persistence::binding(tx, &principal)?.identity != route.identity
        {
            bail!("QQ 恢复回复来源不一致");
        }
        route.expires_at_ms = 0;
        tx.execute(
            "UPDATE qq_reply_routes SET expires_at_ms=0,body=?1 WHERE route_id=?2",
            params![serde_json::to_string(&route)?, id],
        )?;
    }
    for (id, body) in entries(tx, "SELECT inbox_id,body FROM qq_inbox")? {
        super::policy::validate_id(&id)?;
        let value: Value = decode(&body)?;
        let entry = persistence::inbox(tx, &id)?;
        identity(&entry.identity)?;
        let (app,message,command,route,received):(String,String,String,Option<String>,u64)=tx.query_row("SELECT app_id,message_id,command_id,route_id,received_at_ms FROM qq_inbox WHERE inbox_id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        if entry.identity.app_id != app
            || value["message_id"] != message
            || entry.command_id != command
            || entry.route_id != route
            || value["received_at_ms"] != received
            || !entry.canonical_input.is_object()
            || !matches!(entry.status.as_str(), "pending" | "handled")
        {
            bail!("QQ 恢复入站记录不一致");
        }
        if let Some(route) = route {
            persistence::route(tx, &route)?;
        }
        if entry.status == "pending" {
            persistence::mark_handled(
                tx,
                &id,
                &command,
                json!({"command_id":command,"status":"rejected","reason":"restored_no_replay"}),
                now,
            )?;
        }
    }
    // Validate saved transport JSON even though it will not be reusable.
    for (_, body) in entries(tx, "SELECT config_id,body FROM qq_checkpoint")? {
        let point: configuration::Checkpoint = decode(&body)?;
        if !identifier(&point.session_id)
            || !identifier(&point.bot_user_id)
            || point.sequence > i64::MAX as u64
        {
            bail!("QQ 恢复检查点无效");
        }
    }
    configuration::connection_view(tx)?;
    credentials::invalidate_restored(tx)?;
    tx.execute("UPDATE qq_pairings SET expires_at_ms=0", [])?;
    tx.execute("DELETE FROM qq_session_acl", [])?;
    tx.execute("DELETE FROM qq_selections", [])?;
    tx.execute("DELETE FROM qq_checkpoint", [])?;
    tx.execute("DELETE FROM qq_connection_state", [])?;
    tx.execute("UPDATE qq_outbox SET status=CASE status WHEN 'sending' THEN 'unknown' ELSE 'revoked' END,claim_nonce=NULL,reason='restored_no_replay',updated_at_ms=?1 WHERE status IN('pending','sending')",[now])?;
    tx.execute("UPDATE qq_notifications SET state=(SELECT status FROM qq_outbox WHERE id=qq_notifications.outbox_id) WHERE outbox_id IS NOT NULL AND EXISTS(SELECT 1 FROM qq_outbox WHERE id=qq_notifications.outbox_id)",[])?;
    if let Some(c) = config {
        let revision = next_revision(c.revision);
        tx.execute(
            "UPDATE qq_configuration SET enabled=0,credential_ref=NULL,revision=?1 WHERE id=?2",
            params![revision, c.id],
        )?;
        configuration::set_connection_state(
            tx,
            &c.id,
            revision,
            configuration::ConnectionState::Disabled,
            Some(configuration::Reason::Stopped),
        )?;
    }
    // Keep counter history; neither peer text nor a stale wall-clock admission
    // floor is authority. Every identity must pair anew after explicit enable.
    Ok(())
}
#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
