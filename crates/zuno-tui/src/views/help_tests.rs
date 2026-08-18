//! Help view tests: it is generated from the live keymap.

use super::*;
use crate::app::render_offscreen;
use crate::config::{BindingValue, ResolvedTuiConfig};
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::KeyCode;

fn keymap() -> Keymap {
    Keymap::defaults().expect("the shipped table builds")
}

fn help() -> HelpView {
    HelpView::new(ViewContext::defaults(), &keymap())
}

fn render(view: HelpView, width: u16, height: u16) -> Vec<String> {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context))),
    );
    host.open(Box::new(view));
    rows(&render_offscreen(&mut host, width, height).expect("infallible"))
}

// ---------------------------------------------------------------------------
// It comes from the table
// ---------------------------------------------------------------------------

#[test]
fn views_help_covers_every_action_in_the_shipped_table() {
    let grouped = entries(&keymap());
    let total = grouped.values().map(Vec::len).sum::<usize>();
    let expected = DEFINITIONS.iter().filter(|row| !row.is_leader()).count();
    assert_eq!(
        total, expected,
        "the help view lists {total} actions but the table has {expected}"
    );
    assert!(
        expected >= 180,
        "the binding table shrank to {expected} rows, so this scan may be looking at the wrong thing"
    );
}

#[test]
fn views_help_groups_by_scope() {
    let view = help();
    let scopes = view.scopes();
    assert!(
        scopes.len() >= 5,
        "only {} scopes were found, so grouping is not happening: {scopes:?}",
        scopes.len()
    );
    for expected in ["dialog.select", "prompt.autocomplete", "permission.prompt"] {
        assert!(
            scopes.contains(&expected),
            "scope {expected:?} is missing: {scopes:?}"
        );
    }
}

#[test]
fn views_help_reports_the_keys_the_keymap_actually_resolved() {
    // `session_interrupt` rather than `help_show`: the shipped table gives
    // `help_show` the spelling `none` (`keybind.ts` — it is reachable only through
    // the command palette), so it would prove nothing about resolved keys.
    let grouped = entries(&keymap());
    let entry = grouped
        .values()
        .flatten()
        .find(|entry| entry.action == "session_interrupt")
        .expect("`session_interrupt` is in the table");
    assert_ne!(entry.keys, UNBOUND);
    assert_eq!(
        entry.description,
        crate::keybind::definition("session_interrupt")
            .expect("in the table")
            .description
    );
}

#[test]
fn views_help_shows_an_action_the_shipped_table_leaves_unbound() {
    // `session_share` ships with the spelling `none` and this build does not bind it, so
    // a user has to be able to see that it exists and needs binding. The action is looked
    // up rather than hard-coded to `help_show`, which this build *does* bind now: an
    // action from `SHIPPED_DEFAULTS` would assert the opposite of what this guards.
    let unbound_action = crate::keybind::DEFINITIONS
        .iter()
        .find(|definition| {
            definition.keys == crate::keybind::NO_KEY
                && !crate::keybind::SHIPPED_DEFAULTS
                    .iter()
                    .any(|(name, _)| *name == definition.name)
        })
        .expect("some action is still unbound, or this test has nothing to guard");
    let entry = entries(&keymap())
        .values()
        .flatten()
        .find(|entry| entry.action == unbound_action.name)
        .cloned()
        .expect("listed even though it is unbound");
    assert_eq!(entry.keys, UNBOUND);
}

#[test]
fn views_help_follows_a_rebound_key() {
    // The reason help is generated: a hand-written list would still show `f1`.
    let config = ResolvedTuiConfig {
        keybinds: [(String::from("help_show"), BindingValue::parse("ctrl+alt+h"))]
            .into_iter()
            .collect(),
        ..ResolvedTuiConfig::default()
    };
    let keymap = Keymap::from_config(&config).expect("the override is valid");
    let entry = entries(&keymap)
        .values()
        .flatten()
        .find(|entry| entry.action == "help_show")
        .cloned()
        .expect("still listed");
    assert_ne!(entry.keys, UNBOUND, "the override was not applied at all");
    assert!(
        entry.keys.contains("ctrl+alt+h"),
        "the help view shows a stale key: {}",
        entry.keys
    );
}

#[test]
fn views_help_still_lists_an_unbound_action() {
    // A user who unbound something needs to see it exists and has no key.
    let config = ResolvedTuiConfig {
        keybinds: [(String::from("session_interrupt"), BindingValue::Disabled)]
            .into_iter()
            .collect(),
        ..ResolvedTuiConfig::default()
    };
    let keymap = Keymap::from_config(&config).expect("unbinding is valid");
    let entry = entries(&keymap)
        .values()
        .flatten()
        .find(|entry| entry.action == "session_interrupt")
        .cloned()
        .expect("an unbound action is still listed");
    assert_eq!(entry.keys, UNBOUND);
}

// ---------------------------------------------------------------------------
// The off-screen assertion
// ---------------------------------------------------------------------------

#[test]
fn views_help_renders_offscreen_naming_a_real_binding() {
    let mut view = help();
    view.set_filter("interrupt");
    let joined = render(view, 70, 12).join("\n");
    assert!(
        joined.contains("Keybindings"),
        "the help title is missing:\n{joined}"
    );
    let definition = crate::keybind::definition("session_interrupt").expect("in the table");
    assert!(
        joined.contains(definition.description),
        "the description of a real binding is missing:\n{joined}"
    );
    let spelling = keymap()
        .sequences("session_interrupt")
        .first()
        .cloned()
        .expect("bound by default");
    assert!(
        joined.contains(&spelling),
        "the key {spelling:?} the keymap resolved is not shown:\n{joined}"
    );
    assert!(
        joined.contains("session"),
        "the scope heading is missing:\n{joined}"
    );
}

#[test]
fn views_help_unbound_row_is_muted_from_the_palette() {
    let context = ViewContext::defaults();
    let config = ResolvedTuiConfig {
        keybinds: [(String::from("app_debug"), BindingValue::Disabled)]
            .into_iter()
            .collect(),
        ..ResolvedTuiConfig::default()
    };
    let keymap = Keymap::from_config(&config).expect("valid");
    let mut view = HelpView::new(context.clone(), &keymap);
    view.set_filter("unbound");
    let lines = view.lines(60);
    let muted = ratatui::style::Color::from(context.palette().text_muted);
    assert!(
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.fg == Some(muted)),
        "an unbound row is not de-emphasised, so it reads as available"
    );
}

// ---------------------------------------------------------------------------
// Filtering and scrolling
// ---------------------------------------------------------------------------

#[test]
fn views_help_filter_matches_action_description_and_keys() {
    let mut view = help();
    view.set_filter("session_fork");
    assert!(
        view.rows()
            .iter()
            .any(|row| matches!(row, Row::Entry(entry) if entry.action == "session_fork")),
        "filtering by action name found nothing"
    );

    view.set_filter("Fork");
    assert!(
        !view.rows().is_empty(),
        "filtering by description found nothing"
    );

    view.set_filter("pageup");
    assert!(
        view.rows()
            .iter()
            .any(|row| matches!(row, Row::Entry(entry) if entry.keys.contains("pageup"))),
        "filtering by key spelling found nothing"
    );
}

#[test]
fn views_help_filter_with_no_matches_produces_no_rows_and_no_headings() {
    let mut view = help();
    view.set_filter("zzzz-not-a-thing");
    assert!(
        view.rows().is_empty(),
        "an empty group still emitted its heading: {:?}",
        view.rows()
    );
}

#[test]
fn views_help_typing_and_backspace_drive_the_filter() {
    let mut view = help();
    for character in "fork".chars() {
        view.handle_action(action("messages_next"), &press(KeyCode::Char(character)));
    }
    assert_eq!(view.filter(), "fork");
    view.handle_action(action("input_backspace"), &press(KeyCode::Backspace));
    assert_eq!(view.filter(), "for");
}

#[test]
fn views_help_scrolls_and_clamps() {
    let mut view = help();
    let first = render(help(), 70, 6);
    view.handle_action(action("dialog.select.page_down"), &press(KeyCode::PageDown));
    let scrolled = {
        let mut moved = help();
        moved.handle_action(action("dialog.select.page_down"), &press(KeyCode::PageDown));
        render(moved, 70, 6)
    };
    assert_ne!(first, scrolled, "paging down changed nothing");

    // Paging far past the end must clamp rather than render an empty dialog.
    let mut far = help();
    for _ in 0..500 {
        far.handle_action(action("dialog.select.page_down"), &press(KeyCode::PageDown));
    }
    let rendered = render(far, 70, 6);
    assert!(
        rendered.iter().any(|row| !row.trim().is_empty()),
        "scrolling past the end left the help view blank: {rendered:?}"
    );
}

#[test]
fn views_help_home_returns_to_the_top() {
    let mut view = help();
    view.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    view.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    view.handle_action(action("dialog.select.home"), &press(KeyCode::Home));
    let rendered = render(view, 70, 6);
    let baseline = render(help(), 70, 6);
    assert_eq!(rendered, baseline);
}

#[test]
fn views_help_scroll_up_clamps_at_zero() {
    let mut view = help();
    for _ in 0..5 {
        view.handle_action(action("dialog.select.prev"), &press(KeyCode::Up));
    }
    assert_eq!(render(view, 70, 6), render(help(), 70, 6));
}

#[test]
fn views_help_closes_on_escape_and_on_its_own_action() {
    for name in ["app_exit", "help_show", "dialog.select.submit"] {
        let mut view = help();
        assert_eq!(
            view.handle_action(action(name), &press(KeyCode::Esc)),
            DialogStep::Resolved(DialogOutcome::Cancelled),
            "{name} did not close the help view"
        );
    }
}

#[test]
fn help_keeps_the_description_separated_from_a_long_key_list() {
    // Measured on a real terminal before the fix: `ctrl+c, ctrl+d, ctrl+x q` is 23 columns
    // wide against a 22-column field, so the row rendered as `ctrl+x qExit the application`
    // — the separator vanished and the description read as part of the chord.
    let keymap = Keymap::defaults().expect("the shipped binding table resolves");
    let mut view = HelpView::new(ViewContext::defaults(), &keymap);
    view.set_filter("Exit the application");
    let rows = view
        .lines(120)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let row = rows
        .iter()
        .find(|row| row.contains("Exit the application"))
        .unwrap_or_else(|| panic!("the exit row is missing: {rows:?}"));
    assert!(
        row.contains("  Exit the application"),
        "the description is not separated from the keys: [{row}]"
    );
    assert!(
        !row.contains("qExit"),
        "the key column overflowed into the description: [{row}]"
    );
}

#[test]
fn help_rows_all_fill_the_width_they_were_given() {
    let keymap = Keymap::defaults().expect("the shipped binding table resolves");
    let mut view = HelpView::new(ViewContext::defaults(), &keymap);
    for width in [80_u16, 120, 200] {
        for line in view.lines(width) {
            let used: usize = line
                .spans
                .iter()
                .map(|span| crate::views::display_width(&span.content))
                .sum();
            assert_eq!(used, usize::from(width), "at width {width}");
        }
    }
}
