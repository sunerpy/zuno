//! What a host needs to trust about the composed screen.

use super::*;
use crate::app::{TerminalEvent, render_offscreen, terminal_event_channel};
use crate::keybind::{KeyDispatcher, Keymap};
use crate::views::testkit::{action, rows};
use oc_engine::r#loop::TurnEvent;
use oc_llm::event::StreamEvent;

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
    let (_engine_sender, engine_receiver) = oc_engine::r#loop::event_channel();
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
