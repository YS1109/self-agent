//! Shared records exchanged by the daemon, desktop bridge, and channel adapters.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("HARNESS_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("self-agent")
        })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub base_url: String,
    pub token: String,
    pub pid: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SourceContext {
    pub channel_id: String,
    pub actor_id: String,
    pub conversation_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub project_path: String,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: String,
    pub session_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    pub status: String,
    pub execution_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(default = "worker_kind")]
    pub task_kind: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub source: SourceContext,
    pub steps: Vec<TaskStep>,
}

fn worker_kind() -> String {
    "worker".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanNodeInput {
    pub id: String,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanNode {
    pub id: String,
    pub title: String,
    pub objective: String,
    pub depends_on: Vec<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub title: String,
    pub source: SourceContext,
    pub execution_mode: String,
    pub status: String,
    pub revision: u32,
    pub round: u32,
    pub max_rounds: u32,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub nodes: Vec<PlanNode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStep {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub id: String,
    pub task_id: String,
    pub session_id: String,
    pub target: SourceContext,
    pub status: String,
    pub payload: String,
    pub attempts: u32,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub kind: String,
    pub message: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub description: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSelection {
    pub source: SourceContext,
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub tasks: Vec<TaskRun>,
    #[serde(default)]
    pub plans: Vec<Plan>,
    pub deliveries: Vec<Delivery>,
    pub events: Vec<RunEvent>,
    pub capabilities: Vec<Capability>,
    pub selections: Vec<SessionSelection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub command_id: String,
    pub source: SourceContext,
    pub command: Command,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    CreateSession {
        title: String,
        project_path: String,
    },
    SelectSession {
        session_id: String,
    },
    SendMessage {
        #[serde(default)]
        session_id: Option<String>,
        content: String,
    },
    StartTask {
        #[serde(default)]
        session_id: Option<String>,
        title: String,
    },
    CancelTask {
        task_id: String,
    },
    UpdatePlan {
        plan_id: String,
        expected_revision: u32,
        nodes: Vec<PlanNodeInput>,
    },
    StartPlan {
        plan_id: String,
        expected_revision: u32,
    },
    CancelPlan {
        plan_id: String,
    },
    RetryDelivery {
        delivery_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_envelopes_use_tagged_wire_format() {
        let envelope: CommandEnvelope = serde_json::from_value(serde_json::json!({
            "command_id": "intent-1",
            "source": {
                "channel_id": "simulator",
                "actor_id": "owner",
                "conversation_id": "remote-1"
            },
            "command": { "type": "start_task", "title": "Review changes" }
        }))
        .unwrap();
        assert!(matches!(
            envelope.command,
            Command::StartTask {
                session_id: None,
                ..
            }
        ));
        let encoded = serde_json::to_value(envelope).unwrap();
        assert_eq!(encoded["command"]["type"], "start_task");
    }

    #[test]
    fn snapshot_has_all_collections() {
        let value = serde_json::to_value(Snapshot::default()).unwrap();
        for key in [
            "sessions",
            "tasks",
            "plans",
            "deliveries",
            "events",
            "capabilities",
            "selections",
        ] {
            assert_eq!(value[key], serde_json::json!([]));
        }
    }

    #[test]
    fn previous_snapshot_and_tasks_default_to_worker_without_a_plan() {
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "sessions":[],
            "tasks":[{
                "id":"old-task","session_id":"old-session","title":"已有任务",
                "status":"completed","execution_mode":"demo",
                "created_at":"2026-10-23T00:00:00Z","updated_at":"2026-10-23T00:00:00Z",
                "source":{"channel_id":"desktop","actor_id":"owner","conversation_id":"desktop"},
                "steps":[]
            }],
            "deliveries":[],"events":[],"capabilities":[],"selections":[]
        }))
        .unwrap();
        assert!(snapshot.plans.is_empty());
        assert_eq!(snapshot.tasks[0].task_kind, "worker");
        assert!(snapshot.tasks[0].plan_id.is_none());
        assert!(snapshot.tasks[0].node_id.is_none());
        assert!(snapshot.tasks[0].parent_task_id.is_none());
    }

    #[test]
    fn plan_edit_command_preserves_optimistic_revision_and_dependencies() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "type":"update_plan","plan_id":"plan-1","expected_revision":7,
            "nodes":[{"id":"a","title":"检查","objective":"读取项目配置","depends_on":[]},
                {"id":"b","title":"实现","objective":"使用检查结果实现目标","depends_on":["a"]}]
        }))
        .unwrap();
        let Command::UpdatePlan {
            expected_revision,
            nodes,
            ..
        } = command
        else {
            panic!("unexpected command type")
        };
        assert_eq!(expected_revision, 7);
        assert_eq!(nodes[1].depends_on, ["a"]);
    }
}
