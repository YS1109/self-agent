//! One in-flight, non-replayed stable turn/steer request at a time.
use crate::steering::InputAcknowledgement;
use anyhow::{bail, Result};
use harness_core::TaskSupplement;
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct Steering {
    pending: Option<Pending>,
    seen: HashSet<String>,
    content_bytes: usize,
    next_id: u64,
}
struct Pending {
    rpc_id: u64,
    turn_id: String,
    input_ids: Vec<String>,
}
impl Steering {
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn prepare(
        &mut self,
        inputs: Vec<TaskSupplement>,
        thread: &str,
        turn: &str,
        acknowledge: &impl Fn(&[String], InputAcknowledgement) -> Result<()>,
    ) -> Result<Option<Value>> {
        if inputs.is_empty() {
            return Ok(None);
        }
        if self.pending.is_some() {
            bail!("Codex 尚有未确认的补充输入，未发送新的输入");
        }
        let ids: Vec<_> = inputs.iter().map(|input| input.id.clone()).collect();
        let unique: HashSet<_> = ids.iter().collect();
        if unique.len() != ids.len() || ids.iter().any(|id| self.seen.contains(id)) {
            bail!("Codex 补充输入回执重复，未重放输入");
        }
        let bytes: usize = inputs.iter().map(|input| input.content.len()).sum();
        if inputs.len() > 8
            || self.seen.len() + inputs.len() > 32
            || ids.iter().any(|id| id.is_empty() || id.len() > 256)
            || inputs
                .iter()
                .any(|input| input.content.trim().is_empty() || input.content.len() > 8192)
            || self.content_bytes.saturating_add(bytes) > 65536
        {
            acknowledge(
                &ids,
                InputAcknowledgement::NotApplied {
                    reason: "补充输入超过安全预算，未发送到 Codex".into(),
                },
            )?;
            return Ok(None);
        }
        self.seen.extend(ids.iter().cloned());
        self.content_bytes += bytes;
        self.next_id += 1;
        let rpc_id = 1000 + self.next_id;
        let input: Vec<_> = inputs
            .into_iter()
            .map(|input| json!({"type":"text","text":input.content}))
            .collect();
        self.pending = Some(Pending {
            rpc_id,
            turn_id: turn.into(),
            input_ids: ids,
        });
        Ok(Some(json!({"id":rpc_id,"method":"turn/steer","params":{
            "threadId":thread,"expectedTurnId":turn,"input":input
        }})))
    }

    pub fn response(
        &mut self,
        message: &Value,
        turn: Option<&str>,
        acknowledge: &impl Fn(&[String], InputAcknowledgement) -> Result<()>,
    ) -> Result<bool> {
        let Some(pending) = &self.pending else {
            return Ok(false);
        };
        if message["id"].as_u64() != Some(pending.rpc_id) {
            return Ok(false);
        }
        if message.get("method").is_some() {
            return Ok(false);
        }
        if turn != Some(pending.turn_id.as_str()) {
            bail!("Codex 补充输入的回合状态已变化，结果未知");
        }
        let status = if message.get("result").is_none() && message["error"].is_object() {
            InputAcknowledgement::NotApplied {
                reason: "Codex 当前回合拒绝了补充输入".into(),
            }
        } else if message.get("error").is_none() && message["result"]["turnId"] == pending.turn_id {
            InputAcknowledgement::Applied {
                via: "codex_turn_steer".into(),
            }
        } else {
            bail!("Codex 补充输入确认与目标回合不一致，结果未知");
        };
        // Keep pending until durable acknowledgement succeeds. On any uncertain
        // exit the store, not this transport, marks delivering as unknown.
        acknowledge(&pending.input_ids, status)?;
        self.pending = None;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    fn input(id: &str) -> TaskSupplement {
        TaskSupplement {
            id: id.into(),
            task_id: "task".into(),
            content: "补充".into(),
            status: "delivering".into(),
            reason: None,
            applied_via: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
    #[test]
    fn batches_are_bounded_and_repeated_receipts_are_never_resent() {
        let mut state = Steering::default();
        let statuses = Mutex::new(Vec::new());
        let acknowledge = |_: &[String], status| {
            statuses.lock().unwrap().push(status);
            Ok(())
        };
        let mut oversized = input("oversized");
        oversized.content = "x".repeat(8193);
        assert!(state
            .prepare(vec![oversized], "thread", "turn", &acknowledge)
            .unwrap()
            .is_none());
        assert!(matches!(
            &statuses.lock().unwrap()[0],
            InputAcknowledgement::NotApplied { .. }
        ));
        let request = state
            .prepare(vec![input("one")], "thread", "turn", &acknowledge)
            .unwrap()
            .unwrap();
        assert!(state.is_pending());
        assert!(!state
            .response(
                &json!({"id":request["id"].as_u64().unwrap()+1,"result":{"turnId":"turn"}}),
                Some("turn"),
                &acknowledge
            )
            .unwrap());
        assert_eq!(statuses.lock().unwrap().len(), 1);
        assert!(state
            .response(
                &json!({"id":request["id"],"result":{"turnId":"turn"}}),
                Some("turn"),
                &acknowledge
            )
            .unwrap());
        assert!(!state.is_pending());
        assert!(state
            .prepare(vec![input("one")], "thread", "turn", &acknowledge)
            .is_err());
        assert_eq!(statuses.lock().unwrap().len(), 2);
    }
}
