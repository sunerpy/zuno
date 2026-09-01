//! Project durable Zuno messages onto ACP `session/update` notifications.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use url::Url;
use zuno_db::message::{MessageRole, MessageWithParts, PartKind, PartRecord};
use zuno_db::session::{MessageUsage, TokenAccounting};
use zuno_engine::r#loop::{INTERRUPTED_TURN_NOTICE, ToolDiff, ToolInterruption};
use zuno_tool::ToolOutput;
use zuno_types::WorkStateProjection;

use crate::projection::{
    CompletedToolUpdate, completed_tool_update, interrupted_tool_update, tool_call,
};

/// Maximum retained durable messages hydrated for one ACP load.
pub const REPLAY_MESSAGE_CAP: usize = 512;
/// Maximum stored part-data bytes hydrated for one ACP load.
///
/// This must accommodate at least one prompt image accepted by the ACP input
/// boundary (5 MiB decoded, roughly 6.7 MiB base64) plus its JSON envelope.
pub const REPLAY_TRANSCRIPT_BYTE_CAP: u64 = 16 * 1_024 * 1_024;

const DEFAULT_REPLAY_TOTAL_BYTES: usize = REPLAY_TRANSCRIPT_BYTE_CAP as usize;
const DEFAULT_REPLAY_FRAME_BYTES: usize = 8 * 1_024 * 1_024;
const REPLAY_NOTICE_RESERVE_BYTES: usize = 1_024;
const PROVIDER_REASONING_KEY: &str = "providerReasoning";

/// Bounded and path-aware projection policy for one ACP restore.
#[derive(Debug, Clone)]
pub struct ReplayPolicy {
    workspace_root: PathBuf,
    max_total_bytes: usize,
    max_frame_bytes: usize,
}

impl ReplayPolicy {
    #[must_use]
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self::with_limits(
            workspace_root,
            DEFAULT_REPLAY_TOTAL_BYTES,
            DEFAULT_REPLAY_FRAME_BYTES,
        )
    }

    #[must_use]
    pub fn with_limits(
        workspace_root: &Path,
        max_total_bytes: usize,
        max_frame_bytes: usize,
    ) -> Self {
        Self {
            workspace_root: std::fs::canonicalize(workspace_root)
                .unwrap_or_else(|_| workspace_root.to_path_buf()),
            max_total_bytes,
            max_frame_bytes,
        }
    }

    fn actionable_path(&self, path: &Path) -> bool {
        if !path.is_absolute() {
            return false;
        }
        std::fs::canonicalize(path).is_ok_and(|canonical| {
            canonical.is_file() && canonical.starts_with(&self.workspace_root)
        })
    }
}

/// One bounded replay projection plus its explicit omission count.
#[derive(Debug)]
pub struct DurableReplay {
    pub updates: Vec<Value>,
    pub omitted_messages: usize,
}

#[must_use]
pub fn durable_updates(
    history: &[MessageWithParts],
    policy: &ReplayPolicy,
    previously_omitted: usize,
) -> DurableReplay {
    let payload_budget = policy
        .max_total_bytes
        .saturating_sub(REPLAY_NOTICE_RESERVE_BYTES);
    let mut groups = Vec::new();
    let mut encoded_bytes = 0_usize;
    let mut omitted_messages = previously_omitted;

    for (reverse_index, stored) in history.iter().rev().enumerate() {
        let updates = message_updates(stored, policy);
        let sizes = updates
            .iter()
            .map(|update| serde_json::to_vec(update).map(|encoded| encoded.len()))
            .collect::<Result<Vec<_>, _>>();
        let Ok(sizes) = sizes else {
            omitted_messages =
                omitted_messages.saturating_add(history.len().saturating_sub(reverse_index));
            break;
        };
        if sizes.iter().any(|size| *size > policy.max_frame_bytes) {
            omitted_messages =
                omitted_messages.saturating_add(history.len().saturating_sub(reverse_index));
            break;
        }
        let group_bytes = sizes.into_iter().sum::<usize>();
        if encoded_bytes.saturating_add(group_bytes) > payload_budget {
            omitted_messages =
                omitted_messages.saturating_add(history.len().saturating_sub(reverse_index));
            break;
        }
        encoded_bytes = encoded_bytes.saturating_add(group_bytes);
        groups.push(updates);
    }

    groups.reverse();
    let mut updates = groups.into_iter().flatten().collect::<Vec<_>>();
    if omitted_messages > 0 {
        updates.insert(0, replay_omission_update(omitted_messages));
    }
    DurableReplay {
        updates,
        omitted_messages,
    }
}

fn replay_omission_update(omitted_messages: usize) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {
            "type": "text",
            "text": format!(
                "Earlier durable ACP history was omitted from this restore to keep replay bounded \
                 ({omitted_messages} messages)."
            ),
        },
        "_meta": {
            "zuno": {
                "kind": "replay_omission",
                "omittedMessages": omitted_messages,
            }
        },
    })
}

/// Replays the last provider-confirmed context usage, never cumulative session tokens.
#[must_use]
pub fn durable_usage_update(
    history: &[MessageWithParts],
    context_size: u64,
    cumulative_cost: f64,
) -> Option<Value> {
    if context_size == 0 {
        return None;
    }
    let usage = history
        .iter()
        .rev()
        .filter(|message| message.info.role == MessageRole::Assistant)
        .map(|message| MessageUsage::from_data(&message.info.data))
        .find(|usage| usage.reported)?;
    let prompt = match usage.accounting? {
        TokenAccounting::CacheInsideInput => usage.tokens.input,
        TokenAccounting::CacheBesideInput => usage
            .tokens
            .input
            .saturating_add(usage.tokens.cache_read)
            .saturating_add(usage.tokens.cache_write),
    };
    let used = u64::try_from(prompt.saturating_add(usage.tokens.output).max(0)).ok()?;
    let mut update = json!({
        "sessionUpdate": "usage_update",
        "used": used,
        "size": context_size,
    });
    if cumulative_cost.is_finite() && cumulative_cost >= 0.0 {
        update["cost"] = json!({ "amount": cumulative_cost, "currency": "USD" });
    }
    Some(update)
}

/// Replays the complete current plan snapshot in stable ACP's three-state vocabulary.
#[must_use]
pub fn durable_plan_update(work: &WorkStateProjection) -> Option<Value> {
    let plan = work.plan.as_ref()?;
    let mut entries = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let (status, outcome) = match step.status.as_str() {
            "pending" => ("pending", None),
            "in_progress" => ("in_progress", None),
            "completed" => ("completed", Some("completed")),
            "superseded" => ("completed", Some("superseded")),
            _ => return None,
        };
        let mut entry = json!({
            "content": step.title,
            "priority": plan_step_priority(work, &step.id),
            "status": status,
            "_meta": { "zuno": { "stepId": step.id } },
        });
        if let Some(outcome) = outcome {
            entry["_meta"]["zuno"]["outcome"] = json!(outcome);
        }
        entries.push(entry);
    }
    Some(json!({
        "sessionUpdate": "plan",
        "entries": entries,
        "_meta": {
            "zuno": {
                "planId": plan.id,
                "revision": plan.revision,
                "title": plan.title,
            }
        },
    }))
}

/// Replays the complete durable learning snapshot through ACP extensibility.
///
/// `session_info_update` permits metadata-only patches. Keeping learning under
/// `_meta.zuno` lets unaware clients ignore it without rendering false chat
/// content, while capable clients receive the same projection as TUI and Server.
#[must_use]
pub fn durable_learning_update(work: &WorkStateProjection) -> Value {
    json!({
        "sessionUpdate": "session_info_update",
        "_meta": {
            "zuno": {
                "learning": work.learning,
            }
        },
    })
}

/// Stable ACP updates for the complete frontend-neutral work snapshot.
#[must_use]
pub fn durable_work_updates(work: &WorkStateProjection) -> Vec<Value> {
    let mut updates = durable_plan_update(work).into_iter().collect::<Vec<_>>();
    updates.push(durable_learning_update(work));
    updates
}

fn plan_step_priority<'a>(work: &'a WorkStateProjection, step_id: &str) -> &'a str {
    let mut resolved = None;
    for item in work
        .todos
        .iter()
        .filter(|item| item.plan_step_id.as_deref() == Some(step_id))
    {
        match item.priority.as_str() {
            "high" => return "high",
            "medium" => resolved = Some("medium"),
            "low" if resolved.is_none() => resolved = Some("low"),
            _ => {}
        }
    }
    resolved.unwrap_or("medium")
}

fn message_updates(stored: &MessageWithParts, policy: &ReplayPolicy) -> Vec<Value> {
    let mut updates = Vec::new();
    let visible_reasoning = stored
        .parts
        .iter()
        .filter(|part| part.kind == PartKind::Reasoning && !is_provider_reasoning(part))
        .filter_map(|part| non_empty_string(&part.data, "text"))
        .collect::<BTreeSet<_>>();
    for part in &stored.parts {
        if is_provider_reasoning(part)
            && non_empty_string(&part.data, "text")
                .is_some_and(|text| visible_reasoning.contains(text))
        {
            continue;
        }
        match part.kind {
            PartKind::Text => {
                if let Some(text) = non_empty_string(&part.data, "text") {
                    let mut update = message_content_update(
                        message_kind(stored.info.role),
                        json!({ "type": "text", "text": text }),
                        &stored.info.id,
                    );
                    if let Some(metadata) = stored
                        .info
                        .data
                        .get(zuno_db::message::TASK_REPORT_METADATA_KEY)
                    {
                        update["_meta"] = json!({
                            "zuno": {
                                "kind": "task_report",
                                "taskReport": metadata,
                            }
                        });
                    }
                    updates.push(update);
                }
            }
            PartKind::Reasoning => {
                if let Some(text) = non_empty_string(&part.data, "text") {
                    updates.push(message_content_update(
                        "agent_thought_chunk",
                        json!({ "type": "text", "text": text }),
                        &stored.info.id,
                    ));
                }
            }
            PartKind::File => {
                if let Some(content) = stored_file_content(&part.data, policy) {
                    updates.push(message_content_update(
                        message_kind(stored.info.role),
                        content,
                        &stored.info.id,
                    ));
                }
            }
            PartKind::Tool => updates.extend(tool_updates(part, policy)),
            PartKind::StepStart
            | PartKind::StepFinish
            | PartKind::Snapshot
            | PartKind::Patch
            | PartKind::Agent
            | PartKind::Subtask
            | PartKind::Retry
            | PartKind::Compaction => {}
        }
    }
    if let Some(update) = interruption_update(stored) {
        updates.push(update);
    }
    updates
}

fn interruption_update(stored: &MessageWithParts) -> Option<Value> {
    let error = stored.info.data.get("error")?.as_object()?;
    let name = error.get("name")?.as_str()?;
    if !matches!(name, "AbortError" | "MessageAbortedError") {
        return None;
    }
    let data = error.get("data").and_then(Value::as_object);
    Some(json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": INTERRUPTED_TURN_NOTICE },
        "messageId": stored.info.id,
        "_meta": {
            "zuno": {
                "kind": "turn_interrupted",
                "source": data.and_then(|data| data.get("source")).cloned(),
                "reason": data.and_then(|data| data.get("reason")).cloned(),
            },
        },
    }))
}

fn is_provider_reasoning(part: &PartRecord) -> bool {
    part.data
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| metadata.contains_key(PROVIDER_REASONING_KEY))
}

fn message_kind(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user_message_chunk",
        MessageRole::Assistant => "agent_message_chunk",
    }
}

fn message_content_update(kind: &str, content: Value, message_id: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": content,
        "messageId": message_id,
    })
}

fn tool_updates(part: &PartRecord, policy: &ReplayPolicy) -> Vec<Value> {
    let Some(call_id) = non_empty_string(&part.data, "callID") else {
        return Vec::new();
    };
    let Some(name) = non_empty_string(&part.data, "tool") else {
        return Vec::new();
    };
    let display_name = non_empty_string(&part.data, "displayName").unwrap_or(name);
    let state = part.data.get("state").and_then(Value::as_object);
    let status = state
        .and_then(|state| state.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("pending");
    let raw_input = state
        .and_then(|state| state.get("raw"))
        .and_then(Value::as_str)
        .map(json_or_string)
        .or_else(|| state.and_then(|state| state.get("input")).cloned());
    let initial_status = if status == "running" {
        "in_progress"
    } else {
        "pending"
    };
    let mut updates = vec![tool_call(
        call_id,
        display_name,
        name,
        initial_status,
        raw_input.clone(),
    )];
    if !matches!(status, "completed" | "error") {
        return updates;
    }

    let output = state
        .and_then(|state| state.get("output").or_else(|| state.get("error")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = state
        .and_then(|state| state.get("title"))
        .and_then(Value::as_str)
        .unwrap_or(display_name);
    let metadata = state
        .and_then(|state| state.get("metadata"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut durable_output = ToolOutput::text(title, output);
    durable_output.metadata = metadata.clone();
    let written_paths = durable_output
        .written_paths()
        .into_iter()
        .filter(|path| policy.actionable_path(Path::new(path)))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let diff = ToolDiff::from_output(&durable_output)
        .as_ref()
        .and_then(|diff| replay_diff(diff, policy));
    let completed_input = CompletedToolUpdate {
        call_id,
        display_name,
        name,
        title,
        raw_input: raw_input.as_ref(),
        output,
        diff: diff.as_ref(),
        written_paths: &written_paths,
        is_error: status == "error",
        presentation: None,
        metadata: Some(&metadata),
    };
    let interruption = metadata
        .get("interruption")
        .and_then(Value::as_object)
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        .and_then(|mode| match mode {
            "cooperative" => Some(ToolInterruption::Cooperative),
            "forced" => Some(ToolInterruption::Forced),
            _ => None,
        });
    let mut completed = match interruption {
        Some(interruption) => interrupted_tool_update(completed_input, interruption),
        None => completed_tool_update(completed_input),
    };
    if let Some(content) = completed.get_mut("content").and_then(Value::as_array_mut) {
        content.extend(
            state
                .and_then(|state| state.get("attachments"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|value| tool_attachment_content(value, policy)),
        );
    }
    if state
        .and_then(|state| state.get("outcome"))
        .and_then(Value::as_str)
        == Some("blocked")
    {
        let kind = state
            .and_then(|state| state.get("blockKind"))
            .and_then(Value::as_str)
            .unwrap_or("blocked");
        completed["_meta"] = json!({ "zuno": { "blockedKind": kind } });
    }
    updates.push(completed);
    updates
}

fn replay_diff(diff: &ToolDiff, policy: &ReplayPolicy) -> Option<ToolDiff> {
    let files = diff
        .files()
        .iter()
        .filter(|file| policy.actionable_path(Path::new(file.path())))
        .cloned()
        .collect::<Vec<_>>();
    let unified = diff
        .files()
        .is_empty()
        .then(|| diff.unified().map(str::to_owned))
        .flatten();
    ToolDiff::new(unified, files)
}

fn stored_file_content(data: &Map<String, Value>, policy: &ReplayPolicy) -> Option<Value> {
    let mime = non_empty_string(data, "mime");
    let url = non_empty_string(data, "url");
    if mime.is_some_and(|mime| mime.starts_with("image/")) {
        let mime = mime?;
        if let Some(payload) = non_empty_string(data, "data")
            .or_else(|| url.and_then(|url| data_url_payload(url, mime)))
        {
            let mut content = Map::new();
            content.insert("type".to_owned(), json!("image"));
            content.insert("data".to_owned(), json!(payload));
            content.insert("mimeType".to_owned(), json!(mime));
            if let Some(url) = url.filter(|url| resource_link_is_actionable(url, policy)) {
                content.insert("uri".to_owned(), json!(url));
            }
            return Some(Value::Object(content));
        }
    }
    let uri = url?;
    let name = non_empty_string(data, "filename")
        .or_else(|| uri.rsplit('/').next().filter(|name| !name.is_empty()))
        .unwrap_or("attachment");
    if !resource_link_is_actionable(uri, policy) {
        return Some(json!({
            "type": "text",
            "text": format!(
                "Historical local resource `{name}` was omitted because it is outside the active \
                 worktree or no longer exists."
            ),
            "_meta": {
                "zuno": {
                    "kind": "omitted_local_resource",
                }
            },
        }));
    }
    let mut content = Map::new();
    content.insert("type".to_owned(), json!("resource_link"));
    content.insert("name".to_owned(), json!(name));
    content.insert("uri".to_owned(), json!(uri));
    if let Some(title) = non_empty_string(data, "title") {
        content.insert("title".to_owned(), json!(title));
    }
    if let Some(description) = non_empty_string(data, "description") {
        content.insert("description".to_owned(), json!(description));
    }
    if let Some(mime) = mime {
        content.insert("mimeType".to_owned(), json!(mime));
    }
    if let Some(size) = data.get("size").and_then(Value::as_u64) {
        content.insert("size".to_owned(), json!(size));
    }
    Some(Value::Object(content))
}

fn resource_link_is_actionable(uri: &str, policy: &ReplayPolicy) -> bool {
    let Ok(uri) = Url::parse(uri) else {
        return !uri.starts_with("file:");
    };
    if uri.scheme() != "file" {
        return true;
    }
    uri.to_file_path()
        .ok()
        .is_some_and(|path| policy.actionable_path(&path))
}

fn tool_attachment_content(value: &Value, policy: &ReplayPolicy) -> Option<Value> {
    let content = stored_file_content(value.as_object()?, policy)?;
    Some(json!({ "type": "content", "content": content }))
}

fn non_empty_string<'a>(data: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn data_url_payload<'a>(url: &'a str, mime: &str) -> Option<&'a str> {
    let (header, payload) = url.split_once(',')?;
    (header == format!("data:{mime};base64") && !payload.is_empty()).then_some(payload)
}

fn json_or_string(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};

    use super::*;

    fn message(id: &str, role: &str, parts: Vec<PartRecord>) -> MessageWithParts {
        MessageWithParts {
            info: MessageRecord::from_json(json!({
                "id": id,
                "sessionID": "ses",
                "role": role,
                "time": { "created": 1 },
            }))
            .expect("message fixture"),
            parts,
        }
    }

    fn part(id: &str, message_id: &str, data: Value) -> PartRecord {
        let mut data = data.as_object().expect("part object").clone();
        data.insert("id".to_owned(), Value::String(id.to_owned()));
        data.insert("sessionID".to_owned(), Value::String("ses".to_owned()));
        data.insert("messageID".to_owned(), Value::String(message_id.to_owned()));
        PartRecord::from_json(Value::Object(data), 1).expect("part fixture")
    }

    #[test]
    fn replay_preserves_content_tools_typed_diffs_attachments_and_order() {
        let root = tempfile::tempdir().expect("replay root");
        let edited_path = root.path().join("demo.rs");
        std::fs::write(&edited_path, "new\n").expect("write replay target");
        let edited = edited_path.to_string_lossy().into_owned();
        let edited_wire = zuno_paths::wire_path(&edited_path);
        let user = message(
            "msg-user",
            "user",
            vec![
                part("p-text", "msg-user", json!({"type":"text","text":"hello"})),
                part(
                    "p-image",
                    "msg-user",
                    json!({
                        "type": "file",
                        "filename": "pixel.png",
                        "mime": "image/png",
                        "data": "aGVsbG8=",
                        "url": "data:image/png;base64,aGVsbG8=",
                    }),
                ),
            ],
        );
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![
                part(
                    "p-thought",
                    "msg-assistant",
                    json!({"type":"reasoning","text":"thinking"}),
                ),
                part(
                    "p-tool",
                    "msg-assistant",
                    json!({
                        "type": "tool",
                        "callID": "call-edit",
                        "tool": "edit",
                        "displayName": "Edit file",
                        "state": {
                            "status": "completed",
                            "raw": serde_json::to_string(&json!({"filePath": edited}))
                                .expect("raw input"),
                            "input": {"filePath": edited},
                            "title": "Updated demo.rs",
                            "output": "ok",
                            "metadata": {
                                "diff": "@@ -1 +1 @@\n-old\n+new\n",
                                "fileDiffs": [{
                                    "path": edited,
                                    "oldText": "old\n",
                                    "newText": "new\n"
                                }],
                                "writtenPaths": [edited]
                            },
                            "attachments": [{
                                "type": "file",
                                "mime": "image/png",
                                "filename": "preview.png",
                                "url": "data:image/png;base64,cHJldmlldw=="
                            }]
                        }
                    }),
                ),
                part(
                    "p-answer",
                    "msg-assistant",
                    json!({"type":"text","text":"done"}),
                ),
            ],
        );

        let replay = durable_updates(
            &[user, assistant],
            &ReplayPolicy::for_workspace(root.path()),
            0,
        );
        let updates = replay.updates;
        assert_eq!(updates.len(), 6, "every durable visible part is replayed");
        assert_eq!(updates[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[0]["messageId"], "msg-user");
        assert_eq!(updates[0]["content"]["text"], "hello");
        assert_eq!(updates[1]["content"]["type"], "image");
        assert_eq!(updates[1]["content"]["mimeType"], "image/png");
        assert_eq!(updates[2]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(updates[2]["messageId"], "msg-assistant");
        assert_eq!(updates[3]["sessionUpdate"], "tool_call");
        assert_eq!(updates[3]["rawInput"]["filePath"], edited);
        assert_eq!(updates[4]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[4]["status"], "completed");
        assert_eq!(updates[4]["content"][1]["type"], "diff");
        assert_eq!(updates[4]["content"][1]["path"], edited_wire);
        assert_eq!(updates[4]["content"][2]["content"]["type"], "image");
        assert_eq!(
            updates[4]["locations"],
            json!([{"path":edited_wire.clone()}])
        );
        assert_eq!(updates[5]["content"]["text"], "done");
        assert_eq!(updates[5]["messageId"], "msg-assistant");
    }

    #[test]
    fn replay_preserves_cancelled_tool_outcome_and_uncertainty() {
        let root = tempfile::tempdir().expect("replay root");
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![part(
                "p-tool",
                "msg-assistant",
                json!({
                    "type": "tool",
                    "callID": "call-task",
                    "tool": "task",
                    "displayName": "Delegate",
                    "state": {
                        "status": "error",
                        "title": "Inspect repository",
                        "error": "child did not acknowledge cancellation",
                        "metadata": {
                            "interruption": {
                                "mode": "forced",
                                "forced": true,
                                "uncertain": true
                            }
                        }
                    }
                }),
            )],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);
        assert_eq!(replay.updates.len(), 2);
        let cancelled = &replay.updates[1];
        assert_eq!(cancelled["status"], "failed");
        assert_eq!(cancelled["_meta"]["zuno"]["outcome"], "cancelled");
        assert_eq!(cancelled["_meta"]["zuno"]["interruptionMode"], "forced");
        assert_eq!(cancelled["_meta"]["zuno"]["uncertain"], true);
    }

    #[test]
    fn replay_preserves_typed_turn_interruption_provenance() {
        let root = tempfile::tempdir().expect("replay root");
        let mut assistant = message("msg-assistant", "assistant", Vec::new());
        assistant.info.data.insert(
            "error".to_owned(),
            json!({
                "name": "AbortError",
                "data": {
                    "message": INTERRUPTED_TURN_NOTICE,
                    "source": "acp",
                    "reason": "user_cancel"
                }
            }),
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);
        assert_eq!(replay.updates.len(), 1);
        assert_eq!(
            replay.updates[0]["_meta"]["zuno"]["kind"],
            "turn_interrupted"
        );
        assert_eq!(replay.updates[0]["_meta"]["zuno"]["source"], "acp");
        assert_eq!(replay.updates[0]["_meta"]["zuno"]["reason"], "user_cancel");
    }

    #[test]
    fn replay_deduplicates_a_provider_reasoning_capsule_from_visible_reasoning() {
        let root = tempfile::tempdir().expect("replay root");
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![
                part(
                    "p-thought",
                    "msg-assistant",
                    json!({"type":"reasoning","text":"inspect durable state"}),
                ),
                part(
                    "p-provider-reasoning",
                    "msg-assistant",
                    json!({
                        "type": "reasoning",
                        "text": "inspect durable state",
                        "metadata": {
                            "providerReasoning": {
                                "id": "rs_1",
                                "summary": ["inspect durable state"],
                                "encryptedContent": "sealed",
                                "status": "completed"
                            }
                        }
                    }),
                ),
            ],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);
        let thoughts = replay
            .updates
            .iter()
            .filter(|update| update["sessionUpdate"] == "agent_thought_chunk")
            .collect::<Vec<_>>();

        assert_eq!(
            thoughts.len(),
            1,
            "provider replay data must not duplicate visible reasoning: {thoughts:#?}"
        );
        assert_eq!(thoughts[0]["content"]["text"], "inspect durable state");
    }

    #[test]
    fn replay_keeps_provider_reasoning_when_no_visible_summary_exists() {
        let root = tempfile::tempdir().expect("replay root");
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![part(
                "p-provider-reasoning",
                "msg-assistant",
                json!({
                    "type": "reasoning",
                    "text": "provider-only reasoning",
                    "metadata": {
                        "providerReasoning": {
                            "id": "rs_1",
                            "summary": ["provider-only reasoning"],
                            "encryptedContent": "sealed",
                            "status": "completed"
                        }
                    }
                }),
            )],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.updates.len(), 1);
        assert_eq!(replay.updates[0]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(
            replay.updates[0]["content"]["text"],
            "provider-only reasoning"
        );
    }

    #[test]
    fn replay_preserves_host_generated_task_report_metadata() {
        let root = tempfile::tempdir().expect("replay root");
        let mut report = message(
            "input-report",
            "user",
            vec![part(
                "p-report",
                "input-report",
                json!({"type":"text","text":"background result"}),
            )],
        );
        report.info.data.insert(
            zuno_db::message::TASK_REPORT_METADATA_KEY.to_owned(),
            json!({
                "schemaVersion": 1,
                "jobId": "job-1",
                "sessionId": "ses-child",
                "agent": "explorer",
                "status": "completed",
                "finalText": "background result",
                "changedPaths": ["src/lib.rs"]
            }),
        );

        let replay = durable_updates(&[report], &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.updates.len(), 1);
        assert_eq!(replay.updates[0]["content"]["text"], "background result");
        assert_eq!(replay.updates[0]["_meta"]["zuno"]["kind"], "task_report");
        assert_eq!(
            replay.updates[0]["_meta"]["zuno"]["taskReport"]["jobId"],
            "job-1"
        );
        assert_eq!(
            replay.updates[0]["_meta"]["zuno"]["taskReport"]["changedPaths"],
            json!(["src/lib.rs"])
        );
    }

    #[test]
    fn replay_keeps_shell_commands_copyable_and_interpreters_separate() {
        let root = tempfile::tempdir().expect("replay root");
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![part(
                "p-shell",
                "msg-assistant",
                json!({
                    "type": "tool",
                    "callID": "call-shell",
                    "tool": "shell",
                    "displayName": "zsh",
                    "state": {
                        "status": "completed",
                        "raw": r#"{"command":"git diff --check"}"#,
                        "title": "zsh git diff --check",
                        "output": "(no output)",
                        "metadata": {
                            "shell": "zsh"
                        }
                    }
                }),
            )],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.updates[0]["title"], "git diff --check");
        assert_eq!(replay.updates[0]["_meta"]["zuno"]["interpreter"], "zsh");
        assert_eq!(replay.updates[1]["title"], "git diff --check");
        assert_eq!(replay.updates[1]["_meta"]["zuno"]["interpreter"], "zsh");
    }

    #[test]
    fn replay_renders_a_completed_question_as_a_static_answer_card() {
        let root = tempfile::tempdir().expect("replay root");
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![part(
                "p-question",
                "msg-assistant",
                json!({
                    "type": "tool",
                    "callID": "call-question",
                    "tool": "question",
                    "displayName": "Question",
                    "state": {
                        "status": "completed",
                        "raw": serde_json::to_string(&json!({
                            "questions": [{
                                "header": "Database",
                                "question": "Which database?",
                                "options": [
                                    {"label": "Postgres", "description": "Relational"},
                                    {"label": "SQLite", "description": "Embedded"}
                                ]
                            }]
                        }))
                        .expect("raw input"),
                        "title": "Answered · 1 question · 15s",
                        "output": "User has answered your questions.",
                        "metadata": {
                            "answers": [["SQLite"]],
                            "questionStatus": "answered",
                            "questionCount": 1,
                            "elapsedMs": 15_000
                        }
                    }
                }),
            )],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.updates.len(), 2);
        let started = &replay.updates[0];
        assert_eq!(started["title"], "Question · Database");
        assert_eq!(
            started["rawInput"]["questions"][0]["question"],
            "Which database?"
        );
        assert_eq!(started["_meta"]["zuno"]["question"]["status"], "pending");
        let prompt = started["content"][0]["content"]["text"]
            .as_str()
            .expect("static question prompt");
        assert!(prompt.contains("Which database?"), "{prompt}");
        assert!(prompt.contains("Postgres"), "{prompt}");
        assert!(prompt.contains("SQLite"), "{prompt}");

        let completed = &replay.updates[1];
        assert_eq!(completed["title"], "Answered · 1 question · 15s");
        assert_eq!(
            completed["_meta"]["zuno"]["question"]["answers"],
            json!([["SQLite"]])
        );
        assert_eq!(completed["_meta"]["zuno"]["question"]["status"], "answered");
        let card = completed["content"][0]["content"]["text"]
            .as_str()
            .expect("static answered question card");
        assert!(card.contains("Which database?"), "{card}");
        assert!(card.contains("Selected: SQLite"), "{card}");
        assert!(card.contains("Status: answered"), "{card}");
    }

    #[test]
    fn replay_renders_an_unfinished_question_without_reopening_elicitation() {
        let root = tempfile::tempdir().expect("replay root");
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![part(
                "p-question",
                "msg-assistant",
                json!({
                    "type": "tool",
                    "callID": "call-question",
                    "tool": "question",
                    "displayName": "Question",
                    "state": {
                        "status": "running",
                        "input": {
                            "questions": [{
                                "header": "Scope",
                                "question": "How deep?",
                                "multiple": true,
                                "options": [
                                    {"label": "Focused", "description": "One surface"},
                                    {"label": "Complete", "description": "All ACP surfaces"}
                                ]
                            }]
                        }
                    }
                }),
            )],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.updates.len(), 1);
        assert_eq!(replay.updates[0]["sessionUpdate"], "tool_call");
        assert_eq!(replay.updates[0]["status"], "in_progress");
        assert_eq!(
            replay.updates[0]["_meta"]["zuno"]["question"]["status"],
            "pending"
        );
        assert!(
            replay.updates[0]["content"][0]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("How deep?") && text.contains("Complete"))
        );
    }

    #[test]
    fn replay_renders_a_delegation_as_a_typed_subagent_card() {
        let root = tempfile::tempdir().expect("replay root");
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![part(
                "p-task",
                "msg-assistant",
                json!({
                    "type": "tool",
                    "callID": "call-task",
                    "tool": "task",
                    "displayName": "Delegate",
                    "state": {
                        "status": "completed",
                        "raw": serde_json::to_string(&json!({
                            "objective": "Inspect tree",
                            "deliverable": "A child-session call-chain report.",
                            "instructions": "Inspect the ACP child-session call chain.",
                            "success_evidence": "Name the durable events and projection functions.",
                            "scope": {
                                "include": ["crates/zuno-acp/**"],
                                "exclude": ["target/**"]
                            },
                            "constraints": {
                                "must": ["Remain read-only"],
                                "must_not": ["Edit files"]
                            },
                            "dependencies": ["The CodeGraph index is current"],
                            "agent": "explorer",
                            "background": false
                        }))
                        .expect("raw input"),
                        "title": "Inspect tree",
                        "output": "<task id=\"ses-child\" state=\"completed\">done</task>",
                        "metadata": {
                            "subagent": {
                                "sessionId": "ses-child",
                                "jobId": null,
                                "agent": "explorer",
                                "objective": "Inspect tree",
                                "deliverable": "A child-session call-chain report.",
                                "successEvidence": "Name the durable events and projection functions.",
                                "state": "completed",
                                "background": false,
                                "reportDelivery": "foreground",
                                "model": "test/test-model",
                                "effort": "high"
                            }
                        }
                    }
                }),
            )],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.updates.len(), 2);
        assert_eq!(
            replay.updates[0]["title"],
            "Delegate · explorer · Inspect tree"
        );
        assert!(
            replay.updates[0]["content"][0]["content"]["text"]
                .as_str()
                .is_some_and(|text| {
                    text.contains("Inspect the ACP child-session call chain.")
                        && text.contains("A child-session call-chain report.")
                        && text.contains("Name the durable events and projection functions.")
                        && text.contains("crates/zuno-acp/**")
                        && text.contains("Remain read-only")
                })
        );
        assert_eq!(
            replay.updates[1]["_meta"]["zuno"]["subagent"]["sessionId"],
            "ses-child"
        );
        assert_eq!(
            replay.updates[1]["_meta"]["zuno"]["subagent"]["state"],
            "completed"
        );
        let card = replay.updates[1]["content"][0]["content"]["text"]
            .as_str()
            .expect("subagent card");
        assert!(card.contains("Agent: explorer"), "{card}");
        assert!(card.contains("Session: ses-child"), "{card}");
        assert!(card.contains("State: completed"), "{card}");
        assert!(
            replay.updates[1]["content"]
                .as_array()
                .is_some_and(|content| content.iter().any(|item| {
                    item["content"]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("<task id=\"ses-child\""))
                })),
            "the model-visible task result remains available beside the presentation card"
        );
    }

    #[test]
    fn replay_filters_stale_and_external_actionable_paths() {
        let root = tempfile::tempdir().expect("replay root");
        let inside_path = root.path().join("inside.rs");
        std::fs::write(&inside_path, "inside\n").expect("write inside path");
        let outside_root = tempfile::tempdir().expect("outside root");
        let outside_path = outside_root.path().join("outside.rs");
        std::fs::write(&outside_path, "outside\n").expect("write outside path");
        let missing_path = root.path().join("missing.rs");
        let inside = inside_path.to_string_lossy().into_owned();
        let outside = outside_path.to_string_lossy().into_owned();
        let missing = missing_path.to_string_lossy().into_owned();
        let inside_wire = zuno_paths::wire_path(&inside_path);
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![part(
                "p-tool",
                "msg-assistant",
                json!({
                    "type": "tool",
                    "callID": "call-edit",
                    "tool": "edit",
                    "displayName": "Edit files",
                    "state": {
                        "status": "completed",
                        "title": "Edited files",
                        "output": "ok",
                        "metadata": {
                            "fileDiffs": [
                                {"path": inside, "oldText": "old\n", "newText": "inside\n"},
                                {"path": outside, "oldText": "old\n", "newText": "outside\n"},
                                {"path": missing, "oldText": "old\n", "newText": "missing\n"}
                            ],
                            "writtenPaths": [inside, outside, missing]
                        }
                    }
                }),
            )],
        );

        let replay = durable_updates(&[assistant], &ReplayPolicy::for_workspace(root.path()), 0);
        let completed = &replay.updates[1];

        assert_eq!(
            completed["locations"],
            json!([{"path":inside_wire.clone()}])
        );
        let diffs = completed["content"]
            .as_array()
            .expect("tool content")
            .iter()
            .filter(|item| item["type"] == "diff")
            .collect::<Vec<_>>();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0]["path"], inside_wire);
    }

    #[test]
    fn replay_downgrades_stale_and_external_file_links_to_non_actionable_text() {
        let root = tempfile::tempdir().expect("replay root");
        let outside_root = tempfile::tempdir().expect("outside root");
        let outside = outside_root.path().join("outside.md");
        std::fs::write(&outside, "outside\n").expect("write outside resource");
        let missing = root.path().join("missing.md");
        let outside_uri = Url::from_file_path(&outside)
            .expect("outside file URL")
            .to_string();
        let missing_uri = Url::from_file_path(&missing)
            .expect("missing file URL")
            .to_string();
        let user = message(
            "msg-user",
            "user",
            vec![
                part(
                    "p-outside",
                    "msg-user",
                    json!({
                        "type": "file",
                        "filename": "outside.md",
                        "mime": "text/markdown",
                        "url": outside_uri,
                    }),
                ),
                part(
                    "p-missing",
                    "msg-user",
                    json!({
                        "type": "file",
                        "filename": "missing.md",
                        "mime": "text/markdown",
                        "url": missing_uri,
                    }),
                ),
            ],
        );

        let replay = durable_updates(&[user], &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.updates.len(), 2);
        for update in replay.updates {
            assert_eq!(update["content"]["type"], "text");
            assert_eq!(
                update["content"]["_meta"]["zuno"]["kind"],
                "omitted_local_resource"
            );
            assert!(
                update["content"].get("uri").is_none(),
                "a stale local URI must not remain actionable: {update}"
            );
        }
    }

    #[test]
    fn replay_keeps_embedded_image_data_but_removes_an_external_local_uri() {
        let root = tempfile::tempdir().expect("replay root");
        let outside_root = tempfile::tempdir().expect("outside root");
        let outside = outside_root.path().join("outside.png");
        std::fs::write(&outside, b"png").expect("write outside image");
        let outside_uri = Url::from_file_path(&outside)
            .expect("outside image URL")
            .to_string();
        let user = message(
            "msg-user",
            "user",
            vec![part(
                "p-image",
                "msg-user",
                json!({
                    "type": "file",
                    "filename": "outside.png",
                    "mime": "image/png",
                    "data": "cG5n",
                    "url": outside_uri,
                }),
            )],
        );

        let replay = durable_updates(&[user], &ReplayPolicy::for_workspace(root.path()), 0);
        let content = &replay.updates[0]["content"];

        assert_eq!(content["type"], "image");
        assert_eq!(content["data"], "cG5n");
        assert!(
            content.get("uri").is_none(),
            "embedded bytes are safe, but the external local URI must not remain actionable"
        );
    }

    #[test]
    fn replay_budget_keeps_memory_bounded_and_reports_omitted_messages() {
        let root = tempfile::tempdir().expect("replay root");
        let history = (0..6)
            .map(|index| {
                message(
                    &format!("msg-{index}"),
                    "assistant",
                    vec![part(
                        &format!("part-{index}"),
                        &format!("msg-{index}"),
                        json!({"type":"text","text":"x".repeat(300)}),
                    )],
                )
            })
            .collect::<Vec<_>>();
        let policy = ReplayPolicy::with_limits(root.path(), 1_024, 512);

        let replay = durable_updates(&history, &policy, 4);
        let encoded = replay
            .updates
            .iter()
            .map(|update| {
                serde_json::to_vec(update)
                    .expect("encode replay update")
                    .len()
            })
            .sum::<usize>();

        assert!(replay.omitted_messages >= 4);
        assert!(encoded <= 1_024, "bounded replay encoded {encoded} bytes");
        assert_eq!(
            replay.updates[0]["_meta"]["zuno"]["kind"],
            "replay_omission"
        );
    }

    #[test]
    fn replay_keeps_a_contiguous_newest_suffix_when_one_message_is_too_large() {
        let root = tempfile::tempdir().expect("replay root");
        let history = vec![
            message(
                "msg-old",
                "assistant",
                vec![part(
                    "part-old",
                    "msg-old",
                    json!({"type":"text","text":"old"}),
                )],
            ),
            message(
                "msg-oversized",
                "assistant",
                vec![part(
                    "part-oversized",
                    "msg-oversized",
                    json!({"type":"text","text":"x".repeat(2_048)}),
                )],
            ),
            message(
                "msg-new",
                "assistant",
                vec![part(
                    "part-new",
                    "msg-new",
                    json!({"type":"text","text":"new"}),
                )],
            ),
        ];
        let policy = ReplayPolicy::with_limits(root.path(), 8 * 1_024, 512);

        let replay = durable_updates(&history, &policy, 0);

        assert_eq!(replay.omitted_messages, 2);
        assert_eq!(replay.updates.len(), 2);
        assert_eq!(
            replay.updates[0]["_meta"]["zuno"]["kind"],
            "replay_omission"
        );
        assert_eq!(replay.updates[1]["messageId"], "msg-new");
    }

    #[test]
    fn default_replay_frame_accepts_the_largest_supported_prompt_image() {
        let root = tempfile::tempdir().expect("replay root");
        let encoded_len = (5 * 1_024 * 1_024 / 3 + 1) * 4;
        let history = vec![message(
            "msg-image",
            "user",
            vec![part(
                "part-image",
                "msg-image",
                json!({
                    "type": "file",
                    "filename": "large.png",
                    "mime": "image/png",
                    "data": "A".repeat(encoded_len),
                }),
            )],
        )];

        let replay = durable_updates(&history, &ReplayPolicy::for_workspace(root.path()), 0);

        assert_eq!(replay.omitted_messages, 0);
        assert_eq!(replay.updates.len(), 1);
        assert_eq!(replay.updates[0]["content"]["type"], "image");
        assert_eq!(
            replay.updates[0]["content"]["data"]
                .as_str()
                .expect("embedded image data")
                .len(),
            encoded_len
        );
    }

    #[test]
    fn replay_usage_is_the_latest_context_not_cumulative_session_tokens() {
        let mut assistant = message("msg-usage", "assistant", Vec::new());
        assistant.info.data.insert(
            "tokens".to_owned(),
            json!({
                "input": 100,
                "output": 25,
                "reasoning": 0,
                "cache": {"read": 40, "write": 10},
                "accounting": "cache-beside-input"
            }),
        );
        let update = durable_usage_update(&[assistant], 200_000, 1.25)
            .expect("reported latest-message usage");
        assert_eq!(update["sessionUpdate"], "usage_update");
        assert_eq!(update["used"], 175);
        assert_eq!(update["size"], 200_000);
        assert_eq!(update["cost"], json!({"amount":1.25,"currency":"USD"}));
    }

    #[test]
    fn replay_plan_is_a_complete_stable_snapshot_with_todo_priority() {
        let work = zuno_types::WorkStateProjection {
            plan: Some(zuno_types::PlanProjection {
                id: "plan-1".to_owned(),
                goal_id: None,
                parent_plan_id: None,
                stack_depth: 0,
                revision: 3,
                title: "Ship ACP".to_owned(),
                steps: vec![
                    zuno_types::PlanStepProjection {
                        id: "implement".to_owned(),
                        title: "Implement replay".to_owned(),
                        status: "in_progress".to_owned(),
                    },
                    zuno_types::PlanStepProjection {
                        id: "verify".to_owned(),
                        title: "Verify in Zed".to_owned(),
                        status: "superseded".to_owned(),
                    },
                ],
                span: zuno_types::ExecutionSpan::default(),
                time_created: 1,
                time_updated: 2,
            }),
            todos: vec![zuno_types::TodoProjection {
                id: "todo-1".to_owned(),
                goal_id: None,
                plan_step_id: Some("implement".to_owned()),
                parent_id: None,
                subject: "Implement durable projector".to_owned(),
                description: String::new(),
                active_form: None,
                status: "in_progress".to_owned(),
                priority: "high".to_owned(),
                dependencies: Vec::new(),
                owner: None,
                revision: 1,
                span: zuno_types::ExecutionSpan::default(),
                time_created: 1,
                time_updated: 2,
            }],
            ..zuno_types::WorkStateProjection::default()
        };
        let update = durable_plan_update(&work).expect("durable plan");
        assert_eq!(update["sessionUpdate"], "plan");
        assert_eq!(update["entries"].as_array().map(Vec::len), Some(2));
        assert_eq!(update["entries"][0]["status"], "in_progress");
        assert_eq!(update["entries"][0]["priority"], "high");
        assert_eq!(update["entries"][1]["status"], "completed");
        assert_eq!(
            update["entries"][1]["_meta"]["zuno"]["outcome"],
            "superseded"
        );
        assert_eq!(update["entries"][1]["priority"], "medium");
        assert_eq!(update["_meta"]["zuno"]["planId"], "plan-1");
        assert_eq!(update["_meta"]["zuno"]["revision"], 3);
    }

    #[test]
    fn replay_plan_fails_closed_on_an_unrepresentable_status() {
        let work = zuno_types::WorkStateProjection {
            plan: Some(zuno_types::PlanProjection {
                id: "plan-1".to_owned(),
                goal_id: None,
                parent_plan_id: None,
                stack_depth: 0,
                revision: 1,
                title: "Invalid".to_owned(),
                steps: vec![zuno_types::PlanStepProjection {
                    id: "blocked".to_owned(),
                    title: "Blocked".to_owned(),
                    status: "blocked".to_owned(),
                }],
                span: zuno_types::ExecutionSpan::default(),
                time_created: 1,
                time_updated: 1,
            }),
            ..zuno_types::WorkStateProjection::default()
        };
        assert!(durable_plan_update(&work).is_none());
    }

    #[test]
    fn replay_learning_uses_metadata_only_session_info_update() {
        let work = zuno_types::WorkStateProjection {
            learning: zuno_types::LearningStateProjection {
                experiences: vec![zuno_types::ExperienceProjection {
                    id: "exp-1".to_owned(),
                    project_id: "prj-1".to_owned(),
                    session_id: Some("ses-1".to_owned()),
                    source_message_id: Some("msg-1".to_owned()),
                    kind: zuno_types::ExperienceKind::Procedure,
                    title: "Verify before applying".to_owned(),
                    summary: "Run the focused gate first.".to_owned(),
                    resolution: Some("The focused gate passed.".to_owned()),
                    confidence: 9_500,
                    status: zuno_types::ExperienceStatus::Active,
                    promoted_memory_candidate_id: None,
                    time_created: 1,
                    time_updated: 2,
                }],
                ..zuno_types::LearningStateProjection::default()
            },
            ..zuno_types::WorkStateProjection::default()
        };

        let update = durable_learning_update(&work);

        assert_eq!(update["sessionUpdate"], "session_info_update");
        assert!(update.get("title").is_none());
        assert_eq!(
            update["_meta"]["zuno"]["learning"]["experiences"][0]["kind"],
            "procedure"
        );
        assert_eq!(
            update["_meta"]["zuno"]["learning"]["experiences"][0]["sourceMessageId"],
            "msg-1"
        );
    }
}
