//! QQ's private Store boundary. Remote routing is not owner authority. All
//! command effects, ACL updates, receipts and reply intents share one transaction.
use super::*;
use crate::capabilities::vault::CredentialRef;
use crate::qq::{
    admin, configuration as config, credentials, engine, events, notifications, persistence,
    runtime, transport,
};
use harness_core::CredentialOperationView;
fn millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}
pub(super) fn migrate(db: &Connection) -> Result<()> {
    persistence::migrate(db)?;
    config::migrate(db)?;
    credentials::migrate(db)?;
    admin::migrate(db)?;
    engine::migrate(db)?;
    notifications::migrate(db)
}
pub(super) fn recover(tx: &Transaction<'_>) -> Result<()> {
    credentials::recover(tx)?;
    persistence::startup_recover(tx, millis())?;
    config::startup_recover(tx)
}
fn validate_reference(owner: &str, digest: &str, reference: &str) -> Result<()> {
    let binding = crate::qq::credential_binding(owner, digest)?;
    CredentialRef::parse(reference, &binding)?;
    Ok(())
}
fn ready(c: &config::Config, fixture: bool) -> Result<bool> {
    if fixture {
        return Ok(true);
    }
    let Some(reference) = c.credential_ref() else {
        return Ok(false);
    };
    validate_reference(
        &c.id,
        &config::credential_binding_digest(&c.app_id)?,
        reference,
    )?;
    Ok(true)
}
fn constrained(
    settings: &Settings,
    grant: &crate::qq::policy::RemoteGrant,
    validate_executors: bool,
) -> Result<Settings> {
    let mut effective = settings.clone();
    effective.execution_controls = execution::resolve_controls(
        &settings.execution_controls,
        None,
        Some(&grant.controls),
        "native",
    )?;
    if validate_executors && grant.permissions.run {
        execution::resolve_controls(
            &effective.execution_controls,
            None,
            None,
            &settings.main_executor,
        )?;
    }
    if validate_executors && grant.permissions.orchestrate {
        for agent in &settings.agents {
            execution::resolve_controls(
                &effective.execution_controls,
                agent.controls.as_ref(),
                None,
                &agent.executor,
            )?;
        }
    }
    Ok(effective)
}
fn enabled(db: &Connection, id: &str, rev: u64) -> Result<config::Config> {
    let c = config::load(db, id, rev)?;
    if !c.enabled {
        bail!("QQ 配置已停用");
    }
    Ok(c)
}
impl Store {
    pub(crate) fn qq_config(&self) -> Result<Option<config::Config>> {
        config::current(&self.db)
    }
    pub(crate) fn qq_snapshot(&self, fixture: bool) -> Result<Value> {
        let mut snapshot = persistence::public_snapshot(&self.db, 200)?;
        snapshot["configuration"] = self
            .qq_config()?
            .map(|c| {
                let mut v = c.public_view();
                if fixture {
                    v["mode"] = json!("fixture");
                }
                v
            })
            .unwrap_or(Value::Null);
        snapshot["connection"] = config::connection_view(&self.db)?;
        snapshot["credential_operations"] =
            serde_json::to_value(credentials::operations(&self.db)?)?;
        Ok(snapshot)
    }
    pub(crate) fn qq_admin_command(
        &mut self,
        envelope: &Value,
        settings: &Settings,
        fixture: bool,
    ) -> Result<Value> {
        let tx = self.db.transaction()?;
        let applied = admin::apply(
            &tx,
            envelope,
            millis(),
            |c| ready(c, fixture),
            |g| {
                constrained(settings, g, true)?;
                Ok(())
            },
        );
        match applied {
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
    pub(crate) fn qq_admin_receipt(&self, id: &str) -> Result<Option<Value>> {
        admin::receipt(&self.db, id)
    }
    pub(crate) fn qq_checkpoint(
        &self,
        id: &str,
        rev: u64,
    ) -> Result<Option<transport::Checkpoint>> {
        enabled(&self.db, id, rev)?;
        Ok(
            config::load_checkpoint(&self.db, id, rev)?.map(|p| transport::Checkpoint {
                session_id: p.session_id,
                seq: p.sequence,
            }),
        )
    }
    pub(crate) fn qq_ingest(
        &mut self,
        id: &str,
        rev: u64,
        payload: &Value,
        point: &transport::Checkpoint,
    ) -> Result<()> {
        self.qq_ingest_at(id, rev, payload, point, millis())
    }
    // Internal deterministic clock seam; no remote/public clock override.
    fn qq_ingest_at(
        &mut self,
        id: &str,
        rev: u64,
        payload: &Value,
        point: &transport::Checkpoint,
        now: u64,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let c = enabled(&tx, id, rev)?;
        let old = config::load_checkpoint(&tx, id, rev)?;
        let dispatch = events::dispatch(
            &c.app_id,
            old.as_ref().map(|p| p.bot_user_id.as_str()),
            payload,
            now,
        );
        match dispatch {
            Ok(events::Dispatch::Ready { bot_user_id }) => config::ready_checkpoint(
                &tx,
                &config::Checkpoint {
                    config_id: id.into(),
                    config_revision: rev,
                    session_id: point.session_id.clone(),
                    sequence: point.seq,
                    bot_user_id,
                },
            )?,
            other => {
                if payload["t"] == "READY" {
                    bail!("QQ READY 身份无效");
                }
                match other {
                    Ok(events::Dispatch::Input(input)) => {
                        persistence::accept_input_with_policy(&tx, &input, now, &c.reply_policy)?;
                    }
                    Ok(events::Dispatch::ClockSkew { timestamp_ms }) => {
                        persistence::record_clock_skew(&tx, timestamp_ms, now)?;
                    }
                    _ => {}
                }
                config::advance_checkpoint(&tx, id, rev, &point.session_id, point.seq)?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn qq_clear_checkpoint(&mut self, id: &str, rev: u64) -> Result<()> {
        let tx = self.db.transaction()?;
        config::clear_checkpoint(&tx, id, rev)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn qq_connection(
        &mut self,
        id: &str,
        rev: u64,
        state: runtime::ConnectionState,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let (state, reason) = match state {
            runtime::ConnectionState::Connecting => (config::ConnectionState::Connecting, None),
            runtime::ConnectionState::Ready => (config::ConnectionState::Ready, None),
            runtime::ConnectionState::Disconnected => (
                config::ConnectionState::Disconnected,
                Some(config::Reason::Network),
            ),
            runtime::ConnectionState::CredentialUnavailable => (
                config::ConnectionState::Disconnected,
                Some(config::Reason::CredentialUnavailable),
            ),
            runtime::ConnectionState::StorageUnavailable => (
                config::ConnectionState::Disconnected,
                Some(config::Reason::Storage),
            ),
        };
        config::set_connection_state(&tx, id, rev, state, reason)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn qq_process_input(
        &mut self,
        id: &str,
        rev: u64,
        settings: &Settings,
    ) -> Result<(bool, Option<RunEvent>)> {
        // Reconciliation must survive a separately rejected remote command.
        {
            let tx = self.db.transaction()?;
            reconcile(&tx)?;
            tx.commit()?;
        }
        let tx = self.db.transaction()?;
        enabled(&tx, id, rev)?;
        let Some(original) = engine::next(&tx)? else {
            return Ok((false, None));
        };
        let result = engine::process(&tx, id, rev, millis(), |tx, e| {
            let effective = constrained(settings, e.grant, false)?;
            command_transaction(
                tx,
                &e.envelope,
                &effective,
                &e.decision_actor,
                e.copied_project_root.as_deref(),
            )
        });
        match result {
            Ok(processed) => {
                if tx.commit().is_err() {
                    // A storage acknowledgement can fail after the commit. Check
                    // durable input identity, never redispatch or overwrite it.
                    if persistence::inbox(&self.db, &original.inbox_id)?.status == "handled" {
                        return Ok((true, None));
                    }
                    bail!("QQ 输入提交未确认，未推进或重发指令");
                }
                Ok((processed.is_some(), processed.and_then(|p| p.event)))
            }
            Err(_) => {
                tx.rollback()?;
                let tx = self.db.transaction()?;
                engine::reject_pending(&tx, &original.inbox_id, millis())?;
                tx.commit()?;
                Ok((true, None))
            }
        }
    }
    pub(crate) fn qq_claim(
        &mut self,
        id: &str,
        rev: u64,
    ) -> Result<Option<persistence::OutboxClaim>> {
        let tx = self.db.transaction()?;
        let c = enabled(&tx, id, rev)?;
        notifications::bridge(&tx, millis())?;
        let claim = persistence::claim_outbox(&tx, millis())?;
        if let Some(claim) = &claim {
            if claim.route.identity.app_id != c.app_id {
                persistence::finalize_outbox(
                    &tx,
                    &claim.record.id,
                    claim
                        .record
                        .claim_nonce
                        .as_deref()
                        .context("QQ 投递领取缺失")?,
                    persistence::DeliveryOutcome::Failed {
                        reason: persistence::FailureReason::ConfigurationDisabled,
                    },
                    millis(),
                )?;
                notifications::sync(&tx)?;
                tx.commit()?;
                return Ok(None);
            }
        }
        tx.commit()?;
        Ok(claim)
    }
    pub(crate) fn qq_allowed(
        &mut self,
        id: &str,
        rev: u64,
        claim: &persistence::OutboxClaim,
    ) -> Result<bool> {
        let tx = self.db.transaction()?;
        let c = enabled(&tx, id, rev)?;
        let quota = match claim.route.identity.kind {
            persistence::ConversationKind::User => c.reply_policy.max_user,
            persistence::ConversationKind::Group => c.reply_policy.max_group,
        };
        let allowed = millis()
            < claim
                .route
                .received_at_ms
                .min(claim.route.accepted_at_ms)
                .saturating_add(c.reply_policy.window_ms)
            && claim.record.msg_seq <= quota
            && claim.route.identity.app_id == c.app_id
            && config::live_task_route(&tx, &claim.route.route_id, millis()).is_ok()
            && persistence::authorize_send(
                &tx,
                &claim.record.id,
                claim
                    .record
                    .claim_nonce
                    .as_deref()
                    .context("QQ 投递领取缺失")?,
                millis(),
            )?;
        tx.commit()?;
        Ok(allowed)
    }
    pub(crate) fn qq_finish(
        &mut self,
        claim: &persistence::OutboxClaim,
        outcome: persistence::DeliveryOutcome,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        persistence::finalize_outbox(
            &tx,
            &claim.record.id,
            claim
                .record
                .claim_nonce
                .as_deref()
                .context("QQ 投递领取缺失")?,
            outcome,
            millis(),
        )?;
        notifications::sync(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn qq_existing_set(&self, op: &str, id: &str, rev: u64) -> Result<Option<Value>> {
        credentials::existing_set(&self.db, op, id, rev)
    }
    pub(crate) fn qq_begin_set(
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
    pub(crate) fn qq_begin_delete(&mut self, op: &str, id: &str, rev: u64) -> Result<Value> {
        let tx = self.db.transaction()?;
        let receipt = credentials::begin_delete(&tx, op, id, rev, millis(), validate_reference)?;
        reconcile(&tx)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn qq_begin_cleanup(&mut self, op: &str, target: &str) -> Result<Value> {
        let tx = self.db.transaction()?;
        let receipt = credentials::begin_cleanup(&tx, op, target)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(crate) fn qq_credential_operation(
        &self,
        op: &str,
    ) -> Result<Option<CredentialOperationView>> {
        crate::qq::policy::validate_id(op)?;
        if !self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM qq_credential_operations WHERE id=?1)",
            [op],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(None);
        }
        Ok(Some(credentials::operation(&self.db, op)?))
    }
    pub(crate) fn qq_claim_credential(&mut self, op: &str) -> Result<Option<credentials::Work>> {
        let tx = self.db.transaction()?;
        let work = credentials::claim(&tx, op, validate_reference)?;
        tx.commit()?;
        Ok(work)
    }
    pub(crate) fn qq_finish_credential(
        &mut self,
        op: &str,
        outcome: credentials::Outcome,
    ) -> Result<bool> {
        let tx = self.db.transaction()?;
        let result = credentials::finish(&tx, op, outcome, validate_reference)?;
        tx.commit()?;
        Ok(result)
    }
    pub(crate) fn check_task_remote(&self, id: &str) -> Result<()> {
        let task = record(&self.db, "tasks", id)?;
        check_source(&self.db, &task)?;
        super::gateway::check_source(&self.db, &task)
    }
}
/// For snapshots and running work, source fields must match the immutable route.
/// Passive reply expiry does not revoke an otherwise still-authorized long task.
pub(super) fn check_source(db: &Connection, value: &Value) -> Result<()> {
    let source = &value["source"];
    if source["channel_id"] != "qq" {
        return Ok(());
    }
    let route = config::live_task_route(db, text(source, "conversation_id")?, millis())?;
    if source["actor_id"] != route.principal_id {
        bail!("QQ 任务身份不匹配");
    }
    Ok(())
}
pub(super) fn reconcile(tx: &Transaction<'_>) -> Result<Vec<RunEvent>> {
    let mut produced = Vec::new();
    // Record the first stop cause before cancel_plan touches parent/worker tasks.
    for mut task in scheduling::active_remote(tx, scheduling::RemoteTable::Tasks, "qq")? {
        if task_active(&task) && check_source(tx, &task).is_err() {
            if task["stop_reason"].is_null() {
                task["stop_reason"] = json!("remote_permission_revoked");
            }
            put(tx, "tasks", &task)?;
        }
    }
    for plan in scheduling::active_remote(tx, scheduling::RemoteTable::Plans, "qq")? {
        if !matches!(
            plan["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) && check_source(tx, &plan).is_err()
        {
            produced.extend(cancel_plan(tx, text(&plan, "id")?)?);
        }
    }
    for mut task in scheduling::active_remote(tx, scheduling::RemoteTable::Tasks, "qq")? {
        if task_active(&task) && check_source(tx, &task).is_err() {
            task["status"] = json!("cancelled");
            task["error"] = json!("QQ 来源权限已失效；未继续执行。");
            if task["stop_reason"].is_null() {
                task["stop_reason"] = json!("remote_permission_revoked");
            }
            terminal_steps(&mut task, false);
            produced.push(complete_transaction(tx, &mut task)?);
        }
    }
    for mut job in scheduling::active_remote(tx, scheduling::RemoteTable::Jobs, "qq")? {
        if matches!(job["status"].as_str(), Some("pending" | "running"))
            && check_source(tx, &job).is_err()
        {
            job["status"] = json!("cancelled");
            job["error"] = json!("QQ 来源权限已失效；未继续编排。");
            job["updated_at"] = json!(now());
            put(tx, "coordinator_jobs", &job)?;
        }
    }
    Ok(produced)
}
#[cfg(test)]
#[path = "qq_tests.rs"]
mod tests;
