//! Transcript fold and rendering tests.

use super::*;
use crate::app::render_offscreen;
use crate::views::testkit::rows;
use zuno_llm::event::FinishReason;

fn draw(view: &mut TranscriptView, width: u16, height: u16) -> Vec<String> {
    let buffer =
        render_offscreen(view, width, height).expect("the offscreen backend is infallible");
    rows(&buffer)
}

fn provider(event: StreamEvent) -> TurnEvent {
    TurnEvent::Provider { step: 1, event }
}

fn started() -> TurnEvent {
    TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: String::from("msg_1"),
    }
}

fn view() -> TranscriptView {
    TranscriptView::new(ViewContext::defaults())
}

// ---------------------------------------------------------------------------
// The off-screen assertion for the chat view
// ---------------------------------------------------------------------------

#[test]
fn views_chat_transcript_renders_every_part_kind_offscreen() {
    let mut view = view();
    view.transcript_mut()
        .push(Message::user("summarise the plan"));
    for event in [
        started(),
        provider(StreamEvent::ReasoningStart),
        provider(StreamEvent::ReasoningDelta(String::from(
            "## Approach\nread the file first",
        ))),
        provider(StreamEvent::ReasoningDone { duration_secs: 2.5 }),
        provider(StreamEvent::TextDelta(String::from("Here is the summary."))),
        provider(StreamEvent::ToolUseStart {
            id: String::from("call_1"),
            name: String::from("read"),
        }),
        TurnEvent::ToolDispatchStarted {
            step: 1,
            call_id: String::from("call_1"),
            name: String::from("read"),
        },
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("call_1"),
            name: String::from("read"),
            title: String::from("Read src/main.rs"),
            output: String::from("fn main() {}"),
            is_error: false,
        },
        provider(StreamEvent::GeneratedImage {
            id: String::from("img_1"),
            path: String::from("chart.png"),
            metadata_path: None,
            output_format: String::from("png"),
            revised_prompt: None,
        }),
        TurnEvent::TurnCompleted {
            assistant_message_id: String::from("msg_1"),
            steps: 1,
        },
    ] {
        view.handle_event(&AppEvent::Engine(event));
    }

    let rows = draw(&mut view, 48, 16);
    let joined = rows.join("\n");
    assert!(
        joined.contains("▌ You"),
        "the user's turn is missing its header:\n{joined}"
    );
    assert!(
        joined.contains("summarise the plan"),
        "the user's text is missing:\n{joined}"
    );
    assert!(
        joined.contains("│ Assistant"),
        "the assistant's turn is missing its header:\n{joined}"
    );
    assert!(
        joined.contains("Thinking (2.5s)"),
        "the reasoning affordance did not render its duration:\n{joined}"
    );
    assert!(
        joined.contains("Approach"),
        "the collapsed reasoning summary is missing:\n{joined}"
    );
    assert!(
        joined.contains("Here is the summary."),
        "the assistant's text is missing:\n{joined}"
    );
    assert!(
        joined.contains("✓ → Read src/main.rs"),
        "the completed tool call did not render its status, icon, and title:\n{joined}"
    );
    assert!(
        joined.contains("fn main() {}"),
        "the tool output is missing:\n{joined}"
    );
    assert!(
        joined.contains("⎘ chart.png"),
        "the generated-image attachment is missing:\n{joined}"
    );
}

#[test]
fn views_chat_transcript_paints_from_the_palette_not_from_a_literal() {
    let context = ViewContext::defaults();
    let mut view = TranscriptView::new(context.clone());
    view.transcript_mut().push(Message::user("hello"));
    let buffer = render_offscreen(&mut view, 20, 4).expect("infallible");
    // Column zero is the role's left rule and column two is the body, so the two are
    // sampled separately: asserting one cell could not tell a themed transcript from
    // one whose rule and body had collapsed into a single colour.
    let rule = &buffer[(0, 0)];
    let body = &buffer[(2, 0)];
    assert_eq!(
        body.bg,
        ratatui::style::Color::from(context.palette.background_panel),
        "the transcript background did not come from the resolved palette"
    );
    assert_eq!(
        body.fg,
        ratatui::style::Color::from(context.palette.text),
        "the transcript foreground did not come from the resolved palette"
    );
    assert_eq!(
        rule.fg,
        ratatui::style::Color::from(context.palette.border_active),
        "the user's left rule did not come from the resolved palette"
    );
    assert_ne!(
        rule.fg, body.fg,
        "the rule and the body are the same colour, so the transcript has no structure"
    );
}

// ---------------------------------------------------------------------------
// Incremental streaming
// ---------------------------------------------------------------------------

#[test]
fn views_streaming_message_renders_incrementally() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));

    let mut snapshots = Vec::new();
    for delta in ["The", " quick", " brown", " fox"] {
        let result = view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
            String::from(delta),
        ))));
        assert!(
            result.redraw,
            "a text delta must request a frame, otherwise the stream is invisible"
        );
        snapshots.push(draw(&mut view, 30, 6).join("\n"));
    }

    assert!(
        snapshots[0].contains("The") && !snapshots[0].contains("quick"),
        "the first frame already shows later deltas:\n{}",
        snapshots[0]
    );
    assert!(
        snapshots[1].contains("The quick") && !snapshots[1].contains("brown"),
        "the second frame is not the first plus one delta:\n{}",
        snapshots[1]
    );
    assert!(
        snapshots[3].contains("The quick brown fox"),
        "the final frame lost an earlier delta:\n{}",
        snapshots[3]
    );
    // Each frame must be a strict prefix growth of the previous one: that is what
    // "incremental" means, as opposed to "re-rendered from scratch each time".
    for pair in snapshots.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let before_text = before.replace([' ', '\n'], "");
        let after_text = after.replace([' ', '\n'], "");
        assert!(
            after_text.starts_with(&before_text),
            "frame {after_text:?} is not an extension of {before_text:?}"
        );
    }
}

#[test]
fn views_streaming_reasoning_and_text_accumulate_into_separate_parts() {
    let mut transcript = Transcript::new();
    transcript.observe(&started());
    transcript.observe(&provider(StreamEvent::ReasoningStart));
    transcript.observe(&provider(StreamEvent::ReasoningDelta(String::from(
        "plan ",
    ))));
    transcript.observe(&provider(StreamEvent::ReasoningDelta(String::from("more"))));
    transcript.observe(&provider(StreamEvent::ReasoningEnd));
    transcript.observe(&provider(StreamEvent::TextDelta(String::from("answer"))));

    let parts = &transcript.messages()[0].parts;
    assert_eq!(parts.len(), 2, "reasoning and text merged into one part");
    assert_eq!(
        parts[0],
        MessagePart::Reasoning {
            text: String::from("plan more"),
            duration_secs: None,
            streaming: false,
        }
    );
    assert_eq!(parts[1].text(), Some("answer"));
}

#[test]
fn views_retry_rollback_discards_the_interrupted_attempt() {
    let mut transcript = Transcript::new();
    transcript.observe(&started());
    transcript.observe(&provider(StreamEvent::TextDelta(String::from("half an "))));
    transcript.observe(&provider(StreamEvent::RetryRollback { attempt: 1, max: 3 }));
    transcript.observe(&provider(StreamEvent::TextDelta(String::from("whole"))));

    let parts = &transcript.messages()[0].parts;
    let text_parts = parts
        .iter()
        .filter_map(MessagePart::text)
        .collect::<Vec<_>>();
    assert_eq!(
        text_parts,
        ["whole"],
        "the discarded attempt was kept alongside the replay: {parts:?}"
    );
}

#[test]
fn views_retry_rollback_notice_is_visible_in_the_error_colour() {
    let context = ViewContext::defaults();
    let mut view = TranscriptView::new(context.clone());
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
        String::from("discard me"),
    ))));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::RetryRollback {
        attempt: 2,
        max: 3,
    })));

    let buffer = render_offscreen(&mut view, 56, 5).expect("infallible");
    let rendered_rows = rows(&buffer);
    let retry_row = rendered_rows
        .iter()
        .position(|row| row.contains("Retrying provider request (attempt 2/3)"))
        .expect("retry notice is rendered");
    assert!(
        !rendered_rows.join("\n").contains("discard me"),
        "rollback kept the failed attempt: {rendered_rows:?}"
    );
    // Column two, not zero: column zero carries the role's left rule, so sampling it
    // would read the rule's colour and never see the notice's.
    assert_eq!(
        buffer[(2, u16::try_from(retry_row).expect("test row fits u16"))].fg,
        ratatui::style::Color::from(context.palette.error),
        "retry notice did not use the theme's red/error colour"
    );
}

// ---------------------------------------------------------------------------
// Tool call status
// ---------------------------------------------------------------------------

#[test]
fn views_tool_call_walks_pending_running_and_terminal_states() {
    let mut transcript = Transcript::new();
    transcript.observe(&started());
    transcript.observe(&provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("bash"),
    }));
    let status = |transcript: &Transcript| match &transcript.messages()[0].parts[0] {
        MessagePart::Tool { status, .. } => *status,
        other => panic!("expected a tool part, found {other:?}"),
    };
    assert_eq!(status(&transcript), ToolStatus::Pending);

    transcript.observe(&TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: String::from("c1"),
        name: String::from("bash"),
    });
    assert_eq!(status(&transcript), ToolStatus::Running);
    assert!(status(&transcript).is_active());

    transcript.observe(&TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        name: String::from("bash"),
        title: String::from("ls"),
        output: String::from("a\nb"),
        is_error: true,
    });
    assert_eq!(status(&transcript), ToolStatus::Error);
    assert!(!status(&transcript).is_active());
}

#[test]
fn views_tool_dispatch_without_a_provider_stream_still_appears() {
    // A reconnect can deliver the dispatch without the `ToolUseStart` that
    // normally precedes it. Dropping the call would hide work from the user.
    let mut transcript = Transcript::new();
    transcript.observe(&started());
    transcript.observe(&TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: String::from("orphan"),
        name: String::from("grep"),
    });
    assert_eq!(transcript.messages()[0].parts.len(), 1);
}

#[test]
fn views_tool_affordance_matches_the_oracle_icons() {
    for (name, icon) in [
        ("bash", "$"),
        ("glob", "✱"),
        ("grep", "✱"),
        ("read", "→"),
        ("write", "→"),
        ("webfetch", "%"),
        ("websearch", "◈"),
        ("task", "#"),
        ("something_else", "⚙"),
    ] {
        assert_eq!(tool_affordance(name).0, icon, "wrong icon for {name}");
    }
}

#[test]
fn views_pending_tool_renders_its_placeholder_until_arguments_arrive() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c"),
        name: String::from("bash"),
    })));
    let joined = draw(&mut view, 40, 6).join("\n");
    assert!(
        joined.contains("~ $ Writing command..."),
        "a pending bash call did not render the oracle's placeholder:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// The reasoning affordance
// ---------------------------------------------------------------------------

#[test]
fn views_thinking_affordance_toggles_between_summary_and_full_text() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDelta(
        String::from("first line\nsecond line"),
    ))));

    assert_eq!(view.thinking(), ThinkingDisplay::Collapsed);
    let collapsed = draw(&mut view, 40, 8).join("\n");
    assert!(collapsed.contains("first line"), "{collapsed}");
    assert!(
        !collapsed.contains("second line"),
        "collapsed reasoning showed its whole body:\n{collapsed}"
    );

    view.toggle_thinking();
    assert_eq!(view.thinking(), ThinkingDisplay::Expanded);
    let expanded = draw(&mut view, 40, 8).join("\n");
    assert!(
        expanded.contains("second line"),
        "expanded reasoning is still summarised:\n{expanded}"
    );
}

#[test]
fn views_reasoning_summary_drops_a_markdown_heading_and_truncates() {
    assert_eq!(summary("### Plan\nbody"), Some(String::from("Plan")));
    assert_eq!(summary("\n\n   \nreal"), Some(String::from("real")));
    assert_eq!(summary("   "), None);
    let long = "x".repeat(200);
    let summarised = summary(&long).expect("non-empty");
    assert_eq!(summarised.chars().count(), 72);
    assert!(summarised.ends_with('…'));
}

#[test]
fn views_thinking_style_is_the_theme_opacity_composite_not_the_raw_warning() {
    let context = ViewContext::defaults();
    let thinking = context.thinking().fg.expect("a foreground");
    let warning = ratatui::style::Color::from(context.palette.warning);
    assert_ne!(
        thinking, warning,
        "thinkingOpacity was ignored, so reasoning is indistinguishable from a warning"
    );
}

// ---------------------------------------------------------------------------
// Scrolling and the scrollbar
// ---------------------------------------------------------------------------

#[test]
fn views_transcript_offset_clamps_to_the_content_it_rendered() {
    let mut view = view();
    for index in 0..20 {
        view.transcript_mut()
            .push(Message::user(format!("line {index}")));
    }
    draw(&mut view, 30, 5);
    view.set_offset(10_000);
    assert_eq!(
        view.offset(),
        view.content_height() - view.viewport_height(),
        "the offset was allowed past the end of the content"
    );

    view.set_offset(3);
    let rows = draw(&mut view, 30, 5);
    assert!(
        !rows.join("\n").contains("line 0"),
        "a scrolled transcript still shows its first row: {rows:?}"
    );
}

#[test]
fn views_scrollbar_thumb_moves_with_the_offset() {
    let mut bar = ScrollbarView::new(ViewContext::defaults());
    bar.total = 100;
    bar.viewport = 10;
    let position = |bar: &mut ScrollbarView| {
        let buffer = render_offscreen(bar, 1, 10).expect("infallible");
        (0..10)
            .position(|y| buffer[(0, y)].symbol() == ratatui::symbols::block::FULL)
            .expect("a thumb")
    };
    bar.offset = 0;
    let top = position(&mut bar);
    bar.offset = 90;
    let bottom = position(&mut bar);
    assert_eq!(top, 0, "the thumb is not at the top when unscrolled");
    assert!(
        bottom > top,
        "the thumb did not move for a scrolled viewport ({top} -> {bottom})"
    );
}

#[test]
fn views_scrollbar_hides_its_thumb_when_everything_fits() {
    let mut bar = ScrollbarView::new(ViewContext::defaults());
    bar.total = 4;
    bar.viewport = 10;
    let buffer = render_offscreen(&mut bar, 1, 10).expect("infallible");
    assert!(
        (0..10).all(|y| buffer[(0, y)].symbol() != ratatui::symbols::block::FULL),
        "a scrollbar drew a thumb for content that fits"
    );
}

/// A warning has to survive in the transcript, not flash past on the status strip.
///
/// The registry's shadowing diagnostic used to be an `eprintln!`, which under the
/// alternate screen meant the user inherited it on exit with no context. It now
/// travels as a status detail — and the strip holds exactly one, so a warning left
/// only there is overwritten by the next detail and effectively lost. This asserts it
/// lands in the transcript, is attributed to neither party, and is not overwritten.
#[test]
fn views_transcript_keeps_a_warning_detail_that_the_status_strip_would_overwrite() {
    let mut transcript = TranscriptView::new(ViewContext::defaults());
    let warning = "warning: tool `grep` from built-in suppressed by same-named tool from plugin";
    for detail in [warning, "session titled: something else"] {
        transcript.handle_event(&AppEvent::Engine(TurnEvent::Provider {
            step: 0,
            event: StreamEvent::StatusDetail {
                detail: detail.to_owned(),
            },
        }));
    }

    let joined = rows(&render_offscreen(&mut transcript, 90, 8).expect("infallible")).join("\n");
    assert!(
        joined.contains("suppressed by same-named tool from plugin"),
        "the shadowing warning is not visible in the transcript:\n{joined}"
    );
    assert!(
        joined.contains("▲ Session"),
        "the warning must be attributed to the session, not to the user or the model:\n{joined}"
    );
    assert!(
        !joined.contains("session titled"),
        "an ordinary status detail must stay on the strip, not fill the transcript:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// The status strip
// ---------------------------------------------------------------------------

#[test]
fn views_status_strip_renders_agent_model_and_step_offscreen() {
    let mut status = StatusView::new(ViewContext::defaults());
    for event in [
        TurnEvent::AgentResolved {
            step: 1,
            agent: String::from("build"),
        },
        TurnEvent::ModelResolved {
            step: 1,
            provider_id: String::from("anthropic"),
            model_id: String::from("claude"),
        },
        TurnEvent::StepCompleted {
            step: 2,
            finish_reason: Some(FinishReason::Stop),
        },
    ] {
        assert!(
            status.handle_event(&AppEvent::Engine(event)).redraw,
            "a status-changing event did not request a frame"
        );
    }
    let buffer = render_offscreen(&mut status, 48, 1).expect("infallible");
    let row = rows(&buffer).remove(0);
    assert_eq!(row, " build · anthropic/claude · step 2");
}

#[test]
fn views_status_strip_is_idle_before_anything_resolves() {
    let mut status = StatusView::new(ViewContext::defaults());
    let buffer = render_offscreen(&mut status, 20, 1).expect("infallible");
    assert_eq!(rows(&buffer).remove(0), " idle");
}

/// The way out of a raw-mode terminal has to be readable on screen.
///
/// Wiring the exit chord is only half the fix: a binding nothing displays is one a
/// user has to already know. This asserts the strip carries it, and that a terminal
/// too narrow to hold it drops the hint rather than truncating either half — a
/// half-printed key name would be worse than none.
#[test]
fn views_status_strip_shows_the_exit_binding_and_drops_it_when_too_narrow() {
    let mut status = StatusView::new(ViewContext::defaults());
    let row = rows(&render_offscreen(&mut status, 48, 1).expect("infallible")).remove(0);
    assert!(
        row.ends_with(StatusView::EXIT_HINT),
        "the strip must show how to leave: {row:?}"
    );
    assert!(
        row.starts_with(&format!(" {}", StatusView::IDLE)),
        "the hint must not displace the turn state: {row:?}"
    );

    let narrow = rows(&render_offscreen(&mut status, 20, 1).expect("infallible")).remove(0);
    assert_eq!(
        narrow, " idle",
        "a row too narrow for the hint must drop it whole, not truncate it"
    );
}

/// The strip must never read `idle` while a turn is under way.
///
/// Three moments, because the strip can lie at any of them and only the middle one
/// is obvious: before the engine's first event (the prompt is being persisted), while
/// the turn resolves, and after it ends — where carrying the last turn's agent and
/// model forward would leave the row describing a turn that is over.
#[test]
fn views_status_strip_never_reads_idle_while_a_turn_is_under_way() {
    let mut status = StatusView::new(ViewContext::defaults());
    // Reads the turn-state half of a row the exit hint shares. Asserting on the whole
    // row would make any wording of the hint read as the strip lying about the turn.
    let rendered = |status: &mut StatusView| {
        let row = rows(&render_offscreen(status, 48, 1).expect("infallible")).remove(0);
        row.strip_suffix(StatusView::EXIT_HINT)
            .unwrap_or(&row)
            .trim_end()
            .to_owned()
    };

    status.mark_running();
    assert!(status.is_running());
    assert_eq!(
        rendered(&mut status).trim(),
        StatusView::WORKING,
        "the window between a submitted prompt and the engine's first event read idle"
    );

    for event in [
        TurnEvent::TurnStarted {
            session_id: String::from("ses_status"),
        },
        TurnEvent::AgentResolved {
            step: 1,
            agent: String::from("build"),
        },
    ] {
        assert!(status.handle_event(&AppEvent::Engine(event)).redraw);
    }
    assert!(status.is_running());
    assert_eq!(rendered(&mut status), " build");

    assert!(
        status
            .handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
                assistant_message_id: String::from("msg_status"),
                steps: 1,
            }))
            .redraw
    );
    assert!(!status.is_running());
    // The intent here is unchanged: the strip must never describe a turn that is over.
    // What changed is that the agent a *later* turn will run as now survives the reset,
    // because the alternative — a strip whose only pre-turn state is the bare word
    // `idle` — answers neither of the questions a user has before pressing enter. So the
    // assertion is now that `idle` is stated explicitly and that the finished turn's
    // *step* is gone, which is the part that would genuinely have been a lie.
    let after = rendered(&mut status);
    assert!(
        after.contains(StatusView::IDLE),
        "the strip does not say that nothing is running: [{after}]"
    );
    assert!(
        !after.contains("step"),
        "the finished turn's step stayed on the strip, so it still describes a turn \
         that is over: [{after}]"
    );
    assert!(
        after.contains("build"),
        "the agent the next turn will run as is not shown: [{after}]"
    );
}

// ---------------------------------------------------------------------------
// Wrapping
// ---------------------------------------------------------------------------

#[test]
fn views_wrap_breaks_on_words_and_splits_unbreakable_runs() {
    assert_eq!(wrap("a b c", 3), vec!["a b", "c"]);
    assert_eq!(wrap("", 10), vec![""]);
    assert_eq!(wrap("one\ntwo", 10), vec!["one", "two"]);
    assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    assert_eq!(
        wrap("x", 0),
        vec!["x"],
        "a zero width must not divide by zero"
    );
}

#[test]
fn views_transcript_ignores_events_that_change_nothing_visible() {
    let mut view = view();
    let ignored = view.handle_event(&AppEvent::Engine(TurnEvent::HistoryRepaired {
        repaired_tool_results: 3,
    }));
    assert!(
        !ignored.redraw,
        "an invisible engine event forced a frame, which makes the TUI redraw on bookkeeping"
    );
}

#[test]
fn views_transcript_tracks_the_running_flag() {
    let mut transcript = Transcript::new();
    assert!(!transcript.is_running());
    transcript.observe(&TurnEvent::TurnStarted {
        session_id: String::from("s"),
    });
    assert!(transcript.is_running());
    transcript.observe(&TurnEvent::TurnInterrupted {
        assistant_message_id: None,
        steps: 1,
    });
    assert!(!transcript.is_running());
}

#[test]
fn views_attachment_renders_its_mime_when_known() {
    let mut view = view();
    let mut message = Message::user("look at this");
    message.attach("diagram.svg", Some(String::from("image/svg+xml")));
    view.transcript_mut().push(message);
    let joined = draw(&mut view, 44, 6).join("\n");
    assert!(
        joined.contains("⎘ diagram.svg (image/svg+xml)"),
        "the attachment did not render its name and type:\n{joined}"
    );
}

#[test]
fn views_transcript_groups_consecutive_assistant_steps_under_one_header() {
    // Measured on a real terminal: a five-step turn printed `Assistant` five times for
    // what the user experienced as a single reply.
    let mut view = view();
    view.transcript_mut().push(Message::user("go"));
    for step in 1..=3 {
        view.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
            step,
            message_id: format!("m{step}"),
        }));
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
            format!("step {step}"),
        ))));
    }
    let rendered = draw(&mut view, 60, 30);
    let headers = rendered
        .iter()
        .filter(|row| row.contains("Assistant"))
        .count();
    assert_eq!(
        headers,
        1,
        "three assistant steps produced {headers} headers:\n{}",
        rendered.join("\n")
    );
    let joined = rendered.join("\n");
    for step in 1..=3 {
        assert!(
            joined.contains(&format!("step {step}")),
            "grouping dropped step {step}'s text:\n{joined}"
        );
    }
    assert!(
        joined.contains("▌ You"),
        "grouping also swallowed the user's header:\n{joined}"
    );
}

#[test]
fn views_transcript_folds_provider_token_usage_for_the_ambient_panel() {
    // The sidebar and the strip read this one accumulator; a fold that never ran is why
    // a completed turn can still report `no usage reported yet`.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
        input_tokens: Some(1_200),
        output_tokens: Some(340),
        cache_read_input_tokens: Some(80),
        cache_write_input_tokens: None,
    })));
    let tokens = view.transcript().tokens();
    assert_eq!(tokens.input, 1_200);
    assert_eq!(tokens.output, 340);
    assert_eq!(tokens.cache_read, 80);
    assert_eq!(tokens.total(), 1_620);
    assert!(!tokens.is_empty());

    // Two reports accumulate rather than replace, because a turn bills per step.
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(10),
        cache_read_input_tokens: None,
        cache_write_input_tokens: None,
    })));
    assert_eq!(view.transcript().tokens().input, 1_300);
}

#[test]
fn views_transcript_context_percentage_needs_a_declared_window() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
        input_tokens: Some(5_000),
        output_tokens: Some(1_000),
        cache_read_input_tokens: None,
        cache_write_input_tokens: None,
    })));
    assert_eq!(
        view.transcript().context_used(),
        None,
        "a model that declares no window must not produce a percentage"
    );
    view.transcript_mut().set_context_limit(20_000);
    assert_eq!(view.transcript().context_used(), Some(25));
    // Output is excluded: the window bounds the prompt, and including completions would
    // climb past 100 on a long session.
    assert!(view.transcript().context_used().unwrap() <= 100);
}

#[test]
fn views_transcript_renders_a_tool_patch_as_a_diff() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: String::from("m"),
    }));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("edit"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        name: String::from("edit"),
        title: String::from("Edit src/main.rs"),
        output: String::from("@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new();\n }\n"),
        is_error: false,
    }));
    view.toggle_tool_output();
    let joined = draw(&mut view, 90, 24).join("\n");
    assert!(
        joined.contains("@@ -1,3 +1,3 @@"),
        "the hunk header is missing:\n{joined}"
    );
    assert!(
        joined.contains('+') && joined.contains('-'),
        "the patch lost its signs:\n{joined}"
    );
    assert!(
        joined.contains('2') && joined.contains('3'),
        "the diff was rendered without line numbers:\n{joined}"
    );
}

#[test]
fn views_transcript_collapses_long_tool_output_and_says_how_much_it_hid() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: String::from("m"),
    }));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("bash"),
    })));
    let body = (1..=12)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        name: String::from("bash"),
        title: String::from("ls"),
        output: body,
        is_error: false,
    }));
    let collapsed = draw(&mut view, 60, 30).join("\n");
    assert!(
        collapsed.contains("9 more lines"),
        "the collapse notice does not state how much it hid:\n{collapsed}"
    );
    assert!(collapsed.contains("line 1"), "{collapsed}");
    assert!(
        !collapsed.contains("line 12"),
        "collapsed output rendered its whole body:\n{collapsed}"
    );

    view.toggle_tool_output();
    let expanded = draw(&mut view, 60, 30).join("\n");
    assert!(
        expanded.contains("line 12"),
        "expanding did not reveal the rest:\n{expanded}"
    );
    assert!(
        !expanded.contains("more lines"),
        "an expanded block still claims to be hiding rows:\n{expanded}"
    );
}

#[test]
fn views_status_strip_keeps_a_resolved_model_after_the_turn_that_resolved_it_ends() {
    // Measured live: choosing a model in the picker printed
    // `session: model is now myopenai/…` on the strip while the strip's own model field
    // still read `amazon-bedrock/amazon.nova-2-lite-v1:0`. A strip that contradicts the
    // line beside it is worse than one that says nothing.
    let mut view = StatusView::new(ViewContext::defaults());
    view.handle_event(&AppEvent::Engine(TurnEvent::ModelResolved {
        step: 0,
        provider_id: String::from("myopenai"),
        model_id: String::from("global.anthropic.claude-haiku-4-5-20251001-v1:0"),
    }));
    view.handle_event(&AppEvent::Engine(TurnEvent::AgentResolved {
        step: 0,
        agent: String::from("explore"),
    }));
    view.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));
    let line = view.line(160);
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(
        text.contains("myopenai/global.anthropic.claude-haiku-4-5-20251001-v1:0"),
        "the resolved model did not survive the turn's end: [{text}]"
    );
    assert!(
        text.contains("explore"),
        "the resolved agent did not survive the turn's end: [{text}]"
    );
    assert!(
        text.contains(StatusView::IDLE),
        "the strip does not report that nothing is running: [{text}]"
    );
}

#[test]
fn views_status_strip_reports_cumulative_token_usage_and_never_loses_the_exit_hint() {
    let mut view = StatusView::new(ViewContext::defaults());
    for _ in 0..3 {
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(250),
            cache_read_input_tokens: Some(10),
            cache_write_input_tokens: Some(5),
        })));
    }
    assert_eq!(
        view.usage(),
        crate::views::message::TokenUsage {
            input: 3_000,
            output: 750,
            cache_read: 30,
            cache_write: 15,
        },
        "usage is per-step rather than cumulative"
    );
    let text = view
        .line(160)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("↑3,000"), "[{text}]");
    assert!(text.contains("↓750"), "[{text}]");
    assert!(text.contains("⚡45"), "[{text}]");
    assert!(text.contains(StatusView::EXIT_HINT), "[{text}]");

    // Under width pressure the counts go before the exit key, never the other way round.
    let narrow = view
        .line(40)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(
        narrow.contains(StatusView::EXIT_HINT),
        "the exit hint was dropped before the token counts: [{narrow}]"
    );
    assert!(!narrow.contains("↑3,000"), "[{narrow}]");
}

#[test]
fn views_status_strip_omits_a_cache_column_a_provider_never_reported() {
    let mut view = StatusView::new(ViewContext::defaults());
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
        input_tokens: Some(12),
        output_tokens: Some(3),
        cache_read_input_tokens: None,
        cache_write_input_tokens: None,
    })));
    let text = view
        .line(160)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("↑12 ↓3"), "[{text}]");
    assert!(
        !text.contains('⚡'),
        "a permanent `cache 0` is a column of noise: [{text}]"
    );
}
