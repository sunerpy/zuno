//! The welcome screen's two promises: it fills the frame, and its keys are real.

use super::*;
use crate::app::render_offscreen;
use crate::keybind::{ActionComponent as _, Keymap};
use crate::views::testkit::rows;
use crossterm::event::KeyCode;

fn keymap() -> Keymap {
    Keymap::defaults().expect("the shipped binding table resolves")
}

fn facts() -> WelcomeFacts {
    WelcomeFacts {
        directory: Some(String::from("~/src/zuno")),
        branch: Some(String::from("task-r17-solo")),
        model: Some(String::from("myopenai/claude-haiku-4-5")),
        agent: Some(String::from("build")),
        version: Some(String::from("0.1.0")),
        tools: Some(13),
        mcp: Some(2),
        lsp: Some(1),
        skills: Some(0),
    }
}

fn view() -> WelcomeView {
    WelcomeView::new(ViewContext::defaults())
        .with_facts(facts())
        .with_tip(0)
}

/// How many of `height` rows carry at least one non-space character.
fn painted(view: &mut WelcomeView, width: u16, height: u16) -> usize {
    rows(&render_offscreen(view, width, height).expect("infallible"))
        .iter()
        .filter(|row| !row.trim().is_empty())
        .count()
}

// ---------------------------------------------------------------------------
// The defect this module exists to fix
// ---------------------------------------------------------------------------

#[test]
fn views_welcome_fills_a_large_frame_instead_of_leaving_it_blank() {
    // The measured defect: two non-empty rows out of fifty. A floor rather than an
    // exact count so that editing a tip cannot fail this test, and a high enough floor
    // that a screen which regressed to a couple of rows cannot pass it.
    let mut view = view();
    let count = painted(&mut view, 200, 50);
    assert!(
        count >= 14,
        "the welcome screen painted only {count} of 50 rows, which is the emptiness it \
         exists to replace"
    );
}

#[test]
fn views_welcome_degrades_to_a_compact_brand_when_the_wordmark_cannot_fit() {
    // 30 columns cannot carry a 36-column wordmark, and 14 rows cannot carry six rows of
    // it plus the facts. Either alone is enough to fall back.
    let mut view = view();
    for (width, height) in [(30, 40), (200, 14)] {
        let narrow =
            rows(&render_offscreen(&mut view, width, height).expect("infallible")).join("\n");
        assert!(
            narrow.contains("ZUNO"),
            "the compact brand is missing at {width}x{height}:\n{narrow}"
        );
        assert!(
            !narrow.contains("███████╗"),
            "the full wordmark was drawn at {width}x{height}, where it does not fit:\n{narrow}"
        );
    }
}

#[test]
fn views_welcome_never_overflows_eighty_columns() {
    // A row wider than the frame is what makes a narrow terminal look broken rather
    // than narrow, and 80 columns is the width that has to stay correct.
    let mut view = view();
    for row in rows(&render_offscreen(&mut view, 80, 24).expect("infallible")) {
        assert!(
            row.chars().count() <= 80,
            "a row overflowed 80 columns: {row:?}"
        );
    }
    let joined = rows(&render_offscreen(&mut view, 80, 24).expect("infallible")).join("\n");
    assert!(
        joined.contains("~/src/zuno"),
        "80 columns lost the location row:\n{joined}"
    );
}

#[test]
fn views_welcome_draws_the_wordmark_when_it_fits() {
    let mut view = view();
    let wide = rows(&render_offscreen(&mut view, 200, 50).expect("infallible")).join("\n");
    assert!(
        wide.contains("███████╗"),
        "the wordmark is missing on a terminal with room for it:\n{wide}"
    );
    assert!(WelcomeView::wordmark_fits(200, 50));
    assert!(!WelcomeView::wordmark_fits(39, 50));
    assert!(!WelcomeView::wordmark_fits(200, 12));
}

#[test]
fn views_welcome_paints_the_wordmark_shadow_in_its_own_colour() {
    // The per-cell paint is the whole reason the wordmark is not ASCII art. A single
    // styled string could carry only one colour, so this asserts two.
    let context = ViewContext::defaults();
    let mut view = view();
    let buffer = render_offscreen(&mut view, 200, 50).expect("infallible");
    let brand = ratatui::style::Color::from(context.palette.primary);
    let shadow = ratatui::style::Color::from(crate::theme::tint(
        context.palette.background_panel,
        context.palette.primary,
        SHADOW_MIX,
    ));
    let mut brand_cells = 0;
    let mut shadow_cells = 0;
    for y in 0..50 {
        for x in 0..200 {
            let cell = &buffer[(x, y)];
            if cell.symbol() == "█" && cell.fg == brand {
                brand_cells += 1;
            }
            if cell.fg == shadow && cell.symbol() != " " {
                shadow_cells += 1;
            }
        }
    }
    assert!(brand_cells > 0, "no wordmark cell took the brand colour");
    assert!(
        shadow_cells > 0,
        "no wordmark cell took the shadow colour, so the wordmark is flat"
    );
    assert_ne!(brand, shadow, "the shadow resolved to the brand colour");
}

// ---------------------------------------------------------------------------
// Honesty: the facts and the keys
// ---------------------------------------------------------------------------

#[test]
fn views_welcome_states_the_directory_model_and_inventory() {
    let mut view = view();
    let joined = rows(&render_offscreen(&mut view, 200, 50).expect("infallible")).join("\n");
    for needle in [
        "~/src/zuno",
        "task-r17-solo",
        "build",
        "myopenai/claude-haiku-4-5",
        "13 tools",
        "2 mcp",
        "1 lsp",
        "0 skills",
        "zuno 0.1.0",
    ] {
        assert!(joined.contains(needle), "`{needle}` is missing:\n{joined}");
    }
}

#[test]
fn views_welcome_omits_a_fact_it_does_not_have_rather_than_inventing_one() {
    let mut view = WelcomeView::new(ViewContext::defaults());
    let joined = rows(&render_offscreen(&mut view, 200, 50).expect("infallible")).join("\n");
    for forbidden in ["unknown", "n/a", "None"] {
        assert!(
            !joined.contains(forbidden),
            "the screen invented `{forbidden}` for a fact it does not have:\n{joined}"
        );
    }
}

#[test]
fn views_welcome_zero_counts_are_shown_because_zero_is_a_fact() {
    let mut view = WelcomeView::new(ViewContext::defaults()).with_facts(WelcomeFacts {
        mcp: Some(0),
        skills: Some(0),
        ..WelcomeFacts::default()
    });
    let joined = rows(&render_offscreen(&mut view, 200, 50).expect("infallible")).join("\n");
    assert!(
        joined.contains("0 mcp") && joined.contains("0 skills"),
        "a zero count was dropped, which reads as `not measured`:\n{joined}"
    );
}

#[test]
fn views_welcome_every_hint_names_an_action_the_shipped_table_has() {
    // The guard against a dead hint: a typo'd action name would render no key at all,
    // and the row would silently vanish from the grid.
    for (action, label) in HINTS {
        assert!(
            crate::keybind::definition(action).is_some(),
            "the welcome grid advertises `{action}` ({label}), which is not in the \
             binding table"
        );
    }
}

#[test]
fn views_welcome_hints_show_the_users_own_spelling_not_the_default() {
    use crate::config::BindingValue;
    let mut config = crate::config::ResolvedTuiConfig::default();
    config
        .keybinds
        .insert(String::from("input_submit"), BindingValue::parse("ctrl+j"));
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);
    let context = ViewContext::new(&resolved, config);
    let mut view = WelcomeView::new(context).with_facts(facts());
    let joined = rows(&render_offscreen(&mut view, 200, 50).expect("infallible")).join("\n");
    assert!(
        joined.contains("ctrl+j send"),
        "the grid advertised the default spelling after the user rebound it:\n{joined}"
    );
}

#[test]
fn views_welcome_drops_a_hint_the_user_disabled() {
    use crate::config::BindingValue;
    let mut config = crate::config::ResolvedTuiConfig::default();
    config
        .keybinds
        .insert(String::from("mcp_list"), BindingValue::parse("none"));
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);
    let context = ViewContext::new(&resolved, config);
    assert_eq!(crate::views::key_label("mcp_list", &context), None);
    assert!(crate::views::key_label("model_list", &context).is_some());
}

// ---------------------------------------------------------------------------
// Tips
// ---------------------------------------------------------------------------

#[test]
fn views_welcome_tips_advance_and_can_be_hidden() {
    let mut view = view();
    let first = view.tip();
    view.next_tip();
    assert_ne!(view.tip(), first, "the tip did not change");

    view.hide_tips();
    assert!(!view.tips_visible());
    let hidden = rows(&render_offscreen(&mut view, 200, 50).expect("infallible")).join("\n");
    assert!(
        !hidden.contains("● tip"),
        "the tip row is still drawn after being hidden:\n{hidden}"
    );

    // A hidden row plus "next" means "show me one again", not "silently advance".
    view.next_tip();
    assert!(view.tips_visible());
}

#[test]
fn views_welcome_tip_index_wraps_instead_of_panicking() {
    let mut view = WelcomeView::new(ViewContext::defaults()).with_tip(usize::MAX);
    assert!(TIPS.contains(&view.tip()));
    view.next_tip();
    assert!(TIPS.contains(&view.tip()));
}

#[test]
fn views_welcome_facts_can_be_stated_after_construction() {
    let mut view = WelcomeView::new(ViewContext::defaults());
    view.facts_mut().model = Some(String::from("provider/model"));
    let joined = rows(&render_offscreen(&mut view, 200, 50).expect("infallible")).join("\n");
    assert!(joined.contains("provider/model"), "{joined}");
}

#[test]
fn views_welcome_renders_into_a_degenerate_area_without_panicking() {
    let mut view = view();
    for (width, height) in [(0, 0), (1, 1), (200, 1), (1, 50), (36, 20)] {
        let _ = render_offscreen(&mut view, width, height).expect("infallible");
    }
}

#[test]
fn views_welcome_every_advertised_key_is_routed_by_the_session_screen() {
    // The guard for the defect this whole surface exists to remove, found in my own work:
    // `command_list` was advertised as `ctrl+p commands`, was bound in the shipped table,
    // and reached nothing — pressing it did nothing at all, which a user cannot tell from
    // a broken program.
    //
    // Asserting the routing rather than the rendering is the point. A hint renders
    // identically whether or not anything handles it, so no amount of buffer inspection
    // can catch this; the only observable difference is that `handle_action` reports the
    // action as handled.
    let (shutdown, _receiver) = crate::app::terminal_event_channel();
    let mut unrouted = Vec::new();
    for (action, label) in HINTS {
        let definition = crate::keybind::definition(action)
            .unwrap_or_else(|| panic!("`{action}` is not in the binding table"));
        // Each action gets a screen with everything a picker could need, so a refusal is
        // never "the catalog was empty".
        let (sender, _keep) = crate::app::terminal_event_channel();
        let _ = &shutdown;
        let mut screen = crate::views::session::SessionScreen::new(ViewContext::defaults(), sender)
            .with_keymap(keymap())
            .with_catalog(crate::views::session::SessionCatalog {
                models: vec![crate::views::picker::ModelEntry {
                    id: String::from("prov/one"),
                    name: String::from("one"),
                    provider: String::from("prov"),
                }],
                agents: vec![crate::views::picker::AgentEntry {
                    name: String::from("build"),
                    description: String::new(),
                }],
                sessions: vec![crate::views::picker::SessionEntry {
                    id: String::from("ses_1"),
                    title: String::from("earlier"),
                    when: String::from("today"),
                }],
                ..crate::views::session::SessionCatalog::default()
            });
        screen.sidebar_mut().ambient_mut().mcp = vec![crate::views::ambient::Service::new(
            "alpha",
            crate::views::ambient::Health::Ready,
        )];

        // A character is typed first, through the same path a real keystroke takes.
        // `input_submit` on an empty prompt legitimately does nothing, so an empty editor
        // would make this assertion fail for a reason that is not a routing defect.
        screen.handle_event(&crate::app::AppEvent::Terminal(
            crate::app::TerminalEvent::Input(crossterm::event::Event::Key(
                crate::views::testkit::press(KeyCode::Char('x')),
            )),
        ));

        let handled = screen
            .handle_action(definition, &crate::views::testkit::press(KeyCode::Null))
            .handled;
        if !handled {
            unrouted.push(format!("{action} (advertised as `{label}`)"));
        }
    }
    assert!(
        unrouted.is_empty(),
        "the welcome screen advertises keys that reach nothing:\n{}",
        unrouted.join("\n")
    );
}

#[test]
fn views_welcome_mcp_and_help_hints_open_a_real_surface() {
    // The complement: routed *and* actually produces a dialog, not just a handled action.
    let (sender, _receiver) = crate::app::terminal_event_channel();
    let mut screen = crate::views::session::SessionScreen::new(ViewContext::defaults(), sender)
        .with_keymap(keymap());
    screen.sidebar_mut().ambient_mut().mcp = vec![
        crate::views::ambient::Service::new("alpha", crate::views::ambient::Health::Faulted)
            .detailed("handshake timed out"),
    ];

    screen.handle_action(
        crate::views::testkit::action("mcp_list"),
        &crate::views::testkit::press(KeyCode::Null),
    );
    let opened = screen.drain_dialogs();
    assert_eq!(opened.len(), 1, "`mcp_list` opened nothing");
    assert_eq!(opened[0].id(), crate::views::picker::MCP_DIALOG_ID);

    screen.handle_action(
        crate::views::testkit::action("help_show"),
        &crate::views::testkit::press(KeyCode::Null),
    );
    let opened = screen.drain_dialogs();
    assert_eq!(opened.len(), 1, "`help_show` opened nothing");
    assert_eq!(opened[0].id(), crate::views::help::DIALOG_ID);
}

#[test]
fn views_welcome_every_advertised_action_lives_in_a_scope_the_screen_resolves() {
    // The blind spot that let the same defect ship twice in one change. The routing guard
    // above calls `handle_action` directly, which *bypasses scope resolution* — so
    // `mcp_list` passed it while `<leader>k` did nothing on a real terminal, because
    // `scopes()` did not list `mcp` and `KeyDispatcher` therefore never resolved the
    // press. Routing and scope are two independent requirements, and a hint needs both.
    let resolved = crate::views::session::scopes();
    let mut missing = Vec::new();
    for (action, label) in HINTS {
        let definition = crate::keybind::definition(action)
            .unwrap_or_else(|| panic!("`{action}` is not in the binding table"));
        if !resolved.iter().any(|scope| scope == definition.scope) {
            missing.push(format!(
                "{action} (advertised as `{label}`) lives in scope `{}`",
                definition.scope
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "these advertised actions are in scopes `session::scopes()` does not resolve, so \
         their keys can never fire:\n{}",
        missing.join("\n")
    );
}

#[test]
fn views_welcome_advertised_keys_resolve_through_a_real_dispatcher() {
    // The end-to-end complement of the two guards above: press the actual chord the grid
    // advertises, through `KeyDispatcher`, and require that something consumed it. This is
    // the only form of the assertion that could have caught the missing `mcp` scope, so it
    // is the one worth keeping.
    for (action, label) in HINTS {
        let Some(spelling) = crate::views::key_label(action, &ViewContext::defaults()) else {
            // Unbound in the shipped table (`tool_details`, `display_thinking`, `mcp_list`,
            // `help_show`). The grid renders no key for these, so there is no chord to
            // press; `views_welcome_every_hint_names_an_action...` covers them instead.
            continue;
        };
        let mut dispatcher = crate::keybind::KeyDispatcher::new(
            keymap(),
            crate::views::session::scopes(),
            Box::new(routable_screen()),
        );
        // A character first, through the dispatcher, for the same reason the routing guard
        // types one: `input_submit` on an empty prompt correctly submits nothing, so an
        // empty editor would fail this for a reason that is not a resolution defect.
        dispatcher.handle_event(&crate::app::AppEvent::Terminal(
            crate::app::TerminalEvent::Input(crossterm::event::Event::Key(
                crate::views::testkit::press(KeyCode::Char('x')),
            )),
        ));

        // `parse_sequence` is what the keymap itself uses, so the `<leader>` token is
        // substituted the same way rather than parsed as a literal chord — which is how an
        // earlier draft of this test "failed" on `<leader>m`, a key verified working on a
        // real terminal.
        let sequence = crate::keybind::parse_sequence(&spelling, keymap().leader())
            .unwrap_or_else(|error| panic!("`{spelling}` for `{action}` does not parse: {error}"));
        let mut handled = false;
        for parsed in sequence {
            let result = dispatcher.handle_event(&crate::app::AppEvent::Terminal(
                crate::app::TerminalEvent::Input(crossterm::event::Event::Key(chord_event(
                    &parsed,
                ))),
            ));
            handled = handled || result.handled;
        }
        assert!(
            handled,
            "`{spelling}`, advertised as `{label}` for `{action}`, was not consumed by the \
             dispatcher"
        );
    }
}

/// A screen with everything a picker could ask for, so a refusal is never an empty list.
fn routable_screen() -> crate::views::session::SessionScreen {
    let (sender, receiver) = crate::app::terminal_event_channel();
    // The receiver is leaked deliberately: dropping it closes the shutdown channel, and a
    // closed channel would make `app_exit` look unhandled for the wrong reason.
    std::mem::forget(receiver);
    let mut screen = crate::views::session::SessionScreen::new(ViewContext::defaults(), sender)
        .with_keymap(keymap())
        .with_catalog(crate::views::session::SessionCatalog {
            models: vec![crate::views::picker::ModelEntry {
                id: String::from("prov/one"),
                name: String::from("one"),
                provider: String::from("prov"),
            }],
            agents: vec![crate::views::picker::AgentEntry {
                name: String::from("build"),
                description: String::new(),
            }],
            sessions: vec![crate::views::picker::SessionEntry {
                id: String::from("ses_1"),
                title: String::from("earlier"),
                when: String::from("today"),
            }],
            ..crate::views::session::SessionCatalog::default()
        });
    screen.sidebar_mut().ambient_mut().mcp = vec![crate::views::ambient::Service::new(
        "alpha",
        crate::views::ambient::Health::Ready,
    )];
    screen
}

/// The key event that would produce `chord`.
fn chord_event(chord: &crate::keybind::Chord) -> crossterm::event::KeyEvent {
    let rendered = chord.to_string();
    let mut modifiers = crossterm::event::KeyModifiers::NONE;
    if rendered.contains("ctrl+") {
        modifiers |= crossterm::event::KeyModifiers::CONTROL;
    }
    if rendered.contains("alt+") {
        modifiers |= crossterm::event::KeyModifiers::ALT;
    }
    if rendered.contains("shift+") {
        modifiers |= crossterm::event::KeyModifiers::SHIFT;
    }
    let last = rendered.rsplit('+').next().unwrap_or_default().to_owned();
    let code = match last.as_str() {
        "return" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        other => KeyCode::Char(other.chars().next().unwrap_or('?')),
    };
    crossterm::event::KeyEvent {
        code,
        modifiers,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}
