//! Scope resolution shared by QQ input processing and its fixtures. The backend
//! supplies authoritative records inside the same SQLite transaction; nothing
//! here accepts a browser-provided snapshot or a local-owner bearer token.
use super::{
    policy::{RemoteCommand, RemoteGrant},
    presentation,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub(crate) trait RemoteContext {
    fn selected_session(&self) -> Result<Option<String>>;
    fn session_allowed(&self, id: &str) -> Result<bool>;
    fn session(&self, id: &str) -> Result<Value>;
    fn sessions(&self) -> Result<Vec<Value>>;
    fn task(&self, id: &str) -> Result<Value>;
    fn recent_tasks(&self, session_id: &str) -> Result<Vec<Value>>;
    fn plan(&self, id: &str) -> Result<Value>;
    fn recent_plans(&self, session_id: &str) -> Result<Vec<Value>>;
    fn request(&self, id: &str) -> Result<Value>;
    fn requests(&self, task_id: &str) -> Result<Vec<Value>>;
    /// Existing running work can only be influenced when its immutable source
    /// was created under this exact current principal+grant revision. Session
    /// access alone cannot inject remote input into a more privileged local run.
    fn owns_work(&self, record: &Value) -> Result<bool>;
}

pub(crate) enum Prepared {
    Reply(String),
    Execute {
        command: Value,
        /// Create/select update QQ's stable per-principal selection after the
        /// command receipt; SourceContext still uses the immutable reply route.
        select_receipt: bool,
        /// Only this operation may add a new session ACL, in the same commit.
        grant_created_session: bool,
    },
}
fn exec(command: Value) -> Prepared {
    Prepared::Execute {
        command,
        select_receipt: false,
        grant_created_session: false,
    }
}
fn permitted_session(ctx: &impl RemoteContext, id: &str) -> Result<Value> {
    if !ctx.session_allowed(id)? {
        bail!("QQ 身份未获此会话授权");
    }
    ctx.session(id)
}
fn current(ctx: &impl RemoteContext) -> Result<Value> {
    let selected = ctx
        .selected_session()?
        .context("请先发送 /会话，并用 /选择 会话ID 指定目标")?;
    permitted_session(ctx, &selected)
}
fn task(ctx: &impl RemoteContext, id: &str) -> Result<(Value, Value)> {
    let task = ctx.task(id)?;
    let session = permitted_session(
        ctx,
        task["session_id"].as_str().context("任务会话关联无效")?,
    )?;
    Ok((task, session))
}
fn plan(ctx: &impl RemoteContext, id: &str) -> Result<Value> {
    let plan = ctx.plan(id)?;
    permitted_session(
        ctx,
        plan["session_id"].as_str().context("计划会话关联无效")?,
    )?;
    Ok(plan)
}
fn own(ctx: &impl RemoteContext, record: &Value) -> Result<()> {
    if !ctx.owns_work(record)? {
        bail!("不能从 QQ 向本机或其他授权版本创建的在途工作注入输入；请在本机操作或新建受限任务");
    }
    Ok(())
}

/// For Execute, caller must use settings intersected with this principal's
/// controls for *all* agent snapshots, not merely a single run_agent payload.
pub(crate) fn prepare(
    ctx: &impl RemoteContext,
    grant: &RemoteGrant,
    group: bool,
    command: &RemoteCommand,
) -> Result<Prepared> {
    if !grant.permits(command, group) {
        bail!("QQ 绑定没有此操作或群受众权限，请在本机检查授权");
    }
    Ok(match command {
        RemoteCommand::Help => Prepared::Reply(super::policy::HELP.into()),
        RemoteCommand::Sessions => {
            let mut visible = Vec::new();
            for session in ctx.sessions()? {
                if let Some(id) = session["id"].as_str() {
                    if ctx.session_allowed(id)? {
                        visible.push(session);
                    }
                }
            }
            Prepared::Reply(presentation::session_list(
                &visible,
                ctx.selected_session()?.as_deref(),
            ))
        }
        RemoteCommand::Select { session_id } => {
            permitted_session(ctx, session_id)?;
            Prepared::Execute {
                command: json!({"type":"select_session","session_id":session_id}),
                select_receipt: true,
                grant_created_session: false,
            }
        }
        RemoteCommand::Create {
            from_session_id,
            title,
        } => {
            let origin = permitted_session(ctx, from_session_id)?;
            // The transaction executor must store this exact already-authorized
            // root, never canonicalize a newly supplied remote path or follow a
            // swapped symlink to manufacture a broader session grant.
            let root = origin["project_path"]
                .as_str()
                .context("已授权项目根缺失")?;
            Prepared::Execute {
                command: json!({"type":"create_session","title":title,"project_path":root}),
                select_receipt: true,
                grant_created_session: true,
            }
        }
        RemoteCommand::Run { content } => {
            exec(json!({"type":"run_agent","session_id":current(ctx)?["id"],"content":content}))
        }
        RemoteCommand::Orchestrate { content } => {
            exec(json!({"type":"send_message","session_id":current(ctx)?["id"],"content":content}))
        }
        RemoteCommand::Status { task_id } => {
            if let Some(id) = task_id {
                Prepared::Reply(presentation::task_status(&task(ctx, id)?.0))
            } else {
                let session = current(ctx)?;
                let id = session["id"].as_str().context("会话 ID 缺失")?;
                let records = ctx.recent_tasks(id)?;
                let text = records
                    .iter()
                    .filter(|task| task["session_id"] == id)
                    .take(5)
                    .map(presentation::task_status)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Prepared::Reply(if text.is_empty() {
                    "当前会话尚无任务。".into()
                } else {
                    text
                })
            }
        }
        RemoteCommand::Result { task_id } => {
            let (task, session) = task(ctx, task_id)?;
            Prepared::Reply(presentation::task_result(
                &task,
                session["project_path"].as_str().unwrap_or(""),
            ))
        }
        RemoteCommand::Cancel { task_id } => {
            task(ctx, task_id)?;
            exec(json!({"type":"cancel_task","task_id":task_id}))
        }
        RemoteCommand::Continue { task_id, content } => {
            task(ctx, task_id)?;
            exec(json!({"type":"continue_task","task_id":task_id,"content":content}))
        }
        RemoteCommand::Steer { task_id, content } => {
            let (task, _) = task(ctx, task_id)?;
            own(ctx, &task)?;
            exec(json!({"type":"steer_task","task_id":task_id,"content":content}))
        }
        RemoteCommand::Questions { task_id } => {
            let (task, session) = task(ctx, task_id)?;
            own(ctx, &task)?;
            Prepared::Reply(presentation::questions(
                &task,
                &ctx.requests(task_id)?,
                session["project_path"].as_str().unwrap_or(""),
            ))
        }
        RemoteCommand::Answer {
            request_id,
            revision,
            answer,
        } => {
            let request = ctx.request(request_id)?;
            if request["kind"] != "question" || request["status"] != "pending" {
                bail!("QQ 只能回答尚未处理的问题，不能批准动作");
            }
            let (task, _) = task(
                ctx,
                request["task_id"].as_str().context("问题任务关联无效")?,
            )?;
            own(ctx, &task)?;
            if task["waiting_request_id"] != *request_id || request["revision"] != *revision {
                bail!("问题修订或当前等待关联已改变");
            }
            exec(
                json!({"type":"answer_request","request_id":request_id,"expected_revision":revision,"answer":answer}),
            )
        }
        RemoteCommand::Plan { plan_id } => {
            if let Some(id) = plan_id {
                Prepared::Reply(presentation::plan_status(&plan(ctx, id)?))
            } else {
                let session = current(ctx)?;
                let id = session["id"].as_str().context("会话 ID 缺失")?;
                let text = ctx
                    .recent_plans(id)?
                    .iter()
                    .filter(|p| p["session_id"] == id)
                    .take(5)
                    .map(presentation::plan_status)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Prepared::Reply(if text.is_empty() {
                    "当前会话尚无计划；编排作业可能仍在处理。".into()
                } else {
                    text
                })
            }
        }
        RemoteCommand::StartPlan { plan_id, revision } => {
            own(ctx, &plan(ctx, plan_id)?)?;
            exec(json!({"type":"start_plan","plan_id":plan_id,"expected_revision":revision}))
        }
        RemoteCommand::ResumePlan { plan_id, revision } => {
            own(ctx, &plan(ctx, plan_id)?)?;
            exec(json!({"type":"resume_plan","plan_id":plan_id,"expected_revision":revision}))
        }
        RemoteCommand::PausePlan { plan_id, revision } => {
            plan(ctx, plan_id)?;
            exec(json!({"type":"pause_plan","plan_id":plan_id,"expected_revision":revision}))
        }
        RemoteCommand::CancelPlan { plan_id } => {
            plan(ctx, plan_id)?;
            exec(json!({"type":"cancel_plan","plan_id":plan_id}))
        }
        RemoteCommand::Pair { .. } => bail!("配对必须进入单独的待确认流程"),
    })
}

#[cfg(test)]
mod tests {
    use super::super::policy::{conservative_controls, RemotePermissions};
    use super::*;
    const SESSION: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3b";
    struct Fixture {
        allowed: bool,
        owned: bool,
        approval: bool,
    }
    impl RemoteContext for Fixture {
        fn selected_session(&self) -> Result<Option<String>> {
            Ok(Some(SESSION.into()))
        }
        fn session_allowed(&self, _: &str) -> Result<bool> {
            Ok(self.allowed)
        }
        fn session(&self, _: &str) -> Result<Value> {
            Ok(json!({"id":SESSION,"project_path":"/approved/root","title":"会话"}))
        }
        fn sessions(&self) -> Result<Vec<Value>> {
            Ok(vec![self.session(SESSION)?])
        }
        fn task(&self, _: &str) -> Result<Value> {
            Ok(
                json!({"id":SESSION,"session_id":SESSION,"status":"waiting_input","waiting_request_id":SESSION}),
            )
        }
        fn recent_tasks(&self, _: &str) -> Result<Vec<Value>> {
            Ok(vec![self.task(SESSION)?])
        }
        fn plan(&self, _: &str) -> Result<Value> {
            Ok(json!({"id":SESSION,"session_id":SESSION,"revision":1}))
        }
        fn recent_plans(&self, _: &str) -> Result<Vec<Value>> {
            Ok(vec![self.plan(SESSION)?])
        }
        fn request(&self, _: &str) -> Result<Value> {
            Ok(
                json!({"id":SESSION,"task_id":SESSION,"revision":1,"status":"pending","kind":if self.approval {"approval"} else {"question"}}),
            )
        }
        fn requests(&self, _: &str) -> Result<Vec<Value>> {
            Ok(vec![self.request(SESSION)?])
        }
        fn owns_work(&self, _: &Value) -> Result<bool> {
            Ok(self.owned)
        }
    }
    fn grant() -> RemoteGrant {
        RemoteGrant {
            session_ids: vec![SESSION.into()],
            permissions: RemotePermissions {
                view: true,
                read_results: true,
                run: true,
                cancel: true,
                answer: true,
                orchestrate: true,
                create_session: true,
                share_group_results: false,
            },
            controls: conservative_controls(),
            expires_at_ms: 999999,
        }
    }
    #[test]
    fn session_acl_is_required_for_queries_and_every_task_mutation() {
        let ctx = Fixture {
            allowed: false,
            owned: true,
            approval: false,
        };
        for command in [
            RemoteCommand::Run {
                content: "x".into(),
            },
            RemoteCommand::Status {
                task_id: Some(SESSION.into()),
            },
            RemoteCommand::Cancel {
                task_id: SESSION.into(),
            },
            RemoteCommand::Continue {
                task_id: SESSION.into(),
                content: None,
            },
            RemoteCommand::Create {
                from_session_id: SESSION.into(),
                title: "x".into(),
            },
        ] {
            assert!(prepare(&ctx, &grant(), false, &command).is_err());
        }
    }
    #[test]
    fn session_access_cannot_inject_into_more_privileged_local_work() {
        let ctx = Fixture {
            allowed: true,
            owned: false,
            approval: false,
        };
        for command in [
            RemoteCommand::Steer {
                task_id: SESSION.into(),
                content: "x".into(),
            },
            RemoteCommand::Answer {
                request_id: SESSION.into(),
                revision: 1,
                answer: "yes".into(),
            },
            RemoteCommand::StartPlan {
                plan_id: SESSION.into(),
                revision: 1,
            },
            RemoteCommand::ResumePlan {
                plan_id: SESSION.into(),
                revision: 1,
            },
        ] {
            assert!(prepare(&ctx, &grant(), false, &command).is_err());
        }
        assert!(prepare(
            &ctx,
            &grant(),
            false,
            &RemoteCommand::Cancel {
                task_id: SESSION.into()
            }
        )
        .is_ok());
        assert!(prepare(
            &ctx,
            &grant(),
            false,
            &RemoteCommand::Continue {
                task_id: SESSION.into(),
                content: None
            }
        )
        .is_ok());
    }
    #[test]
    fn question_answer_never_approves_and_new_session_copies_only_authorized_root() {
        let mut ctx = Fixture {
            allowed: true,
            owned: true,
            approval: true,
        };
        let answer = RemoteCommand::Answer {
            request_id: SESSION.into(),
            revision: 1,
            answer: "yes".into(),
        };
        assert!(prepare(&ctx, &grant(), false, &answer).is_err());
        ctx.approval = false;
        assert!(prepare(&ctx, &grant(), false, &answer).is_ok());
        let Prepared::Execute {
            command,
            grant_created_session,
            ..
        } = prepare(
            &ctx,
            &grant(),
            false,
            &RemoteCommand::Create {
                from_session_id: SESSION.into(),
                title: "新会话".into(),
            },
        )
        .unwrap()
        else {
            panic!("execution expected")
        };
        assert_eq!(command["project_path"], "/approved/root");
        assert!(grant_created_session);
        assert!(prepare(
            &ctx,
            &grant(),
            true,
            &RemoteCommand::Result {
                task_id: SESSION.into()
            }
        )
        .is_err());
    }
}
