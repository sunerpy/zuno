//! Bounded, coalescing progress. Network latency never holds the model/event
//! loop and a dropped publisher cannot keep an execution alive.

use crate::{LIVE_PATH, TurnStateError, WorkerClient, WorkerExecution};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use zuno_application::live::LiveUpdate;
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::StreamEvent;
use zuno_types::{activity::LiveItem, identity::InvocationId};

impl WorkerClient {
    async fn publish_live(
        &self,
        execution: &WorkerExecution,
        update: &LiveUpdate,
    ) -> Result<(), TurnStateError> {
        if execution.boundary_started() || execution.deadline()? <= tokio::time::Instant::now() {
            return Err(TurnStateError::LeaseLost);
        }
        let grant = execution
            .credential
            .read()
            .map_err(|_| TurnStateError::InvalidData)?
            .grant
            .clone();
        update.validate().map_err(|_| TurnStateError::InvalidData)?;
        self.post(
            LIVE_PATH,
            Some(&grant),
            serde_json::to_vec(update).map_err(|_| TurnStateError::InvalidData)?,
        )
        .await?;
        Ok(())
    }
}

struct Draft {
    update: LiveUpdate,
    message: Option<String>,
    next_id: u32,
}
impl Draft {
    fn new(generation: String) -> Self {
        Self {
            update: LiveUpdate {
                generation,
                sequence: 1,
                message_id: None,
                items: Vec::new(),
            },
            message: None,
            next_id: 0,
        }
    }
    fn clear(&mut self) {
        self.update.items.clear();
        self.update.message_id = None;
        self.next_id = 0;
    }
    fn event(&mut self, event: &TurnEvent) -> bool {
        match event {
            TurnEvent::AssistantMessageCreated { message_id, .. } => {
                self.message = Some(message_id.clone());
                false
            }
            TurnEvent::ProviderRequestStarted { .. } => {
                self.clear();
                self.update.message_id = self.message.clone();
                true
            }
            TurnEvent::Provider {
                event: StreamEvent::TextDelta(text),
                ..
            } => self.text(text, false),
            TurnEvent::Provider {
                event: StreamEvent::ReasoningDelta(text),
                ..
            } => self.text(text, true),
            TurnEvent::Provider {
                event: StreamEvent::RetryRollback { .. },
                ..
            } => {
                self.clear();
                self.update.message_id = self.message.clone();
                true
            }
            TurnEvent::AssistantCheckpointed { .. } => {
                self.clear();
                true
            }
            TurnEvent::ToolCallStarted {
                call_id,
                display_name,
                ..
            } => {
                if self.update.message_id.is_none() || self.update.items.len() >= 16 {
                    return false;
                }
                let Ok(id) = InvocationId::new(call_id) else {
                    return false;
                };
                if self
                    .update
                    .items
                    .iter()
                    .any(|item| matches!(item,LiveItem::Invocation {id:old,..} if *old==id))
                {
                    return false;
                }
                let label = display_name
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(128)
                    .collect::<String>();
                if label.len() > 256 {
                    return false;
                }
                self.update.items.push(LiveItem::Invocation { id, label });
                true
            }
            _ => false,
        }
    }
    fn text(&mut self, text: &str, thinking: bool) -> bool {
        let Some(message) = &self.update.message_id else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        let mut used = self
            .update
            .items
            .iter()
            .map(|item| match item {
                LiveItem::Text { text, .. } | LiveItem::Thinking { text, .. } => text.len(),
                LiveItem::Invocation { label, .. } => label.len(),
            })
            .sum::<usize>();
        // Raw text may JSON-escape to six bytes per byte. Keep the whole
        // coalesced request beneath its private wire bound in that worst case.
        if used >= 32 * 1024 {
            return false;
        }
        let matches = self.update.items.last().is_some_and(|item| match item {
            LiveItem::Thinking { .. } => thinking,
            LiveItem::Text { .. } => !thinking,
            _ => false,
        });
        if !matches {
            if self.update.items.len() >= 16 {
                return false;
            }
            self.next_id += 1;
            let id = format!("live-{}", self.next_id);
            let parent_id = Some(zuno_application::activity::message_id(message));
            self.update.items.push(if thinking {
                LiveItem::Thinking {
                    id,
                    parent_id,
                    text: String::new(),
                    truncated: false,
                }
            } else {
                LiveItem::Text {
                    id,
                    parent_id,
                    text: String::new(),
                    truncated: false,
                }
            });
        }
        let (body, truncated) = match self.update.items.last_mut().expect("draft item") {
            LiveItem::Text {
                text, truncated, ..
            }
            | LiveItem::Thinking {
                text, truncated, ..
            } => (text, truncated),
            _ => unreachable!("text item"),
        };
        let mut end = text.len().min(32 * 1024 - used);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        body.push_str(&text[..end]);
        used += end;
        *truncated |= end < text.len() || used >= 32 * 1024;
        true
    }
}

pub(crate) struct LivePublisher {
    draft: Draft,
    pending: Arc<Mutex<Option<LiveUpdate>>>,
    task: tokio::task::JoinHandle<()>,
}
impl LivePublisher {
    pub(crate) fn start(
        client: WorkerClient,
        execution: WorkerExecution,
        interval: Duration,
    ) -> Self {
        let attempt = execution.lease().ok().map(|lease| lease.attempt_id);
        let generation = format!(
            "live-{}",
            zuno_orchestration::sha256_json(&serde_json::json!([execution.job.id, attempt]))
        );
        let pending = Arc::new(Mutex::new(None::<LiveUpdate>));
        let queue = Arc::clone(&pending);
        let task = tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let next = queue.lock().expect("progress queue").take();
                let Some(update) = next else {
                    continue;
                };
                let published = tokio::time::timeout(
                    Duration::from_secs(2),
                    client.publish_live(&execution, &update),
                )
                .await;
                match published {
                    Ok(Ok(())) => {}
                    Ok(Err(TurnStateError::LeaseLost | TurnStateError::Forbidden)) => return,
                    _ => {
                        // A newer complete snapshot supersedes a failed older
                        // one. Retries do not replay model or tool execution.
                        let mut pending = queue.lock().expect("progress queue");
                        if pending
                            .as_ref()
                            .is_none_or(|newer| newer.sequence < update.sequence)
                        {
                            *pending = Some(update);
                        }
                    }
                }
            }
        });
        Self {
            draft: Draft::new(generation),
            pending,
            task,
        }
    }
    pub(crate) fn observe(&mut self, event: &TurnEvent) {
        if self.draft.event(event) {
            self.draft.update.sequence = self.draft.update.sequence.saturating_add(1);
            *self.pending.lock().expect("progress queue") = Some(self.draft.update.clone());
        }
    }
}
impl Drop for LivePublisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_and_checkpoint_remove_old_drafts_and_only_visible_text_is_projected() {
        let mut draft = Draft::new("generation".to_owned());
        draft.event(&TurnEvent::AssistantMessageCreated {
            step: 1,
            message_id: "message".to_owned(),
        });
        draft.event(&TurnEvent::ProviderRequestStarted {
            step: 1,
            message_count: 1,
            estimated_prompt_tokens: 10,
        });
        draft.event(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::TextDelta("draft".to_owned()),
        });
        draft.event(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ReasoningSignatureDelta("secret".to_owned()),
        });
        assert!(
            !serde_json::to_string(&draft.update)
                .unwrap()
                .contains("secret")
        );
        draft.event(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::RetryRollback { attempt: 1, max: 2 },
        });
        assert!(draft.update.items.is_empty());
        draft.event(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::TextDelta("retry draft".to_owned()),
        });
        draft.event(&TurnEvent::AssistantCheckpointed {
            step: 1,
            message_id: "message".to_owned(),
            interrupted: false,
        });
        assert!(draft.update.items.is_empty());
        assert!(draft.update.message_id.is_none());
    }
    #[test]
    fn coalescing_bounds_escaped_content_and_item_count() {
        let mut draft = Draft::new("generation".to_owned());
        draft.message = Some("message".to_owned());
        draft.update.message_id = draft.message.clone();
        for i in 0..100 {
            draft.text(&"\u{0000}".repeat(4096), i % 2 == 0);
        }
        assert!(draft.update.items.len() <= 16);
        draft.update.validate().unwrap();
        assert!(
            serde_json::to_vec(&draft.update).unwrap().len()
                <= zuno_application::live::MAX_LIVE_BYTES
        );
    }
}
