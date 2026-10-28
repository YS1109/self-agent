//! Small remote projections; never send owner snapshots, tool logs or local
//! source paths as a convenient QQ response. Delivery is plain text only.
use serde_json::Value;

pub(crate) fn task_status(task: &Value) -> String {
    let mut text = format!(
        "任务 {}\n状态：{}",
        id(task),
        status(task["status"].as_str().unwrap_or("unknown"))
    );
    if let Some(reason) = task["stop_reason"].as_str() {
        let reason = match reason {
            "user_cancelled" => "用户取消",
            "permission_denied" | "permission_revoked" | "remote_permission_revoked" => {
                "权限不允许或已撤销"
            }
            "runtime_exhausted" => "运行时长预算耗尽",
            "model_budget_exhausted" => "模型请求预算耗尽",
            "output_budget_exhausted" => "输出预算耗尽",
            "retry_budget_exhausted" | "repair_budget_exhausted" => "重试或修复预算耗尽",
            "external_call_budget_exhausted" => "外部调用预算耗尽",
            "approval_denied" | "approval_expired" => "本机动作授权未通过",
            "input_expired" => "问题等待过期",
            "verification_failed" => "实际验收未通过",
            "daemon_interrupted" | "interrupted_wait" => "后台中断，未自动重放",
            _ => "已停止；具体原因可在本机查看",
        };
        text.push_str(&format!("\n停止原因：{reason}"));
    }
    match task["execution_cleanup"].as_str() {
        Some("pending") => text.push_str("\n清理仍在进行，任务终态不表示副作用已停止。"),
        Some("unknown") => text.push_str("\n清理结果未知；需在本机检查，不会自动重放。"),
        _ => {}
    }
    if !task["waiting_request_id"].is_null() {
        text.push_str("\n任务在等待输入或本机授权；发送 /问题 任务ID 查询可回答的问题。");
    }
    text
}

/// Caller separately authorizes read_results and group audience disclosure.
pub(crate) fn task_result(task: &Value, project_root: &str) -> String {
    let text = task["result"].as_str().unwrap_or_else(|| {
        if matches!(task["status"].as_str(), Some("failed" | "cancelled")) {
            "没有可公开的结果摘要；错误详情保留在本机。"
        } else {
            "尚无终态结果；请稍后重新查询。"
        }
    });
    bounded(
        &format!(
            "{}\n结果摘要：\n{}",
            task_status(task),
            safe_text(text, project_root, 2800)
        ),
        3900,
    )
}

pub(crate) fn session_list(sessions: &[Value], selected: Option<&str>) -> String {
    let mut text = String::from("已授权会话（仅显示本次绑定范围）：");
    for session in sessions.iter().take(32) {
        let sid = id(session);
        text.push_str(&format!(
            "\n{}{} {}",
            if selected == Some(sid) {
                "[当前] "
            } else {
                ""
            },
            sid,
            safe_text(
                session["title"].as_str().unwrap_or("未命名会话"),
                session["project_path"].as_str().unwrap_or(""),
                60
            )
        ));
    }
    if sessions.is_empty() {
        text.push_str("\n没有可访问会话，请在本机检查授权。");
    }
    bounded(&text, 3900)
}

pub(crate) fn plan_status(plan: &Value) -> String {
    let nodes = plan["nodes"].as_array();
    let total = nodes.map_or(0, Vec::len);
    let completed = nodes.map_or(0, |nodes| {
        nodes
            .iter()
            .filter(|node| node["status"] == "completed")
            .count()
    });
    format!("计划 {}\n状态：{}\n修订：{}\n节点完成：{completed}/{total}\n启动、暂停和恢复需要相同计划 ID 与当前修订；不支持远端任意编辑节点。", id(plan),status(plan["status"].as_str().unwrap_or("unknown")), plan["revision"].as_u64().unwrap_or(0))
}

pub(crate) fn questions(task: &Value, requests: &[Value], project_root: &str) -> String {
    let mut text = task_status(task);
    let mut found = false;
    for request in requests.iter().filter(|request| {
        request["kind"] == "question"
            && request["status"] == "pending"
            && request["id"] == task["waiting_request_id"]
            && request["task_id"] == task["id"]
    }) {
        found = true;
        text.push_str(&format!(
            "\n问题 ID：{}\n修订：{}\n{}\n回答格式：/回答 {} {} 你的回答",
            id(request),
            request["revision"].as_u64().unwrap_or(0),
            safe_text(request["prompt"].as_str().unwrap_or(""), project_root, 2200),
            id(request),
            request["revision"].as_u64().unwrap_or(0)
        ));
    }
    if !found {
        text.push_str("\n当前没有可经 QQ 回答的问题。动作授权只能在本机核对和批准。");
    }
    bounded(&text, 3900)
}

fn id(value: &Value) -> &str {
    value["id"]
        .as_str()
        .filter(|id| {
            id.len() == 36
                && uuid::Uuid::parse_str(id)
                    .is_ok_and(|value| value.hyphenated().to_string() == *id)
        })
        .unwrap_or("标识不可用")
}
fn status(value: &str) -> &'static str {
    match value {
        "queued" | "pending" => "排队中",
        "running" => "运行中",
        "waiting_input" => "等待输入",
        "completed" => "已完成",
        "failed" => "失败",
        "cancelled" => "已取消",
        "draft" => "草稿",
        "paused" => "已暂停",
        "reviewing" => "复核中",
        "blocked" => "被阻塞",
        _ => "未知",
    }
}

fn safe_text(value: &str, root: &str, limit: usize) -> String {
    let mut text = value.to_owned();
    if !root.is_empty() {
        text = text.replace(root, "[项目目录]");
        let windows = root.replace('/', "\\");
        let posix = root.replace('\\', "/");
        for path in [windows, posix] {
            if path != root {
                text = text.replace(&path, "[项目目录]");
            }
        }
    }
    text = text
        .chars()
        .filter(|c| !c.is_control() || matches!(*c, '\n' | '\t'))
        .filter(|c| !matches!(*c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'))
        .map(|c| match c {
            '<' => '＜',
            '>' => '＞',
            '@' => '＠',
            _ => c,
        })
        .collect();
    bounded(&text, limit)
}
fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let marker = "\n[摘要已截断，完整记录保留在本机]";
    let mut end = limit.saturating_sub(marker.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ID: &str = "4b33f2ee-8c30-412d-9a3c-49a70e377e3b";
    #[test]
    fn default_status_never_embeds_result_error_or_tool_output() {
        let task = json!({"id":ID,"status":"failed","result":"secret body /private/project","error":"private error","evidence":[{"output":"private tool output"}],"stop_reason":"untrusted error text","execution_cleanup":"unknown"});
        let text = task_status(&task);
        assert!(!text.contains("secret body"));
        assert!(!text.contains("/private"));
        assert!(!text.contains("untrusted"));
        assert!(text.contains("清理结果未知"));
    }
    #[test]
    fn requested_result_is_bounded_plain_text_without_known_project_root() {
        let task = json!({"id":ID,"status":"completed","result":format!("/private/project <@everyone> \u{202e}{}","中文".repeat(3000))});
        let text = task_result(&task, "/private/project");
        assert!(text.len() <= 3900);
        assert!(!text.contains("/private/project"));
        assert!(!text.contains("<@"));
        assert!(!text.contains('\u{202e}'));
        assert!(text.contains("摘要已截断"));
    }
    #[test]
    fn question_projection_never_presents_approval_or_other_task() {
        let task = json!({"id":ID,"status":"waiting_input","waiting_request_id":ID});
        let approval = json!({"id":ID,"task_id":ID,"kind":"approval","status":"pending","action":{"command":"PRIVATE"},"prompt":"PRIVATE"});
        let text = questions(&task, &[approval], "/root");
        assert!(!text.contains("PRIVATE"));
        assert!(text.contains("只能在本机"));
    }
    #[test]
    fn session_list_keeps_all_thirty_two_full_ids_in_one_reply() {
        let sessions: Vec<_> = (0..32)
            .map(|_| json!({"id":uuid::Uuid::new_v4().to_string(),"title":"中文".repeat(100)}))
            .collect();
        let text = session_list(&sessions, sessions[0]["id"].as_str());
        assert!(text.len() <= 3900);
        for session in sessions {
            assert!(text.contains(session["id"].as_str().unwrap()));
        }
    }
}
