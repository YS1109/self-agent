//! Trusted transaction-only gateway boundary. Never forwards owner envelopes.
//! This module is wired only by an explicit schema-ledger integration.
use super::*;
use crate::capabilities::vault::CredentialRef;
use crate::gateway::{
    admin, configuration as config, credentials, persistence, policy, resources, wire::*,
};
use harness_core::{CredentialOperationView, TaskControls};
const MAX_RECORD: usize = 2 * 1024 * 1024;
fn millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    persistence::migrate(tx)?;
    config::migrate(tx)?;
    credentials::migrate(tx)?;
    admin::migrate(tx)?;
    resources::migrate(tx)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS gateway_connection(singleton INTEGER PRIMARY KEY CHECK(singleton=1),config_id TEXT NOT NULL,revision INTEGER NOT NULL,state TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);")?;
    Ok(())
}
pub(super) fn recover(tx: &Transaction<'_>) -> Result<()> {
    credentials::recover(tx)?;
    tx.execute(
        "UPDATE gateway_connection SET state='disconnected',updated_at_ms=?1",
        [millis()],
    )?;
    Ok(())
}
pub(super) fn restore(tx: &Transaction<'_>) -> Result<()> {
    config::invalidate_restored(tx, millis())?;
    tx.execute("DELETE FROM gateway_connection", [])?;
    Ok(())
}
fn validate_reference(owner: &str, digest: &str, reference: &str) -> Result<()> {
    CredentialRef::parse(
        reference,
        &crate::gateway::credential_binding(owner, digest)?,
    )?;
    Ok(())
}
fn enabled(db: &Connection, id: &str, ch: &persistence::TrustedChannel) -> Result<config::Config> {
    let c = config::load(db, id, ch.config_revision)?;
    if !c.enabled || c.device_id != ch.device_id {
        bail!("网关连接代际已失效")
    }
    Ok(c)
}
fn bounded_record(db: &Connection, table: &str, id: &str) -> Result<Value> {
    policy::identifier(id)?;
    let raw:Option<String>=db.query_row(&format!("SELECT CASE WHEN length(CAST(body AS BLOB))<=?2 THEN body ELSE NULL END FROM {table} WHERE id=?1"),params![id,MAX_RECORD],|r|r.get(0)).context("远程资源不存在")?;
    let value: Value = serde_json::from_str(&raw.context("远程资源超过安全读取上限")?)?;
    if value["id"] != id {
        bail!("远程资源索引不一致")
    }
    Ok(value)
}
struct SqlContext<'a, 'c> {
    tx: &'a Transaction<'c>,
    config_id: &'a str,
    channel: &'a persistence::TrustedChannel,
}
impl policy::ResourceContext for SqlContext<'_, '_> {
    fn channel_current(&self) -> bool {
        enabled(self.tx, self.config_id, self.channel).is_ok()
    }
    fn session_exists(&self, id: &str) -> Result<bool> {
        policy::identifier(id)?;
        Ok(self.tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?)
    }
    fn task_session(&self, id: &str) -> Result<String> {
        let task = bounded_record(self.tx, "tasks", id)?;
        Ok(text(&task, "session_id")?.into())
    }
    fn task_controls(&self, id: &str) -> Result<TaskControls> {
        Ok(serde_json::from_value(
            bounded_record(self.tx, "tasks", id)?["controls"].clone(),
        )?)
    }
    fn question_task(&self, id: &str, rev: u32) -> Result<(String, bool)> {
        let q = bounded_record(self.tx, "execution_requests", id)?;
        let task = bounded_record(self.tx, "tasks", text(&q, "task_id")?)?;
        Ok((
            text(&q, "task_id")?.into(),
            q["kind"] == "question"
                && q["status"] == "pending"
                && q["revision"] == rev
                && q["expires_at_ms"].as_u64().is_some_and(|v| v > millis())
                && task["status"] == "waiting_input"
                && task["waiting_request_id"] == id,
        ))
    }
    fn artifact_task(&self, id: &str) -> Result<String> {
        resources::task(self.tx, id)
    }
}
fn constrained(
    settings: &Settings,
    grant: &GatewayGrant,
    validate_executor: bool,
) -> Result<Settings> {
    let mut result = settings.clone();
    result.execution_controls = execution::resolve_controls(
        &settings.execution_controls,
        None,
        Some(&grant.controls),
        "native",
    )?;
    if validate_executor && grant.permissions.run {
        execution::resolve_controls(
            &result.execution_controls,
            None,
            None,
            &settings.main_executor,
        )?;
    }
    Ok(result)
}
fn safe_text(settings: &Settings, root: &str, input: &str, limit: usize) -> String {
    let text = settings.redact(input);
    let text = if root.is_empty() {
        text
    } else {
        text.replace(root, "[PROJECT]")
    };
    let mut text: String = text
        .chars()
        .filter(|c| !matches!(*c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'|'\0'))
        .collect();
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1
        }
        text.truncate(end)
    }
    text
}
fn project_root(tx: &Transaction<'_>, session: &str) -> Result<String> {
    let root: Option<String> = tx.query_row(
        "SELECT substr(json_extract(body,'$.project_path'),1,8192) FROM sessions WHERE id=?1",
        [session],
        |r| r.get(0),
    )?;
    Ok(root.unwrap_or_default())
}
fn task_view(task: &Value, results: bool, settings: &Settings, root: &str) -> Value {
    let status = task["status"]
        .as_str()
        .filter(|s| {
            matches!(
                *s,
                "queued"
                    | "pending"
                    | "running"
                    | "waiting_input"
                    | "completed"
                    | "failed"
                    | "cancelled"
            )
        })
        .unwrap_or("unknown");
    let cleanup = task["execution_cleanup"]
        .as_str()
        .filter(|s| matches!(*s, "pending" | "settled" | "unknown"));
    let mut v = json!({"id":task["id"],"session_id":task["session_id"],"title":"远程任务","status":status,"execution_cleanup":cleanup});
    if results {
        if status == "waiting_input" {
            if let Some(id) = task["waiting_request_id"]
                .as_str()
                .filter(|id| policy::identifier(id).is_ok())
            {
                v["waiting_request_id"] = json!(id);
            }
        }
        v["title"] = json!(safe_text(
            settings,
            root,
            task["title"].as_str().unwrap_or("任务"),
            256
        ));
        for field in ["result", "stream_output", "error"] {
            if let Some(s) = task[field].as_str() {
                v[field] = json!(safe_text(settings, root, s, 1024));
            }
        }
    }
    if task["source"]["channel_id"] == "gateway" {
        v["source_principal_id"] = task["source"]["actor_id"].clone();
        v["source_binding_revision"] = task["source"]["conversation_id"]
            .as_str()
            .and_then(|s| s.rsplit(':').next())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Value::from)
            .unwrap_or(Value::Null);
    }
    v
}
fn cursor(tx: &Transaction<'_>) -> Result<u64> {
    Ok(tx.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='events'),0)",
        [],
        |r| r.get(0),
    )?)
}
fn task_ids(tx: &Transaction<'_>, session: &str) -> Result<Vec<String>> {
    let mut q=tx.prepare("SELECT id FROM tasks WHERE json_extract(body,'$.session_id')=?1 ORDER BY rowid DESC LIMIT 21")?;
    let rows = q
        .query_map([session], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
fn session_view(
    tx: &Transaction<'_>,
    id: &str,
    grant: &GatewayGrant,
    settings: &Settings,
) -> Result<Value> {
    let root = project_root(tx, id)?;
    let results = grant.permissions.read_results;
    let title: Option<String> = tx.query_row(
        "SELECT CASE WHEN length(CAST(json_extract(body,'$.title') AS BLOB))<=8192 THEN json_extract(body,'$.title') ELSE NULL END FROM sessions WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    let ids = task_ids(tx, id)?;
    let mut tasks = Vec::new();
    for task in ids.iter().take(20) {
        tasks.push(task_view(
            &bounded_record(tx, "tasks", task)?,
            results,
            settings,
            &root,
        ));
    }
    let mut messages = Vec::new();
    if results {
        let mut q=tx.prepare("SELECT json_extract(value,'$.id'),json_extract(value,'$.role'),CASE WHEN length(CAST(json_extract(value,'$.content') AS BLOB))<=262144 THEN json_extract(value,'$.content') ELSE NULL END,json_extract(value,'$.task_id') FROM sessions,json_each(sessions.body,'$.messages') WHERE sessions.id=?1 ORDER BY CAST(key AS INTEGER) DESC LIMIT 20")?;
        let rows = q.query_map([id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (mid, role, content, task) = row?;
            messages.push(json!({"id":mid,"role":role,"content":safe_text(settings,&root,content.as_deref().unwrap_or(""),2048),"task_id":task}));
        }
        messages.reverse();
    }
    let mut questions = Vec::new();
    if results && grant.permissions.answer {
        let mut q=tx.prepare("SELECT execution_requests.id FROM execution_requests JOIN tasks ON execution_requests.task_id=tasks.id WHERE json_extract(tasks.body,'$.session_id')=?1 AND json_extract(tasks.body,'$.status')='waiting_input' AND json_extract(tasks.body,'$.waiting_request_id')=execution_requests.id AND json_extract(execution_requests.body,'$.kind')='question' AND json_extract(execution_requests.body,'$.status')='pending' ORDER BY execution_requests.rowid DESC LIMIT 16")?;
        let rows = q
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for qid in rows {
            let q = bounded_record(tx, "execution_requests", &qid)?;
            if q["expires_at_ms"].as_u64().is_none_or(|t| t <= millis()) {
                continue;
            }
            questions.push(json!({"id":q["id"],"task_id":q["task_id"],"session_id":id,"kind":"question","revision":q["revision"],"status":"pending","prompt":safe_text(settings,&root,q["prompt"].as_str().unwrap_or(""),2048),"expires_at_ms":q["expires_at_ms"]}));
        }
    }
    Ok(
        json!({"session":{"id":id,"title":if results{safe_text(settings,&root,title.as_deref().unwrap_or("会话"),256)}else{"已授权会话".into()},"messages":messages},"tasks":tasks,"questions":questions,"cursor":cursor(tx)?,"truncated":ids.len()>20||messages.len()==20}),
    )
}
fn changes_view(
    tx: &Transaction<'_>,
    task: &str,
    settings: &Settings,
    root: &str,
) -> Result<Value> {
    let raw:Option<String>=tx.query_row("SELECT CASE WHEN length(CAST(body AS BLOB))<=524288 THEN body ELSE NULL END FROM task_changes WHERE id=?1",[task],|r|r.get(0)).optional()?.flatten();
    let Some(raw) = raw else {
        return Ok(json!({"state":"unavailable","files":[]}));
    };
    let report: Value = serde_json::from_str(&raw)?;
    if report["state"] == "unavailable" || !report["files"].is_array() {
        return Ok(json!({"state":"unavailable","files":[]}));
    }
    let mut files = Vec::new();
    for item in report["files"].as_array().into_iter().flatten().take(16) {
        let path = item["path"].as_str().unwrap_or("");
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path.split('/').any(|p| p == "..")
        {
            continue;
        }
        files.push(json!({"path":safe_text(settings,root,path,512),"status":safe_text(settings,root,item["status"].as_str().unwrap_or("observed"),32),"diff":safe_text(settings,root,item["diff"].as_str().unwrap_or(""),4096),"binary":item["binary"].as_bool().unwrap_or(false)}));
    }
    Ok(
        json!({"state":"observed","scope":"observed_execution_interval","files":files,"truncated":true}),
    )
}
fn mint_artifacts(
    tx: &Transaction<'_>,
    task: &Value,
    session: &str,
    settings: &Settings,
    root: &str,
) -> Result<()> {
    if !matches!(
        task["status"].as_str(),
        Some("completed" | "failed" | "cancelled")
    ) {
        return Ok(());
    }
    let id = text(task, "id")?;
    if let Some(result) = task["result"].as_str().filter(|s| !s.is_empty()) {
        resources::mint(
            tx,
            id,
            session,
            "result",
            "任务结果.txt",
            &safe_text(settings, root, result, MAX_RECORD),
        )?;
    }
    let evidence:Vec<_>=task["evidence"].as_array().into_iter().flatten().take(8).map(|e|json!({"command":safe_text(settings,root,e["command"].as_str().unwrap_or(""),2048),"output":safe_text(settings,root,e["output"].as_str().unwrap_or(""),4096),"exit_code":e["exit_code"].as_i64()})).collect();
    if !evidence.is_empty() {
        resources::mint(
            tx,
            id,
            session,
            "evidence",
            "已保存命令证据.txt",
            &serde_json::to_string_pretty(&evidence)?,
        )?;
    }
    let changes = changes_view(tx, id, settings, root)?;
    if changes["state"] == "observed" {
        resources::mint(
            tx,
            id,
            session,
            "changes",
            "观察到的改动.txt",
            &serde_json::to_string_pretty(&changes)?,
        )?;
    }
    Ok(())
}
fn read_rpc(
    tx: &Transaction<'_>,
    request: &GatewayRequest,
    binding: &persistence::Binding,
    session: Option<&str>,
    settings: &Settings,
) -> Result<GatewayResult> {
    let grant = binding.grant.as_ref().context("远程授权缺失")?;
    let data = match &request.rpc {
        GatewayRpc::Sessions => {
            let mut sessions = Vec::new();
            for id in &grant.session_ids {
                let title:Option<Option<String>>=tx.query_row("SELECT CASE WHEN length(CAST(json_extract(body,'$.title') AS BLOB))<=8192 THEN json_extract(body,'$.title') ELSE NULL END FROM sessions WHERE id=?1",[id],|r|r.get(0)).optional()?;
                if let Some(title) = title {
                    sessions.push(json!({"id":id,"title":if grant.permissions.read_results{safe_text(settings,&project_root(tx,id)?,title.as_deref().unwrap_or("会话"),256)}else{"已授权会话".into()}}));
                }
            }
            json!({"sessions":sessions})
        }
        GatewayRpc::Session { session_id } => session_view(tx, session_id, grant, settings)?,
        GatewayRpc::Task { task_id } => {
            json!({"task":task_view(&bounded_record(tx,"tasks",task_id)?,grant.permissions.read_results,settings,&project_root(tx,session.context("会话缺失")?)?)})
        }
        GatewayRpc::Events { session_id, after } => {
            let latest = cursor(tx)?;
            let oldest: Option<u64> =
                tx.query_row("SELECT MIN(sequence) FROM events", [], |r| r.get(0))?;
            if *after > latest
                || oldest.is_some_and(|first| *after < first.saturating_sub(1))
                || (oldest.is_none() && *after < latest)
            {
                return Ok(GatewayResult::error(GatewayCode::CursorExpired));
            }
            let mut q=tx.prepare("SELECT sequence,id,json_extract(body,'$.task_id') FROM events WHERE sequence>?1 AND json_extract(body,'$.session_id')=?2 ORDER BY sequence LIMIT 128")?;
            let rows = q.query_map(params![after, session_id], |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            let mut events = Vec::new();
            let mut last = *after;
            for row in rows {
                let (seq, id, task) = row?;
                last = seq;
                events.push(json!({"sequence":seq,"id":id,"session_id":session_id,"task_id":task,"kind":"state_changed"}));
            }
            json!({"cursor":if events.len()<128{latest}else{last},"events":events})
        }
        GatewayRpc::Changes { task_id } => changes_view(
            tx,
            task_id,
            settings,
            &project_root(tx, session.context("会话缺失")?)?,
        )?,
        GatewayRpc::Artifacts { task_id } => {
            let session = session.context("会话缺失")?;
            let task = bounded_record(tx, "tasks", task_id)?;
            mint_artifacts(tx, &task, session, settings, &project_root(tx, session)?)?;
            json!({"artifacts":resources::list(tx,task_id,session)?})
        }
        GatewayRpc::Artifact { resource_id } => {
            let session = session.context("会话缺失")?;
            let task = resources::task(tx, resource_id)?;
            let mut artifact = resources::read(tx, resource_id, &task, session)?;
            artifact.content = safe_text(
                settings,
                &project_root(tx, session)?,
                &artifact.content,
                64 * 1024,
            );
            artifact.summary.bytes = artifact.content.len();
            json!({"artifact":artifact})
        }
        _ => bail!("网关读取类型不支持"),
    };
    Ok(GatewayResult::ok(data))
}
fn execute(
    tx: &Transaction<'_>,
    id: &str,
    channel: &persistence::TrustedChannel,
    request: &GatewayRequest,
    settings: &Settings,
) -> Result<(GatewayResult, Option<RunEvent>)> {
    let c = enabled(tx, id, channel)?;
    policy::identifier(&request.request_id)?;
    policy::identifier(&request.principal_id)?;
    let context = SqlContext {
        tx,
        config_id: id,
        channel,
    };
    let now = millis();
    match &request.rpc {
        GatewayRpc::PairClaim { code, label } => {
            if request.binding_revision != 0 {
                bail!("配对修订必须为零")
            }
            persistence::claim_pairing(
                tx,
                channel,
                &request.principal_id,
                code,
                label,
                now,
                &context,
            )?;
            Ok((
                GatewayResult::ok(persistence::pair_status(
                    tx,
                    channel,
                    &request.principal_id,
                    now,
                )?),
                None,
            ))
        }
        GatewayRpc::PairStatus => Ok((
            GatewayResult::ok(persistence::pair_status(
                tx,
                channel,
                &request.principal_id,
                now,
            )?),
            None,
        )),
        GatewayRpc::Receipt { command_id } => Ok((
            GatewayResult::ok(persistence::read_receipt(
                tx,
                channel,
                &request.principal_id,
                request.binding_revision,
                command_id,
                now,
                &context,
            )?),
            None,
        )),
        GatewayRpc::Command {
            command_id,
            session_id,
            command,
        } => {
            let permit = match persistence::begin_command(tx, channel, request, now, &context)? {
                persistence::CommandStart::Existing(receipt) => {
                    return Ok((GatewayResult::ok(receipt), None))
                }
                persistence::CommandStart::Apply(permit) => permit,
            };
            let settings = constrained(
                settings,
                permit.binding().grant.as_ref().context("授权缺失")?,
                false,
            )?;
            let command = match command {
                GatewayCommand::Run { content } => {
                    json!({"type":"run_agent","session_id":session_id,"content":content})
                }
                GatewayCommand::Cancel { task_id } => {
                    json!({"type":"cancel_task","task_id":task_id})
                }
                GatewayCommand::Answer {
                    request_id,
                    expected_revision,
                    answer,
                } => {
                    json!({"type":"answer_request","request_id":request_id,"expected_revision":expected_revision,"answer":answer})
                }
                GatewayCommand::Steer { task_id, content } => {
                    json!({"type":"steer_task","task_id":task_id,"content":content})
                }
                GatewayCommand::Continue { task_id, content } => {
                    json!({"type":"continue_task","task_id":task_id,"content":content})
                }
            };
            let source = json!({"channel_id":"gateway","actor_id":request.principal_id,"conversation_id":format!("{}:{}:{}",c.id,c.revision,permit.binding().revision)});
            let envelope = json!({"command_id":command_id,"source":source,"command":command});
            let (receipt, event) = command_transaction(
                tx,
                &envelope,
                &settings,
                &format!("gateway:{}", request.principal_id),
                None,
            )?;
            let mut safe = json!({"command_id":command_id,"status":receipt["status"]});
            for key in ["task_id", "session_id", "supplement_id"] {
                if let Some(value) = receipt.get(key) {
                    safe[key] = value.clone();
                }
            }
            persistence::complete_command(tx, channel, &permit, safe.clone(), millis(), &context)?;
            Ok((GatewayResult::ok(safe), event))
        }
        _ => {
            let (binding, session) = persistence::authorize(tx, channel, request, now, &context)?;
            if matches!(
                request.rpc,
                GatewayRpc::Artifacts { .. } | GatewayRpc::Artifact { .. }
            ) && !binding.grant.as_ref().unwrap().permissions.read_results
            {
                return Ok((GatewayResult::error(GatewayCode::Forbidden), None));
            }
            Ok((
                read_rpc(tx, request, &binding, session.as_deref(), settings)?,
                None,
            ))
        }
    }
}
impl Store {
    pub(crate) fn gateway_config(&self) -> Result<Option<config::Config>> {
        config::current(&self.db)
    }
    pub(crate) fn gateway_snapshot(&self, fixture: bool) -> Result<Value> {
        let tx = self.db.unchecked_transaction()?;
        let c = config::current(&tx)?;
        let bindings = if let Some(c) = &c {
            persistence::bindings(&tx, &c.device_id, 128)?
        } else {
            vec![]
        };
        let configuration = c
            .as_ref()
            .map(|c| {
                let mut v = c.public_view();
                v["mode"] = json!(if fixture { "fixture" } else { "self_hosted" });
                v
            })
            .unwrap_or(Value::Null);
        let connection = if let Some(c) = &c {
            tx.query_row("SELECT state,updated_at_ms FROM gateway_connection WHERE config_id=?1 AND revision=?2",params![c.id,c.revision],|r|Ok(json!({"state":r.get::<_,String>(0)?,"updated_at_ms":r.get::<_,u64>(1)?}))).optional()?.unwrap_or(json!({"state":if c.enabled{"disconnected"}else{"disabled"},"updated_at_ms":null}))
        } else {
            json!({"state":"disabled","updated_at_ms":null})
        };
        let operations = credentials::operations(&tx)?;
        let binding_total = bindings.len();
        let operation_total = operations.len();
        let mut shown_bindings = Vec::new();
        let mut size = 0;
        for b in bindings {
            let value = serde_json::to_value(b)?;
            let bytes = serde_json::to_vec(&value)?.len();
            if size + bytes > 128 * 1024 {
                break;
            }
            size += bytes;
            shown_bindings.push(value);
        }
        let mut shown_operations = Vec::new();
        size = 0;
        for op in operations {
            let value = serde_json::to_value(op)?;
            let bytes = serde_json::to_vec(&value)?.len();
            if size + bytes > 64 * 1024 {
                break;
            }
            size += bytes;
            shown_operations.push(value);
        }
        let truncated = shown_bindings.len() < binding_total
            || binding_total == 128
            || shown_operations.len() < operation_total;
        let result = json!({"configuration":configuration,"bindings":shown_bindings,"connection":connection,"credential_operations":shown_operations,"fixture":fixture,"truncated":truncated});
        tx.commit()?;
        Ok(result)
    }
    pub(crate) fn gateway_admin_command(
        &mut self,
        envelope: &Value,
        settings: &Settings,
        fixture: bool,
    ) -> Result<Value> {
        let tx = self.db.transaction()?;
        let current = config::current(&tx)?;
        let id = current.as_ref().map(|c| c.id.as_str()).unwrap_or("");
        let channel = persistence::TrustedChannel {
            device_id: current
                .as_ref()
                .map(|c| c.device_id.clone())
                .unwrap_or_default(),
            config_revision: current.as_ref().map_or(0, |c| c.revision),
        };
        let context = SqlContext {
            tx: &tx,
            config_id: id,
            channel: &channel,
        };
        let result = admin::apply(
            &tx,
            envelope,
            millis(),
            |c| {
                if fixture {
                    return Ok(true);
                }
                let Some(reference) = c.credential_ref() else {
                    return Ok(false);
                };
                validate_reference(&c.id, &c.scope_digest, reference)?;
                Ok(true)
            },
            |grant| {
                constrained(settings, grant, true)?;
                Ok(())
            },
            &context,
        );
        match result {
            Ok(receipt) => {
                reconcile(&tx)?;
                tx.commit()?;
                Ok(receipt)
            }
            Err(_) => {
                tx.rollback()?;
                let tx = self.db.transaction()?;
                let receipt = admin::reject(&tx, envelope)?;
                tx.commit()?;
                Ok(receipt)
            }
        }
    }
    pub(crate) fn gateway_admin_receipt(&self, id: &str) -> Result<Option<Value>> {
        admin::receipt(&self.db, id)
    }
    pub(crate) fn gateway_connection(&mut self, id: &str, rev: u64, state: &str) -> Result<()> {
        if !matches!(
            state,
            "connecting" | "ready" | "disconnected" | "credential_unavailable" | "disabled"
        ) {
            bail!("网关连接状态无效")
        }
        let tx = self.db.transaction()?;
        let c = config::load(&tx, id, rev)?;
        if state == "ready" && !c.enabled {
            bail!("停用网关不能就绪")
        }
        tx.execute("INSERT INTO gateway_connection VALUES(1,?1,?2,?3,?4) ON CONFLICT(singleton) DO UPDATE SET config_id=excluded.config_id,revision=excluded.revision,state=excluded.state,updated_at_ms=excluded.updated_at_ms",params![id,rev,state,millis()])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn gateway_handle(
        &mut self,
        id: &str,
        channel: &persistence::TrustedChannel,
        request: &GatewayRequest,
        settings: &Settings,
    ) -> Result<(GatewayResult, Option<RunEvent>)> {
        let tx = self.db.transaction()?;
        let result = execute(&tx, id, channel, request, settings);
        match result {
            Ok((result, event)) => {
                if serde_json::to_vec(&result)?.len() > GATEWAY_MAX_FRAME_BYTES - 512 {
                    tx.rollback()?;
                    return Ok((GatewayResult::error(GatewayCode::Busy), None));
                }
                match tx.commit() {
                    Ok(()) => Ok((result, event)),
                    Err(_) => Ok((GatewayResult::error(GatewayCode::OutcomeUnknown), None)),
                }
            }
            Err(_) => {
                tx.rollback()?;
                Ok((GatewayResult::error(GatewayCode::Forbidden), None))
            }
        }
    }
    pub(crate) fn gateway_revoked(
        &mut self,
        id: &str,
        channel: &persistence::TrustedChannel,
        principals: &[(String, u64)],
    ) -> Result<Vec<String>> {
        if principals.len() > 128 {
            bail!("网关身份检查超过上限")
        }
        let tx = self.db.transaction()?;
        let context = SqlContext {
            tx: &tx,
            config_id: id,
            channel,
        };
        let mut revoked = Vec::new();
        for (principal, revision) in principals {
            policy::identifier(principal)?;
            if persistence::active_binding(&tx, channel, principal, *revision, millis(), &context)
                .is_err()
            {
                revoked.push(principal.clone())
            }
        }
        reconcile(&tx)?;
        tx.commit()?;
        Ok(revoked)
    }
    pub(crate) fn gateway_existing_set(
        &self,
        op: &str,
        id: &str,
        rev: u64,
    ) -> Result<Option<Value>> {
        credentials::existing_set(&self.db, op, id, rev)
    }
    pub(crate) fn gateway_begin_set(
        &mut self,
        op: &str,
        id: &str,
        rev: u64,
        reference: &CredentialRef,
    ) -> Result<Value> {
        let tx = self.db.transaction()?;
        let receipt =
            credentials::begin_set(&tx, op, id, rev, reference.as_str(), validate_reference)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn gateway_begin_delete(&mut self, op: &str, id: &str, rev: u64) -> Result<Value> {
        let tx = self.db.transaction()?;
        let receipt = credentials::begin_delete(&tx, op, id, rev, millis(), validate_reference)?;
        reconcile(&tx)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn gateway_begin_cleanup(&mut self, op: &str, target: &str) -> Result<Value> {
        let tx = self.db.transaction()?;
        let receipt = credentials::begin_cleanup(&tx, op, target)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn gateway_credential_operation(
        &self,
        op: &str,
    ) -> Result<Option<CredentialOperationView>> {
        policy::identifier(op)?;
        if !self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM gateway_credential_operations WHERE id=?1)",
            [op],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(None);
        }
        Ok(Some(credentials::operation(&self.db, op)?))
    }
    pub(crate) fn gateway_claim_credential(
        &mut self,
        op: &str,
    ) -> Result<Option<credentials::Work>> {
        let tx = self.db.transaction()?;
        let work = credentials::claim(&tx, op, validate_reference)?;
        tx.commit()?;
        Ok(work)
    }
    pub(crate) fn gateway_finish_credential(
        &mut self,
        op: &str,
        outcome: credentials::Outcome,
    ) -> Result<bool> {
        let tx = self.db.transaction()?;
        let changed = credentials::finish(&tx, op, outcome, validate_reference)?;
        reconcile(&tx)?;
        tx.commit()?;
        Ok(changed)
    }
}
/// Checked from RunControl/claim/commit; no passive-reply expiry exists here.
/// A gateway command may target desktop tasks but never changes their source.
pub(super) fn check_source(db: &Connection, record: &Value) -> Result<()> {
    let source = &record["source"];
    if source["channel_id"] != "gateway" {
        return Ok(());
    }
    let identity = text(source, "conversation_id")?;
    let parts: Vec<_> = identity.split(':').collect();
    if parts.len() != 3 {
        bail!("网关任务来源无效")
    }
    policy::identifier(parts[0])?;
    let config_rev: u64 = parts[1].parse()?;
    let binding_rev: u64 = parts[2].parse()?;
    if config_rev.to_string() != parts[1]
        || binding_rev.to_string() != parts[2]
        || binding_rev == 0
        || binding_rev > 9_007_199_254_740_991
    {
        bail!("网关任务修订无效")
    }
    let c = config::load(db, parts[0], config_rev)?;
    if !c.enabled {
        bail!("网关已停用")
    }
    let principal = text(source, "actor_id")?;
    policy::identifier(principal)?;
    let (device,crev,brev,status,expires,raw):(String,u64,u64,String,u64,String)=db.query_row("SELECT device_id,config_revision,revision,status,expires_at_ms,CASE WHEN length(CAST(body AS BLOB))<=262144 THEN body ELSE NULL END FROM gateway_bindings WHERE principal_id=?1",[principal],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    let b: persistence::Binding = serde_json::from_str(&raw)?;
    if device != c.device_id
        || crev != c.revision
        || brev != binding_rev
        || status != "active"
        || expires <= millis()
        || b.principal_id != principal
        || b.device_id != device
        || b.config_revision != crev
        || b.revision != brev
        || b.status != status
        || b.expires_at_ms != expires
    {
        bail!("网关任务授权已失效")
    }
    let session = text(record, "session_id")?;
    policy::identifier(session)?;
    if !db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
        [session],
        |r| r.get::<_, bool>(0),
    )? {
        bail!("网关任务会话已不存在")
    }
    let grant = b.grant.context("网关任务授权缺失")?;
    policy::validate_grant(&grant, millis())?;
    if grant.expires_at_ms != expires
        || !grant.permissions.run
        || !grant
            .session_ids
            .iter()
            .any(|id| record["session_id"] == *id)
    {
        bail!("网关任务会话权限已失效")
    }
    Ok(())
}
fn active_remote_rows(tx: &Transaction<'_>, table: &str) -> Result<Vec<Value>> {
    let mut q=tx.prepare(&format!("SELECT CASE WHEN length(CAST(body AS BLOB))<=?1 THEN body ELSE NULL END FROM {table} WHERE json_extract(body,'$.source.channel_id')='gateway' AND json_extract(body,'$.status') NOT IN ('completed','failed','cancelled') ORDER BY rowid LIMIT 128"))?;
    let rows = q
        .query_map([MAX_RECORD], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|r| Ok(serde_json::from_str(&r)?))
        .collect()
}
pub(super) fn reconcile(tx: &Transaction<'_>) -> Result<Vec<RunEvent>> {
    let mut events = Vec::new();
    for mut task in active_remote_rows(tx, "tasks")? {
        if task_active(&task) && check_source(tx, &task).is_err() {
            if task["stop_reason"].is_null() {
                task["stop_reason"] = json!("remote_permission_revoked")
            }
            put(tx, "tasks", &task)?;
        }
    }
    for plan in active_remote_rows(tx, "plans")? {
        if check_source(tx, &plan).is_err() {
            events.extend(cancel_plan(tx, text(&plan, "id")?)?);
        }
    }
    for mut task in active_remote_rows(tx, "tasks")? {
        if task_active(&task) && check_source(tx, &task).is_err() {
            task["status"] = json!("cancelled");
            task["error"] = json!("网关来源权限已失效；未继续执行。");
            if task["stop_reason"].is_null() {
                task["stop_reason"] = json!("remote_permission_revoked")
            }
            terminal_steps(&mut task, false);
            events.push(complete_transaction(tx, &mut task)?);
        }
    }
    for mut job in active_remote_rows(tx, "coordinator_jobs")? {
        if matches!(job["status"].as_str(), Some("pending" | "running"))
            && check_source(tx, &job).is_err()
        {
            job["status"] = json!("cancelled");
            job["error"] = json!("网关来源权限已失效；未继续编排。");
            job["updated_at"] = json!(now());
            put(tx, "coordinator_jobs", &job)?;
        }
    }
    Ok(events)
}
#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;
