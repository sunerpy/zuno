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
        joined.contains("> You"),
        "the user's turn is missing its header:\n{joined}"
    );
    assert!(
        joined.contains("summarise the plan"),
        "the user's text is missing:\n{joined}"
    );
    assert!(
        joined.contains("* Assistant"),
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
    let cell = &buffer[(0, 0)];
    assert_eq!(
        cell.bg,
        ratatui::style::Color::from(context.palette.background_panel),
        "the transcript background did not come from the resolved palette"
    );
    assert_eq!(
        cell.fg,
        ratatui::style::Color::from(context.palette.text),
        "the transcript foreground did not come from the resolved palette"
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
    assert_eq!(
        buffer[(0, u16::try_from(retry_row).expect("test row fits u16"))].fg,
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

/// The strip must never read `idle` while a turn is under way.
///
/// Three moments, because the strip can lie at any of them and only the middle one
/// is obvious: before the engine's first event (the prompt is being persisted), while
/// the turn resolves, and after it ends — where carrying the last turn's agent and
/// model forward would leave the row describing a turn that is over.
#[test]
fn views_status_strip_never_reads_idle_while_a_turn_is_under_way() {
    let mut status = StatusView::new(ViewContext::defaults());
    let rendered = |status: &mut StatusView| {
        rows(&render_offscreen(status, 48, 1).expect("infallible")).remove(0)
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
    assert_eq!(
        rendered(&mut status).trim(),
        StatusView::IDLE,
        "the finished turn's agent stayed on the strip, so it still describes a turn \
         that is over"
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
