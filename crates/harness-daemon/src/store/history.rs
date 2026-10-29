//! Bounded owner-only history projections. No caller first materializes Store's
//! legacy full snapshot; SQL selects bounded records/message windows directly.
//! A daemon epoch invalidates rowid/array cursors after restart or restore.
use super::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{error::Error, fmt};

pub(crate) const PAGE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const RECORD_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const DETAIL_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const SNAPSHOT_BYTES: usize = 12 * 1024 * 1024;
const PAGE_COUNT: usize = 80;
const ENVELOPE_RESERVE: usize = 32 * 1024;
const MESSAGE_WINDOW: usize = 24;
const DISPLAY_ID_BYTES: usize = 128 * 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryError {
    Reset,
    InvalidQuery,
    NotFound,
    RecordTooLarge,
    InvalidRecord,
    Capacity,
}
impl HistoryError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Reset => "history_reset",
            Self::InvalidQuery => "invalid_history_query",
            Self::NotFound => "not_found",
            Self::RecordTooLarge => "record_too_large",
            Self::InvalidRecord => "history_record_invalid",
            Self::Capacity => "history_capacity",
        }
    }
}
impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Reset => "历史游标已失效，请重新加载第一页",
            Self::InvalidQuery => "历史查询参数无效",
            Self::NotFound => "历史记录不存在",
            Self::RecordTooLarge => "记录超过此接口的大小限制，未截断权威内容",
            Self::InvalidRecord => "历史记录结构无效，未返回私有原始数据",
            Self::Capacity => "有界历史元数据超过限制，未返回不完整的权威快照",
        })
    }
}
impl Error for HistoryError {}

pub(crate) fn safe_history_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn valid_epoch(epoch: &str) -> Result<()> {
    if !uuid::Uuid::parse_str(epoch).is_ok_and(|id| id.hyphenated().to_string() == epoch) {
        return Err(HistoryError::InvalidQuery.into());
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sessions,
    Tasks,
    Plans,
    PlanChanges,
    Deliveries,
    Requests,
    Events,
    Contexts,
    Selections,
}
impl Kind {
    fn page(value: &str) -> Result<Self> {
        Ok(match value {
            "sessions" => Self::Sessions,
            "tasks" => Self::Tasks,
            "plans" => Self::Plans,
            "plan_changes" => Self::PlanChanges,
            "deliveries" => Self::Deliveries,
            "execution_requests" => Self::Requests,
            _ => return Err(HistoryError::InvalidQuery.into()),
        })
    }
    fn table(self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::Tasks => "tasks",
            Self::Plans => "plans",
            Self::PlanChanges => "plan_changes",
            Self::Deliveries => "deliveries",
            Self::Requests => "execution_requests",
            Self::Events => "events",
            Self::Contexts => "session_contexts",
            Self::Selections => "selections",
        }
    }
    fn public_name(self) -> &'static str {
        if self == Self::Contexts {
            "context_states"
        } else {
            self.table()
        }
    }
    fn identifier(self) -> &'static str {
        if self == Self::Selections {
            "json_extract(body,'$.session_id')"
        } else {
            "id"
        }
    }
    fn scope(self) -> &'static str {
        if self == Self::Sessions {
            "id"
        } else {
            "json_extract(body,'$.session_id')"
        }
    }
    fn projection(self) -> &'static str {
        match self {
            Self::Sessions => "json_object('id',json_extract(body,'$.id'),'title',json_extract(body,'$.title'),'project_path',json_extract(body,'$.project_path'),'created_at',json_extract(body,'$.created_at'),'updated_at',json_extract(body,'$.updated_at'),'messages',json('[]'))",
            Self::Contexts => "json_object('session_id',json_extract(body,'$.session_id'),'executor',json_extract(body,'$.executor'),'state',json_extract(body,'$.state'),'reason',json_extract(body,'$.reason'),'covered_messages',json_extract(body,'$.summary.covered_messages'),'source_task_id',CASE WHEN json_extract(body,'$.executor')='native' THEN json_extract(body,'$.summary_source_task_id') ELSE json_extract(body,'$.task_id') END,'updated_at',json_extract(body,'$.updated_at'))",
            _ => "body",
        }
    }
    fn public(self, raw: &str, rowid: i64) -> Result<Value> {
        let value = match self {
            Self::Sessions => typed::<harness_core::Session>(raw),
            Self::Tasks => {
                let mut value: Value =
                    serde_json::from_str(raw).map_err(|_| HistoryError::InvalidRecord)?;
                if !value["supplements"].is_array() {
                    value["supplements"] = json!([]);
                }
                let task: harness_core::TaskRun =
                    serde_json::from_value(value).map_err(|_| HistoryError::InvalidRecord)?;
                Ok(serde_json::to_value(task)?)
            }
            Self::Plans => typed::<harness_core::Plan>(raw),
            Self::PlanChanges => typed::<harness_core::PlanChange>(raw),
            Self::Deliveries => typed::<harness_core::Delivery>(raw),
            Self::Requests => typed::<harness_core::ExecutionRequest>(raw),
            Self::Events => {
                let mut event: RunEvent =
                    serde_json::from_str(raw).map_err(|_| HistoryError::InvalidRecord)?;
                event.sequence = u64::try_from(rowid).map_err(|_| HistoryError::InvalidRecord)?;
                Ok(serde_json::to_value(event)?)
            }
            Self::Contexts => typed::<harness_core::ContextState>(raw),
            Self::Selections => typed::<harness_core::SessionSelection>(raw),
        }?;
        public_ids(&value)?;
        Ok(value)
    }
    fn active(self) -> &'static str {
        match self {
            Self::Tasks => "json_extract(body,'$.status') IN ('queued','running','waiting_input')",
            Self::Plans => {
                "json_extract(body,'$.status') NOT IN ('completed','failed','cancelled','rejected')"
            }
            Self::Requests => "json_extract(body,'$.status')='pending'",
            Self::PlanChanges => {
                "json_extract(body,'$.status') IN ('pending','queued','running','draft')"
            }
            Self::Deliveries => "json_extract(body,'$.status') IN ('pending','sending')",
            Self::Selections => "1",
            _ => "0",
        }
    }
}
fn display_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(HistoryError::InvalidRecord.into());
    }
    if id.len() > DISPLAY_ID_BYTES || id.chars().count() > 128 {
        return Err(HistoryError::RecordTooLarge.into());
    }
    Ok(())
}
fn public_ids(value: &Value) -> Result<()> {
    for key in [
        "id",
        "session_id",
        "task_id",
        "plan_id",
        "node_id",
        "parent_task_id",
    ] {
        if let Some(id) = value.get(key).filter(|id| !id.is_null()) {
            display_id(id.as_str().ok_or(HistoryError::InvalidRecord)?)?;
        }
    }
    for key in ["steps", "nodes"] {
        for item in value[key].as_array().into_iter().flatten() {
            display_id(item["id"].as_str().ok_or(HistoryError::InvalidRecord)?)?;
        }
    }
    Ok(())
}
fn typed<T: DeserializeOwned + Serialize>(raw: &str) -> Result<Value> {
    let typed: T = serde_json::from_str(raw).map_err(|_| HistoryError::InvalidRecord)?;
    Ok(serde_json::to_value(typed)?)
}
fn encoded_size(value: &Value) -> Result<usize> {
    Ok(serde_json::to_vec(value)?.len())
}
fn enforce(value: Value, limit: usize) -> Result<Value> {
    if encoded_size(&value)? > limit {
        return Err(HistoryError::Capacity.into());
    }
    Ok(value)
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    epoch: String,
    kind: String,
    scope: Option<String>,
    before: i64,
    upper: i64,
}
impl Cursor {
    fn encode(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self)?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
    fn decode(raw: &str, epoch: &str, kind: &str, scope: Option<&str>) -> Result<Self> {
        if raw.is_empty()
            || raw.len() > 2048
            || !raw.len().is_multiple_of(2)
            || !raw.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(HistoryError::Reset.into());
        }
        let bytes = raw
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |c: u8| (c as char).to_digit(16).unwrap() as u8;
                (digit(pair[0]) << 4) | digit(pair[1])
            })
            .collect::<Vec<_>>();
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| HistoryError::Reset)?;
        if cursor.epoch != epoch
            || cursor.kind != kind
            || cursor.scope.as_deref() != scope
            || cursor.upper < 0
            || cursor.before < 0
            || cursor.before > cursor.upper.saturating_add(1)
        {
            return Err(HistoryError::Reset.into());
        }
        Ok(cursor)
    }
}
struct Row {
    number: i64,
    active: bool,
    pinned: bool,
}
fn scan(
    db: &Connection,
    kind: Kind,
    scope: Option<&str>,
    before: i64,
    upper: i64,
    order: &str,
    limit: usize,
) -> Result<Vec<Row>> {
    // Only row numbers and flags are collected here. Record bodies are
    // fetched one at a time, never 80 simultaneous 8 MiB allocations.
    let pinned = pin_expression(kind);
    let sql = format!("SELECT rowid,CASE WHEN {} THEN 1 ELSE 0 END,CASE WHEN {pinned} THEN 1 ELSE 0 END FROM {} WHERE rowid<?1 AND rowid<=?2 AND (?3 IS NULL OR {}=?3) ORDER BY {order} LIMIT ?4", kind.active(),kind.table(),kind.scope());
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map(params![before, upper, scope, limit as i64], |r| {
        Ok(Row {
            number: r.get(0)?,
            active: r.get::<_, i64>(1)? != 0,
            pinned: r.get::<_, i64>(2)? != 0,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}
fn pin_expression(kind: Kind) -> String {
    match kind {
        Kind::Sessions => "EXISTS(SELECT 1 FROM selections p WHERE json_extract(p.body,'$.session_id')=sessions.id) OR EXISTS(SELECT 1 FROM tasks t WHERE json_extract(t.body,'$.session_id')=sessions.id AND json_extract(t.body,'$.status') IN ('queued','running','waiting_input'))".into(),
        Kind::Tasks | Kind::Plans => format!("EXISTS(SELECT 1 FROM selections p WHERE json_extract(p.body,'$.session_id')=json_extract({}.body,'$.session_id'))",kind.table()),
        _ => "0".into(),
    }
}
fn owner_selection_rank(kind: Kind) -> String {
    let rank = |body: &str| {
        format!("CASE WHEN json_extract({body},'$.source.channel_id')='desktop' AND json_extract({body},'$.source.actor_id')='owner' THEN 0 WHEN json_extract({body},'$.source.channel_id')='cli' AND json_extract({body},'$.source.actor_id') IN ('owner','local') THEN 1 WHEN json_extract({body},'$.source.channel_id')='simulator' AND json_extract({body},'$.source.actor_id')='owner' THEN 2 ELSE 3 END")
    };
    match kind {
        Kind::Selections => rank("selections.body"),
        Kind::Sessions => format!("COALESCE((SELECT MIN({}) FROM selections p WHERE json_extract(p.body,'$.session_id')=sessions.id),3)",rank("p.body")),
        _ => "(3 + 0)".into(),
    }
}
fn row_id(db: &Connection, kind: Kind, rowid: i64) -> Result<String> {
    let column = kind.identifier();
    let id:Option<String>=db.query_row(&format!("SELECT CASE WHEN length(CAST(({column}) AS BLOB))<=?2 THEN ({column}) END FROM {} WHERE rowid=?1",kind.table()),params![rowid,DISPLAY_ID_BYTES as i64],|r|r.get(0))?;
    let id = id.ok_or(HistoryError::RecordTooLarge)?;
    display_id(&id)?;
    Ok(id)
}
fn raw_row(db: &Connection, kind: Kind, rowid: i64) -> Result<Option<String>> {
    let projection = kind.projection();
    Ok(db.query_row(&format!("SELECT CASE WHEN length(CAST(({projection}) AS BLOB))<=?2 THEN ({projection}) END FROM {} WHERE rowid=?1",kind.table()),params![rowid,RECORD_BYTES as i64],|r|r.get(0))?)
}
fn count(db: &Connection, kind: Kind, scope: Option<&str>, upper: i64) -> Result<u64> {
    Ok(db.query_row(
        &format!(
            "SELECT COUNT(*) FROM {} WHERE rowid<=?1 AND (?2 IS NULL OR {}=?2)",
            kind.table(),
            kind.scope()
        ),
        params![upper, scope],
        |r| r.get(0),
    )?)
}
fn upper(db: &Connection, kind: Kind) -> Result<i64> {
    Ok(db.query_row(
        &format!("SELECT COALESCE(MAX(rowid),0) FROM {}", kind.table()),
        [],
        |r| r.get(0),
    )?)
}
fn unavailable(id: &str) -> Value {
    json!({"id":id,"reason":"record_too_large"})
}
fn push_missing(items: &mut Vec<Value>, id: &str, budget: &mut usize) -> Result<()> {
    let item = unavailable(id);
    let cost = encoded_size(&item)? + 1;
    if cost > *budget {
        return Err(HistoryError::Capacity.into());
    }
    *budget -= cost;
    items.push(item);
    Ok(())
}

impl Store {
    pub(crate) fn history_page(
        &self,
        epoch: &str,
        kind: &str,
        session_id: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Value> {
        valid_epoch(epoch)?;
        if session_id.is_some_and(|id| !safe_history_id(id)) {
            return Err(HistoryError::InvalidQuery.into());
        }
        let kind = Kind::page(kind)?;
        let cursor = match cursor {
            Some(raw) => Cursor::decode(raw, epoch, kind.table(), session_id)?,
            None => {
                let upper = upper(&self.db, kind)?;
                Cursor {
                    epoch: epoch.into(),
                    kind: kind.table().into(),
                    scope: session_id.map(str::to_owned),
                    before: upper.checked_add(1).ok_or(HistoryError::Reset)?,
                    upper,
                }
            }
        };
        let total = count(&self.db, kind, session_id, cursor.upper)?;
        let candidates = scan(
            &self.db,
            kind,
            session_id,
            cursor.before,
            cursor.upper,
            "rowid DESC",
            PAGE_COUNT + 1,
        )?;
        let mut items = Vec::new();
        let mut missing = Vec::new();
        let mut used = 0;
        let mut before = cursor.before;
        let mut windows = serde_json::Map::new();
        for row in candidates.iter().take(PAGE_COUNT) {
            let id = row_id(&self.db, kind, row.number)?;
            let raw = raw_row(&self.db, kind, row.number)?;
            let value = raw
                .as_deref()
                .map(|raw| kind.public(raw, row.number))
                .transpose()?;
            let size = value
                .as_ref()
                .map(encoded_size)
                .transpose()?
                .unwrap_or(RECORD_BYTES + 1);
            if size > PAGE_BYTES - ENVELOPE_RESERVE {
                let absent = unavailable(&id);
                let cost = encoded_size(&absent)? + 1;
                if cost > PAGE_BYTES - ENVELOPE_RESERVE {
                    return Err(HistoryError::RecordTooLarge.into());
                }
                if used + cost > PAGE_BYTES - ENVELOPE_RESERVE {
                    break;
                }
                used += cost;
                missing.push(absent);
                before = row.number;
                continue;
            }
            let window = if kind == Kind::Sessions {
                let total = self.message_count(&id)?;
                Some(
                    json!({"offset":total,"total":total,"returned":0,"has_more_before":total>0,"unavailable":[]}),
                )
            } else {
                None
            };
            let extra = window.as_ref().map(encoded_size).transpose()?.unwrap_or(0)
                + if window.is_some() {
                    encoded_size(&json!(id))? + 2
                } else {
                    0
                };
            if size + extra + 1 > PAGE_BYTES - ENVELOPE_RESERVE {
                return Err(HistoryError::RecordTooLarge.into());
            }
            if used + size + extra + 1 > PAGE_BYTES - ENVELOPE_RESERVE {
                break;
            }
            used += size + extra + 1;
            before = row.number;
            if let Some(window) = window {
                windows.insert(id.clone(), window);
            }
            items.push(value.unwrap());
        }
        let has_more: bool=self.db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {} WHERE rowid<?1 AND rowid<=?2 AND (?3 IS NULL OR {}=?3))",kind.table(),kind.scope()),params![before,cursor.upper,session_id],|r|r.get(0))?;
        let next = if has_more {
            let mut next = cursor.clone();
            next.before = before;
            Some(next.encode()?)
        } else {
            None
        };
        let mut result = json!({"items":items,"total":total,"returned":items.len(),"has_more":has_more,"next_cursor":next,"current_cursor":cursor.encode()?,"anchor":cursor.epoch,"event_cursor":self.event_cursor()?,"unavailable":missing});
        if kind == Kind::Sessions {
            result["session_messages"] = Value::Object(windows);
        }
        enforce(result, PAGE_BYTES)
    }
    fn message_count(&self, session_id: &str) -> Result<u64> {
        self.db
            .query_row(
                "SELECT json_array_length(body,'$.messages') FROM sessions WHERE id=?1",
                [session_id],
                |r| r.get::<_, u64>(0),
            )
            .optional()?
            .ok_or_else(|| HistoryError::NotFound.into())
    }
    pub(crate) fn history_messages(
        &self,
        epoch: &str,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<Value> {
        valid_epoch(epoch)?;
        if !safe_history_id(session_id) {
            return Err(HistoryError::InvalidQuery.into());
        }
        let total = self.message_count(session_id)?;
        let position = match cursor {
            Some(raw) => Cursor::decode(raw, epoch, "messages", Some(session_id))?,
            None => {
                let upper = i64::try_from(total).map_err(|_| HistoryError::Reset)?;
                Cursor {
                    epoch: epoch.into(),
                    kind: "messages".into(),
                    scope: Some(session_id.into()),
                    before: upper,
                    upper,
                }
            }
        };
        if total < position.upper as u64 || position.before > position.upper {
            return Err(HistoryError::Reset.into());
        }
        let (items, missing, before) = message_window(
            &self.db,
            session_id,
            position.before,
            PAGE_COUNT,
            PAGE_BYTES - ENVELOPE_RESERVE,
        )?;
        let has_more = before > 0;
        let next = if has_more {
            let mut next = position.clone();
            next.before = before;
            Some(next.encode()?)
        } else {
            None
        };
        enforce(
            json!({"items":items,"total":position.upper,"returned":items.len(),"has_more":has_more,"next_cursor":next,"current_cursor":position.encode()?,"anchor":position.epoch,"event_cursor":self.event_cursor()?,"offset":before,"unavailable":missing}),
            PAGE_BYTES,
        )
    }
    pub(crate) fn bounded_task_detail(&self, task_id: &str) -> Result<Value> {
        if !safe_history_id(task_id) {
            return Err(HistoryError::InvalidQuery.into());
        }
        let raw:Option<Option<String>>=self.db.query_row("SELECT CASE WHEN length(CAST(body AS BLOB))<=?2 THEN body END FROM tasks WHERE id=?1",params![task_id,RECORD_BYTES as i64],|r|r.get(0)).optional()?;
        let raw = raw
            .ok_or(HistoryError::NotFound)?
            .ok_or(HistoryError::RecordTooLarge)?;
        let task = Kind::Tasks.public(&raw, 0)?;
        if task["id"] != task_id {
            return Err(HistoryError::InvalidRecord.into());
        }
        let event_cursor = self.event_cursor()?;
        let mut result = json!({"task":task,"event_cursor":event_cursor,"events":[],"execution_requests":[],"history":{"events":{"total":0,"returned":0,"has_more":false},"execution_requests":{"total":0,"returned":0,"has_more":false}}});
        let base = encoded_size(&result)?;
        if base + ENVELOPE_RESERVE > DETAIL_BYTES {
            return Err(HistoryError::RecordTooLarge.into());
        }
        let mut remaining = DETAIL_BYTES - base - ENVELOPE_RESERVE;
        let mut metadata_budget = ENVELOPE_RESERVE - 4096;
        // Pending decisions precede event history; any omitted decision remains
        // explicitly counted. No result/evidence/stream field is shortened.
        for kind in [Kind::Requests, Kind::Events] {
            let total: u64 = self.db.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE json_extract(body,'$.task_id')=?1",
                    kind.table()
                ),
                [task_id],
                |r| r.get(0),
            )?;
            let order = if kind == Kind::Requests {
                "CASE WHEN json_extract(body,'$.status')='pending' THEN 0 ELSE 1 END,rowid DESC"
            } else {
                "rowid DESC"
            };
            let sql=format!("SELECT rowid,CASE WHEN length(CAST(id AS BLOB))<=512 THEN id END,CASE WHEN length(CAST(body AS BLOB))<=?2 THEN body END FROM {} WHERE json_extract(body,'$.task_id')=?1 ORDER BY {order} LIMIT ?3",kind.table());
            let mut statement = self.db.prepare(&sql)?;
            let rows = statement.query_map(
                params![task_id, RECORD_BYTES as i64, PAGE_COUNT as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )?;
            let mut items = Vec::new();
            let mut missing = Vec::new();
            for row in rows {
                let (sequence, id, raw) = row?;
                let id = id.ok_or(HistoryError::RecordTooLarge)?;
                display_id(&id)?;
                let value = raw
                    .as_deref()
                    .map(|r| kind.public(r, sequence))
                    .transpose()?;
                let cost = value
                    .as_ref()
                    .map(encoded_size)
                    .transpose()?
                    .unwrap_or(usize::MAX);
                if cost.saturating_add(1) > remaining {
                    push_missing(&mut missing, &id, &mut metadata_budget)?;
                    continue;
                }
                remaining -= cost + 1;
                items.push(value.unwrap());
            }
            if kind == Kind::Events {
                items.reverse();
            }
            let returned = items.len();
            let pending = if kind == Kind::Requests {
                let total_pending: u64 = self.db.query_row("SELECT COUNT(*) FROM execution_requests WHERE json_extract(body,'$.task_id')=?1 AND json_extract(body,'$.status')='pending'", [task_id], |r| r.get(0))?;
                total_pending.saturating_sub(
                    items
                        .iter()
                        .filter(|item| item["status"] == "pending")
                        .count() as u64,
                )
            } else {
                0
            };
            result[kind.public_name()] = json!(items);
            result["history"][kind.public_name()] = json!({"total":total,"returned":returned,"has_more":total>returned as u64,"omitted":total.saturating_sub(returned as u64),"pending":pending,"unavailable":missing});
        }
        enforce(result, DETAIL_BYTES)
    }
}

fn message_window(
    db: &Connection,
    session: &str,
    before: i64,
    count: usize,
    budget: usize,
) -> Result<(Vec<Value>, Vec<Value>, i64)> {
    // SQLite expands only requested message values into Rust. The enclosing
    // session JSON is never deserialized by this path, even for very old chats.
    let mut statement=db.prepare("SELECT CAST(j.key AS INTEGER),CASE WHEN length(CAST(json_extract(j.value,'$.id') AS BLOB))<=512 THEN json_extract(j.value,'$.id') END,CASE WHEN length(CAST(j.value AS BLOB))<=?4 THEN j.value END FROM sessions s,json_each(s.body,'$.messages') j WHERE s.id=?1 AND CAST(j.key AS INTEGER)<?2 ORDER BY CAST(j.key AS INTEGER) DESC LIMIT ?3")?;
    let rows = statement.query_map(
        params![session, before, count as i64, RECORD_BYTES as i64],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        },
    )?;
    let mut items = Vec::new();
    let mut missing = Vec::new();
    let mut used = 0;
    let mut next = before;
    for row in rows {
        let (index, id, raw) = row?;
        let id = id.ok_or(HistoryError::RecordTooLarge)?;
        display_id(&id)?;
        let mut value = raw
            .as_deref()
            .map(typed::<harness_core::Message>)
            .transpose()?;
        if let Some(value) = &mut value {
            public_ids(value)?;
            value["history_offset"] = json!(index);
        }
        let size = value
            .as_ref()
            .map(encoded_size)
            .transpose()?
            .unwrap_or(usize::MAX);
        if size.saturating_add(1) > budget {
            let mut absent = unavailable(&id);
            absent["offset"] = json!(index);
            let cost = encoded_size(&absent)? + 1;
            if cost > PAGE_BYTES - ENVELOPE_RESERVE {
                return Err(HistoryError::RecordTooLarge.into());
            }
            if used + cost > budget {
                break;
            }
            used += cost;
            missing.push(absent);
            next = index;
            continue;
        }
        if used + size + 1 > budget {
            break;
        }
        used += size + 1;
        next = index;
        items.push(value.unwrap());
    }
    items.reverse();
    missing.reverse();
    Ok((items, missing, next))
}

fn static_capabilities() -> Value {
    let shell = std::env::var("HARNESS_ALLOW_COMMANDS").as_deref() == Ok("1");
    json!([
        {"id":"main","name":"主智能体","kind":"agent","description":"当前项目直接执行；显式远程子任务编排","enabled":true},
        {"id":"native","name":"原生编程智能体","kind":"executor","description":"兼容模型与有界工具循环","enabled":true},
        {"id":"workspace","name":"项目文件","kind":"tool","description":"项目根目录内的文件操作","enabled":true},
        {"id":"shell","name":"项目命令","kind":"tool","description":"可信本机命令；需显式启用，无系统级沙箱","enabled":shell},
        {"id":"callback","name":"本地回调","kind":"tool","description":"持久化回调固定投递到任务来源","enabled":true},
        {"id":"codex","name":"Codex 执行器","kind":"executor","description":"本机 Codex CLI，需安装与登录","enabled":true},
        {"id":"roles","name":"子智能体角色","kind":"agent","description":"按固定角色快照执行计划节点","enabled":true},
        {"id":"mcp","name":"MCP 连接","kind":"mcp","description":"显式本机 stdio 工具；需权限，无沙箱","enabled":true},
        {"id":"skills","name":"本地技能","kind":"skill","description":"显式注册的固定正文，不自动加载脚本","enabled":true}
    ])
}
impl Store {
    pub(crate) fn legacy_bounded_snapshot(&self) -> Result<Value> {
        // Old clients must never mistake a window for the full collection.
        // Refuse large histories before Rust decodes them. This conservative
        // preflight is a compatibility limit, not a lossy projection.
        let mut bytes = 0u64;
        for table in [
            "sessions",
            "tasks",
            "plans",
            "plan_changes",
            "deliveries",
            "execution_requests",
            "session_contexts",
            "selections",
        ] {
            let size: u64 = self.db.query_row(
                &format!("SELECT COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM {table}"),
                [],
                |r| r.get(0),
            )?;
            bytes = bytes
                .checked_add(size)
                .ok_or(HistoryError::RecordTooLarge)?;
            if bytes > SNAPSHOT_BYTES as u64 {
                return Err(HistoryError::RecordTooLarge.into());
            }
        }
        let value = self.snapshot()?;
        if encoded_size(&value)? > 16 * 1024 * 1024 {
            return Err(HistoryError::RecordTooLarge.into());
        }
        Ok(value)
    }

    pub(crate) fn windowed_snapshot(&self, epoch: &str) -> Result<Value> {
        valid_epoch(epoch)?;
        let mut result = json!({"event_cursor":self.event_cursor()?,"capabilities":static_capabilities(),"managed_capabilities":self.capabilities_registry()?,"named_providers":self.named_providers()?,"history":{"version":1,"anchor":epoch,"collections":{},"session_messages":{}}});
        let base = encoded_size(&result)?;
        if base + 256 * 1024 > SNAPSHOT_BYTES {
            return Err(HistoryError::Capacity.into());
        }
        let mut remaining = SNAPSHOT_BYTES - base - 256 * 1024;
        let mut metadata_budget = 240 * 1024;
        // Decisions and persisted selections must never be pushed out by old
        // task results. Collections are bounded independently as well as by the
        // final wire-size cap; omitted active/pinned counts stay visible.
        for kind in [
            Kind::Requests,
            Kind::Selections,
            Kind::Sessions,
            Kind::Tasks,
            Kind::Plans,
            Kind::Contexts,
            Kind::PlanChanges,
            Kind::Deliveries,
            Kind::Events,
        ] {
            let upper = upper(&self.db, kind)?;
            let total = count(&self.db, kind, None, upper)?;
            let active: u64 = self.db.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE {}",
                    kind.table(),
                    kind.active()
                ),
                [],
                |r| r.get(0),
            )?;
            let pinned = pin_expression(kind);
            let pinned_total: u64 = self.db.query_row(
                &format!("SELECT COUNT(*) FROM {} WHERE {pinned}", kind.table()),
                [],
                |r| r.get(0),
            )?;
            let order = format!(
                "{},CASE WHEN {} THEN 0 WHEN {pinned} THEN 1 ELSE 2 END,rowid DESC",
                owner_selection_rank(kind),
                kind.active()
            );
            let candidates = scan(
                &self.db,
                kind,
                None,
                upper.saturating_add(1),
                upper,
                &order,
                PAGE_COUNT,
            )?;
            let mut items = Vec::new();
            let mut missing = Vec::new();
            let mut included_active = 0u64;
            let mut included_pinned = 0u64;
            // Reserve room for the other arrays, so a single large selected chat
            // cannot hide every active task and approval.
            let collection_budget = remaining.min(match kind {
                Kind::Sessions => 2 * 1024 * 1024,
                Kind::Selections => 256 * 1024,
                Kind::Requests => 2 * 1024 * 1024,
                Kind::Tasks => 6 * 1024 * 1024,
                _ => 2 * 1024 * 1024,
            });
            let mut spent = 0;
            for row in candidates {
                let id = row_id(&self.db, kind, row.number)?;
                let Some(raw) = raw_row(&self.db, kind, row.number)? else {
                    push_missing(&mut missing, &id, &mut metadata_budget)?;
                    continue;
                };
                let mut value = kind.public(&raw, row.number)?;
                let mut message_meta = None;
                if kind == Kind::Sessions {
                    let total = self.message_count(&id)?;
                    let header = encoded_size(&value)?;
                    if header + 2 > collection_budget.saturating_sub(spent) {
                        push_missing(&mut missing, &id, &mut metadata_budget)?;
                        continue;
                    }
                    let budget = collection_budget
                        .saturating_sub(spent + header + 2)
                        .min(64 * 1024);
                    let (messages, unavailable, offset) =
                        message_window(&self.db, &id, total as i64, MESSAGE_WINDOW, budget)?;
                    message_meta = Some(
                        json!({"offset":offset,"total":total,"returned":messages.len(),"has_more_before":offset>0,"unavailable":unavailable}),
                    );
                    value["messages"] = json!(messages);
                }
                let cost = encoded_size(&value)?
                    + 1
                    + message_meta
                        .as_ref()
                        .map(encoded_size)
                        .transpose()?
                        .unwrap_or(0)
                    + if message_meta.is_some() {
                        encoded_size(&json!(id))? + 2
                    } else {
                        0
                    };
                if cost > collection_budget.saturating_sub(spent) {
                    push_missing(&mut missing, &id, &mut metadata_budget)?;
                    continue;
                }
                spent += cost;
                if let Some(meta) = message_meta {
                    result["history"]["session_messages"][&id] = meta;
                }
                if row.active {
                    included_active += 1;
                }
                if row.pinned {
                    included_pinned += 1;
                }
                items.push(value);
            }
            remaining = remaining.saturating_sub(spent);
            if kind == Kind::Events {
                items.reverse();
            }
            let returned = items.len();
            result[kind.public_name()] = json!(items);
            result["history"]["collections"][kind.public_name()] = json!({"total":total,"returned":returned,"has_more":total>returned as u64,"pending":active.saturating_sub(included_active),"pinned":pinned_total,"pinned_pending":pinned_total.saturating_sub(included_pinned),"omitted":total.saturating_sub(returned as u64),"unavailable":missing});
        }
        enforce(result, SNAPSHOT_BYTES)
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
