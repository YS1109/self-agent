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
    pub status: String,
    pub execution_mode: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub source: SourceContext,
    pub steps: Vec<TaskStep>,
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
            "deliveries",
            "events",
            "capabilities",
            "selections",
        ] {
            assert_eq!(value[key], serde_json::json!([]));
        }
    }
}
