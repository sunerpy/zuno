use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::{PromptAccounting, StreamEvent};
use zuno_tui::views::live_session::{LiveSessionOpen, LiveSessions};
use zuno_tui::views::message::Transcript;
use zuno_types::UsageSnapshot;
use zuno_types::context_usage::{
    ContextRequestIdentity, ContextTokenAccounting, ContextUsageCounters, ContextUsageFreshness,
    ContextUsageSource, ContextUsageTracker,
};

#[test]
fn a_lower_request_estimate_cannot_replace_the_restored_provider_baseline() {
    let mut transcript = Transcript::new();
    transcript.restore_usage(UsageSnapshot {
        last_prompt_tokens: Some(125_350),
        context_limit: Some(200_000),
        confirmed_known: true,
        ..UsageSnapshot::default()
    });
    transcript.observe(&TurnEvent::ProviderRequestStarted {
        step: 2,
        message_count: 3,
        estimated_prompt_tokens: 73_948,
    });
    let context = transcript.context_window().expect("restored context");
    assert!(
        context.prompt_tokens >= 125_350,
        "request estimate {} cannot erase the 125350-token provider baseline",
        context.prompt_tokens
    );
    assert!(context.estimated);
}

fn request(sequence: u64, source: ContextUsageSource) -> ContextRequestIdentity {
    ContextRequestIdentity {
        request_id: format!("synthetic-request-{sequence}"),
        request_sequence: sequence,
        attempt: 1,
        context_epoch: 0,
        provider_id: "synthetic-provider".to_owned(),
        model_id: "synthetic-model".to_owned(),
        source,
        turn_id: Some("synthetic-turn".to_owned()),
        time_started: i64::try_from(sequence).unwrap(),
        request_context_tokens: None,
        history_prefix: None,
    }
}

fn confirmed(session_id: &str, source: ContextUsageSource) -> ContextUsageTracker {
    let mut tracker = ContextUsageTracker::for_source(session_id, source);
    let request = request(1, source);
    tracker.start_request(request.clone(), Some(70_000), Some(0), Some(200_000), 1);
    tracker.observe_usage(
        &request,
        ContextUsageCounters {
            input_tokens: Some(125_350),
            output_tokens: Some(40),
            reasoning_tokens: Some(10),
            accounting: ContextTokenAccounting::CacheInsideInput,
            ..ContextUsageCounters::default()
        },
        2,
    );
    tracker.commit_request(&request, 3);
    tracker
}

#[test]
fn canonical_snapshot_drives_context_and_disjoint_totals_without_raw_overwrite() {
    let tracker = confirmed("ses_main", ContextUsageSource::Main);
    let mut transcript = Transcript::new();
    assert!(transcript.set_context_usage(tracker.snapshot().clone()));
    let context = transcript.context_window().unwrap();
    assert_eq!(context.prompt_tokens, 125_390);
    assert!(!context.estimated);
    assert_eq!(transcript.tokens().output, 30);
    assert_eq!(transcript.tokens().reasoning, 10);
    assert_eq!(transcript.tokens().total(), 125_390);
    assert!(!transcript.observe(&TurnEvent::ProviderRequestStarted {
        step: 2,
        message_count: 3,
        estimated_prompt_tokens: 73_948,
    }));
    assert!(!transcript.observe(&TurnEvent::Provider {
        step: 2,
        event: StreamEvent::TokenUsage {
            input_tokens: Some(5),
            output_tokens: Some(1),
            reasoning_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
    }));
    transcript.restore_usage(UsageSnapshot {
        last_prompt_tokens: Some(1),
        context_limit: Some(100),
        failed_turns: 2,
        ..UsageSnapshot::default()
    });
    assert_eq!(transcript.context_window().unwrap(), context);
    assert_eq!(transcript.tokens().total(), 125_390);
    assert_eq!(transcript.failed_turns(), 2);
    assert_eq!(transcript.context_usage(), Some(tracker.snapshot()));
}

#[test]
fn estimates_unknown_and_compaction_revisions_are_distinct() {
    let mut tracker = confirmed("ses_main", ContextUsageSource::Main);
    let mut transcript = Transcript::new();
    transcript.set_context_usage(tracker.snapshot().clone());
    tracker.start_request(
        request(2, ContextUsageSource::Main),
        Some(73_948),
        Some(200_000),
        Some(200_000),
        4,
    );
    assert!(transcript.set_context_usage(tracker.snapshot().clone()));
    assert_eq!(transcript.context_window().unwrap().prompt_tokens, 325_390);
    assert!(transcript.context_window().unwrap().estimated);
    tracker.reset_epoch(10, 5);
    assert!(transcript.set_context_usage(tracker.snapshot().clone()));
    assert_eq!(transcript.context_window(), None);
    assert_eq!(
        transcript.context_usage().unwrap().freshness,
        ContextUsageFreshness::Unknown
    );
    assert_eq!(transcript.tokens().total(), 125_390);
    let mut compacted = request(3, ContextUsageSource::Main);
    compacted.context_epoch = 10;
    tracker.start_request(compacted, Some(1_000), Some(0), Some(200_000), 6);
    assert!(transcript.set_context_usage(tracker.snapshot().clone()));
    assert_eq!(transcript.context_window().unwrap().prompt_tokens, 1_000);
}

#[test]
fn source_session_revision_and_epoch_guard_the_visible_context() {
    let mut tracker = confirmed("ses_main", ContextUsageSource::Main);
    let mut transcript = Transcript::new();
    let first = tracker.snapshot().clone();
    assert!(transcript.set_context_usage(first.clone()));
    assert!(!transcript.set_context_usage(first.clone()));
    for source in [ContextUsageSource::Child, ContextUsageSource::Learning] {
        assert!(!transcript.set_context_usage(confirmed("ses_main", source).snapshot().clone()));
    }
    assert!(
        !transcript.set_context_usage(
            confirmed("ses_other", ContextUsageSource::Main)
                .snapshot()
                .clone()
        )
    );
    tracker.reset_epoch(10, 4);
    assert!(transcript.set_context_usage(tracker.snapshot().clone()));
    let mut stale = first;
    stale.revision = tracker.snapshot().revision + 10;
    assert!(!transcript.set_context_usage(stale));
    assert_eq!(transcript.context_usage(), Some(tracker.snapshot()));
}

#[test]
fn child_context_updates_only_its_own_live_projection() {
    let sessions = LiveSessions::default();
    for id in ["ses_child_one", "ses_child_two"] {
        sessions.restore(LiveSessionOpen {
            session_id: id.to_owned(),
            parent_session_id: "ses_main".to_owned(),
            title: id.to_owned(),
            agent: "build".to_owned(),
            model: "synthetic-provider/synthetic-model".to_owned(),
            effort: None,
            messages: Vec::new(),
            usage: None,
        });
    }
    let other = sessions.snapshot("ses_child_two").unwrap();
    let tracker = confirmed("ses_child_one", ContextUsageSource::Child);
    assert!(sessions.set_context_usage(tracker.snapshot().clone()));
    assert!(!sessions.set_context_usage(tracker.snapshot().clone()));
    assert_eq!(
        sessions
            .snapshot("ses_child_one")
            .unwrap()
            .transcript
            .context_window()
            .unwrap()
            .prompt_tokens,
        125_390
    );
    let after_other = sessions.snapshot("ses_child_two").unwrap();
    assert_eq!(after_other.generation, other.generation);
    assert!(after_other.transcript.context_usage().is_none());
    assert!(
        !sessions.set_context_usage(
            confirmed("ses_child_one", ContextUsageSource::Main)
                .snapshot()
                .clone()
        )
    );
}

#[test]
fn legacy_output_only_frame_does_not_fabricate_a_zero_prompt() {
    let mut transcript = Transcript::new();
    transcript.set_context_limit(200_000);
    transcript.observe(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::TokenUsage {
            input_tokens: None,
            output_tokens: Some(9),
            reasoning_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
    });
    assert_eq!(transcript.context_window(), None);
    assert_eq!(transcript.last_prompt_tokens(), None);
}
