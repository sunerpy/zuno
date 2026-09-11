use zuno_acp::{AttemptBufferedTurnEventProjector, TurnEventProjector};
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::{PromptAccounting, StreamEvent};
use zuno_types::context_usage::{
    ContextRequestIdentity, ContextTokenAccounting, ContextUsageCounters, ContextUsageFreshness,
    ContextUsageSource, ContextUsageTracker,
};

fn request(step: u32, estimate: u64) -> TurnEvent {
    TurnEvent::ProviderRequestStarted {
        step,
        message_count: 2,
        estimated_prompt_tokens: estimate,
    }
}

fn usage(step: u32, input: Option<u64>, output: Option<u64>) -> TurnEvent {
    TurnEvent::Provider {
        step,
        event: StreamEvent::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
    }
}

#[test]
fn request_estimate_cannot_replace_a_larger_provider_confirmed_context() {
    let mut projector = TurnEventProjector::with_context_size(200_000);
    let _ = projector.project(&request(1, 70_000));
    let confirmed = projector
        .project(&usage(1, Some(125_350), Some(40)))
        .expect("provider usage");
    assert_eq!(confirmed["used"], 125_390);
    let _ = projector.project(&TurnEvent::AssistantCheckpointed {
        step: 1,
        message_id: "synthetic-assistant-1".to_owned(),
        interrupted: false,
    });

    let next = projector
        .project(&request(2, 73_948))
        .expect("next request usage");
    assert_eq!(
        next["used"], 125_390,
        "a fresh rough estimate cannot replace the confirmed baseline"
    );
    let confirmed = projector
        .project(&usage(2, Some(149_501), Some(9)))
        .expect("second provider usage");
    assert_eq!(confirmed["used"], 149_510);
}

#[test]
fn partial_usage_frames_preserve_the_other_fields_of_the_request_snapshot() {
    let mut projector = TurnEventProjector::with_context_size(200_000);
    let _ = projector.project(&request(1, 70_000));
    let _ = projector.project(&usage(1, Some(149_501), None));
    let final_usage = projector
        .project(&usage(1, None, Some(9)))
        .expect("partial final usage");
    assert_eq!(
        final_usage["used"], 149_510,
        "an output-only frame cannot replace the prompt with zero"
    );
}

fn canonical_request(sequence: u64) -> ContextRequestIdentity {
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

fn canonical_usage(input: u64, output: u64) -> ContextUsageCounters {
    ContextUsageCounters {
        input_tokens: Some(input),
        output_tokens: Some(output),
        accounting: ContextTokenAccounting::CacheInsideInput,
        ..ContextUsageCounters::default()
    }
}

fn confirmed_tracker() -> ContextUsageTracker {
    let mut tracker = ContextUsageTracker::new("ses_synthetic");
    tracker.start_request(
        canonical_request(1),
        Some(70_000),
        Some(0),
        Some(200_000),
        1,
    );
    tracker.observe_usage(&canonical_request(1), canonical_usage(125_350, 40), 2);
    tracker.commit_request(&canonical_request(1), 3);
    tracker
}

#[test]
fn canonical_snapshot_is_authoritative_over_raw_estimates_and_provider_frames() {
    let tracker = confirmed_tracker();
    let mut projector = TurnEventProjector::with_context_size(200_000);
    let update = projector.project_context_usage(tracker.snapshot()).unwrap();
    assert_eq!(update["used"], 125_390);
    assert_eq!(
        update["_meta"]["zuno"]["contextUsage"]["freshness"],
        "confirmed"
    );
    assert_eq!(
        update["_meta"]["zuno"]["contextUsage"]["sessionId"],
        "ses_synthetic"
    );
    assert_eq!(update["_meta"]["zuno"]["turnId"], "synthetic-turn");
    assert!(projector.project(&request(2, 73_948)).is_none());
    assert!(projector.project(&usage(2, Some(7), Some(1))).is_none());
    assert!(
        projector
            .project_context_usage(tracker.snapshot())
            .is_none()
    );
}

#[test]
fn canonical_revisions_reject_stale_epoch_other_source_and_other_session() {
    let mut tracker = confirmed_tracker();
    let old = tracker.snapshot().clone();
    let mut projector = TurnEventProjector::new();
    assert!(projector.project_context_usage(&old).is_some());
    tracker.reset_epoch(10, 4);
    let unknown = projector.project_context_usage(tracker.snapshot()).unwrap();
    assert_eq!(unknown["sessionUpdate"], "session_info_update");
    assert!(unknown.get("used").is_none());
    assert_eq!(
        unknown["_meta"]["zuno"]["contextUsage"]["freshness"],
        "unknown"
    );
    assert!(projector.project_context_usage(&old).is_none());

    let mut stale_epoch = old.clone();
    stale_epoch.revision = tracker.snapshot().revision + 10;
    assert!(projector.project_context_usage(&stale_epoch).is_none());
    let mut other_session = tracker.snapshot().clone();
    other_session.revision += 20;
    other_session.session_id = "ses_other".to_owned();
    assert!(projector.project_context_usage(&other_session).is_none());

    for source in [ContextUsageSource::Child, ContextUsageSource::Learning] {
        let mut other = ContextUsageTracker::for_source("ses_synthetic", source);
        other.reset_epoch(20, 5);
        assert!(projector.project_context_usage(other.snapshot()).is_none());
    }
}

#[test]
fn canonical_retry_state_can_decrease_before_append_only_content_is_committed() {
    let mut tracker = confirmed_tracker();
    let mut projector =
        AttemptBufferedTurnEventProjector::with_context_usage(tracker.snapshot().clone()).unwrap();
    tracker.start_request(
        canonical_request(2),
        Some(73_948),
        Some(2_000),
        Some(200_000),
        4,
    );
    let before = projector.project_context_usage(tracker.snapshot());
    assert_eq!(before[0]["used"], 127_390);
    assert!(projector.project(&request(2, 73_948)).is_empty());
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 2,
                event: StreamEvent::TextDelta("discard this provisional content".to_owned()),
            })
            .is_empty()
    );

    tracker.observe_usage(&canonical_request(2), canonical_usage(149_501, 9), 5);
    let provisional = projector.project_context_usage(tracker.snapshot());
    assert_eq!(provisional[0]["used"], 149_510);
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 2,
                event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
            })
            .is_empty()
    );
    tracker.rollback_request(&canonical_request(2), 2, 6);
    let restored = projector.project_context_usage(tracker.snapshot());
    assert_eq!(restored[0]["used"], 127_390);
    assert_eq!(
        restored[0]["_meta"]["zuno"]["contextUsage"]["request"]["attempt"],
        2
    );

    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 2,
                event: StreamEvent::TextDelta("only committed content".to_owned()),
            })
            .is_empty()
    );
    let committed = projector.project(&TurnEvent::AssistantCheckpointed {
        step: 2,
        message_id: "synthetic-message-2".to_owned(),
        interrupted: false,
    });
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0]["content"]["text"], "only committed content");
}

#[test]
fn raw_usage_state_is_revisable_while_text_remains_attempt_buffered() {
    let mut projector = AttemptBufferedTurnEventProjector::with_context_size(200_000);
    let _ = projector.project(&request(1, 70_000));
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 1,
                event: StreamEvent::TextDelta("discarded text".to_owned()),
            })
            .is_empty()
    );
    let measured = projector.project(&usage(1, Some(149_501), Some(9)));
    assert_eq!(measured.len(), 1);
    assert_eq!(measured[0]["used"], 149_510);
    let rolled_back = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
    });
    assert_eq!(rolled_back.len(), 1);
    assert_eq!(rolled_back[0]["used"], 70_000);
    assert_eq!(
        rolled_back[0]["_meta"]["zuno"]["contextUsage"]["freshness"],
        "estimated"
    );
    assert!(projector.project(&usage(0, Some(3), Some(1))).is_empty());
    assert!(
        projector
            .project(&TurnEvent::AssistantCheckpointed {
                step: 1,
                message_id: "synthetic-message-1".to_owned(),
                interrupted: false,
            })
            .is_empty()
    );
}

#[test]
fn live_cache_accounting_and_reasoning_survive_partial_frames() {
    for (accounting, expected) in [
        (PromptAccounting::CacheInsideInput, 125),
        (PromptAccounting::CacheBesideInput, 175),
    ] {
        let mut projector = TurnEventProjector::with_context_size(200_000);
        let _ = projector.project(&request(1, 80));
        let _ = projector.project(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::TokenUsage {
                input_tokens: Some(100),
                output_tokens: None,
                reasoning_tokens: None,
                cache_read_input_tokens: Some(40),
                cache_write_input_tokens: None,
                accounting,
            },
        });
        let final_frame = TurnEvent::Provider {
            step: 1,
            event: StreamEvent::TokenUsage {
                input_tokens: None,
                output_tokens: Some(25),
                reasoning_tokens: Some(10),
                cache_read_input_tokens: None,
                cache_write_input_tokens: Some(10),
                accounting,
            },
        };
        let measured = projector.project(&final_frame).unwrap();
        assert_eq!(measured["used"], expected);
        assert_eq!(
            measured["_meta"]["zuno"]["contextUsage"]["cumulativeUsage"]["output"],
            15
        );
        assert!(projector.project(&final_frame).is_none());
    }
}

#[test]
fn output_only_without_a_request_estimate_remains_explicitly_unknown() {
    let mut projector = TurnEventProjector::with_context_size(200_000);
    let update = projector.project(&usage(1, None, Some(9))).unwrap();
    assert_eq!(update["sessionUpdate"], "session_info_update");
    assert!(update.get("used").is_none());
    assert_eq!(
        update["_meta"]["zuno"]["contextUsage"]["freshness"],
        "unknown"
    );
    assert!(update["_meta"]["zuno"]["contextUsage"]["usedTokens"].is_null());
    assert!(
        TurnEventProjector::with_context_size(0)
            .project(&request(1, 10))
            .is_none()
    );
}

#[test]
fn a_large_unaccounted_tail_and_model_change_use_canonical_state() {
    let mut tracker = confirmed_tracker();
    let mut projector = TurnEventProjector::new();
    assert!(
        projector
            .project_context_usage(tracker.snapshot())
            .is_some()
    );
    tracker.start_request(
        canonical_request(2),
        Some(73_948),
        Some(100_000),
        Some(200_000),
        4,
    );
    let enlarged = projector.project_context_usage(tracker.snapshot()).unwrap();
    assert_eq!(enlarged["used"], 225_390);
    assert_eq!(enlarged["size"], 200_000);

    let mut changed_model = canonical_request(3);
    changed_model.model_id = "another-model".to_owned();
    tracker.start_request(changed_model, Some(1_000), Some(0), Some(32_000), 5);
    assert_eq!(
        tracker.snapshot().freshness,
        ContextUsageFreshness::Estimated
    );
    let changed = projector.project_context_usage(tracker.snapshot()).unwrap();
    assert_eq!(changed["used"], 1_000);
    assert_eq!(changed["size"], 32_000);
    assert!(changed["_meta"]["zuno"]["contextUsage"]["lastConfirmed"].is_null());
}

#[test]
fn compaction_can_lower_the_next_request_and_provider_context() {
    let mut tracker = confirmed_tracker();
    let mut projector = TurnEventProjector::new();
    let _ = projector.project_context_usage(tracker.snapshot());
    tracker.reset_epoch(20, 4);
    let cleared = projector.project_context_usage(tracker.snapshot()).unwrap();
    assert_eq!(cleared["sessionUpdate"], "session_info_update");
    let mut compacted = canonical_request(2);
    compacted.context_epoch = 20;
    tracker.start_request(compacted.clone(), Some(1_100), Some(0), Some(200_000), 5);
    let estimate = projector.project_context_usage(tracker.snapshot()).unwrap();
    assert_eq!(estimate["used"], 1_100);
    tracker.observe_usage(&compacted, canonical_usage(1_050, 5), 6);
    let confirmed = projector.project_context_usage(tracker.snapshot()).unwrap();
    assert_eq!(confirmed["used"], 1_055);
    assert_eq!(
        confirmed["_meta"]["zuno"]["contextUsage"]["freshness"],
        "confirmed"
    );
    assert_eq!(tracker.snapshot().cumulative_usage.total(), 126_445);
}

#[test]
fn ending_an_uncheckpointed_stream_revises_usage_without_releasing_its_text() {
    let mut projector = AttemptBufferedTurnEventProjector::with_context_size(200_000);
    let _ = projector.project(&request(1, 70_000));
    let _ = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::TextDelta("never checkpointed".to_owned()),
    });
    let measured = projector.project(&usage(1, Some(149_501), Some(9)));
    assert_eq!(measured[0]["used"], 149_510);
    let settled = projector.finish();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0]["sessionUpdate"], "usage_update");
    assert_eq!(settled[0]["used"], 70_000);
    assert!(settled[0].get("content").is_none());
    assert!(projector.finish().is_empty());
}
