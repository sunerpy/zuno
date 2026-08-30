//! The palette's one promise: every action is reachable, whether it has a key or not.

use super::*;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep};
use crate::views::testkit::{action, press};
use crossterm::event::KeyCode;

fn keymap() -> Keymap {
    Keymap::defaults().expect("the shipped binding table resolves")
}

#[test]
fn palette_lists_every_non_leader_action() {
    let rows = entries(&keymap());
    let expected = DEFINITIONS
        .iter()
        .filter(|definition| !definition.is_leader())
        .count();
    assert_eq!(rows.len(), expected);
    assert!(
        expected > 100,
        "the binding table shrank to {expected} rows"
    );
}

#[test]
fn palette_excludes_the_leader_because_dispatching_a_prefix_does_nothing() {
    let rows = entries(&keymap());
    assert!(
        !rows.iter().any(|row| row.action == "leader"),
        "the leader prefix is offered as an action"
    );
}

#[test]
fn palette_includes_every_action_the_table_leaves_unbound() {
    // The property, not a fixed list: whichever rows ship with `keys: "none"`, the palette is
    // how they are reached, so every one of them must appear. Naming five specific actions
    // here was wrong — `help_show` and four others have since been given default keys, and a
    // list-shaped assertion fails on that improvement instead of on a real regression.
    let rows = entries(&keymap());
    let unbound_in_table = DEFINITIONS
        .iter()
        .filter(|row| !row.is_leader() && row.keys == "none")
        .map(|row| row.name)
        .collect::<Vec<_>>();
    assert!(
        !unbound_in_table.is_empty(),
        "the table binds everything, so the palette's reason to exist needs restating"
    );
    for action in unbound_in_table {
        assert!(
            rows.iter().any(|row| row.action == action),
            "`{action}` ships unbound and is not in the palette, so nothing can reach it"
        );
    }
}

#[test]
fn palette_puts_bound_actions_before_unbound_ones() {
    let rows = entries(&keymap());
    let last_bound = rows
        .iter()
        .rposition(Entry::is_bound)
        .expect("some action is bound");
    let first_unbound = rows
        .iter()
        .position(|row| !row.is_bound())
        .expect("some action is unbound");
    assert!(
        last_bound < first_unbound,
        "bound and unbound rows are interleaved"
    );
}

#[test]
fn palette_names_the_key_for_a_bound_action_and_says_why_an_unbound_one_has_none() {
    let keymap = keymap();
    let dialog = palette(ViewContext::defaults(), &keymap);
    let rows = dialog
        .visible()
        .into_iter()
        .map(|item| (item.value.clone(), item.description.clone()))
        .collect::<Vec<_>>();
    let exit = rows
        .iter()
        .find(|(value, _)| value == "app_exit")
        .expect("app_exit is in the palette");
    let expected = keymap.sequences("app_exit").join(", ");
    assert!(
        exit.1.contains(&expected),
        "the exit row does not name its key: {}",
        exit.1
    );
    // And an action the user's keymap leaves unbound says why it has no key. Chosen from the
    // live keymap rather than named, for the reason above.
    let unbound = entries(&keymap)
        .into_iter()
        .find(|entry| !entry.is_bound())
        .expect("some action is unbound");
    let row = rows
        .iter()
        .find(|(value, _)| value == unbound.action)
        .unwrap_or_else(|| panic!("{} is not in the palette", unbound.action));
    assert!(row.1.contains(NO_KEY), "{}", row.1);
}

#[test]
fn palette_resolves_to_the_action_name_the_host_dispatches() {
    let keymap = keymap();
    let mut dialog = palette(ViewContext::defaults(), &keymap);
    dialog.set_filter("Open help dialog");
    let selected = dialog
        .selected()
        .expect("the filter matched a row")
        .value
        .clone();
    assert_eq!(selected, "help_show");
    let step = dialog.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    assert_eq!(
        step,
        DialogStep::Resolved(DialogOutcome::Selected {
            dialog: DIALOG_ID,
            value: String::from("help_show"),
        })
    );
}

#[test]
fn palette_is_searchable_by_description_not_only_by_action_name() {
    // A user looking for "the one that shows every key" does not know it is called
    // `help_show`.
    let keymap = keymap();
    let mut dialog = palette(ViewContext::defaults(), &keymap);
    dialog.set_filter("keybinding");
    assert!(
        !dialog.visible().is_empty(),
        "searching a description found nothing"
    );
}

#[test]
fn palette_renders_its_rows_and_its_title() {
    let keymap = keymap();
    let mut dialog = palette(ViewContext::defaults(), &keymap);
    assert!(
        dialog.title().starts_with("Commands ("),
        "{}",
        dialog.title()
    );
    let lines = dialog.lines(120);
    assert_eq!(lines.len(), ROWS, "the palette did not fill its window");
    for line in lines {
        let used: usize = line
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        assert_eq!(used, 120, "a palette row did not fill its width");
    }
}
