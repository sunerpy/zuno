//! What a host needs to trust about the composed screen.

use super::*;
use crate::app::{TerminalEvent, render_offscreen, terminal_event_channel};
use crate::keybind::{KeyDispatcher, Keymap};
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
    assert_eq!(submitted.try_recv().as_deref(), Ok("first"));
    screen.handle_action(action("app_exit"), &press_none());
    assert_eq!(cancelled.try_recv(), Ok(()));

    screen.submit_prompt("second");
    assert_eq!(submitted.try_recv().as_deref(), Ok("second"));
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
