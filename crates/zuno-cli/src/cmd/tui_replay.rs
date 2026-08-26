//! Put a resumed session's persisted history back on screen.
//!
//! # What this fixes
//!
//! `run_turn` rehydrates the whole retained session from the database before it builds
//! a request (`zuno-engine/src/loop.rs`), while the TUI used to construct its
//! `TranscriptView` empty and seed only the *input* history. So `zuno -s <id>` showed a
//! welcome screen, and the first reply quoted a conversation that was nowhere on it —
//! the least defensible failure an interactive surface can have, because the user has
//! no way to tell a confused model from a lying screen.
//!
//! # Why the read is shared and the projection is not
//!
//! [`TurnHost::resumed_history`](super::turn::TurnHost::resumed_history) calls the very
//! function the next request calls, so the rows, their order and the compaction boundary
//! are one decision. That is where screen and model are made to agree.
//!
//! The *projection* is deliberately separate, because
//! [`zuno_engine::r#loop::project_history_owned`] answers a different question and
//! answering it for the screen would be wrong three ways:
//!
//! 1. it splits one stored assistant message into an assistant message plus a
//!    `tool`-role message, which on screen would detach every tool result from the call
//!    it belongs to;
//! 2. it drops unsigned reasoning, because a provider will not accept it back — but the
//!    user *saw* it, and a resumed screen that silently loses it has lost real content;
//! 3. it drops a tool call whose `state` never completed, which is exactly the call a
//!    user resuming after an interruption is looking for.
//!
//! So the agreement that matters is "the same stored messages, in the same order", and
//! that is asserted directly against `project_history_owned_with_ids` in the tests
//! rather than left as a comment.

use serde_json::Value;
use std::collections::BTreeSet;
use zuno_db::message::{MessageRole, MessageWithParts, PartKind, PartRecord};
use zuno_engine::r#loop::INTERRUPTED_TURN_NOTICE;
use zuno_tui::views::message::{Message, MessagePart, Role, ToolStatus};
use zuno_tui::views::toast::ToastLevel;

/// Stored messages one resume will put on screen, newest-last.
///
/// 512 because it is one of the transcript sizes `zuno-tui/tests/render_cost.rs` already
/// sweeps, so it is a size this project has a *measured* frame cost for rather than a
/// guess; the same sweep's largest point is the 931-message subject the memory gates
/// measure, which is the ceiling this sits under.
///
/// A cap is needed at all because a long-lived session is unbounded while a frame's cost
/// grows with the transcript, and a resume is the one moment the whole thing arrives at
/// once. Exceeding it is **reported**, never silent — see [`Replay::omitted`]. A reader
/// who scrolls up and finds history that simply stops has been misled about what the
/// session contains; one who finds a line saying how many turns are missing has not.
pub(crate) const RESUME_MESSAGE_CAP: usize = 512;

/// What a resume put on screen, and what it could not.
pub(crate) struct Replay {
    /// The transcript messages, in stored order.
    pub(crate) messages: Vec<Message>,
    /// Stored messages the cap dropped off the front, or zero when nothing was dropped.
    pub(crate) omitted: usize,
}

impl Replay {
    /// The notice naming what the cap dropped, or [`None`] when it dropped nothing.
    ///
    /// Warning level rather than informational: this is a fact about the screen being
    /// incomplete, which is something the user may need to act on by exporting the
    /// session. `§11.5` reserves the informational colour for confirmations.
    pub(crate) fn omission_notice(&self) -> Option<Message> {
        (self.omitted > 0).then(|| {
            Message::notice(format!(
                "warning: earlier turns not shown — this session has more than \
                 {RESUME_MESSAGE_CAP} stored messages, so the oldest {} are omitted from \
                 the transcript; the model still receives whatever fits its context window",
                self.omitted,
            ))
        })
    }
}

/// Project a session's stored history onto transcript messages.
///
/// A stored message that projects to no renderable part is skipped rather than shown as
/// an empty framed box: an assistant step that recorded only `step-start` and
/// `step-finish` is real in the database and has nothing to say on screen, and a blank
/// frame would read as content that failed to load.
pub(crate) fn project(history: Vec<MessageWithParts>) -> Replay {
    let total = history.len();
    let omitted = total.saturating_sub(RESUME_MESSAGE_CAP);
    let messages = history
        .into_iter()
        .skip(omitted)
        .flat_map(project_message)
        .collect();
    Replay { messages, omitted }
}

/// One stored message as the transcript would hold it.
///
/// A message-level failure becomes a separate session-owned notice. It is not assistant
/// content and therefore must not be nested inside the partial reply it explains.
fn project_message(stored: MessageWithParts) -> Vec<Message> {
    let role = match stored.info.role {
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
    };
    let failure = stored.info.data.get("error").and_then(error_notice);
    let visible_reasoning = stored
        .parts
        .iter()
        .filter(|part| part.kind == PartKind::Reasoning && !is_provider_reasoning(part))
        .filter_map(|part| text(&part.data))
        .collect::<BTreeSet<_>>();
    let parts = stored
        .parts
        .into_iter()
        .filter(|part| {
            !(is_provider_reasoning(part)
                && text(&part.data).is_some_and(|text| visible_reasoning.contains(&text)))
        })
        .filter_map(project_part)
        .collect::<Vec<_>>();
    let mut messages = Vec::with_capacity(2);
    if !parts.is_empty() {
        messages.push(Message {
            role,
            id: Some(stored.info.id),
            parts,
        });
    }
    if let Some(failure) = failure {
        messages.push(Message::noticed(ToastLevel::Error, failure));
    }
    messages
}

fn is_provider_reasoning(part: &PartRecord) -> bool {
    part.data
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| metadata.contains_key("providerReasoning"))
}

/// One stored part as the transcript would hold it, or [`None`] when it has no on-screen form.
///
/// The dropped kinds are dropped for stated reasons, not for lack of attention:
///
/// - `step-start` / `step-finish` / `snapshot` / `agent` / `subtask` have no rendered
///   form in `zuno-tui`; they are bookkeeping the transcript never displayed live either.
/// - `patch` is dropped because the unified diff already travels on the tool part that
///   produced it (`state.metadata.diff`), and rendering both would show every change twice.
/// - `retry` is dropped because [`MessagePart::Retry`] is an *in-flight* affordance — "a
///   replay is waiting or starting now". A retry that resolved during a previous process
///   is not waiting, and replaying it would animate a wait that already ended.
/// - `compaction` is a boundary marker, and on a successful compaction the retained
///   history does not even contain it: `hydrate_retained_history` drains everything
///   before the tail.
fn project_part(part: PartRecord) -> Option<MessagePart> {
    match part.kind {
        PartKind::Text => text(&part.data).map(|text| MessagePart::Text { text }),
        PartKind::Reasoning => reasoning(&part.data),
        PartKind::Tool => tool(&part.data),
        PartKind::File => Some(attachment(&part.data)),
        PartKind::StepStart
        | PartKind::StepFinish
        | PartKind::Snapshot
        | PartKind::Agent
        | PartKind::Subtask
        | PartKind::Patch
        | PartKind::Retry
        | PartKind::Compaction => None,
    }
}

/// A non-empty `text` field, or [`None`].
///
/// Empty is treated as absent rather than rendered, because the streaming projection
/// flushes a text part as soon as it opens and a turn abandoned before its first delta
/// leaves one behind holding `""`.
fn text(data: &serde_json::Map<String, Value>) -> Option<String> {
    data.get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A reasoning block, with its measured duration when the stored span has one.
fn reasoning(data: &serde_json::Map<String, Value>) -> Option<MessagePart> {
    let text = text(data)?;
    Some(MessagePart::Reasoning {
        text,
        duration_secs: duration_secs(data),
        // Whatever was streaming when the process ended is not streaming now, and a
        // spinner that never stops is the one thing worse than no spinner.
        streaming: false,
    })
}

/// The stored `time.{start,end}` span in seconds, when both ends are present and ordered.
fn duration_secs(data: &serde_json::Map<String, Value>) -> Option<f64> {
    let time = data.get("time")?.as_object()?;
    let start = time.get("start")?.as_i64()?;
    let end = time.get("end")?.as_i64()?;
    let elapsed = end.checked_sub(start)?;
    (elapsed > 0).then(|| elapsed as f64 / 1000.0)
}

/// A tool call with whatever the stored `state` reached.
///
/// `callID` and `tool` are the two fields with no honest default — a call with no id
/// cannot be matched to its result and a call with no name cannot be labelled — so their
/// absence drops the part, matching what
/// [`zuno_engine::r#loop::project_history_owned`] does with the same rows.
fn tool(data: &serde_json::Map<String, Value>) -> Option<MessagePart> {
    let call_id = data.get("callID")?.as_str()?.to_owned();
    let name = data.get("tool")?.as_str()?.to_owned();
    let state = data.get("state").and_then(Value::as_object);
    let status = match state
        .and_then(|state| state.get("status"))
        .and_then(Value::as_str)
    {
        Some("completed") => ToolStatus::Completed,
        Some("error")
            if state
                .and_then(|state| state.get("outcome"))
                .and_then(Value::as_str)
                == Some("blocked") =>
        {
            ToolStatus::Blocked
        }
        Some("error") => ToolStatus::Error,
        Some("running") => ToolStatus::Running,
        // `pending`, an unknown status, and a missing `state` all mean the same thing on
        // resume: the call was recorded and never resolved. `Pending` is the honest glyph
        // for it, and it is what makes an interrupted turn legible rather than absent.
        _ => ToolStatus::Pending,
    };
    let output = state
        .and_then(|state| {
            state
                .get("output")
                .or_else(|| state.get("error"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);
    Some(MessagePart::Tool {
        call_id,
        display_name: data
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(&name)
            .to_owned(),
        name,
        ui_intent: data
            .get("uiIntent")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
        // The stored `input` object re-serialised, which is what the live transcript
        // accumulates from the provider's input deltas and what
        // `zuno_tui::views::tool::summary` parses to name the file a `read` read.
        arguments: state
            .and_then(|state| state.get("input"))
            .map_or_else(String::new, ToString::to_string),
        title: state
            .and_then(|state| state.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        status,
        output,
        diff: state
            .and_then(|state| state.get("metadata"))
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get(zuno_tools::diff::METADATA_DIFF_KEY))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// An attachment, labelled with the best name the stored part carries.
///
/// A user-attached image is persisted with a `data:` URL and no `filename`
/// (`turn.rs`), so falling back to the URL would put a base64 blob on screen. The mime
/// type is the honest label in that case: it says what the thing is without pretending
/// to know what it was called.
fn attachment(data: &serde_json::Map<String, Value>) -> MessagePart {
    let mime = data.get("mime").and_then(Value::as_str).map(str::to_owned);
    let filename = data
        .get("filename")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            data.get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.starts_with("data:"))
                .map(|url| url.rsplit('/').next().unwrap_or(url).to_owned())
        })
        .or_else(|| mime.clone())
        .unwrap_or_else(|| String::from("attachment"));
    MessagePart::Attachment { filename, mime }
}

/// A one-line summary of a stored message-level failure.
///
/// The persisted shape is upstream's `{ name, data: { message } }`, so the message is
/// preferred and the name is the fallback; a bare string is accepted because an older
/// row may hold one. An empty or shapeless value yields [`None`] rather than an empty
/// notice row.
fn error_summary(error: &Value) -> Option<String> {
    let text = match error {
        Value::String(text) => text.clone(),
        Value::Object(object) => object
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| data.get("message"))
            .and_then(Value::as_str)
            .or_else(|| object.get("name").and_then(Value::as_str))
            .map(str::to_owned)?,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| format!("this turn ended early: {text}"))
}

/// The session-owned notice for a stored message-level failure.
///
/// Current Zuno checkpoints use `AbortError`; imported sessions may still carry
/// `MessageAbortedError`. Both name the same user action and are normalised to the
/// stable live marker instead of exposing provider/storage wording.
fn error_notice(error: &Value) -> Option<String> {
    let interruption = error
        .as_object()
        .and_then(|object| object.get("name"))
        .and_then(Value::as_str)
        .is_some_and(|name| matches!(name, "AbortError" | "MessageAbortedError"));
    if interruption {
        Some(INTERRUPTED_TURN_NOTICE.to_owned())
    } else {
        error_summary(error)
    }
}
/// The notice shown when a session's stored history could not be read at all.
///
/// A resume must open. `hydrate_retained_history` fails whole rather than per part — one
/// undecodable `part.data` blob fails the query for the session — so the alternative to
/// this notice is refusing to open a session whose *next turn* would fail the same way
/// and say so properly. Opening with an empty transcript and no explanation is the one
/// outcome ruled out: that is the original defect wearing a different hat.
pub(crate) fn failure_notice(session_id: &str, error: &zuno_error::DbError) -> Message {
    Message::noticed(
        ToastLevel::Error,
        format!(
            "error: session {session_id} history could not be read, so this transcript \
             starts empty although the model still has it: {}",
            zuno_error::source::describe(error),
        ),
    )
}

#[cfg(test)]
#[path = "tui_replay_tests.rs"]
mod tests;
