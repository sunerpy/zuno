//! What a host needs to trust about the composed screen.

use super::*;
use crate::app::{TerminalEvent, render_offscreen, terminal_event_channel};
use crate::keybind::{KeyDispatcher, Keymap};
use crate::views::dialog::DialogHost;
use crate::views::editor::Position;
use crate::views::testkit::{action, press, rows};
use crate::views::toast::ToastLevel;
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::StreamEvent;

fn screen() -> (SessionScreen, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = terminal_event_channel();
    (
        SessionScreen::new(ViewContext::defaults(), sender),
        receiver,
    )
}

fn press_none() -> KeyEvent {
    crate::views::testkit::press(crossterm::event::KeyCode::Null)
}

#[test]
fn session_screen_renders_the_transcript_the_status_strip_and_the_prompt() {
    let (mut screen, _shutdown) = screen();
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
        rendered[strip_index(&rendered, &screen, 8)].contains("idle"),
        "the status strip did not render in its own region: {rendered:?}"
    );
    assert!(
        content_row(&rendered, &screen, 8).contains("what I am typing"),
        "the prompt did not render in its own region: {rendered:?}"
    );
}

#[test]
fn session_screen_folds_an_engine_event_into_the_status_strip() {
    let (mut screen, _shutdown) = screen();
    screen.handle_event(&AppEvent::Engine(TurnEvent::ModelResolved {
        step: 1,
        provider_id: String::from("test"),
        model_id: String::from("test-model"),
    }));

    let rendered = rows(&render_offscreen(&mut screen, 40, 8).expect("infallible"));
    assert!(
        rendered[strip_index(&rendered, &screen, 8)].contains("test/test-model"),
        "the resolved model did not reach the status strip: {rendered:?}"
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
        content_row(&rendered, &screen, 8).contains("hi"),
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
        content_row(&rendered, &screen, 8).contains("Pasted"),
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
fn session_screen_unknown_slash_is_visible_and_never_reaches_the_prompt_sink() {
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
    let rendered = rows(&render_offscreen(&mut screen, 100, 12).expect("infallible")).join("\n");
    assert!(
        rendered.contains("unknown command `/not-a-command`"),
        "{rendered}"
    );
    assert!(
        rendered.contains("type `/`") && rendered.contains("ctrl+p"),
        "{rendered}"
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
            },
            crate::views::picker::ModelEntry {
                id: String::from("prov/sonnet"),
                name: String::from("sonnet"),
                provider: String::from("prov"),
            },
        ],
        agents: vec![crate::views::picker::AgentEntry {
            name: String::from("plan"),
            description: String::from("read-only planning"),
        }],
        sessions: Vec::new(),
        model: Some(String::from("prov/haiku")),
        agent: Some(String::from("build")),
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
fn session_screen_says_so_when_a_picker_would_be_empty() {
    // An empty picker that opened would say `no matches`, which a user cannot tell from
    // a surface that failed to load its list.
    let (mut screen, _shutdown) = screen();
    let result = screen.handle_action(action("model_list"), &press_none());
    assert!(result.redraw);
    assert!(
        screen.drain_dialogs().is_empty(),
        "an empty picker was opened anyway"
    );
    let joined = rows(&render_offscreen(&mut screen, 80, 12).expect("infallible")).join("\n");
    assert!(
        joined.contains("nothing to choose from"),
        "the refusal was silent:\n{joined}"
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
    let joined = rows(&render_offscreen(&mut screen, 90, 14).expect("infallible")).join("\n");
    assert!(
        joined.contains("prov/sonnet"),
        "the strip still names the previous model:\n{joined}"
    );
    assert!(
        joined.contains("next turn"),
        "the transcript does not say when the choice takes effect:\n{joined}"
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
    let joined = rows(&render_offscreen(&mut screen, 90, 14).expect("infallible")).join("\n");
    assert!(
        joined.contains("not applied"),
        "a selection with no listener was reported as applied:\n{joined}"
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
    let strip = crate::views::testkit::rows(&rendered)
        .into_iter()
        .rev()
        .find(|row| row.contains("idle"))
        .expect("the status strip");
    assert!(
        strip.contains("src/lib.rs"),
        "the verdict was cleared when the turn ended: [{strip}]"
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
        description: String::from("navigate an indexed codebase"),
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
        description: String::from("navigate\n  a  codebase"),
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

/// The seam defect 3's fix depends on: the transcript can suppress its spinner, but only
/// if something tells it a permission prompt is open. `SessionScreen` sits *below* the
/// dialog stack and cannot see it, so the host reports it on every frame. Without this the
/// suppression is correct, tested, and unreachable — the same shape as the diff viewer that
/// could never open.
#[test]
fn session_stops_spinning_while_a_permission_prompt_is_mounted_over_it() {
    let (mut screen, _shutdown) = screen();
    // A message is required, not decoration: an empty transcript hands the region to the
    // welcome screen, and the spinner this test is about lives in the transcript. Without
    // it the assertions would pass on the status strip's own `working` and prove nothing.
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
        busy.contains("working"),
        "a running turn with nothing outstanding must still spin:\n{busy}"
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
        !waiting.contains("working"),
        "the spinner claimed the process was busy while the prompt asked the user to \
         decide:\n{waiting}"
    );
    assert!(
        waiting.contains("waiting for your approval"),
        "nothing told the user they are the one being waited on:\n{waiting}"
    );

    assert!(
        host.dismiss(),
        "the prompt was mounted, so it can be closed"
    );
    let resumed = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        resumed.contains("working"),
        "the wait notice outlived the prompt that justified it:\n{resumed}"
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
        joined.contains("working"),
        "a picker opened during a live turn is not the turn waiting on the user:\n{joined}"
    );
    assert!(!joined.contains("waiting for your approval"), "{joined}");
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
    // The strip sits directly above the prompt, and the prompt's floor is two rows.
    let strip_row = 30 - 1 - 2;
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
    let empty = screen.transcript.transcript().messages().is_empty();
    let tail = usize::from(welcome_tail_rows(
        empty,
        height,
        STATUS_ROWS,
        prompt_rows(screen.editor.height(), height),
    ));
    let rendered = rows(&render_offscreen(screen, width, height).expect("infallible"));
    let first = rendered
        .iter()
        .position(|row| row.starts_with(PROMPT_MARKER))
        .expect("the prompt paints its gutter marker at every width these tests use");
    rendered.len() - first - tail
}

#[test]
fn session_prompt_keeps_two_rows_for_a_single_line() {
    // Two rows is what the prompt occupied when its height was fixed, so a single-line
    // buffer keeps proportions the user already knows rather than shrinking to one.
    let (mut screen, _shutdown) = screen();
    assert_eq!(prompt_band_rows(&mut screen, 40, 20), 2);
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
    (screen, receiver)
}

/// One wheel notch downwards, observed at `now_ms`.
fn notch(screen: &mut SessionScreen, now_ms: u64) -> EventResult {
    screen.handle_wheel(
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
    screen.handle_wheel(
        &crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 4,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
        now_ms,
    )
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
    for step in 0..60 {
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
    "session_child_cycle",
    "session_child_cycle_reverse",
    "session_child_first",
    "session_compact",
    "session_delete",
    "session_export",
    "session_new",
    "session_parent",
    "session_pin_toggle",
    "session_queued_prompts",
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
    // and growing it silently is impossible. `agent_cycle` and `agent_cycle_reverse` are the
    // two this change took off the list.
    assert_eq!(
        PRESSABLE_BUT_DEAD.len(),
        48,
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
fn prompt_first(rendered: &[String], screen: &SessionScreen, height: u16) -> usize {
    let empty = screen.transcript.transcript().messages().is_empty();
    let band = prompt_rows(screen.editor.height(), height);
    let tail = welcome_tail_rows(empty, height, STATUS_ROWS, band);
    rendered
        .len()
        .saturating_sub(usize::from(tail) + usize::from(band))
}

/// Where the status strip is, which is directly above the prompt band by construction.
fn strip_index(rendered: &[String], screen: &SessionScreen, height: u16) -> usize {
    prompt_first(rendered, screen, height).saturating_sub(STATUS_ROWS as usize)
}

/// The prompt band's rows, located rather than assumed to be the frame's last band.
///
/// Twelve assertions used to index the frame absolutely — `rendered[6]`, `rendered[len - 2]` —
/// which silently encoded "the prompt is the final band". It is not, while the transcript is
/// empty: a tail below it lifts the welcome block and the input into one centred column. An
/// absolute index does not fail informatively when that changes; it reports that some
/// unrelated row lacks the caret. Locating the band keeps every one of those assertions about
/// the band itself.
fn prompt_band<'a>(rendered: &'a [String], screen: &SessionScreen, height: u16) -> &'a [String] {
    let first = prompt_first(rendered, screen, height);
    let band = usize::from(prompt_rows(screen.editor.height(), height));
    &rendered[first.min(rendered.len())..(first + band).min(rendered.len())]
}

/// The prompt band's spacer, which is its last row whenever the band has more than one.
fn spacer_row<'a>(rendered: &'a [String], screen: &SessionScreen, height: u16) -> &'a str {
    prompt_band(rendered, screen, height)
        .last()
        .map_or("", String::as_str)
}

/// The prompt band's first row, the one the caret and the gutter marker share.
fn content_row<'a>(rendered: &'a [String], screen: &SessionScreen, height: u16) -> &'a str {
    prompt_band(rendered, screen, height)
        .first()
        .map_or("", String::as_str)
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
        let band = content_row(&rendered, &blank, 24);
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
            spacer_row(&filled, &tall, 24),
            "",
            "at {width} columns the prompt is flush against its last row, which is the \
             reported defect"
        );
        assert_eq!(
            spacer_row(&rendered, &blank, 24),
            "",
            "at {width} columns an empty prompt has no spacer under it"
        );
        // The right inset, measured rather than assumed: a full row of typed text must stop
        // short of the frame, or the band is only nominally contained.
        let (mut typed, _shutdown) = screen();
        typed.editor.set_text(&"x".repeat(usize::from(width) * 2));
        let rendered = rows(&render_offscreen(&mut typed, width, 24).expect("infallible"));
        let band = content_row(&rendered, &typed, 24);
        assert!(
            crate::views::display_width(band) < usize::from(width),
            "at {width} columns the prompt used its last column, leaving no right inset: \
             {} of {width}",
            crate::views::display_width(band)
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
    let band = content_row(&rendered, &screen, 10);
    assert!(
        band.contains("hi") && band.contains('▏'),
        "the 20x10 prompt lost its content row to chrome: {band:?}"
    );
    assert_eq!(
        spacer_row(&rendered, &screen, 10),
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
    let with_panel = noticed(OVERLONG_NOTICE, crate::views::SIDEBAR_MIN_WIDTH);
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
        rendered.iter().all(|row| !row.contains('│')),
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
#[test]
fn a_delivered_model_choice_is_reported_with_the_success_affordance() {
    let (selections, _keep) = mpsc::channel(4);
    let (screen, _shutdown) = screen();
    let mut screen = screen.with_selection_sink(selections);
    screen.adopt(
        crate::views::picker::MODEL_DIALOG_ID,
        "amazon-bedrock/amazon.nova-lite-v1:0",
    );
    let rendered = rows(&render_offscreen(&mut screen, 120, 24).expect("infallible"));

    let success = notice_body_at(&rendered, ToastLevel::Success);
    assert!(
        success
            .iter()
            .any(|row| row.contains("model set to amazon-bedrock/amazon.nova-lite-v1:0")),
        "a model that was set is not reported at success grade: {rendered:?}"
    );
    // The other half, and the half that fails on the shipped behaviour: the same sentence must
    // not also be reachable through the warning marker. Asserting only the success row would
    // pass a renderer that drew both, and `▲ !` on a confirmation is the reported defect.
    let warned = notice_body_at(&rendered, ToastLevel::Warning);
    assert!(
        !warned.iter().any(|row| row.contains("model set to")),
        "the model confirmation still carries the warning marker: {warned:?}"
    );
}

/// A model choice nobody is listening for stays a warning, because it did not take effect.
#[test]
fn a_refused_model_choice_keeps_the_warning_affordance() {
    // No selection sink, which is the refusal path.
    let (mut screen, _shutdown) = screen();
    screen.adopt(crate::views::picker::MODEL_DIALOG_ID, "p/m");
    let rendered = rows(&render_offscreen(&mut screen, 120, 24).expect("infallible"));

    let warned = notice_body_at(&rendered, ToastLevel::Warning);
    assert!(
        warned
            .iter()
            .any(|row| row.contains("not applied: nothing is listening")),
        "a refused selection is not reported at warning grade: {rendered:?}"
    );
    assert!(
        notice_body_at(&rendered, ToastLevel::Success).is_empty(),
        "a selection that reached nothing was reported as a success: {rendered:?}"
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
        let first = prompt_first(&rendered, &blank, height);
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
        let first = prompt_first(&rendered, &used, height);
        let band = usize::from(prompt_rows(used.editor.height(), height));
        assert_eq!(
            first + band,
            rendered.len(),
            "at {width}x{height} a used session pays for the welcome tail it cannot see"
        );
    }
}

/// The tail cannot starve the region it exists to centre, including at 20x10.
#[test]
fn the_welcome_tail_never_takes_the_row_the_welcome_needs() {
    for height in 1..=12u16 {
        let band = prompt_rows(0, height);
        let tail = welcome_tail_rows(true, height, STATUS_ROWS, band);
        assert!(
            STATUS_ROWS + band + tail < height || height <= STATUS_ROWS + band,
            "at {height} rows the chrome and the tail leave the body nothing: \
             status {STATUS_ROWS} + prompt {band} + tail {tail}"
        );
        let (mut screen, _shutdown) = screen();
        screen.editor.set_text("hi");
        // The panic guard, exercised through the frame rather than the arithmetic: a tail that
        // fabricated a row the buffer does not own panics inside ratatui.
        let rendered = rows(&render_offscreen(&mut screen, 20, height).expect("infallible"));
        assert_eq!(rendered.len(), usize::from(height));
    }
}
