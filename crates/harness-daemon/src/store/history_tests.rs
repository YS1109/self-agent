use super::*;

fn store() -> Store {
    Store::open(Path::new(":memory:")).unwrap()
}
fn epoch() -> String {
    new_id()
}
fn session(db: &Connection, id: &str, messages: Vec<Value>) {
    put(db,"sessions",&json!({"id":id,"title":"历史会话","project_path":"/fixture","created_at":"2026-01-01","updated_at":"2026-01-01","messages":messages})).unwrap();
}
fn task_value(id: &str, session_id: &str, status: &str, result: &str) -> Value {
    json!({"id":id,"session_id":session_id,"title":"任务","status":status,"execution_mode":"native","task_kind":"main","created_at":"same-time","updated_at":"same-time","source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"main"},"steps":[],"result":result,"stream_output":"完整流式前缀","evidence":[{"command":"fixture","exit_code":0,"output":"完整证据"}],"supplements":[]})
}
fn message(index: usize, text: &str) -> Value {
    json!({"id":format!("message-{index}"),"role":"user","content":text,"created_at":"same-time"})
}
fn check_page(page: &Value) {
    assert!(encoded_size(page).unwrap() <= PAGE_BYTES);
    let items = page["items"].as_array().unwrap();
    let unavailable = page["unavailable"].as_array().unwrap();
    assert_eq!(page["returned"], items.len());
    assert!(items.len() + unavailable.len() <= PAGE_COUNT);
    assert_eq!(page["has_more"], !page["next_cursor"].is_null());
    if items.is_empty() && page["has_more"] == true {
        assert!(!unavailable.is_empty());
    }
}
fn code(error: anyhow::Error) -> &'static str {
    error
        .downcast_ref::<HistoryError>()
        .expect("typed error")
        .code()
}
#[test]
fn five_thousand_records_are_bounded_and_keyset_pages_stay_complete_during_insert() {
    let mut store = store();
    let epoch = epoch();
    session(&store.db, "s", vec![]);
    let tx = store.db.transaction().unwrap();
    for n in 0..5000 {
        put(
            &tx,
            "tasks",
            &task_value(&format!("task-{n}"), "s", "completed", &"历史".repeat(1400)),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let snapshot = store.windowed_snapshot(&epoch).unwrap();
    assert!(encoded_size(&snapshot).unwrap() < SNAPSHOT_BYTES);
    assert_eq!(snapshot["history"]["collections"]["tasks"]["total"], 5000);
    assert_eq!(snapshot["tasks"].as_array().unwrap().len(), 80);
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut anchor = None;
    let mut pages = 0;
    loop {
        let page = store
            .history_page(&epoch, "tasks", Some("s"), cursor.as_deref())
            .unwrap();
        assert_eq!(page["anchor"], epoch);
        check_page(&page);
        assert_eq!(page["total"], 5000);
        if let Some(anchor) = &anchor {
            assert_eq!(anchor, &page["anchor"]);
        } else {
            anchor = Some(page["anchor"].clone());
        }
        for item in page["items"].as_array().unwrap() {
            assert!(seen.insert(item["id"].as_str().unwrap().to_owned()));
        }
        pages += 1;
        if pages == 1 {
            put(
                &store.db,
                "tasks",
                &task_value("new-after-anchor", "s", "completed", "new"),
            )
            .unwrap();
            // A metadata update does not move an older record across the keyset.
            put(
                &store.db,
                "tasks",
                &task_value("task-0", "s", "completed", "updated"),
            )
            .unwrap();
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
        assert!(pages < 100);
    }
    assert_eq!(seen.len(), 5000);
    assert!(!seen.contains("new-after-anchor"));
    assert!(store
        .history_page(&epoch, "tasks", Some("s"), None)
        .unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["id"] == "new-after-anchor"));
}
#[test]
fn oversized_records_are_explicit_and_cursor_advances_without_clipping() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "s", vec![]);
    for n in 0..81 {
        put(
            &store.db,
            "tasks",
            &task_value(&format!("task-{n}"), "s", "completed", "small"),
        )
        .unwrap();
    }
    put(
        &store.db,
        "tasks",
        &task_value(
            "exceeds-page",
            "s",
            "completed",
            &"x".repeat(3 * 1024 * 1024),
        ),
    )
    .unwrap();
    put(
        &store.db,
        "tasks",
        &task_value(
            "exceeds-record",
            "s",
            "completed",
            &"x".repeat(RECORD_BYTES + 1),
        ),
    )
    .unwrap();
    let page = store.history_page(&epoch, "tasks", None, None).unwrap();
    check_page(&page);
    assert_eq!(page["unavailable"].as_array().unwrap().len(), 2);
    assert_eq!(page["unavailable"][0]["id"], "exceeds-record");
    assert_eq!(page["unavailable"][0]["reason"], "record_too_large");
    assert_eq!(page["items"].as_array().unwrap().len(), 78);
    let next = store
        .history_page(&epoch, "tasks", None, page["next_cursor"].as_str())
        .unwrap();
    check_page(&next);
    assert_eq!(next["items"].as_array().unwrap().len(), 3);
    assert_eq!(next["has_more"], false);
    assert_eq!(
        code(store.bounded_task_detail("exceeds-record").unwrap_err()),
        "record_too_large"
    );
    let detail = store.bounded_task_detail("exceeds-page").unwrap();
    assert_eq!(
        detail["task"]["result"].as_str().unwrap().len(),
        3 * 1024 * 1024
    );
    assert_eq!(detail["task"]["stream_output"], "完整流式前缀");
    assert_eq!(detail["task"]["evidence"][0]["output"], "完整证据");
    assert_eq!(
        code(store.bounded_task_detail("not-here").unwrap_err()),
        "not_found"
    );
}
#[test]
fn unicode_and_json_escaping_are_measured_on_the_wire_not_string_length() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "s", vec![]);
    let text = "中\n\u{1}\\\"".repeat(8000);
    for n in 0..100 {
        put(
            &store.db,
            "tasks",
            &task_value(&format!("task-{n}"), "s", "completed", &text),
        )
        .unwrap();
    }
    let mut count = 0;
    let mut cursor = None;
    loop {
        let page = store
            .history_page(&epoch, "tasks", None, cursor.as_deref())
            .unwrap();
        check_page(&page);
        for task in page["items"].as_array().unwrap() {
            assert_eq!(task["result"], text);
            count += 1;
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(count, 100);
}
#[test]
fn cursors_bind_epoch_collection_scope_and_reject_unknown_fields_or_unsafe_ids() {
    let store = store();
    session(&store.db, "s", vec![]);
    let epoch = epoch();
    for n in 0..81 {
        put(
            &store.db,
            "tasks",
            &task_value(&format!("task-{n}"), "s", "completed", "x"),
        )
        .unwrap();
    }
    let page = store
        .history_page(&epoch, "tasks", Some("s"), None)
        .unwrap();
    let cursor = page["next_cursor"].as_str().unwrap();
    for (epoch, kind, scope) in [
        (new_id(), "tasks", Some("s")),
        (epoch.clone(), "plans", Some("s")),
        (epoch.clone(), "tasks", None),
    ] {
        assert_eq!(
            code(
                store
                    .history_page(&epoch, kind, scope, Some(cursor))
                    .unwrap_err()
            ),
            "history_reset"
        );
    }
    for cursor in ["a", "../", "{}", &"a".repeat(2050)] {
        assert_eq!(
            code(
                store
                    .history_page(&epoch, "tasks", Some("s"), Some(cursor))
                    .unwrap_err()
            ),
            "history_reset"
        );
    }
    let mut forged =
        serde_json::to_value(Cursor::decode(cursor, &epoch, "tasks", Some("s")).unwrap()).unwrap();
    forged["extra"] = json!(true);
    let encoded = serde_json::to_vec(&forged)
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(
        code(
            store
                .history_page(&epoch, "tasks", Some("s"), Some(&encoded))
                .unwrap_err()
        ),
        "history_reset"
    );
    for id in ["../s", "s?x", "s/x", "", &"x".repeat(129)] {
        assert_eq!(
            code(store.history_messages(&epoch, id, None).unwrap_err()),
            "invalid_history_query"
        );
    }
}
#[test]
fn message_windows_use_absolute_indices_and_fixed_upper_without_full_session_decode() {
    let store = store();
    let epoch = epoch();
    let messages = (0..220)
        .map(|n| message(n, &"长历史".repeat(20_000)))
        .collect::<Vec<_>>();
    session(&store.db, "s", messages.clone());
    let body_bytes: u64 = store
        .db
        .query_row(
            "SELECT length(CAST(body AS BLOB)) FROM sessions WHERE id='s'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(body_bytes > RECORD_BYTES as u64);
    let header = store.history_page(&epoch, "sessions", None, None).unwrap();
    check_page(&header);
    assert_eq!(header["items"][0]["messages"], json!([]));
    assert_eq!(header["session_messages"]["s"]["total"], 220);
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut first = true;
    loop {
        let page = store
            .history_messages(&epoch, "s", cursor.as_deref())
            .unwrap();
        assert_eq!(page["anchor"], epoch);
        check_page(&page);
        assert_eq!(page["total"], 220);
        let mut previous = None;
        for item in page["items"].as_array().unwrap() {
            let index = item["history_offset"].as_u64().unwrap();
            assert!(previous.is_none_or(|previous| previous < index));
            previous = Some(index);
            assert!(seen.insert(index));
            assert_eq!(item["content"], messages[index as usize]["content"]);
        }
        if first {
            let mut appended = messages.clone();
            appended.push(message(220, "new"));
            session(&store.db, "s", appended);
            first = false;
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 220);
    assert!(!seen.contains(&220));
}
#[test]
fn oversized_message_pages_can_be_empty_but_make_explicit_progress() {
    let store = store();
    let epoch = epoch();
    // 80 modest >page-budget messages would consume excessive fixture memory;
    // a small direct window proves the same no-loop rule with oversized values.
    session(
        &store.db,
        "s",
        vec![message(0, "old"), message(1, &"x".repeat(3 * 1024 * 1024))],
    );
    let page = store.history_messages(&epoch, "s", None).unwrap();
    check_page(&page);
    assert_eq!(page["unavailable"][0]["offset"], 1);
    assert_eq!(page["items"][0]["history_offset"], 0);
    assert_eq!(page["has_more"], false);
    let (items, missing, before) =
        message_window(&store.db, "s", 2, 1, PAGE_BYTES - ENVELOPE_RESERVE).unwrap();
    assert!(items.is_empty());
    assert_eq!(missing.len(), 1);
    assert_eq!(before, 1);
}
fn request(db: &Connection, id: &str, task: &str, status: &str, prompt: &str) {
    let body = json!({"id":id,"task_id":task,"session_id":"pinned","kind":"question","prompt":prompt,"action":null,"action_digest":null,"revision":0,"status":status,"answer":null,"decision_actor":null,"expires_at_ms":1,"created_at":"same-time","updated_at":"same-time","consumed_at":null,"reason":"等待"});
    db.execute(
        "INSERT INTO execution_requests(id,task_id,body) VALUES(?1,?2,?3)",
        params![id, task, body.to_string()],
    )
    .unwrap();
}
#[test]
fn active_tasks_selected_session_and_pending_decisions_are_not_hidden_by_new_history() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "pinned", vec![message(0, "selected")]);
    select(
        &store.db,
        &json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"main"}),
        "pinned",
    )
    .unwrap();
    put(
        &store.db,
        "tasks",
        &task_value("active", "pinned", "waiting_input", "running"),
    )
    .unwrap();
    put(
        &store.db,
        "tasks",
        &task_value(
            "oversized-active",
            "pinned",
            "running",
            &"x".repeat(RECORD_BYTES + 1),
        ),
    )
    .unwrap();
    request(&store.db, "pending", "active", "pending", "明确等待回答");
    for n in 0..100 {
        session(&store.db, &format!("s-{n}"), vec![]);
        put(
            &store.db,
            "tasks",
            &task_value(&format!("done-{n}"), &format!("s-{n}"), "completed", "done"),
        )
        .unwrap();
        request(
            &store.db,
            &format!("answered-{n}"),
            "active",
            "answered",
            "old",
        );
    }
    let snapshot = store.windowed_snapshot(&epoch).unwrap();
    assert!(encoded_size(&snapshot).unwrap() <= SNAPSHOT_BYTES);
    assert_eq!(snapshot["sessions"][0]["id"], "pinned");
    assert_eq!(snapshot["selections"][0]["session_id"], "pinned");
    assert_eq!(snapshot["execution_requests"][0]["id"], "pending");
    assert!(snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["id"] == "active"));
    let meta = &snapshot["history"]["collections"]["tasks"];
    assert_eq!(meta["pending"], 1);
    assert!(meta["omitted"].as_u64().unwrap() > 0);
    assert_eq!(meta["unavailable"][0]["id"], "oversized-active");
    assert_eq!(
        snapshot["history"]["collections"]["sessions"]["pinned_pending"],
        0
    );
}
#[test]
fn typed_projection_hides_private_fields_and_detail_events_share_authoritative_cursor() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "s", vec![]);
    let mut task = task_value("task", "s", "completed", "authoritative result");
    task["credential_ref"] = json!("private-ref-canary");
    task["provider_snapshot"] = json!({"secret":"private-ref-canary"});
    put(&store.db, "tasks", &task).unwrap();
    put(&store.db,"session_contexts",&json!({"id":"s:native","session_id":"s","executor":"native","state":"available","reason":null,"summary":{"summary":"private-summary-canary","covered_messages":3,"prefix_fingerprint":"private-ref-canary"},"summary_source_task_id":"task","updated_at":"same-time"})).unwrap();
    for n in 0..100 {
        persist_event(
            &store.db,
            &event(
                Some("s".into()),
                Some("task".into()),
                "task_progress",
                &format!("event-{n}"),
            ),
        )
        .unwrap();
    }
    persist_event(
        &store.db,
        &event(
            Some("s".into()),
            Some("other".into()),
            "task_progress",
            "unrelated",
        ),
    )
    .unwrap();
    let detail = store.bounded_task_detail("task").unwrap();
    assert_eq!(detail["event_cursor"], 101);
    assert_eq!(detail["history"]["events"]["total"], 100);
    assert_eq!(detail["history"]["events"]["returned"], 80);
    assert_eq!(detail["history"]["events"]["has_more"], true);
    assert_eq!(detail["events"][0]["sequence"], 21);
    assert_eq!(detail["events"][79]["sequence"], 100);
    assert_eq!(detail["task"]["result"], "authoritative result");
    assert!(!detail.to_string().contains("private-ref-canary"));
    let snapshot = store.windowed_snapshot(&epoch).unwrap();
    assert_eq!(snapshot["event_cursor"], 101);
    assert!(!snapshot.to_string().contains("private-ref-canary"));
    assert!(!snapshot.to_string().contains("private-summary-canary"));
    assert_eq!(snapshot["context_states"][0]["covered_messages"], 3);
}
#[test]
fn scope_filter_never_returns_other_sessions_and_missing_is_not_oversize() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "legacy_id", vec![]);
    session(&store.db, "other", vec![]);
    put(
        &store.db,
        "tasks",
        &task_value("task", "legacy_id", "completed", "mine"),
    )
    .unwrap();
    put(
        &store.db,
        "tasks",
        &task_value("other-task", "other", "completed", "other"),
    )
    .unwrap();
    let page = store
        .history_page(&epoch, "tasks", Some("legacy_id"), None)
        .unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["id"], "task");
    assert_eq!(
        code(store.history_messages(&epoch, "missing", None).unwrap_err()),
        "not_found"
    );
    assert_eq!(
        code(
            store
                .history_page(&epoch, "settings", None, None)
                .unwrap_err()
        ),
        "invalid_history_query"
    );
}

#[test]
fn snapshot_and_detail_hard_caps_preserve_complete_records_and_report_omissions() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "pinned", vec![]);
    select(
        &store.db,
        &json!({"channel_id":"desktop","actor_id":"owner","conversation_id":"main"}),
        "pinned",
    )
    .unwrap();
    for n in 0..40 {
        session(
            &store.db,
            &format!("session-{n}"),
            vec![message(n, &"m".repeat(60 * 1024))],
        );
    }
    for n in 0..12 {
        put(
            &store.db,
            "tasks",
            &task_value(
                &format!("active-{n}"),
                "pinned",
                "running",
                &"r".repeat(750 * 1024),
            ),
        )
        .unwrap();
    }
    for n in 0..5 {
        request(
            &store.db,
            &format!("pending-{n}"),
            "active-0",
            "pending",
            &"p".repeat(600 * 1024),
        );
    }
    for n in 0..4 {
        put(&store.db,"plans",&json!({"id":format!("plan-{n}"),"session_id":"pinned","title":"计划","source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"main"},"execution_mode":"native","status":"running","revision":1,"round":1,"max_rounds":3,"created_at":"same-time","updated_at":"same-time","nodes":[],"summary":"s".repeat(750*1024)})).unwrap();
    }
    let snapshot = store.windowed_snapshot(&epoch).unwrap();
    let bytes = encoded_size(&snapshot).unwrap();
    assert!(bytes > 8 * 1024 * 1024 && bytes <= SNAPSHOT_BYTES);
    assert!(
        snapshot["history"]["collections"]["execution_requests"]["pending"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        snapshot["history"]["collections"]["tasks"]["pending"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["id"] == "pinned"));
    for task in snapshot["tasks"].as_array().unwrap() {
        assert_eq!(task["result"].as_str().unwrap().len(), 750 * 1024);
        assert_eq!(task["stream_output"], "完整流式前缀");
    }
    put(
        &store.db,
        "tasks",
        &task_value(
            "near-cap",
            "pinned",
            "completed",
            &"x".repeat(7 * 1024 * 1024),
        ),
    )
    .unwrap();
    for n in 0..80 {
        persist_event(
            &store.db,
            &event(
                Some("pinned".into()),
                Some("near-cap".into()),
                "task_progress",
                &format!("{n}{}", "e".repeat(31 * 1024)),
            ),
        )
        .unwrap();
    }
    let detail = store.bounded_task_detail("near-cap").unwrap();
    assert!(encoded_size(&detail).unwrap() <= DETAIL_BYTES);
    assert_eq!(
        detail["task"]["result"].as_str().unwrap().len(),
        7 * 1024 * 1024
    );
    assert_eq!(detail["history"]["events"]["total"], 80);
    assert_eq!(detail["history"]["events"]["has_more"], true);
    assert!(!detail["history"]["events"]["unavailable"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn escaped_legacy_identifiers_count_toward_unavailable_page_budget() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "s", vec![]);
    let prefix = "\u{1}".repeat(125);
    for n in 0..16 {
        put(
            &store.db,
            "tasks",
            &task_value(
                &format!("{prefix}{n}"),
                "s",
                "completed",
                &"x".repeat(3 * 1024 * 1024),
            ),
        )
        .unwrap();
    }
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut pages = 0;
    loop {
        let page = store
            .history_page(&epoch, "tasks", None, cursor.as_deref())
            .unwrap();
        check_page(&page);
        assert!(page["items"].as_array().unwrap().is_empty());
        for missing in page["unavailable"].as_array().unwrap() {
            assert!(seen.insert(missing["id"].as_str().unwrap().to_owned()));
        }
        pages += 1;
        assert!(pages <= 3);
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 16);
    assert!(pages >= 1);
}
#[test]
fn a_corrupt_oversized_identifier_is_rejected_without_unbounded_rust_read() {
    let store = store();
    let epoch = epoch();
    // The SQL guard covers identifiers too, not only body. There is no honest
    // original-ID unavailable entry that would fit this interface's page cap.
    let id = "x".repeat(RECORD_BYTES + 1);
    store
        .db
        .execute("INSERT INTO tasks(id,body) VALUES(?1,'{}')", [id])
        .unwrap();
    assert_eq!(
        code(store.history_page(&epoch, "tasks", None, None).unwrap_err()),
        "record_too_large"
    );
}

#[test]
fn oldest_local_owner_selections_and_session_headers_outrank_remote_selection_flood() {
    let store = store();
    let epoch = epoch();
    for (channel, actor, id) in [
        ("desktop", "owner", "desktop-old"),
        ("cli", "local", "cli-old"),
        ("simulator", "owner", "simulator-old"),
    ] {
        session(&store.db, id, vec![message(0, "本机选中会话")]);
        select(
            &store.db,
            &json!({"channel_id":channel,"actor_id":actor,"conversation_id":"main"}),
            id,
        )
        .unwrap();
    }
    for n in 0..120 {
        let id = format!("remote-{n}");
        session(&store.db, &id, vec![]);
        select(
            &store.db,
            &json!({"channel_id":"qq","actor_id":"owner","conversation_id":id}),
            &id,
        )
        .unwrap();
    }
    let snapshot = store.windowed_snapshot(&epoch).unwrap();
    for (index, id) in ["desktop-old", "cli-old", "simulator-old"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(snapshot["selections"][index]["session_id"], id);
        assert_eq!(snapshot["sessions"][index]["id"], id);
        assert_eq!(snapshot["history"]["session_messages"][id]["total"], 1);
        assert_eq!(snapshot["history"]["session_messages"][id]["returned"], 1);
    }
    assert_eq!(
        snapshot["history"]["collections"]["selections"]["total"],
        123
    );
    assert_eq!(
        snapshot["history"]["collections"]["selections"]["returned"],
        80
    );
    assert_eq!(
        snapshot["history"]["collections"]["selections"]["omitted"],
        43
    );
    assert_eq!(
        snapshot["history"]["collections"]["sessions"]["pinned_pending"],
        43
    );
}

#[test]
fn display_identifiers_allow_legacy_unicode_but_never_exceed_client_codepoint_limit() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "s", vec![]);
    let legacy = "😀".repeat(128);
    put(
        &store.db,
        "tasks",
        &task_value(&legacy, "s", "completed", "visible only"),
    )
    .unwrap();
    let page = store.history_page(&epoch, "tasks", None, None).unwrap();
    assert_eq!(page["items"][0]["id"], legacy);
    assert!(!safe_history_id(&legacy));
    let too_long = "x".repeat(129);
    put(
        &store.db,
        "tasks",
        &task_value(&too_long, "s", "completed", "cannot output invalid wire id"),
    )
    .unwrap();
    assert_eq!(
        code(store.history_page(&epoch, "tasks", None, None).unwrap_err()),
        "record_too_large"
    );
    store
        .db
        .execute("DELETE FROM tasks WHERE id=?1", [too_long])
        .unwrap();
    let invalid_nested = task_value(
        "ordinary",
        "x".repeat(129).as_str(),
        "completed",
        "bad association",
    );
    put(&store.db, "tasks", &invalid_nested).unwrap();
    assert_eq!(
        code(store.history_page(&epoch, "tasks", None, None).unwrap_err()),
        "record_too_large"
    );
}

#[test]
fn legacy_snapshot_never_returns_a_partial_collection_and_pending_detail_is_explicit() {
    let store = store();
    session(&store.db, "s", vec![]);
    put(
        &store.db,
        "tasks",
        &task_value("small", "s", "completed", "complete"),
    )
    .unwrap();
    let legacy = store.legacy_bounded_snapshot().unwrap();
    assert_eq!(legacy["tasks"].as_array().unwrap().len(), 1);
    assert!(legacy.get("history").is_none());
    for n in 0..5 {
        put(
            &store.db,
            "tasks",
            &task_value(
                &format!("old-{n}"),
                "s",
                "completed",
                &"h".repeat(3 * 1024 * 1024),
            ),
        )
        .unwrap();
    }
    assert_eq!(
        code(store.legacy_bounded_snapshot().unwrap_err()),
        "record_too_large"
    );
    let snapshot = store.windowed_snapshot(&epoch()).unwrap();
    assert_eq!(snapshot["history"]["collections"]["tasks"]["total"], 6);
    assert!(snapshot["history"]["collections"]["tasks"]["has_more"]
        .as_bool()
        .unwrap());
    put(
        &store.db,
        "tasks",
        &task_value("large", "s", "running", &"r".repeat(7 * 1024 * 1024)),
    )
    .unwrap();
    request(
        &store.db,
        "pending",
        "large",
        "pending",
        &"p".repeat(2 * 1024 * 1024),
    );
    let detail = store.bounded_task_detail("large").unwrap();
    assert_eq!(
        detail["task"]["result"].as_str().unwrap().len(),
        7 * 1024 * 1024
    );
    assert_eq!(detail["execution_requests"], json!([]));
    assert_eq!(detail["history"]["execution_requests"]["total"], 1);
    assert_eq!(detail["history"]["execution_requests"]["pending"], 1);
    assert_eq!(
        detail["history"]["execution_requests"]["unavailable"][0]["id"],
        "pending"
    );
}

#[test]
fn first_page_current_cursor_keeps_membership_when_returning_or_refreshing() {
    let store = store();
    let epoch = epoch();
    session(&store.db, "s", vec![message(0, "old")]);
    put(
        &store.db,
        "tasks",
        &task_value("old", "s", "completed", "old"),
    )
    .unwrap();
    let first = store.history_page(&epoch, "tasks", None, None).unwrap();
    let messages = store.history_messages(&epoch, "s", None).unwrap();
    put(
        &store.db,
        "tasks",
        &task_value("new", "s", "completed", "new"),
    )
    .unwrap();
    session(&store.db, "s", vec![message(0, "old"), message(1, "new")]);
    let repeated = store
        .history_page(&epoch, "tasks", None, first["current_cursor"].as_str())
        .unwrap();
    assert_eq!(repeated["items"], first["items"]);
    assert_eq!(repeated["total"], 1);
    assert_eq!(repeated["current_cursor"], first["current_cursor"]);
    let repeated_messages = store
        .history_messages(&epoch, "s", messages["current_cursor"].as_str())
        .unwrap();
    assert_eq!(repeated_messages["items"], messages["items"]);
    assert_eq!(repeated_messages["total"], 1);
    assert_eq!(
        repeated_messages["current_cursor"],
        messages["current_cursor"]
    );
    assert_eq!(
        store.history_page(&epoch, "tasks", None, None).unwrap()["total"],
        2
    );
    assert_eq!(
        store.history_messages(&epoch, "s", None).unwrap()["total"],
        2
    );
}
