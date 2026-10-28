use super::*;
use crate::steering::InputAcknowledgement;
use harness_core::TaskSupplement;

pub(super) fn accept(
    tx: &Transaction<'_>,
    task_id: &str,
    content: &str,
) -> Result<(String, String, RunEvent)> {
    let mut task = record(tx, "tasks", task_id)?;
    if task["task_kind"] != "main"
        || task["status"] != "running"
        || task["execution_mode"] == "demo"
    {
        bail!("只能补充正在执行的直接主任务；任务已结束时请明确继续或排队下一条");
    }
    let session_id = text(&task, "session_id")?.to_owned();
    let mut inputs = task["supplements"].as_array().cloned().unwrap_or_default();
    if inputs.len() >= 32
        || inputs
            .iter()
            .filter_map(|input| input["content"].as_str())
            .map(str::len)
            .sum::<usize>()
            + content.len()
            > 65_536
    {
        bail!("此任务的补充要求已达到 32 条或 64 KiB 上限，请排队下一条");
    }
    let id = new_id();
    inputs.push(json!({"id":id,"task_id":task_id,"content":content,"status":"pending", "reason":"已接收，等待执行器安全边界；尚未应用", "applied_via":null,"created_at":now(),"updated_at":now()}));
    task["supplements"] = json!(inputs);
    task["updated_at"] = json!(now());
    put(tx, "tasks", &task)?;
    Ok((
        id,
        session_id.clone(),
        event(
            Some(session_id),
            Some(task_id.into()),
            "input_pending",
            "补充已接收，等待安全边界，不代表已影响执行",
        ),
    ))
}

impl Store {
    pub(crate) fn claim_inputs(&mut self, task_id: &str) -> Result<Vec<TaskSupplement>> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if task["status"] != "running" || task["task_kind"] != "main" {
            return Ok(Vec::new());
        }
        let mut claimed = Vec::new();
        if let Some(inputs) = task.get_mut("supplements").and_then(Value::as_array_mut) {
            for input in inputs
                .iter_mut()
                .filter(|input| input["status"] == "pending")
                .take(8)
            {
                input["status"] = json!("delivering");
                input["reason"] = json!("执行器正在提交补充，尚未确认应用");
                input["updated_at"] = json!(now());
                claimed.push(serde_json::from_value(input.clone())?);
            }
        }
        if !claimed.is_empty() {
            task["updated_at"] = json!(now());
            put(&tx, "tasks", &task)?;
            persist_event(
                &tx,
                &event(
                    Some(text(&task, "session_id")?.into()),
                    Some(task_id.into()),
                    "input_delivering",
                    "补充正在提交，等待执行器确认",
                ),
            )?;
        }
        tx.commit()?;
        Ok(claimed)
    }

    pub(crate) fn ack_inputs(
        &mut self,
        task_id: &str,
        ids: &[String],
        outcome: InputAcknowledgement,
    ) -> Result<Option<RunEvent>> {
        if ids.is_empty() {
            return Ok(None);
        }
        let unique: HashSet<_> = ids.iter().collect();
        if unique.len() != ids.len() || ids.len() > 8 {
            bail!("补充确认标识重复或超过批次上限");
        }
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if task["status"] != "running" || task["task_kind"] != "main" {
            bail!("任务已结束，迟到的补充确认不会应用到其他任务");
        }
        let inputs = task["supplements"]
            .as_array_mut()
            .context("任务没有待确认补充")?;
        if ids.iter().any(|id| {
            !inputs
                .iter()
                .any(|input| input["id"] == *id && input["status"] == "delivering")
        }) {
            bail!("补充确认与待提交记录不匹配");
        }
        let (status, reason, via) = match outcome {
            InputAcknowledgement::Applied { via } => {
                if !matches!(via.as_str(), "native_request" | "codex_turn_steer") {
                    bail!("未知的补充应用方式");
                }
                (
                    "applied",
                    "已包含在执行器确认接收的输入中；不保证模型遵守".to_owned(),
                    Some(via),
                )
            }
            InputAcknowledgement::NotApplied { reason } => {
                if reason.len() > 2048 {
                    bail!("补充拒绝说明过长");
                }
                ("not_applied", reason, None)
            }
        };
        for input in inputs
            .iter_mut()
            .filter(|input| ids.iter().any(|id| input["id"] == *id))
        {
            input["status"] = json!(status);
            input["reason"] = json!(reason);
            input["applied_via"] = json!(via);
            input["updated_at"] = json!(now());
        }
        task["updated_at"] = json!(now());
        put(&tx, "tasks", &task)?;
        let item = event(
            Some(text(&task, "session_id")?.into()),
            Some(task_id.into()),
            &format!("input_{status}"),
            &reason,
        );
        persist_event(&tx, &item)?;
        tx.commit()?;
        Ok(Some(item))
    }
}

pub(super) fn finalize(task: &mut Value) {
    if let Some(inputs) = task.get_mut("supplements").and_then(Value::as_array_mut) {
        for input in inputs {
            let (status, reason) = match input["status"].as_str() {
                Some("pending") => (
                    "not_applied",
                    "任务已结束，未到达可应用的边界；没有移交给下一任务",
                ),
                Some("delivering") => (
                    "unknown",
                    "任务已结束但未取得提交确认；结果未知，不会重放或移交给下一任务",
                ),
                _ => continue,
            };
            input["status"] = json!(status);
            input["reason"] = json!(reason);
            input["updated_at"] = json!(now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "fixture-model".into(),
            api_key: "fixture-private".into(),
            ..Default::default()
        }
    }
    fn envelope(command: Value) -> Value {
        json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"steering-tests"},"command":command})
    }
    fn send(store: &mut Store, command: Value) -> Value {
        store.command(envelope(command), &settings()).unwrap().0
    }
    fn setup() -> (Store, String) {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let receipt = send(
            &mut store,
            json!({"type":"create_session","title":"补充测试","project_path":std::env::temp_dir()}),
        );
        (store, receipt["session_id"].as_str().unwrap().into())
    }
    fn running(store: &mut Store, session: &str, goal: &str) -> String {
        let receipt = send(
            store,
            json!({"type":"run_agent","session_id":session,"content":goal}),
        );
        let task = receipt["task_id"].as_str().unwrap().to_owned();
        assert_eq!(store.claim_task().unwrap().unwrap()["id"], task);
        task
    }
    #[test]
    fn supplement_receipt_is_idempotent_and_claim_ack_are_atomic() {
        let (mut store, session) = setup();
        let task = running(&mut store, &session, "原始目标");
        let request =
            envelope(json!({"type":"steer_task","task_id":task,"content":"保留用户已有改动"}));
        let first = store.command(request.clone(), &settings()).unwrap().0;
        let cursor = store.snapshot().unwrap()["event_cursor"].clone();
        assert_eq!(
            store.command(request.clone(), &settings()).unwrap().0,
            first
        );
        assert_eq!(store.snapshot().unwrap()["event_cursor"], cursor);
        let mut conflict = request;
        conflict["command"]["content"] = json!("另一要求");
        assert!(store.command(conflict, &settings()).is_err());
        let claimed = store.claim_inputs(&task).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, first["supplement_id"]);
        assert_eq!(claimed[0].status, "delivering");
        assert!(store.claim_inputs(&task).unwrap().is_empty());
        assert!(store
            .ack_inputs(
                &task,
                &[claimed[0].id.clone(), "foreign-id".into()],
                InputAcknowledgement::Applied {
                    via: "native_request".into()
                }
            )
            .is_err());
        assert_eq!(
            record(&store.db, "tasks", &task).unwrap()["supplements"][0]["status"],
            "delivering"
        );
        store
            .ack_inputs(
                &task,
                &[claimed[0].id.clone()],
                InputAcknowledgement::Applied {
                    via: "native_request".into(),
                },
            )
            .unwrap();
        assert_eq!(
            record(&store.db, "tasks", &task).unwrap()["supplements"][0]["status"],
            "applied"
        );
        assert!(store
            .ack_inputs(
                &task,
                &[claimed[0].id.clone()],
                InputAcknowledgement::Applied {
                    via: "native_request".into()
                }
            )
            .is_err());
        store.finish(&task, Ok("实际结果".into())).unwrap();
        let next = running(&mut store, &session, "下一条");
        let history = store.execution_history(&next).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|m| m["content"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["原始目标", "保留用户已有改动", "实际结果"]
        );
    }
    #[test]
    fn terminal_races_never_transfer_pending_or_unconfirmed_inputs() {
        let (mut store, session) = setup();
        let task = running(&mut store, &session, "待结束");
        send(
            &mut store,
            json!({"type":"steer_task","task_id":task,"content":"来不及应用"}),
        );
        store.finish(&task, Ok("已完成".into())).unwrap();
        assert_eq!(
            record(&store.db, "tasks", &task).unwrap()["supplements"][0]["status"],
            "not_applied"
        );
        assert!(store
            .command(
                envelope(json!({"type":"steer_task","task_id":task,"content":"迟到"})),
                &settings()
            )
            .is_err());
        let next = running(&mut store, &session, "下一条");
        assert!(store.claim_inputs(&next).unwrap().is_empty());
        let history = store.execution_history(&next).unwrap();
        assert!(!history
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "来不及应用"));
        assert!(history.iter().any(|m| m["role"] == "assistant"
            && m["content"]
                .as_str()
                .is_some_and(|t| t.contains("来不及应用"))));
        send(
            &mut store,
            json!({"type":"steer_task","task_id":next,"content":"确认未知"}),
        );
        let claimed = store.claim_inputs(&next).unwrap();
        send(&mut store, json!({"type":"cancel_task","task_id":next}));
        assert_eq!(
            record(&store.db, "tasks", &next).unwrap()["supplements"][0]["status"],
            "unknown"
        );
        assert!(store
            .ack_inputs(
                &next,
                &[claimed[0].id.clone()],
                InputAcknowledgement::Applied {
                    via: "native_request".into()
                }
            )
            .is_err());
        assert_eq!(
            record(&store.db, "tasks", &next).unwrap()["status"],
            "cancelled"
        );
    }
    #[test]
    fn supplement_budgets_and_receipt_order_are_enforced() {
        let (mut store, session) = setup();
        let task = running(&mut store, &session, "预算");
        let mut ids = Vec::new();
        for n in 0..8 {
            let text = format!("{n}{}", "x".repeat(8191));
            ids.push(
                send(
                    &mut store,
                    json!({"type":"steer_task","task_id":task,"content":text}),
                )["supplement_id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        assert!(store
            .command(
                envelope(json!({"type":"steer_task","task_id":task,"content":"超额"})),
                &settings()
            )
            .is_err());
        assert!(store
            .command(
                envelope(json!({"type":"steer_task","task_id":task,"content":"中".repeat(3000)})),
                &settings()
            )
            .is_err());
        assert_eq!(
            store
                .claim_inputs(&task)
                .unwrap()
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            ids
        );
    }
    #[test]
    fn continue_creates_one_new_task_in_original_session_without_replaying_old_task() {
        let (mut store, session) = setup();
        let task = running(&mut store, &session, "原任务");
        store.finish(&task, Err(anyhow!("保留失败证据"))).unwrap();
        send(
            &mut store,
            json!({"type":"create_session","title":"其他会话","project_path":std::env::temp_dir()}),
        );
        let request = envelope(json!({"type":"continue_task","task_id":task}));
        let receipt = store.command(request.clone(), &settings()).unwrap().0;
        assert_eq!(store.command(request, &settings()).unwrap().0, receipt);
        let next = record(&store.db, "tasks", receipt["task_id"].as_str().unwrap()).unwrap();
        assert_ne!(next["id"], task);
        assert_eq!(next["continued_from"], task);
        assert_eq!(next["session_id"], session);
        assert_eq!(
            record(&store.db, "tasks", &task).unwrap()["status"],
            "failed"
        );
        assert!(store
            .command(
                envelope(json!({"type":"continue_task","task_id":next["id"]})),
                &settings()
            )
            .is_err());
    }
    #[test]
    fn restart_records_unknown_delivery_without_retrying_supplements() {
        let path = std::env::temp_dir().join(format!("self-agent-steering-{}.sqlite", new_id()));
        let mut store = Store::open(&path).unwrap();
        let session = send(
            &mut store,
            json!({"type":"create_session","title":"重启","project_path":std::env::temp_dir()}),
        )["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let task = running(&mut store, &session, "写入未知");
        send(
            &mut store,
            json!({"type":"steer_task","task_id":task,"content":"补充"}),
        );
        store.claim_inputs(&task).unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let old = record(&store.db, "tasks", &task).unwrap();
        assert_eq!(old["status"], "failed");
        assert_eq!(old["supplements"][0]["status"], "unknown");
        assert!(store.claim_inputs(&task).unwrap().is_empty());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn continuing_an_older_task_explicitly_carries_that_tasks_goal_and_outcome() {
        let (mut store, session) = setup();
        let first = running(&mut store, &session, "FIRST_DISTINCT_GOAL");
        store
            .finish(&first, Ok("FIRST_DISTINCT_RESULT".into()))
            .unwrap();
        let second = running(&mut store, &session, "LATEST_UNRELATED_GOAL");
        store
            .finish(&second, Ok("LATEST_UNRELATED_RESULT".into()))
            .unwrap();
        let receipt = send(&mut store, json!({"type":"continue_task","task_id":first}));
        let task = record(&store.db, "tasks", receipt["task_id"].as_str().unwrap()).unwrap();
        let objective = task["objective"].as_str().unwrap();
        assert!(objective.contains("FIRST_DISTINCT_GOAL"));
        assert!(objective.contains("FIRST_DISTINCT_RESULT"));
        assert!(!objective.contains("LATEST_UNRELATED_GOAL"));
        assert!(!objective.contains("LATEST_UNRELATED_RESULT"));
        assert_eq!(task["continued_from"], first);
    }
    #[test]
    fn latest_context_survives_event_window_eviction_and_snapshot_deserialization() {
        let (mut store, session) = setup();
        let task = running(&mut store, &session, "上下文");
        store
            .execution_event(
                &task,
                ExecutionEvent::ContextEstimate {
                    estimated_input_tokens: 123,
                    context_window: 4096,
                    compression_threshold: 80,
                    source: "native_local".into(),
                    phase: "request".into(),
                },
            )
            .unwrap();
        store
            .execution_event(
                &task,
                ExecutionEvent::ContextCompaction {
                    state: "reused".into(),
                },
            )
            .unwrap();
        for _ in 0..305 {
            store
                .execution_event(&task, ExecutionEvent::AssistantDelta { text: "x".into() })
                .unwrap();
        }
        let snapshot = store.snapshot().unwrap();
        assert!(!snapshot["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "context_estimate"
                || event["kind"] == "context_compaction"));
        let typed: harness_core::Snapshot = serde_json::from_value(snapshot).unwrap();
        let current = typed.tasks.iter().find(|item| item.id == task).unwrap();
        assert_eq!(
            current
                .context_estimate
                .as_ref()
                .unwrap()
                .estimated_input_tokens,
            123
        );
        assert_eq!(current.context_compaction.as_ref().unwrap().state, "reused");
        assert!(current.supplements.is_empty());
    }
    #[test]
    fn codex_continuation_accounts_for_applied_supplements_and_rejects_unknown_context() {
        use crate::codex::CodexLifecycle as L;
        let (mut store, session) = setup();
        let start = |store: &mut Store, goal: &str| {
            let settings = Settings {
                main_executor: "codex".into(),
                ..settings()
            };
            let receipt = store
                .command(
                    envelope(json!({"type":"run_agent","session_id":session,"content":goal})),
                    &settings,
                )
                .unwrap()
                .0;
            store.claim_task().unwrap();
            receipt["task_id"].as_str().unwrap().to_owned()
        };
        let begin = |store: &mut Store, task: &str, turn: &str| {
            for update in [
                L::ThreadReady {
                    thread_id: "thread".into(),
                },
                L::TurnStarting {
                    thread_id: "thread".into(),
                },
                L::TurnStarted {
                    thread_id: "thread".into(),
                    turn_id: turn.into(),
                },
            ] {
                store.codex_lifecycle(task, update).unwrap();
            }
        };
        let first = start(&mut store, "FIRST");
        assert!(store
            .prepare_codex_continuation(&first, "config")
            .unwrap()
            .is_none());
        begin(&mut store, &first, "turn1");
        send(
            &mut store,
            json!({"type":"steer_task","task_id":first,"content":"APPLIED_EXTRA_GOAL"}),
        );
        let inputs = store.claim_inputs(&first).unwrap();
        store
            .ack_inputs(
                &first,
                &[inputs[0].id.clone()],
                InputAcknowledgement::Applied {
                    via: "codex_turn_steer".into(),
                },
            )
            .unwrap();
        store
            .codex_lifecycle(
                &first,
                L::TurnCompleted {
                    thread_id: "thread".into(),
                    turn_id: "turn1".into(),
                },
            )
            .unwrap();
        store.finish(&first, Ok("FIRST_RESULT".into())).unwrap();
        let second = start(&mut store, "SECOND");
        assert_eq!(
            store.prepare_codex_continuation(&second, "config").unwrap(),
            Some(("thread".into(), "turn1".into()))
        );
        begin(&mut store, &second, "turn2");
        send(
            &mut store,
            json!({"type":"steer_task","task_id":second,"content":"NOT_APPLIED_EXTRA"}),
        );
        store
            .codex_lifecycle(
                &second,
                L::TurnCompleted {
                    thread_id: "thread".into(),
                    turn_id: "turn2".into(),
                },
            )
            .unwrap();
        store.finish(&second, Ok("SECOND_RESULT".into())).unwrap();
        let third = start(&mut store, "THIRD");
        assert!(store
            .prepare_codex_continuation(&third, "config")
            .unwrap()
            .is_none());
    }
    #[test]
    fn repeated_continuation_uses_fixed_origin_and_never_nests_generated_goals() {
        let (mut store, session) = setup();
        let origin = running(&mut store, &session, "SHORT_ORIGINAL_GOAL");
        store.finish(&origin, Ok("initial result".into())).unwrap();
        let mut previous = origin.clone();
        for round in 0..40 {
            let command = if round == 0 {
                json!({"type":"continue_task","task_id":previous,"content":"PRESERVED_USER_REQUIREMENT"})
            } else {
                json!({"type":"continue_task","task_id":previous})
            };
            let receipt = send(&mut store, command);
            let id = receipt["task_id"].as_str().unwrap().to_owned();
            let task = record(&store.db, "tasks", &id).unwrap();
            let objective = task["objective"].as_str().unwrap();
            assert!(
                objective.len() < 20 * 1024,
                "round {round} grew recursively"
            );
            assert!(objective.contains("SHORT_ORIGINAL_GOAL"));
            assert!(objective.contains("PRESERVED_USER_REQUIREMENT"));
            assert_eq!(task["continued_from"], previous);
            assert_eq!(task["continuation_origin"], origin);
            store.claim_task().unwrap();
            store
                .finish(&id, Ok("bounded result ".repeat(1500)))
                .unwrap();
            previous = id;
        }
    }
}
