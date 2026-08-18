//! What a host needs to trust about the composed screen.

use super::*;
use crate::app::{TerminalEvent, render_offscreen, terminal_event_channel};
use crate::keybind::{KeyDispatcher, Keymap};
use crate::views::editor::Position;
use crate::views::testkit::{action, rows};
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
        rendered[5].contains("idle"),
        "the status strip did not render in its own region: {rendered:?}"
    );
    assert!(
        rendered[6].contains("what I am typing"),
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
        rendered[5].contains("test/test-model"),
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
        rendered[6].contains("hi"),
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
        rendered[6].contains("Pasted"),
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
fn session_screen_undo_and_redo_stay_typed_for_the_runtime_host() {
    for (text, expected) in [
        ("/undo", PromptSubmission::Host(HostCommand::Undo)),
        ("/redo", PromptSubmission::Host(HostCommand::Redo)),
    ] {
        let (sender, _shutdown) = terminal_event_channel();
        let (prompts, mut submitted) = mpsc::channel(1);
        let mut screen =
            SessionScreen::new(ViewContext::defaults(), sender).with_prompt_sink(prompts);
        screen.editor.set_text(text);

        screen.handle_action(action("input_submit"), &press_none());

        assert_eq!(submitted.try_recv(), Ok(expected));
        assert_eq!(screen.submissions(), [text]);
    }
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
    let (mut screen, _shutdown) = screen();
    let empty = rows(&render_offscreen(&mut screen, 200, 40).expect("infallible")).join("\n");
    assert!(
        empty.contains("a coding agent"),
        "the welcome surface is missing on an empty transcript:\n{empty}"
    );
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user("first prompt"));
    let used = rows(&render_offscreen(&mut screen, 200, 40).expect("infallible")).join("\n");
    assert!(
        !used.contains("a coding agent"),
        "the welcome surface survived the first message:\n{used}"
    );
    assert!(used.contains("first prompt"), "{used}");
}

#[test]
fn session_reports_the_files_a_finished_turn_wrote_and_no_others() {
    use zuno_engine::r#loop::TurnEvent;
    let (shutdown, _keep) = mpsc::channel(4);
    let (edits, mut written) = mpsc::channel(4);
    let mut screen = SessionScreen::new(ViewContext::defaults(), shutdown).with_edit_sink(edits);

    let dispatched = |name: &str, title: &str, is_error: bool| {
        AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: String::from("c"),
            name: name.to_owned(),
            title: title.to_owned(),
            output: String::new(),
            diff: None,
            is_error,
        })
    };

    screen.handle_event(&dispatched("edit", "src/lib.rs", false));
    // A read changed nothing; reporting its pre-existing diagnostics would attribute
    // somebody else's problem to this turn.
    screen.handle_event(&dispatched("read", "src/other.rs", false));
    // A failed write changed nothing either.
    screen.handle_event(&dispatched("write", "src/failed.rs", true));
    screen.handle_event(&dispatched("write", "src/new.rs", false));
    // The same file twice is one entry.
    screen.handle_event(&dispatched("edit", "src/lib.rs", false));
    assert!(
        written.try_recv().is_err(),
        "the batch was sent before the turn finished"
    );

    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));
    assert_eq!(
        written.try_recv(),
        Ok(vec![String::from("src/lib.rs"), String::from("src/new.rs")])
    );
}

#[test]
fn session_sends_nothing_for_a_turn_that_wrote_nothing() {
    use zuno_engine::r#loop::TurnEvent;
    let (shutdown, _keep) = mpsc::channel(4);
    let (edits, mut written) = mpsc::channel(4);
    let mut screen = SessionScreen::new(ViewContext::defaults(), shutdown).with_edit_sink(edits);
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnCompleted {
        assistant_message_id: String::from("msg_1"),
        steps: 1,
    }));
    assert!(written.try_recv().is_err());
}

#[test]
fn session_reports_an_interrupted_turns_writes_too() {
    // An aborted turn may already have written; the user still needs to know whether what
    // landed compiles.
    use zuno_engine::r#loop::TurnEvent;
    let (shutdown, _keep) = mpsc::channel(4);
    let (edits, mut written) = mpsc::channel(4);
    let mut screen = SessionScreen::new(ViewContext::defaults(), shutdown).with_edit_sink(edits);
    screen.handle_event(&AppEvent::Engine(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: String::from("c"),
        name: String::from("edit"),
        title: String::from("src/lib.rs"),
        output: String::new(),
        diff: None,
        is_error: false,
    }));
    screen.handle_event(&AppEvent::Engine(TurnEvent::TurnInterrupted {
        assistant_message_id: None,
        steps: 1,
    }));
    assert_eq!(written.try_recv(), Ok(vec![String::from("src/lib.rs")]));
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
    // `scopes()` now lists `diff`, whose viewer owns bare `q`, `n`, `p`, `d`, `v`, `s`
    // and `b`. Those resolve on the session screen whether or not the viewer is open, so
    // typing survives only because this screen returns `IGNORED` for them and an
    // unhandled action falls through to the editor. Give the screen an arm for one of
    // those letters and this test is what says the prompt stopped accepting it.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let (sender, _receiver) = terminal_event_channel();
    let screen = SessionScreen::new(ViewContext::defaults(), sender);
    let mut dispatcher = KeyDispatcher::new(keymap, scopes(), Box::new(screen));

    let typed = "qnpdvsb";
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
        "the diff scope's bare letters stopped reaching the prompt; `{typed}` is not on \
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
            | crate::views::message::MessagePart::Notice { text } => Some(text.clone()),
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
    let notice = last_message(&mut screen);
    assert!(
        notice.contains("copied") && notice.contains('3'),
        "a successful copy said nothing useful: {notice:?}"
    );
}

#[test]
fn session_screen_a_failed_copy_is_visible_rather_than_silent() {
    let (sender, _shutdown) = terminal_event_channel();
    let mut screen =
        SessionScreen::new(ViewContext::defaults(), sender).with_clipboard(broken_clipboard());
    screen.editor.insert_char('x');

    copy_action(&mut screen);

    let notice = last_message(&mut screen);
    assert!(
        notice.contains("copy failed"),
        "a host with no clipboard mechanism reported success: {notice:?}"
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
    let notice = last_message(&mut screen);
    assert!(
        notice.contains("nothing to copy"),
        "an empty copy said nothing: {notice:?}"
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

/// How many rows the prompt band occupied in a `width` × `height` frame.
///
/// Measured back out of the frame — the status strip is located by the state word it
/// always prints, and everything below it is the prompt — rather than by calling
/// `prompt_rows`. Asserting the function's return value would keep passing if `render`
/// stopped consulting it, which is the second-source-of-truth hole this measurement
/// exists to close.
fn prompt_band_rows(screen: &mut SessionScreen, width: u16, height: u16) -> usize {
    let rendered = rows(&render_offscreen(screen, width, height).expect("infallible"));
    let status = rendered
        .iter()
        .position(|row| row.contains("idle") || row.contains("working"))
        .expect("the status strip is always on screen");
    rendered.len() - status - 1
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
