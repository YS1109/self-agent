//! QQ durable state only. Call mutating functions inside the caller's SQLite
//! transaction, commit before acknowledging a gateway dispatch, and perform all
//! network I/O outside that transaction. No transport sequence is input identity.
//! On any mutation error, roll back the whole caller transaction before recording
//! a rejection in a new transaction; never commit a partially failed operation.
use anyhow::{bail, Context, Result};
use harness_core::new_id;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const PASSIVE_WINDOW_MS: u64 = 300_000;
pub(crate) const PLATFORM_CLOCK_SKEW_MS: u64 = 30_000;
const PENDING_BINDING_MS: u64 = 600_000;
const MAX_ATTEMPTS: u32 = 3;
const MAX_INPUT_BYTES: usize = 32 * 1024;
const MAX_INBOX_ROWS: u64 = 10_000;
const MAX_PENDING_INPUTS: u64 = 256;
const MAX_OUTBOX_ROWS: u64 = 10_000;
const MAX_REPLY_BYTES: usize = 4000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConversationKind {
    User,
    Group,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Identity {
    pub app_id: String,
    pub sender_open_id: String,
    pub kind: ConversationKind,
    pub target_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Binding {
    pub principal_id: String,
    pub identity: Identity,
    pub revision: u64,
    pub status: String,
    pub grant: Value,
    pub expires_at_ms: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub input_after_ms: u64,
}

/// Only the local issue response may expose this value. Intentionally neither
/// Debug nor Serialize; the plaintext code is never written to SQLite.
pub(crate) struct PairingCode {
    pub id: String,
    pub code: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ReplyPolicy {
    pub window_ms: u64,
    pub max_user: u32,
    pub max_group: u32,
}

impl Default for ReplyPolicy {
    fn default() -> Self {
        Self {
            window_ms: PASSIVE_WINDOW_MS,
            max_user: 4,
            max_group: 5,
        }
    }
}

impl ReplyPolicy {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=PASSIVE_WINDOW_MS).contains(&self.window_ms)
            || !(1..=4).contains(&self.max_user)
            || !(1..=5).contains(&self.max_group)
        {
            bail!("QQ 被动回复策略只能收紧：窗口 1–300000 毫秒，单聊 1–4 次，群聊 1–5 次");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ReplyRoute {
    pub route_id: String,
    pub principal_id: String,
    pub binding_revision: u64,
    pub identity: Identity,
    pub message_id: String,
    pub received_at_ms: u64,
    #[serde(default)]
    pub accepted_at_ms: u64,
    pub expires_at_ms: u64,
    pub max_messages: u32,
}

/// Construct from an authenticated platform dispatch, not a caller-supplied
/// actor string. Pairing commands MUST use pairing_canonical, not raw content.
pub(crate) struct Inbound {
    pub identity: Identity,
    pub message_id: String,
    pub event_aliases: Vec<String>,
    pub canonical_input: Value,
    pub received_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct AcceptedInput {
    pub inbox_id: String,
    pub command_id: String,
    pub route_id: Option<String>,
    pub duplicate: bool,
    pub status: String,
}

/// Private processing data: do not serialize this as a public snapshot because
/// a pairing canonical_input contains a credential verifier.
pub(crate) struct InboxEntry {
    pub inbox_id: String,
    pub identity: Identity,
    pub canonical_input: Value,
    pub command_id: String,
    pub route_id: Option<String>,
    pub status: String,
    pub receipt: Option<Value>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutboxKind {
    Ack,
    Completion,
}

/// The public snapshot deliberately omits payload and claim_nonce.
#[derive(Clone, Debug)]
pub(crate) struct OutboxRecord {
    pub id: String,
    pub route_id: String,
    #[cfg(test)]
    pub kind: OutboxKind,
    pub payload: String,
    pub msg_seq: u32,
    pub status: String,
    pub attempts: u32,
    pub retry_at_ms: u64,
    pub claim_nonce: Option<String>,
    #[cfg(test)]
    pub platform_id: Option<String>,
    #[cfg(test)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct OutboxClaim {
    pub record: OutboxRecord,
    pub route: ReplyRoute,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FailureReason {
    PlatformRejected,
    ConfigurationDisabled,
}

pub(crate) enum DeliveryOutcome {
    Sent {
        platform_id: String,
    },
    RateLimited {
        retry_at_ms: u64,
    },
    Failed {
        reason: FailureReason,
    },
    /// Timeout, disconnect after posting, 5xx or an uncertain local finalization.
    Unknown,
}

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS qq_pairings(
            id TEXT PRIMARY KEY, app_id TEXT NOT NULL, code_hash TEXT NOT NULL UNIQUE,
            expires_at_ms INTEGER NOT NULL, claimed_binding_id TEXT, created_at_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS qq_ignored_inputs(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), count INTEGER NOT NULL,
            latest_platform_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL, clock_fault_ms INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS qq_pairing_admissions(
            code_hash TEXT PRIMARY KEY, inbox_id TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS qq_pairings_app_expiry ON qq_pairings(app_id,expires_at_ms);
         CREATE TABLE IF NOT EXISTS qq_bindings(
            principal_id TEXT PRIMARY KEY, identity_key TEXT NOT NULL UNIQUE, app_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK(revision>=0), status TEXT NOT NULL CHECK(status IN('pending','active','revoked')), expires_at_ms INTEGER NOT NULL, body TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS qq_inbox(
            inbox_id TEXT PRIMARY KEY, app_id TEXT NOT NULL, message_id TEXT NOT NULL,
            canonical_hash TEXT NOT NULL, command_id TEXT NOT NULL UNIQUE, status TEXT NOT NULL CHECK(status IN('pending','handled')),
            route_id TEXT, body TEXT NOT NULL, receipt TEXT, received_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL, UNIQUE(app_id,message_id));
         CREATE INDEX IF NOT EXISTS qq_inbox_pending ON qq_inbox(status,received_at_ms);
         CREATE TABLE IF NOT EXISTS qq_event_aliases(
            app_id TEXT NOT NULL, alias TEXT NOT NULL, inbox_id TEXT NOT NULL REFERENCES qq_inbox(inbox_id),
            PRIMARY KEY(app_id,alias));
         CREATE TABLE IF NOT EXISTS qq_reply_routes(
            route_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL REFERENCES qq_bindings(principal_id),
            binding_revision INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL,
            next_msg_seq INTEGER NOT NULL CHECK(next_msg_seq>=1), max_messages INTEGER NOT NULL CHECK(max_messages BETWEEN 1 AND 5), body TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS qq_outbox(
            id TEXT PRIMARY KEY, route_id TEXT NOT NULL REFERENCES qq_reply_routes(route_id),
            logical_key TEXT NOT NULL UNIQUE, kind TEXT NOT NULL CHECK(kind IN('ack','completion')), payload TEXT NOT NULL, content_hash TEXT NOT NULL,
            msg_seq INTEGER NOT NULL CHECK(msg_seq BETWEEN 1 AND 5), status TEXT NOT NULL CHECK(status IN('pending','sending','sent','unknown','failed','expired','revoked')), attempts INTEGER NOT NULL CHECK(attempts BETWEEN 0 AND 3),
            retry_at_ms INTEGER NOT NULL, claim_nonce TEXT, platform_id TEXT, reason TEXT,
            created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL, UNIQUE(route_id,logical_key));
         CREATE UNIQUE INDEX IF NOT EXISTS qq_outbox_logical_identity ON qq_outbox(logical_key);
         CREATE INDEX IF NOT EXISTS qq_outbox_pending ON qq_outbox(status,retry_at_ms,created_at_ms);
         CREATE TABLE IF NOT EXISTS qq_session_acl(
            principal_id TEXT NOT NULL REFERENCES qq_bindings(principal_id), session_id TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL, PRIMARY KEY(principal_id,session_id));",
    )?;
    let columns = {
        let mut query = db.prepare("PRAGMA table_info(qq_ignored_inputs)")?;
        let rows = query.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if !columns.iter().any(|column| column == "clock_fault_ms") {
        // SAVEPOINT also works inside the future outer migration ledger. A
        // failed/crashed backfill cannot leave a present-but-zero replay fence.
        db.execute_batch("SAVEPOINT qq_ignored_clock_migration;")?;
        let result = (|| -> Result<()> {
            db.execute(
                "ALTER TABLE qq_ignored_inputs ADD COLUMN clock_fault_ms INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            // Legacy schema cannot distinguish skew from ordinary ignores.
            db.execute(
                "UPDATE qq_ignored_inputs SET clock_fault_ms=latest_platform_ms",
                [],
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            db.execute_batch(
                "ROLLBACK TO qq_ignored_clock_migration; RELEASE qq_ignored_clock_migration;",
            )?;
            return Err(error);
        }
        db.execute_batch("RELEASE qq_ignored_clock_migration;")?;
    }
    Ok(())
}

fn clock(now_ms: u64) -> Result<()> {
    if now_ms > i64::MAX as u64 - PENDING_BINDING_MS {
        bail!("QQ 时间超出支持范围");
    }
    Ok(())
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn opaque_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn validate_identity(identity: &Identity) -> Result<()> {
    if !identifier(&identity.app_id)
        || !identifier(&identity.sender_open_id)
        || !identifier(&identity.target_id)
        || (identity.kind == ConversationKind::User
            && identity.sender_open_id != identity.target_id)
    {
        bail!("QQ 应用、发送者或会话目标无效");
    }
    Ok(())
}

fn hash(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn identity_key(identity: &Identity) -> Result<String> {
    hash(&serde_json::to_value(identity)?)
}

fn decode<T: DeserializeOwned>(body: String) -> Result<T> {
    serde_json::from_str(&body).map_err(|_| anyhow::anyhow!("QQ 持久记录格式无效"))
}

pub(crate) fn binding(db: &Connection, principal_id: &str) -> Result<Binding> {
    let body: String = db
        .query_row(
            "SELECT body FROM qq_bindings WHERE principal_id=?1",
            [principal_id],
            |row| row.get(0),
        )
        .optional()?
        .context("QQ 绑定不存在")?;
    decode(body)
}

fn find_binding(db: &Connection, identity: &Identity) -> Result<Option<Binding>> {
    let body: Option<String> = db
        .query_row(
            "SELECT body FROM qq_bindings WHERE identity_key=?1",
            [identity_key(identity)?],
            |row| row.get(0),
        )
        .optional()?;
    body.map(decode).transpose()
}

fn alive(binding: &Binding, now_ms: u64) -> bool {
    binding.status == "active" && now_ms < binding.expires_at_ms
}

pub(crate) fn live_binding(
    db: &Connection,
    identity: &Identity,
    now_ms: u64,
) -> Result<Option<Binding>> {
    clock(now_ms)?;
    validate_identity(identity)?;
    Ok(find_binding(db, identity)?.filter(|binding| alive(binding, now_ms)))
}

fn save_binding(tx: &Transaction<'_>, binding: &Binding, previous: Option<u64>) -> Result<()> {
    let body = serde_json::to_string(binding)?;
    let changed = if let Some(previous) = previous {
        tx.execute("UPDATE qq_bindings SET revision=?1,status=?2,expires_at_ms=?3,body=?4 WHERE principal_id=?5 AND revision=?6",
            params![binding.revision,binding.status,binding.expires_at_ms,body,binding.principal_id,previous])?
    } else {
        tx.execute("INSERT INTO qq_bindings(principal_id,identity_key,app_id,revision,status,expires_at_ms,body) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![binding.principal_id,identity_key(&binding.identity)?,binding.identity.app_id,binding.revision,binding.status,binding.expires_at_ms,body])?
    };
    if changed != 1 {
        bail!("QQ 绑定版本已改变，未应用决定");
    }
    Ok(())
}

pub(crate) fn issue_pairing(
    tx: &Transaction<'_>,
    app_id: &str,
    now_ms: u64,
    ttl_seconds: u64,
) -> Result<PairingCode> {
    clock(now_ms)?;
    if !identifier(app_id) || !(1..=600).contains(&ttl_seconds) {
        bail!("QQ 配对应用或有效期无效（1–600 秒）");
    }
    let pending: u64 = tx.query_row("SELECT COUNT(*) FROM qq_pairings WHERE app_id=?1 AND expires_at_ms>?2 AND claimed_binding_id IS NULL", params![app_id,now_ms],|row|row.get(0))?;
    if pending >= 32 {
        bail!("QQ 未使用配对码已达到 32 个上限");
    }
    let issued = PairingCode {
        id: new_id(),
        code: new_id().replace('-', ""),
        expires_at_ms: now_ms + ttl_seconds * 1000,
    };
    let digest = pairing_canonical(app_id, &issued.code)?;
    tx.execute("INSERT INTO qq_pairings(id,app_id,code_hash,expires_at_ms,created_at_ms) VALUES(?1,?2,?3,?4,?5)",
        params![issued.id,app_id,digest["code_hash"].as_str().unwrap(),issued.expires_at_ms,now_ms])?;
    Ok(issued)
}

/// Private canonical form safe to persist, but do not expose its verifier in a
/// public view. Never pass a raw /配对 command to accept_input.
pub(crate) fn pairing_canonical(app_id: &str, code: &str) -> Result<Value> {
    if !identifier(app_id) {
        bail!("QQ 配对应用无效");
    }
    let code = code.trim().to_ascii_lowercase();
    if code.len() != 32 || !code.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("QQ 配对码格式无效");
    }
    Ok(json!({"kind":"pairing","code_hash":hash(&json!({"app_id":app_id,"code":code}))?}))
}

fn redeem_digest(
    tx: &Transaction<'_>,
    identity: &Identity,
    digest: &str,
    now_ms: u64,
) -> Result<Binding> {
    clock(now_ms)?;
    validate_identity(identity)?;
    let code_id: Option<String> = tx.query_row("SELECT id FROM qq_pairings WHERE app_id=?1 AND code_hash=?2 AND expires_at_ms>?3 AND claimed_binding_id IS NULL",
        params![identity.app_id,digest,now_ms],|row|row.get(0)).optional()?;
    let code_id = code_id.context("QQ 配对码无效、已过期或已使用")?;
    let existing = find_binding(tx, identity)?;
    if existing
        .as_ref()
        .is_some_and(|binding| binding.status != "revoked" && now_ms < binding.expires_at_ms)
    {
        bail!("此 QQ 身份已有待确认或有效绑定，请在本机管理");
    }
    let previous = existing.as_ref().map(|binding| binding.revision);
    let binding = Binding {
        principal_id: existing
            .as_ref()
            .map(|binding| binding.principal_id.clone())
            .unwrap_or_else(new_id),
        identity: identity.clone(),
        revision: previous.map(|rev| rev + 1).unwrap_or(0),
        status: "pending".into(),
        grant: Value::Null,
        expires_at_ms: now_ms + PENDING_BINDING_MS,
        created_at_ms: existing
            .map(|binding| binding.created_at_ms)
            .unwrap_or(now_ms),
        updated_at_ms: now_ms,
        input_after_ms: 0,
    };
    let changed = tx.execute("UPDATE qq_pairings SET claimed_binding_id=?1 WHERE id=?2 AND claimed_binding_id IS NULL AND expires_at_ms>?3",params![binding.principal_id,code_id,now_ms])?;
    if changed != 1 {
        bail!("QQ 配对码已被其他请求使用");
    }
    save_binding(tx, &binding, previous)?;
    // Re-pairing never inherits sessions created under an earlier grant.
    tx.execute(
        "DELETE FROM qq_session_acl WHERE principal_id=?1",
        [&binding.principal_id],
    )?;
    Ok(binding)
}

#[cfg(test)]
pub(crate) fn redeem_pairing(
    tx: &Transaction<'_>,
    identity: &Identity,
    code: &str,
    now_ms: u64,
) -> Result<Binding> {
    let canonical = pairing_canonical(&identity.app_id, code)?;
    redeem_digest(
        tx,
        identity,
        canonical["code_hash"].as_str().unwrap(),
        now_ms,
    )
}

/// Local-owner only. Caller must authenticate local ownership and validate the
/// grant's semantics; no remote actor/source string can invoke this authority.
pub(crate) fn confirm_binding(
    tx: &Transaction<'_>,
    principal_id: &str,
    expected_revision: u64,
    expected_identity: &Identity,
    grant: Value,
    expires_at_ms: u64,
    now_ms: u64,
) -> Result<Binding> {
    clock(now_ms)?;
    clock(expires_at_ms)?;
    validate_identity(expected_identity)?;
    if expires_at_ms <= now_ms
        || !grant.is_object()
        || serde_json::to_vec(&grant)?.len() > 16 * 1024
    {
        bail!("QQ 授权必须明确有效期并提供有界授权对象");
    }
    let mut current = binding(tx, principal_id)?;
    if current.revision != expected_revision
        || &current.identity != expected_identity
        || current.status != "pending"
        || now_ms >= current.expires_at_ms
    {
        bail!("QQ 待确认身份、范围、版本或有效期已改变");
    }
    current.revision += 1;
    current.status = "active".into();
    current.grant = grant;
    current.expires_at_ms = expires_at_ms;
    current.updated_at_ms = now_ms;
    // Immutable activation fence: ignored input cannot acquire authority after
    // confirmation/rebind, including accepted future skew and clock rollback.
    let ignored: u64 = tx
        .query_row(
            "SELECT latest_platform_ms FROM qq_ignored_inputs WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    current.input_after_ms = now_ms.saturating_add(PLATFORM_CLOCK_SKEW_MS).max(ignored);
    if expires_at_ms <= current.input_after_ms {
        bail!("QQ 授权到期早于安全接收窗口，请检查平台时间与本机时钟");
    }
    save_binding(tx, &current, Some(expected_revision))?;
    Ok(current)
}

pub(crate) fn revoke_binding(
    tx: &Transaction<'_>,
    principal_id: &str,
    expected_revision: u64,
    now_ms: u64,
) -> Result<Binding> {
    clock(now_ms)?;
    let mut current = binding(tx, principal_id)?;
    if current.revision != expected_revision {
        bail!("QQ 绑定版本已改变，未撤销其他版本");
    }
    if current.status == "revoked" {
        return Ok(current);
    }
    current.revision += 1;
    current.status = "revoked".into();
    current.updated_at_ms = now_ms;
    save_binding(tx, &current, Some(expected_revision))?;
    tx.execute("UPDATE qq_outbox SET status='revoked',reason='binding_revoked',updated_at_ms=?1 WHERE status='pending' AND route_id IN(SELECT route_id FROM qq_reply_routes WHERE principal_id=?2)",params![now_ms,principal_id])?;
    // sending is in flight and cannot be recalled. Finalization must not retry.
    Ok(current)
}

pub(crate) fn selection_key(binding: &Binding) -> Result<String> {
    Ok(format!(
        "qq-conversation:{}",
        hash(
            &json!({"principal_id":binding.principal_id,"binding_revision":binding.revision,"identity":binding.identity})
        )?
    ))
}

pub(crate) fn grant_session(
    tx: &Transaction<'_>,
    principal_id: &str,
    session_id: &str,
    now_ms: u64,
) -> Result<()> {
    clock(now_ms)?;
    if !identifier(session_id) || !alive(&binding(tx, principal_id)?, now_ms) {
        bail!("QQ 身份已失效或会话标识无效");
    }
    if allowed_session(tx, principal_id, session_id, now_ms)? {
        return Ok(());
    }
    let initial = binding(tx, principal_id)?.grant["session_ids"]
        .as_array()
        .map_or(0, Vec::len);
    let created: u64 = tx.query_row(
        "SELECT COUNT(*) FROM qq_session_acl WHERE principal_id=?1",
        [principal_id],
        |row| row.get(0),
    )?;
    if initial as u64 + created >= 32 {
        bail!("QQ 绑定可访问会话达到 32 个上限，未创建或扩大授权");
    }
    tx.execute("INSERT OR IGNORE INTO qq_session_acl(principal_id,session_id,created_at_ms) VALUES(?1,?2,?3)",params![principal_id,session_id,now_ms])?;
    Ok(())
}

pub(crate) fn allowed_session(
    db: &Connection,
    principal_id: &str,
    session_id: &str,
    now_ms: u64,
) -> Result<bool> {
    clock(now_ms)?;
    let current = binding(db, principal_id)?;
    if !alive(&current, now_ms) {
        return Ok(false);
    }
    if current.grant["session_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(session_id)))
    {
        return Ok(true);
    }
    Ok(db
        .query_row(
            "SELECT 1 FROM qq_session_acl WHERE principal_id=?1 AND session_id=?2",
            params![principal_id, session_id],
            |row| row.get::<_, u8>(0),
        )
        .optional()?
        .is_some())
}

pub(crate) fn inbox(db: &Connection, inbox_id: &str) -> Result<InboxEntry> {
    let (body, status, receipt): (String, String, Option<String>) = db
        .query_row(
            "SELECT body,status,receipt FROM qq_inbox WHERE inbox_id=?1",
            [inbox_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .context("QQ 输入不存在")?;
    let value: Value = decode(body)?;
    Ok(InboxEntry {
        inbox_id: inbox_id.into(),
        identity: serde_json::from_value(value["identity"].clone())?,
        canonical_input: value["canonical_input"].clone(),
        command_id: value["command_id"]
            .as_str()
            .context("QQ 指令标识缺失")?
            .into(),
        route_id: value["route_id"].as_str().map(str::to_owned),
        status,
        receipt: receipt.map(decode).transpose()?,
    })
}

fn add_aliases(
    tx: &Transaction<'_>,
    app_id: &str,
    inbox_id: &str,
    aliases: &[String],
) -> Result<()> {
    for alias in aliases {
        let previous: Option<String> = tx
            .query_row(
                "SELECT inbox_id FROM qq_event_aliases WHERE app_id=?1 AND alias=?2",
                params![app_id, alias],
                |row| row.get(0),
            )
            .optional()?;
        if previous
            .as_deref()
            .is_some_and(|previous| previous != inbox_id)
        {
            bail!("QQ 事件别名已关联其他消息，未重复执行");
        }
        if previous.is_none() {
            let count: u32 = tx.query_row(
                "SELECT COUNT(*) FROM qq_event_aliases WHERE inbox_id=?1",
                [inbox_id],
                |row| row.get(0),
            )?;
            if count >= 16 {
                bail!("QQ 单消息事件别名达到 16 个上限");
            }
            tx.execute(
                "INSERT INTO qq_event_aliases(app_id,alias,inbox_id) VALUES(?1,?2,?3)",
                params![app_id, alias, inbox_id],
            )?;
        }
    }
    Ok(())
}

/// Constant-size diagnostic/watermark only. Even a time-dependent rejected
/// event must contribute before its checkpoint is acknowledged, so later clock
/// correction cannot turn a previously ignored message into fresh authority.
pub(crate) fn record_ignored_timestamp(
    tx: &Transaction<'_>,
    timestamp_ms: u64,
    now_ms: u64,
) -> Result<()> {
    clock(timestamp_ms)?;
    clock(now_ms)?;
    tx.execute("INSERT INTO qq_ignored_inputs(singleton,count,latest_platform_ms,updated_at_ms) VALUES(1,1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET count=MIN(count+1,9007199254740991),latest_platform_ms=MAX(latest_platform_ms,excluded.latest_platform_ms),updated_at_ms=excluded.updated_at_ms",params![timestamp_ms,now_ms])?;
    Ok(())
}

pub(crate) fn record_clock_skew(
    tx: &Transaction<'_>,
    timestamp_ms: u64,
    now_ms: u64,
) -> Result<()> {
    record_ignored_timestamp(tx, timestamp_ms, now_ms)?;
    tx.execute(
        "UPDATE qq_ignored_inputs SET clock_fault_ms=MAX(clock_fault_ms,?1) WHERE singleton=1",
        [timestamp_ms],
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn accept_input(
    tx: &Transaction<'_>,
    input: &Inbound,
    now_ms: u64,
) -> Result<AcceptedInput> {
    accept_input_with_policy(tx, input, now_ms, &ReplyPolicy::default())?
        .context("QQ 输入未获控制入站资格")
}

pub(crate) fn accept_input_with_policy(
    tx: &Transaction<'_>,
    input: &Inbound,
    now_ms: u64,
    policy: &ReplyPolicy,
) -> Result<Option<AcceptedInput>> {
    policy.validate()?;
    clock(now_ms)?;
    clock(input.received_at_ms)?;
    validate_identity(&input.identity)?;
    if !opaque_id(&input.message_id)
        || input.event_aliases.len() > 16
        || input.event_aliases.iter().any(|alias| !opaque_id(alias))
        || !input.canonical_input.is_object()
        || serde_json::to_vec(&input.canonical_input)?.len() > MAX_INPUT_BYTES
    {
        bail!("QQ 输入、事件别名或内容大小无效");
    }
    if input
        .canonical_input
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|content| content.split_whitespace().next() == Some("/配对"))
    {
        bail!("QQ 配对指令必须先规范化为摘要，不能持久化原始配对码");
    }
    if input.canonical_input["kind"] == "pairing" {
        let fields = input.canonical_input.as_object().unwrap();
        if fields.len() != 2
            || !fields.contains_key("kind")
            || !fields
                .get("code_hash")
                .and_then(Value::as_str)
                .is_some_and(|value| {
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        {
            bail!("QQ 配对输入只能保存摘要，不能保存原始配对码");
        }
    }
    if input.received_at_ms > now_ms.saturating_add(PLATFORM_CLOCK_SKEW_MS) {
        bail!("QQ 消息时间来自异常未来，未接收控制");
    }
    let canonical_hash = hash(
        &json!({"identity":input.identity,"message_id":input.message_id,"received_at_ms":input.received_at_ms,"input":input.canonical_input}),
    )?;
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT inbox_id,canonical_hash FROM qq_inbox WHERE app_id=?1 AND message_id=?2",
            params![input.identity.app_id, input.message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((id, digest)) = existing {
        if digest != canonical_hash {
            bail!("QQ 消息标识已对应不同内容或身份，未重复执行");
        }
        add_aliases(tx, &input.identity.app_id, &id, &input.event_aliases)?;
        let previous = inbox(tx, &id)?;
        return Ok(Some(AcceptedInput {
            inbox_id: id,
            command_id: previous.command_id,
            route_id: previous.route_id,
            duplicate: true,
            status: previous.status,
        }));
    }
    let pairing = input.canonical_input["kind"] == "pairing";
    let current = if pairing {
        None
    } else {
        live_binding(tx, &input.identity, now_ms)?
    };
    let admitted = if pairing {
        let digest = input.canonical_input["code_hash"].as_str().unwrap();
        tx.query_row("SELECT 1 FROM qq_pairings p WHERE p.app_id=?1 AND p.code_hash=?2 AND p.expires_at_ms>?3 AND p.claimed_binding_id IS NULL AND p.created_at_ms<=?4 AND NOT EXISTS(SELECT 1 FROM qq_pairing_admissions a WHERE a.code_hash=p.code_hash)",params![input.identity.app_id,digest,now_ms,input.received_at_ms],|r|r.get::<_,u8>(0)).optional()?.is_some()
    } else {
        current.as_ref().is_some_and(|b| {
            input.received_at_ms
                > if b.input_after_ms == 0 {
                    b.updated_at_ms.saturating_add(PLATFORM_CLOCK_SKEW_MS)
                } else {
                    b.input_after_ms
                }
        })
    };
    let clock_floor: u64 = tx
        .query_row(
            "SELECT clock_fault_ms FROM qq_ignored_inputs WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if !admitted || input.received_at_ms <= clock_floor {
        // Constant-size aggregate only: no attacker-chosen identity/message rows,
        // content, aliases or command IDs. Checkpoint commits with this result.
        record_ignored_timestamp(tx, input.received_at_ms, now_ms)?;
        return Ok(None);
    }
    let total: u64 = tx.query_row("SELECT COUNT(*) FROM qq_inbox", [], |row| row.get(0))?;
    let pending: u64 = tx.query_row(
        "SELECT COUNT(*) FROM qq_inbox WHERE status='pending'",
        [],
        |row| row.get(0),
    )?;
    if total >= MAX_INBOX_ROWS || pending >= MAX_PENDING_INPUTS {
        bail!("QQ 入站记录或待处理数量达到安全上限，已停止接收新输入");
    }
    let id = new_id();
    let command_id = format!(
        "qq:{}",
        hash(&json!({"app_id":input.identity.app_id,"message_id":input.message_id}))?
    );
    let route_id = current.as_ref().map(|_| new_id());
    let body = json!({"identity":input.identity,"message_id":input.message_id,"canonical_input":input.canonical_input,
        "command_id":command_id,"route_id":route_id,"received_at_ms":input.received_at_ms});
    tx.execute("INSERT INTO qq_inbox(inbox_id,app_id,message_id,canonical_hash,command_id,status,route_id,body,received_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,'pending',?6,?7,?8,?9)",
        params![id,input.identity.app_id,input.message_id,canonical_hash,command_id,route_id,serde_json::to_string(&body)?,input.received_at_ms,now_ms])?;
    add_aliases(tx, &input.identity.app_id, &id, &input.event_aliases)?;
    if pairing {
        tx.execute(
            "INSERT INTO qq_pairing_admissions(code_hash,inbox_id) VALUES(?1,?2)",
            params![input.canonical_input["code_hash"].as_str().unwrap(), id],
        )?;
    }
    if let (Some(current), Some(route_id)) = (current, route_id.as_ref()) {
        let route = ReplyRoute {
            route_id: route_id.clone(),
            principal_id: current.principal_id,
            binding_revision: current.revision,
            identity: input.identity.clone(),
            message_id: input.message_id.clone(),
            received_at_ms: input.received_at_ms,
            accepted_at_ms: now_ms,
            expires_at_ms: (input.received_at_ms.min(now_ms) + policy.window_ms)
                .min(current.expires_at_ms),
            max_messages: if input.identity.kind == ConversationKind::User {
                policy.max_user
            } else {
                policy.max_group
            },
        };
        tx.execute("INSERT INTO qq_reply_routes(route_id,principal_id,binding_revision,expires_at_ms,next_msg_seq,max_messages,body) VALUES(?1,?2,?3,?4,1,?5,?6)",params![route.route_id,route.principal_id,route.binding_revision,route.expires_at_ms,route.max_messages,serde_json::to_string(&route)?])?;
    }
    Ok(Some(AcceptedInput {
        inbox_id: id,
        command_id,
        route_id,
        duplicate: false,
        status: "pending".into(),
    }))
}

pub(crate) fn pending_inputs(db: &Connection, limit: u32) -> Result<Vec<InboxEntry>> {
    if !(1..=128).contains(&limit) {
        bail!("QQ 输入批次必须在 1 至 128 条之间");
    }
    let mut statement=db.prepare("SELECT inbox_id FROM qq_inbox WHERE status='pending' ORDER BY received_at_ms,inbox_id LIMIT ?1")?;
    let ids = statement
        .query_map([limit], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ids.iter().map(|id| inbox(db, id)).collect()
}

pub(crate) fn mark_handled(
    tx: &Transaction<'_>,
    inbox_id: &str,
    command_id: &str,
    receipt: Value,
    now_ms: u64,
) -> Result<()> {
    clock(now_ms)?;
    if !receipt.is_object() || serde_json::to_vec(&receipt)?.len() > 16 * 1024 {
        bail!("QQ 指令收据无效或过大");
    }
    let input = inbox(tx, inbox_id)?;
    if input.command_id != command_id {
        bail!("QQ 输入与指令标识不匹配");
    }
    if input.status == "handled" {
        if input.receipt.as_ref() == Some(&receipt) {
            return Ok(());
        }
        bail!("QQ 输入已对应不同处理收据");
    }
    let changed=tx.execute("UPDATE qq_inbox SET status='handled',receipt=?1,updated_at_ms=?2 WHERE inbox_id=?3 AND status='pending'",params![serde_json::to_string(&receipt)?,now_ms,inbox_id])?;
    if changed != 1 {
        bail!("QQ 输入状态已改变");
    }
    Ok(())
}

pub(crate) fn redeem_pairing_input(
    tx: &Transaction<'_>,
    inbox_id: &str,
    now_ms: u64,
) -> Result<Binding> {
    let input = inbox(tx, inbox_id)?;
    if input.canonical_input["kind"] != "pairing" || input.route_id.is_some() {
        bail!("QQ 输入不是配对请求");
    }
    if input.status == "handled" {
        return serde_json::from_value(
            input.receipt.context("QQ 配对收据缺失")?["binding"].clone(),
        )
        .context("QQ 配对收据无效");
    }
    let digest = input.canonical_input["code_hash"]
        .as_str()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .context("QQ 配对摘要无效")?;
    let binding = redeem_digest(tx, &input.identity, digest, now_ms)?;
    mark_handled(
        tx,
        inbox_id,
        &input.command_id,
        json!({"binding":binding}),
        now_ms,
    )?;
    Ok(binding)
}

pub(crate) fn route(db: &Connection, route_id: &str) -> Result<ReplyRoute> {
    let body: String = db
        .query_row(
            "SELECT body FROM qq_reply_routes WHERE route_id=?1",
            [route_id],
            |row| row.get(0),
        )
        .optional()?
        .context("QQ 回复来源不存在")?;
    decode(body)
}

fn route_block(db: &Connection, route: &ReplyRoute, now_ms: u64) -> Result<Option<&'static str>> {
    let current = binding(db, &route.principal_id)?;
    if current.status != "active"
        || current.revision != route.binding_revision
        || current.identity != route.identity
    {
        return Ok(Some("revoked"));
    }
    if now_ms >= route.expires_at_ms || now_ms >= current.expires_at_ms {
        return Ok(Some("expired"));
    }
    Ok(None)
}

pub(crate) fn guard_route(db: &Connection, route_id: &str, now_ms: u64) -> Result<ReplyRoute> {
    clock(now_ms)?;
    let route = route(db, route_id)?;
    if route_block(db, &route, now_ms)?.is_some() {
        bail!("QQ 回复来源已过期或绑定已撤销，不能改为主动发送");
    }
    Ok(route)
}

pub(crate) fn guard_input(db: &Connection, inbox_id: &str, now_ms: u64) -> Result<Binding> {
    let input = inbox(db, inbox_id)?;
    if input.status != "pending" {
        bail!("QQ 输入已经处理，不能重复执行");
    }
    let route = guard_route(
        db,
        input
            .route_id
            .as_deref()
            .context("QQ 输入尚未取得本机确认授权")?,
        now_ms,
    )?;
    binding(db, &route.principal_id)
}

pub(crate) fn outbox(db: &Connection, id: &str) -> Result<OutboxRecord> {
    db.query_row("SELECT id,route_id,logical_key,kind,payload,msg_seq,status,attempts,retry_at_ms,claim_nonce,platform_id,reason FROM qq_outbox WHERE id=?1",[id],|row| {
        #[cfg(test)]
        let kind:String=row.get(3)?;
        Ok(OutboxRecord {id:row.get(0)?,route_id:row.get(1)?,#[cfg(test)] kind:if kind=="ack" {OutboxKind::Ack} else {OutboxKind::Completion},payload:row.get(4)?,msg_seq:row.get(5)?,status:row.get(6)?,attempts:row.get(7)?,retry_at_ms:row.get(8)?,claim_nonce:row.get(9)?,#[cfg(test)] platform_id:row.get(10)?, #[cfg(test)] reason:row.get(11)?})
    }).optional()?.context("QQ 投递记录不存在")
}

pub(crate) fn enqueue_outbox(
    tx: &Transaction<'_>,
    route_id: &str,
    logical_key: &str,
    kind: OutboxKind,
    payload: &str,
    now_ms: u64,
) -> Result<OutboxRecord> {
    clock(now_ms)?;
    if logical_key.trim().is_empty()
        || logical_key.len() > 200
        || logical_key.contains('\0')
        || payload.trim().is_empty()
        || payload.len() > MAX_REPLY_BYTES
        || payload.contains('\0')
    {
        bail!("QQ 回复标识或有界内容无效");
    }
    let kind = if kind == OutboxKind::Ack {
        "ack"
    } else {
        "completion"
    };
    let digest = hash(&json!({"kind":kind,"payload":payload}))?;
    let previous: Option<(String, String, String)> = tx
        .query_row(
            "SELECT id,content_hash,route_id FROM qq_outbox WHERE logical_key=?1",
            [logical_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((id, previous, previous_route)) = previous {
        if previous != digest || previous_route != route_id {
            bail!("QQ 回复逻辑标识已对应不同来源或内容，未重定向或重复发送");
        }
        return outbox(tx, &id);
    }
    let count: u64 = tx.query_row("SELECT COUNT(*) FROM qq_outbox", [], |row| row.get(0))?;
    if count >= MAX_OUTBOX_ROWS {
        bail!("QQ 投递记录达到安全上限，未分配新序号或发送消息");
    }
    let route = route(tx, route_id)?;
    let status = route_block(tx, &route, now_ms)?.unwrap_or("pending");
    let seq: u32 = tx.query_row(
        "SELECT next_msg_seq FROM qq_reply_routes WHERE route_id=?1",
        [route_id],
        |row| row.get(0),
    )?;
    if seq > route.max_messages {
        bail!("QQ 原消息被动回复额度已耗尽，未改为主动发送");
    }
    let changed=tx.execute("UPDATE qq_reply_routes SET next_msg_seq=next_msg_seq+1 WHERE route_id=?1 AND next_msg_seq=?2 AND next_msg_seq<=max_messages",params![route_id,seq])?;
    if changed != 1 {
        bail!("QQ 回复序号已改变，未发送");
    }
    let id = new_id();
    tx.execute("INSERT INTO qq_outbox(id,route_id,logical_key,kind,payload,content_hash,msg_seq,status,attempts,retry_at_ms,created_at_ms,updated_at_ms,reason) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?9,?9,?10)",
        params![id,route_id,logical_key,kind,payload,digest,seq,status,now_ms,(status!="pending").then_some(status)])?;
    outbox(tx, &id)
}

pub(crate) fn claim_outbox(tx: &Transaction<'_>, now_ms: u64) -> Result<Option<OutboxClaim>> {
    clock(now_ms)?;
    let mut statement=tx.prepare("SELECT id FROM qq_outbox WHERE status='pending' AND retry_at_ms<=?1 ORDER BY created_at_ms,rowid LIMIT 128")?;
    let ids = statement
        .query_map([now_ms], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    for id in ids {
        let current = outbox(tx, &id)?;
        let route = route(tx, &current.route_id)?;
        if let Some(status) = route_block(tx, &route, now_ms)? {
            tx.execute("UPDATE qq_outbox SET status=?1,reason=?1,updated_at_ms=?2 WHERE id=?3 AND status='pending'",params![status,now_ms,id])?;
            continue;
        }
        if current.attempts >= MAX_ATTEMPTS {
            tx.execute("UPDATE qq_outbox SET status='failed',reason='rate_limit_exhausted',updated_at_ms=?1 WHERE id=?2 AND status='pending'",params![now_ms,id])?;
            continue;
        }
        let changed=tx.execute("UPDATE qq_outbox SET status='sending',attempts=attempts+1,claim_nonce=?1,updated_at_ms=?2 WHERE id=?3 AND status='pending'",params![new_id(),now_ms,id])?;
        if changed == 1 {
            return Ok(Some(OutboxClaim {
                record: outbox(tx, &id)?,
                route,
            }));
        }
    }
    Ok(None)
}

/// Recheck immediately before transport. A subsequent revocation races only
/// with an already-claimed in-flight send, which cannot be recalled.
pub(crate) fn authorize_send(
    tx: &Transaction<'_>,
    id: &str,
    claim_nonce: &str,
    now_ms: u64,
) -> Result<bool> {
    clock(now_ms)?;
    let current = outbox(tx, id)?;
    if current.status != "sending" || current.claim_nonce.as_deref() != Some(claim_nonce) {
        return Ok(false);
    }
    if let Some(status) = route_block(tx, &route(tx, &current.route_id)?, now_ms)? {
        tx.execute("UPDATE qq_outbox SET status=?1,reason=?1,updated_at_ms=?2 WHERE id=?3 AND status='sending' AND claim_nonce=?4",params![status,now_ms,id,claim_nonce])?;
        return Ok(false);
    }
    Ok(true)
}

pub(crate) fn finalize_outbox(
    tx: &Transaction<'_>,
    id: &str,
    claim_nonce: &str,
    outcome: DeliveryOutcome,
    now_ms: u64,
) -> Result<OutboxRecord> {
    clock(now_ms)?;
    let current = outbox(tx, id)?;
    if current.status != "sending" || current.claim_nonce.as_deref() != Some(claim_nonce) {
        bail!("QQ 投递租约已失效，未覆盖其他结果");
    }
    let route = route(tx, &current.route_id)?;
    let (status, reason, platform_id, retry_at) = match outcome {
        DeliveryOutcome::Sent { platform_id } => {
            if !opaque_id(&platform_id) {
                bail!("QQ 发送成功缺少有效平台消息标识，结果仍未知");
            }
            ("sent", None, Some(platform_id), current.retry_at_ms)
        }
        DeliveryOutcome::Unknown => (
            "unknown",
            Some("transport_unknown"),
            None,
            current.retry_at_ms,
        ),
        DeliveryOutcome::Failed { reason } => (
            "failed",
            Some(match reason {
                FailureReason::PlatformRejected => "platform_rejected",
                FailureReason::ConfigurationDisabled => "configuration_disabled",
            }),
            None,
            current.retry_at_ms,
        ),
        DeliveryOutcome::RateLimited { retry_at_ms } => {
            clock(retry_at_ms)?;
            if let Some(blocked) = route_block(tx, &route, now_ms)? {
                (blocked, Some(blocked), None, current.retry_at_ms)
            } else if current.attempts >= MAX_ATTEMPTS {
                (
                    "failed",
                    Some("rate_limit_exhausted"),
                    None,
                    current.retry_at_ms,
                )
            } else if retry_at_ms <= now_ms || retry_at_ms >= route.expires_at_ms {
                (
                    "expired",
                    Some("retry_outside_reply_window"),
                    None,
                    current.retry_at_ms,
                )
            } else {
                ("pending", Some("explicit_429"), None, retry_at_ms)
            }
        }
    };
    let changed=tx.execute("UPDATE qq_outbox SET status=?1,reason=?2,platform_id=?3,retry_at_ms=?4,updated_at_ms=?5 WHERE id=?6 AND status='sending' AND claim_nonce=?7",
        params![status,reason,platform_id,retry_at,now_ms,id,claim_nonce])?;
    if changed != 1 {
        bail!("QQ 投递记录发生竞争，未重新发送");
    }
    outbox(tx, id)
}

/// Call only on startup, before workers run. If a single local finalization
/// fails, retry only recording Unknown with the same nonce (never the network
/// send); an unavailable database leaves sending until this recovery executes.
pub(crate) fn startup_recover(tx: &Transaction<'_>, now_ms: u64) -> Result<()> {
    clock(now_ms)?;
    tx.execute("UPDATE qq_outbox SET status='unknown',reason='interrupted_send',updated_at_ms=?1 WHERE status='sending'",[now_ms])?;
    tx.execute("UPDATE qq_outbox SET status='expired',reason='reply_expired',updated_at_ms=?1 WHERE status='pending' AND route_id IN(SELECT route_id FROM qq_reply_routes WHERE expires_at_ms<=?1)",[now_ms])?;
    tx.execute("UPDATE qq_outbox SET status='revoked',reason='binding_revoked',updated_at_ms=?1 WHERE status='pending' AND route_id IN(SELECT r.route_id FROM qq_reply_routes r JOIN qq_bindings b ON b.principal_id=r.principal_id WHERE b.status!='active' OR b.expires_at_ms<=?1 OR b.revision!=r.binding_revision)",[now_ms])?;
    Ok(())
}

/// Bounded metadata only; no code, verifier, inbox content, reply payload,
/// credential, or in-flight lease is exposed. Owner confirmation uses binding.
pub(crate) fn public_snapshot(db: &Connection, limit: u32) -> Result<Value> {
    if !(1..=200).contains(&limit) {
        bail!("QQ 公开记录查询上限必须为 1–200");
    }
    let mut statement = db.prepare("SELECT body FROM qq_bindings ORDER BY rowid DESC LIMIT ?1")?;
    let bindings = statement
        .query_map([limit], |row| row.get::<_, String>(0))?
        .map(|body| decode::<Value>(body?))
        .collect::<Result<Vec<_>>>()?;
    let mut statement=db.prepare("SELECT id,app_id,expires_at_ms,claimed_binding_id IS NOT NULL FROM qq_pairings ORDER BY rowid DESC LIMIT ?1")?;
    let pairings=statement.query_map([limit],|row|Ok(json!({"id":row.get::<_,String>(0)?,"app_id":row.get::<_,String>(1)?,"expires_at_ms":row.get::<_,u64>(2)?,"claimed":row.get::<_,bool>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let mut statement=db.prepare("SELECT inbox_id,command_id,status,route_id,received_at_ms FROM qq_inbox ORDER BY rowid DESC LIMIT ?1")?;
    let inbox=statement.query_map([limit],|row|Ok(json!({"inbox_id":row.get::<_,String>(0)?,"command_id":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?,"route_id":row.get::<_,Option<String>>(3)?,"received_at_ms":row.get::<_,u64>(4)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let mut statement=db.prepare("SELECT id,route_id,logical_key,status,msg_seq,attempts,platform_id,reason FROM qq_outbox ORDER BY rowid DESC LIMIT ?1")?;
    let outbox=statement.query_map([limit],|row|Ok(json!({"id":row.get::<_,String>(0)?,"route_id":row.get::<_,String>(1)?,"logical_key":row.get::<_,String>(2)?,"status":row.get::<_,String>(3)?,"msg_seq":row.get::<_,u32>(4)?,"attempts":row.get::<_,u32>(5)?,"platform_id":row.get::<_,Option<String>>(6)?,"reason":row.get::<_,Option<String>>(7)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let ignored = db
        .query_row(
            "SELECT count,updated_at_ms,clock_fault_ms FROM qq_ignored_inputs WHERE singleton=1",
            [],
            |r| Ok(json!({"count":r.get::<_,u64>(0)?,"updated_at_ms":r.get::<_,u64>(1)?,"clock_fault_ms":r.get::<_,u64>(2)?})),
        )
        .optional()?
        .unwrap_or(json!({"count":0,"updated_at_ms":null,"clock_fault_ms":0}));
    Ok(
        json!({"bindings":bindings,"pairings":pairings,"inbox":inbox,"outbox":outbox,"ignored_inputs":ignored}),
    )
}

#[cfg(test)]
#[path = "persistence_tests.rs"]
mod tests;
