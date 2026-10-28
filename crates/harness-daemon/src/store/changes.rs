//! Persist only the bounded, filtered report, never an in-memory before-image.
use super::*;

const MAX_CHANGES_REPORT_BYTES: usize = 512 * 1024;
const RETAINED_CHANGES_REPORTS: usize = 128;
const UNAVAILABLE_REASON: &str = "没有可用的项目改动报告；未执行采集或采集未完成。";

impl Store {
    /// Called under the project writer permit, immediately before capture begins.
    /// The marker is deliberately small and cannot restore private file contents.
    pub(crate) fn begin_changes(&mut self, task_id: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if task["status"] != "running" || task["task_kind"] == "plan" {
            bail!("只有正在执行的项目任务可以开始改动采集");
        }
        if !task["changes_summary"].is_null() {
            bail!("该任务已开始或已结束改动采集，不能替换原基线");
        }
        task["changes_summary"] = json!({
            "state":"pending", "scope":"observed_execution_interval",
            "attribution":"not_exclusive", "capture_started_at":now()
        });
        put(&tx, "tasks", &task)?;
        tx.commit()?;
        Ok(())
    }

    /// Finalize exactly once, including a task cancelled while its worker exits.
    /// Report, summary, retention and notification are one atomic transaction.
    pub(crate) fn save_changes(&mut self, task_id: &str, report: Value) -> Result<RunEvent> {
        validate_report(&report)?;
        let body = serde_json::to_string(&report)?;
        if body.len() > MAX_CHANGES_REPORT_BYTES {
            bail!("项目改动报告超过 512 KiB 存储限制");
        }
        let tx = self.db.transaction()?;
        let mut task = record(&tx, "tasks", task_id)?;
        if task["task_kind"] == "plan"
            || !matches!(
                task["status"].as_str(),
                Some("running" | "completed" | "failed" | "cancelled")
            )
            || !matches!(
                task["changes_summary"]["state"].as_str(),
                Some("captured" | "pending")
            )
        {
            bail!("该任务没有待完成的改动基线，或报告已经保存");
        }
        // INSERT, never UPSERT: an existing final report is immutable.
        tx.execute(
            "INSERT INTO task_changes(id,body) VALUES(?1,?2)",
            params![task_id, body],
        )?;
        let mut summary = report_summary(&report);
        summary["capture_started_at"] = task["changes_summary"]["capture_started_at"].clone();
        task["changes_summary"] = summary;
        put(&tx, "tasks", &task)?;
        prune_reports(&tx)?;
        let mut item = event(
            Some(text(&task, "session_id")?.into()),
            Some(task_id.into()),
            "task_changes",
            if report["state"] == "unavailable" {
                "项目改动采集未完成；任务执行状态保持不变"
            } else {
                "项目改动观察报告已保存；不代表全部变化均由本任务产生"
            },
        );
        persist_event(&tx, &item)?;
        item.sequence = tx.query_row(
            "SELECT sequence FROM events WHERE id=?1",
            [&item.id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(item)
    }

    pub(crate) fn changes(&self, task_id: &str) -> Result<Value> {
        // Validate task existence first: an absent task is not an empty report.
        let task = record(&self.db, "tasks", task_id)?;
        let stored: Option<(usize, Option<String>)> = self
            .db
            .query_row(
                "SELECT length(CAST(body AS BLOB)),
                    CASE WHEN length(CAST(body AS BLOB))<=?2 THEN body ELSE NULL END
                 FROM task_changes WHERE id=?1",
                params![task_id, MAX_CHANGES_REPORT_BYTES as u64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((length, body)) = stored {
            if length > MAX_CHANGES_REPORT_BYTES {
                bail!("持久化改动报告超过读取限制");
            }
            return Ok(serde_json::from_str(&body.context("改动报告内容缺失")?)?);
        }
        let summary = &task["changes_summary"];
        let state = match summary["state"].as_str() {
            Some("expired") => "expired",
            Some("captured" | "pending") => "pending",
            _ => "unavailable",
        };
        Ok(json!({
            "state":state,"task_id":task_id,
            "reason":match state {
                "expired" => "详细报告已超过保留数量；任务及简要统计仍保留。",
                "pending" => "项目改动采集尚未结束。",
                _ => summary["reason"].as_str().unwrap_or(UNAVAILABLE_REASON),
            },
            "summary":summary,
        }))
    }
}

/// Called by Store recovery inside its transaction. A restart cannot recreate
/// the private baseline and must never imply that the observed interval is empty.
pub(super) fn recover_changes(db: &Connection) -> Result<()> {
    for mut task in rows(db, "tasks")? {
        if matches!(
            task["changes_summary"]["state"].as_str(),
            Some("captured" | "pending")
        ) {
            task["changes_summary"]["state"] = json!("unavailable");
            task["changes_summary"]["reason"] =
                json!("后台重启导致内存中的改动基线丢失；没有自动重放或推断项目写入。");
            task["changes_summary"]["updated_at"] = json!(now());
            put(db, "tasks", &task)?;
        }
    }
    Ok(())
}

fn report_summary(report: &Value) -> Value {
    if report["state"] == "unavailable" {
        return json!({"state":"unavailable","reason":report["reason"],
            "scope":"observed_execution_interval","attribution":"not_exclusive","updated_at":now()});
    }
    json!({
        "state":"available", "scope":"observed_execution_interval", "attribution":"not_exclusive",
        "visible_files":report["files"].as_array().map_or(0, Vec::len),
        "skipped_files":report["skipped"].as_array().map_or(0, Vec::len),
        "partial":report["partial"].as_bool().unwrap_or(true),
        "excluded_count":report["excluded_count"].as_u64().unwrap_or(0),
        "sensitive_file_count":report["sensitive_file_count"].as_u64().unwrap_or(0),
        "updated_at":now(),
    })
}

fn prune_reports(db: &Connection) -> Result<()> {
    let expired = db
        .prepare("SELECT id FROM task_changes ORDER BY rowid DESC LIMIT -1 OFFSET ?1")?
        .query_map([RETAINED_CHANGES_REPORTS as u64], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for id in expired {
        let mut task = record(db, "tasks", &id)?;
        task["changes_summary"]["state"] = json!("expired");
        task["changes_summary"]["expired_at"] = json!(now());
        put(db, "tasks", &task)?;
        db.execute("DELETE FROM task_changes WHERE id=?1", [&id])?;
    }
    Ok(())
}

fn validate_keys(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value.as_object().context("改动报告字段必须是对象")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        bail!("改动报告包含未批准字段；不保存原始文件快照");
    }
    Ok(())
}

fn validate_report(report: &Value) -> Result<()> {
    if report["state"] == "unavailable" {
        validate_keys(report, &["state", "reason"])?;
        let reason = report["reason"].as_str().context("缺少改动不可用原因")?;
        if reason.len() > 1024 {
            bail!("改动不可用原因超过长度限制");
        }
        return Ok(());
    }
    validate_keys(
        report,
        &[
            "version",
            "scope",
            "coverage",
            "attribution",
            "notice",
            "baseline_git",
            "current_git",
            "files",
            "skipped",
            "partial",
            "excluded_count",
            "sensitive_file_count",
            "limits",
            "exclusions",
        ],
    )?;
    if report["version"] != 1
        || report["scope"] != "observed_execution_interval"
        || report["coverage"] != "filtered_workspace"
        || report["attribution"] != "not_exclusive"
    {
        bail!("改动报告协议或归属声明无效");
    }
    let files = report["files"].as_array().context("缺少改动文件列表")?;
    let skipped = report["skipped"].as_array().context("缺少跳过文件列表")?;
    if files.len() + skipped.len() > 200 {
        bail!("改动报告文件条目超过限制");
    }
    for file in files {
        validate_keys(
            file,
            &[
                "path",
                "previous_path",
                "status",
                "comparison",
                "binary",
                "before_bytes",
                "after_bytes",
                "before_readonly",
                "after_readonly",
                "before_mode",
                "after_mode",
                "diff_state",
                "diff",
            ],
        )?;
    }
    for skipped in skipped {
        validate_keys(skipped, &["path", "reason"])?;
    }
    for name in ["baseline_git", "current_git"] {
        let git = &report[name];
        validate_keys(
            git,
            &[
                "state",
                "entries",
                "omitted_count",
                "reason",
                "scope",
                "head_comparison",
                "worktree_comparison",
                "reasons",
                "notice",
                "limits",
            ],
        )?;
        if let Some(limits) = git.get("limits") {
            validate_keys(
                limits,
                &["metadata_bytes", "entries", "object_bytes", "delta_depth"],
            )?;
            if limits
                .as_object()
                .unwrap()
                .values()
                .any(|value| value.as_u64().is_none())
            {
                bail!("Git 元数据预算必须为非负整数");
            }
        }
        if let Some(entries) = git["entries"].as_array() {
            if entries.len() > 200 {
                bail!("Git 状态条目超过限制");
            }
            for entry in entries {
                validate_keys(
                    entry,
                    &["path", "index", "worktree", "untracked", "raw_worktree"],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(store: &mut Store, command: Value) -> Value {
        store
            .command(
                json!({"command_id":new_id(),
            "source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"changes-tests"},
            "command":command}),
                &Settings {
                    execution_mode: "demo".into(),
                    ..Settings::default()
                },
            )
            .unwrap()
            .0
    }

    fn task(store: &mut Store) -> String {
        let session = send(
            store,
            json!({"type":"create_session","title":"changes",
            "project_path":std::env::temp_dir()}),
        );
        let receipt = send(
            store,
            json!({"type":"run_agent","session_id":session["session_id"],"content":"test changes"}),
        );
        store.claim_task().unwrap().unwrap();
        receipt["task_id"].as_str().unwrap().to_owned()
    }

    fn report() -> Value {
        json!({"version":1,"scope":"observed_execution_interval","coverage":"filtered_workspace",
            "attribution":"not_exclusive","partial":false,"excluded_count":2,"sensitive_file_count":1,
            "files":[{"path":"src/main.rs","status":"modified","diff_state":"available","diff":"safe diff"}],
            "skipped":[],"baseline_git":{"state":"not_git"},"current_git":{"state":"not_git"}})
    }

    #[test]
    fn saves_once_without_changing_cancelled_or_completed_task() {
        for terminal in ["cancelled", "completed", "failed"] {
            let mut store = Store::open(Path::new(":memory:")).unwrap();
            let id = task(&mut store);
            assert_eq!(store.changes(&id).unwrap()["state"], "unavailable");
            store.begin_changes(&id).unwrap();
            assert_eq!(store.changes(&id).unwrap()["state"], "pending");
            assert!(store.begin_changes(&id).is_err());
            if terminal == "cancelled" {
                send(&mut store, json!({"type":"cancel_task","task_id":id}));
            } else {
                store
                    .finish(
                        &id,
                        if terminal == "completed" {
                            Ok("done".into())
                        } else {
                            Err(anyhow!("failure"))
                        },
                    )
                    .unwrap();
            }
            let before = record(&store.db, "tasks", &id).unwrap();
            let cursor = store.event_cursor().unwrap();
            let event = store.save_changes(&id, report()).unwrap();
            assert!(event.sequence > cursor);
            assert_eq!(event.kind, "task_changes");
            assert_eq!(store.changes(&id).unwrap(), report());
            let after = record(&store.db, "tasks", &id).unwrap();
            for field in ["status", "result", "error", "updated_at", "steps"] {
                assert_eq!(before[field], after[field]);
            }
            assert_eq!(after["changes_summary"]["visible_files"], 1);
            assert!(store.save_changes(&id, report()).is_err());
            assert_eq!(store.event_cursor().unwrap(), event.sequence);
            assert!(!store.snapshot().unwrap().to_string().contains("safe diff"));
        }
    }

    #[test]
    fn event_failure_rolls_back_report_and_summary() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let id = task(&mut store);
        store.begin_changes(&id).unwrap();
        let cursor = store.event_cursor().unwrap();
        store
            .db
            .execute_batch(
                "CREATE TRIGGER reject_changes BEFORE INSERT ON events
            WHEN json_extract(NEW.body,'$.kind')='task_changes'
            BEGIN SELECT RAISE(ABORT,'fixture'); END;",
            )
            .unwrap();
        assert!(store.save_changes(&id, report()).is_err());
        assert_eq!(store.changes(&id).unwrap()["state"], "pending");
        assert_eq!(store.event_cursor().unwrap(), cursor);
        store
            .db
            .execute_batch("DROP TRIGGER reject_changes")
            .unwrap();
        store.save_changes(&id, report()).unwrap();
    }

    #[test]
    fn retention_expires_only_detail_and_preserves_summary_and_task() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let mut first = None;
        for _ in 0..=RETAINED_CHANGES_REPORTS {
            let id = task(&mut store);
            store.begin_changes(&id).unwrap();
            store.save_changes(&id, report()).unwrap();
            store.finish(&id, Ok("unchanged result".into())).unwrap();
            first.get_or_insert(id);
        }
        let id = first.unwrap();
        let task = record(&store.db, "tasks", &id).unwrap();
        assert_eq!(task["status"], "completed");
        assert_eq!(task["result"], "unchanged result");
        assert_eq!(task["changes_summary"]["state"], "expired");
        assert_eq!(task["changes_summary"]["visible_files"], 1);
        assert_eq!(task["changes_summary"]["excluded_count"], 2);
        assert_eq!(store.changes(&id).unwrap()["state"], "expired");
        assert!(store.save_changes(&id, report()).is_err());
        let count: usize = store
            .db
            .query_row("SELECT COUNT(*) FROM task_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, RETAINED_CHANGES_REPORTS);
    }

    #[test]
    fn restart_marks_lost_baseline_unavailable_and_keeps_finished_reports() {
        let path = std::env::temp_dir().join(format!("changes-store-{}.sqlite", new_id()));
        let (pending, saved);
        {
            let mut store = Store::open(&path).unwrap();
            pending = task(&mut store);
            store.begin_changes(&pending).unwrap();
            saved = task(&mut store);
            store.begin_changes(&saved).unwrap();
            store.save_changes(&saved, report()).unwrap();
        }
        {
            let mut store = Store::open(&path).unwrap();
            assert_eq!(store.changes(&pending).unwrap()["state"], "unavailable");
            assert_eq!(store.changes(&saved).unwrap(), report());
            assert!(store.save_changes(&pending, report()).is_err());
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_missing_task_raw_images_and_oversize_without_finalizing() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        assert!(store.changes("missing").is_err());
        assert!(store.begin_changes("missing").is_err());
        let id = task(&mut store);
        assert!(store.save_changes(&id, report()).is_err());
        store.begin_changes(&id).unwrap();
        let mut raw = report();
        raw["files"][0]["before_image"] = json!("private source");
        assert!(store.save_changes(&id, raw).is_err());
        let mut oversized = report();
        oversized["files"][0]["diff"] = json!("x".repeat(MAX_CHANGES_REPORT_BYTES));
        assert!(store.save_changes(&id, oversized).is_err());
        assert_eq!(store.changes(&id).unwrap()["state"], "pending");
        store
            .save_changes(&id, json!({"state":"unavailable","reason":"安全采集失败"}))
            .unwrap();
        assert_eq!(store.changes(&id).unwrap()["state"], "unavailable");
        assert_eq!(
            record(&store.db, "tasks", &id).unwrap()["status"],
            "running"
        );
    }
}
