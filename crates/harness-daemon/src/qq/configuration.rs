//! QQ configuration and connection fences. Caller owns the transaction and must
//! roll it back on any error. Vault and transport callbacks are internal trusted
//! capabilities, never user-supplied evidence of authority.
use super::persistence::{self, ReplyPolicy, ReplyRoute};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) const TOKEN_ENDPOINT: &str = "https://api.bot.qq.com/app/getAppAccessToken";

// Deliberately no Debug/Serialize: credential reference is private daemon data.
#[derive(Clone)]
pub(crate) struct Config {
    pub id: String,
    pub revision: u64,
    pub app_id: String,
    pub enabled: bool,
    pub reply_policy: ReplyPolicy,
    credential_ref: Option<String>,
}
impl Config {
    pub(crate) fn credential_ref(&self) -> Option<&str> {
        self.credential_ref.as_deref()
    }
    pub(crate) fn public_view(&self) -> Value {
        json!({"id":self.id,"revision":self.revision,"app_id":self.app_id,
            "enabled":self.enabled,"mode":"official","reply_policy":self.reply_policy,
            "credential_state":if self.credential_ref.is_some(){"configured"}else{"missing"}})
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigPatch {
    pub app_id: String,
    pub enabled: bool,
    pub reply_policy: ReplyPolicy,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    pub config_id: String,
    pub config_revision: u64,
    pub session_id: String,
    pub sequence: u64,
    pub bot_user_id: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionState {
    Disconnected,
    Connecting,
    Ready,
    Backoff,
    Disabled,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Startup,
    ConfigurationChanged,
    CredentialUnavailable,
    Network,
    PlatformRejected,
    Storage,
    Stopped,
}

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS qq_configuration(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL UNIQUE,
        revision INTEGER NOT NULL CHECK(revision>=0), app_id TEXT NOT NULL,
        enabled INTEGER NOT NULL CHECK(enabled IN(0,1)), mode TEXT NOT NULL CHECK(mode='official'),
        reply_policy TEXT NOT NULL, credential_ref TEXT);
        CREATE TABLE IF NOT EXISTS qq_checkpoint(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        config_id TEXT NOT NULL, config_revision INTEGER NOT NULL, body TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS qq_connection_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        config_id TEXT NOT NULL, config_revision INTEGER NOT NULL, state TEXT NOT NULL, reason TEXT, updated_at_ms INTEGER);")?;
    // Development-schema upgrade: legacy rows have unknown update time (NULL),
    // never a fabricated migration/read timestamp.
    let columns = {
        let mut stmt = db.prepare("PRAGMA table_info(qq_connection_state)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if !columns.iter().any(|name| name == "updated_at_ms") {
        db.execute(
            "ALTER TABLE qq_connection_state ADD COLUMN updated_at_ms INTEGER",
            [],
        )?;
    }
    Ok(())
}
fn app_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'_' || v == b'-')
    {
        bail!("QQ AppID 格式无效");
    }
    Ok(())
}
fn bounded_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'_' || v == b'-')
    {
        bail!("QQ 连接标识格式无效");
    }
    Ok(())
}
fn safe_number(value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        bail!("QQ 序列或版本超出范围");
    }
    Ok(())
}
pub(crate) fn credential_binding_digest(app: &str) -> Result<String> {
    app_id(app)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"app_id":app,"token_endpoint":TOKEN_ENDPOINT,"mode":"official"})
        )?)
    ))
}
pub(crate) fn current(db: &Connection) -> Result<Option<Config>> {
    let row=db.query_row("SELECT id,revision,app_id,enabled,reply_policy,credential_ref FROM qq_configuration WHERE singleton=1",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,bool>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?))).optional()?;
    row.map(|(id, revision, app_id, enabled, policy, credential_ref)| {
        Ok(Config {
            id,
            revision,
            app_id,
            enabled,
            reply_policy: serde_json::from_str(&policy)?,
            credential_ref,
        })
    })
    .transpose()
}
pub(crate) fn load(db: &Connection, id: &str, revision: u64) -> Result<Config> {
    safe_number(revision)?;
    let config = current(db)?.context("QQ 尚未配置")?;
    if config.id != id || config.revision != revision {
        bail!("QQ 配置版本已改变");
    }
    Ok(config)
}
pub(crate) fn create(tx: &Transaction<'_>, app: &str) -> Result<Config> {
    app_id(app)?;
    if current(tx)?.is_some() {
        bail!("QQ 配置已存在");
    }
    let c = Config {
        id: uuid::Uuid::new_v4().to_string(),
        revision: 0,
        app_id: app.into(),
        enabled: false,
        reply_policy: ReplyPolicy::default(),
        credential_ref: None,
    };
    tx.execute(
        "INSERT INTO qq_configuration VALUES(1,?1,0,?2,0,'official',?3,NULL)",
        params![c.id, c.app_id, serde_json::to_string(&c.reply_policy)?],
    )?;
    set_connection_state(
        tx,
        &c.id,
        0,
        ConnectionState::Disabled,
        Some(Reason::ConfigurationChanged),
    )?;
    Ok(c)
}
fn revoke_app(tx: &Transaction<'_>, app: &str, now: u64) -> Result<()> {
    safe_number(now)?;
    let entries = {
        let mut stmt=tx.prepare("SELECT principal_id,revision FROM qq_bindings WHERE app_id=?1 AND status IN('active','pending')")?;
        let rows = stmt.query_map([app], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, revision) in entries {
        persistence::revoke_binding(tx, &id, revision, now)?;
    }
    tx.execute(
        "DELETE FROM qq_pairings WHERE app_id=?1 AND claimed_binding_id IS NULL",
        [app],
    )?;
    Ok(())
}
fn save(tx: &Transaction<'_>, mut c: Config) -> Result<Config> {
    let previous = c.revision;
    c.revision = c.revision.checked_add(1).context("QQ 配置版本耗尽")?;
    safe_number(c.revision)?;
    let count=tx.execute("UPDATE qq_configuration SET revision=?1,app_id=?2,enabled=?3,reply_policy=?4,credential_ref=?5 WHERE id=?6 AND revision=?7",params![c.revision,c.app_id,c.enabled,serde_json::to_string(&c.reply_policy)?,c.credential_ref,c.id,previous])?;
    if count != 1 {
        bail!("QQ 配置版本已改变");
    }
    tx.execute("DELETE FROM qq_checkpoint", [])?;
    set_connection_state(
        tx,
        &c.id,
        c.revision,
        if c.enabled {
            ConnectionState::Disconnected
        } else {
            ConnectionState::Disabled
        },
        Some(Reason::ConfigurationChanged),
    )?;
    Ok(c)
}
/// The readiness callback must resolve the scoped vault binding, or an explicitly
/// process-injected fixture. It is never selected via persisted/user endpoints.
pub(crate) fn update(
    tx: &Transaction<'_>,
    id: &str,
    revision: u64,
    patch: ConfigPatch,
    now: u64,
    ready: impl FnOnce(&Config) -> Result<bool>,
) -> Result<Config> {
    let mut c = load(tx, id, revision)?;
    app_id(&patch.app_id)?;
    patch.reply_policy.validate()?;
    let changed = c.app_id != patch.app_id;
    if changed && c.credential_ref.is_some() {
        bail!("更换 QQ AppID 前必须先断开凭据");
    }
    if !patch.enabled || changed {
        revoke_app(tx, &c.app_id, now)?;
    }
    c.app_id = patch.app_id;
    c.enabled = patch.enabled;
    c.reply_policy = patch.reply_policy;
    if c.enabled && !ready(&c)? {
        bail!("QQ 凭据尚未就绪");
    }
    save(tx, c)
}
pub(crate) fn attach_credential(
    tx: &Transaction<'_>,
    id: &str,
    revision: u64,
    reference: &str,
    validate: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<Config> {
    let mut c = load(tx, id, revision)?;
    if reference.is_empty() || reference.len() > 512 || reference.chars().any(char::is_control) {
        bail!("QQ 凭据引用格式无效");
    }
    validate(reference, &credential_binding_digest(&c.app_id)?)?;
    c.credential_ref = Some(reference.into());
    save(tx, c)
}
pub(crate) fn disconnect_credential(
    tx: &Transaction<'_>,
    id: &str,
    revision: u64,
    now: u64,
) -> Result<Config> {
    let mut c = load(tx, id, revision)?;
    revoke_app(tx, &c.app_id, now)?;
    c.enabled = false;
    c.credential_ref = None;
    save(tx, c)
}
fn enabled(db: &Connection, id: &str, revision: u64) -> Result<Config> {
    let c = load(db, id, revision)?;
    if !c.enabled {
        bail!("QQ 配置已停用");
    }
    Ok(c)
}
pub(crate) fn load_checkpoint(
    db: &Connection,
    id: &str,
    revision: u64,
) -> Result<Option<Checkpoint>> {
    load(db, id, revision)?;
    let value: Option<String> = db
        .query_row(
            "SELECT body FROM qq_checkpoint WHERE config_id=?1 AND config_revision=?2",
            params![id, revision],
            |r| r.get(0),
        )
        .optional()?;
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
/// Only call for a freshly authenticated, transport-validated READY dispatch.
/// A different session requires runtime to durably clear the old checkpoint first.
pub(crate) fn ready_checkpoint(tx: &Transaction<'_>, point: &Checkpoint) -> Result<()> {
    enabled(tx, &point.config_id, point.config_revision)?;
    bounded_id(&point.session_id)?;
    bounded_id(&point.bot_user_id)?;
    safe_number(point.sequence)?;
    if let Some(old) = load_checkpoint(tx, &point.config_id, point.config_revision)? {
        if old.session_id != point.session_id {
            bail!("QQ 新会话必须先持久清除旧检查点");
        }
        if point.sequence < old.sequence || point.bot_user_id != old.bot_user_id {
            bail!("QQ 同会话检查点不可倒退或改变机器人身份");
        }
    }
    tx.execute("INSERT INTO qq_checkpoint VALUES(1,?1,?2,?3) ON CONFLICT(singleton) DO UPDATE SET config_id=excluded.config_id,config_revision=excluded.config_revision,body=excluded.body",params![point.config_id,point.config_revision,serde_json::to_string(point)?])?;
    set_connection_state(
        tx,
        &point.config_id,
        point.config_revision,
        ConnectionState::Ready,
        None,
    )
}
pub(crate) fn advance_checkpoint(
    tx: &Transaction<'_>,
    id: &str,
    revision: u64,
    session: &str,
    sequence: u64,
) -> Result<()> {
    enabled(tx, id, revision)?;
    safe_number(sequence)?;
    let mut p = load_checkpoint(tx, id, revision)?.context("QQ 尚无可恢复会话")?;
    if p.session_id != session || sequence < p.sequence {
        bail!("QQ 会话或序列不匹配");
    }
    p.sequence = sequence;
    tx.execute(
        "UPDATE qq_checkpoint SET body=?1 WHERE config_id=?2 AND config_revision=?3",
        params![serde_json::to_string(&p)?, id, revision],
    )?;
    Ok(())
}
pub(crate) fn clear_checkpoint(tx: &Transaction<'_>, id: &str, revision: u64) -> Result<()> {
    load(tx, id, revision)?;
    tx.execute(
        "DELETE FROM qq_checkpoint WHERE config_id=?1 AND config_revision=?2",
        params![id, revision],
    )?;
    set_connection_state(
        tx,
        id,
        revision,
        ConnectionState::Disconnected,
        Some(Reason::Stopped),
    )
}
pub(crate) fn set_connection_state(
    tx: &Transaction<'_>,
    id: &str,
    revision: u64,
    state: ConnectionState,
    reason: Option<Reason>,
) -> Result<()> {
    let c = load(tx, id, revision)?;
    if matches!(
        state,
        ConnectionState::Ready | ConnectionState::Connecting | ConnectionState::Backoff
    ) && !c.enabled
    {
        bail!("停用的 QQ 配置不能连接");
    }
    if state == ConnectionState::Ready && load_checkpoint(tx, id, revision)?.is_none() {
        bail!("QQ Ready 必须有已验证的检查点");
    }
    tx.execute("INSERT INTO qq_connection_state(singleton,config_id,config_revision,state,reason,updated_at_ms) VALUES(1,?1,?2,?3,?4,?5) ON CONFLICT(singleton) DO UPDATE SET config_id=excluded.config_id,config_revision=excluded.config_revision,state=excluded.state,reason=excluded.reason,updated_at_ms=excluded.updated_at_ms",params![id,revision,serde_json::to_string(&state)?,reason.map(|v|serde_json::to_string(&v)).transpose()?,chrono::Utc::now().timestamp_millis()])?;
    Ok(())
}
pub(crate) fn connection_view(db: &Connection) -> Result<Value> {
    type StateRow = (String, u64, String, Option<String>, Option<i64>);
    let row:Option<StateRow>=db.query_row("SELECT config_id,config_revision,state,reason,updated_at_ms FROM qq_connection_state WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    match row {
        Some((id, revision, state, reason, updated_at_ms)) => Ok(
            json!({"config_id":id,"config_revision":revision,"updated_at_ms":updated_at_ms,"state":serde_json::from_str::<ConnectionState>(&state)?,"reason":reason.map(|r|serde_json::from_str::<Reason>(&r)).transpose()?}),
        ),
        None => Ok(json!({"state":"disconnected","updated_at_ms":null})),
    }
}
pub(crate) fn startup_recover(tx: &Transaction<'_>) -> Result<()> {
    if let Some(c) = current(tx)? {
        set_connection_state(
            tx,
            &c.id,
            c.revision,
            if c.enabled {
                ConnectionState::Disconnected
            } else {
                ConnectionState::Disabled
            },
            Some(Reason::Startup),
        )?;
    }
    Ok(())
}
/// Long-running tasks retain authority after the passive reply window, but not
/// after binding revocation, expiry, identity/config changes, or adapter disable.
pub(crate) fn live_task_route(db: &Connection, route_id: &str, now: u64) -> Result<ReplyRoute> {
    safe_number(now)?;
    let c = current(db)?.context("QQ 尚未配置")?;
    let route = persistence::route(db, route_id)?;
    let binding = persistence::binding(db, &route.principal_id)?;
    if !c.enabled
        || c.app_id != route.identity.app_id
        || binding.status != "active"
        || binding.revision != route.binding_revision
        || binding.identity != route.identity
        || now >= binding.expires_at_ms
    {
        bail!("QQ 任务来源权限已失效");
    }
    Ok(route)
}
#[cfg(test)]
#[path = "configuration_tests.rs"]
mod tests;
