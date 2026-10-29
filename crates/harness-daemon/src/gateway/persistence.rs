//! Pure transactional browser authority and committed receipts. The caller owns
//! the transaction and must commit ACL + effect + receipt together. Errors must
//! roll back; no pending-command queue or network side effect is implemented.
use super::{
    policy::{self, ResourceContext},
    wire::{GatewayGrant, GatewayRequest, GatewayRpc},
};
use anyhow::{bail, Context, Result};
use harness_core::new_id;
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub(crate) struct TrustedChannel {
    pub device_id: String,
    pub config_revision: u64,
}
/// Secret is returned once to the local owner. Never log or serialize this type.
pub(crate) struct PairingCode {
    pub id: String,
    pub code: String,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub principal_id: String,
    pub device_id: String,
    pub config_revision: u64,
    pub revision: u64,
    pub status: String,
    pub label: String,
    pub pairing_id: String,
    pub expires_at_ms: u64,
    pub grant: Option<GatewayGrant>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
pub(crate) struct AuthorizedCommand<'tx, 'conn> {
    transaction: &'tx Transaction<'conn>,
    binding: Binding,
    command_id: String,
    session_id: String,
    request_hash: String,
}
impl AuthorizedCommand<'_, '_> {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    #[cfg(test)]
    pub fn command_id(&self) -> &str {
        &self.command_id
    }
}
pub(crate) enum CommandStart<'tx, 'conn> {
    Existing(Value),
    Apply(Box<AuthorizedCommand<'tx, 'conn>>),
}

pub(crate) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS gateway_pairings(
        id TEXT PRIMARY KEY,device_id TEXT NOT NULL,config_revision INTEGER NOT NULL,
        code_hash TEXT NOT NULL UNIQUE,expires_at_ms INTEGER NOT NULL,
        status TEXT NOT NULL CHECK(status IN('issued','claimed','revoked')),
        claimed_principal TEXT,created_at_ms INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS gateway_bindings(
        principal_id TEXT PRIMARY KEY,device_id TEXT NOT NULL,config_revision INTEGER NOT NULL,
        revision INTEGER NOT NULL,status TEXT NOT NULL CHECK(status IN('pending','active','revoked')),
        expires_at_ms INTEGER NOT NULL,body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS gateway_receipts(
        command_id TEXT PRIMARY KEY,device_id TEXT NOT NULL,config_revision INTEGER NOT NULL,
        principal_id TEXT NOT NULL,binding_revision INTEGER NOT NULL,session_id TEXT NOT NULL,
        request_hash TEXT NOT NULL,receipt TEXT NOT NULL,invalidated INTEGER NOT NULL DEFAULT 0,
        created_at_ms INTEGER NOT NULL);
      CREATE INDEX IF NOT EXISTS gateway_pairings_active ON gateway_pairings(device_id,status,expires_at_ms);
      CREATE INDEX IF NOT EXISTS gateway_bindings_device ON gateway_bindings(device_id,status);")?;
    Ok(())
}
fn valid_time(now: u64) -> Result<()> {
    if now > i64::MAX as u64 - 30 * 24 * 60 * 60 * 1000 {
        bail!("远程授权时间超出支持范围");
    }
    Ok(())
}
fn channel(channel: &TrustedChannel) -> Result<()> {
    policy::identifier(&channel.device_id)?;
    if channel.config_revision == 0 || channel.config_revision > 9_007_199_254_740_991 {
        bail!("远程配置版本无效");
    }
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bounded_rows(tx: &Transaction<'_>, table: &str, limit: u64) -> Result<()> {
    let count: u64 = tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })?;
    if count >= limit {
        bail!("远程持久记录达到容量上限，未创建新记录");
    }
    Ok(())
}
fn put_binding(tx: &Transaction<'_>, binding: &Binding) -> Result<()> {
    tx.execute("INSERT INTO gateway_bindings VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(principal_id) DO UPDATE SET revision=excluded.revision,status=excluded.status,expires_at_ms=excluded.expires_at_ms,body=excluded.body",params![binding.principal_id,binding.device_id,binding.config_revision,binding.revision,binding.status,binding.expires_at_ms,serde_json::to_string(binding)?])?;
    Ok(())
}
pub(crate) fn binding(tx: &Transaction<'_>, principal: &str) -> Result<Binding> {
    policy::identifier(principal)?;
    let (device,config,revision,status,expires,body):(String,u64,u64,String,u64,String)=tx.query_row("SELECT device_id,config_revision,revision,status,expires_at_ms,body FROM gateway_bindings WHERE principal_id=?1",[principal],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).context("远程身份不存在")?;
    let binding: Binding = serde_json::from_str(&body).context("远程身份记录无效")?;
    if binding.revision > 9_007_199_254_740_991
        || (binding.status == "pending" && binding.revision != 0)
        || (binding.status == "active" && binding.revision == 0)
        || !matches!(binding.status.as_str(), "pending" | "active" | "revoked")
        || binding.principal_id != principal
        || binding.device_id != device
        || binding.config_revision != config
        || binding.revision != revision
        || binding.status != status
        || binding.expires_at_ms != expires
    {
        bail!("远程身份索引与记录不一致，未授予访问");
    }
    Ok(binding)
}
/// Local-owner audit only; never return this unfiltered to a browser RPC.
pub(crate) fn bindings(tx: &Transaction<'_>, device_id: &str, limit: u32) -> Result<Vec<Binding>> {
    policy::identifier(device_id)?;
    if !(1..=256).contains(&limit) {
        bail!("远程身份查询上限为 1 至 256 条");
    }
    let mut query=tx.prepare("SELECT body FROM gateway_bindings WHERE device_id=?1 ORDER BY expires_at_ms DESC,principal_id LIMIT ?2")?;
    let bodies: Vec<String> = query
        .query_map(params![device_id, limit], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    bodies
        .into_iter()
        .map(|body| serde_json::from_str(&body).context("远程身份记录无效"))
        .collect()
}
fn same_device(binding: &Binding, channel: &TrustedChannel) -> Result<()> {
    if binding.device_id != channel.device_id || binding.config_revision != channel.config_revision
    {
        bail!("远程身份与当前设备配置不匹配");
    }
    Ok(())
}

pub(crate) fn issue_pairing(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    now: u64,
    ttl_seconds: u64,
) -> Result<PairingCode> {
    valid_time(now)?;
    self::channel(channel)?;
    if !(1..=600).contains(&ttl_seconds) {
        bail!("远程配对码有效期必须为 1 至 600 秒");
    }
    bounded_rows(tx, "gateway_pairings", 4096)?;
    let outstanding:u64=tx.query_row("SELECT COUNT(*) FROM gateway_pairings WHERE device_id=?1 AND config_revision=?2 AND status='issued' AND expires_at_ms>?3",params![channel.device_id,channel.config_revision,now],|row|row.get(0))?;
    if outstanding >= 32 {
        bail!("远程未使用配对码最多 32 个");
    }
    let pairing = PairingCode {
        id: new_id(),
        code: format!("{}{}", new_id().replace('-', ""), new_id().replace('-', "")),
        expires_at_ms: now + ttl_seconds * 1000,
    };
    tx.execute(
        "INSERT INTO gateway_pairings VALUES(?1,?2,?3,?4,?5,'issued',NULL,?6)",
        params![
            pairing.id,
            channel.device_id,
            channel.config_revision,
            digest(pairing.code.as_bytes()),
            pairing.expires_at_ms,
            now
        ],
    )?;
    Ok(pairing)
}
/// Invalid/unknown codes never create stranger rows. Repeated exact claim is
/// idempotent only for the same principal and label before owner confirmation.
pub(crate) fn claim_pairing(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    principal: &str,
    code: &str,
    label: &str,
    now: u64,
    context: &impl ResourceContext,
) -> Result<Binding> {
    valid_time(now)?;
    self::channel(channel)?;
    policy::identifier(principal)?;
    if !context.channel_current() {
        bail!("远程连接已失效，未处理配对");
    }
    if code.len() != 64
        || !code
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || label.trim().is_empty()
        || label.len() > 128
        || label.chars().any(char::is_control)
    {
        bail!("远程配对请求格式无效");
    }
    let found:Option<(String,String,Option<String>,u64)>=tx.query_row("SELECT id,status,claimed_principal,expires_at_ms FROM gateway_pairings WHERE code_hash=?1 AND device_id=?2 AND config_revision=?3",params![digest(code.as_bytes()),channel.device_id,channel.config_revision],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    let (id, status, claimed, expires) = found.context("配对码无效或已失效")?;
    if expires <= now || status == "revoked" {
        bail!("配对码无效或已失效");
    }
    if status == "claimed" {
        if claimed.as_deref() != Some(principal) {
            bail!("配对码已使用");
        }
        let current = binding(tx, principal)?;
        if current.status != "pending" || current.label != label {
            bail!("配对声明与原记录不一致");
        }
        return Ok(current);
    }
    if tx.query_row(
        "SELECT COUNT(*) FROM gateway_bindings WHERE principal_id=?1",
        [principal],
        |row| row.get::<_, u64>(0),
    )? > 0
    {
        bail!("远程身份标识已存在，必须重新配对");
    }
    bounded_rows(tx, "gateway_bindings", 4096)?;
    let result = Binding {
        principal_id: principal.into(),
        device_id: channel.device_id.clone(),
        config_revision: channel.config_revision,
        revision: 0,
        status: "pending".into(),
        label: label.into(),
        pairing_id: id.clone(),
        expires_at_ms: expires,
        grant: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    put_binding(tx, &result)?;
    let changed=tx.execute("UPDATE gateway_pairings SET status='claimed',claimed_principal=?1 WHERE id=?2 AND status='issued'",params![principal,id])?;
    if changed != 1 {
        bail!("配对码状态已改变");
    }
    Ok(result)
}
/// Local-owner only. Exact principal/revision/label/pairing ID are the pending
/// claim shown in the confirmation UI, not values trusted from the browser.
#[allow(clippy::too_many_arguments)] // Exact owner confirmation fields are intentionally separate.
pub(crate) fn confirm_binding(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    principal: &str,
    expected_revision: u64,
    expected_label: &str,
    expected_pairing_id: &str,
    grant: GatewayGrant,
    now: u64,
    context: &impl ResourceContext,
) -> Result<Binding> {
    valid_time(now)?;
    self::channel(channel)?;
    policy::validate_grant(&grant, now)?;
    for session in &grant.session_ids {
        if !context.session_exists(session)? {
            bail!("远程授权会话不存在");
        }
    }
    let mut current = binding(tx, principal)?;
    same_device(&current, channel)?;
    if current.status != "pending"
        || current.revision != expected_revision
        || current.label != expected_label
        || current.pairing_id != expected_pairing_id
        || current.expires_at_ms <= now
    {
        bail!("远程待确认身份已变化或过期");
    }
    current.status = "active".into();
    current.revision += 1;
    current.expires_at_ms = grant.expires_at_ms;
    current.grant = Some(grant);
    current.updated_at_ms = now;
    put_binding(tx, &current)?;
    Ok(current)
}
pub(crate) fn revoke_binding(
    tx: &Transaction<'_>,
    principal: &str,
    expected_revision: u64,
    now: u64,
) -> Result<Binding> {
    valid_time(now)?;
    let mut current = binding(tx, principal)?;
    if current.revision != expected_revision {
        bail!("远程身份版本已改变");
    }
    if current.status != "revoked" {
        current.status = "revoked".into();
        current.revision = current
            .revision
            .saturating_add(1)
            .min(9_007_199_254_740_991);
        current.updated_at_ms = now;
        put_binding(tx, &current)?;
    }
    Ok(current)
}
pub(crate) fn pair_status(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    principal: &str,
    now: u64,
) -> Result<Value> {
    valid_time(now)?;
    self::channel(channel)?;
    let current = binding(tx, principal)?;
    same_device(&current, channel)?;
    let expired = current.expires_at_ms <= now;
    let active = current.status == "active" && !expired;
    Ok(
        json!({"status":if expired {"revoked"}else{&current.status},"principal_id":current.principal_id,"binding_revision":current.revision,"expires_at_ms":current.expires_at_ms,
        "permissions":if active {serde_json::to_value(&current.grant.as_ref().context("远程授权记录缺失")?.permissions)?}else{json!({})},
        "session_ids":if active {json!(current.grant.as_ref().unwrap().session_ids)}else{json!([])}}),
    )
}
pub(crate) fn active_binding(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    principal: &str,
    revision: u64,
    now: u64,
    context: &impl ResourceContext,
) -> Result<Binding> {
    valid_time(now)?;
    self::channel(channel)?;
    if !context.channel_current() {
        bail!("远程连接已失效");
    }
    let binding = binding(tx, principal)?;
    same_device(&binding, channel)?;
    if binding.status != "active"
        || binding.revision != revision
        || binding.expires_at_ms <= now
        || binding.grant.is_none()
    {
        bail!("远程授权无效、版本变化或已过期");
    }
    let grant = binding.grant.as_ref().unwrap();
    if grant.expires_at_ms != binding.expires_at_ms {
        bail!("远程授权期限记录不一致");
    }
    policy::validate_grant(grant, now)?;
    Ok(binding)
}
pub(crate) fn authorize(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    request: &GatewayRequest,
    now: u64,
    context: &impl ResourceContext,
) -> Result<(Binding, Option<String>)> {
    policy::identifier(&request.request_id)?;
    let binding = active_binding(
        tx,
        channel,
        &request.principal_id,
        request.binding_revision,
        now,
        context,
    )?;
    let session = policy::authorize_rpc(
        binding.grant.as_ref().unwrap(),
        &binding.principal_id,
        binding.revision,
        &request.rpc,
        context,
    )?;
    Ok((binding, session))
}
fn request_hash(request: &GatewayRequest) -> Result<String> {
    let bytes = serde_json::to_vec(&request.rpc)?;
    if bytes.len() > 256 * 1024 {
        bail!("远程指令超过 256 KiB 上限");
    }
    Ok(digest(&bytes))
}
/// Check receipt BEFORE mutable resource-state predicates (e.g. answered
/// question now closed). Duplicate committed commands return their original
/// receipt, but only after current identity/revision/session authorization.
pub(crate) fn begin_command<'tx, 'conn>(
    tx: &'tx Transaction<'conn>,
    channel: &TrustedChannel,
    request: &GatewayRequest,
    now: u64,
    context: &impl ResourceContext,
) -> Result<CommandStart<'tx, 'conn>> {
    policy::identifier(&request.request_id)?;
    let GatewayRpc::Command {
        command_id,
        session_id,
        ..
    } = &request.rpc
    else {
        bail!("远程请求不是指令");
    };
    policy::identifier(command_id)?;
    policy::identifier(session_id)?;
    let binding = active_binding(
        tx,
        channel,
        &request.principal_id,
        request.binding_revision,
        now,
        context,
    )?;
    let grant = binding.grant.as_ref().unwrap();
    if !grant.session_ids.contains(session_id) || !context.session_exists(session_id)? {
        bail!("远程指令会话未授权");
    }
    let hash = request_hash(request)?;
    if let Some(existing) = lookup_receipt(
        tx,
        channel,
        &binding,
        command_id,
        Some((session_id, &hash)),
        context,
    )? {
        return Ok(CommandStart::Existing(existing));
    }
    policy::authorize_rpc(
        grant,
        &binding.principal_id,
        binding.revision,
        &request.rpc,
        context,
    )?;
    bounded_rows(tx, "gateway_receipts", 10000)?;
    Ok(CommandStart::Apply(Box::new(AuthorizedCommand {
        transaction: tx,
        binding,
        command_id: command_id.clone(),
        session_id: session_id.clone(),
        request_hash: hash,
    })))
}
/// Called in the SAME transaction as begin_command + the actual daemon effect.
/// Rechecks the channel and binding so revocation cannot be skipped at commit.
pub(crate) fn complete_command(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    permit: &AuthorizedCommand<'_, '_>,
    receipt: Value,
    now: u64,
    context: &impl ResourceContext,
) -> Result<()> {
    if !std::ptr::eq(tx, permit.transaction) {
        bail!("远程指令授权不能跨事务使用");
    }
    active_binding(
        tx,
        channel,
        &permit.binding.principal_id,
        permit.binding.revision,
        now,
        context,
    )?;
    if !receipt.is_object() || serde_json::to_vec(&receipt)?.len() > 64 * 1024 {
        bail!("远程指令收据格式无效或过大");
    }
    tx.execute(
        "INSERT INTO gateway_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0,?9)",
        params![
            permit.command_id,
            channel.device_id,
            channel.config_revision,
            permit.binding.principal_id,
            permit.binding.revision,
            permit.session_id,
            permit.request_hash,
            serde_json::to_string(&receipt)?,
            now
        ],
    )?;
    Ok(())
}
fn lookup_receipt(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    binding: &Binding,
    command_id: &str,
    exact: Option<(&str, &str)>,
    context: &impl ResourceContext,
) -> Result<Option<Value>> {
    type ReceiptRow = (String, u64, String, u64, String, String, String, u64);
    let row:Option<ReceiptRow>=tx.query_row("SELECT device_id,config_revision,principal_id,binding_revision,session_id,request_hash,receipt,invalidated FROM gateway_receipts WHERE command_id=?1",[command_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
    let Some((device, config, principal, revision, session, hash, receipt, invalidated)) = row
    else {
        return Ok(None);
    };
    if device != channel.device_id
        || config != channel.config_revision
        || principal != binding.principal_id
        || revision != binding.revision
        || invalidated != 0
        || !binding
            .grant
            .as_ref()
            .unwrap()
            .session_ids
            .contains(&session)
        || exact.is_some_and(|(expected_session, expected_hash)| {
            expected_session != session || expected_hash != hash
        })
    {
        bail!("远程指令标识冲突或收据不属于当前授权");
    }
    if !context.channel_current() || !context.session_exists(&session)? {
        bail!("远程连接或原收据会话已失效");
    }
    Ok(Some(
        serde_json::from_str(&receipt).context("远程指令收据损坏")?,
    ))
}
pub(crate) fn read_receipt(
    tx: &Transaction<'_>,
    channel: &TrustedChannel,
    principal: &str,
    revision: u64,
    command_id: &str,
    now: u64,
    context: &impl ResourceContext,
) -> Result<Value> {
    policy::identifier(command_id)?;
    let binding = active_binding(tx, channel, principal, revision, now, context)?;
    if !binding.grant.as_ref().unwrap().permissions.view {
        bail!("未授权读取指令收据");
    }
    lookup_receipt(tx, channel, &binding, command_id, None, context)?.context("远程指令收据不存在")
}
/// Backup restore invalidates reusable authority while retaining binding and
/// receipt audit. This schema has no pending commands/tokens to replay; pairing
/// hashes are revoked and committed receipts become inaccessible.
pub(crate) fn restore_invalidate(tx: &Transaction<'_>, now: u64) -> Result<()> {
    valid_time(now)?;
    let rows: Vec<(String, u64)> = {
        let mut query = tx.prepare(
            "SELECT principal_id,revision FROM gateway_bindings WHERE status!='revoked'",
        )?;
        let rows = query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (id, revision) in rows {
        revoke_binding(tx, &id, revision, now)?;
    }
    tx.execute(
        "UPDATE gateway_pairings SET status='revoked' WHERE status!='revoked'",
        [],
    )?;
    tx.execute(
        "UPDATE gateway_receipts SET invalidated=1 WHERE invalidated=0",
        [],
    )?;
    Ok(())
}
#[cfg(test)]
#[path = "persistence_tests.rs"]
mod tests;
