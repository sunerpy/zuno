//! Transcript fold and rendering tests.

use super::*;
use crate::app::render_offscreen;
use crate::views::testkit::rows;
use zuno_llm::event::PromptAccounting;

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

/// The terminal columns one produced [`Line`] occupies.
///
/// Measured on the `Line` rather than on a rendered buffer row, and that distinction is the
/// whole point. `testkit::rows` yields one character per *cell*, so a wide glyph arrives as
/// the glyph plus the blank continuation cell the terminal reserved — a string whose
/// `display_width` is three for two columns. Worse, ratatui has already clipped anything
/// that overran, so an assertion made after rendering can never see the overrun it is
/// looking for. The `Line` is the last place the transcript's own arithmetic is still
/// visible.
fn line_columns(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| crate::views::display_width(&span.content))
        .sum()
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
            display_name: String::from("read"),
            name: String::from("read"),
            ui_intent: zuno_tool::ToolUiIntent::Generic,
        },
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("call_1"),
            display_name: String::from("read"),
            name: String::from("read"),
            title: String::from("Read src/main.rs"),
            output: String::from("fn main() {}"),
            diff: None,
            written_paths: Vec::new(),
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
        joined.contains("◇ Thought · 2.5s"),
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
        joined.contains("✓ Tool · → Read src/main.rs"),
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
        ratatui::style::Color::from(context.palette().background_panel),
        "the transcript background did not come from the resolved palette"
    );
    assert_eq!(
        body.fg,
        ratatui::style::Color::from(context.palette().text),
        "the transcript foreground did not come from the resolved palette"
    );
    assert_eq!(
        rule.fg,
        ratatui::style::Color::from(context.palette().border_active),
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

/// A retry is rendered as a warning, not as an error.
///
/// §11.5 assigns `warning` to retry and `error` to failure, and the two must differ: a
/// replay that goes on to succeed painted the same red as a dead turn means a user
/// scanning the transcript for red cannot tell "the provider hiccuped" from "this failed".
#[test]
fn views_retry_rollback_notice_is_visible_in_the_warning_colour() {
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
        .position(|row| row.contains("retry 2/3"))
        .expect("retry notice is rendered");
    assert!(
        !rendered_rows.join("\n").contains("discard me"),
        "rollback kept the failed attempt: {rendered_rows:?}"
    );
    // Column two, not zero: column zero carries the role's left rule, so sampling it
    // would read the rule's colour and never see the notice's.
    assert_eq!(
        buffer[(2, u16::try_from(retry_row).expect("test row fits u16"))].fg,
        ratatui::style::Color::from(context.palette().warning),
        "retry notice did not use the theme's warning colour"
    );
    assert_ne!(
        context.palette().warning,
        context.palette().error,
        "the theme collapsed warning and error, so this assertion proves nothing"
    );
}

/// The retry notice survives the narrowest terminal the layout supports.
///
/// The sentence it replaced ran to 45 columns and was cut after `attempt 2` at 40, which
/// discarded the `2/3` the row existed to state — a notice whose only payload is the part
/// that gets clipped is worse than no notice, because it looks complete.
#[test]
fn views_retry_notice_states_its_count_at_the_narrowest_width() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::RetryRollback {
        attempt: 2,
        max: 3,
    })));
    for width in [40_u16, 60, 80, 120, 200] {
        let joined = draw(&mut view, width, 8).join("\n");
        assert!(
            joined.contains("retry 2/3"),
            "the retry count did not survive {width} columns:\n{joined}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tool call status
// ---------------------------------------------------------------------------

#[test]
fn views_tool_call_walks_pending_running_and_terminal_states() {
    let mut transcript = Transcript::new();
    transcript.observe(&started());
    transcript.observe(&TurnEvent::ToolCallStarted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("zsh"),
        name: String::from("shell"),
        ui_intent: zuno_tool::ToolUiIntent::Generic,
    });
    transcript.observe(&provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("shell"),
    }));
    let state = |transcript: &Transcript| match &transcript.messages()[0].parts[0] {
        MessagePart::Tool {
            name,
            display_name,
            status,
            ..
        } => (name.clone(), display_name.clone(), *status),
        other => panic!("expected a tool part, found {other:?}"),
    };
    assert_eq!(
        state(&transcript),
        (
            String::from("shell"),
            String::from("zsh"),
            ToolStatus::Pending
        )
    );

    transcript.observe(&TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("zsh"),
        name: String::from("shell"),
        ui_intent: zuno_tool::ToolUiIntent::Generic,
    });
    assert_eq!(
        state(&transcript),
        (
            String::from("shell"),
            String::from("zsh"),
            ToolStatus::Running
        )
    );
    assert!(state(&transcript).2.is_active());

    transcript.observe(&TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("zsh"),
        name: String::from("shell"),
        title: String::from("false"),
        output: String::from("a\nb"),
        diff: None,
        written_paths: Vec::new(),
        is_error: true,
    });
    assert_eq!(
        state(&transcript),
        (
            String::from("shell"),
            String::from("zsh"),
            ToolStatus::Error
        )
    );
    assert!(!state(&transcript).2.is_active());
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
        display_name: String::from("grep"),
        name: String::from("grep"),
        ui_intent: zuno_tool::ToolUiIntent::Generic,
    });
    assert_eq!(transcript.messages()[0].parts.len(), 1);
}

#[test]
fn views_tool_affordance_matches_the_oracle_icons() {
    for (name, icon) in [
        ("shell", "$"),
        ("glob", "✱"),
        ("grep", "✱"),
        ("read", "→"),
        ("write", "→"),
        ("webfetch", "%"),
        ("web_search", "◈"),
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
        name: String::from("shell"),
    })));
    let joined = draw(&mut view, 40, 6).join("\n");
    assert!(
        joined.contains("~ Tool · $ Writing command..."),
        "a pending shell call did not render the oracle's placeholder:\n{joined}"
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
fn views_reasoning_and_tool_rows_name_their_different_content_types() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    for event in [
        provider(StreamEvent::ReasoningDelta(String::from(
            "checking the constraints",
        ))),
        provider(StreamEvent::ReasoningDone { duration_secs: 1.5 }),
        provider(StreamEvent::ToolUseStart {
            id: String::from("call_1"),
            name: String::from("shell"),
        }),
        provider(StreamEvent::ToolInputDelta {
            id: String::from("call_1"),
            delta: String::from(r#"{"command":"python3 - <<'PY'\nprint(1)\nPY"}"#),
        }),
    ] {
        view.handle_event(&AppEvent::Engine(event));
    }

    let joined = draw(&mut view, 96, 16).join("\n");
    assert!(
        joined.contains("Thought"),
        "reasoning has no explicit category label:\n{joined}"
    );
    assert!(
        joined.contains("Tool · $ shell"),
        "the tool row is visually indistinguishable from reasoning:\n{joined}"
    );
}

#[test]
fn views_blocked_tool_is_a_warning_while_an_execution_failure_remains_an_error() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    for event in [
        provider(StreamEvent::ToolUseStart {
            id: String::from("blocked"),
            name: String::from("shell"),
        }),
        TurnEvent::ToolDispatchBlocked {
            step: 1,
            call_id: String::from("blocked"),
            kind: zuno_engine::r#loop::ToolBlockKind::InvalidArguments,
        },
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("blocked"),
            display_name: String::from("shell"),
            name: String::from("shell"),
            title: String::from("shell blocked"),
            output: String::from("redirection outside the worktree was refused; nothing ran"),
            diff: None,
            written_paths: Vec::new(),
            is_error: true,
        },
        provider(StreamEvent::ToolUseStart {
            id: String::from("failed"),
            name: String::from("shell"),
        }),
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("failed"),
            display_name: String::from("shell"),
            name: String::from("shell"),
            title: String::from("shell failed"),
            output: String::from("process exited with status 1"),
            diff: None,
            written_paths: Vec::new(),
            is_error: true,
        },
    ] {
        view.handle_event(&AppEvent::Engine(event));
    }

    let lines = view.lines(96);
    let blocked = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("! Tool · $ shell blocked")
        })
        .expect("blocked tool header");
    let failed = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("✗ Tool · $ shell failed")
        })
        .expect("failed tool header");
    let blocked_glyph = blocked
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "!")
        .expect("blocked status glyph");
    let failed_glyph = failed
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "✗")
        .expect("failed status glyph");
    let blocked_title = blocked
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "shell blocked")
        .expect("blocked title");
    let failed_title = failed
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "shell failed")
        .expect("failed title");
    assert_eq!(
        blocked_glyph.style.fg,
        Some(ratatui::style::Color::from(
            ViewContext::defaults().palette().warning
        ))
    );
    assert_eq!(
        failed_glyph.style.fg,
        Some(ratatui::style::Color::from(
            ViewContext::defaults().palette().error
        ))
    );
    assert_eq!(blocked_title.style.fg, ViewContext::defaults().text().fg);
    assert_eq!(failed_title.style.fg, ViewContext::defaults().text().fg);

    let joined = draw(&mut view, 96, 18).join("\n");
    assert!(
        joined.contains("! Tool · $ shell blocked"),
        "the refusal still looks like an execution failure:\n{joined}"
    );
    assert!(
        joined.contains("✗ Tool · $ shell failed"),
        "a process failure lost its error state:\n{joined}"
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
fn views_user_messages_render_commonmark_tables_instead_of_literal_pipes() {
    let mut view = view();
    view.transcript_mut().push(Message::user(
        "| # | 约束 | 形式 |\n| ---: | --- | --- |\n| 1 | Web 不在 A/E | `Web ∉ {A,E}` |\n",
    ));

    let joined = draw(&mut view, 72, 12).join("\n");
    assert!(
        joined.contains("Web") && joined.contains("A/E") && joined.contains("{A,E}"),
        "the table content was lost:\n{joined}"
    );
    assert!(
        joined.contains('┼') && joined.contains('│'),
        "the user message was not rendered as a table:\n{joined}"
    );
    assert!(
        !joined.contains("| ---: |"),
        "the user-facing transcript leaked CommonMark table syntax:\n{joined}"
    );
}

#[test]
fn views_assistant_markdown_and_header_use_neutral_scan_hierarchy() {
    let context = ViewContext::defaults();
    let mut view = TranscriptView::new(context.clone());
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
        String::from("## Approach\n\n1. inspect the call path\n\nBody copy stays primary."),
    ))));

    let lines = view.lines(72);
    let assistant = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == "Assistant")
        })
        .expect("assistant heading");
    let title = assistant
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "Assistant")
        .expect("assistant title span");
    assert!(
        title.style.add_modifier.contains(Modifier::BOLD),
        "the speaker title should be stronger than its body"
    );
    let separator = assistant
        .spans
        .iter()
        .find(|span| span.content.contains('─'))
        .expect("weak separator after the assistant title");
    assert_eq!(
        separator.style.fg,
        context.muted().fg,
        "the speaker separator should recede instead of becoming an accent bar"
    );

    let heading = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "Approach")
        .expect("markdown heading");
    assert_eq!(heading.style.fg, context.title().fg);
    assert!(heading.style.add_modifier.contains(Modifier::BOLD));

    let enumeration = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "1. ")
        .expect("ordered-list marker");
    assert_eq!(
        enumeration.style.fg,
        context.muted().fg,
        "list chrome should use a neutral theme role rather than a bright cyan/blue"
    );

    let body = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "Body")
        .expect("markdown body");
    assert_eq!(
        body.style.fg,
        context.text().fg,
        "ordinary prose must remain the primary reading layer"
    );
}

#[test]
fn views_thinking_style_matches_codex_neutral_dim_italic_text() {
    let context = ViewContext::defaults();
    let style = context.thinking();
    let thinking = style.fg.expect("a foreground");
    assert_eq!(
        thinking,
        context.text().fg.expect("ordinary text foreground"),
        "reasoning should use the terminal's normal foreground rather than purple"
    );
    assert!(
        style.add_modifier.contains(Modifier::DIM),
        "reasoning should be visually secondary without changing hue"
    );
    assert!(
        style.add_modifier.contains(Modifier::ITALIC),
        "reasoning should retain Codex's italic treatment"
    );
}

#[test]
fn views_secondary_ui_text_uses_muted_neutral_foreground() {
    let context = ViewContext::defaults();
    assert_eq!(
        context.secondary().fg,
        context.muted().fg,
        "secondary TUI and sidebar copy should not inherit the theme's purple accent"
    );
}

#[test]
fn views_tool_header_layers_disclosure_status_name_and_summary() {
    let context = ViewContext::defaults();
    let mut view = TranscriptView::new(context.clone());
    view.handle_event(&AppEvent::Engine(started()));
    complete_tool(
        &mut view,
        "call_read",
        "read",
        r#"{"filePath":"crates/zuno-tui/src/views/message.rs"}"#,
        "source",
    );

    let lines = view.lines(96);
    let header = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Tool · → read crates/zuno-tui/src/views/message.rs")
        })
        .expect("tool header");
    let exact = |text: &str| {
        header
            .spans
            .iter()
            .find(|span| span.content.as_ref() == text)
            .unwrap_or_else(|| panic!("tool header has no {text:?} span: {header:#?}"))
    };

    assert_eq!(exact("▸").style.fg, context.muted().fg);
    assert_eq!(exact("✓").style.fg, context.text().fg);
    assert!(
        exact("Tool").style.add_modifier.contains(Modifier::BOLD),
        "the activity kind should read as a compact title"
    );
    assert!(
        exact("read").style.add_modifier.contains(Modifier::BOLD),
        "the tool identity should remain scannable across a dense activity list"
    );
    assert_eq!(
        exact("crates/zuno-tui/src/views/message.rs").style.fg,
        context.muted().fg,
        "the argument summary should recede beneath the tool identity"
    );
}

#[test]
fn views_tool_overflow_hint_uses_muted_chrome_instead_of_accent_text() {
    let context = ViewContext::defaults();
    let mut view = TranscriptView::new(context.clone());
    view.handle_event(&AppEvent::Engine(started()));
    complete_tool(
        &mut view,
        "call_read",
        "read",
        r#"{"filePath":"src/lib.rs"}"#,
        "one\ntwo\nthree\nfour\nfive",
    );

    let overflow = view
        .lines(72)
        .into_iter()
        .flat_map(|line| line.spans.into_iter())
        .find(|span| span.content.contains("more lines"))
        .expect("collapsed output notice");
    assert_eq!(overflow.style.fg, context.muted().fg);
    assert_ne!(
        overflow.style.fg,
        context.accent().fg,
        "a recurring disclosure hint should not read as a purple/green primary action"
    );
}

// ---------------------------------------------------------------------------
// Scrolling and the scrollbar
// ---------------------------------------------------------------------------

/// The reply the real provider produced for `write a markdown table with 2 rows, then a
/// rust code block`, which is the shape the truncation was reported on.
const TABLE_THEN_CODE: &str = "| Name | Value |\n|---|---:|\n| Alpha | 1 |\n| Beta | 2 |\n\n\
     ```rust\nfn main() {\n    println!(\"Hello, Rust!\");\n}\n```\n";

/// Fold `source` into `view` the way the provider delivers it: one delta per chunk.
///
/// One delta at a time rather than one `TextDelta` carrying the whole reply, because the
/// frames that matter are the ones *between* the deltas — a test that pushed a finished
/// message would render exactly one frame and so could not observe a viewport that fails
/// to keep up with content growing underneath it.
fn stream_reply(view: &mut TranscriptView, source: &str, width: u16, height: u16) {
    view.handle_event(&AppEvent::Engine(started()));
    for chunk in source.split_inclusive('\n') {
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
            chunk.to_owned(),
        ))));
        // A frame per delta, which is what the host does: `observe` reports a redraw for
        // every one of them. The offset the last frame leaves behind is the whole subject.
        draw(view, width, height);
    }
    view.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));
}

/// A streaming reply taller than the pane has to end on screen, not above the fold.
///
/// # What this catches
///
/// The transcript's viewport never followed the newest row: `following` was set at
/// construction and read by nothing, and `Component::render` only ever *lowered* an
/// offset that had run past the end. So `offset` stayed at 0 for a session's whole life
/// and every row past `area.height` was below the fold — a reply that overflowed the pane
/// appeared cut off at whatever row the pane happened to end on, which for the reported
/// case was the table's first row.
///
/// # Why it has to stream
///
/// Rendering a finished message would draw one frame, and one frame cannot show a
/// viewport failing to keep up. Neither could a test of `markdown::render`, which returned
/// all ten rows of this reply at every width, nor one of [`TranscriptView::lines`], which
/// is measured before the viewport is applied. The defect lived in exactly the gap between
/// `lines()` and the cells, so the assertion has to be made on painted rows after a
/// sequence of frames.
#[test]
fn views_transcript_follows_the_newest_row_as_a_reply_streams_in() {
    let mut view = view();
    view.transcript_mut().push(Message::user(
        "write a markdown table with 2 rows, then a rust code block",
    ));
    stream_reply(&mut view, TABLE_THEN_CODE, 80, 8);
    let painted = draw(&mut view, 80, 8);
    let screen = painted.join("\n");
    let produced = view.lines(80);
    assert!(
        produced.len() > 8,
        "the fixture stopped overflowing an 8-row pane, so this asserts nothing: \
         {} rows",
        produced.len()
    );

    // Both dimensions, because either alone has a degenerate solution. "The tail is
    // visible" is satisfied by a pane that grew; "the head is gone" is satisfied by a
    // transcript that lost its first message. Together they say the viewport moved.
    assert!(
        screen.contains("println!"),
        "the code block the reply ended with is off screen:\n{screen}"
    );
    assert!(
        !screen.contains("write a markdown table"),
        "the prompt is still on screen, so nothing scrolled and the pane must have grown:\n{screen}"
    );
    assert_eq!(
        view.offset(),
        produced.len() - 8,
        "the viewport is not resting on the newest row"
    );

    // The painted rows are the tail of the produced rows, span for span. Asserting the
    // window rather than two landmarks is what stops a fix that scrolls to some *other*
    // position from passing: only one offset makes this equality hold.
    let tail: Vec<String> = produced[produced.len() - 8..]
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    let shown: Vec<String> = painted
        .iter()
        .map(|row| row.trim_end().to_owned())
        .collect();
    assert_eq!(
        shown, tail,
        "the painted window is not the tail of the transcript"
    );
}

/// A reader who scrolled back stays where they were while the turn keeps growing.
///
/// The counter-dimension to the test above, and the reason the fix is conditional on
/// `following` rather than an unconditional pin to the bottom. An unconditional pin passes
/// every assertion up there and makes the transcript unreadable during a live turn: each
/// delta would yank the view away from the row being read.
#[test]
fn views_transcript_leaves_a_reader_who_scrolled_away_where_they_left_off() {
    let mut view = view();
    view.transcript_mut().push(Message::user(
        "write a markdown table with 2 rows, then a rust code block",
    ));
    view.handle_event(&AppEvent::Engine(started()));
    let mut chunks = TABLE_THEN_CODE.split_inclusive('\n');
    for chunk in chunks.by_ref().take(4) {
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
            chunk.to_owned(),
        ))));
        draw(&mut view, 80, 8);
    }
    // The reader scrolls to the top, which disarms following.
    view.set_offset(0);
    for chunk in chunks {
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
            chunk.to_owned(),
        ))));
        draw(&mut view, 80, 8);
    }
    let screen = draw(&mut view, 80, 8).join("\n");
    assert_eq!(view.offset(), 0, "a live turn yanked the viewport");
    assert!(
        screen.contains("write a markdown table"),
        "the row the reader was on is gone:\n{screen}"
    );
}

/// Every overflowing shape ends on screen, not just markdown prose.
///
/// The cause is the viewport rather than any one block emitter, so a fix that special-cased
/// the reported shape would be a fix to the symptom. Each of these was truncated by the
/// same offset for the same reason, and each is checked by the row it must end on.
#[test]
fn views_transcript_follows_the_newest_row_for_every_overflowing_shape() {
    // Long plain prose: no table, no fence, nothing markdown-specific to blame.
    let mut prose = view();
    prose.transcript_mut().push(Message::user("summarise"));
    let sentences = (0..12)
        .map(|index| format!("Sentence number {index} of the answer.\n"))
        .collect::<String>();
    stream_reply(&mut prose, &sentences, 80, 8);
    assert!(
        draw(&mut prose, 80, 8).join("\n").contains("number 11"),
        "the end of a long prose reply is below the fold"
    );

    // An expanded reasoning block, which wraps with no row cap at all.
    let mut thinking = view();
    thinking.toggle_thinking();
    assert_eq!(thinking.thinking(), ThinkingDisplay::Expanded);
    thinking.transcript_mut().push(Message::user("think"));
    thinking.handle_event(&AppEvent::Engine(started()));
    thinking.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningStart)));
    for index in 0..12 {
        thinking.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDelta(
            format!("thought step {index}\n"),
        ))));
        draw(&mut thinking, 80, 8);
    }
    thinking.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDone {
        duration_secs: 1.0,
    })));
    assert!(
        draw(&mut thinking, 80, 8).join("\n").contains("step 11"),
        "the end of an expanded reasoning block is below the fold"
    );

    // A tool result, which arrives on one event rather than as deltas.
    let mut tool = view();
    tool.transcript_mut().push(Message::user("read the file"));
    tool.handle_event(&AppEvent::Engine(started()));
    tool.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("call_1"),
        name: String::from("read"),
    })));
    draw(&mut tool, 80, 8);
    tool.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("call_1"),
        display_name: String::from("read"),
        name: String::from("read"),
        title: String::from("Read src/main.rs"),
        output: (0..6)
            .map(|index| format!("output row {index}\n"))
            .collect::<String>(),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));
    let painted = draw(&mut tool, 80, 8).join("\n");
    assert!(
        painted.contains("row 5") || painted.contains("more"),
        "a tool result's last row is below the fold:\n{painted}"
    );
}

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
    // The `▲` rule at column zero, not the `Session` heading this used to look for. The
    // heading is gone — see `TranscriptView::role_label` — and the property it was standing
    // in for is unchanged and still checked here: the row must open with the session's own
    // marker and with neither party's. `Role::marker`'s own note says this is what those
    // three glyphs are for, so this reads the stronger carrier rather than a weaker one.
    assert!(
        joined
            .lines()
            .any(|row| row.starts_with(Role::System.marker())),
        "the warning must be attributed to the session, not to the user or the model:\n{joined}"
    );
    assert!(
        !joined.contains(Role::User.marker()) && !joined.contains(Role::Assistant.marker()),
        "the warning is attributed to a party to the conversation:\n{joined}"
    );
    assert!(
        !joined.contains("session titled"),
        "an ordinary status detail must stay on the strip, not fill the transcript:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// The reply identity
// ---------------------------------------------------------------------------

#[test]
fn views_reply_identity_renders_agent_catalog_model_and_effort() {
    let mut status = StatusView::new(ViewContext::defaults());
    status.set_model_names([(
        String::from("myopenai/claude-opus-5"),
        String::from("Claude Opus 5"),
    )]);
    status.describe("Atlas - Plan Executor", "myopenai/claude-opus-5");
    status.set_effort(Some(zuno_llm::effort::ReasoningEffort::Max));

    let buffer = render_offscreen(&mut status, 80, 1).expect("infallible");
    assert_eq!(
        rows(&buffer).remove(0).trim_end(),
        " ▣ Atlas - Plan Executor · Claude Opus 5 (max)"
    );
}

#[test]
fn views_reply_identity_is_empty_until_an_identity_is_known() {
    let mut status = StatusView::new(ViewContext::defaults());
    assert!(!status.has_identity());
    let buffer = render_offscreen(&mut status, 20, 1).expect("infallible");
    assert!(rows(&buffer).remove(0).trim().is_empty());
}

#[test]
fn views_reply_identity_keeps_the_resolved_identity_after_completion() {
    let mut status = StatusView::new(ViewContext::defaults());
    for event in [
        TurnEvent::TurnStarted {
            session_id: String::from("ses_status"),
        },
        TurnEvent::AgentResolved {
            step: 1,
            agent: String::from("build"),
        },
        TurnEvent::ModelResolved {
            step: 1,
            provider_id: String::from("anthropic"),
            model_id: String::from("claude"),
        },
        TurnEvent::TurnCompleted {
            assistant_message_id: String::from("msg_status"),
            steps: 1,
        },
    ] {
        assert!(status.handle_event(&AppEvent::Engine(event)).redraw);
    }
    assert!(!status.is_running());
    let row = rows(&render_offscreen(&mut status, 48, 1).expect("infallible")).remove(0);
    assert!(row.contains("▣ build · claude"), "{row:?}");
    assert!(!row.contains("idle") && !row.contains("working"), "{row:?}");
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
fn views_transcript_marks_an_interrupted_turn_once_as_session_state() {
    let mut view = view();
    view.transcript_mut().push(Message::user("stop this turn"));
    for event in [
        TurnEvent::TurnStarted {
            session_id: String::from("s"),
        },
        started(),
        provider(StreamEvent::TextDelta(String::from("partial reply"))),
        TurnEvent::TurnInterrupted {
            assistant_message_id: Some(String::from("msg_1")),
            steps: 1,
        },
        // Terminal delivery can be retried by a client boundary. The transcript marker
        // describes one turn and therefore remains exactly once.
        TurnEvent::TurnInterrupted {
            assistant_message_id: Some(String::from("msg_1")),
            steps: 1,
        },
    ] {
        view.handle_event(&AppEvent::Engine(event));
    }

    let notices = view
        .transcript()
        .messages()
        .iter()
        .filter_map(|message| {
            let [MessagePart::Notice { text, level }] = message.parts.as_slice() else {
                return None;
            };
            (text == "Conversation interrupted by user.").then_some((message.role, *level))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices,
        vec![(Role::System, crate::views::toast::ToastLevel::Error)],
        "the user interruption must be a single session-owned error marker"
    );

    let joined = draw(&mut view, 72, 12).join("\n");
    assert!(
        joined.contains("Conversation interrupted by user."),
        "the interruption marker is not visible in the conversation:\n{joined}"
    );
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
        accounting: PromptAccounting::CacheInsideInput,
    })));
    let tokens = view.transcript().tokens();
    // 1,120 and not 1,200: OpenAI's `prompt_tokens` of 1,200 *contains* the 80
    // `cached_tokens`, so the plain-rate prompt tokens are the 1,120 that remain. The
    // buckets are stored disjoint, which is what lets `total` be a sum.
    assert_eq!(tokens.input, 1_120);
    assert_eq!(tokens.output, 340);
    assert_eq!(tokens.cache_read, 80);
    // 1,540 and not the 1,620 this test used to freeze. The old figure was
    // `1200 + 340 + 80`, which counted the 80 cached tokens twice — once inside the
    // prompt figure that already contained them and once again as cache. The provider
    // billed 1,200 prompt tokens plus 340 completion tokens, and 1,200 + 340 is 1,540.
    assert_eq!(tokens.total(), 1_540);
    assert!(!tokens.is_empty());

    // Two reports accumulate rather than replace, because a turn bills per step.
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(10),
        cache_read_input_tokens: None,
        cache_write_input_tokens: None,
        accounting: PromptAccounting::CacheInsideInput,
    })));
    assert_eq!(view.transcript().tokens().input, 1_220);
}

#[test]
fn views_transcript_restores_durable_usage_and_marks_unknown_history() {
    let mut transcript = crate::views::message::Transcript::new();
    transcript.restore_usage(zuno_types::UsageSnapshot {
        confirmed: crate::views::message::TokenUsage {
            input: 900,
            output: 100,
            reasoning: 0,
            cache_read: 200,
            cache_write: 0,
            unclassified: 0,
        },
        last_prompt_tokens: Some(1_100),
        context_limit: Some(10_000),
        confirmed_known: true,
        ..zuno_types::UsageSnapshot::default()
    });
    assert_eq!(
        transcript.usage_state(),
        crate::views::message::UsageState::Known
    );
    assert_eq!(transcript.tokens().total(), 1_200);
    assert_eq!(
        transcript.context_window(),
        Some(crate::views::message::ContextWindowUsage {
            prompt_tokens: 1_100,
            limit: 10_000,
            estimated: false,
        })
    );

    transcript.restore_usage(zuno_types::UsageSnapshot {
        last_confirmed_at: Some(1),
        ..zuno_types::UsageSnapshot::default()
    });
    assert_eq!(
        transcript.usage_state(),
        crate::views::message::UsageState::Unavailable
    );
}

#[test]
fn views_transcript_counts_a_cached_token_once_whichever_convention_the_provider_uses() {
    // Two providers reporting the *same* request, in their own conventions: a 1,200-token
    // prompt of which 80 came from cache. OpenAI puts the 80 inside its 1,200; Anthropic
    // reports 1,120 alongside its 80. A session total and a context percentage must not
    // depend on which of the two answered — and before this they did, because both figures
    // were arithmetic on the raw fields.
    let openai = {
        let mut view = view();
        view.handle_event(&AppEvent::Engine(started()));
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
            input_tokens: Some(1_200),
            output_tokens: Some(340),
            cache_read_input_tokens: Some(80),
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        })));
        view
    };
    let anthropic = {
        let mut view = view();
        view.handle_event(&AppEvent::Engine(started()));
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
            input_tokens: Some(1_120),
            output_tokens: Some(340),
            cache_read_input_tokens: Some(80),
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheBesideInput,
        })));
        view
    };

    assert_eq!(
        openai.transcript().tokens(),
        anthropic.transcript().tokens(),
        "the same request billed the same way must land in the same buckets"
    );
    assert_eq!(openai.transcript().tokens().total(), 1_540);
    assert_eq!(
        openai.transcript().last_prompt_tokens(),
        Some(1_200),
        "the whole prompt, cache included, is what occupies the window"
    );
    assert_eq!(anthropic.transcript().last_prompt_tokens(), Some(1_200));
}

#[test]
fn views_failed_provider_request_keeps_confirmed_usage_and_exposes_local_estimate() {
    let mut transcript = Transcript::new();
    transcript.restore_usage(zuno_types::UsageSnapshot {
        confirmed: TokenUsage {
            input: 3_000,
            output: 500,
            reasoning: 0,
            cache_read: 700,
            cache_write: 0,
            unclassified: 0,
        },
        last_prompt_tokens: Some(4_200),
        context_limit: Some(10_000),
        confirmed_known: true,
        ..zuno_types::UsageSnapshot::default()
    });
    let confirmed = transcript.tokens();

    transcript.observe(&TurnEvent::ProviderRequestStarted {
        step: 1,
        message_count: 7,
        estimated_prompt_tokens: 8_500,
    });
    assert_eq!(
        transcript.context_window(),
        Some(ContextWindowUsage {
            prompt_tokens: 8_500,
            limit: 10_000,
            estimated: true,
        })
    );
    transcript.observe(&TurnEvent::TurnFailed {
        assistant_message_id: None,
        steps: 1,
        message: "provider rejected malformed tool input".to_owned(),
    });

    assert_eq!(transcript.tokens(), confirmed);
    assert_eq!(transcript.failed_turns(), 1);
    assert_eq!(transcript.last_prompt_tokens(), Some(4_200));
    assert_eq!(
        transcript.context_window(),
        Some(ContextWindowUsage {
            prompt_tokens: 8_500,
            limit: 10_000,
            estimated: true,
        }),
        "a rejected request keeps its local estimate without overwriting confirmed usage"
    );
}

#[test]
fn views_transcript_context_percentage_measures_the_last_prompt_not_the_session() {
    // The `125%` defect. `context_used` read `tokens.input + tokens.cache_read` from an
    // accumulator whose `add` is `+=`, so two 80k prompts against a 128k window summed to
    // 160k and displayed a percentage that cannot exist. The second prompt did not make
    // the window fuller; it replaced what was in it.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.transcript_mut().set_context_limit(128_000);
    for _ in 0..2 {
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
            input_tokens: Some(80_000),
            output_tokens: Some(500),
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        })));
    }

    let context = view.transcript().context_window().expect("declared window");
    assert_eq!(context.prompt_tokens, 80_000);
    assert_eq!(context.limit, 128_000);
    assert!(
        (context.percent() - 62.5).abs() < f64::EPSILON,
        "80,000 of a 128,000-token window is 62.5%, however many turns have run"
    );
    assert_eq!(
        view.transcript().tokens().input,
        160_000,
        "the cumulative figure is still cumulative; it is simply not the percentage"
    );
}

#[test]
fn views_transcript_context_percentage_counts_cached_prompt_tokens_as_occupying_the_window() {
    // Cache changes what a prompt *costs*, not how much of the window it fills: a
    // 100k-token prompt read from cache still leaves only 28k of a 128k window. A
    // percentage computed from the uncached remainder alone would report an almost-empty
    // window right before the model refuses the next turn.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.transcript_mut().set_context_limit(128_000);
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TokenUsage {
        input_tokens: Some(100_000),
        output_tokens: Some(200),
        cache_read_input_tokens: Some(96_000),
        cache_write_input_tokens: None,
        accounting: PromptAccounting::CacheInsideInput,
    })));

    let context = view.transcript().context_window().expect("declared window");
    assert!((context.percent() - 78.125).abs() < f64::EPSILON);
    assert_eq!(view.transcript().tokens().input, 4_000);
    assert_eq!(view.transcript().tokens().cache_read, 96_000);
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
        accounting: PromptAccounting::CacheInsideInput,
    })));
    assert_eq!(
        view.transcript().context_window(),
        None,
        "a model that declares no window must not produce a percentage"
    );
    view.transcript_mut().set_context_limit(20_000);
    let context = view.transcript().context_window().expect("declared window");
    assert_eq!(context.percent(), 25.0);
    // Output is excluded: the window bounds the prompt, and including completions would
    // climb past 100 on a long session.
    assert!(context.percent() <= 100.0);
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
        display_name: String::from("edit"),
        name: String::from("edit"),
        title: String::from("Edit src/main.rs"),
        output: String::from("@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new();\n }\n"),
        diff: None,
        written_paths: Vec::new(),
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

/// The defect this whole `diff` field exists for: `edit` reports a sentence, so a viewer
/// that could only recognise a patch *in the output* was permanently empty for the one
/// tool that changes code.
#[test]
fn views_transcript_finds_the_patch_of_a_mutation_whose_output_is_a_sentence() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("edit"),
    })));
    let patch = "--- src/main.rs\n+++ src/main.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("edit"),
        name: String::from("edit"),
        title: String::from("src/main.rs"),
        // Exactly what `edit` really returns, and deliberately not a patch.
        output: String::from("Edit applied successfully."),
        diff: zuno_engine::r#loop::ToolDiff::new(Some(String::from(patch)), Vec::new()),
        written_paths: Vec::new(),
        is_error: false,
    }));
    assert!(
        !looks_like_diff("Edit applied successfully."),
        "the premise of this test is that the output alone is not recognisable as a patch"
    );
    assert_eq!(
        view.transcript().latest_diff().as_deref(),
        Some(patch),
        "a mutation's patch must be reachable even though its output is a sentence"
    );
}

/// The pre-existing source must keep working: a shell `git diff` carries its patch *as*
/// its output and has no `diff` field, so dropping that branch would trade one empty
/// viewer for another.
#[test]
fn views_transcript_still_finds_a_patch_that_arrived_as_output() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("shell"),
    })));
    let patch = "@@ -1,2 +1,2 @@\n-old\n+new\n";
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("shell"),
        name: String::from("shell"),
        title: String::from("git diff"),
        output: String::from(patch),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));
    assert_eq!(view.transcript().latest_diff().as_deref(), Some(patch));
}

/// An honest empty viewer beats one showing something that is not the change: a tool that
/// mutated nothing attaches no patch, and `read` never had one to attach.
#[test]
fn views_transcript_reports_no_patch_when_no_tool_produced_one() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("read"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("read"),
        name: String::from("read"),
        title: String::from("src/main.rs"),
        output: String::from("fn main() {}"),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));
    assert_eq!(view.transcript().latest_diff(), None);
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
        name: String::from("shell"),
    })));
    let body = (1..=12)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("shell"),
        name: String::from("shell"),
        title: String::from("ls"),
        output: body,
        diff: None,
        written_paths: Vec::new(),
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
fn views_task_results_render_as_a_child_session_instead_of_raw_envelope_markup() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("task_1"),
        name: String::from("renamed_delegate"),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
        id: String::from("task_1"),
        delta: String::from(
            r#"{"description":"trace the runtime","subagent_type":"deep","prompt":"inspect it"}"#,
        ),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseEnd {
        id: String::from("task_1"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: String::from("task_1"),
        display_name: String::from("renamed_delegate"),
        name: String::from("renamed_delegate"),
        ui_intent: zuno_tool::ToolUiIntent::Subagent,
    }));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("task_1"),
        display_name: String::from("renamed_delegate"),
        name: String::from("renamed_delegate"),
        title: String::from("Delegated runtime trace"),
        output: String::from(
            "<task id=\"ses_child\" state=\"completed\">\n\
             <summary>runtime trace</summary>\n\
             <task_result>\n\
             child answer with the traced call path\n\
             </task_result>\n\
             </task>",
        ),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));

    let joined = draw(&mut view, 80, 20).join("\n");
    assert!(
        joined.contains("session ses_child · completed"),
        "the task row did not identify its child session and state:\n{joined}"
    );
    assert!(
        joined.contains("child answer with the traced call path"),
        "the child result was lost while unwrapping the task envelope:\n{joined}"
    );
    assert!(
        !joined.contains("<task")
            && !joined.contains("<summary>")
            && !joined.contains("<task_result>"),
        "wire markup leaked into the conversation instead of using the task renderer:\n{joined}"
    );
}

#[test]
fn views_reply_identity_contains_no_transient_liveness_or_usage() {
    let mut status = StatusView::new(ViewContext::defaults());
    status.describe("build", "myopenai/gpt-5.6-sol");
    status.set_effort(Some(zuno_llm::effort::ReasoningEffort::Max));
    status.mark_running();
    status.set_awaiting_user(Some(AwaitingUser::Approval));

    let row = rows(&render_offscreen(&mut status, 80, 1).expect("infallible")).remove(0);
    assert!(row.contains("▣ build · gpt-5.6-sol (max)"), "{row:?}");
    for absent in ["working", "idle", "awaiting", "ctx", "tokens", "cancel"] {
        assert!(
            !row.contains(absent),
            "transient footer state leaked into the reply identity: {row:?}"
        );
    }
}

#[test]
fn views_transcript_omits_transient_turn_liveness_rows() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(TurnEvent::TurnStarted {
        session_id: String::from("s"),
    }));
    let busy = draw(&mut view, 60, 12).join("\n");
    assert!(!busy.contains("working"), "{busy}");

    assert!(
        view.transcript_mut()
            .set_awaiting_user(Some(AwaitingUser::Approval))
    );
    let waiting = draw(&mut view, 60, 12).join("\n");
    assert!(!waiting.contains("waiting for your approval"), "{waiting}");
}

/// A repeated report is not a change, so it must not cost a redraw.
#[test]
fn views_transcript_reports_an_unchanged_permission_state_as_no_change() {
    let mut transcript = Transcript::new();
    assert!(transcript.set_awaiting_user(Some(AwaitingUser::Approval)));
    assert!(!transcript.set_awaiting_user(Some(AwaitingUser::Approval)));
    assert_eq!(transcript.awaiting_user(), Some(AwaitingUser::Approval));
}

#[test]
fn views_reply_identity_tracks_awaiting_state_for_the_live_footer() {
    let mut status = StatusView::new(ViewContext::defaults());
    assert!(status.set_awaiting_user(Some(AwaitingUser::Approval)));
    assert_eq!(status.awaiting_user(), Some(AwaitingUser::Approval));
    assert!(!status.set_awaiting_user(Some(AwaitingUser::Approval)));
    assert!(status.set_awaiting_user(None));
    assert_eq!(status.awaiting_user(), None);
}

// ---------------------------------------------------------------------------
// Layout: wide glyphs, vertical rhythm, and the width sweep
// ---------------------------------------------------------------------------

/// A CJK prompt is wrapped in columns, so none of it is lost to the frame.
///
/// This is the defect the width sweep exists for. At 40 columns the transcript used to
/// wrap after 38 *characters*, ratatui clipped the row at 38 *columns*, and everything
/// past the cut was gone — not pushed to the next row, gone, because the wrap had already
/// counted it as delivered. The assertion is therefore in two halves: no row may be wider
/// than the frame, **and** the text must still all be there.
#[test]
fn views_transcript_wraps_wide_glyph_prose_without_losing_any_of_it() {
    let prompt = "帮我把 diff viewer 接上文件树，顺便看一下 wrap 的宽字符行为";
    for width in [40_u16, 60, 80, 120, 200] {
        let mut view = view();
        view.transcript_mut().push(Message::user(prompt));
        for line in view.lines(width) {
            assert_eq!(
                line_columns(&line),
                usize::from(width),
                "a produced row measured {} columns in a {width}-column frame",
                line_columns(&line)
            );
        }
        // Every fragment of the prompt, across however many rows it took. Reconstructed
        // from the produced spans and not from the buffer, because a buffer row interleaves
        // the blank continuation cell the terminal reserves after each wide glyph: `帮我把`
        // comes back as `帮 我 把` there and no substring search can tell that apart from
        // three characters that really were spaced. The tail is the half that a character
        // count used to drop.
        let flowed = view
            .lines(width)
            .iter()
            .flat_map(|line| line.spans.iter().skip(1))
            .map(|span| span.content.trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("");
        for fragment in [
            "帮我把",
            "diff",
            "viewer",
            "接上文件树",
            "一下",
            "wrap",
            "宽字符行为",
        ] {
            assert!(
                flowed.contains(fragment),
                "{fragment:?} was never laid out at {width} columns: {flowed:?}"
            );
        }
        // And the rows do reach real cells: an assertion that stopped at the produced lines
        // would pass on a view that never rendered.
        let rendered = draw(&mut view, width, 24);
        assert!(
            rendered.iter().any(|row| row.contains("wrap")),
            "the produced rows never reached the buffer at {width} columns:\n{rendered:#?}"
        );
    }
}

/// An emoji is two columns wide too, and the tool row that carries one must still fit.
#[test]
fn views_transcript_keeps_emoji_rows_inside_the_frame() {
    let mut view = view();
    view.transcript_mut()
        .push(Message::user("🎉 done 🚀 shipped 🔥 fast"));
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
        String::from("結果は ✅ です — 🎯 命中"),
    ))));
    for width in [20_u16, 40, 60, 80] {
        for line in view.lines(width) {
            assert_eq!(
                line_columns(&line),
                usize::from(width),
                "an emoji row measured {} columns in a {width}-column frame: {line:?}",
                line_columns(&line)
            );
        }
        // And it still reaches a real buffer without panicking.
        let _ = draw(&mut view, width, 20);
    }
}

/// The left rule runs unbroken through a multi-step turn, and breaks between speakers.
///
/// Both halves matter, and they are the same decision seen from two sides. The header is
/// deliberately printed only on a change of speaker so that a five-step reply does not say
/// `Assistant` five times — but the blank row that used to separate two assistant messages
/// cut the rule into one fragment per step, which is exactly the continuity the suppressed
/// header was relying on. So a same-role gap carries the rule and a role change does not,
/// which is also what gives the eye two distinguishable grades of gap.
#[test]
fn views_transcript_rule_survives_a_step_boundary_and_breaks_between_speakers() {
    let mut view = view();
    view.transcript_mut().push(Message::user("go"));
    for event in [
        started(),
        provider(StreamEvent::TextDelta(String::from("step one"))),
        TurnEvent::AssistantMessageCreated {
            step: 2,
            message_id: String::from("msg_2"),
        },
        provider(StreamEvent::TextDelta(String::from("step two"))),
    ] {
        view.transcript_mut().observe(&event);
    }
    view.transcript_mut()
        .push(Message::notice("warning: heads up"));

    let rendered = draw(&mut view, 48, 20);
    let one = rendered
        .iter()
        .position(|row| row.contains("step one"))
        .expect("the first step is rendered");
    let two = rendered
        .iter()
        .position(|row| row.contains("step two"))
        .expect("the second step is rendered");
    assert_eq!(
        two,
        one + 2,
        "the two steps are not separated by exactly one row:\n{rendered:#?}"
    );
    let step_gap = &rendered[one + 1];
    assert!(
        step_gap.starts_with(Role::Assistant.marker()),
        "the gap inside one reply dropped the rule, so the turn reads as two: {step_gap:?}"
    );
    assert!(
        step_gap.trim().len() <= Role::Assistant.marker().len(),
        "the gap row carries content as well as the rule: {step_gap:?}"
    );

    // Located by the notice's own text rather than by a `Session` heading, which no longer
    // exists. The assertions below are unchanged: what is being checked is the *separator*
    // above the session's first row, and that row is now the notice itself.
    let session = rendered
        .iter()
        .position(|row| row.contains("heads up"))
        .expect("the notice is rendered");
    let speaker_gap = &rendered[session - 1];
    assert!(
        speaker_gap.trim().is_empty(),
        "the change of speaker is not marked by a blank row: {speaker_gap:?}"
    );
    assert!(
        !speaker_gap.starts_with(Role::Assistant.marker())
            && !speaker_gap.starts_with(Role::System.marker()),
        "the change of speaker kept a rule, so it reads like another step: {speaker_gap:?}"
    );
}

/// Collapsed reasoning costs one row, and says whether it has finished.
///
/// Two rows was the previous form: a header plus an indented summary. Reasoning arrives on
/// every step of every turn, so at four steps that is eight rows of secondary content ahead
/// of an answer that might be three. The summary moves onto the header instead.
#[test]
fn views_collapsed_reasoning_is_one_row_and_reports_its_tense() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDelta(
        String::from("## Approach\nread the file first\nthen decide"),
    ))));

    let streaming = draw(&mut view, 60, 12);
    let live = streaming
        .iter()
        .filter(|row| row.contains("◇ Thought"))
        .count();
    assert_eq!(
        live, 1,
        "collapsed reasoning spent more than one row:\n{streaming:#?}"
    );
    let header = streaming
        .iter()
        .find(|row| row.contains("thinking…"))
        .expect("a block still receiving deltas says so in the present tense");
    assert!(
        header.contains("Approach"),
        "the one row it gets does not say what the reasoning is about: {header:?}"
    );
    assert!(
        !header.contains("12.0s"),
        "an unfinished block claimed a duration it has not been told: {header:?}"
    );

    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDone {
        duration_secs: 12.0,
    })));
    let done = draw(&mut view, 60, 12).join("\n");
    assert!(
        done.contains("◇ Thought · 12.0s"),
        "a finished block did not switch to the past tense with its duration:\n{done}"
    );
}

/// A summary is dropped rather than wrapped when the row cannot hold it.
///
/// Wrapping it would spend the second row the one-row form exists to save, and the glyph
/// plus the duration are what the row is actually for.
#[test]
fn views_collapsed_reasoning_drops_a_summary_that_would_not_fit() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDelta(
        String::from("a summary far too long for a narrow terminal to carry\nmore"),
    ))));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDone {
        duration_secs: 1.0,
    })));
    let narrow = draw(&mut view, 30, 10);
    let rows_used = narrow
        .iter()
        .filter(|row| row.contains("◇ Thought"))
        .count();
    assert_eq!(
        rows_used, 1,
        "the summary took a row of its own:\n{narrow:#?}"
    );
    let joined = narrow.join("\n");
    assert!(
        joined.contains("◇ Thought · 1.0s"),
        "the duration was dropped instead of the summary:\n{joined}"
    );
    assert!(
        !joined.contains("summary far too long"),
        "an unfittable summary was rendered anyway and clipped:\n{joined}"
    );
    for line in view.lines(30) {
        assert_eq!(
            line_columns(&line),
            30,
            "the collapsed row measured {} columns in a 30-column frame",
            line_columns(&line)
        );
    }
}

/// The collapse notice is marked as elided, not as another collapsible header.
///
/// `▸` opens the block it labels, and a collapsed reasoning header a few rows above is
/// using it for exactly that, so the same glyph on a truncation notice read as a second
/// nested section. `…` is the mark this crate already uses for a cut.
#[test]
fn views_tool_output_overflow_is_marked_as_a_cut_not_as_a_header() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("shell"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("shell"),
        name: String::from("shell"),
        title: String::from("ls"),
        output: (1..=9)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));
    let rendered = draw(&mut view, 60, 20);
    let notice = rendered
        .iter()
        .find(|row| row.contains("more lines"))
        .expect("the collapse notice is rendered");
    assert!(
        notice.contains(ELIDED),
        "the notice does not mark itself as a cut: {notice:?}"
    );
    assert!(
        !notice.contains(ThinkingDisplay::Collapsed.glyph()),
        "the notice still borrows the reasoning header's expand glyph: {notice:?}"
    );
}

/// The transcript renders at every supported width, and does not panic when squeezed.
///
/// 20x10 is §11.6's floor. The wide-glyph body is included because the arithmetic that
/// breaks at a small width is the same arithmetic wide glyphs break.
#[test]
fn views_transcript_renders_at_every_width_without_overrunning_or_panicking() {
    for (width, height) in [
        (200_u16, 40_u16),
        (120, 30),
        (80, 24),
        (60, 20),
        (40, 16),
        (20, 10),
    ] {
        let mut view = view();
        view.transcript_mut().push(Message::user("测试宽字符 wrap"));
        for event in [
            started(),
            provider(StreamEvent::ReasoningDelta(String::from(
                "推理内容\n第二行",
            ))),
            provider(StreamEvent::TextDelta(String::from("回答 with ascii"))),
            provider(StreamEvent::ToolUseStart {
                id: String::from("c1"),
                name: String::from("read"),
            }),
            TurnEvent::ToolDispatchCompleted {
                step: 1,
                call_id: String::from("c1"),
                display_name: String::from("read"),
                name: String::from("read"),
                title: String::from("读取 crates/zuno-tui/src/views/message.rs"),
                output: String::from("一行\n二行\n三行\n四行\n五行"),
                diff: None,
                written_paths: Vec::new(),
                is_error: false,
            },
            provider(StreamEvent::RetryRollback { attempt: 1, max: 3 }),
        ] {
            view.transcript_mut().observe(&event);
        }
        view.transcript_mut().push(Message::notice("warning: 注意"));
        for line in view.lines(width) {
            assert_eq!(
                line_columns(&line),
                usize::from(width),
                "a row measured {} columns at {width}x{height}",
                line_columns(&line)
            );
        }
        // Then prove the rows reach a buffer of that size at all: §11.6's floor is 20x10,
        // and a view that computes correct rows and panics on the way out is still broken.
        let _ = draw(&mut view, width, height);
    }
}

/// `wrap` measures columns, and never hangs on a glyph wider than the row.
#[test]
fn views_wrap_measures_columns_and_survives_a_one_column_row() {
    // Six columns of CJK in a four-column row: two glyphs, then one.
    assert_eq!(wrap("日本語", 4), vec!["日本", "語"]);
    assert_eq!(wrap("日本語", 6), vec!["日本語"]);
    // A two-column glyph cannot fit a one-column row. It is emitted alone rather than
    // consuming zero bytes forever, which is what an unguarded `truncate` would do. The
    // trailing empty row is the pre-existing shape of a word consumed exactly: it predates
    // the column measurement and only occurs at width one, which no transcript body is
    // ever laid out at.
    assert_eq!(wrap("日本", 1), vec!["日", "本", ""]);
    for row in wrap("帮我把 diff viewer 接上文件树", 12) {
        assert!(
            crate::views::display_width(&row) <= 12,
            "wrap produced a row wider than it was asked for: {row:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// P2-4: the argument summary reaches the frame
// ---------------------------------------------------------------------------

/// How far a row's body sits past the role's left rule, in columns.
///
/// Measured *after* the rule rather than from column zero, and that is the whole reason
/// this helper exists: a naive `row.len() - row.trim_start().len()` returns zero for every
/// transcript row, because the first character is `▌`, `│` or `▲` and none of them is
/// whitespace. A first version of the indent assertions used exactly that and reported
/// `0 > 0` for a pair of rows the rendered frame showed correctly inset — the test was
/// wrong, not the layout.
fn body_indent(row: &str) -> usize {
    let body = row
        .strip_prefix(Role::User.marker())
        .or_else(|| row.strip_prefix(Role::Assistant.marker()))
        .or_else(|| row.strip_prefix(Role::System.marker()))
        .unwrap_or(row);
    body.len() - body.trim_start().len()
}

/// Feed one complete tool call, arguments included, and return the drawn rows.
fn tool_call(name: &str, arguments: &str, output: &str, diff: Option<&str>) -> Vec<String> {
    tool_call_shown(name, arguments, output, diff, ToolDisplay::Collapsed)
}

/// [`tool_call`], with the output affordance set to `display`.
///
/// Expanded is a distinct fixture rather than a flag on the assertions, because the
/// collapsed cap is only three rows: a diff assertion written against the collapsed view
/// was looking for an added line that the cap had correctly withheld, and read as a
/// missing-diff failure when the diff was in fact rendering.
fn tool_call_shown(
    name: &str,
    arguments: &str,
    output: &str,
    diff: Option<&str>,
    display: ToolDisplay,
) -> Vec<String> {
    let mut view = view();
    if display == ToolDisplay::Expanded {
        view.toggle_tool_output();
    }
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: name.to_owned(),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
        id: String::from("c1"),
        delta: arguments.to_owned(),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseEnd {
        id: String::from("c1"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: name.to_owned(),
        name: name.to_owned(),
        // Deliberately a title that names the *kind* of work and drops the argument, which
        // is what a real provider sends and what used to be all the row said.
        title: String::from("Ran a tool"),
        output: output.to_owned(),
        diff: diff.and_then(|patch| {
            zuno_engine::r#loop::ToolDiff::new(Some(patch.to_owned()), Vec::new())
        }),
        written_paths: Vec::new(),
        is_error: false,
    }));
    draw(&mut view, 90, 30)
}

fn complete_tool(view: &mut TranscriptView, id: &str, name: &str, arguments: &str, output: &str) {
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: id.to_owned(),
        name: name.to_owned(),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
        id: id.to_owned(),
        delta: arguments.to_owned(),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseEnd {
        id: id.to_owned(),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: id.to_owned(),
        display_name: name.to_owned(),
        name: name.to_owned(),
        title: String::from("Ran a tool"),
        output: output.to_owned(),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));
}

#[test]
fn views_compacted_activity_lists_each_command_read_and_search_summary() {
    let context = ViewContext::defaults();
    let mut view = TranscriptView::new(context.clone());
    view.set_activity_display(ActivityDisplay::Summary);
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDelta(
        String::from("Inspect the rendering path before changing it."),
    ))));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDone {
        duration_secs: 1.2,
    })));
    complete_tool(
        &mut view,
        "command",
        "exec_command",
        r#"{"cmd":"cargo test -p zuno-tui"}"#,
        "command output",
    );
    complete_tool(
        &mut view,
        "read",
        "read",
        r#"{"path":"crates/zuno-tui/src/views/message.rs","offset":1884,"limit":48}"#,
        "file contents",
    );
    complete_tool(
        &mut view,
        "search",
        "google_search",
        r#"{"query":"Codex tool activity presentation"}"#,
        "search results",
    );

    let compact_lines = view.cached_lines_for_test(100);
    let compact = row_text(&compact_lines).join("\n");
    for expected in [
        "$ cargo test -p zuno-tui",
        "→ read · crates/zuno-tui/src/views/message.rs [offset=1884,limit=48]",
        "◈ search · Codex tool activity presentation",
        "Ctrl+T details",
    ] {
        assert!(
            compact.contains(expected),
            "compacted activity omitted {expected:?}:\n{compact}"
        );
    }
    for hidden in ["command output", "file contents", "search results"] {
        assert!(
            !compact.contains(hidden),
            "compacted activity leaked full result {hidden:?}:\n{compact}"
        );
    }
    let command_style = compact_lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("cargo test -p zuno-tui"))
        .expect("the command summary has a rendered span")
        .style;
    assert_eq!(
        command_style.fg,
        context.secondary().fg,
        "activity summaries should use the neutral secondary palette"
    );

    view.set_activity_display(ActivityDisplay::Detailed);
    view.toggle_tool_output();
    let detailed = draw(&mut view, 120, 80).join("\n");
    for expected in [
        "Arguments",
        r#""cmd": "cargo test -p zuno-tui""#,
        "Result",
        "command output",
        "file contents",
        "search results",
    ] {
        assert!(
            detailed.contains(expected),
            "expanded activity lost {expected:?}:\n{detailed}"
        );
    }
}

#[test]
fn views_tool_row_names_the_argument_and_not_only_the_kind_of_work() {
    // The P2-4 defect, stated as an assertion: `title` alone said `Read src/main.rs` for
    // one call and `Read src/lib.rs` for the next only because the provider chose to put
    // the path in its sentence — and for `glob`, `grep` and `shell` it did not. The
    // arguments are the only reliable source, so the row is built from them.
    let rendered = tool_call(
        "read",
        r#"{"filePath":"crates/zuno-tui/src/views/diff.rs"}"#,
        "x",
        None,
    );
    let joined = rendered.join("\n");
    assert!(
        joined.contains("read crates/zuno-tui/src/views/diff.rs"),
        "the tool row did not name the file it read:\n{joined}"
    );
    assert!(
        !joined.contains("Ran a tool"),
        "the provider's generic title displaced the argument summary:\n{joined}"
    );
}

#[test]
fn views_shell_row_uses_the_real_interpreter_instead_of_the_wire_id() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("shell"),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
        id: String::from("c1"),
        delta: String::from(r#"{"command":"cargo test"}"#),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("zsh"),
        name: String::from("shell"),
        title: String::from("Ran cargo test"),
        output: String::from("ok"),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));

    let joined = draw(&mut view, 80, 12).join("\n");
    assert!(
        joined.contains("Tool · $ zsh cargo test"),
        "the completed shell call did not use its actual interpreter:\n{joined}"
    );
    assert!(
        !joined.contains("Tool · $ shell"),
        "the provider-facing wire id leaked into the user-facing shell label:\n{joined}"
    );
}

#[test]
fn views_tool_row_falls_back_to_the_title_when_the_arguments_never_parsed() {
    // A completed call whose argument JSON never arrived still has the provider's sentence,
    // and a sentence beats the bare wire name.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("read"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("read"),
        name: String::from("read"),
        title: String::from("Read something"),
        output: String::from("x"),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));
    let joined = draw(&mut view, 60, 12).join("\n");
    assert!(
        joined.contains("Read something"),
        "a call with no parseable arguments lost the provider's title too:\n{joined}"
    );
}

#[test]
fn views_tool_arguments_accumulate_across_the_deltas_that_carry_them() {
    // The provider writes the JSON in fragments, and this is the fold that keeps them.
    // Nothing downstream of the engine carries the arguments — `ToolDispatchCompleted` has
    // `title`, `output` and `diff` only — so if this fold is dropped, every per-tool
    // summary silently reverts to the title.
    let mut transcript = Transcript::new();
    transcript.observe(&started());
    transcript.observe(&provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("shell"),
    }));
    for fragment in [r#"{"comm"#, r#"and":"cargo "#, r#"test"}"#] {
        assert!(
            transcript.observe(&provider(StreamEvent::ToolInputDelta {
                id: String::from("c1"),
                delta: String::from(fragment),
            })),
            "an argument fragment reported nothing changed, so the row would not redraw"
        );
    }
    match &transcript.messages()[0].parts[0] {
        MessagePart::Tool { arguments, .. } => {
            assert_eq!(arguments, r#"{"command":"cargo test"}"#);
        }
        other => panic!("expected a tool part, found {other:?}"),
    }
}

#[test]
fn views_tool_row_of_each_tool_is_distinguishable_from_the_others() {
    // The property the whole of §7.5 exists for. Six calls in one turn, each rendering a
    // row that names what *it* did — the failure being six identical rows.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    for (index, (name, arguments)) in [
        ("read", r#"{"filePath":"src/a.rs"}"#),
        ("grep", r#"{"pattern":"fn main"}"#),
        ("shell", r#"{"command":"cargo build"}"#),
        ("web_search", r#"{"queries":["ratatui spans"]}"#),
        (
            "todo_update",
            r#"{"changes":[{"action":"add","id":"todo_ship","subject":"ship it","description":"ship it","status":"pending","priority":"high"}]}"#,
        ),
        (
            "memory_propose",
            r#"{"target":"project","action":"add","content":"run cargo fmt"}"#,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("c{index}");
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
            id: id.clone(),
            name: name.to_owned(),
        })));
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
            id: id.clone(),
            delta: arguments.to_owned(),
        })));
        view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: id,
            display_name: name.to_owned(),
            name: name.to_owned(),
            title: String::from("Ran a tool"),
            output: String::new(),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        }));
    }
    let rendered = draw(&mut view, 90, 30);
    for expected in [
        "read src/a.rs",
        "grep \"fn main\"",
        "shell cargo build",
        "web_search ratatui spans",
        "todo_update 1 changes · ship it",
        "memory_propose add project: run cargo fmt",
    ] {
        assert!(
            rendered.iter().any(|row| row.contains(expected)),
            "no row carried {expected:?}, so this call is indistinguishable from its \
             siblings:\n{}",
            rendered.join("\n")
        );
    }
}

#[test]
fn views_tool_row_is_inset_one_column_past_the_prose_it_belongs_to() {
    // §7.5's tool indent, asserted positionally rather than by searching for a label: the
    // rule occupies column 0, the prose starts at column 2, and a tool row starts at 3.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
        String::from("plain prose"),
    ))));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("read"),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
        id: String::from("c1"),
        delta: String::from(r#"{"filePath":"src/a.rs"}"#),
    })));
    let rendered = draw(&mut view, 60, 12);
    let prose = rendered
        .iter()
        .find(|row| row.contains("plain prose"))
        .expect("the prose row");
    let tool = rendered
        .iter()
        .find(|row| row.contains("src/a.rs"))
        .expect("the tool row");
    assert!(
        body_indent(tool) > body_indent(prose),
        "the tool row is not inset past the prose, so it reads as another paragraph of \
         the reply:\nprose {prose:?}\ntool  {tool:?}"
    );
}

// ---------------------------------------------------------------------------
// P2-4: collapse, and its affordance
// ---------------------------------------------------------------------------

#[test]
fn views_collapsed_tool_output_names_the_key_that_expands_it() {
    // The eye-caught defect: the notice rendered `… 9 more lines` with nothing after it,
    // because `key_label` reads the upstream table where `tool_details` is `none` — while
    // this build binds it through `SHIPPED_DEFAULTS` and the key worked the whole time. A
    // cap the user cannot discover how to lift is a cap that hides content permanently.
    let rendered = tool_call(
        "shell",
        r#"{"command":"ls"}"#,
        &(1..=9)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
        None,
    );
    let notice = rendered
        .iter()
        .find(|row| row.contains("more lines"))
        .expect("the collapse notice is rendered");
    let key = crate::views::pressable_label("tool_details", &ViewContext::defaults())
        .expect("this build binds tool_details");
    assert!(
        notice.contains(&key),
        "the notice hides output without saying which key returns it — it must name the \
         binding the running keymap resolved ({key}): {notice:?}"
    );
    assert!(
        !notice.contains("<leader>"),
        "the notice printed a leader token, which names no key a user can press: {notice:?}"
    );
}

#[test]
fn views_collapse_threshold_is_the_preview_row_count_and_the_notice_counts_the_rest() {
    // The boundary, on both sides. One row under the cap must produce no notice at all —
    // a notice reading `… 0 more lines` is worse than none — and one row over must state
    // exactly how many were withheld.
    let rows = |count: usize| {
        let output = (1..=count)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        tool_call("shell", r#"{"command":"ls"}"#, &output, None).join("\n")
    };
    let under = rows(TOOL_OUTPUT_PREVIEW_ROWS);
    assert!(
        !under.contains("more lines"),
        "output that fitted still claimed it was cut:\n{under}"
    );
    let over = rows(TOOL_OUTPUT_PREVIEW_ROWS + 1);
    assert!(
        over.contains("1 more lines"),
        "one row over the cap did not report exactly one hidden row:\n{over}"
    );
    let far_over = rows(TOOL_OUTPUT_PREVIEW_ROWS + 12);
    assert!(
        far_over.contains("12 more lines"),
        "the hidden count is wrong:\n{far_over}"
    );
}

#[test]
fn views_expanding_tool_output_lifts_the_cap_and_removes_the_notice() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("shell"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("shell"),
        name: String::from("shell"),
        title: String::from("ls"),
        output: (1..=9)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    }));
    let collapsed = draw(&mut view, 60, 24).join("\n");
    assert!(collapsed.contains("more lines"), "{collapsed}");
    assert!(!collapsed.contains("line 9"), "{collapsed}");

    view.toggle_tool_output();
    let expanded = draw(&mut view, 60, 24).join("\n");
    assert!(
        expanded.contains("line 9"),
        "expanding did not reveal the withheld rows:\n{expanded}"
    );
    assert!(
        !expanded.contains("more lines"),
        "the notice survived after the cap it describes was lifted:\n{expanded}"
    );
}

#[test]
fn views_expanded_tool_call_reveals_structured_arguments_and_result_sections() {
    let arguments = r#"{"query":"Zuno agent delegation","urls":["https://example.com/a","https://example.com/b"]}"#;
    let collapsed = tool_call("google_search", arguments, "two relevant sources", None).join("\n");
    assert!(
        !collapsed.contains("Arguments"),
        "the compact transcript should stay compact:\n{collapsed}"
    );

    let expanded = tool_call_shown(
        "google_search",
        arguments,
        "two relevant sources",
        None,
        ToolDisplay::Expanded,
    )
    .join("\n");
    for expected in [
        "Arguments",
        r#""query": "Zuno agent delegation""#,
        r#""urls": ["#,
        "Result",
        "two relevant sources",
    ] {
        assert!(
            expanded.contains(expected),
            "expanded tool details are missing {expected:?}:\n{expanded}"
        );
    }
}

#[test]
fn views_a_single_enormous_line_is_reported_as_cut_rather_than_silently_clipped() {
    // The other way output hides: one row to the row cap, a megabyte to the wrap. Without
    // the character cap the wrap pays for the whole thing; without the *notice* the reader
    // trusts a truncated line as complete.
    let huge = "x".repeat(crate::views::tool::COLLAPSED_CHARS + 500);
    let rendered = tool_call("shell", r#"{"command":"cat big"}"#, &huge, None).join("\n");
    assert!(
        rendered.contains("cut at"),
        "an over-long single line was clipped with no notice, so the reader cannot tell \
         it from a complete result:\n{rendered}"
    );
}

// ---------------------------------------------------------------------------
// P2-4: a result that carries a patch renders as a diff
// ---------------------------------------------------------------------------

#[test]
fn views_tool_result_renders_the_patch_field_and_not_only_a_diff_shaped_output() {
    // The dropped-`diff` defect. `TurnEvent::ToolDispatchCompleted` carries the patch
    // beside the output, `Transcript::latest_diff` reads it, and the *transcript* ignored
    // it: `tool_output_lines` only ever diff-sniffed `output`, and every mutating tool's
    // output is a sentence. So `edit` — the one tool that changes code — showed
    // `applied 1 change` and nothing else, while the patch sat in the part unread.
    let rendered = tool_call_shown(
        "edit",
        r#"{"filePath":"src/a.rs","oldString":"old();","newString":"new();"}"#,
        "applied 1 change",
        Some("@@ -1,3 +1,4 @@\n fn main() {\n-    old();\n+    new();\n }\n"),
        ToolDisplay::Expanded,
    );
    let joined = rendered.join("\n");
    assert!(
        joined.contains("@@ -1,3 +1,4 @@"),
        "the hunk header is missing, so the patch was not rendered as a diff:\n{joined}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("old();")),
        "the removed line is missing:\n{joined}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("new();")),
        "the added line is missing:\n{joined}"
    );
    // And it went through the real diff renderer, which is what puts line numbers on it —
    // rather than through the prose path, which would print the patch text unstyled.
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("1") && row.contains("fn main()")),
        "the context row carries no line number, so this is unstyled prose rather than \
         the diff view:\n{joined}"
    );
}

#[test]
fn views_a_diff_bearing_result_uses_the_diff_palette_not_the_muted_output_style() {
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("edit"),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("edit"),
        name: String::from("edit"),
        title: String::from("Edit"),
        output: String::from("applied 1 change"),
        diff: zuno_engine::r#loop::ToolDiff::new(
            Some(String::from("@@ -1,2 +1,2 @@\n-old\n+new\n")),
            Vec::new(),
        ),
        written_paths: Vec::new(),
        is_error: false,
    }));
    let context = ViewContext::defaults();
    let added = ratatui::style::Color::from(context.palette().diff_added);
    let painted = view.lines(60).into_iter().any(|line| {
        line.spans
            .iter()
            .any(|span| span.style.fg == Some(added) && span.content.contains("new"))
    });
    assert!(
        painted,
        "the added line is not painted in the palette's diff colour, so the patch is \
         rendering as plain output"
    );
}

#[test]
fn views_a_failed_tool_marks_the_call_as_error_but_keeps_its_output_readable() {
    // The error hangs below the tool row rather than replacing it. The status row carries the
    // red error semantic; prose remains primary text so a multi-line diagnostic stays readable.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("shell"),
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
        id: String::from("c1"),
        delta: String::from(r#"{"command":"false"}"#),
    })));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("shell"),
        name: String::from("shell"),
        title: String::from("false"),
        output: String::from("exit status 1"),
        diff: None,
        written_paths: Vec::new(),
        is_error: true,
    }));
    let rendered = draw(&mut view, 60, 12);
    let joined = rendered.join("\n");
    assert!(
        joined.contains("shell false"),
        "the failing call stopped naming itself, so the error replaced the row instead of \
         hanging below it:\n{joined}"
    );
    let context = ViewContext::defaults();
    let error = ratatui::style::Color::from(context.palette().error);
    let primary = ratatui::style::Color::from(context.palette().text);
    let lines = view.lines(60);
    assert!(
        lines.iter().any(|line| line.spans.iter().any(|span| {
            span.style.fg == Some(error)
                && (span.content.contains('✗') || span.content.contains("shell"))
        })),
        "the failed call row did not carry the semantic error colour"
    );
    assert!(
        lines.iter().any(|line| line.spans.iter().any(|span| {
            span.style.fg == Some(primary) && span.content.contains("exit status 1")
        })),
        "the diagnostic body was not kept in readable primary text"
    );
}

// ---------------------------------------------------------------------------
// P2-5: reasoning is visually subordinate to the answer
// ---------------------------------------------------------------------------

#[test]
fn views_reasoning_is_inset_past_the_answer_in_both_display_states() {
    // §11.2's fourth item. Reasoning recurs on every step and is frequently longer than
    // the reply, so flush with the prose it competes with the answer for the eye: the
    // header sat exactly where the reply's first sentence sits.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDelta(
        String::from("weighing the options\nsecond thought"),
    ))));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDone {
        duration_secs: 12.0,
    })));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
        String::from("the answer"),
    ))));

    let indent = |rows: &[String], needle: &str| {
        let row = rows
            .iter()
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row carried {needle:?} in {rows:#?}"));
        body_indent(row)
    };

    let collapsed = draw(&mut view, 70, 16);
    let answer = indent(&collapsed, "the answer");
    assert!(
        indent(&collapsed, "◇ Thought") > answer,
        "the collapsed reasoning header is flush with the answer, so it competes with it:\n{}",
        collapsed.join("\n")
    );

    view.toggle_thinking();
    let expanded = draw(&mut view, 70, 16);
    assert!(
        indent(&expanded, "second thought") > indent(&expanded, "◇ Thought"),
        "the reasoning body is not nested under its own header, so a long thought reads \
         as the reply:\n{}",
        expanded.join("\n")
    );
    assert!(
        indent(&expanded, "second thought") > answer,
        "the reasoning body is not subordinate to the answer:\n{}",
        expanded.join("\n")
    );
}

#[test]
fn views_reasoning_body_is_dimmer_than_the_answer_and_the_answer_is_not_italic() {
    // Weight and posture carry the hierarchy without assigning reasoning a competing hue.
    // Asserted on the produced spans, because a rendered cell cannot report a modifier.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ReasoningDelta(
        String::from("weighing the options"),
    ))));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::TextDelta(
        String::from("the answer"),
    ))));
    view.toggle_thinking();
    let lines = view.lines(70);
    let span_with = |needle: &str| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains(needle))
            .unwrap_or_else(|| panic!("no span carried {needle:?}"))
            .clone()
    };
    let reasoning = span_with("weighing the options");
    // `"answer"`, not `"the answer"`: markdown emits one span per word, so the assistant's
    // prose arrives as `["the", " ", "answer"]` and no single span holds the phrase. The
    // reasoning body is plain-wrapped and does keep its whole row in one span, which is why
    // the two needles differ in shape.
    let answer = span_with("answer");
    assert!(
        reasoning.style.add_modifier.contains(Modifier::ITALIC),
        "the reasoning body is not italic, so one of the two hierarchy signals is missing"
    );
    assert!(
        reasoning.style.add_modifier.contains(Modifier::DIM),
        "the reasoning body is not dim, so it competes with the answer"
    );
    assert!(
        !answer.style.add_modifier.contains(Modifier::ITALIC),
        "the answer is italic too, which erases the distinction"
    );
    assert!(
        !answer.style.add_modifier.contains(Modifier::DIM),
        "the answer is dim too, which erases the distinction"
    );
    assert_eq!(
        reasoning.style.fg, answer.style.fg,
        "reasoning should use neutral text hierarchy rather than a purple accent"
    );
}

// ---------------------------------------------------------------------------
// P2-4/P2-5 at width: wide glyphs, and the narrow frames
// ---------------------------------------------------------------------------

#[test]
fn views_a_cjk_tool_argument_stays_inside_the_frame_at_every_width() {
    // The §11.5 rule on the surface P2-4 added. A CJK path measured in characters comes
    // back "short enough", and ratatui then clips it — so the tail this row exists to show
    // is discarded, and the row count the scroller trusts is wrong by the same factor.
    for width in [200_u16, 120, 80, 60, 40, 20] {
        let mut view = view();
        view.handle_event(&AppEvent::Engine(started()));
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
            id: String::from("c1"),
            name: String::from("read"),
        })));
        view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolInputDelta {
            id: String::from("c1"),
            delta: String::from(
                r#"{"filePath":"crates/文档/说明书/読み方.rs","offset":1,"limit":9}"#,
            ),
        })));
        view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("c1"),
            display_name: String::from("read"),
            name: String::from("read"),
            title: String::from("Read"),
            output: String::from("说明\n読み\n混合 mixed 内容\nfourth"),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        }));
        for line in view.lines(width) {
            assert_eq!(
                line_columns(&line),
                usize::from(width),
                "a CJK tool row measured {} columns at width {width}",
                line_columns(&line)
            );
        }
        // And it reaches a buffer of that size without panicking, including §11.6's floor.
        let _ = draw(&mut view, width, if width == 20 { 10 } else { 24 });
    }
}

#[test]
fn views_a_tool_call_survives_the_smallest_supported_frame() {
    // 20x10, §11.6's floor, with every part kind a tool call can bring: a summary too long
    // for the row, a collapse notice too long for the row, and a diff.
    let mut view = view();
    view.handle_event(&AppEvent::Engine(started()));
    view.handle_event(&AppEvent::Engine(provider(StreamEvent::ToolUseStart {
        id: String::from("c1"),
        name: String::from("edit"),
    })));
    view.handle_event(&AppEvent::Engine(provider(
        StreamEvent::ToolInputDelta {
            id: String::from("c1"),
            delta: String::from(
                r#"{"filePath":"crates/zuno-tui/src/views/message.rs","oldString":"a","newString":"b"}"#,
            ),
        },
    )));
    view.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c1"),
        display_name: String::from("edit"),
        name: String::from("edit"),
        title: String::from("Edit"),
        output: String::from("applied"),
        diff: zuno_engine::r#loop::ToolDiff::new(
            Some(String::from(
                "@@ -1,4 +1,4 @@\n context\n-removed line\n+added line\n more context\n",
            )),
            Vec::new(),
        ),
        written_paths: Vec::new(),
        is_error: false,
    }));
    for line in view.lines(20) {
        assert_eq!(
            line_columns(&line),
            20,
            "a row overran the 20-column floor: {line:?}"
        );
    }
    let rendered = draw(&mut view, 20, 10);
    assert_eq!(rendered.len(), 10);
}

// ---------------------------------------------------------------------------
// The eyeball probe: one realistic transcript, printed
// ---------------------------------------------------------------------------

/// Build the transcript every visual assertion below reads: a user prompt, an
/// assistant reply with markdown, three tool calls (one long-output, one carrying a
/// unified diff), and a reasoning block.
fn realistic() -> TranscriptView {
    let mut view = view();
    view.transcript_mut()
        .push(Message::user("帮我把 diff viewer 接上文件树"));
    let long = (1..=12)
        .map(|n| format!("crates/zuno-tui/src/views/file{n}.rs"))
        .collect::<Vec<_>>()
        .join("\n");
    let patch = "@@ -1,3 +1,4 @@\n fn main() {\n-    old();\n+    new();\n+    extra();\n }\n";
    for event in [
        started(),
        provider(StreamEvent::ReasoningStart),
        provider(StreamEvent::ReasoningDelta(String::from(
            "## Plan\nread the parser, then wire the tree",
        ))),
        provider(StreamEvent::ReasoningDone {
            duration_secs: 12.0,
        }),
        provider(StreamEvent::TextDelta(String::from(
            "## Approach\n\nI will do **two** things:\n\n- read `diff.rs`\n- wire the tree\n",
        ))),
        provider(StreamEvent::ToolUseStart {
            id: String::from("c1"),
            name: String::from("read"),
        }),
        provider(StreamEvent::ToolInputDelta {
            id: String::from("c1"),
            delta: String::from(
                r#"{"filePath":"crates/zuno-tui/src/views/diff.rs","offset":1,"limit":162}"#,
            ),
        }),
        provider(StreamEvent::ToolUseEnd {
            id: String::from("c1"),
        }),
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("c1"),
            display_name: String::from("read"),
            name: String::from("read"),
            title: String::from("Read diff.rs"),
            output: String::from("pub fn parse(patch: &str) -> Vec<DiffLine> {"),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        },
        provider(StreamEvent::ToolUseStart {
            id: String::from("c2"),
            name: String::from("glob"),
        }),
        provider(StreamEvent::ToolInputDelta {
            id: String::from("c2"),
            delta: String::from(r#"{"pattern":"**/*.rs","path":"crates/zuno-tui/src/views"}"#),
        }),
        provider(StreamEvent::ToolUseEnd {
            id: String::from("c2"),
        }),
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("c2"),
            display_name: String::from("glob"),
            name: String::from("glob"),
            title: String::from("Find files"),
            output: long,
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        },
        provider(StreamEvent::ToolUseStart {
            id: String::from("c3"),
            name: String::from("edit"),
        }),
        provider(StreamEvent::ToolInputDelta {
            id: String::from("c3"),
            delta: String::from(
                r#"{"filePath":"crates/zuno-tui/src/views/session.rs","oldString":"old();","newString":"new();"}"#,
            ),
        }),
        provider(StreamEvent::ToolUseEnd {
            id: String::from("c3"),
        }),
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("c3"),
            display_name: String::from("edit"),
            name: String::from("edit"),
            title: String::from("Edit session.rs"),
            output: String::from("applied 1 change"),
            diff: zuno_engine::r#loop::ToolDiff::new(Some(String::from(patch)), Vec::new()),
            written_paths: vec![String::from("crates/zuno-tui/src/views/session.rs")],
            is_error: false,
        },
        provider(StreamEvent::TextDelta(String::from(
            "\nThe tree is wired now.",
        ))),
    ] {
        view.transcript_mut().observe(&event);
    }
    view
}

#[test]
#[ignore = "printer, not an assertion: run with --ignored --nocapture to eyeball the rendering"]
fn views_transcript_visual_probe() {
    for width in [80u16, 60, 40] {
        println!("\n=========== width {width} ===========");
        for line in realistic().lines(width) {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            println!("|{}|", text.trim_end());
        }
    }
}

// ---------------------------------------------------------------------------
// The per-message row cache (plan §11.3 R2-R5)
// ---------------------------------------------------------------------------

/// The text of every row, which is what a reader sees and what a cache must preserve.
fn row_text(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

/// The text *and* the style of every span, which is the whole of a rendered row.
///
/// Text alone would let a cache serve a correctly worded row in the wrong colour, which
/// is exactly the failure a theme change under a live cache produces.
fn row_spans(lines: &[Line<'static>]) -> Vec<Vec<(String, Style)>> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| (span.content.to_string(), span.style))
                .collect()
        })
        .collect()
}

/// Every transcript shape the cache has to be right about.
fn cache_subjects() -> Vec<(&'static str, TranscriptView)> {
    let mut empty = view();
    empty
        .transcript_mut()
        .set_awaiting_user(Some(AwaitingUser::Approval));

    let mut running = view();
    for event in [
        started(),
        provider(StreamEvent::ToolUseStart {
            id: String::from("r1"),
            name: String::from("shell"),
        }),
        TurnEvent::ToolDispatchStarted {
            step: 1,
            call_id: String::from("r1"),
            display_name: String::from("shell"),
            name: String::from("shell"),
            ui_intent: zuno_tool::ToolUiIntent::Generic,
        },
    ] {
        running.transcript_mut().observe(&event);
    }

    let mut diagnostics = view();
    diagnostics
        .transcript_mut()
        .push(Message::diagnostics(crate::views::lsp::Report::unchecked(
            "src/main.rs",
        )));
    diagnostics
        .transcript_mut()
        .push(Message::notice(String::from("warning: something")));

    let mut same_role = view();
    for index in 0..4 {
        same_role
            .transcript_mut()
            .push(Message::user(format!("prompt {index}")));
    }

    let mut retried = view();
    for event in [
        started(),
        provider(StreamEvent::TextDelta(String::from("discarded"))),
        provider(StreamEvent::RetryRollback { attempt: 2, max: 3 }),
        provider(StreamEvent::TextDelta(String::from("kept"))),
    ] {
        retried.transcript_mut().observe(&event);
    }

    vec![
        ("empty transcript awaiting permission", empty),
        ("a running tool call", running),
        ("diagnostics and a notice", diagnostics),
        ("four consecutive user prompts", same_role),
        ("a retried turn", retried),
        ("the realistic multi-part turn", realistic()),
    ]
}

/// The load-bearing test: the cache changes nothing about what is drawn.
///
/// `lines` is the specification and `cached_lines` the implementation, so this renders
/// every subject at every width both ways and requires span-for-span, style-for-style
/// equality. A cache that alters output is not a faster renderer, it is a wrong one.
#[test]
fn views_transcript_cache_returns_what_the_uncached_path_would() {
    for (name, mut subject) in cache_subjects() {
        for pass in 0..3 {
            for width in [20_u16, 40, 80, 120] {
                let expected = row_spans(&subject.lines(width));
                let actual = row_spans(&subject.cached_lines_for_test(width));
                assert_eq!(
                    actual, expected,
                    "{name}: pass {pass} at {width} columns rendered differently through the \
                     cache than through the uncached path"
                );
            }
        }
        // And once more with both affordances flipped, which changes how many rows a
        // reasoning block and a tool result produce.
        subject.toggle_thinking();
        subject.toggle_tool_output();
        for width in [20_u16, 80] {
            let expected = row_spans(&subject.lines(width));
            let actual = row_spans(&subject.cached_lines_for_test(width));
            assert_eq!(
                actual, expected,
                "{name}: with both affordances expanded, the cache disagreed with the \
                 uncached path at {width} columns"
            );
        }
    }
}

#[test]
fn views_transcript_cache_recalls_an_unchanged_frame_instead_of_re_rendering_it() {
    let mut view = realistic();
    let first = row_text(&view.cached_lines_for_test(80));
    let (hits_after_first, misses_after_first) = view.cache().counts();
    assert_eq!(
        hits_after_first, 0,
        "the first frame reported a cache hit, so it recalled rows nothing had stored"
    );
    assert!(
        misses_after_first > 0,
        "the first frame consulted the cache for no message at all"
    );
    assert!(
        view.cache().stored_entries() > 0,
        "the first frame stored nothing, so a second frame cannot be cheaper"
    );

    let second = row_text(&view.cached_lines_for_test(80));
    let (hits, _) = view.cache().counts();
    assert_eq!(
        second, first,
        "an unchanged transcript rendered differently"
    );
    assert_eq!(
        hits, misses_after_first,
        "the second frame recalled {hits} of {misses_after_first} messages; an unchanged \
         transcript must recall every one it stored"
    );
}

#[test]
fn views_transcript_cache_misses_when_the_width_changes() {
    let mut view = realistic();
    view.cached_lines_for_test(80);
    let (_, before) = view.cache().counts();
    view.cached_lines_for_test(80);
    let (hits, _) = view.cache().counts();
    assert!(hits > 0, "the fixture never hit at a stable width");

    let narrow = row_text(&view.cached_lines_for_test(40));
    let (hits_after, _) = view.cache().counts();
    assert_eq!(
        hits_after, hits,
        "a width change was served from the cache, so rows laid out for 80 columns were \
         drawn into 40"
    );
    assert_eq!(
        narrow,
        row_text(&view.lines(40)),
        "the re-rendered frame does not match the uncached path"
    );
    assert!(before > 0);
}

#[test]
fn views_transcript_cache_misses_when_the_theme_changes() {
    // `ViewContext` shares one `Arc<RwLock<Arc<Resolved>>>`, so a live re-theme happens
    // underneath an already-populated cache. This is the case a palette-hash key would
    // have got wrong for `thinkingOpacity`, which `Palette::entries` does not report.
    let context = ViewContext::defaults();
    let mut view = TranscriptView::new(context.clone());
    view.transcript_mut().push(Message::user("hello"));
    let dark = row_spans(&view.cached_lines_for_test(60));
    view.cached_lines_for_test(60);
    let (hits_before, _) = view.cache().counts();
    assert!(hits_before > 0, "the fixture never hit before the re-theme");

    let registry = crate::theme::ThemeRegistry::new();
    let light = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Light);
    context.set_theme(&light);

    let repainted = row_spans(&view.cached_lines_for_test(60));
    let (hits_after, _) = view.cache().counts();
    assert_eq!(
        hits_after, hits_before,
        "a re-theme was served from the cache, so the transcript kept painting the old \
         palette while every other surface had switched"
    );
    assert_ne!(
        repainted, dark,
        "the two themes produced identical styles, so this cannot detect a stale palette"
    );
    assert_eq!(
        repainted,
        row_spans(&view.lines(60)),
        "after a re-theme the cached path disagrees with the uncached one"
    );
}

#[test]
fn views_transcript_cache_misses_when_a_display_affordance_changes() {
    for (name, toggle) in [("thinking", 0_u8), ("tool output", 1)] {
        let mut view = realistic();
        view.cached_lines_for_test(80);
        view.cached_lines_for_test(80);
        let (hits_before, _) = view.cache().counts();
        assert!(
            hits_before > 0,
            "{name}: the fixture never hit before the toggle"
        );

        if toggle == 0 {
            view.toggle_thinking();
        } else {
            view.toggle_tool_output();
        }
        let after = row_spans(&view.cached_lines_for_test(80));
        let (hits_after, _) = view.cache().counts();
        assert_eq!(
            hits_after, hits_before,
            "{name}: the affordance changed and the cache still served the old rows"
        );
        assert_eq!(
            after,
            row_spans(&view.lines(80)),
            "{name}: after the toggle the cached path disagrees with the uncached one"
        );
    }
}

/// A streaming append re-renders the tail and recalls the prefix.
///
/// This is the plan's O(n²) stated as a test: the work a delta forces must be
/// proportional to what changed, not to the transcript's length.
#[test]
fn views_transcript_cache_recalls_the_prefix_across_a_streaming_append() {
    let mut view = view();
    for index in 0..40 {
        view.transcript_mut()
            .push(Message::user(format!("prompt {index}")));
    }
    view.cached_lines_for_test(80);
    view.cached_lines_for_test(80);
    let (hits_before, _) = view.cache().counts();
    let stored = view.cache().stored_entries();
    assert_eq!(stored, 40, "the fixture stored {stored} of 40 messages");

    for event in [
        started(),
        provider(StreamEvent::TextDelta(String::from("partial"))),
    ] {
        view.transcript_mut().observe(&event);
    }
    let (_, misses_before) = view.cache().counts();
    let grown = row_text(&view.cached_lines_for_test(80));
    let (hits_after, misses_after) = view.cache().counts();
    assert_eq!(
        hits_after - hits_before,
        40,
        "the 40 unchanged messages were not all recalled, so a delta still costs the \
         whole transcript"
    );
    assert_eq!(
        misses_after - misses_before,
        1,
        "more than the appended message was re-rendered"
    );
    assert_eq!(
        grown,
        row_text(&view.lines(80)),
        "the incrementally built frame disagrees with a full render"
    );

    // A second delta into the same message must still re-render only that message.
    view.transcript_mut()
        .observe(&provider(StreamEvent::TextDelta(String::from(" more"))));
    let (hits_two, misses_two) = view.cache().counts();
    let again = row_text(&view.cached_lines_for_test(80));
    let (hits_three, misses_three) = view.cache().counts();
    // Content first: a message mutated in place and served from its own stale entry is
    // the failure that reaches a user, and it must be what fails rather than a count.
    assert!(
        again.iter().any(|row| row.contains("partial more")),
        "the second delta is missing, so the tail was served from a stale entry: {again:?}"
    );
    assert_eq!(
        again,
        row_text(&view.lines(80)),
        "after a second delta the cached path disagrees with the uncached one"
    );
    assert_eq!(
        hits_three - hits_two,
        40,
        "the second delta recalled {} messages rather than the 40 unchanged ones",
        hits_three - hits_two
    );
    assert_eq!(
        misses_three - misses_two,
        1,
        "the second delta re-rendered {} messages rather than only the one it changed",
        misses_three - misses_two
    );
}

#[test]
fn views_transcript_cache_never_recalls_a_row_carrying_the_spinner() {
    // A running call renders `Transcript::spinner()`, which advances on the app's
    // independent animation clock. Recalling it would freeze the animation and, worse,
    // claim a frame is current when its liveness signal is stale.
    let mut view = view();
    view.transcript_mut().push(Message::user("go"));
    for event in [
        TurnEvent::TurnStarted {
            session_id: String::from("s"),
        },
        started(),
        provider(StreamEvent::ToolUseStart {
            id: String::from("c"),
            name: String::from("shell"),
        }),
        TurnEvent::ToolDispatchStarted {
            step: 1,
            call_id: String::from("c"),
            display_name: String::from("shell"),
            name: String::from("shell"),
            ui_intent: zuno_tool::ToolUiIntent::Generic,
        },
    ] {
        view.transcript_mut().observe(&event);
    }
    view.cached_lines_for_test(80);

    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..SPINNER.len() {
        assert!(
            view.handle_event(&AppEvent::AnimationFrame).redraw,
            "a running transcript ignored the animation clock"
        );
        let frame = row_text(&view.cached_lines_for_test(80));
        let glyph = frame
            .iter()
            .find(|row| row.contains(" $ "))
            .and_then(|row| {
                SPINNER
                    .iter()
                    .find(|frame| row.contains(**frame))
                    .map(|frame| (*frame).to_owned())
            });
        if let Some(glyph) = glyph {
            seen.insert(glyph);
        }
        assert_eq!(
            frame,
            row_text(&view.lines(80)),
            "a spinner frame served from the cache disagreed with a fresh render"
        );
    }
    assert!(
        seen.len() > 1,
        "the running call's glyph never changed across {} animation frames, so its row was \
         recalled: {seen:?}",
        SPINNER.len()
    );
    // Asserted after the animation, so the user-visible property is what fails first if
    // the exclusion is removed. Only the settled user prompt may be stored.
    assert_eq!(
        view.cache().stored_entries(),
        1,
        "the message with the running call was stored, or the settled one was not"
    );
}

#[test]
fn views_transcript_animation_clock_stops_at_turn_boundaries_and_human_prompts() {
    let mut view = view();
    view.transcript_mut().observe(&TurnEvent::TurnStarted {
        session_id: String::from("s"),
    });
    view.transcript_mut().observe(&started());
    let initial = view.transcript().spinner();

    assert!(view.handle_event(&AppEvent::AnimationFrame).redraw);
    assert_ne!(
        view.transcript().spinner(),
        initial,
        "an animation frame did not advance the liveness glyph"
    );

    view.transcript_mut()
        .set_awaiting_user(Some(AwaitingUser::Answer));
    let waiting = view.transcript().spinner();
    assert!(
        !view.handle_event(&AppEvent::AnimationFrame).redraw,
        "a prompt waiting on the user requested a pointless animation redraw"
    );
    assert_eq!(view.transcript().spinner(), waiting);

    view.transcript_mut().set_awaiting_user(None);
    view.transcript_mut().observe(&TurnEvent::TurnCompleted {
        assistant_message_id: String::from("m"),
        steps: 1,
    });
    let completed = view.transcript().spinner();
    assert!(
        !view.handle_event(&AppEvent::AnimationFrame).redraw,
        "a completed turn kept the animation clock alive"
    );
    assert_eq!(view.transcript().spinner(), completed);
}

#[test]
fn views_transcript_cache_stays_inside_its_row_bound() {
    // Overrun the budget with *tall* messages rather than many, because that is the hazard the
    // bound is expressed in rows to cover: one message can outweigh thousands, and an
    // entry-count bound would have admitted all of them.
    //
    // A user prompt is the vehicle, and it used to be a notice. That is worth recording rather
    // than quietly editing: this fixture read `Message::notice` precisely because notices then
    // wrapped with no row cap, and [`NOTICE_MAX_ROWS`] has since given them one — a 512-line
    // notice now renders as five rows and the fixture stopped reaching the bound at all, which
    // is how the change was noticed here. `MessagePart::Text` on a user message is still
    // uncapped `wrap`, so it carries the same hazard the bound exists for.
    let mut view = view();
    let rows_each = 512;
    let messages = MAX_CACHED_ROWS / rows_each + 8;
    for index in 0..messages {
        let body = (0..rows_each)
            .map(|row| format!("- line {row} of prompt {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.transcript_mut().push(Message::user(body));
    }
    let frame = view.cached_lines_for_test(60);
    assert!(
        frame.len() > MAX_CACHED_ROWS,
        "the fixture produced {} rows, which does not exceed the {MAX_CACHED_ROWS}-row \
         bound, so nothing was evicted",
        frame.len()
    );
    assert!(
        view.cache().stored_rows() <= MAX_CACHED_ROWS,
        "the cache holds {} rows against a {MAX_CACHED_ROWS}-row bound",
        view.cache().stored_rows()
    );
    // Rendering again must not grow it either: an eviction policy that re-admitted
    // everything each frame would satisfy the bound only between frames.
    view.cached_lines_for_test(60);
    assert!(
        view.cache().stored_rows() <= MAX_CACHED_ROWS,
        "a second frame pushed the cache to {} rows",
        view.cache().stored_rows()
    );
    assert_eq!(
        row_text(&view.cached_lines_for_test(60)),
        row_text(&view.lines(60)),
        "an evicting cache stopped agreeing with the uncached path"
    );
}

#[test]
fn views_transcript_cache_forgets_a_message_that_no_longer_exists() {
    // A replaced transcript is shorter than the cache. Without the truncation those
    // slots would hold their rows against the bound for the rest of the process.
    let mut view = view();
    for index in 0..30 {
        view.transcript_mut()
            .push(Message::user(format!("prompt {index}")));
    }
    view.cached_lines_for_test(60);
    assert_eq!(view.cache().stored_entries(), 30);
    let held = view.cache().stored_rows();
    assert!(held > 0);

    *view.transcript_mut() = Transcript::new();
    view.transcript_mut().push(Message::user("fresh"));
    view.cached_lines_for_test(60);
    assert_eq!(
        view.cache().stored_entries(),
        1,
        "the cache kept entries for messages the transcript no longer has"
    );
    assert!(
        view.cache().stored_rows() < held,
        "the discarded messages' rows are still counted against the bound"
    );
}

/// Two different messages must not share a fingerprint.
///
/// The cache is keyed on it, so a collision between two shapes a transcript actually
/// produces would draw one message's rows in another's place.
#[test]
fn views_transcript_fingerprint_separates_every_part_shape() {
    let report = crate::views::lsp::Report::unchecked("src/main.rs");
    let shapes: Vec<(&str, Message)> = vec![
        ("text", Message::user("same")),
        (
            "notice with the same string",
            Message::notice(String::from("same")),
        ),
        ("diagnostics", Message::diagnostics(report)),
        (
            "reasoning, streaming",
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![MessagePart::Reasoning {
                    text: String::from("same"),
                    duration_secs: None,
                    streaming: true,
                }],
            },
        ),
        (
            "reasoning, settled",
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![MessagePart::Reasoning {
                    text: String::from("same"),
                    duration_secs: None,
                    streaming: false,
                }],
            },
        ),
        (
            "reasoning, timed",
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![MessagePart::Reasoning {
                    text: String::from("same"),
                    duration_secs: Some(1.0),
                    streaming: false,
                }],
            },
        ),
        (
            "attachment",
            Message {
                role: Role::User,
                id: None,
                parts: vec![MessagePart::Attachment {
                    filename: String::from("same"),
                    mime: None,
                }],
            },
        ),
        (
            "retry",
            Message {
                role: Role::System,
                id: None,
                parts: vec![MessagePart::Retry { attempt: 2, max: 3 }],
            },
        ),
        (
            "retry, different count",
            Message {
                role: Role::System,
                id: None,
                parts: vec![MessagePart::Retry { attempt: 1, max: 3 }],
            },
        ),
        (
            "tool, pending",
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![MessagePart::Tool {
                    call_id: String::from("c"),
                    display_name: String::from("shell"),
                    name: String::from("shell"),
                    ui_intent: zuno_tool::ToolUiIntent::Generic,
                    arguments: String::new(),
                    title: None,
                    status: ToolStatus::Pending,
                    output: None,
                    diff: None,
                }],
            },
        ),
        (
            "tool, completed",
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![MessagePart::Tool {
                    call_id: String::from("c"),
                    display_name: String::from("shell"),
                    name: String::from("shell"),
                    ui_intent: zuno_tool::ToolUiIntent::Generic,
                    arguments: String::new(),
                    title: None,
                    status: ToolStatus::Completed,
                    output: None,
                    diff: None,
                }],
            },
        ),
        (
            "tool, completed with output",
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![MessagePart::Tool {
                    call_id: String::from("c"),
                    display_name: String::from("shell"),
                    name: String::from("shell"),
                    ui_intent: zuno_tool::ToolUiIntent::Generic,
                    arguments: String::new(),
                    title: None,
                    status: ToolStatus::Completed,
                    output: Some(String::new()),
                    diff: None,
                }],
            },
        ),
    ];
    let mut seen: std::collections::BTreeMap<u64, &str> = std::collections::BTreeMap::new();
    for (name, message) in &shapes {
        let print = fingerprint(message);
        if let Some(other) = seen.insert(print, name) {
            panic!("`{name}` and `{other}` share fingerprint {print}");
        }
        assert_eq!(
            print,
            fingerprint(message),
            "`{name}` fingerprinted differently on a second call, so the key is unstable"
        );
    }
    // The role is part of the key: the same parts from a different speaker render a
    // different header and a different rule.
    let mut assistant = Message::user("same");
    assistant.role = Role::Assistant;
    assert_ne!(
        fingerprint(&Message::user("same")),
        fingerprint(&assistant),
        "two roles carrying the same text fingerprint alike, so a cached user row could \
         be served for an assistant message"
    );
}
