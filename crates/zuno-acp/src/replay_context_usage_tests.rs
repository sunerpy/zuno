use serde_json::{Value, json};
use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};
use zuno_types::context_usage::{
    ContextRequestIdentity, ContextTokenAccounting, ContextUsageCounters, ContextUsageSource,
    ContextUsageTracker,
};

use super::{durable_context_usage_update, durable_usage_update};
use crate::TurnEventProjector;

fn message(id: &str, role: &str, parts: Vec<Value>) -> MessageWithParts {
    MessageWithParts {
        info: MessageRecord::from_json(json!({
            "id": id,
            "sessionID": "ses_synthetic",
            "role": role,
            "providerID": "synthetic-provider",
            "modelID": "synthetic-model",
            "time": {"created": 1},
        }))
        .unwrap(),
        parts: parts
            .into_iter()
            .enumerate()
            .map(|(index, data)| {
                let mut data = data.as_object().unwrap().clone();
                data.insert("id".to_owned(), json!(format!("{id}-part-{index}")));
                data.insert("sessionID".to_owned(), json!("ses_synthetic"));
                data.insert("messageID".to_owned(), json!(id));
                PartRecord::from_json(Value::Object(data), 1).unwrap()
            })
            .collect(),
    }
}

fn assistant(id: &str, accounting: &str) -> MessageWithParts {
    let mut message = message(id, "assistant", Vec::new());
    message.info.data.insert(
        "tokens".to_owned(),
        json!({
            "input": 100,
            "output": 15,
            "reasoning": 10,
            "cache": {"read": 40, "write": 10},
            "accounting": accounting,
        }),
    );
    message
}

fn request(sequence: u64) -> ContextRequestIdentity {
    ContextRequestIdentity {
        request_id: format!("synthetic-request-{sequence}"),
        request_sequence: sequence,
        attempt: 1,
        context_epoch: 0,
        provider_id: "synthetic-provider".to_owned(),
        model_id: "synthetic-model".to_owned(),
        source: ContextUsageSource::Main,
        turn_id: Some("synthetic-turn".to_owned()),
        time_started: i64::try_from(sequence).unwrap(),
        request_context_tokens: None,
        history_prefix: None,
    }
}

#[test]
fn canonical_live_and_replayed_state_have_identical_context_semantics() {
    let mut tracker = ContextUsageTracker::new("ses_synthetic");
    tracker.start_request(request(1), Some(70_000), Some(0), Some(200_000), 1);
    tracker.observe_usage(
        &request(1),
        ContextUsageCounters {
            input_tokens: Some(125_350),
            output_tokens: Some(40),
            accounting: ContextTokenAccounting::CacheInsideInput,
            ..ContextUsageCounters::default()
        },
        2,
    );
    tracker.commit_request(&request(1), 3);
    tracker.start_request(request(2), Some(73_948), Some(3_000), Some(200_000), 4);
    let encoded = serde_json::to_vec(&tracker).unwrap();
    let restored: ContextUsageTracker = serde_json::from_slice(&encoded).unwrap();
    restored.validate().unwrap();
    let live = TurnEventProjector::new()
        .project_context_usage(restored.snapshot())
        .unwrap();
    let mut replay = durable_context_usage_update(restored.snapshot(), 1.25).unwrap();
    assert_eq!(replay["used"], 128_390);
    assert_eq!(
        replay["_meta"]["zuno"]["contextUsage"]["freshness"],
        "estimated"
    );
    assert_eq!(replay["cost"], json!({"amount": 1.25, "currency": "USD"}));
    replay.as_object_mut().unwrap().remove("cost");
    assert_eq!(replay, live);
}

#[test]
fn historical_replay_restores_disjoint_reasoning_and_cache_splits_once() {
    for (accounting, expected) in [("cache-inside-input", 125), ("cache-beside-input", 175)] {
        let replay =
            durable_usage_update(&[assistant("assistant", accounting)], 200_000, 0.0).unwrap();
        assert_eq!(replay["used"], expected);
        assert_eq!(
            replay["_meta"]["zuno"]["contextUsage"]["freshness"],
            "confirmed"
        );
        assert_eq!(
            replay["_meta"]["zuno"]["contextUsage"]["cumulativeKnown"],
            false
        );
        assert_eq!(
            replay["_meta"]["zuno"]["contextUsage"]["lastConfirmed"]["usage"]["outputTokens"],
            25
        );
    }
}

#[test]
fn replay_counts_returned_tool_content_after_the_measured_assistant() {
    let mut assistant = assistant("assistant", "cache-inside-input");
    let tool_parts = message(
        "assistant",
        "assistant",
        vec![json!({
            "type": "tool",
            "callID": "synthetic-read",
            "tool": "read",
            "state": {
                "status": "completed",
                "input": {"filePath": "/not-read/large-source.rs", "limit": 10},
                "output": "x".repeat(8_000),
                "metadata": {"sourceFileSize": 1_000_000_000_u64},
            },
        })],
    );
    assistant.parts = tool_parts.parts;
    let replay = durable_usage_update(&[assistant], 200_000, 0.0).unwrap();
    let used = replay["used"].as_u64().unwrap();
    assert!(
        used > 2_125,
        "the returned tool text must extend the confirmed 125-token baseline"
    );
    assert!(
        used < 2_500,
        "unread file bytes cannot be part of this request"
    );
    assert_eq!(
        replay["_meta"]["zuno"]["contextUsage"]["freshness"],
        "estimated"
    );
}

#[test]
fn historical_unknown_usage_never_becomes_a_zero_prompt() {
    let mut missing_input = message("assistant", "assistant", Vec::new());
    missing_input.info.data.insert(
        "tokens".to_owned(),
        json!({"output": 9, "accounting": "cache-inside-input"}),
    );
    let replay = durable_usage_update(&[missing_input], 200_000, 0.0).unwrap();
    assert_eq!(replay["sessionUpdate"], "session_info_update");
    assert!(replay.get("used").is_none());
    assert_eq!(
        replay["_meta"]["zuno"]["contextUsage"]["freshness"],
        "unknown"
    );
    assert!(replay["_meta"]["zuno"]["contextUsage"]["usedTokens"].is_null());
}

#[test]
fn learning_and_child_request_usage_cannot_replace_the_main_context() {
    let main = assistant("main", "cache-inside-input");
    let mut learning = assistant("learning", "cache-inside-input");
    learning
        .info
        .data
        .insert("requestPurpose".to_owned(), json!("learning"));
    learning.info.data["tokens"]["input"] = json!(800_000);
    let mut child = assistant("child", "cache-inside-input");
    child
        .info
        .data
        .insert("requestPurpose".to_owned(), json!("child-turn"));
    child.info.data["tokens"]["input"] = json!(900_000);
    let replay = durable_usage_update(&[main, learning, child], 200_000, 0.0).unwrap();
    assert_eq!(replay["used"], 125);
    assert_eq!(
        replay["_meta"]["zuno"]["contextUsage"]["lastConfirmed"]["request"]["requestId"],
        "main"
    );
}

#[test]
fn unmeasured_new_model_does_not_inherit_the_previous_models_confirmation() {
    let original = assistant("original", "cache-inside-input");
    let mut new_model = message("new-model", "assistant", Vec::new());
    new_model
        .info
        .data
        .insert("modelID".to_owned(), json!("another-model"));
    let replay = durable_usage_update(&[original, new_model], 32_000, 0.0).unwrap();
    assert_eq!(replay["sessionUpdate"], "session_info_update");
    assert!(replay["_meta"]["zuno"]["contextUsage"]["lastConfirmed"].is_null());
}

fn compaction_history() -> Vec<MessageWithParts> {
    let original = assistant("original", "cache-inside-input");
    let tail = message(
        "tail",
        "user",
        vec![json!({"type": "text", "text": "Keep this latest user instruction."})],
    );
    let marker = message(
        "marker",
        "user",
        vec![json!({"type": "compaction", "tail_start_id": "tail"})],
    );
    let mut summary = assistant("summary", "cache-inside-input");
    summary.parts = message(
        "summary",
        "assistant",
        vec![json!({"type": "text", "text": "A small durable compaction summary."})],
    )
    .parts;
    summary.info.data.insert("summary".to_owned(), json!(true));
    summary
        .info
        .data
        .insert("parentID".to_owned(), json!("marker"));
    summary.info.data.insert("finish".to_owned(), json!("stop"));
    summary.info.data["tokens"]["input"] = json!(500_000);
    vec![original, tail, marker, summary]
}

#[test]
fn compaction_summary_usage_is_not_the_new_foreground_context() {
    let mut history = compaction_history();
    let unknown = durable_usage_update(&history, 200_000, 0.0).unwrap();
    assert_eq!(unknown["sessionUpdate"], "session_info_update");
    assert!(unknown["_meta"]["zuno"]["contextUsage"]["lastConfirmed"].is_null());
    assert_eq!(
        unknown["_meta"]["zuno"]["contextUsage"]["freshness"],
        "unknown"
    );

    let mut compacted = assistant("compacted-main", "cache-inside-input");
    compacted.info.data["tokens"]["input"] = json!(1_100);
    history.push(compacted);
    let replay = durable_usage_update(&history, 200_000, 0.0).unwrap();
    assert_eq!(replay["used"], 1_125);
    assert!(
        replay["_meta"]["zuno"]["contextUsage"]["contextEpoch"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn failed_and_orphaned_compactions_do_not_reset_the_confirmed_baseline() {
    for orphaned in [false, true] {
        let mut history = compaction_history();
        // Keep the original measured assistant in the retained tail.
        history[2].parts[0]
            .data
            .insert("tail_start_id".to_owned(), json!("original"));
        if orphaned {
            history[3]
                .info
                .data
                .insert("parentID".to_owned(), json!("missing-marker"));
        } else {
            history[3]
                .info
                .data
                .insert("error".to_owned(), json!({"message": "failed"}));
        }
        let replay = durable_usage_update(&history, 200_000, 0.0).unwrap();
        assert!(replay["used"].as_u64().unwrap() >= 125);
        assert_eq!(
            replay["_meta"]["zuno"]["contextUsage"]["lastConfirmed"]["request"]["requestId"],
            "original"
        );
        assert_eq!(replay["_meta"]["zuno"]["contextUsage"]["contextEpoch"], 0);
    }
}

#[test]
fn invalid_canonical_snapshots_and_nonfinite_costs_are_not_projected_as_usage() {
    let mut tracker = ContextUsageTracker::new("ses_synthetic");
    tracker.start_request(request(1), Some(10), Some(0), Some(100), 1);
    for cost in [f64::NAN, f64::INFINITY, -1.0] {
        let update = durable_context_usage_update(tracker.snapshot(), cost).unwrap();
        assert!(update.get("cost").is_none());
    }
    let mut inconsistent = tracker.snapshot().clone();
    inconsistent.used_tokens = Some(999);
    assert!(durable_context_usage_update(&inconsistent, 0.0).is_none());
    assert!(durable_usage_update(&[], 200_000, 0.0).is_none());
    assert!(durable_usage_update(&[assistant("one", "cache-inside-input")], 0, 0.0).is_none());
}
