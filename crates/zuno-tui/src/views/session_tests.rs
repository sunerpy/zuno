//! What a host needs to trust about the composed screen.

use super::*;
use crate::app::{TerminalEvent, render_offscreen, terminal_event_channel};
use crate::keybind::{KeyDispatcher, Keymap};
use crate::views::dialog::DialogHost;
use crate::views::editor::Position;
use crate::views::message::{Role, USER_BOX_RIGHT, USER_BOX_RULE};
use crate::views::testkit::{action, press, rows};
use crate::views::toast::ToastLevel;
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::StreamEvent;

fn screen() -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    screen_with(ViewContext::defaults())
}

/// A screen whose tests explicitly exercise application mouse handling.
fn mouse_screen() -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);
    let context = ViewContext::new(
        &resolved,
        crate::config::ResolvedTuiConfig {
            mouse: true,
            ..crate::config::ResolvedTuiConfig::default()
        },
    );
    screen_with(context)
}

fn screen_with(context: ViewContext) -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = terminal_event_channel();
    (SessionScreen::new(context, sender), receiver)
}

fn press_none() -> KeyEvent {
    crate::views::testkit::press(crossterm::event::KeyCode::Null)
}

#[test]
fn session_screen_renders_the_transcript_reply_identity_and_prompt() {
    let (mut screen, _shutdown) = screen();
    screen.status_mut().describe("build", "test/test-model");
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("earlier prompt"));
    screen.editor.set_text("what I am typing");

    let rendered = rows(&render_offscreen(&mut screen, 40, 8).expect("infallible"));
    let joined = rendered.join("\n");
    assert!(
        joined.contains("earlier prompt"),
        "the transcript region is empty:\n{joined}"
    );
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("▣ build · test-model")),
        "the reply identity did not render in its own region: {rendered:?}"
    );
    assert!(
        content_row(&rendered, &screen, 40, 8).contains("what I am typing"),
        "the prompt did not render in its own region: {rendered:?}"
    );
}

#[test]
fn session_screen_folds_an_engine_event_into_the_reply_identity() {
    let (mut screen, _shutdown) = screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("start a reply"));
    screen.handle_event(&AppEvent::Engine(TurnEvent::ModelResolved {
        step: 1,
        provider_id: String::from("test"),
        model_id: String::from("test-model"),
    }));

    let rendered = rows(&render_offscreen(&mut screen, 40, 8).expect("infallible"));
    assert!(
        rendered.iter().any(|row| row.contains("▣ test-model")),
        "the resolved model did not reach the reply identity: {rendered:?}"
    );
}

#[test]
fn session_screen_renders_provider_text_as_it_streams() {
    // Incrementality asserted as growth of the accumulated text, not as a frame
    // diff: a frame also carries the status strip and the prompt, which change for
    // reasons that have nothing to do with the message.
    let (mut screen, _shutdown) = screen();
    screen.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: String::from("m"),
    }));
    let mut accumulated = String::new();
    for delta in ["Hel", "lo ", "there"] {
        screen.handle_event(&AppEvent::Engine(TurnEvent::Provider {
            step: 1,
            event: StreamEvent::TextDelta(delta.to_owned()),
        }));
        accumulated.push_str(delta);
        let joined = rows(&render_offscreen(&mut screen, 40, 8).expect("infallible")).join("\n");
        assert!(
            joined.contains(accumulated.trim_end()),
            "the frame does not show every delta received so far \
             ({accumulated:?}):\n{joined}"
        );
    }
}

#[test]
fn session_screen_renders_a_file_reference_refusal_in_the_transcript() {
    let (mut screen, _shutdown) = screen();
    screen.handle_event(&AppEvent::Engine(TurnEvent::Provider {
        step: 0,
        event: StreamEvent::Error {
            message: "file reference `@src/missing.rs` not found".to_owned(),
            retry_after: None,
        },
    }));

    let rendered = rows(&render_offscreen(&mut screen, 80, 10).expect("infallible")).join("\n");
    assert!(rendered.contains("@src/missing.rs"), "{rendered}");
    assert!(rendered.contains("not found"), "{rendered}");
}

#[test]
fn session_screen_types_a_printable_key_into_the_prompt() {
    // The keymap claims no binding for a bare letter, so the screen is the only
    // place that can put one in the buffer. A screen that forwarded only engine
    // events would render a prompt nobody can type into.
    let (mut screen, _shutdown) = screen();
    for character in "hi".chars() {
        screen.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(crate::views::testkit::press(
                crossterm::event::KeyCode::Char(character),
            )),
        )));
    }

    let rendered = rows(&render_offscreen(&mut screen, 40, 8).expect("infallible"));
    assert!(
        content_row(&rendered, &screen, 40, 8).contains("hi"),
        "the typed characters did not reach the prompt: {rendered:?}"
    );
}

#[test]
fn session_screen_submitting_moves_the_prompt_into_the_transcript() {
    let (mut screen, _shutdown) = screen();
    screen.editor.set_text("send this");
    let result = screen.handle_action(action("input_submit"), &press_none());

    assert!(result.redraw);
    assert_eq!(screen.submissions(), ["send this"]);
    let joined = rows(&render_offscreen(&mut screen, 40, 8).expect("infallible")).join("\n");
    assert!(
        joined.contains("send this"),
        "the submitted text is not in the transcript:\n{joined}"
    );
}

#[test]
fn session_screen_a_submission_during_work_is_queued_for_the_next_turn() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(2);
    let (mutations, _mutation_source) = mpsc::channel(2);
    let projection = crate::views::picker::QueuedInputProjection::default();
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_queued_inputs(projection.clone(), mutations);
    screen.status.mark_running();

    screen.submit_prompt("change direction");

    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Queue(Box::new(PromptSubmission::Text(
            String::from("change direction")
        )))),
        "the active turn caused the follow-up to be refused instead of admitted"
    );
    assert!(
        ActionComponent::drain_toasts(&mut screen).is_empty(),
        "the screen claimed queue admission before SQLite committed"
    );
    projection.publish(
        vec![crate::views::picker::QueuedInputEntry {
            id: String::from("msg_queued"),
            text: String::from("change direction"),
            delivery: crate::views::picker::QueuedInputDelivery::Queue,
            revision: 1,
            editable: true,
        }],
        Some(crate::views::picker::QueuedInputNotice {
            input_id: String::from("msg_queued"),
            kind: crate::views::picker::QueuedInputNoticeKind::Admitted(
                crate::views::picker::QueuedInputDelivery::Queue,
            ),
        }),
    );
    screen.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    let notices = ActionComponent::drain_toasts(&mut screen);
    assert!(
        notices
            .iter()
            .any(|toast| toast.text().contains("next turn")),
        "the committed queue input was not acknowledged: {notices:?}"
    );
    let rendered = rows(&render_offscreen(&mut screen, 72, 10).expect("infallible")).join("\n");
    assert!(
        !rendered.contains("not sent: a turn is already running"),
        "the old channel-capacity refusal remained visible:\n{rendered}"
    );
}

#[test]
fn session_screen_ctrl_enter_marks_a_busy_submission_as_an_explicit_steer() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(2);
    let (mutations, _mutation_source) = mpsc::channel(2);
    let projection = crate::views::picker::QueuedInputProjection::default();
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_queued_inputs(projection.clone(), mutations);
    screen.status.mark_running();
    screen.editor.set_text("change direction now");

    screen.handle_action(action("input_force_submit"), &press_none());

    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Steer(Box::new(PromptSubmission::Text(
            String::from("change direction now")
        ))))
    );
    assert!(
        ActionComponent::drain_toasts(&mut screen).is_empty(),
        "the screen claimed steer admission before SQLite committed"
    );
    projection.publish(
        vec![crate::views::picker::QueuedInputEntry {
            id: String::from("msg_steer"),
            text: String::from("change direction now"),
            delivery: crate::views::picker::QueuedInputDelivery::Steer,
            revision: 1,
            editable: true,
        }],
        Some(crate::views::picker::QueuedInputNotice {
            input_id: String::from("msg_steer"),
            kind: crate::views::picker::QueuedInputNoticeKind::Admitted(
                crate::views::picker::QueuedInputDelivery::Steer,
            ),
        }),
    );
    screen.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    let notices = ActionComponent::drain_toasts(&mut screen);
    assert!(
        notices
            .iter()
            .any(|toast| toast.text().contains("next safe point")),
        "the committed steer was not acknowledged: {notices:?}"
    );
}

// ---------------------------------------------------------------------------
// Bracketed paste
// ---------------------------------------------------------------------------

/// The event a terminal in bracketed-paste mode delivers for one paste.
fn paste(text: &str) -> AppEvent {
    AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Paste(text.to_owned())))
}

#[test]
fn session_screen_a_multi_line_paste_inserts_every_line_and_submits_no_turn() {
    // The exact real-terminal failure this feature exists to fix. Without bracketed
    // paste the same eight lines arrived as individual keys, every newline resolved to
    // `input_submit`, and the transcript filled with `not sent: a turn is already
    // running` — eight turns from one paste.
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);

    let pasted = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight";
    let result = screen.handle_event(&paste(pasted));

    assert!(result.redraw);
    assert_eq!(
        screen.editor.height(),
        8,
        "an eight-line paste did not become eight prompt lines: {:?}",
        screen.editor.text()
    );
    assert_eq!(screen.editor.text(), pasted);
    assert!(
        submitted.try_recv().is_err(),
        "a paste started a turn; nothing should have been submitted"
    );
    assert!(
        screen.submissions().is_empty(),
        "a paste was recorded as a submission: {:?}",
        screen.submissions()
    );
}

#[test]
fn session_screen_a_large_paste_shows_a_summary_but_submits_the_whole_text() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);

    let lines: Vec<String> = (0..crate::views::editor::PASTE_SUMMARY_LINES + 4)
        .map(|index| format!("line {index}"))
        .collect();
    let pasted = lines.join("\n");
    screen.handle_event(&paste(&pasted));

    let rendered = rows(&render_offscreen(&mut screen, 40, 8).expect("infallible"));
    assert!(
        content_row(&rendered, &screen, 40, 8).contains("Pasted"),
        "the prompt band does not show the summary affordance: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|row| row.contains("line 12")),
        "the prompt band was flooded with the paste instead of summarising it: {rendered:?}"
    );

    screen.handle_action(action("input_submit"), &press_none());
    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Text(pasted.clone())),
        "the summary was sent to the model instead of the pasted text"
    );
    assert_eq!(
        screen.submissions(),
        [pasted],
        "the transcript recorded the summary rather than what was sent"
    );
}

#[test]
fn session_screen_a_pasted_path_is_not_taken_for_a_slash_command() {
    // `/etc/hosts` resolves to `unknown command /etc`, which refuses the prompt *after*
    // the editor has cleared it — so an unescaped pasted path is a discarded paste.
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);

    screen.handle_event(&paste("/etc/hosts"));
    screen.handle_action(action("input_submit"), &press_none());

    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Text(String::from("/etc/hosts"))),
        "a pasted absolute path did not reach the model as literal text"
    );
    let joined = rows(&render_offscreen(&mut screen, 60, 10).expect("infallible")).join("\n");
    assert!(
        !joined.contains("unknown command"),
        "a pasted path was routed as a command:\n{joined}"
    );
}

#[test]
fn session_screen_refuses_a_paste_while_a_modal_owns_the_keyboard() {
    // `DialogHost` forwards every non-key event to the base unconditionally — that is
    // what keeps an open dialog from stalling the loop — so a paste would otherwise land
    // in the prompt hidden behind the dialog.
    let (screen, _shutdown) = screen();
    let mut host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    host.open(permission_prompt());
    // The base learns which dialog is up from `observe_modal`, which the host derives
    // per frame rather than pushing on change.
    let _frame = render_offscreen(&mut host, 60, 12).expect("infallible");

    host.handle_event(&paste("pasted\nwhile\nmodal"));

    let joined = rows(&render_offscreen(&mut host, 60, 12).expect("infallible")).join("\n");
    assert!(
        !joined.contains("pasted"),
        "pasted text reached the prompt behind an open dialog:\n{joined}"
    );
}

#[test]
fn session_screen_the_paste_binding_reports_that_it_could_not_read_the_clipboard() {
    // `EditorSignal::Paste` used to fall into a bare redraw, so the binding did nothing
    // and said nothing. The host clipboard still refuses to read — see
    // `external::SystemClipboard::read` — and the point is that the refusal is now shown.
    let (mut screen, _shutdown) = screen();
    let result = screen.handle_action(action("input_paste"), &press_none());

    assert!(result.redraw);
    let joined = rows(&render_offscreen(&mut screen, 60, 12).expect("infallible")).join("\n");
    assert!(
        joined.contains("paste failed") || joined.contains("nothing to paste"),
        "the paste binding neither pasted nor said why:\n{joined}"
    );
}

#[test]
fn session_screen_the_paste_binding_inserts_what_a_readable_clipboard_holds() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let clipboard = Arc::new(crate::views::external::MemoryClipboard::holding(
        crate::views::external::ClipboardContent::text("from\nthe\nclipboard"),
    ));
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_clipboard(clipboard);

    screen.handle_action(action("input_paste"), &press_none());

    assert_eq!(screen.editor.text(), "from\nthe\nclipboard");
    assert!(
        submitted.try_recv().is_err(),
        "pasting from the clipboard started a turn"
    );
}

#[test]
fn session_screen_ui_slash_dispatches_without_reaching_the_prompt_sink() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_catalog(catalog());
    screen.editor.set_text("/models");

    let result = screen.handle_action(action("input_submit"), &press_none());

    assert!(result.redraw);
    assert!(
        submitted.try_recv().is_err(),
        "a UI action reached the model"
    );
    let dialogs = screen.drain_dialogs();
    assert_eq!(dialogs.len(), 1);
    assert_eq!(dialogs[0].id(), crate::views::picker::MODEL_DIALOG_ID);
}

#[test]
fn session_screen_catalog_slash_stays_typed_for_the_host() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_slash_commands([CatalogCommand::new(
            "review",
            Some("Review changes".to_owned()),
        )]);
    screen.editor.set_text("/review src/lib.rs carefully");

    screen.handle_action(action("input_submit"), &press_none());

    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Command {
            name: "review".to_owned(),
            arguments: "src/lib.rs carefully".to_owned(),
        })
    );
    assert_eq!(screen.submissions(), ["/review src/lib.rs carefully"]);
}

#[test]
fn session_screen_direct_skill_stays_typed_with_its_source() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_slash_commands([CatalogCommand::skill(
            "github-project-scaffold",
            Some("Prepare a public repository".to_owned()),
            "/skills/github-project-scaffold/SKILL.md",
        )]);
    screen
        .editor
        .set_text("/github-project-scaffold audit this repository");

    screen.handle_action(action("input_submit"), &press_none());

    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Skill {
            name: "github-project-scaffold".to_owned(),
            source: "/skills/github-project-scaffold/SKILL.md".to_owned(),
            arguments: "audit this repository".to_owned(),
        })
    );
}

#[test]
fn session_screen_plan_command_requires_confirmation_before_switching_agents() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(catalog());
    screen.editor.set_text("/plan");

    screen.handle_action(action("input_submit"), &press_none());

    assert!(
        chosen.try_recv().is_err(),
        "Plan mode changed before the user confirmed it"
    );
    let dialogs = screen.drain_dialogs();
    assert_eq!(dialogs.len(), 1);
    assert_eq!(dialogs[0].id(), PLAN_START_CONFIRM_DIALOG_ID);

    screen.apply_dialog_outcome(
        PLAN_START_CONFIRM_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: PLAN_START_CONFIRM_DIALOG_ID,
            value: crate::views::basics::CONFIRM_VALUE.to_owned(),
        },
    );

    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Agent(String::from("plan")))
    );
    assert_eq!(screen.catalog.agent.as_deref(), Some("plan"));
}

#[test]
fn session_screen_start_plan_switches_immediately_without_model_text() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(catalog());
    screen.editor.set_text("/start-plan");

    screen.handle_action(action("input_submit"), &press_none());

    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Agent(String::from("plan")))
    );
    assert_eq!(screen.catalog.agent.as_deref(), Some("plan"));
    assert!(
        screen.drain_dialogs().is_empty(),
        "the explicit start command unexpectedly asked twice"
    );
}

#[test]
fn session_screen_start_work_requires_a_durable_plan() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(1);
    let mut offered = catalog();
    offered.agent = Some(String::from("plan"));
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(offered);
    screen.editor.set_text("/start-work");

    screen.handle_action(action("input_submit"), &press_none());

    assert!(chosen.try_recv().is_err());
    assert!(screen.drain_dialogs().is_empty());
    assert!(
        ActionComponent::drain_toasts(&mut screen)
            .iter()
            .any(|toast| toast.text().contains("no durable plan is ready")),
        "the missing durable plan was not explained"
    );
}

#[test]
fn session_screen_start_work_confirms_the_durable_plan_before_orchestrated_work() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(1);
    let mut offered = catalog();
    offered.agent = Some(String::from("plan"));
    let work = crate::views::ambient::WorkState::new(zuno_types::WorkStateProjection {
        plan: Some(zuno_types::PlanProjection {
            id: String::from("plan_release"),
            goal_id: None,
            revision: 3,
            title: String::from("Release hardening"),
            steps: vec![zuno_types::PlanStepProjection {
                id: String::from("step_scan"),
                title: String::from("Inspect the repository"),
                status: String::from("pending"),
            }],
            span: zuno_types::ExecutionSpan::default(),
            time_created: 1,
            time_updated: 2,
        }),
        ..zuno_types::WorkStateProjection::default()
    });
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(offered)
        .with_work_state(work);
    screen.editor.set_text("/start-work");

    screen.handle_action(action("input_submit"), &press_none());

    assert!(
        chosen.try_recv().is_err(),
        "Work mode changed before the durable plan was confirmed"
    );
    let dialogs = screen.drain_dialogs();
    assert_eq!(dialogs.len(), 1);
    assert_eq!(dialogs[0].id(), WORK_START_CONFIRM_DIALOG_ID);

    screen.apply_dialog_outcome(
        WORK_START_CONFIRM_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: WORK_START_CONFIRM_DIALOG_ID,
            value: crate::views::basics::CONFIRM_VALUE.to_owned(),
        },
    );

    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Agent(String::from("orchestrator")))
    );
    assert_eq!(screen.catalog.agent.as_deref(), Some("orchestrator"));
}

#[test]
fn session_screen_redo_stays_typed_for_the_runtime_host_and_asks_nothing() {
    // `/redo` reapplies the boundary the user just left by confirming an undo, so it
    // restores a state they were shown and agreed to. Confirming it too would only teach
    // people to press through both prompts. `/undo` is the one that asks; see below.
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);
    screen.editor.set_text("/redo");

    screen.handle_action(action("input_submit"), &press_none());

    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Host(HostCommand::Redo))
    );
    assert_eq!(screen.submissions(), ["/redo"]);
    assert!(
        screen.drain_dialogs().is_empty(),
        "`/redo` opened a dialog it does not need"
    );
}

/// Type `text` into the hosted screen and submit it, the way a user would.
///
/// The dismissal in the middle is not ceremony: typing `/` opens the slash autocomplete,
/// and while it is open the screen remaps `input_submit` to
/// `prompt.autocomplete.select` — so a test that submitted straight away would be
/// picking from a list rather than sending the command, and would see no dialog at all.
fn type_and_submit(host: &mut DialogHost, text: &str) {
    for character in text.chars() {
        host.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(press(crossterm::event::KeyCode::Char(character))),
        )));
    }
    host.handle_action(action("session_interrupt"), &press_none());
    host.handle_action(action("input_submit"), &press_none());
}

/// A screen with a prompt sink, mounted under the host that opens its dialogs.
///
/// Through the host on purpose: the screen can only *ask* for a dialog
/// (`drain_dialogs`), so a test that inspected the request would prove the confirmation
/// was built and never that it can be answered — the "built, tested, impossible to open"
/// failure this project has removed four times.
fn hosted_screen() -> (DialogHost, mpsc::Receiver<PromptSubmission>) {
    let (sender, shutdown) = terminal_event_channel();
    // Leaked deliberately: dropping the receiver closes the shutdown channel, and the
    // screen reports a closed channel rather than the behaviour under test.
    std::mem::forget(shutdown);
    let (prompts, submitted) = mpsc::channel(1);
    let context = ViewContext::defaults();
    let screen = SessionScreen::new(context.clone(), sender).with_prompt_sink(prompts);
    (DialogHost::new(context, Box::new(screen)), submitted)
}

#[test]
fn session_screen_undo_asks_before_it_restores_the_worktree() {
    let (mut host, mut submitted) = hosted_screen();
    type_and_submit(&mut host, "/undo");

    assert_eq!(
        host.active(),
        Some(crate::views::session::UNDO_CONFIRM_DIALOG_ID),
        "`/undo` reached the driver with nothing asked"
    );
    assert!(
        submitted.try_recv().is_err(),
        "the worktree was restored before the user answered"
    );
    let shown = rows(&render_offscreen(&mut host, 70, 16).expect("infallible")).join("\n");
    assert!(
        shown.contains("cannot be recovered") && shown.contains("Restore"),
        "the confirmation did not say what it would do:\n{shown}"
    );

    host.handle_action(action("dialog.select.submit"), &press_none());
    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Host(HostCommand::Undo)),
        "confirming the undo did not restore anything"
    );
    assert!(
        !host.is_open(),
        "the confirmation stayed up after answering"
    );
}

#[test]
fn session_screen_cancelling_the_undo_confirmation_restores_nothing() {
    // The half that matters: a confirmation that ran the action either way is worse than
    // none, because it teaches the user their answer is not read.
    let (mut host, mut submitted) = hosted_screen();
    type_and_submit(&mut host, "/undo");
    host.handle_action(action("session_interrupt"), &press_none());

    assert!(!host.is_open());
    assert!(
        submitted.try_recv().is_err(),
        "cancelling the confirmation restored the worktree anyway"
    );
}

#[test]
fn session_screen_offers_a_prompt_dialog_when_there_is_no_external_editor() {
    // The action means "give me more room for this text". Answering it with one
    // transcript line saying no left the request unserved, which is what this replaces.
    let (mut host, _submitted) = hosted_screen();
    for character in "draft".chars() {
        host.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(press(crossterm::event::KeyCode::Char(character))),
        )));
    }
    host.handle_action(action("editor_open"), &press_none());
    assert_eq!(
        host.active(),
        Some(crate::views::session::EDITOR_FALLBACK_DIALOG_ID),
        "with no `$EDITOR` worker the request went nowhere"
    );
    let shown = rows(&render_offscreen(&mut host, 70, 16).expect("infallible")).join("\n");
    assert!(
        shown.contains("draft"),
        "the fallback did not open on what the user had already typed:\n{shown}"
    );

    for character in "-more".chars() {
        host.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(press(crossterm::event::KeyCode::Char(character))),
        )));
    }
    host.handle_action(action("dialog.prompt.submit"), &press_none());
    assert!(!host.is_open());
    let after = rows(&render_offscreen(&mut host, 70, 16).expect("infallible")).join("\n");
    assert!(
        after.contains("draft-more"),
        "the edited text did not land back in the prompt:\n{after}"
    );
}

#[test]
fn session_screen_cancelling_the_editor_fallback_leaves_the_prompt_untouched() {
    // `Ok(None)`'s behaviour on the real editor path, matched here: the two routes agree
    // on both outcomes and not only the successful one.
    let (mut host, _submitted) = hosted_screen();
    for character in "keep".chars() {
        host.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(press(crossterm::event::KeyCode::Char(character))),
        )));
    }
    host.handle_action(action("editor_open"), &press_none());
    host.handle_action(action("session_interrupt"), &press_none());
    let after = rows(&render_offscreen(&mut host, 70, 16).expect("infallible")).join("\n");
    assert!(
        after.contains("keep"),
        "cancelling the fallback cleared the prompt:\n{after}"
    );
}

#[test]
fn session_screen_a_failed_external_editor_opens_an_alert_that_waits_to_be_read() {
    // A toast would truncate a child's diagnostic into a corner for five seconds, and the
    // transcript scrolls it away behind whatever the turn prints next. The user has to
    // read this one to know whether their draft survived.
    let (sender, shutdown) = terminal_event_channel();
    std::mem::forget(shutdown);
    let (results, result_source) = mpsc::channel(1);
    let (requests, _request_source) = mpsc::channel(1);
    let context = ViewContext::defaults();
    let screen =
        SessionScreen::new(context.clone(), sender).with_external_editor(requests, result_source);
    let mut host = DialogHost::new(context, Box::new(screen));
    results
        .try_send(Err(crate::views::external::ExternalError::NoEditor))
        .expect("capacity 1");

    host.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    assert_eq!(
        host.active(),
        Some(crate::views::session::EDITOR_ALERT_DIALOG_ID),
        "the editor failure was reported without waiting to be read"
    );
    let shown = rows(&render_offscreen(&mut host, 70, 16).expect("infallible")).join("\n");
    assert!(
        shown.contains("The prompt is unchanged"),
        "the alert did not say what happened to the draft:\n{shown}"
    );

    host.handle_action(action("dialog.select.submit"), &press_none());
    assert!(!host.is_open(), "enter did not dismiss the alert");
}

#[test]
fn session_screen_copy_feedback_reaches_the_screen_and_survives_a_dialog_opening_over_it() {
    // The end-to-end property from the real caller, in the order it actually happens. A
    // modal owns the keyboard, so `messages_copy` cannot be pressed *while* a dialog is
    // up — the host absorbs it, which is the behaviour that keeps `session_new` from
    // firing behind a permission prompt. The reachable sequence is copy, then open
    // something within the toast's five seconds, and the notice has to stay readable: a
    // transcript line would be behind the modal, where the user cannot see the feedback
    // they are waiting for.
    let (sender, shutdown) = terminal_event_channel();
    std::mem::forget(shutdown);
    let context = ViewContext::defaults();
    let clipboard = Arc::new(crate::views::external::MemoryClipboard::default());
    let mut screen = SessionScreen::new(context.clone(), sender)
        .with_clipboard(Arc::clone(&clipboard) as Arc<dyn Clipboard>);
    for character in "abc".chars() {
        screen.editor.insert_char(character);
    }
    let mut host = DialogHost::new(context.clone(), Box::new(screen));

    host.handle_action(action("messages_copy"), &press_none());
    let copied = rows(&render_offscreen(&mut host, 70, 14).expect("infallible"));
    assert!(
        copied[0].contains("copied"),
        "the copy confirmation never reached the screen at all: {copied:?}"
    );

    host.open(Box::new(crate::views::basics::AlertDialog::new(
        context,
        "alert.test",
        "In the way",
        "body ".repeat(200),
    )));
    let after = rows(&render_offscreen(&mut host, 70, 14).expect("infallible"));
    assert!(
        after[1..]
            .iter()
            .any(|row| row.contains("In the way") || row.contains("body")),
        "the dialog did not open, so this proves nothing about layering: {after:?}"
    );
    assert!(
        after[0].contains("copied"),
        "the dialog opened over the copy confirmation and hid it: {after:?}"
    );
}

#[test]
fn session_screen_forwards_mcp_toggle_requests_without_waiting() {
    let (sender, _shutdown) = terminal_event_channel();
    let (toggles, mut requested) = mpsc::channel(1);
    let projection =
        crate::views::picker::McpProjection::new(vec![crate::views::picker::McpServer {
            name: "context7".to_owned(),
            state: crate::views::picker::McpState::Connected,
            desired_enabled: true,
        }]);
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), sender).with_mcp_control(projection, toggles);
    let request = crate::views::picker::McpToggleRequest {
        server: "context7".to_owned(),
        desired_enabled: false,
    };

    let result = screen.apply_dialog_outcome(
        crate::views::picker::MCP_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::McpToggle(request.clone()),
    );

    assert!(result.redraw);
    assert_eq!(requested.try_recv(), Ok(request));
}

#[test]
fn session_an_mcp_status_change_after_the_panel_opened_reaches_the_panel_and_the_sidebar() {
    // The defect, as reported from a real terminal: with the MCP list open, the panel row
    // read `◐ Connecting` while the sidebar had already moved on to
    // `✗ … out after 30s`. The two never actually disagreed — both re-read the same
    // projection while they draw — they shared one *stale frame*, because the lifecycle
    // worker's `TerminalEvent::Wake` reached every surface, was claimed by none, and so
    // painted nothing.
    //
    // Driven through the real `DialogHost` with the dialog genuinely open, and through a
    // `Wake` rather than a direct render: a test that simply rendered again would repaint by
    // construction and be silent about the only thing that was broken.
    let (sender, _shutdown) = terminal_event_channel();
    let (toggles, _requested) = mpsc::channel(1);
    let projection =
        crate::views::picker::McpProjection::new(vec![crate::views::picker::McpServer {
            name: "adk-docs-mcp".to_owned(),
            state: crate::views::picker::McpState::Connecting,
            desired_enabled: true,
        }]);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_mcp_control(projection.clone(), toggles);
    // A transcript, because the sidebar — one of the two surfaces this compares — is not drawn
    // on the welcome screen at any width. See `sidebar_drawn`.
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("so the panel has a transcript to sit beside"));
    let mut host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    host.handle_action(action("mcp_list"), &press_none());
    assert_eq!(
        host.active(),
        Some(crate::views::picker::MCP_DIALOG_ID),
        "the MCP list did not open, so nothing below is about an open panel"
    );

    // Wide enough for the sidebar, so both surfaces are on the same frame and the assertion
    // cannot be satisfied by whichever one happens to be drawn.
    let opened = rows(&render_offscreen(&mut host, 130, 30).expect("infallible")).join("\n");
    assert!(
        opened.contains("Connecting"),
        "the panel did not open on the state the projection held:\n{opened}"
    );

    // The failure the user actually hit: not a different state, a *terminal* one arriving
    // after the panel was built.
    projection.replace(vec![crate::views::picker::McpServer {
        name: "adk-docs-mcp".to_owned(),
        state: crate::views::picker::McpState::Failed("handshake timed out after 30s".to_owned()),
        desired_enabled: true,
    }]);

    let woken = host.handle_event(&crate::app::AppEvent::Terminal(TerminalEvent::Wake));
    assert!(
        woken.redraw,
        "a projection change reported no redraw, so the scheduler leaves the stale frame on \
         the terminal — which is the whole defect"
    );

    let after = rows(&render_offscreen(&mut host, 130, 30).expect("infallible"));
    let joined = after.join("\n");
    // Located per surface rather than frame-wide. A frame-wide `contains("Failed")` would be
    // satisfied by the sidebar alone — which was never the broken half — so each row is
    // pinned by the text only its own surface prints: the dialog states the reason, the
    // sidebar states its section heading's summary.
    let panel_row = after
        .iter()
        .find(|row| row.contains("handshake timed out"))
        .unwrap_or_else(|| panic!("the open panel still shows no failure reason:\n{joined}"));
    assert!(
        !panel_row.contains("Connecting"),
        "the panel row carries both states at once: {panel_row:?}"
    );
    assert!(
        after
            .iter()
            .any(|row| row.contains("MCP") && row.contains("failed")),
        "the sidebar's summary did not follow the same change, so this proves one surface \
         rather than the single source both read:\n{joined}"
    );
    assert!(
        !joined.contains("Connecting"),
        "some surface is still painting the state the server left:\n{joined}"
    );

    // Idempotence: the worker republishes on a broadcast lag and after every toggle, and a
    // frame per republication is a repaint of identical rows on a screen the redraw
    // scheduler is otherwise entitled to leave alone.
    let unchanged = host.handle_event(&crate::app::AppEvent::Terminal(TerminalEvent::Wake));
    assert!(
        !unchanged.redraw,
        "a wake with no projection change still asked for a frame"
    );
}

#[test]
fn session_screen_unknown_slash_is_a_short_toast_and_never_reaches_the_prompt_sink() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);
    screen.editor.set_text("/not-a-command secret");

    screen.handle_action(action("input_submit"), &press_none());

    assert!(
        submitted.try_recv().is_err(),
        "unknown slash input was forwarded to the model"
    );
    assert!(screen.submissions().is_empty());
    let toasts = ActionComponent::drain_toasts(&mut screen);
    let toast = toasts
        .iter()
        .find(|toast| toast.text().contains("unknown command `/not-a-command`"))
        .expect("the refusal is raised as a transient notice");
    assert_eq!(toast.level(), ToastLevel::Warning);
    assert_eq!(toast.ttl(), crate::views::toast::TOAST_TTL);
    let rendered = rows(&render_offscreen(&mut screen, 100, 12).expect("infallible")).join("\n");
    assert!(
        !rendered.contains("unknown command"),
        "a transient command refusal leaked into durable transcript rows:\n{rendered}"
    );
}

#[test]
fn session_screen_doubled_slash_submits_one_literal_slash() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);
    screen.editor.set_text("//review this literally");

    screen.handle_action(action("input_submit"), &press_none());

    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Text("/review this literally".to_owned()))
    );
    assert_eq!(screen.submissions(), ["/review this literally"]);
}

#[test]
fn session_screen_slash_autocomplete_is_an_overlay_and_completion_does_not_submit() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);
    let before = prompt_band_rows(&mut screen, 80, 20);
    screen.editor.set_text("/mo");
    screen.refresh_autocomplete();

    let rendered = rows(&render_offscreen(&mut screen, 80, 20).expect("infallible")).join("\n");
    // Singular. `model_list` canonicalises to `/model` and keeps `/models` only as an
    // alias, because `/model` is the spelling a user reaches for — and the plural is what
    // this assertion used to demand while the overlay correctly offered the singular. The
    // description is asserted alongside the name so this cannot pass on a bare substring
    // that a half-drawn overlay would also satisfy.
    assert!(rendered.contains("/model"), "{rendered}");
    assert!(rendered.contains("List available models"), "{rendered}");
    assert_eq!(prompt_band_rows(&mut screen, 80, 20), before);

    screen.handle_action(action("input_submit"), &press_none());
    assert_eq!(screen.editor.text(), "/model ");
    assert!(submitted.try_recv().is_err(), "completion submitted a turn");
}

#[test]
fn session_screen_reference_autocomplete_uses_the_host_source_without_growing_the_prompt_band() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_reference_source(Box::new(
            crate::views::autocomplete::StaticSource::new().file("src/main.rs"),
        ));
    let before = prompt_band_rows(&mut screen, 80, 20);
    screen.editor.set_text("@src/ma");
    screen.refresh_autocomplete();

    let rendered = rows(&render_offscreen(&mut screen, 80, 20).expect("infallible")).join("\n");
    assert!(rendered.contains("src/main.rs"), "{rendered}");
    assert_eq!(
        prompt_band_rows(&mut screen, 80, 20),
        before,
        "the floating reference overlay participated in vertical allocation"
    );

    screen.handle_action(action("input_submit"), &press_none());
    assert_eq!(screen.editor.text(), "@src/main.rs ");
    assert!(submitted.try_recv().is_err(), "completion submitted a turn");
}

#[test]
fn session_screen_exposes_the_autocomplete_scope_only_while_it_is_open() {
    let (mut screen, _shutdown) = screen();
    assert_eq!(ActionComponent::focused_scopes(&screen), ["history"]);

    screen.editor.set_text("/mo");
    screen.refresh_autocomplete();
    assert_eq!(
        ActionComponent::focused_scopes(&screen),
        ["prompt.autocomplete"]
    );

    screen.editor.set_text("ordinary prompt");
    screen.refresh_autocomplete();
    assert_eq!(ActionComponent::focused_scopes(&screen), ["history"]);
}

#[test]
fn session_screen_external_editor_request_carries_the_current_prompt() {
    let (sender, _shutdown) = terminal_event_channel();
    let (requests, mut requested) = mpsc::channel(1);
    let (_results, result_source) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_external_editor(requests, result_source);
    screen.editor.set_text("draft body");

    let result = screen.handle_action(action("editor_open"), &press_none());

    assert!(result.redraw);
    assert_eq!(
        requested.try_recv().expect("one editor request"),
        EditorRequest::new("draft body")
    );
}

#[test]
fn session_screen_external_editor_result_replaces_the_prompt() {
    let (sender, _shutdown) = terminal_event_channel();
    let (requests, _requested) = mpsc::channel(1);
    let (results, result_source) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_external_editor(requests, result_source);
    screen.editor.set_text("draft body");
    results
        .try_send(Ok(Some(String::from("edited body"))))
        .expect("result channel accepts the edit");

    let result = screen.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));

    assert!(result.redraw);
    assert_eq!(screen.editor.text(), "edited body");
}

#[test]
fn session_screen_empty_external_editor_result_keeps_the_prompt() {
    let (sender, _shutdown) = terminal_event_channel();
    let (requests, _requested) = mpsc::channel(1);
    let (results, result_source) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_external_editor(requests, result_source);
    screen.editor.set_text("keep this body");
    results
        .try_send(Ok(None))
        .expect("result channel accepts the no-change result");

    screen.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));

    assert_eq!(screen.editor.text(), "keep this body");
}

#[test]
fn session_screen_resolving_app_exit_requests_shutdown_through_the_channel() {
    // The property the boot path depends on: `App::run` returns on nothing but a
    // `Shutdown` event, so a screen that only returned a flag would never end.
    let (mut screen, mut shutdown) = screen();
    screen.handle_action(action("app_exit"), &press_none());

    assert!(
        matches!(shutdown.try_recv(), Ok(TerminalEvent::Shutdown)),
        "resolving `app_exit` did not put a shutdown on the terminal channel"
    );
}

/// The key event `app_exit`'s first shipped spelling describes.
///
/// Built from the table rather than written as `ctrl+c` so a rebinding of the
/// action moves this test with it instead of silently testing the wrong key.
fn exit_key_event() -> KeyEvent {
    let spelling = action("app_exit")
        .keys
        .split(',')
        .next()
        .expect("one spelling");
    let chord = crate::keybind::Chord::parse(spelling).expect("a valid spelling");
    let rendered = chord.to_string();
    KeyEvent {
        code: crossterm::event::KeyCode::Char(rendered.chars().last().expect("a key character")),
        modifiers: if rendered.contains("ctrl+") {
            crossterm::event::KeyModifiers::CONTROL
        } else {
            crossterm::event::KeyModifiers::NONE
        },
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

#[test]
fn session_screen_the_exit_key_clears_a_typed_prompt_before_it_leaves() {
    // The two-press behaviour the reference TUI has: the first press throws away
    // what was typed, and only a press with nothing to clear ends the session. A
    // screen that exited on the first press would lose a half-written prompt.
    let (mut screen, mut shutdown) = screen();
    screen.editor.set_text("half-written");
    screen.handle_action(action("input_clear"), &exit_key_event());

    assert!(
        shutdown.try_recv().is_err(),
        "the first press must clear the prompt, not leave"
    );
    assert!(
        screen.editor.text().is_empty(),
        "the prompt was not cleared"
    );

    screen.handle_action(action("input_clear"), &exit_key_event());
    assert!(
        matches!(shutdown.try_recv(), Ok(TerminalEvent::Shutdown)),
        "a press with nothing to clear must request shutdown"
    );
}

#[test]
fn session_screen_ctrl_d_only_leaves_when_the_prompt_is_empty() {
    let (mut screen, mut shutdown) = screen();
    screen.editor.set_text("keep this prompt");

    screen.handle_action(action("input_delete"), &key_event("ctrl+d"));
    assert!(
        shutdown.try_recv().is_err(),
        "ctrl+d left while the prompt still contained text"
    );
    assert_eq!(
        screen.editor.text(),
        "keep this prompt",
        "ctrl+d at the end of the prompt should remain an editor delete, not clear input"
    );

    screen.editor.set_text("");
    screen.handle_action(action("input_delete"), &key_event("ctrl+d"));
    assert!(
        matches!(shutdown.try_recv(), Ok(TerminalEvent::Shutdown)),
        "ctrl+d with an empty prompt did not request shutdown"
    );
}

#[test]
fn session_screen_scopes_shadow_the_exit_action_with_the_editor_bindings() {
    // Recording the shadowing rather than asserting the convenient answer. Both
    // `ctrl+c` and `ctrl+d` are bound twice in the shipped table, the `input` scope
    // wins, and that is exactly why the screen has to treat the editor actions as
    // exit when there is nothing to act on.
    let mut keymap = Keymap::defaults().expect("the shipped table builds");
    let scopes = scopes();
    let borrowed = scopes.iter().map(String::as_str).collect::<Vec<_>>();
    for (spelling, expected) in [("ctrl+c", "input_clear"), ("ctrl+d", "input_delete")] {
        let chord = crate::keybind::Chord::parse(spelling).expect("a valid spelling");
        let resolved = keymap.resolve(&borrowed, chord, std::time::Instant::now());
        let crate::keybind::Resolution::Action { definition, .. } = resolved else {
            panic!("`{spelling}` does not resolve in the session screen's scope chain");
        };
        assert_eq!(
            definition.name, expected,
            "`{spelling}` no longer resolves to the editor action the screen \
             compensates for; the exit path needs revisiting"
        );
    }
    assert!(
        action("app_exit").keys.starts_with("ctrl+c"),
        "the exit action no longer claims the chord the editor shadows"
    );
}

/// The scope chain a focused permission prompt installs, as the CLI bridge builds it.
///
/// Reproduced here rather than imported because the bridge lives in `zuno-cli`, above
/// this crate. `session` is in it so `escape` can reject — and that is precisely what
/// drags `session_delete` in front of `app_exit` on `ctrl+d`.
fn focused_permission_scopes() -> Vec<String> {
    let mut chain = vec![
        String::from("permission.prompt"),
        String::from("dialog.select"),
        String::from("dialog.prompt"),
        String::from("session"),
    ];
    chain.extend(scopes());
    chain
}

fn permission_prompt() -> Box<dyn crate::views::dialog::Dialog> {
    Box::new(crate::views::permission::PermissionPrompt::new(
        ViewContext::defaults(),
        zuno_permission::PermissionRequest {
            id: String::from("req_exit"),
            session_id: String::from("ses_exit"),
            permission: String::from("bash"),
            patterns: vec![String::from("*")],
            metadata: serde_json::Map::new(),
            always: vec![String::from("*")],
            tool: None,
        },
        &serde_json::json!({}),
    ))
}

#[test]
fn session_screen_places_permission_prompts_in_the_composer_region() {
    let (screen, _shutdown) = screen();
    let area = Rect::new(0, 0, 120, 30);
    let question = screen
        .dialog_region(crate::views::question::DIALOG_ID, area)
        .expect("question composer region");
    let permission = screen
        .dialog_region(crate::views::permission::DIALOG_ID, area)
        .expect("permission composer region");

    assert_eq!(
        permission, question,
        "approval should replace the composer instead of floating over the transcript"
    );
}

/// An open modal must not be able to trap a user in a raw-mode terminal.
///
/// The regression this pins was reachable and reported: with a permission prompt up,
/// `ctrl+d` resolves to `session_delete` and `ctrl+c` to `input_clear`, the prompt
/// understands neither, and `DialogHost` used to absorb both — so the one component
/// that sends `Shutdown` never heard either key. Raw mode having taken `SIGINT` away,
/// nothing could end the process.
///
/// Asserted through the real tree at the real focused scope chain, because every
/// layer in it may decline to forward and the defect lived in exactly one of them.
#[test]
fn session_screen_the_exit_chord_leaves_even_while_a_modal_owns_the_keyboard() {
    for spelling in exit_chord_spellings() {
        let (screen, mut shutdown) = screen();
        let mut host =
            crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
        host.open(permission_prompt());
        assert!(host.is_open(), "the prompt is not focused");
        let mut dispatcher = KeyDispatcher::new(
            Keymap::defaults().expect("the shipped table builds"),
            focused_permission_scopes(),
            Box::new(host),
        );

        dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(key_event(&spelling)),
        )));

        assert!(
            matches!(shutdown.try_recv(), Ok(TerminalEvent::Shutdown)),
            "`{spelling}` was absorbed by the modal, leaving no way out of the TUI"
        );
    }
}

/// Every single-chord spelling the table gives to `app_exit`.
fn exit_chord_spellings() -> Vec<String> {
    action("app_exit")
        .keys
        .split(',')
        .map(str::trim)
        .filter(|spelling| !spelling.contains(crate::keybind::LEADER_TOKEN))
        .map(str::to_owned)
        .collect()
}

fn key_event(spelling: &str) -> KeyEvent {
    let chord = crate::keybind::Chord::parse(spelling).expect("a valid spelling");
    let rendered = chord.to_string();
    KeyEvent {
        code: crossterm::event::KeyCode::Char(rendered.chars().last().expect("a key character")),
        modifiers: if rendered.contains("ctrl+") {
            crossterm::event::KeyModifiers::CONTROL
        } else {
            crossterm::event::KeyModifiers::NONE
        },
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

/// A modal still owns every key that is not the way out.
///
/// The other half of the exception: forwarding *all* ignored actions would let
/// `session_new` fire behind a permission prompt, which is the property the modal
/// discipline exists for.
#[test]
fn session_screen_a_modal_still_absorbs_actions_that_are_not_the_exit() {
    let (screen, mut shutdown) = screen();
    let mut host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    host.open(permission_prompt());

    let absorbed = host.handle_action(action("session_new"), &press_none());

    assert!(
        absorbed.handled && !absorbed.redraw,
        "an unrelated action was not absorbed by the modal: {absorbed:?}"
    );
    assert!(
        shutdown.try_recv().is_err(),
        "an unrelated action reached the base and requested shutdown"
    );
}

/// `delete` on an empty prompt must not quit the application.
///
/// `input_delete`'s shipped spelling is `ctrl+d,delete,shift+delete`, so a screen
/// that read exit intent from the *action name* quit on all three. Intent belongs to
/// the chord: only the ones `app_exit` binds may leave.
#[test]
fn session_screen_a_non_exit_spelling_of_the_same_action_does_not_leave() {
    let (mut screen, mut shutdown) = screen();
    screen.handle_action(
        action("input_delete"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Delete),
    );

    assert!(
        shutdown.try_recv().is_err(),
        "`delete` left the application, which the table never bound it to do"
    );
}

/// An exit chord during a running turn cancels the turn; the next one always leaves.
///
/// The second press is asserted **without** delivering `TurnInterrupted`, because
/// that is the case a real terminal exposed: a turn parked on a permission ask never
/// reaches the engine's interrupt check, so it stays "running" after an abort. A
/// screen that decided by re-reading the strip cancelled forever and never left. The
/// only safe rule is that one press is remembered and the next one leaves regardless.
#[test]
fn session_screen_the_second_exit_chord_leaves_even_if_the_cancelled_turn_never_ends() {
    let (sender, mut shutdown) = terminal_event_channel();
    let (cancels, mut cancelled) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_cancel_sink(cancels);
    screen.status.mark_running();

    screen.handle_action(action("app_exit"), &press_none());
    assert_eq!(cancelled.try_recv(), Ok(()), "the turn was not cancelled");
    assert_eq!(screen.cancellations(), 1);
    assert!(
        shutdown.try_recv().is_err(),
        "the first press tore the application down instead of cancelling the turn"
    );

    assert!(
        screen.status.is_running(),
        "this test is only meaningful while the strip still reports a running turn"
    );
    screen.handle_action(action("app_exit"), &press_none());
    assert!(
        matches!(shutdown.try_recv(), Ok(TerminalEvent::Shutdown)),
        "a turn that ignored its abort left the user with no way out"
    );
    assert_eq!(
        screen.cancellations(),
        1,
        "the second press must leave, not cancel again"
    );
}

#[test]
fn session_screen_double_escape_cancels_without_leaving_the_application() {
    let (sender, mut shutdown) = terminal_event_channel();
    let (cancels, mut cancelled) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_cancel_sink(cancels);
    screen.status.mark_running();

    screen.handle_action(
        action("session_interrupt"),
        &press(crossterm::event::KeyCode::Esc),
    );
    assert!(
        cancelled.try_recv().is_err(),
        "the first escape cancelled instead of asking for confirmation"
    );
    assert!(
        shutdown.try_recv().is_err(),
        "the first escape left the application"
    );
    let first = rows(&render_offscreen(&mut screen, 80, 24).expect("infallible"));
    let hint = first
        .iter()
        .position(|row| row.contains("esc again to interrupt"))
        .unwrap_or_else(|| {
            panic!(
                "the first escape did not put its confirmation in the live footer:\n{}",
                first.join("\n")
            )
        });
    let prompt = first
        .iter()
        .position(|row| row.contains(PROMPT_PLACEHOLDER))
        .expect("the empty composer is visible");
    assert!(
        hint > prompt,
        "the interrupt confirmation belongs in the footer below the composer:\n{}",
        first.join("\n")
    );

    screen.handle_action(
        action("session_interrupt"),
        &press(crossterm::event::KeyCode::Esc),
    );
    assert_eq!(
        cancelled.try_recv(),
        Ok(()),
        "the second escape did not cancel"
    );
    assert_eq!(screen.cancellations(), 1);
    assert!(
        shutdown.try_recv().is_err(),
        "double escape cancelled the whole TUI instead of the active turn"
    );
}

#[test]
fn session_screen_two_escapes_cancel_even_when_the_first_closes_a_question_dialog() {
    let (sender, _shutdown) = terminal_event_channel();
    let (cancels, mut cancelled) = mpsc::channel(1);
    let context = ViewContext::defaults();
    let mut screen = SessionScreen::new(context.clone(), sender).with_cancel_sink(cancels);
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("choose a delivery mode"));
    screen.status.mark_running();
    let mut host = DialogHost::new(context.clone(), Box::new(screen));
    host.open(Box::new(crate::views::question::QuestionPrompt::new(
        context,
        vec![crate::views::question::QuestionRequest::new(
            "How should the result be delivered?",
            "Delivery",
            vec![crate::views::question::QuestionOption::new(
                "Next step",
                "Wake the parent on its next safe point",
            )],
        )],
    )));
    // Paint once so the base knows the running turn is waiting behind this modal.
    let _ = render_offscreen(&mut host, 80, 24).expect("infallible");

    host.handle_action(
        action("session_interrupt"),
        &press(crossterm::event::KeyCode::Esc),
    );
    assert_eq!(
        host.active(),
        None,
        "the first escape did not close the dialog"
    );
    assert!(
        cancelled.try_recv().is_err(),
        "the first escape cancelled instead of arming confirmation"
    );
    let armed = rows(&render_offscreen(&mut host, 80, 24).expect("infallible")).join("\n");
    assert!(
        armed.contains("esc again to interrupt"),
        "closing a modal consumed the escape instead of also arming the active turn:\n{armed}"
    );

    host.handle_action(
        action("session_interrupt"),
        &press(crossterm::event::KeyCode::Esc),
    );
    assert_eq!(
        cancelled.try_recv(),
        Ok(()),
        "two physical escape presses did not cancel the active turn"
    );
}

#[test]
fn session_screen_two_escapes_cancel_even_when_the_first_rejects_permission() {
    let (sender, _shutdown) = terminal_event_channel();
    let (cancels, mut cancelled) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_cancel_sink(cancels);
    screen.status.mark_running();
    let mut host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    host.open(permission_prompt());
    let _ = render_offscreen(&mut host, 80, 24).expect("infallible");

    host.handle_action(
        action("session_interrupt"),
        &press(crossterm::event::KeyCode::Esc),
    );
    assert_eq!(
        host.active(),
        None,
        "the first escape did not reject the permission prompt"
    );
    assert!(
        cancelled.try_recv().is_err(),
        "the first escape cancelled instead of arming confirmation"
    );

    host.handle_action(
        action("session_interrupt"),
        &press(crossterm::event::KeyCode::Esc),
    );
    assert_eq!(
        cancelled.try_recv(),
        Ok(()),
        "two physical escape presses did not cancel the permission-blocked turn"
    );
}

#[test]
fn session_screen_escape_confirmation_expires_before_a_later_press() {
    let (sender, _shutdown) = terminal_event_channel();
    let (cancels, mut cancelled) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_cancel_sink(cancels);
    screen.status.mark_running();

    screen.request_interrupt_at(1_000);
    screen.request_interrupt_at(1_000 + INTERRUPT_CONFIRM_WINDOW_MS + 1);

    assert!(
        cancelled.try_recv().is_err(),
        "an old escape arm cancelled a turn after the confirmation window"
    );
    assert_eq!(screen.cancellations(), 0);
}

/// A later turn is cancellable again; only the turn already asked about is remembered.
#[test]
fn session_screen_a_new_turn_can_be_cancelled_after_an_earlier_one_was() {
    let (sender, _shutdown) = terminal_event_channel();
    let (prompts, mut submitted) = mpsc::channel(1);
    let (cancels, mut cancelled) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_sink(prompts)
        .with_cancel_sink(cancels);

    screen.submit_prompt("first");
    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Text(String::from("first")))
    );
    screen.handle_action(action("app_exit"), &press_none());
    assert_eq!(cancelled.try_recv(), Ok(()));
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnInterrupted {
        assistant_message_id: None,
        steps: 0,
    }));

    screen.submit_prompt("second");
    assert_eq!(
        submitted.try_recv(),
        Ok(PromptSubmission::Text(String::from("second")))
    );
    screen.handle_action(action("app_exit"), &press_none());

    assert_eq!(
        cancelled.try_recv(),
        Ok(()),
        "a fresh turn inherited the previous turn's spent cancellation"
    );
    assert_eq!(screen.cancellations(), 2);
}

/// A refusing cancel sink must cost a cancellation, never the way out.
#[test]
fn session_screen_a_full_cancel_sink_falls_through_to_shutdown() {
    let (sender, mut shutdown) = terminal_event_channel();
    let (cancels, _held) = mpsc::channel(1);
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), sender).with_cancel_sink(cancels.clone());
    screen.status.mark_running();
    cancels.try_send(()).expect("the sink starts empty");

    screen.handle_action(action("app_exit"), &press_none());

    assert!(
        matches!(shutdown.try_recv(), Ok(TerminalEvent::Shutdown)),
        "a sink that could not take the request must not swallow the exit"
    );
}

#[test]
fn session_screen_is_dispatchable_through_the_keymap_without_naming_a_key() {
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let (screen, _shutdown) = screen();
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(screen));
    let spelling = action("input_clear")
        .keys
        .split(',')
        .next()
        .expect("one spelling");
    let chord = crate::keybind::Chord::parse(spelling).expect("a valid spelling");
    let event = KeyEvent {
        code: match chord.to_string().rsplit('+').next().unwrap_or_default() {
            "return" => crossterm::event::KeyCode::Enter,
            "escape" => crossterm::event::KeyCode::Esc,
            other => crossterm::event::KeyCode::Char(other.chars().next().unwrap_or('?')),
        },
        modifiers: if chord.to_string().contains("ctrl+") {
            crossterm::event::KeyModifiers::CONTROL
        } else {
            crossterm::event::KeyModifiers::NONE
        },
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };

    assert!(
        dispatcher
            .handle_event(&AppEvent::Terminal(TerminalEvent::Input(
                crossterm::event::Event::Key(event)
            )))
            .handled,
        "the dispatcher did not resolve the editor's own default spelling"
    );
}

#[test]
fn session_screen_enter_still_submits_without_a_focused_dialog() {
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let (mut screen, _shutdown) = screen();
    screen.editor.set_text("send normally");
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(screen));
    let result = dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        crossterm::event::Event::Key(crate::views::testkit::press(
            crossterm::event::KeyCode::Enter,
        )),
    )));

    assert!(result.redraw, "Enter no longer submitted the normal prompt");
}

/// A draw target that renders offscreen, for a loop test with no terminal.
struct OffscreenTarget {
    terminal: ratatui::Terminal<ratatui::backend::TestBackend>,
}

impl crate::app::DrawTarget for OffscreenTarget {
    fn draw(&mut self, root: &mut dyn Component) -> std::io::Result<()> {
        self.terminal
            .draw(|frame| root.render(frame, frame.area()))
            .map(|_| ())
            .map_err(|error| match error {})
    }

    fn clear(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn resize(&mut self, width: u16, height: u16) -> std::io::Result<()> {
        self.terminal
            .resize(ratatui::layout::Rect::new(0, 0, width, height))
            .map_err(|error| match error {})
    }
}

struct NoopLifecycle;

impl crate::app::TerminalLifecycle for NoopLifecycle {
    fn enter(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn restore(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn is_active(&self) -> bool {
        true
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_screen_exit_key_ends_the_application_loop() {
    // The property the `tui` command's whole lifetime depends on, asserted through
    // the real tree — dispatcher over dialog host over screen — because each layer
    // may decline to forward, and a declined `app_exit` is a TUI nobody can leave.
    let (terminal_sender, terminal_receiver) = terminal_event_channel();
    let (_engine_sender, engine_receiver) = zuno_engine::r#loop::event_channel();
    let screen = SessionScreen::new(ViewContext::defaults(), terminal_sender.clone());
    let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    let root = KeyDispatcher::new(
        Keymap::defaults().expect("the shipped table builds"),
        scopes(),
        Box::new(host),
    );
    let target = OffscreenTarget {
        terminal: ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8))
            .expect("infallible"),
    };
    let (mut app, _owner) = crate::app::App::new(
        Box::new(root),
        Box::new(target),
        std::sync::Arc::new(NoopLifecycle),
        terminal_receiver,
        engine_receiver,
    );

    let spelling = action("app_exit")
        .keys
        .split(',')
        .next()
        .expect("one spelling");
    let chord = crate::keybind::Chord::parse(spelling).expect("a valid spelling");
    let rendered = chord.to_string();
    let event = KeyEvent {
        code: crossterm::event::KeyCode::Char(rendered.chars().last().expect("a key character")),
        modifiers: if rendered.contains("ctrl+") {
            crossterm::event::KeyModifiers::CONTROL
        } else {
            crossterm::event::KeyModifiers::NONE
        },
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    terminal_sender
        .send(TerminalEvent::Input(crossterm::event::Event::Key(event)))
        .await
        .expect("the application is listening");

    tokio::time::timeout(std::time::Duration::from_secs(5), app.run())
        .await
        .expect("pressing the exit key must end the loop")
        .expect("the loop must end cleanly");
}

// ---------------------------------------------------------------------------
// Pickers: the four surfaces nothing could previously reach
// ---------------------------------------------------------------------------

fn catalog() -> SessionCatalog {
    SessionCatalog {
        models: vec![
            crate::views::picker::ModelEntry {
                id: String::from("prov/haiku"),
                name: String::from("haiku"),
                provider: String::from("prov"),
                reasoning: false,
            },
            crate::views::picker::ModelEntry {
                id: String::from("prov/sonnet"),
                name: String::from("sonnet"),
                provider: String::from("prov"),
                reasoning: false,
            },
        ],
        agents: vec![crate::views::picker::AgentEntry {
            name: String::from("plan"),
            description: String::from("read-only planning"),
        }],
        sessions: Vec::new(),
        session: None,
        model: Some(String::from("prov/haiku")),
        agent: Some(String::from("build")),
        presets: Vec::new(),
        preset: None,
        reasoning: false,
        reasoning_efforts: Default::default(),
        effort: None,
    }
}

#[test]
fn session_screen_asks_the_host_to_open_the_model_picker() {
    // Before the `drain_dialogs` seam existed, `model_picker` was constructible only
    // from its own tests: the screen sits *below* the dialog host, so it could not open
    // anything, and `<leader>m` resolved to an action nothing acted on.
    let (mut screen, _shutdown) = screen();
    *screen.catalog_mut() = catalog();
    let result = screen.handle_action(action("model_list"), &press_none());
    assert!(result.handled && result.redraw);
    let requested = screen.drain_dialogs();
    assert_eq!(requested.len(), 1, "no dialog was requested");
    assert_eq!(requested[0].id(), crate::views::picker::MODEL_DIALOG_ID);
    assert!(
        screen.drain_dialogs().is_empty(),
        "a drained request was offered twice"
    );
}

#[test]
fn session_screen_preset_command_opens_the_picker_and_applies_its_choice() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(4);
    let mut catalog = catalog();
    catalog.presets = vec![String::from("deliberate"), String::from("fast")];
    catalog.preset = Some(String::from("fast"));
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_catalog(catalog)
        .with_selection_sink(selections);

    screen.submit_prompt("/preset");

    let requested = screen.drain_dialogs();
    assert_eq!(requested.len(), 1, "the preset picker was not requested");
    assert_eq!(requested[0].id(), crate::views::picker::PRESET_DIALOG_ID);

    screen.adopt(crate::views::picker::PRESET_DIALOG_ID, "deliberate");
    assert_eq!(screen.catalog.preset.as_deref(), Some("deliberate"));
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Preset(String::from("deliberate")))
    );
}

#[test]
fn session_screen_named_preset_switches_without_opening_a_picker() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(4);
    let mut catalog = catalog();
    catalog.presets = vec![String::from("deliberate"), String::from("fast")];
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_catalog(catalog)
        .with_selection_sink(selections);

    screen.submit_prompt("/preset fast");

    assert!(screen.drain_dialogs().is_empty());
    assert_eq!(screen.catalog.preset.as_deref(), Some("fast"));
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Preset(String::from("fast")))
    );
}

#[test]
fn session_screen_opens_a_picker_through_the_dialog_host() {
    // The end-to-end proof: a key press resolved by the dispatcher reaches the screen,
    // the screen asks, and the host opens — with no component naming a key.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let (sender, _receiver) = terminal_event_channel();
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender);
    *screen.catalog_mut() = catalog();
    let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(host));

    for chord in ["ctrl+x", "m"] {
        let parsed = crate::keybind::Chord::parse(chord).expect("a valid spelling");
        let event = KeyEvent {
            code: match parsed.to_string().rsplit('+').next() {
                Some("m") => crossterm::event::KeyCode::Char('m'),
                _ => crossterm::event::KeyCode::Char('x'),
            },
            modifiers: if parsed.to_string().contains("ctrl+") {
                crossterm::event::KeyModifiers::CONTROL
            } else {
                crossterm::event::KeyModifiers::NONE
            },
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        dispatcher.handle_event(&crate::app::AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(event),
        )));
    }
    let joined = rows(&render_offscreen(&mut dispatcher, 80, 24).expect("infallible")).join("\n");
    assert!(
        joined.contains("Models"),
        "the leader sequence did not open the model picker:\n{joined}"
    );
    assert!(joined.contains("haiku"), "{joined}");
}

#[test]
fn session_screen_reports_an_unavailable_generic_picker_without_polluting_the_transcript() {
    let (mut screen, _shutdown) = screen();
    let result = screen.handle_action(action("model_list"), &press_none());
    assert!(result.redraw);
    assert!(
        screen.drain_dialogs().is_empty(),
        "an empty picker was opened anyway"
    );
    let joined = rows(&render_offscreen(&mut screen, 80, 12).expect("infallible")).join("\n");
    assert!(
        !joined.contains("nothing to choose from") && !joined.contains("Nothing is available"),
        "transient picker state leaked into the transcript:\n{joined}"
    );
    let toasts = ActionComponent::drain_toasts(&mut screen);
    assert!(
        toasts
            .iter()
            .any(|toast| toast.text().contains("Nothing is available")),
        "the refusal was silent: {toasts:?}"
    );
}

#[test]
fn empty_session_list_opens_an_explicit_empty_state_without_writing_a_message() {
    let (mut screen, _shutdown) = screen();
    let result = screen.handle_action(action("session_list"), &press_none());
    assert!(result.redraw);
    assert_eq!(screen.drain_dialogs().len(), 1);

    let transcript = rows(&render_offscreen(&mut screen, 80, 12).expect("infallible")).join("\n");
    assert!(
        !transcript.contains("nothing to choose from")
            && !transcript.contains("No saved sessions yet"),
        "opening /session wrote interface state into the conversation:\n{transcript}"
    );
}

#[test]
fn session_materialization_makes_the_new_session_discoverable_without_a_remount() {
    let (mut screen, _shutdown) = screen();
    *screen.catalog_mut() = catalog();
    screen.handle_event(&AppEvent::Engine(TurnEvent::SessionMaterialized {
        session_id: String::from("ses_new"),
        title: String::from("New session"),
    }));

    assert_eq!(screen.catalog.session.as_deref(), Some("ses_new"));
    assert_eq!(screen.catalog.sessions.len(), 1);
    assert_eq!(screen.catalog.sessions[0].id, "ses_new");
    assert_eq!(screen.catalog.sessions[0].when, "now");
}

#[test]
fn session_new_action_requests_a_fresh_lazy_session_from_the_host() {
    let (sender, _receiver) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(1);
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), sender).with_selection_sink(selections);

    let result = screen.handle_action(action("session_new"), &press_none());

    assert!(result.redraw);
    assert_eq!(chosen.try_recv(), Ok(Selection::NewSession));
    assert!(
        ActionComponent::drain_toasts(&mut screen)
            .iter()
            .any(|toast| toast.text().contains("starting a new session")),
        "the in-process remount was silent"
    );
}

#[test]
fn session_screen_opens_the_session_picker_and_forwards_a_switch() {
    let (sender, _receiver) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(4);
    let mut offered = catalog();
    offered.sessions = vec![
        crate::views::picker::SessionEntry {
            id: String::from("ses_current"),
            title: String::from("current work"),
            when: String::from("10:00 AM"),
        },
        crate::views::picker::SessionEntry {
            id: String::from("ses_previous"),
            title: String::from("previous work"),
            when: String::from("yesterday"),
        },
    ];
    offered.session = Some(String::from("ses_current"));
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(offered);

    let result = screen.handle_action(action("session_list"), &press_none());
    assert!(result.redraw);
    let dialogs = screen.drain_dialogs();
    assert_eq!(
        dialogs.len(),
        1,
        "the populated session picker did not open"
    );
    assert_eq!(dialogs[0].id(), crate::views::picker::SESSION_DIALOG_ID);

    screen.apply_dialog_outcome(
        crate::views::picker::SESSION_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: crate::views::picker::SESSION_DIALOG_ID,
            value: String::from("ses_previous"),
        },
    );
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Session(String::from("ses_previous"))),
        "the selected session never reached the host"
    );
    assert_eq!(
        screen.catalog.session.as_deref(),
        Some("ses_current"),
        "the old screen claimed the switch before the host validated it"
    );
    assert!(
        ActionComponent::drain_toasts(&mut screen)
            .iter()
            .any(|toast| toast.text().contains("switching to session")),
        "the screen did not state that a full session switch was starting"
    );
}

#[test]
fn session_screen_selecting_the_current_session_is_a_visible_noop() {
    let (sender, _receiver) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(1);
    let mut offered = catalog();
    offered.sessions = vec![crate::views::picker::SessionEntry {
        id: String::from("ses_current"),
        title: String::from("current work"),
        when: String::from("now"),
    }];
    offered.session = Some(String::from("ses_current"));
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(offered);

    screen.apply_dialog_outcome(
        crate::views::picker::SESSION_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: crate::views::picker::SESSION_DIALOG_ID,
            value: String::from("ses_current"),
        },
    );

    assert!(
        chosen.try_recv().is_err(),
        "the active session was re-opened"
    );
    assert!(
        ActionComponent::drain_toasts(&mut screen)
            .iter()
            .any(|toast| toast.text().contains("already active")),
        "the no-op selection was silent"
    );
}

#[test]
fn session_screen_rename_request_opens_a_prefilled_prompt_and_forwards_the_new_title() {
    let (sender, _receiver) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(2);
    let mut offered = catalog();
    offered.sessions = vec![crate::views::picker::SessionEntry {
        id: String::from("ses_current"),
        title: String::from("current work"),
        when: String::from("now"),
    }];
    offered.session = Some(String::from("ses_current"));
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(offered);

    let result = screen.apply_dialog_outcome(
        crate::views::picker::SESSION_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Session(
            crate::views::picker::SessionDialogAction::Rename {
                id: String::from("ses_current"),
                title: String::from("current work"),
            },
        ),
    );
    assert!(result.redraw);
    let mut requested = screen.drain_dialogs();
    assert_eq!(requested.len(), 1, "rename did not open its prompt");
    let mut prompt = requested.pop().expect("one prompt");
    assert_eq!(prompt.id(), SESSION_RENAME_DIALOG_ID);
    let joined = prompt
        .lines(60)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        joined.contains("current work"),
        "the existing title was not prefilled:\n{joined}"
    );

    screen.apply_dialog_outcome(
        SESSION_RENAME_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Submitted {
            dialog: SESSION_RENAME_DIALOG_ID,
            text: String::from("renamed work"),
        },
    );
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::SessionRename {
            id: String::from("ses_current"),
            title: String::from("renamed work"),
        })
    );
}

#[test]
fn session_screen_confirmed_delete_forwards_a_typed_session_delete() {
    let (sender, _receiver) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(1);
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), sender).with_selection_sink(selections);

    screen.apply_dialog_outcome(
        crate::views::picker::SESSION_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Session(
            crate::views::picker::SessionDialogAction::Delete {
                id: String::from("ses_old"),
                title: String::from("old work"),
            },
        ),
    );

    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::SessionDelete(String::from("ses_old")))
    );
}

#[test]
fn session_screen_applies_a_model_choice_and_forwards_it() {
    let (sender, _receiver) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(4);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_catalog(catalog());
    let result = screen.apply_dialog_outcome(
        crate::views::picker::MODEL_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: crate::views::picker::MODEL_DIALOG_ID,
            value: String::from("prov/sonnet"),
        },
    );
    assert!(result.redraw);
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Model(String::from("prov/sonnet"))),
        "the choice never reached the host"
    );
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("use the selected model"));
    let joined = rows(&render_offscreen(&mut screen, 90, 14).expect("infallible")).join("\n");
    assert!(
        joined.contains("▣ build · sonnet"),
        "the reply identity still names the previous model:\n{joined}"
    );
    // On the toast rather than in the transcript — see `SessionScreen::adopt`. The claim is
    // unchanged: the user is told *when* the choice takes effect, because a strip that already
    // names the new model without saying that would imply the running turn had switched.
    let toasts = ActionComponent::drain_toasts(&mut screen);
    assert!(
        toasts
            .iter()
            .any(|toast| toast.text().contains("next turn")),
        "nothing says when the choice takes effect: {toasts:?}"
    );
}

#[test]
fn session_screen_reports_a_choice_that_reached_nobody() {
    // The defect class this whole seam is about: a picker that appears to work and a
    // selection that went nowhere.
    let (mut screen, _shutdown) = screen();
    *screen.catalog_mut() = catalog();
    screen.apply_dialog_outcome(
        crate::views::picker::AGENT_DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: crate::views::picker::AGENT_DIALOG_ID,
            value: String::from("plan"),
        },
    );
    let toasts = ActionComponent::drain_toasts(&mut screen);
    assert!(
        toasts
            .iter()
            .any(|toast| toast.text().contains("not applied")),
        "a selection with no listener was reported as applied: {toasts:?}"
    );
}

#[test]
fn session_screen_theme_picker_opens_without_a_catalog() {
    // Themes come from the registry, not the host, so this picker is never empty.
    let (mut screen, _shutdown) = screen();
    screen.handle_action(action("theme_list"), &press_none());
    let requested = screen.drain_dialogs();
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0].id(), crate::views::picker::THEME_DIALOG_ID);
}

#[test]
fn session_screen_thinking_and_tool_detail_keys_reach_the_transcript() {
    // Both actions ship unbound in the oracle's table, so they are reachable only
    // through a user binding or a palette — but the *routing* must exist, or binding
    // them would still do nothing.
    let (mut screen, _shutdown) = screen();
    let before = screen.transcript_mut().thinking();
    screen.handle_action(action("display_thinking"), &press_none());
    assert_ne!(
        screen.transcript_mut().thinking(),
        before,
        "`display_thinking` did not reach the transcript"
    );
    let tools = screen.transcript_mut().tool_output();
    screen.handle_action(action("tool_details"), &press_none());
    assert_ne!(
        screen.transcript_mut().tool_output(),
        tools,
        "`tool_details` did not reach the transcript"
    );
}

#[test]
fn session_screen_sidebar_toggle_hides_the_ambient_panel() {
    let (mut screen, _shutdown) = screen();
    // A message, because the panel is not drawn on the welcome screen at any width — see
    // `sidebar_drawn` and `the_ambient_panel_waits_for_a_transcript`. Without one the positive
    // half of this test would be unsatisfiable and the negative half would hold vacuously.
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("so the panel has a transcript to sit beside"));
    assert!(screen.sidebar_visible());
    let wide = rows(&render_offscreen(&mut screen, 200, 30).expect("infallible")).join("\n");
    assert!(wide.contains("Context"), "the panel is not drawn:\n{wide}");

    screen.handle_action(action("sidebar_toggle"), &press_none());
    assert!(!screen.sidebar_visible());
    let hidden = rows(&render_offscreen(&mut screen, 200, 30).expect("infallible")).join("\n");
    assert!(
        !hidden.contains("Context"),
        "the panel is still drawn after being toggled off:\n{hidden}"
    );
}

#[test]
fn session_screen_drops_the_panel_rather_than_squeezing_it() {
    let (mut screen, _shutdown) = screen();
    // The width threshold is only observable once the panel is eligible at all, and it is not
    // eligible while the transcript is empty — see `sidebar_drawn`.
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("so the panel has a transcript to sit beside"));
    let narrow = rows(&render_offscreen(&mut screen, 100, 30).expect("infallible")).join("\n");
    assert!(
        !narrow.contains("Context"),
        "the panel was drawn at 100 columns, below the threshold:\n{narrow}"
    );
    let wide = rows(&render_offscreen(&mut screen, 120, 30).expect("infallible")).join("\n");
    assert!(
        wide.contains("Context"),
        "the panel is missing at exactly the threshold width:\n{wide}"
    );
}

#[test]
fn session_screen_shows_the_welcome_surface_only_while_the_transcript_is_empty() {
    // The anchor has to satisfy two properties, and only the lead line satisfies both: it
    // is unconditional in `WelcomeView::lines`, and it appears on no other surface. A
    // looser needle such as `/ for commands` also matches the *composer's* placeholder
    // (`ask anything, or / for commands`), which is drawn in both halves of this test — so
    // the negative assertion would hold vacuously and this test would pass with the
    // welcome screen gone entirely.
    const WELCOME_ONLY: &str = "type / for commands";
    let (mut screen, _shutdown) = screen();
    let empty = rows(&render_offscreen(&mut screen, 200, 40).expect("infallible")).join("\n");
    assert!(
        empty.contains(WELCOME_ONLY),
        "the welcome surface is missing on an empty transcript:\n{empty}"
    );
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("first prompt"));
    let used = rows(&render_offscreen(&mut screen, 200, 40).expect("infallible")).join("\n");
    assert!(
        !used.contains(WELCOME_ONLY),
        "the welcome surface survived the first message:\n{used}"
    );
    assert!(used.contains("first prompt"), "{used}");
}

#[test]
fn session_screen_can_open_an_empty_conversation_without_returning_to_the_welcome_page() {
    const WELCOME_ONLY: &str = "type / for commands";
    let (sender, _shutdown) = terminal_event_channel();
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).without_welcome();

    let rendered = rows(&render_offscreen(&mut screen, 120, 32).expect("infallible")).join("\n");
    assert!(
        !rendered.contains(WELCOME_ONLY),
        "an in-app /new remount returned to the welcome page:\n{rendered}"
    );
    assert!(
        rendered.contains(PROMPT_PLACEHOLDER),
        "the empty conversation page has no composer:\n{rendered}"
    );
    assert!(
        !screen.transcript_mut().transcript().conversation_started(),
        "suppressing the welcome page must not fabricate a durable conversation"
    );
}

#[test]
fn session_screen_suppresses_late_provider_and_tool_events_after_cancellation_is_requested() {
    let (sender, _shutdown) = terminal_event_channel();
    let (cancels, mut cancelled) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_cancel_sink(cancels);
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("stop this turn"));
    screen.status.mark_running();

    for _ in 0..2 {
        screen.handle_action(
            action("session_interrupt"),
            &press(crossterm::event::KeyCode::Esc),
        );
    }
    assert_eq!(cancelled.try_recv(), Ok(()));

    screen.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
        step: 2,
        message_id: String::from("late"),
    }));
    screen.handle_event(&AppEvent::Engine(TurnEvent::Provider {
        step: 2,
        event: StreamEvent::TextDelta(String::from("late model output")),
    }));
    screen.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchStarted {
        step: 2,
        call_id: String::from("late-tool"),
        name: String::from("bash"),
        ui_intent: zuno_tool::ToolUiIntent::Generic,
    }));

    let stopping = rows(&render_offscreen(&mut screen, 100, 24).expect("infallible")).join("\n");
    assert!(!stopping.contains("late model output"), "{stopping}");
    assert!(!stopping.contains("late-tool"), "{stopping}");
    assert!(stopping.contains("interrupting…"), "{stopping}");

    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnInterrupted {
        assistant_message_id: None,
        steps: 2,
    }));
    let settled = rows(&render_offscreen(&mut screen, 100, 24).expect("infallible")).join("\n");
    assert!(!settled.contains("late model output"), "{settled}");
    assert!(!settled.contains("interrupting…"), "{settled}");
}

/// A screen wired to a pending-edit set, with the nudge receiver and the set itself.
///
/// The shutdown receiver comes back so the caller keeps it alive: a dropped one closes
/// the channel and the screen would then be reporting into nothing.
fn screen_with_edit_sink() -> (
    SessionScreen,
    crate::views::lsp::PendingEditReader,
    mpsc::Receiver<()>,
    mpsc::Receiver<crate::app::TerminalEvent>,
) {
    let (shutdown, keep) = mpsc::channel(4);
    let (wake, nudges) = mpsc::channel(1);
    let pending = crate::views::lsp::PendingEdits::new(wake);
    let reader = pending.reader();
    let screen = SessionScreen::new(ViewContext::defaults(), shutdown).with_edit_sink(pending);
    (screen, reader, nudges, keep)
}

#[test]
fn session_reports_the_files_a_finished_turn_wrote_and_no_others() {
    use zuno_engine::r#loop::TurnEvent;
    let (mut screen, reader, mut nudges, _keep) = screen_with_edit_sink();

    let dispatched = |name: &str, paths: &[&str], is_error: bool| {
        AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("c"),
            name: name.to_owned(),
            // Prose, as `apply_patch`'s really is: nothing may read a path out of it.
            title: String::from("Success. Updated the following files:"),
            output: String::new(),
            diff: None,
            written_paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            is_error,
        })
    };

    screen.handle_event(&dispatched("edit", &["src/lib.rs"], false));
    // A read wrote nothing, so it reports nothing: attributing a file's pre-existing
    // diagnostics to this turn would blame the user for somebody else's problem.
    screen.handle_event(&dispatched("read", &[], false));
    // A failed write changed nothing either, even though it names a path.
    screen.handle_event(&dispatched("write", &["src/failed.rs"], true));
    screen.handle_event(&dispatched("write", &["src/new.rs"], false));
    // The same file twice is one entry.
    screen.handle_event(&dispatched("edit", &["src/lib.rs"], false));
    assert!(
        nudges.try_recv().is_err(),
        "the set was handed over before the turn finished"
    );
    assert_eq!(reader.take().0, Vec::<String>::new());

    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));
    assert_eq!(nudges.try_recv(), Ok(()));
    assert_eq!(
        reader.take().0,
        vec![String::from("src/lib.rs"), String::from("src/new.rs")]
    );
}

#[test]
fn session_reports_every_file_one_multi_file_patch_wrote() {
    // The `apply_patch` shape, which is the only writing tool a GPT model is shown: one
    // call, several files, and a `title` that is a summary sentence rather than a path.
    // A host reading `title` would check a file called
    // `Success. Updated the following files:`, and `path.is_file()` would then drop it —
    // so the turn's real writes were silently never checked.
    use zuno_engine::r#loop::TurnEvent;
    let (mut screen, reader, mut nudges, _keep) = screen_with_edit_sink();
    screen.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c"),
        name: String::from("apply_patch"),
        title: String::from("Success. Updated the following files:\nM a.rs\nA b.rs"),
        output: String::from("Success. Updated the following files:\nM a.rs\nA b.rs"),
        diff: Some(String::from("@@ -1 +1 @@\n-old\n+new\n")),
        written_paths: vec![String::from("a.rs"), String::from("b.rs")],
        is_error: false,
    }));
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));

    assert_eq!(nudges.try_recv(), Ok(()));
    assert_eq!(
        reader.take().0,
        vec![String::from("a.rs"), String::from("b.rs")],
        "a multi-file patch must report every file it wrote, not one and not none"
    );
}

#[test]
fn session_sends_nothing_for_a_turn_that_wrote_nothing() {
    use zuno_engine::r#loop::TurnEvent;
    let (mut screen, reader, mut nudges, _keep) = screen_with_edit_sink();
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));
    assert!(nudges.try_recv().is_err());
    assert_eq!(reader.take().0, Vec::<String>::new());
}

#[test]
fn session_reports_an_interrupted_turns_writes_too() {
    // An aborted turn may already have written; the user still needs to know whether what
    // landed compiles.
    use zuno_engine::r#loop::TurnEvent;
    let (mut screen, reader, mut nudges, _keep) = screen_with_edit_sink();
    screen.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c"),
        name: String::from("edit"),
        title: String::from("src/lib.rs"),
        output: String::new(),
        diff: None,
        written_paths: vec![String::from("src/lib.rs")],
        is_error: false,
    }));
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnInterrupted {
        assistant_message_id: None,
        steps: 1,
    }));
    assert_eq!(nudges.try_recv(), Ok(()));
    assert_eq!(reader.take().0, vec![String::from("src/lib.rs")]);
}

#[test]
fn a_full_nudge_channel_never_loses_a_files_place_in_the_set() {
    // The dropped-batch defect. The checker awaits a language server's startup and then
    // its diagnostics per file, so several short turns finishing in a row leave the nudge
    // unconsumed. When the paths travelled *as* the message, the second `try_send` failed
    // with `Full` and the files unique to that batch were never checked again — the screen
    // kept showing the first turn's diagnostics, or none, and said nothing about it.
    use zuno_engine::r#loop::TurnEvent;
    let (mut screen, reader, mut nudges, _keep) = screen_with_edit_sink();
    let wrote = |path: &str| {
        AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("c"),
            name: String::from("apply_patch"),
            title: String::from("Success. Updated the following files:"),
            output: String::new(),
            diff: None,
            written_paths: vec![path.to_owned()],
            is_error: false,
        })
    };
    let finished = || {
        AppEvent::Engine(TurnEvent::TurnCompleted {
            assistant_message_id: String::from("msg"),
            steps: 1,
        })
    };

    // Three turns, no drain in between: the capacity-one channel is full from the first.
    for path in ["first.rs", "second.rs", "third.rs"] {
        screen.handle_event(&wrote(path));
        screen.handle_event(&finished());
    }

    assert_eq!(nudges.try_recv(), Ok(()), "the first nudge is queued");
    assert!(
        nudges.try_recv().is_err(),
        "the later nudges coalesced into the queued one, which is the point"
    );
    assert_eq!(
        reader.take().0,
        vec![
            String::from("first.rs"),
            String::from("second.rs"),
            String::from("third.rs")
        ],
        "a coalesced nudge must still hand over every file, including the two turns \
         whose nudge found the channel full"
    );
}

#[test]
fn the_pending_edit_set_stops_growing_at_its_bound_and_counts_what_it_refused() {
    // Fed through the screen rather than by calling `merge` directly: the set is durable
    // across turns, so its bound is what stops a long session from growing without limit,
    // and the path that fills it in production is a completed turn.
    use zuno_engine::r#loop::TurnEvent;
    let (mut screen, reader, mut _nudges, _keep) = screen_with_edit_sink();
    let limit = crate::views::lsp::PENDING_EDIT_LIMIT;
    screen.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c"),
        name: String::from("apply_patch"),
        title: String::from("Success. Updated the following files:"),
        output: String::new(),
        diff: None,
        written_paths: (0..limit + 5)
            .map(|index| format!("file{index}.rs"))
            .collect(),
        is_error: false,
    }));
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg"),
        steps: 1,
    }));

    let (files, overflowed) = reader.take();
    assert_eq!(files.len(), limit, "the set grew past its bound");
    assert_eq!(
        overflowed, 5,
        "a refused path must be counted, so the report can say the set was truncated"
    );
}

#[test]
fn session_puts_an_arriving_report_in_the_transcript_and_on_the_strip() {
    let (shutdown, _keep) = mpsc::channel(4);
    let (reports, receiver) = mpsc::channel(4);
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), shutdown).with_diagnostics_source(receiver);
    reports
        .try_send(crate::views::lsp::Report::checked(
            "src/lib.rs",
            "rust",
            vec![crate::views::lsp::Diagnostic {
                severity: crate::views::lsp::Severity::Error,
                line: 7,
                column: 2,
                source: Some(String::from("rustc")),
                message: String::from("mismatched types"),
            }],
        ))
        .expect("the inlet accepts a report");
    // Any event drains the inlet, for the reason the permission bridge pumps on every
    // event: a dropped nudge must not leave a verdict sitting in a channel.
    let result = screen.handle_event(&AppEvent::Terminal(TerminalEvent::Resize {
        width: 120,
        height: 40,
    }));
    assert!(result.redraw);

    let rendered = crate::app::render_offscreen(&mut screen, 120, 24).expect("an offscreen frame");
    let joined = crate::views::testkit::rows(&rendered).join("\n");
    assert!(joined.contains("src/lib.rs"), "{joined}");
    assert!(joined.contains("1 error"), "{joined}");
    assert!(joined.contains("mismatched types"), "{joined}");
    assert!(
        joined.contains("7:2"),
        "the position is missing, so the row cannot be acted on: {joined}"
    );
}

#[test]
fn session_diagnostics_survive_the_end_of_the_turn_that_produced_them() {
    // The verdict describes the working tree, not the turn. Clearing it at a turn boundary
    // would hide something that is still true.
    use zuno_engine::r#loop::TurnEvent;
    let (shutdown, _keep) = mpsc::channel(4);
    let (reports, receiver) = mpsc::channel(4);
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), shutdown).with_diagnostics_source(receiver);
    reports
        .try_send(crate::views::lsp::Report::checked(
            "src/lib.rs",
            "rust",
            vec![crate::views::lsp::Diagnostic {
                severity: crate::views::lsp::Severity::Error,
                line: 1,
                column: 1,
                source: None,
                message: String::from("boom"),
            }],
        ))
        .expect("the inlet accepts a report");
    screen.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));
    let rendered = crate::app::render_offscreen(&mut screen, 160, 24).expect("an offscreen frame");
    let joined = crate::views::testkit::rows(&rendered).join("\n");
    assert!(
        joined.contains("src/lib.rs") && joined.contains("boom"),
        "the verdict was cleared when the turn ended:\n{joined}"
    );
}

/// Turn one chord spelling into the key event a terminal would send.
///
/// Function keys go through `Chord::parse`'s own spelling so `f1` cannot silently be
/// read as the character `f`, which is the shape of mistake that makes a binding test
/// pass while the key does nothing.
fn key_event_for(spelling: &str) -> KeyEvent {
    let chord = crate::keybind::Chord::parse(spelling).expect("a valid spelling");
    let tail = spelling.rsplit('+').next().unwrap_or(spelling);
    let code = match tail {
        "return" => crossterm::event::KeyCode::Enter,
        "escape" => crossterm::event::KeyCode::Esc,
        "space" => crossterm::event::KeyCode::Char(' '),
        "up" => crossterm::event::KeyCode::Up,
        "down" => crossterm::event::KeyCode::Down,
        other => match other
            .strip_prefix('f')
            .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
            .and_then(|rest| rest.parse::<u8>().ok())
        {
            Some(index) => crossterm::event::KeyCode::F(index),
            None => crossterm::event::KeyCode::Char(
                other.chars().next().expect("a non-empty chord tail"),
            ),
        },
    };
    let mut modifiers = crossterm::event::KeyModifiers::NONE;
    let rendered = chord.to_string();
    if rendered.contains("ctrl+") {
        modifiers |= crossterm::event::KeyModifiers::CONTROL;
    }
    if rendered.contains("alt+") {
        modifiers |= crossterm::event::KeyModifiers::ALT;
    }
    if rendered.contains("shift+") {
        modifiers |= crossterm::event::KeyModifiers::SHIFT;
    }
    KeyEvent {
        code,
        modifiers,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

/// Send every chord of `spelling` through `dispatcher`, returning the last result.
fn dispatch_sequence(
    dispatcher: &mut KeyDispatcher,
    spelling: &str,
    leader: crate::keybind::Chord,
) -> crate::app::EventResult {
    let sequence = crate::keybind::parse_sequence(spelling, leader).expect("a valid spelling");
    let mut last = crate::app::EventResult::IGNORED;
    for chord in sequence {
        last = dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(key_event_for(&chord.to_string())),
        )));
    }
    last
}

/// Resolve one spelling through the same focused-plus-static chain as `KeyDispatcher`,
/// while retaining the screen so a test can inspect the editor state afterwards.
fn dispatch_to_screen(
    screen: &mut SessionScreen,
    spelling: &str,
) -> (&'static str, crate::app::EventResult) {
    let mut keymap = Keymap::defaults().expect("the shipped table builds");
    let sequence = crate::keybind::parse_sequence(spelling, keymap.leader())
        .expect("a valid shipped spelling");
    let owned = scopes();
    let mut resolution = crate::keybind::Resolution::Unmatched;
    let mut event = press_none();
    for chord in sequence {
        let mut chain = ActionComponent::focused_scopes(screen);
        chain.extend(owned.iter().map(String::as_str));
        event = key_event_for(&chord.to_string());
        resolution = keymap.resolve(&chain, chord, std::time::Instant::now());
    }
    let crate::keybind::Resolution::Action { definition, .. } = resolution else {
        panic!("`{spelling}` did not resolve through the session scope chain");
    };
    (definition.name, screen.handle_action(definition, &event))
}

#[test]
fn session_up_on_an_empty_prompt_recalls_the_newest_persisted_prompt() {
    let (sender, _shutdown) = terminal_event_channel();
    let (records, _recorded) = mpsc::channel(1);
    let screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_history(vec![String::from("persisted across restart")], records);
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(screen));

    let result = dispatch_sequence(&mut dispatcher, "up", leader);

    assert!(result.redraw, "Up did not change the restarted prompt");
    let joined = rows(&render_offscreen(&mut dispatcher, 80, 12).expect("infallible")).join("\n");
    assert!(
        joined.contains("persisted across restart"),
        "Up left persisted history unreachable after restart:\n{joined}"
    );
}

#[test]
fn session_up_on_an_empty_prompt_scrolls_a_long_transcript() {
    let (mut screen, _shutdown) = scrollable(scroll_config(None, None));
    let bottom = screen.transcript.content_height() - screen.transcript.viewport_height();
    assert!(bottom > 0, "fixture is not scrollable");
    screen.transcript.set_offset(bottom);

    let (resolved, result) = dispatch_to_screen(&mut screen, "up");

    assert_eq!(resolved, "messages_line_up");
    assert!(result.redraw);
    assert_eq!(screen.transcript.offset(), bottom - 1);
    assert!(screen.editor.is_empty());
}

#[test]
fn session_up_on_line_three_of_a_multi_line_prompt_moves_the_cursor() {
    let (mut screen, _shutdown) = screen();
    let pasted = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight";
    screen.editor.set_text(pasted);
    screen.editor.handle_action(action("input_buffer_home"));
    screen.editor.handle_action(action("input_move_down"));
    screen.editor.handle_action(action("input_move_down"));
    assert_eq!(screen.editor.cursor(), Position { line: 2, column: 0 });

    let (resolved, result) = dispatch_to_screen(&mut screen, "up");

    assert_eq!(resolved, "input_move_up");
    assert!(result.redraw);
    assert_eq!(screen.editor.cursor(), Position { line: 1, column: 0 });
    assert_eq!(screen.editor.text(), pasted);
}

#[test]
fn session_up_on_the_first_line_of_a_multi_line_prompt_walks_history() {
    let (sender, _shutdown) = terminal_event_channel();
    let (records, _recorded) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_history(vec![String::from("remembered prompt")], records);
    screen.editor.set_text("draft one\ndraft two\ndraft three");
    screen.editor.handle_action(action("input_buffer_home"));

    let (resolved, result) = dispatch_to_screen(&mut screen, "up");

    assert_eq!(resolved, "history_previous");
    assert!(result.redraw);
    assert_eq!(screen.editor.text(), "remembered prompt");
}

#[test]
fn session_down_above_the_last_line_moves_within_the_multi_line_prompt() {
    let (mut screen, _shutdown) = screen();
    screen.editor.set_text("one\ntwo\nthree");
    screen.editor.handle_action(action("input_buffer_home"));

    let (resolved, result) = dispatch_to_screen(&mut screen, "down");

    assert_eq!(resolved, "history_next");
    assert!(result.redraw);
    assert_eq!(screen.editor.cursor(), Position { line: 1, column: 0 });
    assert_eq!(screen.editor.text(), "one\ntwo\nthree");
}

#[test]
fn session_down_past_the_newest_history_entry_restores_the_draft() {
    let (sender, _shutdown) = terminal_event_channel();
    let (records, _recorded) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_prompt_history(vec![String::from("remembered prompt")], records);
    let draft = "draft one\ndraft two\ndraft three";
    screen.editor.set_text(draft);
    screen.editor.handle_action(action("input_buffer_home"));
    let _recalled = dispatch_to_screen(&mut screen, "up");
    assert_eq!(screen.editor.text(), "remembered prompt");

    let (resolved, result) = dispatch_to_screen(&mut screen, "down");

    assert_eq!(resolved, "history_next");
    assert!(result.redraw);
    assert_eq!(
        screen.editor.text(),
        draft,
        "the in-progress draft was silently destroyed by a history round trip"
    );
}

/// A screen with every catalog and ambient list the six bound surfaces read from.
fn furnished_screen() -> SessionScreen {
    let (sender, _receiver) = terminal_event_channel();
    let (mcp_toggles, _mcp_requests) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_mcp_control(
            crate::views::picker::McpProjection::new(vec![crate::views::picker::McpServer {
                name: "context7".to_owned(),
                state: crate::views::picker::McpState::Connected,
                desired_enabled: true,
            }]),
            mcp_toggles,
        )
        .with_keymap(Keymap::defaults().expect("the shipped table builds"));
    *screen.catalog_mut() = catalog();
    let ambient = screen.sidebar_mut().ambient_mut();
    ambient.skills = vec![crate::views::ambient::SkillSummary {
        name: String::from("codegraph"),
        source: String::from("/skills/codegraph/SKILL.md"),
        description: String::from("navigate an indexed codebase"),
        loaded: false,
    }];
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("edit something"));
    // Started before Completed, which is the order a turn produces: the transcript
    // materialises the part on `ToolDispatchStarted` and only *updates* it on
    // `ToolDispatchCompleted`, so a fixture that sent the completion alone would be
    // silently dropped and the diff viewer would have nothing to open.
    screen
        .transcript_mut()
        .transcript_mut()
        .observe(&TurnEvent::ToolDispatchStarted {
            step: 1,
            call_id: String::from("call_1"),
            name: String::from("edit"),
            ui_intent: zuno_tool::ToolUiIntent::Generic,
        });
    screen
        .transcript_mut()
        .transcript_mut()
        .observe(&TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("call_1"),
            name: String::from("edit"),
            title: String::from("src/lib.rs"),
            output: String::from(
                "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line\n",
            ),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        });
    screen
}

#[test]
fn every_shipped_default_binding_resolves_in_the_screens_own_scope_chain() {
    // Half one of two: SCOPE. A binding is dead if its scope is missing from `scopes()`,
    // and this is the half that says so. It asserts through `Keymap::resolve` with the
    // real chain, because that is the function `KeyDispatcher` calls.
    //
    // Deliberately *not* asserted through the dispatcher's `EventResult`: an unresolved
    // chord falls through to the editor, which inserts the character and reports
    // `handled`, so a `handled` assertion here would pass with the scope list empty. That
    // is the shape of un-failable assertion this project keeps finding.
    let leader = Keymap::defaults()
        .expect("the shipped table builds")
        .leader();
    let owned = scopes();
    let chain = owned.iter().map(String::as_str).collect::<Vec<_>>();

    for (action_name, spelling) in crate::keybind::SHIPPED_DEFAULTS {
        let mut keymap = Keymap::defaults().expect("the shipped table builds");
        let sequence = crate::keybind::parse_sequence(spelling, leader).expect("a valid spelling");
        let now = std::time::Instant::now();
        let mut resolution = crate::keybind::Resolution::Unmatched;
        for chord in sequence {
            resolution = keymap.resolve(&chain, chord, now);
        }
        match resolution {
            crate::keybind::Resolution::Action { definition, .. } => assert_eq!(
                definition.name, *action_name,
                "`{spelling}` resolved to `{}` instead of `{action_name}`",
                definition.name
            ),
            other => panic!(
                "`{action_name}` is bound to `{spelling}` but the screen's scope chain does \
                 not resolve it ({other:?}); its scope is missing from `scopes()`"
            ),
        }
    }
}

#[test]
fn every_shipped_default_binding_is_routed_by_the_screen() {
    // Half two of two: ROUTING. A binding is equally dead if the scope is listed and
    // nothing acts on the action, so each one is asserted to be *consumed* by the screen.
    // `handle_action` is the right layer for this half only because the previous test
    // already proved the key reaches it.
    for (action_name, _) in crate::keybind::SHIPPED_DEFAULTS {
        let mut screen = furnished_screen();
        let result = screen.handle_action(action(action_name), &press_none());
        assert!(
            result.handled,
            "`{action_name}` resolves from a real chord but no arm in \
             `SessionScreen::handle_view_action` acts on it"
        );
    }
}

#[test]
fn every_shipped_default_binding_puts_something_on_the_screen() {
    // `handled` alone would be satisfied by an arm that consumed the key and did nothing,
    // so each surface is also asserted to render its own text. The four that open a modal
    // are proven by the dialog's title; the two toggles are proven by the transcript
    // still rendering, since a toggle has no title of its own.
    let leader = Keymap::defaults()
        .expect("the shipped table builds")
        .leader();
    let expected = [
        ("help_show", "Keybindings"),
        ("diff_open", "Diff"),
        ("prompt_skills", "Skills"),
        ("mcp_list", "MCP servers"),
    ];

    for (action_name, needle) in expected {
        let spelling = crate::keybind::SHIPPED_DEFAULTS
            .iter()
            .find(|(name, _)| *name == action_name)
            .map(|(_, spelling)| *spelling)
            .expect("the action has a shipped default");
        let keymap = Keymap::defaults().expect("the shipped table builds");
        let screen = furnished_screen();
        let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
        let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(host));

        dispatch_sequence(&mut dispatcher, spelling, leader);
        let joined =
            rows(&render_offscreen(&mut dispatcher, 100, 24).expect("infallible")).join("\n");
        assert!(
            joined.contains(needle),
            "`{action_name}` on `{spelling}` did not put `{needle}` on the screen:\n{joined}"
        );
    }
}

#[test]
fn the_two_diagnostics_panels_open_from_the_keys_the_table_binds_them_to() {
    // The counterpart of the test above for the two `§8.7` panels, which are bound by
    // `DEFINITIONS` rather than by `SHIPPED_DEFAULTS` and so are outside its list. This is
    // the whole-chain proof: a real chord through a real `KeyDispatcher` over the real
    // scope list, asserted on the frame. `status_view` was reachable in the binding table
    // and in `handle_view_action` long before its scope was registered — that gap is what
    // left `editor_open` dead, and only a test shaped like this one sees it.
    // Only the bound one is asserted here. `debug_view` ships with no key — upstream's
    // choice, and the table is not editable to align with a newer upstream — so its route
    // is the palette and it has a test of its own below. Pressing a null key at it here
    // would assert nothing while looking like coverage.
    let leader = Keymap::defaults()
        .expect("the shipped table builds")
        .leader();
    let spelling = crate::keybind::definition("status_view")
        .expect("the action is in the shipped table")
        .keys;
    assert_ne!(
        spelling,
        crate::keybind::NO_KEY,
        "`status_view` lost its key, so this test is no longer exercising a chord"
    );
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let screen = furnished_screen();
    let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(host));

    dispatch_sequence(&mut dispatcher, spelling, leader);
    // Below `SIDEBAR_MIN_WIDTH`, so the ambient panel is not drawn at all. At 130 it is,
    // and `render` copies the same `McpProjection` into `ambient.mcp` every frame — so a
    // frame-wide `contains("context7")` passed off the *sidebar* whether or not the census
    // read anything. Removing the census's live MCP read left this test green, which is
    // the vacuous-needle failure `views_tests` warns about: the row under test was owned
    // by another surface.
    let drawn = rows(&render_offscreen(&mut dispatcher, 118, 24).expect("infallible"));
    let joined = drawn.join("\n");
    assert!(
        joined.contains("Status"),
        "`status_view` on `{spelling}` did not open the census:\n{joined}"
    );
    // `MCP servers` is the census's own heading; the sidebar's is `MCP`. Located
    // positionally so this proves the member was grouped under it rather than merely
    // present somewhere on the frame.
    let heading = drawn
        .iter()
        .position(|row| row.contains("MCP servers"))
        .unwrap_or_else(|| panic!("the census has no MCP group:\n{joined}"));
    assert!(
        drawn
            .get(heading + 1)
            .is_some_and(|row| row.contains("context7")),
        "the live MCP server is not the first row under the census's own heading, so the \
         census is not reading the projection the MCP dialog reads:\n{joined}"
    );
}

#[test]
fn the_debug_panel_is_reachable_from_the_palette_because_it_ships_unbound() {
    // `debug_view` has no key in the shipped table, so the palette is its only route. The
    // palette re-enters `dispatch_action` with the chosen action's name, which is exactly
    // the path a user takes; asserting on the frame proves the panel opened rather than
    // that the action was merely accepted.
    let mut screen = furnished_screen();
    screen.set_diagnostics(
        Vec::new(),
        crate::views::diagnostics::DebugFacts {
            session: Some(String::from("ses_probe")),
            ..crate::views::diagnostics::DebugFacts::default()
        },
    );
    let mut host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    host.handle_action(
        crate::views::testkit::action("debug_view"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Null),
    );
    let joined = rows(&render_offscreen(&mut host, 130, 24).expect("infallible")).join("\n");
    assert!(
        joined.contains("Debug") && joined.contains("ses_probe"),
        "the debug report did not open with its facts on screen:\n{joined}"
    );
}

#[test]
fn copying_the_debug_report_reaches_the_clipboard_and_says_so() {
    // `§8.7`'s "Enter 复制全部并 toast", end to end from the panel's own key through the
    // screen's clipboard seam. A dialog that emitted an outcome nobody routed would look
    // identical from inside the panel's own tests.
    let (sender, _receiver) = terminal_event_channel();
    let clipboard = Arc::new(crate::views::external::MemoryClipboard::default());
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_clipboard(Arc::clone(&clipboard) as Arc<dyn Clipboard>);
    screen.set_diagnostics(
        Vec::new(),
        crate::views::diagnostics::DebugFacts {
            version: Some(String::from("9.9.9")),
            session: Some(String::from("ses_copy")),
            ..crate::views::diagnostics::DebugFacts::default()
        },
    );
    let mut host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    host.handle_action(
        crate::views::testkit::action("debug_view"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Null),
    );
    host.handle_action(
        crate::views::testkit::action("dialog.select.submit"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Null),
    );

    let copied = clipboard
        .read()
        .expect("a memory clipboard cannot fail")
        .expect("the report never reached the clipboard");
    assert!(
        !copied.is_image(),
        "the report was copied as an image rather than as text"
    );
    assert!(
        copied.data.contains("version: 9.9.9") && copied.data.contains("session: ses_copy"),
        "the clipboard holds something other than the report:\n{}",
        copied.data
    );
    assert!(
        host.is_open(),
        "copying closed the report, so the fields cannot be read after copying them"
    );
    let raised = host.toasts_mut().current().is_some();
    assert!(
        raised,
        "the copy landed but nothing on screen said so, which teaches the user the key \
         is broken"
    );
}

#[test]
fn the_two_display_toggles_change_what_the_transcript_renders() {
    // The toggles have no modal to name them, so they are proven by their effect: tool
    // output visible, then hidden. Without this a toggle bound to a key that resolved and
    // flipped nothing would satisfy the reachability test above.
    let leader = Keymap::defaults()
        .expect("the shipped table builds")
        .leader();
    let spelling = crate::keybind::SHIPPED_DEFAULTS
        .iter()
        .find(|(name, _)| *name == "tool_details")
        .map(|(_, spelling)| *spelling)
        .expect("tool_details has a shipped default");
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(furnished_screen()));

    let before = rows(&render_offscreen(&mut dispatcher, 100, 24).expect("infallible")).join("\n");
    dispatch_sequence(&mut dispatcher, spelling, leader);
    let after = rows(&render_offscreen(&mut dispatcher, 100, 24).expect("infallible")).join("\n");
    assert_ne!(
        before, after,
        "`tool_details` on `{spelling}` resolved but changed nothing on screen"
    );
}

#[test]
fn exposing_the_diff_scope_did_not_stop_its_bare_letters_being_typed() {
    // `scopes()` lists `diff`, whose viewer owns bare `q`, `n`, `p`, `d`, `v`, `s`, `b`,
    // `[`, `]`, `?` and `E` — eleven, not the nine the `scopes()` comment used to name.
    // Those resolve on the session screen whether or not the viewer is open,
    // so typing survives only because this screen returns `IGNORED` for them and an
    // unhandled action falls through to the editor. Give the screen an arm for one of
    // those characters and this test is what says the prompt stopped accepting it.
    //
    // `DiffDialog` *does* have arms for seven of them, and that is safe for a reason this
    // test cannot see: a dialog is only offered an action while it sits on
    // `DialogHost`'s stack. With the stack empty the same action arrives at this screen
    // instead, which is the case under test.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let (sender, _receiver) = terminal_event_channel();
    let screen = SessionScreen::new(ViewContext::defaults(), sender);
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(screen));

    let typed = "qnpdvsb[]?E";
    for character in typed.chars() {
        dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(crate::views::testkit::press(
                crossterm::event::KeyCode::Char(character),
            )),
        )));
    }

    let joined = rows(&render_offscreen(&mut dispatcher, 100, 12).expect("infallible")).join("\n");
    assert!(
        joined.contains(typed),
        "the diff scope's bare characters stopped reaching the prompt; `{typed}` is not \
         on screen:\n{joined}"
    );

    // Derived, not hand-maintained: a bare character added to the `diff` scope later is
    // reachable from the prompt the moment the row lands, and the list above would
    // otherwise silently stop covering it. `[` and `]` were in the table and missing from
    // this test for exactly that reason.
    let bare = crate::keybind::DEFINITIONS
        .iter()
        .filter(|definition| definition.scope == "diff")
        .flat_map(|definition| definition.keys.split(','))
        .filter(|spelling| spelling.chars().count() == 1)
        .filter_map(|spelling| spelling.chars().next())
        .collect::<std::collections::BTreeSet<_>>();
    let uncovered = bare
        .iter()
        .filter(|character| !typed.contains(**character))
        .collect::<Vec<_>>();
    assert!(
        uncovered.is_empty(),
        "the `diff` scope binds {uncovered:?} to a bare character that this test never \
         types, so nothing proves the prompt still accepts it"
    );
}

/// The shortest description among the leader's continuations in the production chain.
///
/// Derived rather than named: a grid cell truncates a long description legitimately, so an
/// assertion must target one that fits at any cell width, and which row that is changes
/// whenever the table does.
fn shortest_leader_description() -> &'static str {
    let mut keymap = Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let chain = scopes();
    let chain = chain.iter().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(
        keymap.resolve(&chain, leader, std::time::Instant::now()),
        crate::keybind::Resolution::Pending
    );
    let mut all = keymap.continuations(&chain);
    assert!(all.len() >= 20, "only {} continuations", all.len());
    all.sort_by_key(|entry| crate::views::display_width(entry.definition.description));
    all[0].definition.description
}

#[test]
fn session_pressing_the_leader_paints_a_which_key_panel_end_to_end() {
    // The reachability bar, and the reason it is asserted here rather than on the view:
    // `WhichKeyView` was complete, tested and had zero production construction sites for
    // as long as it existed. This drives one real `ctrl+x` through `KeyDispatcher` into
    // the same `DialogHost`-over-`SessionScreen` stack `cmd/tui.rs` builds, then reads
    // the frame. Nothing short of that distinguishes "wired" from "built".
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let (sender, _shutdown) = terminal_event_channel();
    let screen = SessionScreen::new(ViewContext::defaults(), sender);
    let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(host));

    let before = rows(&render_offscreen(&mut dispatcher, 100, 14).expect("infallible")).join("\n");
    dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        crossterm::event::Event::Key(key_event_for(&leader.to_string())),
    )));
    let after = rows(&render_offscreen(&mut dispatcher, 100, 14).expect("infallible")).join("\n");

    let expected = crate::keybind::Keymap::defaults()
        .expect("builds")
        .continuations(&["session"]);
    assert!(
        expected.is_empty(),
        "continuations must be empty with nothing pending, or this test proves nothing"
    );
    assert_ne!(
        before, after,
        "one leader press changed no cell, so the panel is not mounted in the production \
         stack:\n{after}"
    );
    // A description from the table, so this cannot pass on incidental chrome.
    let needle = shortest_leader_description();
    assert!(
        after.contains(needle),
        "`{leader}` did not paint the leader's continuations; `{needle}` is absent:\n{after}"
    );
}

#[test]
fn session_an_abandoned_leader_sequence_takes_its_panel_with_it() {
    // The `Unmatched` branch. Before it reported an inactive prefix, the panel opened by
    // `ctrl+x` stayed on screen over every later keystroke — a stuck overlay reporting
    // continuations for a sequence that had already been abandoned.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let (sender, _shutdown) = terminal_event_channel();
    let screen = SessionScreen::new(ViewContext::defaults(), sender);
    let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(host));

    dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        crossterm::event::Event::Key(key_event_for(&leader.to_string())),
    )));
    let needle = shortest_leader_description();
    let open = rows(&render_offscreen(&mut dispatcher, 100, 14).expect("infallible")).join("\n");
    assert!(open.contains(needle), "{open}");

    // `~` begins no sequence in any registered scope, so the leader is abandoned.
    dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        crossterm::event::Event::Key(crate::views::testkit::press(
            crossterm::event::KeyCode::Char('~'),
        )),
    )));
    let closed = rows(&render_offscreen(&mut dispatcher, 100, 14).expect("infallible")).join("\n");
    assert!(
        !closed.contains(needle),
        "the panel outlived the sequence it was explaining:\n{closed}"
    );
    // And the key that abandoned it still reached the prompt. Notifying the panel from
    // this branch made the result `handled` for one revision, which swallowed exactly
    // this character — the panel closed correctly and the keystroke vanished.
    assert!(
        closed.contains('~'),
        "the key that abandoned the sequence was consumed instead of typed:\n{closed}"
    );
}

#[test]
fn session_completing_a_leader_sequence_closes_the_panel() {
    // The `Action` branch, which is the common case: `ctrl+x` then a bound key. The panel
    // must be gone by the frame that shows the action's own effect.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let (sender, _shutdown) = terminal_event_channel();
    let screen = SessionScreen::new(ViewContext::defaults(), sender);
    let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(host));

    dispatch_sequence(&mut dispatcher, "<leader>s", leader);
    let after = rows(&render_offscreen(&mut dispatcher, 100, 14).expect("infallible")).join("\n");
    assert!(
        !after.contains(shortest_leader_description()),
        "the which-key panel survived a completed sequence:\n{after}"
    );
}

#[test]
fn session_the_leader_panel_does_not_stop_its_continuation_keys_being_typed() {
    // The diff-scope precedent. Every continuation the panel advertises is reached
    // through the leader, so none of them is a bare binding and the prompt must still
    // accept each as text. Derived from the table, because a hand-kept list stops
    // covering the row added after it.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let mut probe = Keymap::defaults().expect("builds");
    assert_eq!(
        probe.resolve(
            &scopes().iter().map(String::as_str).collect::<Vec<_>>(),
            leader,
            std::time::Instant::now()
        ),
        crate::keybind::Resolution::Pending
    );
    let single = probe
        .continuations(&scopes().iter().map(String::as_str).collect::<Vec<_>>())
        .into_iter()
        .filter(|entry| entry.keys.chars().count() == 1)
        .filter_map(|entry| entry.keys.chars().next())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        single.len() >= 10,
        "only {} single-character continuations; this guard is measuring nothing",
        single.len()
    );

    let (sender, _receiver) = terminal_event_channel();
    let screen = SessionScreen::new(ViewContext::defaults(), sender);
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(screen));
    let typed = single.iter().copied().collect::<String>();
    for character in single {
        dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(crate::views::testkit::press(
                crossterm::event::KeyCode::Char(character),
            )),
        )));
    }
    let joined = rows(&render_offscreen(&mut dispatcher, 100, 12).expect("infallible")).join("\n");
    assert!(
        joined.contains(&typed),
        "a leader continuation's letter stopped reaching the prompt; `{typed}` is not on \
         screen:\n{joined}"
    );
}

#[test]
fn every_action_the_screen_consumes_lives_in_a_scope_it_resolves() {
    // The hole the two guards above leave. Both derive their set from a hand-kept list —
    // `SHIPPED_DEFAULTS` there, `HINTS` in
    // `views_welcome_every_advertised_action_lives_in_a_scope_the_screen_resolves` — so an
    // action that upstream's table *already* spells and the welcome grid does not advertise
    // is covered by neither. `editor_open` was exactly that: `<leader>e` in the table, an
    // arm in `handle_action`, `editor` missing from `scopes()`. On a real terminal
    // `ctrl+x e` therefore left `beforee` in the prompt — the chord never resolved and the
    // unmatched `e` fell through and was typed.
    //
    // So the set is derived from the screen instead of from a list: whatever
    // `handle_action` consumes has to be reachable. A derived set cannot fall out of step
    // with a list nobody remembered to extend.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let static_scopes = scopes();
    let mut unreachable = Vec::new();
    for definition in crate::keybind::DEFINITIONS {
        // A row with no spelling is the palette's business, not this chain's. Asked of the
        // keymap rather than of `definition.keys`, so a spelling this build adds through
        // `SHIPPED_DEFAULTS` counts as pressable — which is what the running binary sees.
        let sequences = keymap.sequences(definition.name);
        if sequences.is_empty() {
            continue;
        }
        let consumed = reachability_screens()
            .into_iter()
            .any(|mut screen| screen.handle_action(definition, &press_none()).handled);
        if !consumed {
            continue;
        }
        let registered = static_scopes.iter().any(|scope| scope == definition.scope)
            || reachability_screens()
                .into_iter()
                .any(|screen| ActionComponent::focused_scopes(&screen).contains(&definition.scope));
        if !registered {
            unreachable.push(format!(
                "{} (`{}`) lives in unregistered scope `{}`",
                definition.name,
                sequences.join("` or `"),
                definition.scope
            ));
            continue;
        }

        // Exact-resolution is enforceable for editor actions because their action identity
        // is their behaviour. It is not enforceable for every screen action: `app_exit` is
        // intentionally reached through shadowing and compensated from the physical chord,
        // while unrelated legacy leader collisions also exist. Restricting the ordering half
        // to every action the editor consumes is the narrowest general rule that catches this
        // defect family without pretending those deliberate/non-L1 cases are equivalent.
        let editor_consumes = reachability_screens()
            .into_iter()
            .any(|mut screen| screen.editor.handle_action(definition) != EditorSignal::None);
        if editor_consumes
            && !editor_action_resolves_in_a_reachable_screen_state(definition.name, &sequences)
        {
            unreachable.push(format!(
                "{} (`{}`) in scope `{}` is shadowed in every reachable editor state",
                definition.name,
                sequences.join("` or `"),
                definition.scope
            ));
        }
    }
    assert!(
        unreachable.is_empty(),
        "the screen acts on these actions, but their scope is absent or an earlier scope \
         shadows every editor state:\n{}",
        unreachable.join("\n")
    );
}

/// Representative focus/editor states that can change the ordered scope chain.
///
/// The guard derives actions from the screen and states from every conditional branch in
/// `focused_scopes`; adding another branch requires adding its reachable state here. This is
/// what lets the assertion distinguish "scope listed" from "action can actually win" without
/// hard-coding the history action names into the guard.
fn reachability_screens() -> Vec<SessionScreen> {
    let mut boundary = furnished_screen();
    boundary
        .editor
        .load_history(vec![String::from("older"), String::from("newest")]);

    let mut in_history = furnished_screen();
    in_history
        .editor
        .load_history(vec![String::from("older"), String::from("newest")]);
    in_history.editor.handle_action(action("history_previous"));

    let mut interior = furnished_screen();
    interior.editor.set_text("first\nsecond\nthird");
    interior.editor.handle_action(action("input_move_up"));

    let mut autocomplete = furnished_screen();
    autocomplete.editor.set_text("/mo");
    autocomplete.refresh_autocomplete();

    vec![boundary, in_history, interior, autocomplete]
}

fn editor_action_resolves_in_a_reachable_screen_state(
    action_name: &str,
    sequences: &[String],
) -> bool {
    let owned = scopes();
    reachability_screens().into_iter().any(|screen| {
        let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
        let mut chain = ActionComponent::focused_scopes(&host);
        chain.extend(owned.iter().map(String::as_str));
        sequences.iter().any(|spelling| {
            let mut keymap = Keymap::defaults().expect("the shipped table builds");
            let sequence = crate::keybind::parse_sequence(spelling, keymap.leader())
                .expect("a spelling returned by Keymap parses");
            let mut resolution = crate::keybind::Resolution::Unmatched;
            let now = std::time::Instant::now();
            for chord in sequence {
                resolution = keymap.resolve(&chain, chord, now);
            }
            matches!(
                resolution,
                crate::keybind::Resolution::Action { definition, .. }
                    if definition.name == action_name
            )
        })
    })
}

#[test]
fn the_external_editor_chord_reaches_the_worker_channel() {
    // The strong form of the assertion above, for the one action it was written for.
    // Asserting `"editor"` is in `scopes()` would pass while any hop between the keymap
    // and the worker was broken; this presses the real chord through `KeyDispatcher` and
    // requires the request to arrive on the channel `drive_external_editor` reads, so it
    // only passes when the whole path works.
    //
    // The leader is the half that failed on a real terminal, so both presses go through
    // `handle_event`: `ctrl+x` alone only resolves to `Pending`, and it is the second
    // press that either resolves to `editor_open` or falls through and types `e`.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let spelling = keymap
        .sequences("editor_open")
        .into_iter()
        .next()
        .expect("`editor_open` is pressable in the shipped table");
    let (sender, _shutdown) = terminal_event_channel();
    let (requests, mut requested) = mpsc::channel(1);
    let (_results, result_source) = mpsc::channel(1);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_external_editor(requests, result_source);
    screen.editor.set_text("before");
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(screen));

    dispatch_sequence(&mut dispatcher, &spelling, leader);

    let request = requested.try_recv().unwrap_or_else(|error| {
        panic!("`{spelling}` reached no external editor request ({error}); `editor_open` is unreachable")
    });
    assert_eq!(
        request,
        EditorRequest::new("before"),
        "the request did not carry the prompt as typed"
    );
    // The symptom, asserted directly: an unresolved chord types its trailing key. `beforee`
    // on screen is what a user sees when this regresses, and it is the observation that
    // found the defect.
    let joined = rows(&render_offscreen(&mut dispatcher, 100, 12).expect("infallible")).join("\n");
    assert!(
        !joined.contains("beforee"),
        "`{spelling}` did not resolve; its trailing key was typed into the prompt:\n{joined}"
    );
}

#[test]
fn session_command_palette_opens_and_dispatches_an_unbound_action() {
    // The palette's reason to exist: forty-three rows of the binding table ship with
    // `keys: "none"`, and upstream's answer for reaching one is the palette. This asserts
    // the whole path — open it, choose a keyless action, and require that the action ran.
    let (screen, _shutdown) = screen();
    let mut screen = screen.with_keymap(Keymap::defaults().expect("the shipped table builds"));
    assert_eq!(
        screen.handle_action(action("command_list"), &press_none()),
        EventResult::REDRAW
    );
    let mut opened = screen.drain_dialogs();
    assert_eq!(opened.len(), 1, "the palette did not open");
    let palette = opened.pop().expect("one dialog");
    assert_eq!(palette.id(), crate::views::palette::DIALOG_ID);

    // `display_thinking` is what the palette dispatches here; whether the table also binds it
    // to a key is beside the point — applying the outcome must actually flip the affordance.
    let before = screen.transcript.thinking();
    let result = screen.apply_dialog_outcome(
        crate::views::palette::DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: crate::views::palette::DIALOG_ID,
            value: String::from("display_thinking"),
        },
    );
    assert!(result.redraw);
    assert_ne!(
        screen.transcript.thinking(),
        before,
        "a palette choice did not run the action it named"
    );
}

#[test]
fn session_command_palette_refuses_to_open_itself() {
    // Otherwise choosing "list available commands" inside the palette pushes a second one,
    // and every later choice leaves one behind.
    let (screen, _shutdown) = screen();
    let mut screen = screen.with_keymap(Keymap::defaults().expect("the shipped table builds"));
    screen.handle_action(action("command_list"), &press_none());
    let _opened = screen.drain_dialogs();
    let result = screen.apply_dialog_outcome(
        crate::views::palette::DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: crate::views::palette::DIALOG_ID,
            value: String::from("command_list"),
        },
    );
    assert!(!result.handled, "the palette re-opened itself");
    assert!(screen.drain_dialogs().is_empty());
}

#[test]
fn session_command_palette_dispatches_a_dialog_opener_into_a_dialog() {
    let (screen, _shutdown) = screen();
    let mut screen = screen.with_keymap(Keymap::defaults().expect("the shipped table builds"));
    screen.handle_action(action("command_list"), &press_none());
    let _opened = screen.drain_dialogs();
    screen.apply_dialog_outcome(
        crate::views::palette::DIALOG_ID,
        &crate::views::dialog::DialogOutcome::Selected {
            dialog: crate::views::palette::DIALOG_ID,
            value: String::from("help_show"),
        },
    );
    let opened = screen.drain_dialogs();
    assert_eq!(opened.len(), 1, "the palette's choice opened no dialog");
    assert_eq!(opened[0].id(), crate::views::help::DIALOG_ID);
}

#[test]
fn session_palette_needs_a_keymap_and_says_so_rather_than_guessing() {
    // Without a keymap there is no honest spelling to print, so the screen reports it in
    // the transcript instead of inventing one.
    let (mut screen, _shutdown) = screen();
    let result = screen.handle_action(action("command_list"), &press_none());
    assert!(result.redraw);
    assert!(
        screen.drain_dialogs().is_empty(),
        "a palette was built with no keymap to read spellings from"
    );
}

#[test]
fn session_every_action_the_palette_can_name_is_routed_or_harmlessly_ignored() {
    // The palette offers every non-leader action, so dispatching any of them must not
    // panic and must not be able to leave a dialog half-open. This is the blast-radius
    // assertion for making a third of the binding table reachable at once.
    for definition in crate::keybind::DEFINITIONS
        .iter()
        .filter(|row| !row.is_leader())
    {
        let (screen, _shutdown) = screen();
        let mut screen = screen.with_keymap(Keymap::defaults().expect("the shipped table builds"));
        let _ = screen.apply_dialog_outcome(
            crate::views::palette::DIALOG_ID,
            &crate::views::dialog::DialogOutcome::Selected {
                dialog: crate::views::palette::DIALOG_ID,
                value: definition.name.to_owned(),
            },
        );
        assert!(
            screen.drain_dialogs().len() <= 1,
            "`{}` opened more than one dialog",
            definition.name
        );
    }
}

#[test]
fn session_skill_picker_reports_an_empty_skill_set_rather_than_an_empty_list() {
    let (mut screen, _shutdown) = screen();
    screen.handle_action(action("prompt_skills"), &press_none());
    assert!(
        screen.drain_dialogs().is_empty(),
        "an empty skill set opened a picker saying `no matches`"
    );
}

#[test]
fn session_skill_picker_lists_discovered_skills_on_one_row_each() {
    let (mut screen, _shutdown) = screen();
    screen.sidebar_mut().ambient_mut().skills = vec![crate::views::ambient::SkillSummary {
        name: String::from("codegraph"),
        source: String::from("/skills/codegraph/SKILL.md"),
        description: String::from("navigate\n  a  codebase"),
        loaded: false,
    }];
    screen.handle_action(action("prompt_skills"), &press_none());
    let mut opened = screen.drain_dialogs();
    assert_eq!(opened.len(), 1, "the skill picker did not open");
    let mut picker = opened.pop().expect("one dialog");
    assert_eq!(picker.id(), crate::views::picker::SKILL_DIALOG_ID);
    let rendered = picker
        .lines(80)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("codegraph"), "{rendered}");
    assert!(
        rendered.contains("navigate a codebase"),
        "the description kept its newlines: {rendered}"
    );
}

/// The live footer stops pulsing while a permission prompt is mounted.
#[test]
fn session_stops_spinning_while_a_permission_prompt_is_mounted_over_it() {
    let (mut screen, _shutdown) = screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("edit something"));
    let context = ViewContext::defaults();
    let mut host = crate::views::dialog::DialogHost::new(context.clone(), Box::new(screen));
    host.handle_event(&AppEvent::Engine(TurnEvent::TurnStarted {
        session_id: String::from("s"),
    }));

    let busy = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        busy.contains('▰') && busy.contains("esc interrupt"),
        "a running turn with nothing outstanding must still pulse:\n{busy}"
    );

    host.open(Box::new(crate::views::permission::PermissionPrompt::new(
        context,
        zuno_permission::PermissionRequest {
            id: String::from("req_1"),
            session_id: String::from("s"),
            permission: String::from("edit"),
            patterns: vec![String::from("src/**")],
            metadata: serde_json::Map::new(),
            always: vec![String::from("src/**")],
            tool: None,
        },
        &serde_json::json!({"filePath": "src/main.rs"}),
    )));

    let waiting = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        !waiting.contains("esc interrupt"),
        "the pulse claimed the process was busy while the prompt asked the user to \
         decide:\n{waiting}"
    );
    assert!(
        waiting.contains("awaiting approval"),
        "nothing told the user they are the one being waited on:\n{waiting}"
    );

    assert!(
        host.dismiss(),
        "the prompt was mounted, so it can be closed"
    );
    let resumed = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        resumed.contains('▰') && resumed.contains("esc interrupt"),
        "the wait notice outlived the prompt that justified it:\n{resumed}"
    );
}

#[test]
fn session_reports_that_a_question_is_waiting_for_the_user() {
    let (mut screen, _shutdown) = screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("choose a delivery mode"));
    let context = ViewContext::defaults();
    let mut host = crate::views::dialog::DialogHost::new(context.clone(), Box::new(screen));
    host.handle_event(&AppEvent::Engine(TurnEvent::TurnStarted {
        session_id: String::from("s"),
    }));
    host.open(Box::new(crate::views::question::QuestionPrompt::new(
        context,
        vec![crate::views::question::QuestionRequest::new(
            "How should the result be delivered?",
            "Delivery",
            vec![crate::views::question::QuestionOption::new(
                "Next step",
                "Wake the parent on its next safe point",
            )],
        )],
    )));

    let waiting = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        !waiting.contains("esc interrupt"),
        "the pulse claimed the process was busy while the question waited for an answer:\n\
         {waiting}"
    );
    assert!(
        waiting.contains("awaiting answer"),
        "the running-state surface did not name what it needs from the user:\n{waiting}"
    );
}

/// A picker is opened *by* the user while work continues, so suppressing the spinner
/// behind one would claim the turn had stopped when it had not. Only a permission ask
/// means the turn is blocked on a human.
#[test]
fn session_keeps_spinning_behind_a_dialog_that_is_not_a_permission_ask() {
    let (mut screen, _shutdown) = screen();
    *screen.catalog_mut() = catalog();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("edit something"));
    let context = ViewContext::defaults();
    let mut host = crate::views::dialog::DialogHost::new(context, Box::new(screen));
    host.handle_event(&AppEvent::Engine(TurnEvent::TurnStarted {
        session_id: String::from("s"),
    }));
    host.handle_action(action("model_list"), &press_none());
    assert_eq!(
        host.active(),
        Some("model_list"),
        "the picker must actually be mounted, or this asserts nothing"
    );

    let joined = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        joined.contains('▰') && joined.contains("esc interrupt"),
        "a picker opened during a live turn is not the turn waiting on the user:\n{joined}"
    );
    assert!(!joined.contains("awaiting approval"), "{joined}");
}

// ---------------------------------------------------------------------------
// Live theme switching
// ---------------------------------------------------------------------------

/// A theme other than the default, for the switch to land on.
const OTHER_THEME: &str = "gruvbox";

/// A screen with a transcript, and the context it shares with every surface it built.
///
/// The context is returned rather than re-derived because that is the property under
/// test: a caller holding a clone sees what the screen's children paint with, so an
/// assertion on it is an assertion about the whole tree rather than about the dialog.
fn themed_screen() -> (SessionScreen, ViewContext, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = terminal_event_channel();
    let context = ViewContext::defaults();
    let mut screen = SessionScreen::new(context.clone(), sender)
        .with_keymap(Keymap::defaults().expect("the shipped table builds"));
    screen.transcript_mut().transcript_mut().push(Message::user(
        "a message, so the transcript owns the body area",
    ));
    (screen, context, receiver)
}

/// The theme picker, open over `screen`, driven through the real host.
fn opened_theme_picker(
    screen: SessionScreen,
    context: &ViewContext,
) -> crate::views::dialog::DialogHost {
    let mut host = crate::views::dialog::DialogHost::new(context.clone(), Box::new(screen));
    host.handle_action(action("theme_list"), &press_none());
    assert_eq!(
        host.active(),
        Some(crate::views::picker::THEME_DIALOG_ID),
        "the theme picker did not open"
    );
    host
}

/// Reopen the theme picker on a host whose base is already a session screen.
fn reopen_theme_picker(
    mut host: crate::views::dialog::DialogHost,
) -> crate::views::dialog::DialogHost {
    host.handle_action(action("theme_list"), &press_none());
    assert_eq!(
        host.active(),
        Some(crate::views::picker::THEME_DIALOG_ID),
        "the theme picker did not reopen"
    );
    host
}

/// Type `text` into the open dialog's filter, the way an unclaimed key reaches it.
fn filter_dialog(host: &mut crate::views::dialog::DialogHost, text: &str) {
    for character in text.chars() {
        host.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            CrosstermEvent::Key(crate::views::testkit::press(
                crossterm::event::KeyCode::Char(character),
            )),
        )));
    }
}

/// Submit the open dialog through the action `enter` resolves to.
fn submit_dialog(host: &mut crate::views::dialog::DialogHost) {
    host.handle_action(
        action("dialog.select.submit"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Enter),
    );
}

/// Cancel the open dialog through the action `escape` resolves to.
fn cancel_dialog(host: &mut crate::views::dialog::DialogHost) {
    host.handle_action(
        action("session_interrupt"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Esc),
    );
}

/// Every background colour on `row` of a freshly rendered frame.
///
/// A whole row rather than one cell, and 100 columns rather than 130, so the sidebar is
/// absent and the transcript owns the width. The row is read from the top of the frame
/// because the dialog is drawn at the *bottom*: a frame assertion in this crate has
/// already once passed vacuously by inspecting a row a dialog was covering, so the row
/// under test must be one the overlay cannot reach.
fn row_backgrounds(
    host: &mut crate::views::dialog::DialogHost,
    row: u16,
) -> Vec<ratatui::style::Color> {
    let buffer = render_offscreen(host, 100, 30).expect("infallible");
    assert!(
        buffer.area.height > row,
        "the frame is shorter than the row under test"
    );
    (0..buffer.area.width)
        .map(|column| {
            buffer[(column, row)]
                .style()
                .bg
                .expect("every cell is filled")
        })
        .collect()
}

/// A theme's panel background, as a ratatui colour.
fn panel_of(theme: &str, mode: crate::theme::Mode) -> ratatui::style::Color {
    ratatui::style::Color::from(
        crate::theme::ThemeRegistry::new()
            .resolve(theme, mode)
            .palette
            .background_panel,
    )
}

#[test]
fn session_theme_switch_repaints_the_transcript_and_not_only_the_picker() {
    // The requirement the shared-theme design exists for. Asserted on the transcript —
    // a surface built *before* the picker existed and living outside the dialog — because
    // a re-theme confined to the dialog is exactly the half-themed screen that would be
    // worse than no switching at all.
    let (screen, context, _shutdown) = themed_screen();
    let mode = context.theme().mode;
    let starting_panel = panel_of(&context.theme().name, mode);
    let other_panel = panel_of(OTHER_THEME, mode);
    assert_ne!(
        starting_panel, other_panel,
        "the two themes share a panel background, so this test could not tell them apart"
    );

    let mut host = opened_theme_picker(screen, &context);
    let before = row_backgrounds(&mut host, 0);
    assert!(
        before.contains(&starting_panel),
        "the transcript row was not painted in the starting theme, so the frame under \
         test is the wrong one: {before:?}"
    );

    filter_dialog(&mut host, OTHER_THEME);

    let after = row_backgrounds(&mut host, 0);
    assert_ne!(
        before, after,
        "the transcript kept its old colours while the picker previewed a new theme"
    );
    assert!(
        after.contains(&other_panel),
        "the transcript is not painted in the highlighted theme: {after:?}"
    );
    assert!(
        !after.contains(&starting_panel),
        "part of the transcript row still carries the old theme: {after:?}"
    );
}

#[test]
fn session_theme_switch_also_repaints_the_status_strip() {
    // A second surface outside the dialog, because a propagation bug that reached the
    // transcript by some other route would still be a bug.
    //
    // Read *after* the picker closes, not while it is open. A 13-row theme picker drawn
    // at the bottom of a 30-row frame covers rows 17 onwards, and the strip sits at row
    // 27 — so the open-dialog version of this test inspected the dialog's own footer and
    // reported "the row under test is not the status strip". That is the vacuous frame
    // assertion this crate has hit before, caught here by asserting the starting colour
    // first instead of only asserting that something changed.
    let (screen, context, _shutdown) = themed_screen();
    let mode = context.theme().mode;
    let element = |theme: &str| {
        ratatui::style::Color::from(
            crate::theme::ThemeRegistry::new()
                .resolve(theme, mode)
                .palette
                .background_element,
        )
    };
    let starting = element(&context.theme().name);
    let other = element(OTHER_THEME);
    assert_ne!(
        starting, other,
        "the two themes share an element background"
    );

    let mut host = crate::views::dialog::DialogHost::new(context.clone(), Box::new(screen));
    // The strip sits directly above the prompt. Derived from `prompt_rows` rather than
    // restated as a literal: this test is about a *repaint*, not about the prompt's height,
    // and the literal that used to be here silently became "some transcript row" the moment
    // the prompt's floor changed — which reports "the row under test is not the status
    // strip" about a frame that has one. The transcript is non-empty here, so there is no
    // welcome tail below the prompt to account for.
    let strip_row = 30 - 1 - prompt_rows(0, 30);
    let before = row_backgrounds(&mut host, strip_row);
    assert!(
        before.contains(&starting),
        "the row under test is not the status strip: {before:?}"
    );

    host.handle_action(action("theme_list"), &press_none());
    filter_dialog(&mut host, OTHER_THEME);
    submit_dialog(&mut host);
    assert!(!host.is_open(), "the picker is still covering the strip");

    let after = row_backgrounds(&mut host, strip_row);
    assert!(
        after.contains(&other) && !after.contains(&starting),
        "the status strip did not follow the theme: {after:?}"
    );
}

#[test]
fn session_theme_picker_escape_restores_the_theme_it_opened_over() {
    let (screen, context, _shutdown) = themed_screen();
    let original = context.theme();
    let mut host = opened_theme_picker(screen, &context);

    filter_dialog(&mut host, OTHER_THEME);
    assert_eq!(
        context.theme().name,
        OTHER_THEME,
        "the preview never applied, so cancelling has nothing to undo"
    );

    cancel_dialog(&mut host);

    assert!(!host.is_open(), "escape did not close the picker");
    assert_eq!(
        context.theme().name,
        original.name,
        "cancelling left the user in a theme they only scrolled past"
    );
    assert_eq!(
        context.palette().background_panel,
        original.palette.background_panel,
        "the name came back but the colours did not"
    );
}

#[test]
fn session_theme_picker_enter_commits_and_the_theme_survives_the_dialog_closing() {
    let (screen, context, _shutdown) = themed_screen();
    let mut host = opened_theme_picker(screen, &context);
    filter_dialog(&mut host, OTHER_THEME);

    submit_dialog(&mut host);

    assert!(!host.is_open(), "enter did not close the picker");
    assert_eq!(
        context.theme().name,
        OTHER_THEME,
        "the committed theme was dropped when the dialog closed"
    );

    // A commit must also survive whatever the *next* picker does, which is the failure a
    // restore point left behind would cause: escape out of any later theme picker and the
    // earlier commit would be silently undone.
    let mut reopened = reopen_theme_picker(host);
    cancel_dialog(&mut reopened);
    assert_eq!(
        context.theme().name,
        OTHER_THEME,
        "cancelling a second picker undid the first picker's commit"
    );
}

#[test]
fn session_reopening_the_theme_picker_starts_from_the_committed_theme() {
    // The configuration file still names the theme the session started with, so a picker
    // that opened on `config.theme` would put the cursor on a theme that is no longer
    // showing — and escaping would then "restore" the user out of their own choice.
    let (screen, context, _shutdown) = themed_screen();
    let mut host = opened_theme_picker(screen, &context);
    filter_dialog(&mut host, OTHER_THEME);
    submit_dialog(&mut host);

    let registry = crate::theme::ThemeRegistry::new();
    let reopened =
        crate::views::picker::theme_picker(context.clone(), &registry, context.theme().mode);
    assert_eq!(
        reopened.selected().map(|item| item.value.clone()),
        Some(String::from(OTHER_THEME)),
        "the reopened picker did not start on the committed theme"
    );
}

#[test]
fn session_committing_a_theme_sends_nothing_a_host_would_rebuild_a_turn_for() {
    // The deliberate CLI discard (`cmd/tui.rs`: `Selection::Theme(_) => return None`)
    // exists because that channel rebuilds the turn host. A theme must therefore not
    // travel on it at all: nothing is sent, no turn is disturbed, and the user is not
    // told "not applied: nothing is listening" about a theme that visibly did apply.
    let (sender, mut selections) = mpsc::channel(4);
    let context = ViewContext::defaults();
    let (shutdown, _receiver) = terminal_event_channel();
    let mut screen = SessionScreen::new(context.clone(), shutdown)
        .with_keymap(Keymap::defaults().expect("the shipped table builds"))
        .with_selection_sink(sender);
    *screen.catalog_mut() = catalog();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("a message"));

    let mut host = opened_theme_picker(screen, &context);
    filter_dialog(&mut host, OTHER_THEME);
    submit_dialog(&mut host);

    assert_eq!(
        context.theme().name,
        OTHER_THEME,
        "the theme did not apply, so this test proves nothing about the channel"
    );
    assert!(
        selections.try_recv().is_err(),
        "a theme was pushed onto the channel that rebuilds the turn host"
    );

    // The same sink still carries the selections that genuinely belong to the host, so
    // the assertion above cannot be passing because the channel is broken.
    host.handle_action(action("agent_list"), &press_none());
    assert_eq!(
        host.active(),
        Some(crate::views::picker::AGENT_DIALOG_ID),
        "the agent picker did not open"
    );
    submit_dialog(&mut host);
    assert!(
        matches!(selections.try_recv(), Ok(Selection::Agent(_))),
        "the selection channel carries nothing at all, so the theme assertion was vacuous"
    );
}

// ---------------------------------------------------------------------------
// The prompt band, the wheel, and the clipboard
// ---------------------------------------------------------------------------
//
// Rewritten after this file was reverted to HEAD during the theme task and eighteen
// uncommitted tests were lost with it. The implementations they covered — `prompt_rows`,
// the `Scroller` wiring and the injected `Clipboard` — were untouched, so what follows
// re-derives the coverage from those implementations rather than restoring the originals,
// which are not recoverable. Recorded in the notepad; the names are the originals' so a
// future reader can match them against the lost set.

/// A screen whose copies land in a clipboard the caller can read back.
///
/// Injected rather than global so the assertion is not order-dependent across the suite,
/// and so the suite never spawns `xclip` or paints an escape sequence into captured
/// output.
fn screen_with_clipboard() -> (
    SessionScreen,
    Arc<crate::views::external::MemoryClipboard>,
    mpsc::Receiver<TerminalEvent>,
) {
    let (sender, receiver) = terminal_event_channel();
    let clipboard = Arc::new(crate::views::external::MemoryClipboard::default());
    let screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_clipboard(Arc::clone(&clipboard) as Arc<dyn Clipboard>);
    (screen, clipboard, receiver)
}

/// A clipboard with no mechanism at all, which is how a real host with neither a
/// terminal nor a native program behaves.
fn broken_clipboard() -> Arc<dyn Clipboard> {
    let log = crate::views::external::CopyLog::shared();
    Arc::new(crate::views::external::SystemClipboard::new(
        None,
        false,
        None,
        Box::new(crate::views::external::ScriptedRunner::failing(log)),
    ))
}

/// The toast the screen last asked its host to raise.
///
/// Drained the way [`crate::views::dialog::DialogHost`] drains it, so a test reads the
/// same value production does. Copy feedback moved off the transcript because a
/// transcript line is permanent and is hidden behind an open modal; see
/// `SessionScreen::copy`. A frame-level assertion that it actually reaches the screen
/// above a dialog lives in `views/toast_tests.rs`, and one from this real caller is
/// below.
fn last_toast(screen: &mut SessionScreen) -> Toast {
    screen
        .drain_toasts()
        .pop()
        .expect("the screen raised no toast at all")
}

/// The notice text of the last message in the transcript.
fn last_message(screen: &mut SessionScreen) -> String {
    screen
        .transcript_mut()
        .transcript()
        .messages()
        .last()
        .map(message_text)
        .unwrap_or_default()
}

/// Every human-readable part of `message`, joined.
///
/// `Notice` as well as `Text`: the screen reports a copy through
/// [`crate::views::message::Message::notice`], so a helper that only read `Text` would
/// report an empty string and every assertion below would be about nothing.
fn message_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            crate::views::message::MessagePart::Text { text }
            | crate::views::message::MessagePart::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ask the screen to copy, the way the keymap does.
fn copy_action(screen: &mut SessionScreen) -> EventResult {
    screen.handle_action(action("messages_copy"), &press_none())
}

#[test]
fn session_screen_copying_puts_the_text_on_the_clipboard() {
    let (mut screen, clipboard, _shutdown) = screen_with_clipboard();
    for character in "copy me".chars() {
        screen.editor.insert_char(character);
    }

    assert!(copy_action(&mut screen).redraw);

    assert_eq!(
        clipboard.read().expect("a memory clipboard cannot fail"),
        Some(crate::views::external::ClipboardContent::text("copy me")),
        "the copy signal's payload never reached the clipboard"
    );
}

#[test]
fn session_screen_says_on_screen_that_the_copy_landed() {
    // A copy key that paints nothing teaches the user the binding is broken, so success
    // has to be visible and not only silent.
    let (mut screen, _clipboard, _shutdown) = screen_with_clipboard();
    for character in "abc".chars() {
        screen.editor.insert_char(character);
    }
    copy_action(&mut screen);
    let toast = last_toast(&mut screen);
    assert!(
        toast.text().contains("copied") && toast.text().contains('3'),
        "a successful copy said nothing useful: {:?}",
        toast.text()
    );
    assert_eq!(
        toast.level(),
        ToastLevel::Success,
        "a copy that worked was not reported as a success"
    );
    assert!(
        last_message(&mut screen).is_empty(),
        "the copy also went into the transcript, where it is permanent"
    );
}

#[test]
fn session_screen_a_failed_copy_is_visible_rather_than_silent() {
    let (sender, _shutdown) = terminal_event_channel();
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), sender).with_clipboard(broken_clipboard());
    screen.editor.insert_char('x');

    copy_action(&mut screen);

    let toast = last_toast(&mut screen);
    assert!(
        toast.text().contains("copy failed"),
        "a host with no clipboard mechanism reported success: {:?}",
        toast.text()
    );
    assert_eq!(
        toast.level(),
        ToastLevel::Error,
        "a failed copy was not reported as an error"
    );
}

#[test]
fn session_screen_copying_nothing_leaves_the_clipboard_alone() {
    // Writing the empty string would destroy whatever the user already had.
    let (mut screen, clipboard, _shutdown) = screen_with_clipboard();
    copy_action(&mut screen);

    assert_eq!(
        clipboard.read().expect("a memory clipboard cannot fail"),
        None,
        "an empty prompt overwrote the clipboard"
    );
    let toast = last_toast(&mut screen);
    assert!(
        toast.text().contains("nothing to copy"),
        "an empty copy said nothing: {:?}",
        toast.text()
    );
    assert_eq!(
        toast.level(),
        ToastLevel::Warning,
        "nothing failed, so this is not an error: `§11.5` reserves that colour"
    );
}

#[test]
fn session_screen_copying_prefers_the_selection_over_the_whole_buffer() {
    let (mut screen, clipboard, _shutdown) = screen_with_clipboard();
    for character in "abcdef".chars() {
        screen.editor.insert_char(character);
    }
    // Two selecting movements from the end, so the selection is the tail and not the lot.
    screen.editor.handle_action(action("input_select_left"));
    screen.editor.handle_action(action("input_select_left"));
    assert_eq!(
        screen.editor.selection(),
        Some(String::from("ef")),
        "the fixture failed to select, so this test would pass for the wrong reason"
    );

    copy_action(&mut screen);

    assert_eq!(
        clipboard.read().expect("a memory clipboard cannot fail"),
        Some(crate::views::external::ClipboardContent::text("ef")),
        "the whole buffer was copied over the user's selection"
    );
}

/// How many rows the prompt band occupied in a `width` x `height` frame.
///
/// Measured back out of the frame rather than by calling `prompt_rows`. Asserting that
/// function's return value would keep passing if `render` stopped consulting it, which is the
/// second-source-of-truth hole this measurement exists to close.
///
/// The band's top edge is found from the gutter marker `render` paints there — content, so it
/// moves when the band moves. Its bottom edge cannot be found the same way: the band ends in a
/// blank spacer and the rows below it are blank too, so the only thing that separates them is
/// the tail's length, which comes from the same function `render` uses. Deriving the *whole*
/// span from `prompt_rows` instead would make this arithmetic rather than a measurement and
/// would reopen exactly the hole above.
///
/// `prompt_band` deliberately locates the band the other way round — counted from the bottom,
/// never from the marker — because one of its callers asserts that the marker is there.
fn prompt_band_rows(screen: &mut SessionScreen, width: u16, height: u16) -> usize {
    let tail = usize::from(screen.prompt_and_tail(width, height).1);
    let rendered = rows(&render_offscreen(screen, width, height).expect("infallible"));
    let first = rendered
        .iter()
        .position(|row| row.contains(PROMPT_MARKER))
        .expect("the prompt paints its gutter marker at every width these tests use");
    // Every band below the prompt is subtracted — the welcome tail and `INFO_ROWS`.
    // The reply identity is above the prompt and therefore does not enter this measure.
    rendered.len() - first - tail - usize::from(info_rows(height))
}

/// One complete left click at `(column, row)`, delivered the way the event loop does.
///
/// Through `handle_event` rather than `handle_mouse`, because the defect was not a missing
/// hit test — it was that nothing on this screen consumed a press at all. A test that called
/// the hit test directly would pass against a screen whose `handle_event` still discards
/// every mouse event, which is precisely the shipped state being fixed.
fn click_at(
    screen: &mut (impl crate::app::Component + ?Sized),
    column: u16,
    row: u16,
) -> EventResult {
    pointer_at(
        screen,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column,
        row,
    )
    .merge(pointer_at(
        screen,
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        column,
        row,
    ))
}

/// Deliver one pointer event through the same terminal-event path as the live app.
fn pointer_at(
    screen: &mut (impl crate::app::Component + ?Sized),
    kind: crossterm::event::MouseEventKind,
    column: u16,
    row: u16,
) -> EventResult {
    screen.handle_event(&crate::app::AppEvent::Terminal(TerminalEvent::Input(
        crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }),
    )))
}

/// The terminal coordinate occupied by an ASCII control label.
///
/// These tests click the rendered bytes rather than duplicate dialog geometry. Every
/// label passed here is ASCII, so its byte offset is also its terminal-column offset.
fn control_at(rendered: &[String], label: &str) -> (u16, u16) {
    let row = rendered
        .iter()
        .position(|line| line.contains(label))
        .unwrap_or_else(|| {
            panic!(
                "control `{label}` is not rendered:\n{}",
                rendered.join("\n")
            )
        });
    let column = rendered[row]
        .find(label)
        .expect("the row selected above contains the label");
    (
        u16::try_from(column).expect("control column fits the frame"),
        u16::try_from(row).expect("control row fits the frame"),
    )
}

#[test]
fn session_a_click_on_one_tool_header_expands_only_that_call() {
    let (mut screen, _shutdown) = mouse_screen();
    let provider = |event| TurnEvent::Provider { step: 1, event };
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("run both checks"));
    screen.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: String::from("assistant"),
    }));
    for (call_id, command, prefix) in [
        ("call_first", "first-command", "first-output"),
        ("call_second", "second-command", "second-output"),
    ] {
        for event in [
            provider(StreamEvent::ToolUseStart {
                id: call_id.to_owned(),
                name: String::from("bash"),
            }),
            provider(StreamEvent::ToolInputDelta {
                id: call_id.to_owned(),
                delta: format!(r#"{{"command":"{command}"}}"#),
            }),
            provider(StreamEvent::ToolUseEnd {
                id: call_id.to_owned(),
            }),
            TurnEvent::ToolDispatchCompleted {
                step: 1,
                call_id: call_id.to_owned(),
                name: String::from("bash"),
                title: command.to_owned(),
                output: (1..=6)
                    .map(|line| format!("{prefix}-{line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                diff: None,
                written_paths: Vec::new(),
                is_error: false,
            },
        ] {
            screen.handle_event(&AppEvent::Engine(event));
        }
    }

    screen
        .transcript_mut()
        .set_activity_display(crate::views::message::ActivityDisplay::Detailed);
    let before = rows(&render_offscreen(&mut screen, 100, 32).expect("infallible"));
    assert!(
        !before.join("\n").contains("first-output-6")
            && !before.join("\n").contains("second-output-6"),
        "the fixture did not start with both calls collapsed:\n{}",
        before.join("\n")
    );
    let first_row = u16::try_from(
        before
            .iter()
            .position(|row| row.contains("first-command"))
            .expect("the first tool header is drawn"),
    )
    .expect("in frame");

    assert!(click_at(&mut screen, 4, first_row).redraw);
    let after = rows(&render_offscreen(&mut screen, 100, 32).expect("infallible"));
    let joined = after.join("\n");
    assert!(
        joined.contains("first-output-6"),
        "clicking the first header did not reveal its withheld output:\n{joined}"
    );
    assert!(
        !joined.contains("second-output-6"),
        "clicking the first header expanded its sibling too:\n{joined}"
    );
    let first_header = after
        .iter()
        .find(|row| row.contains("first-command"))
        .expect("the first header remains");
    let second_header = after
        .iter()
        .find(|row| row.contains("second-command"))
        .expect("the second header remains");
    assert!(
        first_header.contains('▾') && second_header.contains('▸'),
        "the disclosure glyphs do not describe the two independent states:\n{joined}"
    );
}

#[test]
fn session_a_click_on_one_thought_reveals_its_full_text_without_opening_its_sibling() {
    let (mut screen, _shutdown) = mouse_screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("solve the constraints"));
    screen.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: String::from("assistant"),
    }));
    let provider = |event| TurnEvent::Provider { step: 1, event };
    for event in [
        provider(StreamEvent::ReasoningStart),
        provider(StreamEvent::ReasoningDelta(String::from(
            "first compact topic\nfirst private detail\nFIRST_FULL_THOUGHT_SENTINEL",
        ))),
        provider(StreamEvent::ReasoningDone { duration_secs: 2.0 }),
        provider(StreamEvent::TextDelta(String::from(
            "intermediate answer\n",
        ))),
        provider(StreamEvent::ReasoningStart),
        provider(StreamEvent::ReasoningDelta(String::from(
            "second compact topic\nsecond private detail\nSECOND_FULL_THOUGHT_SENTINEL",
        ))),
        provider(StreamEvent::ReasoningDone { duration_secs: 3.0 }),
    ] {
        screen.handle_event(&AppEvent::Engine(event));
    }

    let before = rows(&render_offscreen(&mut screen, 100, 28).expect("infallible"));
    let before_joined = before.join("\n");
    assert!(
        !before_joined.contains("FIRST_FULL_THOUGHT_SENTINEL")
            && !before_joined.contains("SECOND_FULL_THOUGHT_SENTINEL"),
        "the fixture did not start with both thought blocks collapsed:\n{before_joined}"
    );
    let first_row = u16::try_from(
        before
            .iter()
            .position(|row| row.contains("first compact topic"))
            .expect("the first thought header is drawn"),
    )
    .expect("in frame");

    assert!(click_at(&mut screen, 5, first_row).redraw);
    let after = rows(&render_offscreen(&mut screen, 100, 28).expect("infallible"));
    let joined = after.join("\n");
    assert!(
        joined.contains("FIRST_FULL_THOUGHT_SENTINEL"),
        "clicking the thought header did not reveal the complete persisted body:\n{joined}"
    );
    assert!(
        !joined.contains("SECOND_FULL_THOUGHT_SENTINEL"),
        "clicking one thought expanded its sibling too:\n{joined}"
    );
    let first_header = after
        .iter()
        .find(|row| row.contains("◇ Thought · 2.0s"))
        .expect("the first thought header remains");
    let second_header = after
        .iter()
        .find(|row| row.contains("second compact topic"))
        .expect("the second thought header remains");
    assert!(
        first_header.contains('▾') && second_header.contains('▸'),
        "the disclosure glyphs do not describe the two independent states:\n{joined}"
    );
}

#[test]
fn session_a_transcript_drag_auto_copies_and_right_click_repeats_the_pane_bounded_copy() {
    let clipboard = Arc::new(crate::views::external::MemoryClipboard::default());
    let (screen, _shutdown) = mouse_conversing();
    let mut screen = screen.with_clipboard(clipboard.clone());
    let initial = rows(&render_offscreen(&mut screen, 120, 32).expect("infallible"));
    let row = u16::try_from(
        initial
            .iter()
            .position(|row| row.contains("Here is the summary"))
            .expect("the assistant row is drawn"),
    )
    .expect("in frame");

    pointer_at(
        &mut screen,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        row,
    );
    pointer_at(
        &mut screen,
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        119,
        row,
    );
    pointer_at(
        &mut screen,
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        119,
        row,
    );

    assert!(
        screen.transcript.has_selection(),
        "the transcript never acquired the drag selection"
    );
    let selected = render_offscreen(&mut screen, 120, 32).expect("infallible");
    let selected_bg = screen
        .context
        .selected()
        .bg
        .expect("the selected style has a background");
    assert_eq!(
        selected[(10, row)].bg,
        selected_bg,
        "the transcript side of the drag is not highlighted"
    );
    let sidebar_column = 120 - crate::views::ambient::SIDEBAR_WIDTH + 2;
    assert_ne!(
        selected[(sidebar_column, row)].bg,
        selected_bg,
        "the highlight crossed into the sidebar"
    );

    let copied = clipboard
        .read()
        .expect("a memory clipboard cannot fail")
        .expect("releasing the drag auto-copied the selection")
        .data;
    assert!(
        copied.contains("Here is the summary of the plan."),
        "the copied selection lost the transcript text: {copied:?}"
    );
    for sidebar_text in ["Context", "MCP", "Skills"] {
        assert!(
            !copied.contains(sidebar_text),
            "the copied selection crossed into the sidebar and captured {sidebar_text:?}: \
            {copied:?}"
        );
    }

    clipboard.write("sentinel").expect("memory clipboard write");
    pointer_at(
        &mut screen,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
        10,
        row,
    );
    let copied_again = clipboard
        .read()
        .expect("a memory clipboard cannot fail")
        .expect("right click copied the retained selection")
        .data;
    assert_eq!(copied_again, copied);
}

#[test]
fn session_sidebar_marks_a_host_preloaded_skill_without_a_fake_tool_call() {
    let (mut screen, _shutdown) = screen();
    screen.sidebar_mut().ambient_mut().skills = vec![crate::views::ambient::SkillSummary {
        name: String::from("codegraph"),
        source: String::from("/skills/codegraph/SKILL.md"),
        description: String::from("navigate the indexed repository"),
        loaded: false,
    }];
    screen.sidebar_mut().toggle_skills();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("use codegraph"));

    screen.handle_event(&AppEvent::Engine(TurnEvent::SkillLoaded {
        name: String::from("codegraph"),
        source: String::from("/skills/codegraph/SKILL.md"),
    }));

    let rendered = rows(&render_offscreen(&mut screen, 120, 32).expect("infallible")).join("\n");
    assert!(
        rendered.contains("1/1 loaded") && rendered.contains("✓ codegraph · loaded"),
        "a host preload was not projected as loaded:\n{rendered}"
    );
    assert!(
        !screen
            .transcript_mut()
            .transcript_mut()
            .messages()
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| matches!(part, crate::views::message::MessagePart::Tool { .. })),
        "host preloading fabricated an assistant tool call"
    );
}

#[test]
fn session_sidebar_distinguishes_discovered_skills_from_successfully_loaded_skills() {
    let (mut screen, _shutdown) = screen();
    screen.sidebar_mut().ambient_mut().skills = vec![crate::views::ambient::SkillSummary {
        name: String::from("codegraph"),
        source: String::from("/skills/codegraph/SKILL.md"),
        description: String::from("navigate the indexed repository"),
        loaded: false,
    }];
    screen.sidebar_mut().toggle_skills();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("analyse this repository"));

    let before = rows(&render_offscreen(&mut screen, 120, 32).expect("infallible")).join("\n");
    assert!(
        before.contains("0/1 loaded") && before.contains("· codegraph"),
        "a discovered-but-unused skill was presented as loaded:\n{before}"
    );

    let provider = |event| TurnEvent::Provider { step: 1, event };
    for event in [
        TurnEvent::AssistantMessageCreated {
            step: 1,
            message_id: String::from("assistant-resource"),
        },
        provider(StreamEvent::ToolUseStart {
            id: String::from("skill_resource_1"),
            name: String::from("skill"),
        }),
        provider(StreamEvent::ToolInputDelta {
            id: String::from("skill_resource_1"),
            delta: String::from(
                r#"{"action":"read_resource","name":"codegraph","path":"references/index.md"}"#,
            ),
        }),
        provider(StreamEvent::ToolUseEnd {
            id: String::from("skill_resource_1"),
        }),
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("skill_resource_1"),
            name: String::from("skill"),
            title: String::from("Skill resource: codegraph/references/index.md"),
            output: String::from("reference body"),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        },
    ] {
        screen.handle_event(&AppEvent::Engine(event));
    }
    let resource_only =
        rows(&render_offscreen(&mut screen, 120, 32).expect("infallible")).join("\n");
    assert!(
        resource_only.contains("0/1 loaded") && resource_only.contains("· codegraph"),
        "reading a resource without loading SKILL.md was incorrectly counted as loaded:\n\
         {resource_only}"
    );

    for event in [
        TurnEvent::AssistantMessageCreated {
            step: 2,
            message_id: String::from("assistant"),
        },
        provider(StreamEvent::ToolUseStart {
            id: String::from("skill_1"),
            name: String::from("skill"),
        }),
        provider(StreamEvent::ToolInputDelta {
            id: String::from("skill_1"),
            delta: String::from(r#"{"action":"load","name":"codegraph"}"#),
        }),
        provider(StreamEvent::ToolUseEnd {
            id: String::from("skill_1"),
        }),
        TurnEvent::ToolDispatchCompleted {
            step: 2,
            call_id: String::from("skill_1"),
            name: String::from("skill"),
            title: String::from("Loaded codegraph"),
            output: String::from("complete skill body"),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        },
    ] {
        screen.handle_event(&AppEvent::Engine(event));
    }

    let after = rows(&render_offscreen(&mut screen, 120, 32).expect("infallible")).join("\n");
    assert!(
        after.contains("1/1 loaded") && after.contains("✓ codegraph"),
        "a successfully completed skill call did not update the sidebar:\n{after}"
    );
}

#[test]
fn session_sidebar_tracks_same_named_skills_by_source() {
    let (mut screen, _shutdown) = screen();
    screen.sidebar_mut().ambient_mut().skills = vec![
        crate::views::ambient::SkillSummary {
            name: String::from("review"),
            source: String::from("/skills/team-a/review/SKILL.md"),
            description: String::from("team A review"),
            loaded: false,
        },
        crate::views::ambient::SkillSummary {
            name: String::from("review"),
            source: String::from("/skills/team-b/review/SKILL.md"),
            description: String::from("team B review"),
            loaded: false,
        },
    ];
    let provider = |event| TurnEvent::Provider { step: 1, event };
    for event in [
        TurnEvent::AssistantMessageCreated {
            step: 1,
            message_id: String::from("assistant"),
        },
        provider(StreamEvent::ToolUseStart {
            id: String::from("skill_review"),
            name: String::from("skill"),
        }),
        provider(StreamEvent::ToolInputDelta {
            id: String::from("skill_review"),
            delta: String::from(
                r#"{"action":"load","name":"review","source":"/skills/team-b/review/SKILL.md"}"#,
            ),
        }),
        provider(StreamEvent::ToolUseEnd {
            id: String::from("skill_review"),
        }),
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("skill_review"),
            name: String::from("skill"),
            title: String::from("Loaded review"),
            output: String::from("complete skill body"),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        },
    ] {
        screen.handle_event(&AppEvent::Engine(event));
    }

    let _rendered = render_offscreen(&mut screen, 120, 32).expect("infallible");
    let skills = &screen.sidebar_mut().ambient_mut().skills;
    assert!(
        !skills[0].loaded && skills[1].loaded,
        "loading one source marked the wrong same-named skill as loaded: {skills:?}"
    );
}

#[test]
fn session_a_click_on_a_sidebar_section_heading_collapses_it_through_the_event_loop() {
    // The whole route: the input filter forwards the press
    // (`app_the_input_filter_forwards_exactly_what_a_screen_consumes`), the screen's mouse
    // match dispatches it, and the panel's recorded geometry answers. Before this the only
    // mouse consumer on the screen was the wheel, so a click on a heading that draws a
    // disclosure triangle was discarded.
    let (mut screen, _shutdown) = mouse_screen();
    screen.sidebar_mut().ambient_mut().lsp = vec![crate::views::ambient::Service::new(
        "rust-analyzer",
        crate::views::ambient::Health::Ready,
    )];
    // And a transcript, because the panel is not drawn on the welcome screen at any width —
    // see `sidebar_drawn`.
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("so the panel has a transcript to sit beside"));
    // Wide enough that the panel is drawn at all — below `SIDEBAR_MIN_WIDTH` it is dropped,
    // and this test would be asserting against a screen that has no sidebar.
    let rendered = rows(&render_offscreen(&mut screen, 130, 30).expect("infallible"));
    let heading = rendered
        .iter()
        .position(|row| row.contains("LSP"))
        .unwrap_or_else(|| panic!("the sidebar is not on this frame:\n{}", rendered.join("\n")));
    let heading = u16::try_from(heading).expect("a 30-row frame");
    assert!(
        screen.sidebar.expanded().lsp,
        "the section under test starts collapsed, so opening it proves nothing"
    );

    // A column inside the panel. Derived from the frame's own width so the coordinate cannot
    // drift away from `SIDEBAR_WIDTH`.
    let inside = 130 - crate::views::ambient::SIDEBAR_WIDTH + 4;
    let outcome = click_at(&mut screen, inside, heading);

    assert!(
        outcome.redraw,
        "the click changed the panel but reported no repaint, so the collapse would not \
         reach the terminal until something unrelated redrew"
    );
    assert!(
        !screen.sidebar.expanded().lsp,
        "a click on the LSP heading did not collapse it"
    );
    let after = rows(&render_offscreen(&mut screen, 130, 30).expect("infallible")).join("\n");
    assert!(
        !after.contains("rust-analyzer"),
        "the section reports itself collapsed but still draws its rows:\n{after}"
    );

    // A click on the transcript side is now claimed by pane-bounded selection, but it must
    // not reach the panel.
    let elsewhere = click_at(&mut screen, 2, heading);
    assert!(
        elsewhere.handled,
        "the transcript did not claim the click for pane-bounded selection"
    );
    assert!(
        !screen.sidebar.expanded().lsp,
        "a press over the transcript reached the panel anyway"
    );
}

#[test]
fn session_a_click_where_the_sidebar_used_to_be_does_nothing_once_it_is_hidden() {
    // The staleness half. The panel's targets are frame geometry, so the frame that stops
    // drawing it must retract them — otherwise hiding the sidebar leaves its old rows
    // swallowing presses aimed at the transcript that took those columns.
    let (mut screen, _shutdown) = screen();
    // A transcript, because the panel is not drawn on the welcome screen at any width — see
    // `sidebar_drawn`. Without one there would be no targets to go stale.
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("so the panel has a transcript to sit beside"));
    let rendered = rows(&render_offscreen(&mut screen, 130, 30).expect("infallible"));
    let heading = u16::try_from(
        rendered
            .iter()
            .position(|row| row.contains("MCP"))
            .expect("the sidebar is on this frame"),
    )
    .expect("a 30-row frame");
    let inside = 130 - crate::views::ambient::SIDEBAR_WIDTH + 4;
    let before = screen.sidebar.expanded();

    screen.handle_action(action("sidebar_toggle"), &press_none());
    assert!(!screen.sidebar_visible(), "the sidebar is still visible");
    let _ = render_offscreen(&mut screen, 130, 30).expect("infallible");

    let outcome = click_at(&mut screen, inside, heading);
    assert!(
        outcome.handled,
        "the widened transcript did not claim the old panel coordinate"
    );
    assert_eq!(
        before,
        screen.sidebar.expanded(),
        "a press at the hidden panel's old coordinates toggled a section the user cannot see"
    );
}

#[test]
fn session_prompt_offers_four_rows_to_an_empty_buffer_once_the_pane_can_pay_for_them() {
    // The complaint, twice: the empty prompt was two rows — one of text and one spacer — so
    // it read as a one-line field rather than a place to compose a paragraph.
    //
    // A table over heights rather than a single assertion, because the interesting property
    // is not "four" but *how the floor degrades*: it is granted through the third-of-screen
    // cap, never over it, which is also what keeps `u16::clamp` from aborting (see
    // `prompt_rows`). Literals on both sides — a row count derived from the constants would
    // follow them wherever they went and pin nothing.
    //
    // Each row states what the transcript is left with, because that is the cost being
    // accepted. The transcript is empty in this fixture, so `body` here excludes the welcome
    // tail as well as the strip and the prompt.
    for (height, band) in [
        // Under six rows the cap is at the survival floor and the prompt gets two.
        (4_u16, 2_usize),
        (6, 2),
        // Six to eight rows: the cap is two, still the survival floor.
        (8, 2),
        // Nine to eleven: the cap is three, so the preferred four is cut down to it.
        (9, 3),
        (11, 3),
        // Twelve and up the preferred floor is affordable and stops growing.
        (12, 4),
        (24, 4),
        (50, 4),
    ] {
        let (mut screen, _shutdown) = screen();
        assert_eq!(
            prompt_band_rows(&mut screen, 40, height),
            band,
            "an empty prompt on a {height}-row pane"
        );
    }

    // The 24-row pane is the one the decision was made on: the shortest common size.
    // The reply now sizes to its content rather than claiming all spare rows, so only
    // the prompt height itself is pinned here.
    let (mut screen, _shutdown) = screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("so the transcript owns the body region"));
    assert_eq!(
        prompt_band_rows(&mut screen, 40, 24),
        4,
        "a conversation gets the same prompt height as the welcome screen; a composer that \
         shrinks when the first reply lands reads as two different applications"
    );
}

#[test]
fn session_prompt_never_panics_on_any_viewport_the_preferred_floor_cannot_fit() {
    // `PROMPT_PREFERRED_ROWS` is four and `height / PROMPT_MAX_SHARE` is under four for
    // every viewport shorter than twelve rows, so the floor exceeds the cap on far more
    // sizes than the old two-row floor did — it moved the hazard from "shorter than six
    // rows" to "shorter than twelve". `u16::clamp` panics when its minimum exceeds its
    // maximum, so every one of those heights is a potential abort.
    //
    // Rendered, not just computed: the band feeds a `Layout` and then `prompt_frame`, and
    // `.max(1)` in the latter once fabricated a row the band did not own and panicked inside
    // ratatui's buffer. Only a real frame covers both.
    for height in 1..=12 {
        let (mut screen, _shutdown) = screen();
        assert!(
            render_offscreen(&mut screen, 20, height).is_ok(),
            "rendering a 20x{height} viewport failed instead of degrading"
        );
        let band = prompt_rows(screen.editor.height(), height);
        assert!(
            band <= (height / PROMPT_MAX_SHARE).max(PROMPT_MIN_ROWS),
            "the prompt took {band} rows of a {height}-row pane, which is over its own cap"
        );
    }
}

#[test]
fn session_prompt_grows_with_the_typed_line_count() {
    let (mut screen, _shutdown) = screen();
    for _ in 0..3 {
        screen.editor.insert_char('x');
        screen.editor.insert_char('\n');
    }
    assert_eq!(
        screen.editor.height(),
        4,
        "the fixture did not produce the line count this test is about"
    );
    assert_eq!(
        prompt_band_rows(&mut screen, 40, 30),
        5,
        "the prompt did not grow to content plus the row the cursor is about to open"
    );
}

#[test]
fn session_prompt_growth_stops_at_a_third_of_the_screen() {
    // A pasted diff allowed to take the whole height would evict the transcript it is
    // about to be sent against.
    let (mut screen, _shutdown) = screen();
    for _ in 0..40 {
        screen.editor.insert_char('x');
        screen.editor.insert_char('\n');
    }
    assert_eq!(prompt_band_rows(&mut screen, 40, 30), 30 / 3);
}

#[test]
fn session_prompt_survives_a_viewport_shorter_than_the_floor_allows() {
    // `height / 3` is under the two-row floor for any viewport shorter than six rows, and
    // `u16::clamp` panics when its minimum exceeds its maximum — so the naive
    // `wanted.clamp(PROMPT_MIN_ROWS, height / PROMPT_MAX_SHARE)` aborts the process on a
    // 20x10 terminal, a size a real pane reaches.
    for height in 1..=10 {
        let (mut screen, _shutdown) = screen();
        let rendered = render_offscreen(&mut screen, 20, height);
        assert!(
            rendered.is_ok(),
            "rendering a {height}-row viewport failed instead of degrading"
        );
    }
}

/// A context carrying the two scroll keys.
fn scroll_config(speed: Option<f64>, acceleration: Option<bool>) -> ViewContext {
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);
    ViewContext::new(
        &resolved,
        crate::config::ResolvedTuiConfig {
            scroll_speed: speed,
            scroll_acceleration: acceleration
                .map(|enabled| crate::config::ScrollAcceleration { enabled }),
            ..crate::config::ResolvedTuiConfig::default()
        },
    )
}

/// A screen with more transcript than fits, so an offset has somewhere to go.
fn scrollable(context: ViewContext) -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = terminal_event_channel();
    let mut screen = SessionScreen::new(context, sender);
    for index in 0..80 {
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::user(format!("line {index}")));
    }
    // The scroller reads the transcript's own measurements, which only exist after a
    // render: a wheel event on an unmeasured transcript has a zero viewport and moves
    // nothing, which would make every assertion below vacuous.
    let _ = render_offscreen(&mut screen, 40, 12).expect("infallible");
    // Explicitly to the top, because that render leaves the viewport on the *newest* row —
    // see `TranscriptView`'s `following`. Every wheel assertion below counts rows moved
    // from a known start, and the top is the only start a downward notch can move from.
    // This used to be implicit: the offset happened to stay at 0 because nothing ever
    // raised it, which is the truncation
    // `message_tests::views_transcript_follows_the_newest_row_as_a_reply_streams_in`
    // reports. Disarming `following` here is what these tests always relied on, now said
    // out loud rather than inherited from a defect.
    screen.transcript.set_offset(0);
    assert!(
        !screen.transcript.is_following(),
        "the fixture is still following, so a downward notch has nowhere to go"
    );
    (screen, receiver)
}

/// One wheel notch downwards, observed at `now_ms`.
fn notch(screen: &mut SessionScreen, now_ms: u64) -> EventResult {
    screen.handle_mouse(
        &crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollDown,
            column: 4,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
        now_ms,
    )
}

/// One wheel notch upwards, observed at `now_ms`.
fn notch_up(screen: &mut SessionScreen, now_ms: u64) -> EventResult {
    screen.handle_mouse(
        &crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 4,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
        now_ms,
    )
}

/// The composed screen's first frame shows the newest row, not the oldest.
///
/// The layer the user meets, and the one the report came from: a reply longer than the
/// transcript pane appeared cut off at whatever row the pane ended on. Asserted here as
/// well as in `message_tests` because `SessionScreen` is what decides how tall the
/// transcript's region is — the pane is `area` minus the status strip, the prompt band and
/// the welcome tail, so a screen that gave the transcript the whole area would hide the
/// defect from a `TranscriptView`-only test.
///
/// Its own fixture rather than `scrollable`, which now scrolls to the top on purpose.
#[test]
fn session_a_fresh_render_rests_on_the_newest_row_not_the_oldest() {
    let (sender, _receiver) = terminal_event_channel();
    let mut screen = SessionScreen::new(scroll_config(None, None), sender);
    for index in 0..80 {
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::user(format!("line {index}")));
    }
    let painted =
        crate::views::testkit::rows(&render_offscreen(&mut screen, 40, 24).expect("infallible"))
            .join("\n");
    let transcript = &screen.transcript;
    assert!(
        transcript.content_height() > transcript.viewport_height(),
        "the fixture fits the pane, so this asserts nothing"
    );
    assert_eq!(
        transcript.offset(),
        transcript.content_height() - transcript.viewport_height(),
        "the first frame rested away from the newest row"
    );
    // Both dimensions again: the newest message present and the oldest gone. Either alone
    // is satisfied by a pane that grew rather than a viewport that moved.
    assert!(
        painted.contains("line 79"),
        "the newest message is below the fold:\n{painted}"
    );
    assert!(
        !painted.contains("line 0\n") && !painted.contains("line 0 "),
        "the oldest message is still on screen, so nothing scrolled:\n{painted}"
    );
}

#[test]
fn session_dragging_the_visible_scrollbar_moves_the_transcript_to_the_bottom() {
    let (mut screen, _shutdown) = scrollable(scroll_config(None, None));
    let before = render_offscreen(&mut screen, 120, 24).expect("infallible");
    let area = screen
        .scrollbar_area
        .expect("an overflowing used session mounts its scrollbar");
    assert!(
        matches!(
            before[(area.x, area.y)].symbol(),
            ratatui::symbols::block::FULL | ratatui::symbols::line::VERTICAL
        ),
        "the mounted scrollbar column was blank"
    );
    let max = screen
        .transcript
        .content_height()
        .saturating_sub(screen.transcript.viewport_height());
    assert!(max > 0, "the fixture does not overflow");
    assert_eq!(
        screen.transcript.offset(),
        0,
        "the fixture is not at the top"
    );

    pointer_at(
        &mut screen,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        area.x,
        area.y,
    );
    pointer_at(
        &mut screen,
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        area.x,
        area.bottom().saturating_sub(1),
    );
    pointer_at(
        &mut screen,
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        area.x,
        area.bottom().saturating_sub(1),
    );

    assert_eq!(
        screen.transcript.offset(),
        max,
        "dragging the thumb to the bottom did not reach the final viewport"
    );
    assert!(
        !screen.transcript.has_selection(),
        "a scrollbar drag also created a transcript selection"
    );
    let after = rows(&render_offscreen(&mut screen, 120, 24).expect("infallible")).join("\n");
    assert!(
        after.contains("line 79"),
        "the scrollbar reached the final offset but the newest message is not visible:\n{after}"
    );
}

#[test]
fn session_wheel_notch_uses_the_default_three_lines_when_nothing_is_configured() {
    // `scroll.ts:26` — the default is three lines per notch, not one. Getting it wrong is
    // a difference every user notices.
    let (mut screen, _shutdown) = scrollable(scroll_config(None, None));
    notch(&mut screen, 1_000);
    // The literal three, not `DEFAULT_SCROLL_SPEED`. Asserting against the constant is a
    // tautology that survives editing the constant, which is exactly the change this test
    // is here to catch.
    assert_eq!(screen.transcript.offset(), 3);
}

#[test]
fn session_wheel_notch_moves_the_transcript_by_the_configured_scroll_speed() {
    let (mut screen, _shutdown) = scrollable(scroll_config(Some(5.0), None));
    notch(&mut screen, 1_000);
    assert_eq!(screen.transcript.offset(), 5);
}

#[test]
fn session_wheel_carries_a_fractional_speed_across_separate_events() {
    // The whole reason the scroller is a field rather than built per event: a multiplier
    // under one row would otherwise round to no movement forever.
    let (mut screen, _shutdown) = scrollable(scroll_config(Some(0.5), None));
    let first = notch(&mut screen, 1_000);
    assert!(
        !first.redraw && screen.transcript.offset() == 0,
        "half a row moved the view, so there is no carry to test"
    );
    let second = notch(&mut screen, 2_000);
    assert!(second.redraw, "the carried half row never arrived");
    assert_eq!(screen.transcript.offset(), 1);
}

#[test]
fn session_wheel_scrolls_back_up_and_stops_at_the_top() {
    let (mut screen, _shutdown) = scrollable(scroll_config(Some(3.0), None));
    notch(&mut screen, 1_000);
    assert!(screen.transcript.offset() > 0);
    for step in 0..10 {
        notch_up(&mut screen, 2_000 + step * 1_000);
    }
    assert_eq!(
        screen.transcript.offset(),
        0,
        "scrolling up past the top did not clamp"
    );
}

#[test]
fn session_wheel_acceleration_compounds_a_fast_streak_but_not_a_slow_one() {
    // A streak inside `STREAK_TIMEOUT_MS` accelerates; one outside it resets to a
    // multiplier of one.
    let fast = {
        let (mut screen, _shutdown) = scrollable(scroll_config(None, Some(true)));
        for step in 0..4 {
            notch(&mut screen, 1_000 + step * 10);
        }
        screen.transcript.offset()
    };
    let slow = {
        let (mut screen, _shutdown) = scrollable(scroll_config(None, Some(true)));
        for step in 0..4 {
            notch(
                &mut screen,
                1_000 + step * (crate::views::scroll::STREAK_TIMEOUT_MS + 50),
            );
        }
        screen.transcript.offset()
    };
    assert!(
        fast > slow,
        "a fast streak ({fast}) did not out-scroll a slow one ({slow})"
    );
}

#[test]
fn session_wheel_acceleration_disabled_keeps_a_constant_speed_under_a_fast_streak() {
    // `scroll_speed` set and acceleration absent means a constant multiplier, so four
    // rapid notches move exactly four times the speed.
    let (mut screen, _shutdown) = scrollable(scroll_config(Some(2.0), Some(false)));
    for step in 0..4 {
        notch(&mut screen, 1_000 + step * 10);
    }
    assert_eq!(screen.transcript.offset(), 8);
}

#[test]
fn session_keyboard_scrolling_stays_one_row_per_press_whatever_the_wheel_is_configured_to() {
    // Acceleration is a property of a continuous gesture. A line the user asked for by
    // name must not become four because they pressed the key quickly.
    let (mut screen, _shutdown) = scrollable(scroll_config(Some(7.0), Some(true)));
    screen.handle_action(action("messages_line_down"), &press_none());
    assert_eq!(screen.transcript.offset(), 1);
    screen.handle_action(action("messages_line_down"), &press_none());
    assert_eq!(screen.transcript.offset(), 2);
}

#[test]
fn session_wheel_landing_on_the_bottom_re_arms_following_a_live_turn() {
    // A transcript the user scrolled away from must not be yanked by a streaming reply;
    // one they scrolled back to the bottom of must follow again.
    let (mut screen, _shutdown) = scrollable(scroll_config(Some(3.0), None));
    notch_up(&mut screen, 1_000);
    // Derived from the measured transcript rather than the literal `60` this used to be.
    // Each notch moves at least one row, so a notch per row cannot fail to reach the
    // bottom — whereas a fixed count silently stops reaching it the moment a message
    // occupies more rows than it used to, which is what framing the user's turn did:
    // 60 notches at speed 3.0 moved 180 rows against a transcript that had become 313.
    // The claim being tested is about what happens *at* the bottom, so a count that no
    // longer arrives there tests nothing.
    let notches = u64::try_from(screen.transcript.content_height()).unwrap_or(u64::MAX);
    for step in 0..notches {
        notch(&mut screen, 2_000 + step * 1_000);
    }
    let bottom = screen.transcript.content_height() - screen.transcript.viewport_height();
    assert_eq!(
        screen.transcript.offset(),
        bottom,
        "the wheel reached the bottom without re-arming following"
    );
}

#[test]
fn session_ignores_a_mouse_event_that_is_not_a_vertical_wheel() {
    // The transcript has one axis. Claiming a click or a drag would take it away from
    // whatever surface grows a use for it next.
    let (mut screen, _shutdown) = scrollable(scroll_config(Some(3.0), None));
    let moved = screen.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        CrosstermEvent::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 4,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }),
    )));
    assert!(!moved.redraw);
    assert_eq!(screen.transcript.offset(), 0);
}

/// Every action that resolves on a real key press and reaches nothing.
///
/// A frozen census, not an approval. `PRESSABLE_BUT_DEAD` exists because before it nothing
/// in this crate could name a single member of this set: the previous eight defects of the
/// family were each found by a person pressing a key on a terminal and noticing silence, one
/// at a time. The list is what turns that into a build failure.
///
/// Two kinds of row live here and the difference is stated per row. `diff_*` is deliberate —
/// `focused_scopes` documents why the whole `diff` scope is registered from the prompt for
/// `diff_open`'s sake, and why an arm for any of its bare characters would stop that
/// character being typeable; those actions reach the viewer through `DialogHost`, which owns
/// the keyboard while it is open. Everything else is a spelling upstream's table ships for a
/// capability this screen has not grown yet, recorded so the next author sees the backlog
/// instead of rediscovering one row of it.
const PRESSABLE_BUT_DEAD: &[&str] = &[
    // Deliberate: the bare arrows the delegated-task view owns. They reach
    // `crate::views::subagent::SubagentView` through `DialogHost`, which owns the keyboard
    // and promotes the `session` scope while that view is open — the same arrangement the
    // `diff_*` rows below describe. An arm on this screen would take `left`, `right` and
    // `up` away from the prompt cursor, which the `input` scope wins ahead of `session`
    // precisely so that it keeps them.
    "session_child_cycle",
    "session_child_cycle_reverse",
    "session_parent",
    // Deliberate: bare characters the diff viewer owns; an arm here would make them untypeable.
    "diff_close",
    "diff_collapse",
    "diff_expand",
    "diff_expand_all",
    "diff_help",
    "diff_next_file",
    "diff_next_hunk",
    "diff_previous_file",
    "diff_previous_hunk",
    "diff_single_patch",
    "diff_stage_hunk",
    "diff_switch_focus",
    "diff_switch_source",
    "diff_toggle",
    "diff_toggle_file_tree",
    "diff_toggle_layout",
    "diff_toggle_view",
    "diff_unstage_hunk",
    // Backlog: bound by the shipped table, no capability behind it on this screen yet.
    "input_redo",
    "messages_redo",
    "messages_toggle_conceal",
    "messages_undo",
    "model_cycle_recent",
    "model_cycle_recent_reverse",
    "model_favorite_toggle",
    "model_provider_list",
    "session_background",
    "session_compact",
    "session_delete",
    "session_export",
    "session_pin_toggle",
    "session_quick_switch_1",
    "session_quick_switch_2",
    "session_quick_switch_3",
    "session_quick_switch_4",
    "session_quick_switch_5",
    "session_quick_switch_6",
    "session_quick_switch_7",
    "session_quick_switch_8",
    "session_quick_switch_9",
    "session_rename",
    "session_timeline",
];

#[test]
fn every_bound_action_in_a_registered_scope_either_reaches_something_or_is_a_named_gap() {
    // The hole `every_action_the_screen_consumes_lives_in_a_scope_it_resolves` leaves, and the
    // reason `agent_cycle` shipped dead as the eighth of its kind. That guard opens with
    //
    //     let consumed = ...any(|screen| screen.handle_action(definition, ..).handled);
    //     if !consumed { continue; }
    //
    // so its subject is only ever an action the screen *already acts on*. It proves
    // "consumed implies reachable" and is structurally silent about the converse.
    // `agent_cycle` was bound to `tab`, sat in the registered `agent` scope, and resolved
    // correctly on a real terminal — it simply had no `handle_action` arm, so `consumed` was
    // false, so the loop `continue`d past it and reported nothing. Every earlier defect of
    // this family was a missing *scope* for an arm that existed, which is the direction that
    // guard covers; this one was a missing *arm* for a scope that existed, which is the
    // direction no guard covered. The two neighbouring guards miss it for their own reasons:
    // both derive their subject from a hand-kept list (`SHIPPED_DEFAULTS` there, `HINTS` in
    // the welcome grid's), and `agent_cycle` is on neither.
    //
    // This asserts the converse, as an equality in both directions. A newly bound action with
    // no arm fails until its author wires it up or names it here; an action that gains an arm
    // fails until its name is removed. The second direction is what stops the list decaying
    // into a blanket suppression — a stale entry is itself a failure.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let static_scopes = scopes();
    let mut dead = Vec::new();
    let mut live_but_listed = Vec::new();
    for definition in crate::keybind::DEFINITIONS {
        if keymap.sequences(definition.name).is_empty() {
            continue;
        }
        let in_scope = static_scopes.iter().any(|scope| scope == definition.scope)
            || reachability_screens()
                .into_iter()
                .any(|screen| ActionComponent::focused_scopes(&screen).contains(&definition.scope));
        if !in_scope {
            continue;
        }
        let consumed = reachability_screens()
            .into_iter()
            .any(|mut screen| screen.handle_action(definition, &press_none()).handled);
        let listed = PRESSABLE_BUT_DEAD.contains(&definition.name);
        if !consumed && !listed {
            dead.push(format!(
                "{} (`{}`) resolves in scope `{}` and reaches nothing",
                definition.name,
                keymap.sequences(definition.name).join("` or `"),
                definition.scope
            ));
        }
        if consumed && listed {
            live_but_listed.push(definition.name);
        }
    }
    assert!(
        dead.is_empty(),
        "these actions are pressable and reach nothing, which is how `agent_cycle` shipped \
         as the eighth 'built but unreachable'. Give each an arm in `handle_action`, or add \
         it to `PRESSABLE_BUT_DEAD` with the reason it is a gap:\n{}",
        dead.join("\n")
    );
    assert!(
        live_but_listed.is_empty(),
        "`PRESSABLE_BUT_DEAD` still names these, but the screen now acts on them; a stale \
         entry suppresses this guard for a row it no longer describes, so remove them: \
         {live_but_listed:?}"
    );
    // The census is a number as well as a set, so shrinking it is a visible event in a diff
    // and growing it silently is impossible. `agent_cycle` and `agent_cycle_reverse` were the
    // two an earlier change took off the list; `session_child_first` gained the delegated-task
    // view, and `session_queued_prompts` gained the durable queue picker.
    assert_eq!(
        PRESSABLE_BUT_DEAD.len(),
        45,
        "the pressable-but-dead census changed size; that is a real event either way and the \
         count is pinned so it cannot pass unremarked"
    );
}

/// Where the prompt band starts in a rendered frame, counted back from the bottom.
///
/// Not located by the strip's own text: the strip degrades through four tiers and drops its
/// state word on the way, so `idle`/`working` is not a row every frame has — a 40-column frame
/// reporting a resolved model prints the model and the exit hint and no state at all.
///
/// Not located by the prompt's gutter marker either, and that is the more important exclusion:
/// `the_prompt_is_contained_by_a_gutter_and_a_spacer_at_every_supported_width` asserts the
/// located row *starts with* that marker, so finding the row by the marker would make its
/// assertion true by construction. A guard that reads the value it is checking only restates it.
///
/// So the position is counted from the frame's last row through the two lengths `render` gives
/// those bands. A `render` that stopped consulting either would point this at the wrong row and
/// the content assertions would fail, which is the direction that has to work.
/// `width` as well as `height`, because the tail depends on the welcome block's height and
/// that block loses the wordmark below forty columns and gains hint rows below sixty. A
/// helper that assumed one width would point every caller at the wrong row on the others.
fn prompt_first(rendered: &[String], screen: &SessionScreen, width: u16, height: u16) -> usize {
    let (band, tail) = screen.prompt_and_tail(width, height);
    // Every band `render` draws below the prompt is subtracted. The reply identity and
    // unused transcript spacer are above it.
    rendered
        .len()
        .saturating_sub(usize::from(tail) + usize::from(info_rows(height)) + usize::from(band))
}

/// The prompt band's rows, located rather than assumed to be the frame's last band.
///
/// Twelve assertions used to index the frame absolutely — `rendered[6]`, `rendered[len - 2]` —
/// which silently encoded "the prompt is the final band". It is not, while the transcript is
/// empty: a tail below it lifts the welcome block and the input into one centred column. An
/// absolute index does not fail informatively when that changes; it reports that some
/// unrelated row lacks the caret. Locating the band keeps every one of those assertions about
/// the band itself.
fn prompt_band(
    rendered: &[String],
    screen: &SessionScreen,
    width: u16,
    height: u16,
) -> Vec<String> {
    let first = prompt_first(rendered, screen, width, height);
    let band = usize::from(screen.prompt_and_tail(width, height).0);
    let (x, columns) = composer_span(screen, width, height);
    rendered[first.min(rendered.len())..(first + band).min(rendered.len())]
        .iter()
        .map(|row| {
            row.chars()
                .skip(x)
                .take(columns)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// The composer's first column and its width, for a `width` by `height` frame.
///
/// The band is narrower than the frame while the transcript is empty, so a whole frame row
/// carries the body surface's margin and the focus rules as well as the band. Every claim about
/// the band's own containment — its marker, its right inset, its spacer — has to be read from
/// the band's columns or it is a claim about the margin, which would be satisfied by a composer
/// with no chrome at all.
///
/// Through the production `composer_region` rather than by re-deriving the arithmetic: a second
/// copy that drifted would point every assertion below at the wrong columns, and the row it read
/// would be blank, so the failure would name the wrong thing.
fn composer_span(screen: &SessionScreen, width: u16, height: u16) -> (usize, usize) {
    // The production predicate, not `messages().is_empty()`: a session notice leaves the
    // composer bounded by the body, so a helper that counted notices as a transcript would hand
    // every assertion below the wrong columns and read the margin instead of the band.
    let empty = !screen.transcript.transcript().conversation_started();
    let sidebar = sidebar_drawn(screen.sidebar_visible(), empty, width);
    let region = composer_region(
        content_bounds(Rect::new(0, 0, width, height), sidebar),
        empty,
    );
    (usize::from(region.x), usize::from(region.width))
}

fn expected_welcome_composer_span(width: u16) -> (usize, usize) {
    let margin = usize::from(width > 2);
    let available = usize::from(width).saturating_sub(margin * 2);
    let columns = available.min(usize::from(WELCOME_COMPOSER_MAX_COLS));
    (margin + (available - columns) / 2, columns)
}

/// The prompt band's spacer, which is its last row whenever the band has more than one.
fn spacer_row(rendered: &[String], screen: &SessionScreen, width: u16, height: u16) -> String {
    prompt_band(rendered, screen, width, height)
        .last()
        .cloned()
        .unwrap_or_default()
}

/// The prompt band's first row, the one the caret and the gutter marker share.
fn content_row(rendered: &[String], screen: &SessionScreen, width: u16, height: u16) -> String {
    prompt_band(rendered, screen, width, height)
        .first()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn the_prompt_is_contained_by_a_gutter_and_a_spacer_at_every_supported_width() {
    // The defect this pins was reported from a real 120x32 pane: the whole prompt rendered as
    // one bare `▏` on the terminal's final row, with nothing between it and the screen edge.
    // Asserting `prompt_rows` would not have caught it — the band was the right height and
    // empty. Only the frame says whether anything is *in* it, so this reads the frame.
    for width in [200u16, 120, 80, 60, 40] {
        let (mut blank, _shutdown) = screen();
        let rendered = rows(&render_offscreen(&mut blank, width, 24).expect("infallible"));
        let band = content_row(&rendered, &blank, width, 24);
        assert!(
            band.starts_with("› "),
            "at {width} columns the prompt has no gutter marker: {band:?}"
        );
        assert!(
            band.contains('▏'),
            "at {width} columns the caret is not in the band: {band:?}"
        );
        // A buffer tall enough to fill the band, because that is the only state in which the
        // spacer is load-bearing: an empty prompt never reaches the last row, so asserting the
        // blank against one would pass with no spacer at all.
        let (mut tall, _shutdown) = screen();
        tall.editor.set_text(
            &(0..12)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let filled = rows(&render_offscreen(&mut tall, width, 24).expect("infallible"));
        assert!(
            filled.iter().any(|row| row.contains("line 11")),
            "at {width} columns the tall fixture did not fill the band: {filled:?}"
        );
        assert_eq!(
            spacer_row(&filled, &tall, width, 24),
            "",
            "at {width} columns the prompt is flush against its last row, which is the \
             reported defect"
        );
        assert_eq!(
            spacer_row(&rendered, &blank, width, 24),
            "",
            "at {width} columns an empty prompt has no spacer under it"
        );
        // The right inset, measured rather than assumed: a full row of typed text must stop
        // short of the frame, or the band is only nominally contained.
        let (mut typed, _shutdown) = screen();
        typed.editor.set_text(&"x".repeat(usize::from(width) * 2));
        let rendered = rows(&render_offscreen(&mut typed, width, 24).expect("infallible"));
        let band = content_row(&rendered, &typed, width, 24);
        // Against the composer's own columns, not the frame's. Comparing to the frame would be
        // trivially true wherever the box is narrower than the frame — 200 and 120 columns
        // here — so the inset would go unmeasured at exactly the widths the box is centred at.
        let columns = composer_span(&typed, width, 24).1;
        assert!(
            crate::views::display_width(&band) < columns,
            "at {width} columns the prompt used its last column, leaving no right inset: \
             {} of {columns}",
            crate::views::display_width(&band)
        );
    }
}

#[test]
fn the_prompt_keeps_a_typeable_row_on_a_twenty_by_ten_pane() {
    // `prompt_rows` refuses to panic at this size and has its own tests for that. What chrome
    // adds is a second way to lose: a spacer or a gutter that takes the band's only row leaves
    // a prompt that cannot be typed into, which no arithmetic assertion would notice.
    let (mut screen, _shutdown) = screen();
    screen.editor.set_text("hi");
    let rendered = rows(&render_offscreen(&mut screen, 20, 10).expect("infallible"));
    let band = content_row(&rendered, &screen, 20, 10);
    assert!(
        band.contains("hi") && band.contains('▏'),
        "the 20x10 prompt lost its content row to chrome: {band:?}"
    );
    assert_eq!(
        spacer_row(&rendered, &screen, 20, 10),
        "",
        "the spacer row is missing at 20x10"
    );
}

#[test]
fn prompt_chrome_is_dropped_before_the_buffer_is_squeezed() {
    // Chrome that costs more than it gives is dropped, and the threshold is asserted on both
    // sides so neither branch can rot into being unreachable — the failure mode that made a
    // fourth attribution level dead code elsewhere in this repo.
    let wide = Rect::new(0, 0, 20, 2);
    let (gutter, editor) = prompt_frame(wide);
    assert!(gutter.is_some(), "20 columns can afford the gutter");
    assert_eq!(editor.width, 20 - PROMPT_GUTTER_COLS - PROMPT_RIGHT_INSET);
    assert_eq!(editor.height, 1, "the spacer took the second of two rows");

    let narrow = Rect::new(0, 0, PROMPT_GUTTER_COLS + PROMPT_RIGHT_INSET + 1, 2);
    let (gutter, editor) = prompt_frame(narrow);
    assert!(
        gutter.is_none(),
        "a pane this narrow kept chrome instead of columns"
    );
    assert_eq!(
        editor.width, narrow.width,
        "dropping the gutter must give the columns back to the buffer"
    );

    let single = Rect::new(0, 0, 40, 1);
    let (_gutter, editor) = prompt_frame(single);
    assert_eq!(
        editor.height, 1,
        "a one-row band spent its only row on the spacer"
    );
}

#[test]
fn the_empty_prompt_says_what_to_do_and_a_typed_one_does_not() {
    let (mut screen, _shutdown) = screen();
    let empty = rows(&render_offscreen(&mut screen, 60, 24).expect("infallible")).join("\n");
    assert!(
        empty.contains(PROMPT_PLACEHOLDER),
        "the empty prompt offers no hint:\n{empty}"
    );
    screen.editor.set_text("a real prompt");
    let typed = rows(&render_offscreen(&mut screen, 60, 24).expect("infallible")).join("\n");
    assert!(
        !typed.contains(PROMPT_PLACEHOLDER),
        "the hint survived the text it was standing in for:\n{typed}"
    );
}

/// A notice long enough that no supported width can render it whole.
const OVERLONG_NOTICE: &str = "MCP server `context7` was not toggled: lifecycle worker is busy \
     or unavailable, and the request was dropped rather than queued because a queued toggle \
     would apply at an unpredictable time long after the user asked for it and the panel would \
     disagree with the server about its own state for the whole interval";

/// The notice's own rows, cut to the transcript's columns and stripped of the `! ` marker.
///
/// The cut is the load-bearing part. `rows` returns a whole frame row, so with the ambient
/// panel drawn a notice row carries the panel's text too — measuring that row's width measures
/// the terminal, and reassembling it interleaves the panel into the sentence. Both mistakes
/// were made writing these tests, and both looked like failures of the shipping code.
fn notice_body(rendered: &[String]) -> Vec<String> {
    notice_body_at(rendered, ToastLevel::Warning)
}

/// The same, for a notice drawn at `level`.
///
/// Split out because the marker is no longer always `!`: a notice carries one of `§11.5`'s four
/// levels and prints that level's glyph, so a helper hard-coded to the warning marker would
/// silently return no rows for a success and every assertion over it would be about nothing —
/// which is the failure mode this helper's own docs already warn about.
fn notice_body_at(rendered: &[String], level: ToastLevel) -> Vec<String> {
    let prefix = format!("▲ {} ", level.glyph());
    rendered
        .iter()
        .filter(|row| row.starts_with(&prefix))
        .map(|row| {
            let main = row.split('│').next().unwrap_or(row);
            main.trim_start_matches(&prefix).trim_end().to_owned()
        })
        .collect()
}

fn noticed(text: &str, width: u16) -> Vec<String> {
    let (mut screen, _shutdown) = screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::notice(text));
    rows(&render_offscreen(&mut screen, width, 24).expect("infallible"))
}

/// The same, in a session that has actually started.
///
/// The distinction is load-bearing for anything that needs the ambient panel on screen: a
/// notice is a [`Role::System`] message and no longer counts as a conversation, so
/// `sidebar_drawn` keeps the panel off a transcript that holds nothing else — see
/// `Transcript::conversation_started`. A fixture that pushed only a notice would therefore
/// assert about a panel that is not drawn, which is an assertion about nothing.
fn noticed_in_conversation(text: &str, width: u16) -> Vec<String> {
    let (mut screen, _shutdown) = screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("a first prompt"));
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::notice(text));
    rows(&render_offscreen(&mut screen, width, 24).expect("infallible"))
}

#[test]
fn a_long_notice_stops_at_the_cap_and_says_how_much_it_kept_back() {
    for width in [60u16, 40, 24] {
        let rendered = noticed(OVERLONG_NOTICE, width);
        let body = notice_body(&rendered);
        // The literal, not `NOTICE_MAX_ROWS`. An assertion that reads the constant follows it:
        // raising the cap to six would keep this green while the notice quietly took another
        // row of the reply. Five rows out of a 24-row frame is the property; the constant is
        // pinned separately below so the two cannot drift apart unremarked.
        assert_eq!(
            body.len(),
            5,
            "at {width} columns the notice took {} rows: {body:?}",
            body.len()
        );
        // The affordance before the count, because a cut with no mark is indistinguishable
        // from a sentence that simply ended — which is what the reported capture looked like.
        let last = body.last().expect("the cap is not zero");
        assert!(
            last.contains(crate::views::message::ELIDED) && last.contains("more lines"),
            "at {width} columns the notice was cut with nothing to say so: {last:?}"
        );
        assert!(
            last.split_whitespace()
                .any(|word| word.parse::<usize>().is_ok()),
            "the overflow row states no count, so the reader cannot tell how much is missing: \
             {last:?}"
        );
    }
}

#[test]
fn the_notice_cap_is_the_number_the_assertions_were_written_against() {
    assert_eq!(
        crate::views::message::NOTICE_MAX_ROWS,
        5,
        "the notice cap moved; the row assertions in this file spell 5 as a literal on purpose, \
         so change them together and re-derive the number from a measurement"
    );
}

#[test]
fn a_short_notice_is_shown_whole_with_no_overflow_row() {
    // The other side of the cap. Without this the cap could be `0` and the test above would
    // still pass on its `ELIDED` assertion alone.
    let body = notice_body(&noticed("agent set to plan for the next turn", 80));
    assert_eq!(
        body.len(),
        1,
        "a one-line notice did not fit in one row: {body:?}"
    );
    assert!(
        !body[0].contains("more lines"),
        "an uncut notice claimed it was cut: {body:?}"
    );
}

#[test]
fn a_notice_never_reaches_the_sidebar_column_with_or_without_the_panel() {
    // The reported capture: rows 2 and 3 of a 120-column frame ended flush against the
    // panel's `│`, and the guidance the sentence carried was read as truncated. The assertion
    // is positional rather than a substring search, because "the text stops before the rule"
    // is the property and a substring test cannot see a missing column.
    let with_panel = noticed_in_conversation(OVERLONG_NOTICE, crate::views::SIDEBAR_MIN_WIDTH);
    // The panel's own left rule, which sits at the start of the sidebar's area; the gap
    // column is the one immediately before it.
    let rule_column =
        usize::from(crate::views::SIDEBAR_MIN_WIDTH - crate::views::ambient::SIDEBAR_WIDTH);
    let mut inspected = 0;
    for row in with_panel.iter().filter(|row| row.starts_with("▲ ! ")) {
        inspected += 1;
        let columns: Vec<char> = row.chars().collect();
        assert_eq!(
            columns.get(rule_column),
            Some(&'│'),
            "the sidebar rule is not where the layout puts it: {row:?}"
        );
        assert_eq!(
            columns.get(rule_column - 1),
            Some(&' '),
            "a notice row touched the sidebar rule, which reads as the panel cutting the \
             sentence: {row:?}"
        );
    }
    assert!(
        inspected >= 2,
        "the wrapped notice produced only {inspected} rows to check"
    );

    // With the panel hidden at the *same* total width, the notice must take the columns it
    // gave up. Compared at one width on purpose: a narrower frame would be narrower for
    // reasons that have nothing to do with the panel, and the comparison would prove nothing.
    let (mut hidden, _shutdown) = screen();
    // The same first prompt as `with_panel`, so the toggle really is the only difference
    // between the two frames being compared.
    hidden
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("a first prompt"));
    hidden
        .transcript_mut()
        .transcript_mut()
        .push(Message::notice(OVERLONG_NOTICE));
    hidden.handle_action(action("sidebar_toggle"), &press_none());
    let rendered = rows(
        &render_offscreen(&mut hidden, crate::views::SIDEBAR_MIN_WIDTH, 24).expect("infallible"),
    );
    let widest = |rows: &[String]| {
        notice_body(rows)
            .iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0)
    };
    assert!(
        rendered
            .iter()
            .all(|row| !row.contains("no usage reported yet")),
        "the panel is still drawn after the toggle, so this proves nothing"
    );
    assert!(
        widest(&rendered) > widest(&with_panel),
        "hiding the panel did not widen the notice: {} columns either way",
        widest(&rendered)
    );
    assert!(
        widest(&rendered) <= usize::from(crate::views::SIDEBAR_MIN_WIDTH),
        "the notice overflowed a panel-less frame: {} columns",
        widest(&rendered)
    );
}

#[test]
fn a_notice_wraps_between_words_and_never_inside_one() {
    // A prior fix in this crate broke `discar`/`ded`. Reassembling the rows and requiring the
    // words back is what catches that, rather than eyeballing a frame.
    let joined = notice_body(&noticed(OVERLONG_NOTICE, 60))
        .into_iter()
        .filter(|row| !row.contains("more lines"))
        .collect::<Vec<_>>()
        .join(" ");
    for word in ["lifecycle", "unavailable", "unpredictable", "context7"] {
        if OVERLONG_NOTICE.contains(word) && joined.contains(&word[..4]) {
            assert!(
                joined.contains(word),
                "`{word}` was split across rows: {joined:?}"
            );
        }
    }
}

/// A rendered row's text with the blank ratatui writes into a wide glyph's second cell removed.
///
/// [`crate::views::testkit::rows`] emits one character per *cell*, so a double-width glyph
/// arrives as the glyph followed by a space that is not in the content. Stripping those is what
/// makes the row comparable to the string it was rendered from — and measuring the raw row with
/// `display_width` instead counts a wide glyph as three columns, which is how the first version
/// of the test below "failed" on correct output.
fn cells_to_text(row: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for character in row.chars() {
        if skip && character == ' ' {
            skip = false;
            continue;
        }
        skip = unicode_width::UnicodeWidthChar::width(character) == Some(2);
        out.push(character);
    }
    out
}

#[test]
fn a_cjk_notice_keeps_every_glyph_inside_the_transcript_column() {
    // Columns, not characters — and the assertion has to be "nothing was lost" rather than
    // "the row is narrow enough", because the row is one character per cell by construction and
    // so is trivially in range. A wrap that measured characters would break CJK at twice the
    // available columns, `ruled` would cut each row at the frame, and the glyphs past the cut
    // would be discarded silently. Reassembling the rows and requiring a prefix of the source
    // is what sees that.
    let text = "服务器连接失败：生命周期工作线程正忙或不可用，这条请求被丢弃而不是排队，\
                因为排队的切换会在用户请求之后一段无法预测的时间才生效";
    for width in [crate::views::SIDEBAR_MIN_WIDTH, 80, 40] {
        let rendered = noticed(text, width);
        let body: Vec<String> = notice_body(&rendered)
            .into_iter()
            .filter(|row| !row.contains("more lines"))
            .map(|row| cells_to_text(&row).trim_end().to_owned())
            .collect();
        assert!(
            !body.is_empty(),
            "at {width} columns the CJK notice rendered nothing"
        );
        let reassembled: String = body.concat();
        assert!(
            text.starts_with(&reassembled),
            "at {width} columns the CJK notice lost or reordered glyphs:\n{reassembled}"
        );
        for row in &body {
            assert!(
                crate::views::display_width(row) <= usize::from(width),
                "a CJK notice row measures {} columns in a {width}-column frame: {row:?}",
                crate::views::display_width(row)
            );
        }
    }
}

/// A screen with agents to walk and a sink that records where they went.
fn cyclable() -> (SessionScreen, mpsc::Receiver<Selection>) {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, applied) = mpsc::channel(8);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_selection_sink(selections)
        .with_keymap(Keymap::defaults().expect("the shipped table builds"));
    screen.catalog_mut().agents = ["build", "general", "plan"]
        .into_iter()
        .map(|name| crate::views::picker::AgentEntry {
            name: String::from(name),
            description: String::from("an agent"),
        })
        .collect();
    screen.catalog_mut().agent = Some(String::from("build"));
    (screen, applied)
}

#[test]
fn tab_cycles_the_agent_through_the_production_dispatch_chain() {
    // Pressed as a key event through `KeyDispatcher`, not called as a method: the defect was
    // that `tab` resolved to `agent_cycle` correctly and no arm existed, so every assertion
    // short of the real chain passed while the key did nothing. The frame and the channel are
    // both read, because a switch the strip shows and the host never hears is the other half
    // of the same defect.
    let (screen, mut applied) = cyclable();
    let mut dispatcher = KeyDispatcher::new(
        Keymap::defaults().expect("the shipped table builds"),
        scopes(),
        Box::new(DialogHost::new(ViewContext::defaults(), Box::new(screen))),
    );
    let result = dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        CrosstermEvent::Key(press(crossterm::event::KeyCode::Tab)),
    )));
    assert!(
        result.handled,
        "`tab` reached nothing; `agent_cycle` is unreachable"
    );
    let selection = applied
        .try_recv()
        .expect("`tab` reached no host; the agent did not actually change");
    assert_eq!(
        selection,
        Selection::Agent(String::from("general")),
        "`tab` did not move one place along the catalog"
    );
    let joined = rows(&render_offscreen(&mut dispatcher, 100, 14).expect("infallible")).join("\n");
    assert!(
        joined.contains("general"),
        "the switch reached the host but nothing on screen says so, which is \
         indistinguishable from a dead key:\n{joined}"
    );
}

#[test]
fn shift_tab_cycles_back_from_the_event_a_terminal_really_sends() {
    // `KeyCode::BackTab` with `SHIFT`, which is what crossterm reports for the `CSI Z` a
    // default terminal sends. Constructing `Tab` with `SHIFT` here would test the Kitty
    // encoding and pass against the broken spelling this test exists for.
    let (screen, mut applied) = cyclable();
    let mut dispatcher = KeyDispatcher::new(
        Keymap::defaults().expect("the shipped table builds"),
        scopes(),
        Box::new(DialogHost::new(ViewContext::defaults(), Box::new(screen))),
    );
    let mut event = press(crossterm::event::KeyCode::BackTab);
    event.modifiers = crossterm::event::KeyModifiers::SHIFT;
    let result = dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        CrosstermEvent::Key(event),
    )));
    assert!(
        result.handled,
        "shift-tab reached nothing from the key event a terminal actually sends"
    );
    assert_eq!(
        applied.try_recv().expect("shift-tab reached no host"),
        Selection::Agent(String::from("plan")),
        "cycling backwards from the first agent must wrap to the last"
    );
}

#[test]
fn cycling_wraps_at_both_ends_and_reports_a_refused_sink() {
    let (mut screen, mut applied) = cyclable();
    screen.catalog_mut().agent = Some(String::from("plan"));
    screen.handle_action(action("agent_cycle"), &press_none());
    assert_eq!(
        applied.try_recv().expect("delivered"),
        Selection::Agent(String::from("build")),
        "cycling forward off the last agent must wrap to the first"
    );
    assert_eq!(
        screen.welcome_mut().facts().agent.as_deref(),
        Some("build"),
        "the welcome identity did not follow the selected agent"
    );

    // No sink at all: the notice-vs-silence decision is what separates this from the defect
    // family. A key that appears to switch and reaches nothing must say so.
    let (sender, _shutdown) = terminal_event_channel();
    let mut orphan = SessionScreen::new(ViewContext::defaults(), sender);
    orphan.catalog_mut().agents = ["build", "plan"]
        .into_iter()
        .map(|name| crate::views::picker::AgentEntry {
            name: String::from(name),
            description: String::new(),
        })
        .collect();
    orphan.handle_action(action("agent_cycle"), &press_none());
    let toasts = ActionComponent::drain_toasts(&mut orphan);
    assert!(
        toasts
            .iter()
            .any(|toast| toast.level() == ToastLevel::Warning
                && toast.text().contains("not applied")),
        "a cycle that reached no host reported success or nothing: {toasts:?}"
    );
}

#[test]
fn cycling_a_catalog_with_nothing_to_cycle_says_so_rather_than_going_quiet() {
    // Silence here would be indistinguishable from the dead key this action shipped as, which
    // is the whole reason the empty and single-agent cases are not early `IGNORED` returns.
    for agents in [Vec::new(), vec![String::from("build")]] {
        let (sender, _shutdown) = terminal_event_channel();
        let mut screen = SessionScreen::new(ViewContext::defaults(), sender);
        let count = agents.len();
        screen.catalog_mut().agents = agents
            .into_iter()
            .map(|name| crate::views::picker::AgentEntry {
                name,
                description: String::new(),
            })
            .collect();
        let result = screen.handle_action(action("agent_cycle"), &press_none());
        assert!(result.handled, "a {count}-agent catalog left the key dead");
        let toasts = ActionComponent::drain_toasts(&mut screen);
        assert!(
            toasts
                .iter()
                .any(|toast| toast.text().contains("no other agent")),
            "a {count}-agent catalog cycled in silence: {toasts:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// `§11.5` grades: a notice says which of the four it is
// ---------------------------------------------------------------------------

/// A committed model choice is reported as a success, not as a warning.
///
/// Read off the toast rather than off a rendered transcript notice, because that is where the
/// confirmation now goes — see [`SessionScreen::adopt`]. The grade distinction this test exists
/// for is unchanged and is if anything more directly checked: `Toast::level` is the level, where
/// the previous form inferred it from a marker glyph painted into a row.
#[test]
fn a_delivered_model_choice_is_reported_with_the_success_affordance() {
    let (selections, _keep) = mpsc::channel(4);
    let (screen, _shutdown) = screen();
    let mut screen = screen.with_selection_sink(selections);
    screen.adopt(
        crate::views::picker::MODEL_DIALOG_ID,
        "amazon-bedrock/amazon.nova-lite-v1:0",
    );
    assert_eq!(
        screen.welcome_mut().facts().model.as_deref(),
        Some("amazon-bedrock/amazon.nova-lite-v1:0"),
        "the welcome identity did not follow the selected model"
    );
    let toasts = ActionComponent::drain_toasts(&mut screen);
    assert!(
        toasts.iter().any(|toast| {
            toast.level() == ToastLevel::Success
                && toast
                    .text()
                    .contains("model set to amazon-bedrock/amazon.nova-lite-v1:0")
        }),
        "a model that was set is not reported at success grade: {toasts:?}"
    );
    // The other half, and the half that fails on the shipped behaviour: the same sentence must
    // not also be reachable at warning grade. Asserting only the success toast would pass a
    // surface that raised both, and `!` on a confirmation is the reported defect.
    assert!(
        !toasts.iter().any(|toast| {
            toast.level() == ToastLevel::Warning && toast.text().contains("model set to")
        }),
        "the model confirmation still carries the warning affordance: {toasts:?}"
    );
    // And it does not also write history, which is the defect the toast replaces: a
    // confirmation left in the transcript is exported and re-read as part of the conversation.
    assert!(
        rows(&render_offscreen(&mut screen, 120, 24).expect("infallible"))
            .iter()
            .all(|row| !row.contains("model set to")),
        "the confirmation is still pushed onto the transcript as well"
    );
}

/// A model choice nobody is listening for stays a warning, because it did not take effect.
#[test]
fn a_refused_model_choice_keeps_the_warning_affordance() {
    // No selection sink, which is the refusal path.
    let (mut screen, _shutdown) = screen();
    screen.adopt(crate::views::picker::MODEL_DIALOG_ID, "p/m");
    let toasts = ActionComponent::drain_toasts(&mut screen);
    assert!(
        toasts.iter().any(|toast| {
            toast.level() == ToastLevel::Warning
                && toast.text().contains("not applied: nothing is listening")
        }),
        "a refused selection is not reported at warning grade: {toasts:?}"
    );
    assert!(
        !toasts
            .iter()
            .any(|toast| toast.level() == ToastLevel::Success),
        "a selection that reached nothing was reported as a success: {toasts:?}"
    );
}

// ---------------------------------------------------------------------------
// The welcome tail, and the fact that it costs a used session nothing
// ---------------------------------------------------------------------------

/// The input is lifted off the bottom edge while the transcript is empty, and only then.
#[test]
fn the_welcome_screen_lifts_the_prompt_and_a_used_session_does_not() {
    for (width, height) in [(120u16, 34u16), (60, 34), (200, 50), (80, 24)] {
        let (mut blank, _shutdown) = screen();
        let rendered = rows(&render_offscreen(&mut blank, width, height).expect("infallible"));
        let first = prompt_first(&rendered, &blank, width, height);
        let band = usize::from(prompt_rows(blank.editor.height(), height));
        assert!(
            first + band < rendered.len(),
            "at {width}x{height} the welcome prompt still reaches the frame's last row"
        );
        // The lift is the whole point, so it is asserted as a distance and not as "non-zero":
        // a one-row tail would satisfy `<` above and leave the complaint exactly as reported.
        assert!(
            rendered.len() - (first + band) >= 2,
            "at {width}x{height} the welcome prompt is lifted by only {} row(s)",
            rendered.len() - (first + band)
        );

        let (mut used, _shutdown) = screen();
        used.transcript_mut()
            .transcript_mut()
            .push(Message::user("a first prompt"));
        let rendered = rows(&render_offscreen(&mut used, width, height).expect("infallible"));
        let first = prompt_first(&rendered, &used, width, height);
        let band = usize::from(prompt_rows(used.editor.height(), height));
        assert_eq!(
            first + band + usize::from(info_rows(height)),
            rendered.len(),
            "at {width}x{height} a used session pays for the welcome tail it cannot see"
        );
    }
}

/// The tail cannot starve the region it centres the input inside, including at 20x10.
///
/// Restated for the arithmetic that replaced the halved-slack tail, not relaxed: the claim is
/// still "the body keeps a row", and it is still checked at every height from one to twelve.
/// What changed is where the guarantee comes from. Halving guaranteed it structurally — half of
/// anything leaves the other half — and the new formula does not halve the region at all, it
/// takes `min(centred, body_max - head)`. So the guarantee now rests entirely on that second
/// term, and specifically on its `max(1)`: the head is what the body is reserved *for*, and a
/// head measured as zero rows would otherwise let the tail take everything.
///
/// The zero-head case is reachable rather than hypothetical, which is what makes the range here
/// the right one: a frame these short gives the welcome screen a region it cannot state a brand
/// into, and `render` still has to hand the body a row.
#[test]
fn the_welcome_tail_never_takes_the_row_the_welcome_needs() {
    for height in 1..=12u16 {
        let (mut screen, _shutdown) = screen();
        screen.editor.set_text("hi");
        let (band, tail) = screen.prompt_and_tail(20, height);
        let info = info_rows(height);
        let body_max = height.saturating_sub(info.saturating_add(band));
        assert!(
            info + band + tail < height || height <= info + band,
            "at {height} rows the chrome and the tail leave the body nothing: \
             info {info} + prompt {band} + tail {tail}"
        );
        // The `max(1)` in `welcome_tail_rows`, asserted as the property it buys rather than as
        // the expression: whatever the head measures, the body is left at least one row to put
        // it in. Drop the `max(1)` and a frame whose head is empty fails here rather than
        // panicking somewhere downstream.
        assert!(
            tail < body_max || body_max == 0,
            "at {height} rows the tail took all {body_max} rows of the body"
        );
        // The panic guard, exercised through the frame rather than the arithmetic: a tail that
        // fabricated a row the buffer does not own panics inside ratatui.
        let rendered = rows(&render_offscreen(&mut screen, 20, height).expect("infallible"));
        assert_eq!(rendered.len(), usize::from(height));
    }
}

/// The empty screen's **input band** sits on the frame's middle, at five real terminal sizes.
///
/// # What is centred, and why the previous two answers were both wrong
///
/// Reported three times. The first arrangement pinned the prompt to the bottom outright. The
/// second lifted it by a capped half-region tail, which satisfied
/// `the_welcome_screen_lifts_the_prompt_and_a_used_session_does_not` — a six-row lift — and
/// still left nine dead rows under a prompt at row 22 of 32, because a *lift* says nothing
/// about where anything ends up. The third balanced the whole *composite*: block, strip and
/// prompt as one object, blank rows above the brand against blank rows below the composer. That
/// arithmetic was exact, provable, and measured on a real pane as an input box at rows 23–26 of
/// 32 — because centring an object whose top nine-tenths is text puts the *text* in the middle
/// and the input near the bottom.
///
/// So the thing measured here is the band, and it is measured from the frame's two edges: the
/// rows above it and the rows below it must differ by at most one, which is the odd row an odd
/// frame cannot split. That is what "the input box is in the centre of the area" says, and
/// unlike the composite version it cannot be satisfied by a screen with the input anywhere else.
///
/// # Both edges are found by paint, and one of them was once found by arithmetic
///
/// The version this replaces took its lower edge from `prompt_first` — the same function
/// `render` splits by — so it asked "did the tail get the rows the formula says" and never
/// "is the row above the tail something a reader can see". It passed against a build the user
/// measured as a one-row prompt with nine dead rows beneath it.
///
/// Both edges of the band are therefore located by background: the composer carries
/// `element` and every surrounding row carries `surface`. Revert the band's fill to
/// `text` and its rows join the surface, so this fails without consulting the layout
/// arithmetic it is meant to verify.
///
/// Read at the composer's own column rather than at column zero, because the box no longer spans
/// the frame: column zero is the body surface's margin on every size here, so a probe there
/// would find no composer at all.
///
/// # The foot is required to be below it, which is what stops the head from being trimmed again
///
/// A band centred by *deleting* rows above it would satisfy the skew bound while removing the
/// screen. So the lead line — the one row `WelcomeView::foot` always emits — has to be found
/// strictly below the band, and the wordmark strictly above it. Together they say the surface
/// still brackets the input rather than having been cut away from it.
///
/// Measured at five sizes rather than derived at one: 24 is the shortest common pane and the
/// one the old arrangement could not centre at any tail length, 32 is the reported one, 50 is
/// where the earlier six-row cap did its worst, and 80 columns is the width with no sidebar.
#[test]
fn the_prompt_band_is_centred_on_the_frame() {
    for (width, height) in [(120u16, 24u16), (120, 32), (120, 50), (80, 32), (200, 50)] {
        let (mut blank, _shutdown) = screen();
        let buffer = render_offscreen(&mut blank, width, height).expect("infallible");
        let rendered = rows(&buffer);
        let palette = blank.context.palette();
        let surface = ratatui::style::Color::from(palette.background_panel);
        let element = ratatui::style::Color::from(palette.background_element);
        assert_ne!(
            surface, element,
            "the theme gives the composer no surface to be distinct in"
        );

        let probe = u16::try_from(composer_span(&blank, width, height).0).expect("in frame");
        let bg = |y: usize| buffer[(probe, u16::try_from(y).expect("in frame"))].bg;
        let band_last = (0..rendered.len())
            .rposition(|y| bg(y) == element)
            .expect("the composer is painted in its own surface");
        let band_first = (0..=band_last)
            .rev()
            .take_while(|y| bg(*y) == element)
            .last()
            .expect("the run contains the row it ends on");
        assert!(
            band_first <= band_last && band_last + 1 < rendered.len(),
            "at {width}x{height} the composer run is {band_first}..={band_last} of {} rows, \
             which leaves no surface below it",
            rendered.len()
        );
        assert_eq!(
            bg(band_last + 1),
            surface,
            "at {width}x{height} the composer has no visible lower edge"
        );
        let last = band_last;
        let gap_above = band_first;
        let gap_below = rendered.len() - 1 - last;

        let skew = gap_above.abs_diff(gap_below);
        assert!(
            skew <= 1,
            "at {width}x{height} the input band a reader can see runs {band_first}..={last}, \
             which is {gap_above} rows from the top and {gap_below} from the bottom, a skew of \
             {skew}"
        );
        // Both halves non-zero, because a band flush against both edges would report a skew of
        // zero while being the layout this replaces.
        assert!(
            gap_above > 0 && gap_below > 0,
            "at {width}x{height} the band is flush against an edge: \
             {gap_above} above, {gap_below} below"
        );
        // The edges found by paint have to be the edges the split produced, or the measures
        // above are of different things and the skew is a coincidence.
        // `gap_below` counts the welcome tail and the info row, the only bands below
        // the composer.
        let (band, tail) = blank.prompt_and_tail(width, height);
        let info = info_rows(height);
        assert_eq!(
            (last + 1 - band_first, gap_below),
            (usize::from(band), usize::from(tail + info)),
            "at {width}x{height} the painted band is rows {band_first}..={last} with \
             {gap_below} rows below it, but the split gave the band {band}, the tail \
             {tail} and the info row {info}"
        );
        // And the surface still brackets the input rather than having been trimmed off it.
        let wordmark = rendered
            .iter()
            .position(|row| row.contains('█'))
            .expect("every size here is wide and tall enough for the wordmark");
        let lead = rendered
            .iter()
            .position(|row| row.contains("type / for commands"))
            .expect("the welcome surface always teaches `/`");
        assert!(
            wordmark < band_first && lead > last,
            "at {width}x{height} the welcome surface no longer brackets the input: wordmark \
             on row {wordmark}, band {band_first}..={last}, lead line on row {lead}"
        );
    }
}

/// The ambient panel waits for a transcript, and appears the moment one exists.
///
/// The reported defect: the welcome screen drew Context / LSP / MCP down its right third, where
/// every figure the panel carries is zero or unresolved on a session that has not run a turn —
/// so a third of the frame stated nothing while pushing the brand and the composer off the axis
/// they are centred on.
///
/// # Both halves, at a width where the panel would otherwise be drawn
///
/// The absence alone is satisfiable by deleting the panel, and the presence alone by never
/// having hidden it, so neither half is a test on its own. Measured at 130 and 200 columns —
/// both at or above [`crate::views::SIDEBAR_MIN_WIDTH`], which is what makes the empty frame's
/// silence a decision rather than the width threshold doing its usual job.
///
/// The width threshold is deliberately *not* the mechanism: `sidebar_drawn` adds a term rather
/// than moving the bar, and `session_screen_drops_the_panel_rather_than_squeezing_it` still pins
/// 120 as the boundary for a used session. Raising `SIDEBAR_MIN_WIDTH` instead would have taken
/// the panel away from the state that has something to put in it.
///
/// `sidebar_visible()` is asserted true throughout, so what is being observed is the render
/// decision and not the toggle: a screen that answered this by flipping the user's own toggle
/// would leave the panel gone after the first message until they pressed it back.
#[test]
fn the_ambient_panel_waits_for_a_transcript() {
    for width in [130u16, 200] {
        assert!(
            width >= crate::views::SIDEBAR_MIN_WIDTH,
            "{width} is below the threshold, so this size proves nothing about the empty frame"
        );
        let (mut screen, _shutdown) = screen();
        screen.sidebar_mut().ambient_mut().lsp = vec![crate::views::ambient::Service::new(
            "rust-analyzer",
            crate::views::ambient::Health::Ready,
        )];
        assert!(
            screen.sidebar_visible(),
            "the fixture starts with the panel toggled off, so its absence would prove nothing"
        );

        let empty = rows(&render_offscreen(&mut screen, width, 30).expect("infallible"));
        // Located by the panel's own section headings and by a service only it names. `Context`
        // and `MCP` also occur in the welcome census, so neither is sufficient alone — this is
        // the same needle-collision hazard `session_screen_shows_the_welcome_surface_only_while_
        // the_transcript_is_empty` records.
        let panel_row = |frame: &[String]| {
            frame
                .iter()
                .position(|row| row.contains("rust-analyzer"))
                .filter(|_| {
                    frame
                        .iter()
                        .any(|row| row.contains("no usage reported yet"))
                })
        };
        assert_eq!(
            panel_row(&empty),
            None,
            "at {width} columns the ambient panel is drawn on the welcome screen, where every \
             figure it carries is zero or unresolved:\n{}",
            empty.join("\n")
        );

        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::user("the first thing anyone types"));
        let used = rows(&render_offscreen(&mut screen, width, 30).expect("infallible"));
        assert!(
            panel_row(&used).is_some(),
            "at {width} columns the panel never came back once there was a transcript beside \
             it, so it was removed rather than deferred:\n{}",
            used.join("\n")
        );
        assert!(
            screen.sidebar_visible(),
            "the panel returned by flipping the user's own toggle rather than by the render \
             decision, so the first message would silently re-enable a panel they turned off"
        );
    }
}

/// The composer occupies a central region with air on both sides, and closes with visible edges.
///
/// The reported defect, in the owner's words: the input box took the whole frame rather than a
/// central region, and could not be told from the band around it. Those are one complaint — a
/// region as wide as the frame has no left or right edge for the eye to close, so no background
/// choice can make it read as a box.
///
/// # Three properties, because any one alone is satisfiable by the defect
///
/// * The band's columns are strictly fewer than the frame's, and *centred* — the margins on the
///   two sides differ by at most the odd column. Narrowing alone would allow a box pinned left.
/// * The margin columns carry the body surface, so what is beside the box is the screen behind
///   it rather than an unpainted seam holding ratatui's `Color::Reset`.
/// * The two columns immediately outside the box carry the rule glyphs, which is what closes it.
///
/// The welcome composer is bounded for readability; a separate assertion below proves that the
/// cap disappears after the first user message.
#[test]
fn the_welcome_composer_has_a_readable_maximum_width_and_visible_edges() {
    for (width, height) in [
        (200u16, 50u16),
        (120, 32),
        (120, 24),
        (80, 24),
        (60, 24),
        (20, 10),
    ] {
        let (mut blank, _shutdown) = screen();
        let buffer = render_offscreen(&mut blank, width, height).expect("infallible");
        let rendered = rows(&buffer);
        let (x, columns) = composer_span(&blank, width, height);
        let first = prompt_first(&rendered, &blank, width, height);
        let band = usize::from(blank.prompt_and_tail(width, height).0);
        let row = u16::try_from(first).expect("in frame");
        let palette = blank.context.palette();
        let surface = ratatui::style::Color::from(palette.background_panel);

        assert_eq!(
            (x, columns),
            expected_welcome_composer_span(width),
            "at {width}x{height} the welcome composer did not stay centred within its readable \
             maximum width"
        );
        let right = usize::from(width) - x - columns;
        assert!(
            x > 0 && right > 0 && x.abs_diff(right) <= 1,
            "at {width}x{height} the composer is not centred: {x} columns of air on the left \
             and {right} on the right"
        );
        // The margin belongs to the body surface. An unpainted margin would keep ratatui's
        // `Color::Reset` and render as the terminal's own background — the colour seam the
        // centring band's fill exists to remove, reintroduced sideways.
        for probe in [0, width - 1] {
            assert_eq!(
                buffer[(probe, row)].bg,
                surface,
                "at {width}x{height} column {probe} beside the composer is not the body surface"
            );
        }
        // And the edges, on every row of the input box.
        let left_rule = u16::try_from(x).expect("in frame") - 1;
        let right_rule = u16::try_from(x + columns).expect("in frame");
        for offset in 0..band {
            let y = row + u16::try_from(offset).expect("in frame");
            assert_eq!(
                (
                    buffer[(left_rule, y)].symbol(),
                    buffer[(right_rule, y)].symbol()
                ),
                (COMPOSER_LEFT_RULE, COMPOSER_RIGHT_RULE),
                "at {width}x{height} row {offset} of the composer is not closed on both sides, \
                 so the box has no edge for the eye to follow"
            );
        }
    }
}

#[test]
fn a_started_conversation_removes_the_welcome_width_cap() {
    let width = 200;
    let height = 50;
    let (screen, _shutdown) = conversing();
    let (x, columns) = composer_span(&screen, width, height);
    let sidebar = sidebar_drawn(screen.sidebar_visible(), false, width);
    let content = content_bounds(Rect::new(0, 0, width, height), sidebar);

    assert_eq!(
        (x, columns),
        (
            usize::from(content.x.saturating_add(1)),
            usize::from(content.width.saturating_sub(2)),
        ),
        "a conversation should use its whole left content column with one-cell margins"
    );
    assert!(
        columns > usize::from(WELCOME_COMPOSER_MAX_COLS),
        "the active composer is still capped at the welcome width: {columns}"
    );
}

/// The whole input band is painted in a surface of its own, so a reader can see how tall it is.
///
/// This is what makes "the input is centred" a claim about something visible. The band is four
/// rows and only the first carries text, so unless the other three are painted in a background
/// the surrounding surface does not use, the box reads as one row of text over three rows of air
/// — which is the complaint, twice, not the fix.
///
/// # The earlier version of this test asserted the defect
///
/// It compared the band's background against the frame's **last row** and required them to
/// differ. They did, and it passed, and the screen was still wrong: the last row was the centring
/// band, which nothing painted, so it held ratatui's `Color::Reset`. The comparison was therefore
/// against the *absence* of a paint decision rather than against a colour the design chose, and
/// `text` — whose background is the transcript's own `background_panel` — cleared it easily while
/// being exactly the colour that makes the box vanish.
///
/// So the comparison is against `background_panel` by name, and it is made in both directions: the
/// band must not be the surface, and every row of it must be the same non-surface colour. That is
/// checkable. Whether the difference is *perceptible* is not, and this test does not claim it —
/// the choice of `element` rests on the status strip having been the one row of this composite
/// users could always see, which is evidence from a terminal rather than from an assertion.
#[test]
fn the_prompt_band_is_painted_to_its_full_height() {
    for (width, height) in [(120u16, 32u16), (80, 24)] {
        let (mut blank, _shutdown) = screen();
        let buffer = render_offscreen(&mut blank, width, height).expect("infallible");
        let rendered = rows(&buffer);
        let first = prompt_first(&rendered, &blank, width, height);
        let band = usize::from(blank.prompt_and_tail(width, height).0);
        assert!(band >= 2, "at {width}x{height} there is no band to measure");

        let palette = blank.context.palette();
        let surface = ratatui::style::Color::from(palette.background_panel);
        let element = ratatui::style::Color::from(palette.background_element);
        assert_ne!(
            surface, element,
            "the theme gives the composer no surface to be distinct in"
        );

        // At the composer's own second column, not the frame's: the box is narrower than the
        // frame while the transcript is empty, so column one is the body surface's margin.
        let probe = u16::try_from(composer_span(&blank, width, height).0 + 1).expect("in frame");
        let row_of = |y: usize| buffer[(probe, u16::try_from(y).expect("in frame"))].bg;
        for offset in 0..band {
            assert_eq!(
                row_of(first + offset),
                element,
                "at {width}x{height} row {offset} of the band is not in the composer's own \
                 surface, so the box reads as fewer than {band} rows"
            );
        }
        // And the rows on either side are the surface, which is what makes the band an edge
        // rather than the start of an unbounded region.
        assert_eq!(
            row_of(rendered.len() - 1),
            surface,
            "at {width}x{height} the centring band is unpainted, so the composite's bottom edge \
             cannot be told from the terminal's own background"
        );
        assert_eq!(
            row_of(first.saturating_sub(1)),
            surface,
            "at {width}x{height} the row above the prompt band is not the body surface"
        );
        // The body surface resumes immediately below the band. Asserted so the
        // lower edge is located from both sides.
        assert_eq!(
            row_of(first + band),
            surface,
            "at {width}x{height} the box's bottom edge cannot be told from what is below it"
        );
    }
}

/// The instant a message lands the welcome composition yields completely.
///
/// The transition, not the two end states — those are asserted above and by
/// `the_welcome_screen_lifts_the_prompt_and_a_used_session_does_not`. What can go wrong only
/// *between* them is a centring band that outlives the screen it centred: the transcript would
/// then be pushed around by rows belonging to a welcome block nobody can see. So this renders
/// the empty frame, pushes one message, renders again, and requires the tail to be gone and the
/// body to have grown by exactly the rows it gave up.
#[test]
fn the_first_message_takes_the_whole_welcome_band_with_it() {
    for (width, height) in [(120u16, 32u16), (80, 24), (200, 50)] {
        let (mut screen, _shutdown) = screen();
        let before = rows(&render_offscreen(&mut screen, width, height).expect("infallible"));
        let (band_before, tail_before) = screen.prompt_and_tail(width, height);
        assert!(
            tail_before > 0,
            "at {width}x{height} the empty screen has no band to yield"
        );
        let below = info_rows(height);
        let body_before = before.len() - usize::from(tail_before + band_before + below);

        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::user("the first thing anyone types"));
        let after = rows(&render_offscreen(&mut screen, width, height).expect("infallible"));
        let (band_after, tail_after) = screen.prompt_and_tail(width, height);
        assert_eq!(
            tail_after, 0,
            "at {width}x{height} a used session still pays for the welcome band"
        );
        let body_after = after.len() - usize::from(band_after + below);
        assert_eq!(
            body_after,
            body_before + usize::from(tail_before),
            "at {width}x{height} the transcript did not inherit every row the band held"
        );
        // And the prompt is back on the frame's last row, which is what "yields completely"
        // means to a reader: the input stops moving between turns.
        assert_eq!(
            prompt_first(&after, &screen, width, height) + usize::from(band_after + below),
            after.len(),
            "at {width}x{height} the prompt is still lifted after the first message"
        );
        // The rows are gone *and* so is what was drawn in them. The band the empty screen used
        // to centre the input now carries the far half of the welcome surface, so "the tail is
        // zero" no longer implies "nothing of the welcome screen is left" — a `render` that
        // dropped the `empty` guard on `render_foot` would paint hint rows over the transcript
        // while every row count above still balanced.
        let joined = after.join("\n");
        for absent in ["type / for commands", "all keys", "past sessions"] {
            assert!(
                !joined.contains(absent),
                "at {width}x{height} `{absent}` survived the first message, so the welcome \
                 surface is being drawn under a transcript:\n{joined}"
            );
        }
        assert!(
            before.join("\n").contains("type / for commands"),
            "at {width}x{height} the empty screen never stated the lead line, so the check \
             above passed without anything having to be retracted"
        );
    }
}

/// The input band grows as lines are added, and stays centred while it grows.
///
/// # Two paths, because they are two different code paths
///
/// `session_prompt_grows_with_the_typed_line_count` drives `editor.insert_char('\n')`
/// directly, which is neither of the paths a user has. A newline arrives either as a chord the
/// keymap resolves to `input_newline` — `shift+return`, `ctrl+return`, `alt+return` or
/// `ctrl+j`, all four in the shipped table — or as a bracketed paste, which reaches
/// `SessionScreen::paste` and never touches the keymap at all. Both are exercised here through
/// the surfaces that own them: the chord through a real `KeyDispatcher` over the real shipped
/// keymap, the paste through `handle_event`.
///
/// # Centring is asserted at every step, not only at the end
///
/// The tail is `(height - band) / 2`, so band and tail move in opposite directions by the same
/// amount and the band stays centred by construction. That is precisely the kind of claim that
/// is easy to believe and easy to break — a tail computed from the *empty* band, or memoised
/// across frames, would pass a check made only before typing and only after. So the frame is
/// re-measured after each line, and the band's two gaps are re-compared each time.
///
/// The heights are the ones where growth is actually affordable. The cap is a third of the
/// pane, so a 32-row frame grants ten rows and a 24-row frame eight; both are above the
/// four-row floor, which is what makes "it grew" observable at all.
#[test]
fn the_input_band_grows_on_both_input_paths_and_stays_centred() {
    /// The band's painted extent and the blank rows on either side of it.
    ///
    /// By paint, for the reason `the_prompt_band_is_centred_on_the_frame` records at length:
    /// the band's own arithmetic cannot be used to check where the band ended up.
    ///
    /// Over `dyn Component` rather than over `SessionScreen`, because one of the two paths
    /// drives the screen through a `KeyDispatcher` that owns it — asking the screen directly
    /// would render a different object from the one the chord reached.
    /// `probe` is the composer's own first column, because the box is narrower than the frame
    /// on the welcome screen and column zero is the body surface's margin.
    fn measured(
        root: &mut dyn Component,
        element: ratatui::style::Color,
        probe: u16,
        width: u16,
        height: u16,
    ) -> (usize, usize, usize) {
        let buffer = render_offscreen(root, width, height).expect("infallible");
        let bg = |y: usize| buffer[(probe, u16::try_from(y).expect("in frame"))].bg;
        let last = (0..usize::from(height))
            .rposition(|y| bg(y) == element)
            .expect("the composer is painted in its own surface");
        let first = (0..=last)
            .rev()
            .take_while(|y| bg(*y) == element)
            .last()
            .expect("the run contains the row it ends on");
        assert!(first <= last, "the composer run is empty");
        (last + 1 - first, first, usize::from(height) - 1 - last)
    }

    let newline = crate::views::testkit::action("input_newline");
    assert!(
        newline.keys.split(',').any(|key| key == "ctrl+j"),
        "the shipped table no longer binds `ctrl+j` to `input_newline`, so the chord this \
         drives is not a newline: {}",
        newline.keys
    );

    let element = ratatui::style::Color::from(ViewContext::defaults().palette().background_element);
    for (width, height) in [(120u16, 32u16), (80, 24)] {
        // Every fixture here has an empty transcript, so the panel is not drawn and the composer
        // is centred on the whole frame. Taken from the production narrowing rather than
        // re-derived.
        let probe = composer_region(content_bounds(Rect::new(0, 0, width, height), false), true).x;
        // The chord path. `shift+return` cannot be delivered through a real terminal — the
        // legacy encoding gives it the same bytes as `return` — so `ctrl+j` is the spelling
        // driven here, and it is the same binding.
        let keymap = Keymap::defaults().expect("the shipped table builds");
        let (typed, _shutdown) = screen();
        let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(typed));
        let mut seen = Vec::new();
        for line in 1..=4 {
            assert!(
                dispatcher
                    .handle_event(&AppEvent::Terminal(TerminalEvent::Input(
                        crossterm::event::Event::Key(KeyEvent {
                            code: crossterm::event::KeyCode::Char('j'),
                            modifiers: crossterm::event::KeyModifiers::CONTROL,
                            kind: crossterm::event::KeyEventKind::Press,
                            state: crossterm::event::KeyEventState::NONE,
                        })
                    )))
                    .handled,
                "the dispatcher did not resolve `ctrl+j` to a newline on line {line}"
            );
            let (band, above, below) = measured(&mut dispatcher, element, probe, width, height);
            assert!(
                above.abs_diff(below) <= 1,
                "at {width}x{height} a {band}-row band after {line} newline(s) sits {above} \
                 rows from the top and {below} from the bottom"
            );
            seen.push(band);
        }
        assert!(
            seen.last() > seen.first(),
            "at {width}x{height} four newlines through the keymap left the band at {seen:?}; \
             the floor is four rows, so a fifth line has to buy a fifth row"
        );

        // The paste path, which reaches the editor without passing the keymap at all. One
        // event carrying every line, which is what a terminal in bracketed-paste mode sends.
        let (mut pasted, _shutdown) = screen();
        let (band_before, _, _) = measured(&mut pasted, element, probe, width, height);
        pasted.handle_event(&paste("one\ntwo\nthree\nfour\nfive\nsix"));
        let (band_after, above, below) = measured(&mut pasted, element, probe, width, height);
        assert!(
            band_after > band_before,
            "at {width}x{height} a six-line paste left the band at {band_after} rows, the \
             same as the empty {band_before}"
        );
        assert!(
            above.abs_diff(below) <= 1,
            "at {width}x{height} the band grew to {band_after} rows on a paste and went off \
             centre: {above} above, {below} below"
        );
    }
}

/// The whole welcome surface survives a startup diagnostic, and the diagnostic is readable.
///
/// # The regression, exactly as it was reported
///
/// A startup notice — a theme that fell back, a prompt history that could not be read — is
/// pushed into the transcript before the first frame. The welcome surface was drawn under
/// `messages().is_empty()`, so one such line reported "the conversation has begun" and took
/// the wordmark, the hint grid, the hidden sidebar and the composer's centring with it. What
/// the owner saw was a screen of orange warnings and nothing else. Every one of those four is
/// therefore asserted here, in one test, because they failed together and a test for any one
/// of them alone would let the other three come back.
///
/// # And the diagnostic still has to be visible
///
/// Split across two tests deliberately. The cheapest way to satisfy "the welcome screen is
/// intact" is to stop drawing the notice at all, which trades the reported defect for a worse
/// one: a user whose theme failed would be told nothing. `a_startup_notice_is_readable_beside_
/// the_welcome_screen` is what refuses that trade.
#[test]
fn the_welcome_surface_survives_a_startup_notice() {
    for (width, height) in [(80u16, 24u16), (120, 32), (120, 50), (130, 50)] {
        let (mut plain, _plain_shutdown) = screen();
        plain.sidebar_mut().ambient_mut().lsp = vec![crate::views::ambient::Service::new(
            "rust-analyzer",
            crate::views::ambient::Health::Ready,
        )];
        let (mut warned, _warned_shutdown) = screen();
        warned.sidebar_mut().ambient_mut().lsp = vec![crate::views::ambient::Service::new(
            "rust-analyzer",
            crate::views::ambient::Health::Ready,
        )];
        // Exactly what `tui.rs` pushes for a theme that could not be resolved.
        warned
            .transcript_mut()
            .transcript_mut()
            .push(Message::notice(
                "warning: theme `nord` was not found; falling back to the built-in palette",
            ));

        let clean = rows(&render_offscreen(&mut plain, width, height).expect("infallible"));
        let rendered = rows(&render_offscreen(&mut warned, width, height).expect("infallible"));

        // 1. The brand. Compared against the unwarned frame rather than against a literal, so
        //    this keeps measuring the wordmark at widths where it degrades to the compact form.
        let brand = |frame: &[String]| {
            frame
                .iter()
                .any(|row| row.contains(crate::views::welcome::WORDMARK[0].trim()))
                || frame.iter().any(|row| row.contains("ZUNO"))
        };
        assert!(
            brand(&clean),
            "at {width}x{height} the fixture draws no brand even with no notice, so this \
             proves nothing:\n{}",
            clean.join("\n")
        );
        assert!(
            brand(&rendered),
            "at {width}x{height} a startup notice took the wordmark with it:\n{}",
            rendered.join("\n")
        );

        // 2. The hint grid, which is the welcome surface's foot below the composer.
        assert!(
            rendered.iter().any(|row| row.contains("/model")),
            "at {width}x{height} a startup notice took the hint grid with it:\n{}",
            rendered.join("\n")
        );

        // 3. The sidebar stays away. Located by a service only the panel names — `Context` and
        //    `MCP` also occur in the welcome census, the needle collision
        //    `the_ambient_panel_waits_for_a_transcript` records.
        assert!(
            warned.sidebar_visible(),
            "the fixture has the panel toggled off, so its absence would prove nothing"
        );
        assert!(
            !rendered.iter().any(|row| row.contains("rust-analyzer")),
            "at {width}x{height} a startup notice brought the ambient panel onto the welcome \
             screen, where every figure it carries is zero or unresolved:\n{}",
            rendered.join("\n")
        );

        // 4. The composer stays a centred box. Read from the production region, and required to
        //    be closed on both sides — a narrowed band with no rules is the "reads as a band
        //    rather than a box" defect, and a full-width one is the original complaint.
        let (x, columns) = composer_span(&warned, width, height);
        assert_eq!(
            (x, columns),
            composer_span(&plain, width, height),
            "at {width}x{height} a startup notice changed the composer's region"
        );
        assert_eq!(
            (x, columns),
            expected_welcome_composer_span(width),
            "at {width}x{height} the composer did not retain its welcome-page width policy"
        );
        let first = content_row(&rendered, &warned, width, height);
        let full = &rendered[prompt_first(&rendered, &warned, width, height)];
        let edges = full.chars().collect::<Vec<_>>();
        assert!(
            first.contains(PROMPT_MARKER),
            "at {width}x{height} the composer lost its gutter marker: {first:?}"
        );
        if x > 0 {
            assert_eq!(
                (edges.get(x - 1).copied(), edges.get(x + columns).copied()),
                (Some('▌'), Some('▐')),
                "at {width}x{height} the composer is not closed on both sides: {full:?}"
            );
        }
    }
}

/// A startup diagnostic is still on screen while the welcome surface holds the frame.
///
/// The other half of `the_welcome_surface_survives_a_startup_notice`: the layout must not be
/// repaired by suppressing the warning. Its text is required verbatim, and required to sit
/// above the composer rather than merely somewhere on the frame — a notice drawn under the
/// hint grid would read as a hint.
#[test]
fn a_startup_notice_is_readable_beside_the_welcome_screen() {
    const WARNING: &str = "warning: theme `nord` was not found; falling back to the built-in";

    for (width, height) in [(80u16, 24u16), (120, 32), (120, 50)] {
        let (mut screen, _shutdown) = screen();
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::notice(WARNING));
        let rendered = rows(&render_offscreen(&mut screen, width, height).expect("infallible"));

        let notice = rendered
            .iter()
            .position(|row| row.contains("was not found"))
            .unwrap_or_else(|| {
                panic!(
                    "at {width}x{height} the startup notice is nowhere on the frame, so the \
                     welcome layout was repaired by hiding the warning:\n{}",
                    rendered.join("\n")
                )
            });
        // Verbatim, not just the needle: a notice cut mid-sentence with no elision mark is
        // indistinguishable from one that fitted, which is the failure `a_long_notice_stops_at_
        // the_cap_and_says_how_much_it_kept_back` exists for.
        assert!(
            rendered[notice].contains(WARNING),
            "at {width}x{height} the notice was clipped: {:?}",
            rendered[notice]
        );
        assert!(
            notice < prompt_first(&rendered, &screen, width, height),
            "at {width}x{height} the notice is drawn at row {notice}, at or below the composer, \
             so it reads as part of the hint grid rather than as a report:\n{}",
            rendered.join("\n")
        );
        // Above the brand, so it used the rows the bottom-anchored head leaves blank rather
        // than displacing the head.
        let brand = brand_row(&rendered);
        assert!(
            notice < brand,
            "at {width}x{height} the notice is below the brand at row {brand}, so it displaced \
             the head instead of using the rows above it:\n{}",
            rendered.join("\n")
        );
    }
}

/// The notice block is bottom-anchored in the blank run, not pinned to row zero.
///
/// # Why this is measured across two heights rather than as a gap of `n` rows
///
/// The transcript draws its own role header above a notice and its own bottom margin below it,
/// so the distance from the notice's text to the brand is a transcript-internal number. An
/// assertion spelling it out would pass for a top-anchored block on a short frame and would
/// have to be re-tuned every time the transcript's own chrome changed.
///
/// Bottom-anchoring has a height-independent signature instead: growing the frame lengthens the
/// blank run *above* the block and leaves the rows below it alone. Top-anchoring is the exact
/// mirror — the gap below grows and the gap above stays at zero — so comparing 32 rows with 50
/// at one width separates them with no constant to maintain. One width, because a different
/// width would re-wrap the notice and change the block's own height.
#[test]
fn a_startup_notice_sits_against_the_brand_rather_than_the_frame_top() {
    let measure = |height: u16| {
        let (mut screen, _shutdown) = screen();
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::notice(
                "warning: theme `nord` was not found; falling back to the built-in palette",
            ));
        let rendered = rows(&render_offscreen(&mut screen, 120, height).expect("infallible"));
        let notice = rendered
            .iter()
            .position(|row| row.contains("was not found"))
            .expect("the notice is drawn");
        (notice, brand_row(&rendered) - notice)
    };

    let (short_above, short_below) = measure(32);
    let (tall_above, tall_below) = measure(50);

    assert_eq!(
        short_below, tall_below,
        "the rows between the notice and the brand grew with the frame ({short_below} at 32 \
         rows, {tall_below} at 50), which is what a top-anchored block does"
    );
    assert!(
        tall_above > short_above,
        "the notice stayed {tall_above} rows from the top of both frames, so it is pinned to \
         the top rather than riding the brand down"
    );
}

/// The row the welcome brand is on, by either of the two forms it takes.
///
/// The wordmark degrades to a compact word below `WORDMARK_MIN_HEIGHT`, so a needle for the
/// block glyphs alone would silently find nothing on a short frame and every assertion built on
/// it would be about row zero.
fn brand_row(rendered: &[String]) -> usize {
    rendered
        .iter()
        .position(|row| {
            row.contains(crate::views::welcome::WORDMARK[0].trim()) || row.contains("ZUNO")
        })
        .expect("the welcome brand is drawn")
}

// ---------------------------------------------------------------------------
// The conversation screen: the four defects reported from a live 120x32 pane
// ---------------------------------------------------------------------------

/// A screen mid-conversation, with the ambient facts a real host resolves.
///
/// Both roles, because three of the four assertions below are about telling them apart, and a
/// fixture holding only a prompt would let a renderer that framed *every* message pass. The
/// directory and the context figure are set because the info row states them, and a fixture
/// that left them unresolved would assert about an empty row.
///
/// The two widths every test here runs at are 120 and 80: [`crate::views::SIDEBAR_MIN_WIDTH`]
/// is 120, so the first is the only one where the panel is drawn and the second is the widest
/// common pane where it is not. A defect about the composer's columns can only be seen at the
/// first, and a regression that fixed it by narrowing unconditionally can only be seen at the
/// second.
fn conversing() -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    conversing_with(ViewContext::defaults())
}

fn mouse_conversing() -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    let (screen, receiver) = mouse_screen();
    conversing_from(screen, receiver)
}

fn conversing_with(context: ViewContext) -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    let (screen, receiver) = screen_with(context);
    conversing_from(screen, receiver)
}

fn conversing_from(
    mut screen: SessionScreen,
    shutdown: mpsc::Receiver<TerminalEvent>,
) -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    screen
        .status_mut()
        .describe("build", "myopenai/gpt-5.6-sol");
    screen.sidebar_mut().ambient_mut().directory = Some(String::from("~/work/zuno"));
    // Through the transcript, not by setting `Ambient::context_used` directly: `render`
    // re-derives that field from the transcript on every frame so the panel, the strip and the
    // info row cannot disagree, and a fixture that wrote the field would be overwritten before
    // the first assertion — while appearing to work at whatever width the panel is not drawn.
    screen
        .transcript_mut()
        .transcript_mut()
        .set_context_limit(100_000);
    screen
        .transcript_mut()
        .transcript_mut()
        .observe(&zuno_engine::r#loop::TurnEvent::Provider {
            step: 1,
            event: zuno_llm::event::StreamEvent::TokenUsage {
                input_tokens: Some(37_000),
                output_tokens: Some(0),
                cache_read_input_tokens: None,
                cache_write_input_tokens: None,
                accounting: zuno_llm::event::PromptAccounting::CacheInsideInput,
            },
        });
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("summarise the plan"));
    let mut assistant = Message::new(crate::views::message::Role::Assistant);
    assistant
        .parts
        .push(crate::views::message::MessagePart::Text {
            text: String::from("Here is the summary of the plan."),
        });
    screen.transcript_mut().transcript_mut().push(assistant);
    (screen, shutdown)
}

/// OpenCode's identity row belongs to the reply, not to the composer.
///
/// With a short transcript it sits immediately after the assistant content and leaves the
/// unused viewport below it. Once the transcript consumes the available viewport it becomes a
/// sticky row directly above the composer instead of scrolling away with older content.
#[test]
fn the_agent_identity_follows_a_short_reply_then_sticks_above_the_composer() {
    let identity_catalog = || SessionCatalog {
        models: vec![crate::views::picker::ModelEntry {
            id: String::from("myopenai/claude-opus-5"),
            name: String::from("Claude Opus 5"),
            provider: String::from("myopenai"),
            reasoning: true,
        }],
        model: Some(String::from("myopenai/claude-opus-5")),
        agent: Some(String::from("Atlas - Plan Executor")),
        reasoning: true,
        effort: Some(zuno_llm::effort::ReasoningEffort::Max),
        ..SessionCatalog::default()
    };

    let (short, _shutdown) = conversing();
    let mut short = short.with_catalog(identity_catalog());
    short
        .status_mut()
        .describe("Atlas - Plan Executor", "myopenai/claude-opus-5");
    short
        .status_mut()
        .set_effort(Some(zuno_llm::effort::ReasoningEffort::Max));
    let short_rows = rows(&render_offscreen(&mut short, 100, 24).expect("infallible"));
    let reply = short_rows
        .iter()
        .position(|row| row.contains("Here is the summary of the plan."))
        .expect("assistant reply");
    let identity = short_rows
        .iter()
        .position(|row| row.contains("▣ Atlas - Plan Executor · Claude Opus 5 (max)"))
        .unwrap_or_else(|| panic!("the reply identity is absent:\n{}", short_rows.join("\n")));
    let prompt = short_rows
        .iter()
        .position(|row| row.contains(PROMPT_PLACEHOLDER))
        .expect("composer");
    assert!(
        reply < identity && identity < prompt,
        "the identity row does not follow the reply above the composer:\n{}",
        short_rows.join("\n")
    );
    assert!(
        short_rows[identity + 1..prompt]
            .iter()
            .all(|row| row.trim().is_empty()),
        "a short reply should leave its unused viewport below the identity row:\n{}",
        short_rows.join("\n")
    );

    let (mut long, _shutdown) = screen();
    long = long.with_catalog(identity_catalog());
    long.status_mut()
        .describe("Atlas - Plan Executor", "myopenai/claude-opus-5");
    long.status_mut()
        .set_effort(Some(zuno_llm::effort::ReasoningEffort::Max));
    long.transcript_mut()
        .transcript_mut()
        .push(Message::user("fill the viewport"));
    let mut answer = Message::new(Role::Assistant);
    answer.parts.push(crate::views::message::MessagePart::Text {
        text: (1..=40)
            .map(|line| format!("reply line {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    long.transcript_mut().transcript_mut().push(answer);
    let long_rows = rows(&render_offscreen(&mut long, 100, 18).expect("infallible"));
    let identity = long_rows
        .iter()
        .position(|row| row.contains("▣ Atlas - Plan Executor · Claude Opus 5 (max)"))
        .unwrap_or_else(|| panic!("the sticky identity is absent:\n{}", long_rows.join("\n")));
    let prompt = long_rows
        .iter()
        .position(|row| row.contains(PROMPT_PLACEHOLDER))
        .expect("composer");
    assert_eq!(
        identity + 1,
        prompt,
        "once content fills the viewport the identity must stick directly above the composer:\n{}",
        long_rows.join("\n")
    );
}

/// A live turn uses the final row as one compact control surface.
///
/// The pulse is the first visible item, followed by the configured interrupt key. Context
/// occupancy and command discovery remain on the same row, and the animation changes without
/// moving any text.
#[test]
fn the_running_footer_pulses_before_escape_and_keeps_context_and_commands_visible() {
    let (mut screen, _shutdown) = conversing();
    screen.mark_turn_accepted();

    let first = rows(&render_offscreen(&mut screen, 100, 24).expect("infallible"));
    let footer = first.last().expect("footer");
    let escape = footer
        .find("esc interrupt")
        .unwrap_or_else(|| panic!("the interrupt affordance is absent: {footer:?}"));
    let pulse = footer
        .find('▰')
        .unwrap_or_else(|| panic!("the pulse bar is absent: {footer:?}"));
    assert!(
        pulse < escape,
        "the pulse must lead the escape hint: {footer:?}"
    );
    assert!(
        footer.contains("37.0K (37%)"),
        "the live footer does not show context occupancy: {footer:?}"
    );
    assert!(
        footer.contains("commands"),
        "the live footer dropped command discovery: {footer:?}"
    );
    assert!(
        !footer.contains("working"),
        "the pulse already communicates liveness; a second state word is noise: {footer:?}"
    );

    screen.handle_event(&AppEvent::AnimationFrame);
    let second = rows(&render_offscreen(&mut screen, 100, 24).expect("infallible"));
    let second_footer = second.last().expect("footer");
    assert_ne!(
        footer, second_footer,
        "an animation frame did not advance the pulse"
    );
    assert!(second_footer.contains("esc interrupt"), "{second_footer:?}");
    assert!(second_footer.contains("37.0K (37%)"), "{second_footer:?}");
}

/// Defect 1: a fresh conversation does not open with a `Session` header block.
///
/// The reported frame spent its first two rows on a `⚠ Session` heading over a one-line
/// `model set to …` confirmation. Both halves are asserted, because either alone is satisfiable
/// by the defect: the heading must be gone from the transcript, *and* a model switch must not
/// put a row there at all — it is a toast now, so the row it used to own does not exist.
///
/// The notice used here is a `Role::System` message, which is what a startup warning is, so the
/// session can still say things about itself; what it may not do is wear a header while doing
/// so. That distinction is the whole test: a renderer that dropped session notices entirely
/// would pass the first assertion and fail the second-to-last.
#[test]
fn the_conversation_does_not_open_with_a_session_header() {
    for width in [120u16, 80] {
        let (mut screen, _shutdown) = conversing();
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::notice("warning: theme `nord` was not found"));
        let rendered = rows(&render_offscreen(&mut screen, width, 32).expect("infallible"));
        let joined = rendered.join("\n");
        assert!(
            !joined.contains("Session"),
            "at {width} columns the transcript still wears a `Session` header:\n{joined}"
        );
        // The notice itself survives, attributed by its own rule. Without this the test would
        // be satisfied by a renderer that hid every session message, which would make a theme
        // fallback unreportable.
        assert!(
            rendered.iter().any(|row| row.contains("was not found")
                && row.trim_start().starts_with(Role::System.marker())),
            "at {width} columns the session's own notice is gone or unattributed:\n{joined}"
        );
        // And a model switch does not write a row at all: it is the reported example, and it
        // reaches the same renderer through `adopt`.
        let (mut switched, _shutdown) = conversing();
        switched.adopt(crate::views::picker::MODEL_DIALOG_ID, "prov/model");
        let after =
            rows(&render_offscreen(&mut switched, width, 32).expect("infallible")).join("\n");
        assert!(
            !after.contains("model set to"),
            "at {width} columns a model switch still opens the conversation with a notice:\n{after}"
        );
    }
}

/// The composer and reply identity stop at the body's edge, with an info row at the foot.
///
/// # Two claims, and the first is only visible at 120 columns
///
/// At 120 the panel occupies the last `SIDEBAR_WIDTH + SIDEBAR_GAP_COLS` columns, so the
/// composer and identity must end before them — the reported defect was both running to
/// column 119 under a panel whose rule stands at 81. At 80 the panel is not drawn, so the
/// composer may use the whole frame; asserting the narrowing at both widths would forbid that
/// and is why the two are checked differently rather than in one loop body.
///
/// # The info row is asserted by content *and* by surface
///
/// Content alone would pass a row that merely repeated the identity. So the row must carry the
/// directory and the command key, must be the frame's last row, and must be painted in the body
/// surface rather than the composer's `element`.
#[test]
fn the_composer_stays_inside_the_body_and_gains_an_info_row() {
    for width in [120u16, 80] {
        let (mut screen, _shutdown) = conversing();
        let buffer = render_offscreen(&mut screen, width, 32).expect("infallible");
        let rendered = rows(&buffer);
        let (x, columns) = composer_span(&screen, width, 32);
        let sidebar = width >= crate::views::SIDEBAR_MIN_WIDTH;

        if sidebar {
            let body = usize::from(width)
                - usize::from(crate::views::ambient::SIDEBAR_WIDTH)
                - usize::from(SIDEBAR_GAP_COLS);
            assert!(
                x + columns <= body,
                "at {width} columns the composer runs to column {} while the body ends at \
                 {body}, so the input box crosses into the sidebar's region",
                x + columns
            );
            // The gap between the left column and the full-height sidebar stays blank. The
            // sidebar itself legitimately has content on this row now.
            let identity = rendered
                .iter()
                .position(|row| row.contains('▣'))
                .expect("the reply identity");
            let gap: String = rendered[identity]
                .chars()
                .skip(body)
                .take(usize::from(SIDEBAR_GAP_COLS))
                .collect();
            assert!(
                gap.trim().is_empty(),
                "at {width} columns the reply identity reaches across the sidebar gap: \
                 {gap:?}"
            );
        } else {
            assert_eq!(
                (x, columns),
                (1, usize::from(width.saturating_sub(2))),
                "at {width} columns there is no panel, so the composer must fill the frame \
                 except for its one-cell margins"
            );
        }

        // The info row: last row of the frame, its own content, its own surface.
        let info = rendered.last().expect("the frame has rows");
        assert!(
            info.contains("~/work/zuno"),
            "at {width} columns the info row does not say where the session is: {info:?}"
        );
        assert!(
            info.contains("commands"),
            "at {width} columns the info row does not name the command key: {info:?}"
        );
        assert!(
            info.contains("ctx 37.0k/100.0k (37.0%)"),
            "at {width} columns the info row does not report the context spend: {info:?}"
        );
        let palette = screen.context.palette();
        let last = u16::try_from(rendered.len() - 1).expect("in frame");
        assert_eq!(
            buffer[(1, last)].bg,
            ratatui::style::Color::from(palette.background_panel),
            "at {width} columns the info row shares the composer's surface, so it reads as \
             another row of the box rather than as the screen's own footer"
        );
        let identity = rendered
            .iter()
            .position(|row| row.contains('▣'))
            .expect("the reply identity");
        assert!(
            identity < rendered.len() - 1,
            "at {width} columns the identity is on the frame's last row, so there is no info row \
             beneath it"
        );
    }
}

/// Defect 3: the user's prompt is wrapped in a bordered container and the reply is not.
///
/// # Both sides, because a renderer that framed everything is the same failure in reverse
///
/// The complaint was that the two were indistinguishable. So the prompt's row must open with
/// the user's rule *and* close with the box's right edge, and the reply's rows must do neither.
/// Checking only the prompt would pass a renderer that framed the assistant too, leaving the
/// two as alike as before.
///
/// The heading is asserted on the top rule rather than on a row of its own, which is where it
/// now rides — see `TranscriptView::push_boxed`.
#[test]
fn the_users_prompt_is_framed_and_the_reply_is_not() {
    for width in [120u16, 80] {
        let (mut screen, _shutdown) = conversing();
        let rendered = rows(&render_offscreen(&mut screen, width, 32).expect("infallible"));
        // Sliced to the body's own columns, because at 120 the panel is drawn *on the same
        // rows*: a whole frame row carries `▌ You ───▐ │   Context`, so an `ends_with` against
        // the frame would be asserting about the sidebar rather than about the box. This is the
        // same subtraction `content_bounds` performs, taken from the production predicate.
        let content = if sidebar_drawn(screen.sidebar_visible(), false, width) {
            usize::from(width)
                - usize::from(crate::views::ambient::SIDEBAR_WIDTH)
                - usize::from(SIDEBAR_GAP_COLS)
        } else {
            usize::from(width)
        };
        let body = content.saturating_sub(usize::from(screen.scrollbar_visible));
        let rendered: Vec<String> = rendered
            .iter()
            .map(|row| row.chars().take(body).collect())
            .collect();
        let joined = rendered.join("\n");

        let top = rendered
            .iter()
            .position(|row| row.contains("You"))
            .unwrap_or_else(|| panic!("at {width} columns the prompt has no heading:\n{joined}"));
        assert!(
            rendered[top].starts_with(Role::User.marker())
                && rendered[top].trim_end().ends_with(USER_BOX_RIGHT),
            "at {width} columns the prompt's top rule is not closed on both sides: {:?}",
            rendered[top]
        );
        let body = rendered
            .iter()
            .position(|row| row.contains("summarise the plan"))
            .unwrap_or_else(|| panic!("at {width} columns the prompt is missing:\n{joined}"));
        assert!(
            rendered[body].starts_with(Role::User.marker())
                && rendered[body].trim_end().ends_with(USER_BOX_RIGHT),
            "at {width} columns the prompt's own text is not inside the box: {:?}",
            rendered[body]
        );
        // The closing rule, which is what makes it a container rather than a header with a
        // right edge. Located as the first row after the body that carries no text of its own.
        assert!(
            rendered[body + 1..]
                .iter()
                .take(2)
                .any(|row| row.starts_with(Role::User.marker())
                    && row.contains(USER_BOX_RULE)
                    && row.trim_end().ends_with(USER_BOX_RIGHT)),
            "at {width} columns the prompt's box is never closed:\n{joined}"
        );

        // And the reply is bare prose: its own rule, no frame.
        let reply = rendered
            .iter()
            .position(|row| row.contains("Here is the summary"))
            .unwrap_or_else(|| panic!("at {width} columns the reply is missing:\n{joined}"));
        assert!(
            rendered[reply].starts_with(Role::Assistant.marker()),
            "at {width} columns the reply lost its own rule: {:?}",
            rendered[reply]
        );
        assert!(
            !rendered[reply].trim_end().ends_with(USER_BOX_RIGHT),
            "at {width} columns the reply is framed too, so the two sides are as alike as \
             before: {:?}",
            rendered[reply]
        );
    }
}

/// Defect 4: a press on the prompt opens a menu offering copy and revert.
///
/// # Driven end to end through the hosted mouse path
///
/// The prompt, menu row, and confirmation button all go through the event loop's own path.
/// A test that called `apply_dialog_outcome` directly would pass while the visible controls
/// remained impossible to click. Copy is asserted by reading the text back out of an injected
/// [`crate::views::external::MemoryClipboard`], not by finding a toast — a toast saying
/// `copied 18 characters` is exactly what a menu row that wrote nowhere would also produce.
///
/// # Revert is asserted to *confirm*, not to submit
///
/// It overwrites files on disk, so the row must open the same confirmation `/undo` opens. A
/// test that accepted a submitted `/undo` would pass a build in which one stray click destroys
/// uncommitted work.
#[test]
fn a_press_on_the_prompt_opens_a_menu_that_copies_and_reverts() {
    for width in [120u16, 80] {
        let clipboard = Arc::new(crate::views::external::MemoryClipboard::default());
        let (screen, _shutdown) = mouse_conversing();
        let context = screen.context.clone();
        let screen = screen.with_clipboard(clipboard.clone());
        let mut host = DialogHost::new(context, Box::new(screen));
        let rendered = rows(&render_offscreen(&mut host, width, 32).expect("infallible"));
        let (column, row) = control_at(&rendered, "summarise the plan");

        assert!(
            click_at(&mut host, column, row).redraw,
            "at {width} columns a press on the prompt was not consumed, so no menu can open"
        );
        assert_eq!(
            host.active(),
            Some(MESSAGE_ACTIONS_DIALOG_ID),
            "at {width} columns a press on the prompt did not open the message menu"
        );

        // Copy: click the rendered row, and the text has to reach a clipboard rather than
        // merely produce a success toast.
        let menu = rows(&render_offscreen(&mut host, width, 32).expect("infallible"));
        let (column, row) = control_at(&menu, "Copy message");
        assert!(
            click_at(&mut host, column, row).redraw,
            "at {width} columns the Copy message row did not accept a click"
        );
        assert!(!host.is_open(), "the copied message menu stayed open");
        // Read back through the trait, which is what the screen wrote through: `read` returns
        // whatever `write` last stored, so this is the round trip rather than a claim about it.
        let held = crate::views::external::Clipboard::read(clipboard.as_ref())
            .expect("a memory clipboard never fails to read");
        assert_eq!(
            held.as_ref().map(|content| content.data.as_str()),
            Some("summarise the plan"),
            "at {width} columns the copy row put nothing on the clipboard: {held:?}"
        );

        // Revert: click the menu row, prove it confirms without submitting, then click the
        // rendered Restore button and observe the actual host command.
        let (screen, _shutdown) = mouse_conversing();
        let context = screen.context.clone();
        let (prompts, mut submitted) = mpsc::channel(1);
        let screen = screen.with_prompt_sink(prompts);
        let mut reverting = DialogHost::new(context, Box::new(screen));
        let rendered = rows(&render_offscreen(&mut reverting, width, 32).expect("infallible"));
        let (column, row) = control_at(&rendered, "summarise the plan");
        click_at(&mut reverting, column, row);
        let menu = rows(&render_offscreen(&mut reverting, width, 32).expect("infallible"));
        let offered = menu.iter().any(|line| line.contains("Revert this turn"));
        assert!(
            offered,
            "at {width} columns the menu on the newest prompt offers no revert row"
        );
        let (column, row) = control_at(&menu, "Revert this turn");
        assert!(
            click_at(&mut reverting, column, row).redraw,
            "at {width} columns the Revert row did not accept a click"
        );
        assert_eq!(
            reverting.active(),
            Some(UNDO_CONFIRM_DIALOG_ID),
            "at {width} columns revert did not ask before overwriting the worktree"
        );
        assert!(
            submitted.try_recv().is_err(),
            "at {width} columns revert reached the driver without a confirmation"
        );

        let confirmation = rows(&render_offscreen(&mut reverting, width, 32).expect("infallible"));
        let (column, row) = control_at(&confirmation, "Restore");
        assert!(
            click_at(&mut reverting, column, row).redraw,
            "at {width} columns the Restore button did not accept a click"
        );
        assert_eq!(
            submitted.try_recv(),
            Ok(PromptSubmission::Host(HostCommand::Undo)),
            "at {width} columns clicking Restore did not reach the undo handler"
        );
        assert!(
            !reverting.is_open(),
            "at {width} columns the confirmation stayed open after Restore"
        );
    }
}

/// The session's name is drawn above the panel's `Context` block on a real frame.
///
/// The panel-level test in `ambient_tests` proves the row order the panel composes; this
/// proves the whole wiring it depends on — a projection published from outside the render
/// loop, an observer that reports the change, and a frame at a width where the sidebar is
/// actually drawn.
///
/// Both halves are load-bearing and fail separately. Drop `observe_session_title` from the
/// merge chain and the `redraw` assertion fails while the frame stays correct — which is
/// the real defect, a named session that does not repaint until something unrelated
/// happens to. Stop reading the projection in `render` and the ordering assertion fails
/// instead. Nothing here sets `Ambient::title` by hand, so the name can only arrive by the
/// seam production uses.
#[test]
fn session_screen_states_the_session_name_above_the_sidebars_context_block() {
    for width in [crate::views::SIDEBAR_MIN_WIDTH, 160] {
        assert!(
            width >= crate::views::SIDEBAR_MIN_WIDTH,
            "{width} is below the threshold, so the sidebar would not be drawn at all"
        );
        let projection = crate::views::ambient::SessionTitle::default();
        let (sender, _shutdown) = terminal_event_channel();
        let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
            .with_session_title(projection.clone());
        // The panel is withheld until there is a transcript beside it, so a name asserted on
        // the welcome screen would prove nothing about where the panel puts it.
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::user("refactor the user service"));

        // Published from outside the render loop, exactly as the turn driver publishes it.
        projection.replace(Some(String::from("Refactoring user service")));
        assert!(
            screen
                .handle_event(&AppEvent::Terminal(TerminalEvent::Wake))
                .redraw,
            "a newly named session must ask for a frame, or the name sits unseen until \
             something else happens to repaint"
        );

        let frame = rows(&render_offscreen(&mut screen, width, 30).expect("infallible"));
        let row_of = |needle: &str| {
            frame
                .iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| {
                    panic!(
                        "`{needle}` is not on the {width}-column frame:\n{}",
                        frame.join("\n")
                    )
                })
        };
        // `no usage reported yet` is the panel's own copy, so finding it proves the `Context`
        // located below is the sidebar's heading and not some other row carrying the word.
        assert!(
            frame
                .iter()
                .any(|row| row.contains("no usage reported yet")),
            "the ambient panel was not drawn at {width} columns, so this proves nothing \
             about where it puts the name:\n{}",
            frame.join("\n")
        );
        assert!(
            row_of("Refactoring user service") < row_of("Context"),
            "at {width} columns the session name is not above the Context block:\n{}",
            frame.join("\n")
        );
    }
}

// ---------------------------------------------------------------------------
// Reasoning level (ctrl+t) and the delegated-task view (ctrl+x down).

/// A catalog whose model reasons, so the cycling key has something to cycle.
fn reasoning_catalog() -> SessionCatalog {
    let mut catalog = catalog();
    catalog.reasoning = true;
    for model in &mut catalog.models {
        model.reasoning = true;
        catalog.reasoning_efforts.insert(
            model.id.clone(),
            zuno_llm::effort::ReasoningEffort::ALL.to_vec(),
        );
    }
    catalog
}

/// `ctrl+t` steps the reasoning level and states it on the strip.
///
/// The level, the commit to the host and the rendered row are asserted together on
/// purpose: any one of them alone is satisfied by a change that does not reach the other
/// two, which is exactly the "changed a label" failure this feature must not be.
#[test]
fn variant_cycle_steps_the_reasoning_level_and_shows_it_on_the_model_row() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(4);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_catalog(reasoning_catalog())
        .with_selection_sink(selections);
    screen.status_mut().describe("build", "prov/haiku");

    let result = screen.handle_action(action("variant_cycle"), &press_none());
    assert!(result.handled && result.redraw, "ctrl+t did nothing");

    assert_eq!(
        screen.catalog.effort,
        Some(zuno_llm::effort::ReasoningEffort::Off),
        "the first press must land on the weakest level rather than mid-scale"
    );
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Effort(zuno_llm::effort::ReasoningEffort::Off)),
        "the level has to reach the host, or nothing re-resolves the request"
    );

    screen.handle_action(action("variant_cycle"), &press_none());
    assert_eq!(
        screen.catalog.effort,
        Some(zuno_llm::effort::ReasoningEffort::Low)
    );
    assert_eq!(
        screen.welcome_mut().facts().reasoning.as_deref(),
        Some("low"),
        "the welcome identity did not follow the selected reasoning effort"
    );
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Effort(zuno_llm::effort::ReasoningEffort::Low))
    );

    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("show the selected effort"));
    let joined = rows(&render_offscreen(&mut screen, 80, 24).expect("infallible")).join("\n");
    assert!(
        joined.contains("▣ build · haiku"),
        "the model row is not on screen, so this proves nothing:\n{joined}"
    );
    assert!(
        joined.contains("(low)"),
        "the chosen level is not shown on the model row:\n{joined}"
    );
}

/// Explicit model variants define the levels the interactive selector may offer.
#[test]
fn variant_cycle_uses_only_the_current_models_declared_levels() {
    use zuno_llm::effort::ReasoningEffort;

    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(4);
    let mut catalog = reasoning_catalog();
    catalog.reasoning_efforts.insert(
        String::from("prov/haiku"),
        vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ],
    );
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_catalog(catalog)
        .with_selection_sink(selections);

    screen.handle_action(action("variant_cycle"), &press_none());
    assert_eq!(screen.catalog.effort, Some(ReasoningEffort::Low));
    assert_eq!(
        chosen.try_recv(),
        Ok(Selection::Effort(ReasoningEffort::Low))
    );

    for expected in [
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Xhigh,
        ReasoningEffort::Max,
        ReasoningEffort::Low,
    ] {
        screen.handle_action(action("variant_cycle"), &press_none());
        assert_eq!(screen.catalog.effort, Some(expected));
        assert_eq!(chosen.try_recv(), Ok(Selection::Effort(expected)));
    }
}

/// On a model that does not reason, `ctrl+t` must change nothing visible.
///
/// This is 所有功能都要完整可用 inverted: a key that looks live and is not is worse than
/// one that is absent. It must not invent a level, must not show one on the strip, and
/// must not send one to the host — while still saying why, so the refusal is legible.
#[test]
fn variant_cycle_does_nothing_visible_on_a_model_that_cannot_reason() {
    let (sender, _shutdown) = terminal_event_channel();
    let (selections, mut chosen) = mpsc::channel(4);
    // `catalog()` declares `reasoning: false` for every model, as the host does for a
    // model whose resolved catalog entry has no reasoning capability.
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender)
        .with_catalog(catalog())
        .with_selection_sink(selections);
    screen.status_mut().describe("build", "prov/haiku");
    let before = rows(&render_offscreen(&mut screen, 80, 24).expect("infallible")).join("\n");

    screen.handle_action(action("variant_cycle"), &press_none());

    assert_eq!(
        screen.catalog.effort, None,
        "a level was adopted for a model that cannot use one"
    );
    assert_eq!(
        screen.status.effort(),
        None,
        "the strip claims a reasoning level the request would not carry"
    );
    assert_eq!(
        chosen.try_recv(),
        Err(mpsc::error::TryRecvError::Empty),
        "an inapplicable level was sent to the host"
    );
    let after = rows(&render_offscreen(&mut screen, 80, 24).expect("infallible")).join("\n");
    assert!(
        !after.contains("▣ build · haiku ("),
        "the reply identity grew a reasoning segment on a model that cannot reason:\n{after}"
    );
    assert!(
        before.lines().next() == after.lines().next(),
        "the frame changed above the toast row"
    );

    let toasts = screen.drain_toasts();
    assert_eq!(toasts.len(), 1, "the refusal was silent");
    assert!(
        toasts[0]
            .text()
            .contains("does not support selectable reasoning effort"),
        "the refusal does not say why: {:?}",
        toasts[0].text()
    );
    assert!(
        toasts[0]
            .text()
            .contains("Choose a reasoning-capable model"),
        "the refusal gives no next step: {:?}",
        toasts[0].text()
    );
    assert_eq!(
        toasts[0].ttl(),
        crate::views::toast::TOAST_ATTENTION_TTL,
        "the capability explanation inherited the short confirmation timeout"
    );
}

/// Switching to a model that cannot reason drops the level rather than keeping it shown.
#[test]
fn choosing_a_model_without_reasoning_clears_the_level() {
    let (sender, _shutdown) = terminal_event_channel();
    let mut catalog = reasoning_catalog();
    catalog.models[1].reasoning = false;
    let plain = catalog.models[1].id.clone();
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_catalog(catalog);
    screen.handle_action(action("variant_cycle"), &press_none());
    assert!(screen.catalog.effort.is_some());

    screen.adopt(crate::views::picker::MODEL_DIALOG_ID, &plain);

    assert!(
        !screen.catalog.reasoning,
        "the screen still believes the new model reasons"
    );
    assert_eq!(
        screen.catalog.effort, None,
        "the level survived onto a model whose request cannot carry it"
    );
    assert_eq!(screen.status.effort(), None);
    assert_eq!(screen.welcome_mut().facts().model.as_deref(), Some(&*plain));
    assert_eq!(screen.welcome_mut().facts().reasoning, None);
}

/// A reasoning model cannot inherit a level absent from its declared variants.
#[test]
fn choosing_a_reasoning_model_clears_an_unsupported_level() {
    use zuno_llm::effort::ReasoningEffort;

    let (sender, _shutdown) = terminal_event_channel();
    let mut catalog = reasoning_catalog();
    let next = catalog.models[1].id.clone();
    catalog.reasoning_efforts.insert(
        next.clone(),
        vec![ReasoningEffort::Low, ReasoningEffort::High],
    );
    catalog.effort = Some(ReasoningEffort::Xhigh);
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender).with_catalog(catalog);
    screen.status_mut().set_effort(Some(ReasoningEffort::Xhigh));

    screen.adopt(crate::views::picker::MODEL_DIALOG_ID, &next);

    assert!(screen.catalog.reasoning);
    assert_eq!(screen.catalog.effort, None);
    assert_eq!(screen.status.effort(), None);
    assert_eq!(screen.welcome_mut().facts().model.as_deref(), Some(&*next));
    assert_eq!(screen.welcome_mut().facts().reasoning, None);
}

/// `ctrl+x` then `down` opens the delegated-task view.
///
/// Driven through the dispatcher and the dialog host rather than by calling the handler,
/// because the thing under test is that the *chord* reaches a surface: `session_child_first`
/// was a row in the shipped table with no handler anywhere in the crate.
#[test]
fn the_leader_down_chord_opens_the_delegated_task_view() {
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let (sender, _receiver) = terminal_event_channel();
    let mut screen = SessionScreen::new(ViewContext::defaults(), sender);
    screen
        .transcript_mut()
        .transcript_mut()
        .push(delegating_message());
    let host = crate::views::dialog::DialogHost::new(ViewContext::defaults(), Box::new(screen));
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(host));

    for event in [
        KeyEvent {
            code: crossterm::event::KeyCode::Char('x'),
            modifiers: crossterm::event::KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        },
        press(crossterm::event::KeyCode::Down),
    ] {
        dispatcher.handle_event(&crate::app::AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(event),
        )));
    }

    let joined = rows(&render_offscreen(&mut dispatcher, 100, 24).expect("infallible")).join("\n");
    assert!(
        joined.contains("Subagents"),
        "ctrl+x down did not open the delegated-task view:\n{joined}"
    );
    assert!(
        joined.contains("survey the auth code"),
        "the view opened without the delegation this session made:\n{joined}"
    );
}

/// One assistant message carrying two `task` calls, as a delegating turn records them.
fn delegating_message() -> Message {
    let call = |id: &str, agent: &str, description: &str, session: &str| {
        crate::views::message::MessagePart::Tool {
            call_id: String::from(id),
            name: String::from("task"),
            ui_intent: zuno_tool::ToolUiIntent::Subagent,
            arguments: format!(
                r#"{{"description":"{description}","prompt":"go","subagent_type":"{agent}"}}"#
            ),
            title: None,
            status: crate::views::message::ToolStatus::Completed,
            output: Some(format!(
                "<task id=\"{session}\" state=\"completed\">\nok\n</task>"
            )),
            diff: None,
        }
    };
    Message {
        role: Role::Assistant,
        id: Some(String::from("msg_delegating")),
        parts: vec![
            call("c1", "explore", "survey the auth code", "ses_child_a"),
            call("c2", "librarian", "find the RFC", "ses_child_b"),
        ],
    }
}

// ---------------------------------------------------------------------------
// Resuming a session with `-s`: the persisted transcript is put back on screen.

/// The two messages a resumed session's persisted history would project to.
///
/// Written as view messages rather than read from a database, because `zuno-tui` has no
/// database dependency and must not acquire one: the projection from stored rows lives in
/// `zuno-cli/src/cmd/tui_replay.rs` and is tested against a real SQLite session there.
/// What this crate owns, and what these tests pin, is what a replayed transcript *does*
/// to the screen.
fn resumed_history() -> Vec<Message> {
    let mut assistant = Message::new(Role::Assistant);
    assistant
        .parts
        .push(crate::views::message::MessagePart::Text {
            text: String::from("The guard clamps the width to the frame."),
        });
    assistant.id = Some(String::from("msg_assistant_0"));
    let mut user = Message::user("what does the guard do");
    user.id = Some(String::from("msg_user_0"));
    vec![user, assistant]
}

/// A resumed session shows the conversation the model has, on the first frame.
///
/// This is the defect in full: the TUI built its `TranscriptView` empty while the next
/// request rehydrated the whole session from the database, so `zuno -s <id>` showed a
/// welcome screen and the reply quoted turns that were nowhere on it.
///
/// Both halves are asserted at both widths because either alone is satisfied by the
/// defect. A screen that replayed the prose but still drew the wordmark would pass the
/// first; one that suppressed the welcome without replaying anything would pass the
/// second and be the original bug.
#[test]
fn a_resumed_session_shows_its_prior_turns_instead_of_the_welcome_screen() {
    for width in [crate::views::SIDEBAR_MIN_WIDTH, 80] {
        let (mut screen, _shutdown) = screen();
        screen
            .transcript_mut()
            .transcript_mut()
            .replay(resumed_history());

        let rendered = rows(&render_offscreen(&mut screen, width, 32).expect("infallible"));
        let joined = rendered.join("\n");

        assert!(
            rendered
                .iter()
                .any(|row| row.contains("what does the guard do")),
            "at {width} columns the resumed prompt is not on screen:\n{joined}"
        );
        assert!(
            rendered
                .iter()
                .any(|row| row.contains("The guard clamps the width")),
            "at {width} columns the resumed reply is not on screen:\n{joined}"
        );
        assert!(
            !rendered.iter().any(|row| row.contains("ZUNO")),
            "at {width} columns a resumed conversation still draws the wordmark, so the user \
             reads a fresh start:\n{joined}"
        );
        assert!(
            !rendered.iter().any(|row| row.contains("/model")),
            "at {width} columns a resumed conversation still draws the welcome hint grid:\n\
             {joined}"
        );
    }
}

/// The welcome guard reads `true` on a replayed transcript, which is what un-hides the panel.
///
/// Asserted on a real frame and located by a service only the ambient panel names, for the
/// reason `the_welcome_surface_survives_a_startup_notice` records: `Context` and `MCP` also
/// occur in the welcome census, so finding either would prove nothing.
#[test]
fn a_resumed_session_draws_the_sidebar_on_its_first_frame() {
    let (mut screen, _shutdown) = screen();
    screen.sidebar_mut().ambient_mut().lsp = vec![crate::views::ambient::Service::new(
        "rust-analyzer",
        crate::views::ambient::Health::Ready,
    )];
    screen
        .transcript_mut()
        .transcript_mut()
        .replay(resumed_history());

    assert!(
        screen.transcript_mut().transcript().conversation_started(),
        "a replayed conversation that does not count as started keeps the welcome screen"
    );
    let rendered = rows(
        &render_offscreen(&mut screen, crate::views::SIDEBAR_MIN_WIDTH, 32).expect("infallible"),
    );
    assert!(
        rendered.iter().any(|row| row.contains("rust-analyzer")),
        "a resumed session opened without its ambient panel:\n{}",
        rendered.join("\n")
    );
}

/// PR #23's title and the replayed transcript coexist, which is the whole resumed frame.
///
/// The two arrived separately — the title in PR #23, the transcript here — and the panel
/// that carries the name is only drawn once a transcript exists beside it. So a resume is
/// the first configuration in which both are on screen at once, and this is the test that
/// says so rather than trusting two features that were never rendered together.
#[test]
fn a_resumed_session_states_its_name_above_the_transcript_it_replayed() {
    let projection =
        crate::views::ambient::SessionTitle::new(Some(String::from("Explaining the width guard")));
    let (sender, _shutdown) = terminal_event_channel();
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), sender).with_session_title(projection);
    screen
        .transcript_mut()
        .transcript_mut()
        .replay(resumed_history());

    let rendered = rows(
        &render_offscreen(&mut screen, crate::views::SIDEBAR_MIN_WIDTH, 32).expect("infallible"),
    );
    let joined = rendered.join("\n");
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("Explaining the width guard")),
        "the name a resumed session already had is not on its first frame:\n{joined}"
    );
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("what does the guard do")),
        "the title is shown but the transcript it names is not:\n{joined}"
    );
}

/// A startup notice still gets the welcome screen when nothing was replayed.
///
/// The regression guard for PR #23's fix, aimed at the ordering this change introduced:
/// `replay` runs *before* the notices in `tui.rs`, so a fresh session replays an empty
/// list and must be indistinguishable from one that never called `replay` at all. An
/// implementation that counted the call rather than the messages would fail here.
#[test]
fn replaying_nothing_leaves_a_fresh_sessions_welcome_screen_intact() {
    let (mut screen, _shutdown) = screen();
    screen.transcript_mut().transcript_mut().replay(Vec::new());
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::notice(
            "warning: theme `nord` was not found; falling back to the built-in palette",
        ));

    assert_eq!(screen.transcript_mut().transcript().replayed(), 0);
    assert!(
        !screen.transcript_mut().transcript().conversation_started(),
        "an empty replay must not claim a conversation began"
    );
    let rendered = rows(&render_offscreen(&mut screen, 120, 32).expect("infallible"));
    assert!(
        rendered.iter().any(|row| row.contains("/model")),
        "an empty replay took the welcome hint grid with it:\n{}",
        rendered.join("\n")
    );
}

/// Replay refuses a transcript that already holds messages, so the prefix is a fact.
#[test]
fn replay_declines_a_transcript_that_has_already_started() {
    let (mut screen, _shutdown) = screen();
    let transcript = screen.transcript_mut().transcript_mut();
    transcript.push(Message::user("typed in this process"));
    transcript.replay(resumed_history());

    assert_eq!(
        transcript.replayed(),
        0,
        "a replay after the first live message would make `replayed` a lie about which \
         messages this process ran"
    );
    assert_eq!(transcript.messages().len(), 1);
}

/// The message menu withholds revert on a replayed prompt, newest included.
///
/// `SnapshotHistory` is rebuilt empty on every launch, so the worktree checkpoint a
/// replayed turn opened belongs to a process that has exited. Offering the row there would
/// produce a menu entry whose only possible outcome is `nothing to undo` — the exact
/// failure mode `message_actions` documents and this codebase has already paid for.
///
/// `Copy` is asserted to remain, because a test that only counted rows would also pass
/// against a menu that stopped opening at all.
#[test]
fn the_message_menu_offers_no_revert_on_a_replayed_prompt() {
    for width in [120u16, 80] {
        let (mut screen, _shutdown) = mouse_screen();
        screen
            .transcript_mut()
            .transcript_mut()
            .replay(resumed_history());
        let rendered = rows(&render_offscreen(&mut screen, width, 32).expect("infallible"));
        let row = u16::try_from(
            rendered
                .iter()
                .position(|row| row.contains("what does the guard do"))
                .unwrap_or_else(|| {
                    panic!(
                        "at {width} columns the replayed prompt is not drawn:\n{}",
                        rendered.join("\n")
                    )
                }),
        )
        .expect("in frame");

        assert!(
            click_at(&mut screen, 3, row).redraw,
            "at {width} columns a press on the replayed prompt was not consumed"
        );
        let mut opened = screen.drain_dialogs();
        assert_eq!(opened.len(), 1, "at {width} columns no menu opened");
        let mut offers = |needle: &str| {
            opened[0]
                .lines(60)
                .iter()
                .any(|line| line.spans.iter().any(|span| span.content.contains(needle)))
        };
        assert!(
            offers("Copy"),
            "at {width} columns the menu on a replayed prompt offers nothing at all, so the \
             absence of revert proves nothing"
        );
        assert!(
            !offers("Revert"),
            "at {width} columns the menu offers revert on a prompt this process never ran, \
             whose checkpoint no longer exists"
        );
    }
}

/// Revert returns on the first prompt this process actually ran.
///
/// The complement of the test above, and it is what keeps the gate from being "revert is
/// gone": a resumed session that types a new prompt has a real checkpoint again, and the
/// row must come back for it.
#[test]
fn the_message_menu_offers_revert_on_a_prompt_typed_after_a_resume() {
    let (mut screen, _shutdown) = mouse_screen();
    screen
        .transcript_mut()
        .transcript_mut()
        .replay(resumed_history());
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("now change the guard"));

    let rendered = rows(&render_offscreen(&mut screen, 120, 32).expect("infallible"));
    let row = u16::try_from(
        rendered
            .iter()
            .position(|row| row.contains("now change the guard"))
            .expect("the live prompt is drawn"),
    )
    .expect("in frame");
    click_at(&mut screen, 3, row);
    let mut opened = screen.drain_dialogs();

    assert!(
        opened[0].lines(60).iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("Revert"))
        }),
        "the prompt this process ran has a live checkpoint and must still offer revert"
    );
}
