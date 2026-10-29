use super::*;
fn settings() -> Settings {
    Settings {
        execution_mode: "native".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "fixture-not-called".into(),
        main_executor: "native".into(),
        api_key: "fixture-secret-canary".into(),
        ..Settings::default()
    }
}
fn owner(s: &mut Store, command: Value) -> Value {
    s.command(json!({"command_id":new_id(),"source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"local"},"command":command}),&settings()).unwrap().0
}
struct Fixture {
    store: Store,
    c: config::Config,
    channel: persistence::TrustedChannel,
    principal: String,
    session: String,
    grant: GatewayGrant,
}
impl Fixture {
    fn new() -> Self {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let session=owner(&mut store,json!({"type":"create_session","title":"私有会话 fixture-secret-canary","project_path":std::env::temp_dir()}))["session_id"].as_str().unwrap().to_string();
        let device = new_id();
        let create=store.gateway_admin_command(&json!({"id":new_id(),"command":{"type":"configure","config_id":null,"expected_revision":null,"origin":"https://fixture.invalid","device_id":device,"enabled":false}}),&settings(),true).unwrap();
        assert_eq!(create["status"], "completed");
        let c = store.gateway_config().unwrap().unwrap();
        let enabled=store.gateway_admin_command(&json!({"id":new_id(),"command":{"type":"configure","config_id":c.id,"expected_revision":c.revision,"origin":c.origin,"device_id":c.device_id,"enabled":true}}),&settings(),true).unwrap();
        assert_eq!(enabled["status"], "completed");
        let c = store.gateway_config().unwrap().unwrap();
        let channel = persistence::TrustedChannel {
            device_id: c.device_id.clone(),
            config_revision: c.revision,
        };
        let pairing = store
            .gateway_admin_command(
                &json!({"id":new_id(),"command":{"type":"issue_pairing","ttl_seconds":60}}),
                &settings(),
                true,
            )
            .unwrap();
        let principal = new_id();
        let request = GatewayRequest {
            request_id: new_id(),
            principal_id: principal.clone(),
            binding_revision: 0,
            rpc: GatewayRpc::PairClaim {
                code: pairing["pairing"]["code"].as_str().unwrap().into(),
                label: "测试手机".into(),
            },
        };
        let (reply, _) = store
            .gateway_handle(&c.id, &channel, &request, &settings())
            .unwrap();
        assert_eq!(reply.code, GatewayCode::Ok);
        assert_eq!(reply.data.unwrap()["status"], "pending");
        let grant = GatewayGrant {
            session_ids: vec![session.clone()],
            permissions: GatewayPermissions {
                view: true,
                read_results: true,
                read_artifacts: true,
                run: true,
                cancel: true,
                answer: true,
            },
            controls: json!({"budget":{"max_model_requests":32,"max_runtime_seconds":300,"max_retries":0,"max_repair_attempts":2,"max_output_bytes":32768,"max_external_calls":32},"permissions":{"read_files":true,"write_files":"ask","run_commands":"deny","managed_tools":"deny"},"acceptance_commands":[],"capabilities":{"mcp_servers":[],"skills":[]}}),
            expires_at_ms: millis() + 600000,
        };
        let receipt=store.gateway_admin_command(&json!({"id":new_id(),"command":{"type":"confirm_binding","principal_id":principal,"expected_revision":0,"expected_label":"测试手机","expected_pairing_id":pairing["pairing"]["id"],"grant":grant}}),&settings(),true).unwrap();
        assert_eq!(receipt["status"], "completed");
        Self {
            store,
            c,
            channel,
            principal,
            session,
            grant,
        }
    }
    fn request(&self, rpc: GatewayRpc) -> GatewayRequest {
        GatewayRequest {
            request_id: new_id(),
            principal_id: self.principal.clone(),
            binding_revision: 1,
            rpc,
        }
    }
    fn handle(&mut self, rpc: GatewayRpc) -> GatewayResult {
        let r = self.request(rpc);
        self.store
            .gateway_handle(&self.c.id, &self.channel, &r, &settings())
            .unwrap()
            .0
    }
    fn command(&mut self, command: GatewayCommand) -> GatewayResult {
        self.handle(GatewayRpc::Command {
            command_id: new_id(),
            session_id: self.session.clone(),
            command,
        })
    }
    fn run(&mut self) -> String {
        let result = self.command(GatewayCommand::Run {
            content: "实际本地夹具，未请求模型".into(),
        });
        assert_eq!(result.code, GatewayCode::Ok);
        result.data.unwrap()["task_id"].as_str().unwrap().into()
    }
    fn task(&self, id: &str) -> Value {
        record(&self.store.db, "tasks", id).unwrap()
    }
    fn put_task(&mut self, task: &Value) {
        let tx = self.store.db.transaction().unwrap();
        put(&tx, "tasks", task).unwrap();
        tx.commit().unwrap();
    }
    fn restrict_results(&mut self) {
        let tx = self.store.db.transaction().unwrap();
        let mut b = persistence::binding(&tx, &self.principal).unwrap();
        b.grant.as_mut().unwrap().permissions.read_results = false;
        b.grant.as_mut().unwrap().permissions.read_artifacts = false;
        tx.execute(
            "UPDATE gateway_bindings SET body=?1 WHERE principal_id=?2",
            params![serde_json::to_string(&b).unwrap(), self.principal],
        )
        .unwrap();
        tx.commit().unwrap();
    }
}
#[test]
fn real_transaction_keeps_source_controls_original_command_id_and_receipt() {
    let mut f = Fixture::new();
    let command = new_id();
    let req = f.request(GatewayRpc::Command {
        command_id: command.clone(),
        session_id: f.session.clone(),
        command: GatewayCommand::Run {
            content: "fixture".into(),
        },
    });
    let (result, event) = f
        .store
        .gateway_handle(&f.c.id, &f.channel, &req, &settings())
        .unwrap();
    assert_eq!(result.code, GatewayCode::Ok);
    assert!(event.is_some());
    let receipt = result.data.unwrap();
    let task = f.task(receipt["task_id"].as_str().unwrap());
    assert_eq!(task["source"]["channel_id"], "gateway");
    assert_eq!(task["source"]["actor_id"], f.principal);
    assert_eq!(
        task["source"]["conversation_id"],
        format!("{}:{}:1", f.c.id, f.c.revision)
    );
    assert!(check_source(&f.store.db, &task).is_ok());
    assert!(
        task["controls"]["budget"]["max_runtime_seconds"]
            .as_u64()
            .unwrap()
            <= f.grant.controls["budget"]["max_runtime_seconds"]
                .as_u64()
                .unwrap()
    );
    let (again, event) = f
        .store
        .gateway_handle(&f.c.id, &f.channel, &req, &settings())
        .unwrap();
    assert_eq!(again.data.unwrap(), receipt);
    assert!(event.is_none());
    assert_eq!(
        f.handle(GatewayRpc::Receipt {
            command_id: command
        })
        .data
        .unwrap(),
        receipt
    );
    assert_eq!(rows(&f.store.db, "tasks").unwrap().len(), 1);
}
#[test]
fn receipt_insert_failure_rolls_back_effect_messages_commands_and_events() {
    let mut f = Fixture::new();
    let before = f.store.event_cursor().unwrap();
    let session = record(&f.store.db, "sessions", &f.session).unwrap();
    f.store.db.execute_batch("CREATE TRIGGER fail_gateway_receipt BEFORE INSERT ON gateway_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let result = f.command(GatewayCommand::Run {
        content: "must roll back".into(),
    });
    assert_eq!(result.code, GatewayCode::Forbidden);
    assert!(rows(&f.store.db, "tasks").unwrap().is_empty());
    assert_eq!(
        record(&f.store.db, "sessions", &f.session).unwrap(),
        session
    );
    assert_eq!(f.store.event_cursor().unwrap(), before);
    assert_eq!(
        f.store
            .db
            .query_row("SELECT COUNT(*) FROM gateway_receipts", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn current_channel_and_cross_session_authority_are_not_browser_supplied() {
    let mut f = Fixture::new();
    let other = owner(
        &mut f.store,
        json!({"type":"create_session","title":"other","project_path":std::env::temp_dir()}),
    )["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        f.handle(GatewayRpc::Session {
            session_id: other.clone()
        })
        .code,
        GatewayCode::Forbidden
    );
    assert_eq!(
        f.handle(GatewayRpc::Command {
            command_id: new_id(),
            session_id: other,
            command: GatewayCommand::Run {
                content: "denied".into()
            }
        })
        .code,
        GatewayCode::Forbidden
    );
    let request = f.request(GatewayRpc::Sessions);
    let wrong = persistence::TrustedChannel {
        device_id: new_id(),
        config_revision: f.c.revision,
    };
    assert_eq!(
        f.store
            .gateway_handle(&f.c.id, &wrong, &request, &settings())
            .unwrap()
            .0
            .code,
        GatewayCode::Forbidden
    );
    assert_eq!(
        f.store
            .gateway_handle(&new_id(), &f.channel, &request, &settings())
            .unwrap()
            .0
            .code,
        GatewayCode::Forbidden
    );
}
#[test]
fn projection_redacts_current_secrets_and_no_results_means_no_prompt_or_evidence() {
    let mut f = Fixture::new();
    let id = f.run();
    let mut task = f.task(&id);
    task["title"] = json!("PRIVATE_PROMPT fixture-secret-canary");
    task["result"] = json!("PRIVATE_RESULT fixture-secret-canary");
    task["error"] = json!("PRIVATE_ERROR");
    task["evidence"] =
        json!([{"command":"SECRET_COMMAND","output":"PRIVATE_EVIDENCE","exit_code":0}]);
    f.put_task(&task);
    let visible = f.handle(GatewayRpc::Session {
        session_id: f.session.clone(),
    });
    assert_eq!(visible.code, GatewayCode::Ok);
    assert!(!serde_json::to_string(&visible)
        .unwrap()
        .contains("fixture-secret-canary"));
    f.restrict_results();
    for rpc in [
        GatewayRpc::Session {
            session_id: f.session.clone(),
        },
        GatewayRpc::Task {
            task_id: id.clone(),
        },
        GatewayRpc::Events {
            session_id: f.session.clone(),
            after: f.store.event_cursor().unwrap(),
        },
    ] {
        let result = f.handle(rpc);
        assert_eq!(result.code, GatewayCode::Ok);
        let raw = serde_json::to_string(&result).unwrap();
        for canary in [
            "PRIVATE_PROMPT",
            "PRIVATE_RESULT",
            "PRIVATE_ERROR",
            "SECRET_COMMAND",
            "PRIVATE_EVIDENCE",
            "fixture-secret-canary",
        ] {
            assert!(!raw.contains(canary), "leaked {canary}")
        }
    }
    assert_eq!(
        f.handle(GatewayRpc::Artifacts { task_id: id }).code,
        GatewayCode::Forbidden
    );
}
#[test]
fn question_answer_is_scoped_and_never_approves_actions() {
    let mut f = Fixture::new();
    let id = f.run();
    let mut task = f.task(&id);
    task["status"] = json!("running");
    f.put_task(&task);
    let question = f
        .store
        .create_decision_request(&id, "question", "请选择", None, 60)
        .unwrap();
    let projected = f
        .handle(GatewayRpc::Session {
            session_id: f.session.clone(),
        })
        .data
        .unwrap();
    assert_eq!(projected["questions"][0]["id"], question["id"]);
    let waiting = projected["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .unwrap();
    assert_eq!(waiting["waiting_request_id"], question["id"]);
    assert_eq!(waiting["status"], "waiting_input");
    let answer = GatewayCommand::Answer {
        request_id: question["id"].as_str().unwrap().into(),
        expected_revision: 0,
        answer: "fixture answer".into(),
    };
    let command_id = new_id();
    let request = f.request(GatewayRpc::Command {
        command_id,
        session_id: f.session.clone(),
        command: answer,
    });
    let result = f
        .store
        .gateway_handle(&f.c.id, &f.channel, &request, &settings())
        .unwrap()
        .0;
    assert_eq!(result.code, GatewayCode::Ok);
    assert_eq!(
        bounded_record(
            &f.store.db,
            "execution_requests",
            question["id"].as_str().unwrap()
        )
        .unwrap()["decision_actor"],
        format!("gateway:{}", f.principal)
    );
    assert_eq!(
        f.store
            .gateway_handle(&f.c.id, &f.channel, &request, &settings())
            .unwrap()
            .0
            .code,
        GatewayCode::Ok
    );
    let id = f.run();
    let mut task = f.task(&id);
    task["status"] = json!("running");
    f.put_task(&task);
    let approval = f
        .store
        .create_decision_request(
            &id,
            "approval",
            "不能远程批准",
            Some(json!({"kind":"write_file","secret":"APPROVAL_CANARY"})),
            60,
        )
        .unwrap();
    let result = f.command(GatewayCommand::Answer {
        request_id: approval["id"].as_str().unwrap().into(),
        expected_revision: 0,
        answer: "approve".into(),
    });
    assert_eq!(result.code, GatewayCode::Forbidden);
    let snapshot = f.handle(GatewayRpc::Session {
        session_id: f.session.clone(),
    });
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("APPROVAL_CANARY"));
}
#[test]
fn revocation_cancels_gateway_work_not_desktop_targets() {
    let mut f = Fixture::new();
    let remote = f.run();
    let desktop = owner(
        &mut f.store,
        json!({"type":"run_agent","session_id":f.session,"content":"desktop"}),
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let receipt=f.store.gateway_admin_command(&json!({"id":new_id(),"command":{"type":"revoke_binding","principal_id":f.principal,"expected_revision":1}}),&settings(),true).unwrap();
    assert_eq!(receipt["status"], "completed");
    assert_eq!(f.task(&remote)["status"], "cancelled");
    assert_eq!(f.task(&remote)["stop_reason"], "remote_permission_revoked");
    assert_ne!(f.task(&desktop)["status"], "cancelled");
    assert!(check_source(&f.store.db, &f.task(&remote)).is_err());
    assert!(check_source(&f.store.db, &f.task(&desktop)).is_ok());
}
#[test]
fn cancelling_desktop_task_does_not_claim_its_source_even_after_executor_change() {
    let mut f = Fixture::new();
    let id = owner(
        &mut f.store,
        json!({"type":"run_agent","session_id":f.session,"content":"desktop"}),
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let req = f.request(GatewayRpc::Command {
        command_id: new_id(),
        session_id: f.session.clone(),
        command: GatewayCommand::Cancel {
            task_id: id.clone(),
        },
    });
    let mut settings = settings();
    settings.main_executor = "codex".into();
    assert_eq!(
        f.store
            .gateway_handle(&f.c.id, &f.channel, &req, &settings)
            .unwrap()
            .0
            .code,
        GatewayCode::Ok
    );
    assert_eq!(f.task(&id)["source"]["channel_id"], "desktop");
}
#[test]
fn artifacts_are_terminal_immutable_acl_checked_and_redacted_on_read() {
    let mut f = Fixture::new();
    let id = f.run();
    assert!(f
        .handle(GatewayRpc::Artifacts {
            task_id: id.clone()
        })
        .data
        .unwrap()["artifacts"]
        .as_array()
        .unwrap()
        .is_empty());
    let mut task = f.task(&id);
    task["status"] = json!("completed");
    task["result"] = json!("persisted fixture-secret-canary");
    f.put_task(&task);
    let list = f
        .handle(GatewayRpc::Artifacts {
            task_id: id.clone(),
        })
        .data
        .unwrap();
    let artifact = list["artifacts"][0]["id"].as_str().unwrap().to_string();
    let text = f.handle(GatewayRpc::Artifact {
        resource_id: artifact.clone(),
    });
    assert_eq!(text.code, GatewayCode::Ok);
    assert!(!serde_json::to_string(&text)
        .unwrap()
        .contains("fixture-secret-canary"));
    task["result"] = json!("new content not original");
    f.put_task(&task);
    f.handle(GatewayRpc::Artifacts { task_id: id });
    assert!(f
        .handle(GatewayRpc::Artifact {
            resource_id: artifact.clone()
        })
        .data
        .unwrap()["artifact"]["content"]
        .as_str()
        .unwrap()
        .contains("persisted"));
    let tx = f.store.db.transaction().unwrap();
    restore(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        f.handle(GatewayRpc::Artifact {
            resource_id: artifact
        })
        .code,
        GatewayCode::Forbidden
    );
}
#[test]
fn restore_rolls_back_or_disables_without_rearming_credentials() {
    let mut f = Fixture::new();
    {
        let tx = f.store.db.transaction().unwrap();
        restore(&tx).unwrap();
        assert!(!config::current(&tx).unwrap().unwrap().enabled);
        tx.rollback().unwrap();
    }
    assert_eq!(f.handle(GatewayRpc::Sessions).code, GatewayCode::Ok);
    let tx = f.store.db.transaction().unwrap();
    restore(&tx).unwrap();
    recover(&tx).unwrap();
    tx.commit().unwrap();
    assert!(!f.store.gateway_config().unwrap().unwrap().enabled);
    assert_eq!(f.handle(GatewayRpc::Sessions).code, GatewayCode::Forbidden);
}

#[test]
fn accepted_desktop_inputs_keep_provenance_and_controls_after_revoke() {
    let mut f = Fixture::new();
    let desktop=owner(&mut f.store,json!({"type":"run_agent","session_id":f.session,"content":"desktop under explicit controls","controls":f.grant.controls}))["task_id"].as_str().unwrap().to_string();
    let mut task = f.task(&desktop);
    task["status"] = json!("running");
    f.put_task(&task);
    let original_source = task["source"].clone();
    let original_controls = task["controls"].clone();
    let steer_id = new_id();
    let steer = f.request(GatewayRpc::Command {
        command_id: steer_id.clone(),
        session_id: f.session.clone(),
        command: GatewayCommand::Steer {
            task_id: desktop.clone(),
            content: "ACCEPTED_BEFORE_REVOKE".into(),
        },
    });
    assert_eq!(
        f.store
            .gateway_handle(&f.c.id, &f.channel, &steer, &settings())
            .unwrap()
            .0
            .code,
        GatewayCode::Ok
    );
    let question = f
        .store
        .create_decision_request(&desktop, "question", "独立桌面任务问题", None, 60)
        .unwrap();
    assert_eq!(
        f.command(GatewayCommand::Answer {
            request_id: question["id"].as_str().unwrap().into(),
            expected_revision: 0,
            answer: "ACCEPTED_ANSWER".into()
        })
        .code,
        GatewayCode::Ok
    );
    f.store.gateway_admin_command(&json!({"id":new_id(),"command":{"type":"revoke_binding","principal_id":f.principal,"expected_revision":1}}),&settings(),true).unwrap();
    let after = f.task(&desktop);
    assert_eq!(after["source"], original_source);
    assert_eq!(after["controls"], original_controls);
    assert_eq!(after["status"], "waiting_input");
    assert!(after["supplements"]
        .to_string()
        .contains("ACCEPTED_BEFORE_REVOKE"));
    assert!(after["question_answers"]
        .to_string()
        .contains("ACCEPTED_ANSWER"));
    assert!(check_source(&f.store.db, &after).is_ok());
    let envelope: String = f
        .store
        .db
        .query_row(
            "SELECT envelope FROM commands WHERE id=?1",
            [steer_id],
            |r| r.get(0),
        )
        .unwrap();
    let envelope: Value = serde_json::from_str(&envelope).unwrap();
    assert_eq!(envelope["source"]["actor_id"], f.principal);
    assert_eq!(envelope["source"]["channel_id"], "gateway");
    assert_eq!(
        f.command(GatewayCommand::Steer {
            task_id: desktop,
            content: "AFTER_REVOKE_REJECTED".into()
        })
        .code,
        GatewayCode::Forbidden
    );
    assert!(!f.task(after["id"].as_str().unwrap())["supplements"]
        .to_string()
        .contains("AFTER_REVOKE_REJECTED"));
}
#[test]
fn inputs_cannot_target_desktop_tasks_above_grant_ceiling() {
    let mut f = Fixture::new();
    let id = owner(
        &mut f.store,
        json!({"type":"run_agent","session_id":f.session,"content":"broader desktop"}),
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut task = f.task(&id);
    task["status"] = json!("running");
    task["controls"]["budget"]["max_runtime_seconds"] = json!(900);
    f.put_task(&task);
    assert_eq!(
        f.command(GatewayCommand::Steer {
            task_id: id.clone(),
            content: "must not inject".into()
        })
        .code,
        GatewayCode::Forbidden
    );
    assert_eq!(f.task(&id)["controls"], task["controls"]);
}

#[test]
fn credential_store_uses_exact_gateway_scope_and_recovery_never_replays() {
    use crate::capabilities::vault::{FreshCredential, SecretBytes, VaultBinding};
    let mut f = Fixture::new();
    let binding = crate::gateway::credential_binding(&f.c.id, &f.c.scope_digest).unwrap();
    let fresh = FreshCredential::new(
        &binding,
        SecretBytes::new(b"simulated-never-written-secret".to_vec()).unwrap(),
    );
    let reference = fresh.reference();
    let op = new_id();
    let receipt = f
        .store
        .gateway_begin_set(&op, &f.c.id, f.c.revision, reference)
        .unwrap();
    assert!(!receipt.to_string().contains(reference.as_str()));
    assert_eq!(
        f.store
            .gateway_existing_set(&op, &f.c.id, f.c.revision)
            .unwrap()
            .unwrap()["no_secret_applied"],
        true
    );
    let claimed = f.store.gateway_claim_credential(&op).unwrap().unwrap();
    assert_eq!(claimed.operation_id, op);
    {
        let tx = f.store.db.transaction().unwrap();
        recover(&tx).unwrap();
        tx.commit().unwrap();
    }
    assert!(f.store.gateway_claim_credential(&op).unwrap().is_none());
    let view = f.store.gateway_credential_operation(&op).unwrap().unwrap();
    let view = serde_json::to_value(view).unwrap();
    assert_eq!(view["owner_kind"], "gateway");
    assert_eq!(view["status"], "unknown");
    assert!(!view.to_string().contains(reference.as_str()));
    let foreign = VaultBinding::service(&f.c.id, "qq_client_secret", &f.c.scope_digest).unwrap();
    let other = FreshCredential::new(&foreign, SecretBytes::new(b"fixture".to_vec()).unwrap());
    assert!(f
        .store
        .gateway_begin_set(&new_id(), &f.c.id, f.c.revision, other.reference())
        .is_err());
    let tx = f.store.db.transaction().unwrap();
    restore(&tx).unwrap();
    tx.commit().unwrap();
    assert!(f.store.gateway_begin_cleanup(&new_id(), &op).is_err());
}

#[test]
fn worst_case_json_escaping_never_exceeds_wire_limit_or_leaks_denied_results() {
    let mut f = Fixture::new();
    let id = f.run();
    let mut task = f.task(&id);
    task["status"] = json!("completed");
    let text = format!(
        "{}fixture-secret-canary PRIVATE_RESULT",
        "\u{0001}".repeat(70_000)
    );
    for field in ["result", "stream_output", "error"] {
        task[field] = json!(text);
    }
    f.put_task(&task);
    for _ in 0..19 {
        let mut next = task.clone();
        next["id"] = json!(new_id());
        let tx = f.store.db.transaction().unwrap();
        put(&tx, "tasks", &next).unwrap();
        tx.commit().unwrap();
    }
    let response = f.handle(GatewayRpc::Session {
        session_id: f.session.clone(),
    });
    assert!(matches!(response.code, GatewayCode::Busy | GatewayCode::Ok));
    assert!(serde_json::to_vec(&response).unwrap().len() < GATEWAY_MAX_FRAME_BYTES);
    assert!(!serde_json::to_string(&response)
        .unwrap()
        .contains("fixture-secret-canary"));
    let list = f.handle(GatewayRpc::Artifacts {
        task_id: id.clone(),
    });
    assert_eq!(list.code, GatewayCode::Ok);
    let listed = list.data.unwrap();
    assert_eq!(listed["artifacts"][0]["truncated"], true);
    let resource = listed["artifacts"][0]["id"].as_str().unwrap().to_string();
    let artifact = f.handle(GatewayRpc::Artifact {
        resource_id: resource,
    });
    assert_eq!(artifact.code, GatewayCode::Busy);
    assert!(serde_json::to_vec(&artifact).unwrap().len() < GATEWAY_MAX_FRAME_BYTES);
    f.restrict_results();
    let denied = f.handle(GatewayRpc::Session {
        session_id: f.session.clone(),
    });
    assert_eq!(denied.code, GatewayCode::Ok);
    let raw = serde_json::to_string(&denied).unwrap();
    assert!(raw.len() < GATEWAY_MAX_FRAME_BYTES);
    assert!(!raw.contains("PRIVATE_RESULT"));
    assert!(!raw.contains("\\u0001"));
}
#[test]
fn tampered_artifact_only_grant_cannot_bypass_result_permission() {
    let mut f = Fixture::new();
    let id = f.run();
    let tx = f.store.db.transaction().unwrap();
    let mut b = persistence::binding(&tx, &f.principal).unwrap();
    b.grant.as_mut().unwrap().permissions.read_results = false;
    b.grant.as_mut().unwrap().permissions.read_artifacts = true;
    tx.execute(
        "UPDATE gateway_bindings SET body=?1 WHERE principal_id=?2",
        params![serde_json::to_string(&b).unwrap(), f.principal],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        f.handle(GatewayRpc::Artifacts { task_id: id }).code,
        GatewayCode::Forbidden
    );
}

#[test]
fn gateway_completion_persists_result_and_event_without_fake_outbound_delivery() {
    let mut f = Fixture::new();
    let id = f.run();
    let event = f
        .store
        .finish(&id, Ok("已持久化的拉取结果".into()))
        .unwrap()
        .unwrap();
    assert_eq!(event.kind, "task_completed");
    assert_eq!(f.task(&id)["result"], "已持久化的拉取结果");
    assert!(rows(&f.store.db, "deliveries").unwrap().is_empty());
    let result = f.handle(GatewayRpc::Task { task_id: id });
    assert_eq!(result.code, GatewayCode::Ok);
    assert!(serde_json::to_string(&result.data)
        .unwrap()
        .contains("已持久化的拉取结果"));
    let session = record(&f.store.db, "sessions", &f.session).unwrap();
    assert!(serde_json::to_string(&session)
        .unwrap()
        .contains("已持久化的拉取结果"));
}
