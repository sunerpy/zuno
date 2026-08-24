//! Keybind engine tests.
//!
//! The binding table is a product surface, so the coverage test is driven by a
//! checked-in Zuno fixture rather than a hand-written list: a row that disappears
//! from either side is named, not silently skipped.

use super::*;
use crate::config::{BindingItem, ResolveOptions, TuiConfig};
use crossterm::event::KeyEventState;
use ratatui::layout::Rect;

/// The shipped Zuno keybind baseline.
const FIXTURE: &str = include_str!("../tests/fixtures/zuno-keybinds.tsv");

/// The count the plan and the oracle agree on.
const EXPECTED_ROWS: usize = 185;

/// Rows whose default spelling deliberately differs from the oracle, and why.
///
/// The fixture is a compatibility surface, so a difference has to be declared rather than
/// tolerated — `table_matches_the_upstream_fixture_row_for_row` requires the shipped value to
/// equal the declared replacement *and* to still differ from upstream, so an entry cannot
/// linger as a blanket suppression after the reason expires.
const SPELLING_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "agent_cycle_reverse",
        "shift+backtab,backtab,shift+tab",
        "upstream's `shift+tab` cannot resolve here: crossterm reports the press as \
         `KeyCode::BackTab` with `SHIFT`, so the chord it produces is `shift+backtab` and the \
         oracle's spelling never matches. Upstream reads keys through a different runtime and \
         does not have this fold. The upstream spelling is kept last for the Kitty protocol, \
         which does report `Tab` with `SHIFT`.",
    ),
    (
        "input_newline",
        "shift+return,alt+return,ctrl+j",
        "Zuno reserves `ctrl+return` for the explicit steer gesture while a turn is running; \
         newline remains available through Shift+Enter, Alt+Enter, and Ctrl+J.",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    keys: String,
    command: String,
    prevent_default: Option<bool>,
    description: String,
}

fn fixture_rows() -> Vec<Row> {
    let rows = FIXTURE
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(
                fields.len(),
                5,
                "fixture row has {} fields, expected 5: {line:?}",
                fields.len()
            );
            Row {
                name: fields[0].to_owned(),
                keys: fields[1].to_owned(),
                command: fields[2].to_owned(),
                prevent_default: match fields[3] {
                    "" => None,
                    "true" => Some(true),
                    "false" => Some(false),
                    other => panic!("unexpected prevent_default {other:?} in fixture row {line:?}"),
                },
                description: fields[4].to_owned(),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        EXPECTED_ROWS,
        "the upstream fixture has {} rows; it must have {EXPECTED_ROWS} or the coverage test is \
         measuring the wrong thing",
        rows.len()
    );
    rows
}

fn derived_scope(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((namespace, _)) => namespace.to_owned(),
        None => name.split('_').next().unwrap_or(name).to_owned(),
    }
}

fn keymap_with(overrides: &[(&str, BindingValue)]) -> Keymap {
    let mut config = ResolvedTuiConfig::default();
    for (name, value) in overrides {
        config.keybinds.insert((*name).to_owned(), value.clone());
    }
    Keymap::from_config(&config).expect("the keymap should build")
}

fn key(code: KeyCode, modifiers: CrosstermModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// Replay a rendered chord sequence and return the final resolution.
fn replay(keymap: &mut Keymap, scope: &str, sequence: &[Chord], now: Instant) -> Resolution {
    let mut last = Resolution::Unmatched;
    for chord in sequence {
        last = keymap.resolve(&[scope], *chord, now);
    }
    last
}

#[test]
fn table_matches_the_zuno_fixture_row_for_row() {
    let rows = fixture_rows();
    assert_eq!(
        DEFINITIONS.len(),
        EXPECTED_ROWS,
        "DEFINITIONS has {} rows, expected {EXPECTED_ROWS}",
        DEFINITIONS.len()
    );

    for (index, row) in rows.iter().enumerate() {
        let found = definition(&row.name).unwrap_or_else(|| {
            panic!(
                "Zuno binding `{}` (fixture row {}) is missing from DEFINITIONS",
                row.name,
                index + 1
            )
        });
        if let Some((_, expected, _)) = SPELLING_DIVERGENCES
            .iter()
            .find(|(name, _, _)| *name == row.name)
        {
            assert_eq!(
                found.keys, *expected,
                "`{}` is a declared spelling divergence, but its keys are neither upstream's \
                 nor the declared replacement",
                row.name
            );
            assert_ne!(
                found.keys, row.keys,
                "`{}` is listed in `SPELLING_DIVERGENCES` but now matches upstream; remove the \
                 entry rather than leaving a suppression behind",
                row.name
            );
        } else {
            assert_eq!(found.keys, row.keys, "`{}` default keys differ", row.name);
        }
        assert_eq!(found.command, row.command, "`{}` command differs", row.name);
        assert_eq!(
            found.prevent_default, row.prevent_default,
            "`{}` prevent_default differs",
            row.name
        );
        assert_eq!(
            found.description, row.description,
            "`{}` description differs",
            row.name
        );
        assert_eq!(
            found.scope,
            derived_scope(&row.name),
            "`{}` scope was not derived by the documented rule",
            row.name
        );
        assert_eq!(
            DEFINITIONS[index].name,
            row.name,
            "DEFINITIONS row {} is `{}` but the Zuno fixture has `{}`; table order is part of the surface",
            index + 1,
            DEFINITIONS[index].name,
            row.name
        );
    }

    for entry in DEFINITIONS {
        assert!(
            rows.iter().any(|row| row.name == entry.name),
            "DEFINITIONS has `{}`, which upstream does not",
            entry.name
        );
    }
}

#[test]
fn every_binding_resolves_to_its_documented_action() {
    let rows = fixture_rows();
    let leader = Chord::parse("ctrl+x").expect("the default leader parses");
    let start = Instant::now();

    let mut asserted = 0_usize;
    let mut unbound = 0_usize;
    let mut sequences_checked = 0_usize;

    for row in &rows {
        let entry = definition(&row.name)
            .unwrap_or_else(|| panic!("`{}` is missing from DEFINITIONS", row.name));

        if entry.is_leader() {
            let keymap = keymap_with(&[]);
            assert_eq!(
                keymap.leader(),
                Chord::parse(&row.keys).expect("the leader spelling parses"),
                "the `leader` row must configure the leader chord"
            );
            asserted += 1;
            continue;
        }

        let value = BindingValue::parse(entry.keys);
        let spellings = value.spellings();
        if spellings.is_empty() {
            assert_eq!(
                entry.keys, "none",
                "`{}` has no shipped spellings but is not `none`",
                row.name
            );
            let keymap = keymap_with(&[]);
            // An action upstream ships with no key must stay unbound here *unless* this
            // build gives it one. Upstream reaches these through its command palette, so
            // `none` costs it nothing; here it means the surface behind the action cannot
            // be opened at all. `SHIPPED_DEFAULTS` is that list, and every row of it is
            // asserted to resolve — in the shipped scope, to the action that asked for it
            // — rather than merely being non-empty.
            match crate::keybind::SHIPPED_DEFAULTS
                .iter()
                .find(|(name, _)| *name == row.name)
            {
                None => {
                    assert!(
                        keymap.sequences(&row.name).is_empty(),
                        "`{}` is `none` upstream, is not in SHIPPED_DEFAULTS, and must \
                         therefore be unbound",
                        row.name
                    );
                    unbound += 1;
                }
                Some((_, shipped)) => {
                    let sequence = parse_sequence(shipped, leader)
                        .expect("every shipped default spelling must parse");
                    let mut keymap = keymap_with(&[]);
                    assert!(
                        matches!(
                            replay(&mut keymap, entry.scope, &sequence, start),
                            Resolution::Action { definition, .. } if definition.name == row.name
                        ),
                        "`{}` is bound to `{shipped}` by SHIPPED_DEFAULTS but does not \
                         resolve to itself in scope `{}`",
                        row.name,
                        entry.scope
                    );
                    sequences_checked += 1;
                }
            }
            asserted += 1;
            continue;
        }

        for spelling in spellings {
            let sequence =
                parse_sequence(spelling, leader).expect("every default spelling must parse");
            let mut keymap = keymap_with(&[]);
            let resolution = replay(&mut keymap, entry.scope, &sequence, start);
            match resolution {
                Resolution::Action { definition, .. } => assert_eq!(
                    definition.name, row.name,
                    "`{spelling}` in scope `{}` resolved to `{}`, not `{}`",
                    entry.scope, definition.name, row.name
                ),
                other => panic!(
                    "`{spelling}` in scope `{}` did not resolve to `{}`: {other:?}",
                    entry.scope, row.name
                ),
            }
            if sequence.len() > 1 {
                let mut keymap = keymap_with(&[]);
                assert_eq!(
                    keymap.resolve(&[entry.scope], sequence[0], start),
                    Resolution::Pending,
                    "the first chord of `{spelling}` must leave the engine pending"
                );
            }
            sequences_checked += 1;
        }
        asserted += 1;
    }

    assert_eq!(
        asserted, EXPECTED_ROWS,
        "only {asserted} of {EXPECTED_ROWS} bindings were asserted"
    );
    // Upstream ships 43 `none` rows. The ones this build binds are still counted as
    // upstream-unbound — the parity claim is about upstream's table, not this keymap — so
    // the two numbers are asserted separately: the total must stay 43, and the remainder
    // must be exactly the rows `SHIPPED_DEFAULTS` does not claim. Asserting only the
    // remainder would let a silent shrinkage of the upstream table pass.
    const UPSTREAM_UNBOUND: usize = 43;
    assert_eq!(
        unbound + crate::keybind::SHIPPED_DEFAULTS.len(),
        UPSTREAM_UNBOUND,
        "upstream ships {UPSTREAM_UNBOUND} `none` bindings; found {unbound} still unbound \
         plus {} bound by this build",
        crate::keybind::SHIPPED_DEFAULTS.len()
    );
    assert!(
        sequences_checked >= 170,
        "only {sequences_checked} key sequences were replayed; the coverage test is not measuring \
         the real table"
    );
}

#[test]
fn defaults_have_no_conflicts_and_cover_every_scope() {
    let keymap = Keymap::defaults().expect("the shipped defaults must not conflict");
    let scopes = keymap.scope_names();
    let local_scopes = crate::keybind::LOCAL_DEFINITIONS
        .iter()
        .map(|definition| definition.scope)
        .filter(|scope| {
            *scope != "leader"
                && !crate::keybind::DEFINITIONS
                    .iter()
                    .any(|definition| definition.scope == *scope)
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let expected_scopes = 38 + local_scopes;
    assert_eq!(
        scopes.len(),
        expected_scopes,
        "expected 38 upstream action scopes plus {local_scopes} Zuno-native scopes, found {}: \
         {scopes:?}",
        scopes.len()
    );
    assert_eq!(
        keymap.leader_timeout(),
        crate::config::DEFAULT_LEADER_TIMEOUT
    );
}

#[test]
fn permission_horizontal_arrows_resolve_only_in_the_permission_dialog_scope() {
    let mut keymap = Keymap::defaults().expect("the shipped defaults must build");
    let now = Instant::now();

    for (spelling, expected) in [
        ("left", "dialog.permission.prev"),
        ("right", "dialog.permission.next"),
    ] {
        let Resolution::Action { definition, .. } = keymap.resolve(
            &["dialog.permission"],
            Chord::parse(spelling).expect("valid arrow"),
            now,
        ) else {
            panic!("{spelling} did not resolve in the permission dialog");
        };
        assert_eq!(definition.name, expected);
    }

    assert!(
        matches!(
            keymap.resolve(
                &["dialog.select"],
                Chord::parse("left").expect("valid arrow"),
                now
            ),
            Resolution::Unmatched
        ),
        "horizontal permission navigation leaked into vertical list dialogs"
    );
}

#[test]
fn question_navigation_resolves_only_in_the_question_dialog_scope() {
    let mut keymap = Keymap::defaults().expect("the shipped defaults must build");
    let now = Instant::now();

    for (spelling, expected) in [
        ("left", "dialog.question.prev_question"),
        ("right", "dialog.question.next_question"),
        ("k", "dialog.question.prev_option"),
        ("j", "dialog.question.next_option"),
    ] {
        let Resolution::Action { definition, .. } = keymap.resolve(
            &["dialog.question"],
            Chord::parse(spelling).expect("valid question key"),
            now,
        ) else {
            panic!("{spelling} did not resolve in the question dialog");
        };
        assert_eq!(definition.name, expected);
    }

    assert!(
        matches!(
            keymap.resolve(
                &["dialog.select"],
                Chord::parse("right").expect("valid arrow"),
                now
            ),
            Resolution::Unmatched
        ),
        "question navigation leaked into other list dialogs"
    );
}

#[test]
fn a_leader_sequence_resolves_end_to_end() {
    let mut keymap = Keymap::defaults().expect("defaults build");
    let start = Instant::now();
    let leader = keymap.leader();

    assert_eq!(
        keymap.resolve(&["session"], leader, start),
        Resolution::Pending
    );
    assert_eq!(render_sequence(keymap.pending()), "ctrl+x");

    let compact = Chord::parse("c").expect("`c` parses");
    match keymap.resolve(&["session"], compact, start) {
        Resolution::Action { definition, .. } => {
            assert_eq!(definition.name, "session_compact");
            assert_eq!(definition.command, "session.compact");
        }
        other => panic!("`<leader>c` did not resolve: {other:?}"),
    }
    assert!(
        keymap.pending().is_empty(),
        "a completed sequence must clear the pending state"
    );
}

#[test]
fn a_leader_sequence_times_out_per_config() {
    let timeout = Duration::from_millis(250);
    let config = ResolvedTuiConfig {
        leader_timeout: timeout,
        ..ResolvedTuiConfig::default()
    };
    let mut keymap = Keymap::from_config(&config).expect("defaults with a short timeout build");
    assert_eq!(keymap.leader_timeout(), timeout);

    let start = Instant::now();
    let leader = keymap.leader();
    let compact = Chord::parse("c").expect("`c` parses");

    // Inside the window the sequence still completes.
    assert_eq!(
        keymap.resolve(&["session"], leader, start),
        Resolution::Pending
    );
    match keymap.resolve(
        &["session"],
        compact,
        start + timeout - Duration::from_millis(1),
    ) {
        Resolution::Action { definition, .. } => assert_eq!(definition.name, "session_compact"),
        other => panic!("the sequence should still be live just inside the timeout: {other:?}"),
    }

    // Past the window the pending leader is gone, so `c` stands alone -- and
    // nothing in the `session` scope binds a bare `c`.
    assert_eq!(
        keymap.resolve(&["session"], leader, start),
        Resolution::Pending
    );
    assert_eq!(render_sequence(keymap.pending()), "ctrl+x");
    assert_eq!(
        keymap.resolve(&["session"], compact, start + timeout),
        Resolution::Unmatched,
        "a leader sequence must be abandoned once the configured timeout elapses"
    );
    assert!(keymap.pending().is_empty());
}

#[test]
fn expire_drops_a_pending_sequence_without_a_key_press() {
    let timeout = Duration::from_millis(100);
    let config = ResolvedTuiConfig {
        leader_timeout: timeout,
        ..ResolvedTuiConfig::default()
    };
    let mut keymap = Keymap::from_config(&config).expect("keymap builds");
    let start = Instant::now();

    assert_eq!(
        keymap.resolve(&["session"], keymap.leader(), start),
        Resolution::Pending
    );
    assert!(!keymap.expire(start + timeout - Duration::from_millis(1)));
    assert_eq!(render_sequence(keymap.pending()), "ctrl+x");
    assert!(keymap.expire(start + timeout));
    assert!(keymap.pending().is_empty());
}

#[test]
fn a_user_override_replaces_exactly_one_binding() {
    let baseline = Keymap::defaults().expect("defaults build");
    let overridden = keymap_with(&[("session_compact", BindingValue::parse("ctrl+alt+w"))]);

    let mut changed = Vec::new();
    for entry in DEFINITIONS.iter().filter(|entry| !entry.is_leader()) {
        let before = baseline.sequences(entry.name);
        let after = overridden.sequences(entry.name);
        if before != after {
            changed.push((entry.name, before, after));
        }
    }

    assert_eq!(
        changed.len(),
        1,
        "an override of one action changed {} bindings: {changed:?}",
        changed.len()
    );
    let (name, before, after) = &changed[0];
    assert_eq!(*name, "session_compact");
    assert_eq!(before, &vec!["ctrl+x c".to_owned()]);
    assert_eq!(after, &vec!["ctrl+alt+w".to_owned()]);
    assert_eq!(overridden.leader(), baseline.leader());

    let mut keymap = overridden;
    let start = Instant::now();
    match keymap.resolve(
        &["session"],
        Chord::parse("ctrl+alt+w").expect("parses"),
        start,
    ) {
        Resolution::Action { definition, .. } => assert_eq!(definition.name, "session_compact"),
        other => panic!("the override should resolve: {other:?}"),
    }
    assert_eq!(
        keymap.resolve(&["session"], keymap.leader(), start),
        Resolution::Pending,
        "the leader still starts other sequences"
    );
}

#[test]
fn an_override_can_unbind_with_false_or_none() {
    for value in [BindingValue::Disabled, BindingValue::parse("none")] {
        let keymap = keymap_with(&[("session_compact", value)]);
        assert!(keymap.sequences("session_compact").is_empty());
    }
}

#[test]
fn a_duplicate_binding_is_reported_with_both_actions_named() {
    let mut config = ResolvedTuiConfig::default();
    config
        .keybinds
        .insert("session_new".to_owned(), BindingValue::parse("<leader>l"));

    let error = Keymap::from_config(&config).expect_err("a duplicate must not build");
    let KeybindError::Conflicts { conflicts } = &error else {
        panic!("expected a conflict report, got {error:?}");
    };
    assert_eq!(conflicts.len(), 1, "unexpected conflicts: {conflicts:?}");
    let conflict = &conflicts[0];
    assert_eq!(conflict.kind, ConflictKind::Duplicate);
    assert_eq!(conflict.scope, "session");
    assert_eq!(conflict.sequence, "ctrl+x l");
    assert_eq!(
        conflict.actions,
        vec!["session_list".to_owned(), "session_new".to_owned()]
    );
    assert_eq!(
        conflict.to_string(),
        "`ctrl+x l` is bound to both `session_list` and `session_new` in scope `session`"
    );
    assert_eq!(
        error.to_string(),
        "1 keybind conflict:\n  `ctrl+x l` is bound to both `session_list` and `session_new` in \
         scope `session`"
    );
}

#[test]
fn three_actions_on_one_key_all_appear_in_the_report() {
    let mut config = ResolvedTuiConfig::default();
    config
        .keybinds
        .insert("session_new".to_owned(), BindingValue::parse("<leader>l"));
    config
        .keybinds
        .insert("session_fork".to_owned(), BindingValue::parse("<leader>l"));

    let error = Keymap::from_config(&config).expect_err("a duplicate must not build");
    let KeybindError::Conflicts { conflicts } = &error else {
        panic!("expected a conflict report, got {error:?}");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0].to_string(),
        "`ctrl+x l` is bound to all of `session_fork`, `session_list`, and `session_new` in scope \
         `session`"
    );
}

#[test]
fn a_binding_that_shadows_a_longer_sequence_is_reported() {
    let mut config = ResolvedTuiConfig::default();
    config
        .keybinds
        .insert("session_new".to_owned(), BindingValue::parse("ctrl+x"));

    let error = Keymap::from_config(&config).expect_err("a shadowed sequence must not build");
    let KeybindError::Conflicts { conflicts } = &error else {
        panic!("expected a conflict report, got {error:?}");
    };
    let shadow = conflicts
        .iter()
        .find(|conflict| matches!(conflict.kind, ConflictKind::PrefixShadow { .. }))
        .expect("a prefix shadow should be reported");
    assert_eq!(shadow.scope, "session");
    assert_eq!(shadow.sequence, "ctrl+x");
    assert!(
        shadow.actions.contains(&"session_new".to_owned()) && shadow.actions.len() == 2,
        "both the shadowing and the shadowed action must be named: {:?}",
        shadow.actions
    );
    assert_eq!(
        shadow.to_string(),
        "`ctrl+x` is bound to both `session_export` and `session_new` in scope `session`, which \
         shadows the longer sequence `ctrl+x x`"
    );
}

#[test]
fn an_unrecognized_keybind_name_is_reported() {
    let mut config = ResolvedTuiConfig::default();
    config
        .keybinds
        .insert("sesion_compact".to_owned(), BindingValue::parse("ctrl+q"));
    let error = Keymap::from_config(&config).expect_err("a typo must not build");
    assert_eq!(
        error.to_string(),
        "unrecognized keybind: sesion_compact",
        "a typo in a keybind name must be named, not ignored"
    );
}

#[test]
fn an_invalid_spelling_names_the_action_and_the_spelling() {
    let mut config = ResolvedTuiConfig::default();
    config
        .keybinds
        .insert("session_new".to_owned(), BindingValue::parse("hyper+wat"));
    let error = Keymap::from_config(&config).expect_err("an unknown key must not build");
    assert_eq!(
        error.to_string(),
        "`session_new` has an invalid key spelling `hyper+wat`: `wat` is not a key name"
    );
}

#[test]
fn unbinding_the_leader_is_refused_rather_than_silently_dropping_28_bindings() {
    let mut config = ResolvedTuiConfig::default();
    config
        .keybinds
        .insert("leader".to_owned(), BindingValue::Disabled);
    let error = Keymap::from_config(&config).expect_err("an unbound leader must not build");
    assert_eq!(
        error,
        KeybindError::LeaderDisabled { count: 28 },
        "28 default spellings contain `<leader>`"
    );
    assert!(error.to_string().contains("28 bindings use `<leader>`"));
}

#[test]
fn rebinding_the_leader_moves_every_leader_sequence() {
    let keymap = keymap_with(&[("leader", BindingValue::parse("ctrl+space"))]);
    assert_eq!(keymap.leader(), Chord::new(Modifiers::CTRL, Key::Char(' ')));
    assert_eq!(
        keymap.sequences("session_compact"),
        vec!["ctrl+space c".to_owned()]
    );
}

#[test]
fn scope_precedence_is_ordered_rather_than_arbitrary() {
    // `right` is `diff_expand` in the diff viewer and `session_child_cycle` in a
    // session. Whichever scope the caller lists first is the one that answers.
    let mut keymap = Keymap::defaults().expect("defaults build");
    let start = Instant::now();
    let right = Chord::parse("right").expect("parses");

    match keymap.resolve(&["diff", "session"], right, start) {
        Resolution::Action { definition, .. } => assert_eq!(definition.name, "diff_expand"),
        other => panic!("expected diff_expand: {other:?}"),
    }
    match keymap.resolve(&["session", "diff"], right, start) {
        Resolution::Action { definition, .. } => {
            assert_eq!(definition.name, "session_child_cycle");
        }
        other => panic!("expected session_child_cycle: {other:?}"),
    }
}

#[test]
fn an_unmatched_chord_abandons_a_pending_sequence() {
    let mut keymap = Keymap::defaults().expect("defaults build");
    let start = Instant::now();
    assert_eq!(
        keymap.resolve(&["session"], keymap.leader(), start),
        Resolution::Pending
    );
    assert_eq!(
        keymap.resolve(&["session"], Chord::parse("z").expect("parses"), start),
        Resolution::Unmatched
    );
    assert!(keymap.pending().is_empty());
}

#[test]
fn multi_chord_sequences_are_not_a_leader_special_case() {
    let keymap = keymap_with(&[("session_new", BindingValue::parse("ctrl+w g s"))]);
    assert_eq!(
        keymap.sequences("session_new"),
        vec!["ctrl+w g s".to_owned()]
    );

    let mut keymap = keymap;
    let start = Instant::now();
    for chord in ["ctrl+w", "g"] {
        assert_eq!(
            keymap.resolve(&["session"], Chord::parse(chord).expect("parses"), start),
            Resolution::Pending,
            "`{chord}` should extend the sequence"
        );
    }
    match keymap.resolve(&["session"], Chord::parse("s").expect("parses"), start) {
        Resolution::Action { definition, .. } => assert_eq!(definition.name, "session_new"),
        other => panic!("a three-chord sequence should resolve: {other:?}"),
    }
}

#[test]
fn chord_spellings_normalize_the_way_a_terminal_reports_them() {
    let uppercase = Chord::parse("E").expect("parses");
    assert_eq!(uppercase, Chord::parse("shift+e").expect("parses"));
    assert_eq!(uppercase.to_string(), "shift+e");

    // A terminal may or may not set shift alongside a shifted glyph.
    assert_eq!(
        Chord::from_key_event(&key(KeyCode::Char('?'), CrosstermModifiers::SHIFT)),
        Some(Chord::parse("?").expect("parses"))
    );
    assert_eq!(
        Chord::from_key_event(&key(KeyCode::Char('E'), CrosstermModifiers::NONE)),
        Some(uppercase)
    );
    assert_eq!(
        Chord::from_key_event(&key(KeyCode::Char('E'), CrosstermModifiers::SHIFT)),
        Some(uppercase)
    );

    assert_eq!(Chord::parse("enter"), Chord::parse("return"));
    assert_eq!(Chord::parse("space").expect("parses").to_string(), "space");
    assert_eq!(
        Chord::parse("ctrl+alt+shift+k")
            .expect("parses")
            .to_string(),
        "ctrl+alt+shift+k"
    );
    assert_eq!(
        Chord::parse("ctrl++").expect("a trailing plus is the key"),
        Chord::new(Modifiers::CTRL, Key::Char('+'))
    );
    assert_eq!(
        Chord::parse("shift+f2").expect("parses"),
        Chord::new(Modifiers::SHIFT, Key::Function(2))
    );
    assert_eq!(Chord::parse(""), Err(SpellingError::Empty));
    assert_eq!(
        Chord::parse("krtl+a"),
        Err(SpellingError::UnknownModifier("krtl".to_owned()))
    );
}

#[test]
fn every_default_spelling_parses() {
    let leader = Chord::parse("ctrl+x").expect("parses");
    let mut parsed = 0_usize;
    for entry in DEFINITIONS {
        for spelling in BindingValue::parse(entry.keys).spellings() {
            parse_sequence(spelling, leader).unwrap_or_else(|error| {
                panic!(
                    "`{}` spelling `{spelling}` failed to parse: {error}",
                    entry.name
                )
            });
            parsed += 1;
        }
    }
    assert!(
        parsed >= 170,
        "only {parsed} spellings were parsed; the table is not being read"
    );
}

// -- KeyDispatcher: the seam that keeps bindings out of view code ------------

#[derive(Default)]
struct Recorder {
    actions: Vec<&'static str>,
    pending: Vec<String>,
    continuations: Vec<usize>,
    raw_events: usize,
}

impl Component for Recorder {
    fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect) {}

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        self.raw_events += 1;
        EventResult::IGNORED
    }
}

impl ActionComponent for Recorder {
    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> EventResult {
        self.actions.push(action.name);
        EventResult::REDRAW
    }

    fn pending_changed(&mut self, pending: &PendingPrefix) -> EventResult {
        self.pending.push(pending.label());
        self.continuations.push(pending.continuations.len());
        EventResult::REDRAW
    }
}

#[test]
fn the_dispatcher_hands_actions_not_keys_to_the_component() {
    let keymap = Keymap::defaults().expect("defaults build");
    let mut dispatcher = KeyDispatcher::new(
        keymap,
        vec!["session".to_owned()],
        Box::new(Recorder::default()),
    );
    let start = Instant::now();

    let leader = dispatcher.keymap().leader();
    let pending =
        dispatcher.dispatch_key(&key(KeyCode::Char('x'), CrosstermModifiers::CONTROL), start);
    assert_eq!(leader, Chord::parse("ctrl+x").expect("parses"));
    assert_eq!(pending, EventResult::REDRAW);

    let fired = dispatcher.dispatch_key(&key(KeyCode::Char('c'), CrosstermModifiers::NONE), start);
    assert_eq!(fired, EventResult::REDRAW);

    // Unmatched must not consume the key — that fall-through is what lets a bare letter
    // reach the editor — but it may still need a frame, because this recorder is a
    // which-key consumer and the panel it opened has to disappear. The two bits are
    // therefore asserted separately rather than against `IGNORED`, which conflates them.
    let unmatched =
        dispatcher.dispatch_key(&key(KeyCode::Char('z'), CrosstermModifiers::NONE), start);
    assert!(
        !unmatched.handled,
        "an unmatched chord was consumed, so its key never reaches the prompt"
    );
    assert!(
        unmatched.redraw,
        "the consumer was told the sequence ended but no frame was requested, so a \
         which-key panel would stay on screen until something else redrew"
    );
    // A component that does not consume pending changes still sees the old contract
    // exactly: nothing happened.
    let mut plain = KeyDispatcher::new(
        Keymap::defaults().expect("defaults build"),
        vec!["session".to_owned()],
        Box::new(Silent),
    );
    assert_eq!(
        plain.dispatch_key(&key(KeyCode::Char('z'), CrosstermModifiers::NONE), start),
        EventResult::IGNORED
    );
}

/// A component with no which-key surface, using every default trait method.
struct Silent;

impl Component for Silent {
    fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect) {}

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

impl ActionComponent for Silent {
    fn handle_action(&mut self, _action: &'static Definition, _event: &KeyEvent) -> EventResult {
        EventResult::IGNORED
    }
}

#[test]
fn the_dispatcher_consumes_terminal_events_from_the_existing_loop() {
    let keymap = Keymap::defaults().expect("defaults build");
    let mut dispatcher = KeyDispatcher::new(
        keymap,
        vec!["diff".to_owned()],
        Box::new(Recorder::default()),
    );

    let event = AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Key(key(
        KeyCode::Char('n'),
        CrosstermModifiers::NONE,
    ))));
    assert_eq!(dispatcher.handle_event(&event), EventResult::REDRAW);

    // Anything that is not a resolvable key press still reaches the component.
    let resize = AppEvent::Terminal(TerminalEvent::Resize {
        width: 80,
        height: 24,
    });
    assert_eq!(dispatcher.handle_event(&resize), EventResult::IGNORED);

    let released = AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Key(KeyEvent {
        code: KeyCode::Char('n'),
        modifiers: CrosstermModifiers::NONE,
        kind: KeyEventKind::Release,
        state: KeyEventState::NONE,
    })));
    assert_eq!(dispatcher.handle_event(&released), EventResult::IGNORED);
}

#[test]
fn the_terminal_suspend_fork_rewrites_exactly_two_bindings() {
    let resolved = TuiConfig::default()
        .resolve(ResolveOptions {
            terminal_suspend: false,
        })
        .expect("resolve succeeds");
    let keymap = Keymap::from_config(&resolved).expect("the rewritten table must not conflict");

    assert!(
        keymap.sequences("terminal_suspend").is_empty(),
        "a host that cannot suspend must not advertise the binding"
    );
    assert_eq!(
        keymap.sequences("input_undo"),
        vec![
            "ctrl+z".to_owned(),
            "ctrl+-".to_owned(),
            "super+z".to_owned()
        ],
        "ctrl+z is worth more as undo than as a no-op"
    );

    let suspendable = TuiConfig::default()
        .resolve(ResolveOptions {
            terminal_suspend: true,
        })
        .expect("resolve succeeds");
    let keymap = Keymap::from_config(&suspendable).expect("defaults build");
    assert_eq!(
        keymap.sequences("terminal_suspend"),
        vec!["ctrl+z".to_owned()]
    );
    assert_eq!(
        keymap.sequences("input_undo"),
        vec!["ctrl+-".to_owned(), "super+z".to_owned()]
    );
}

#[test]
fn an_explicit_input_undo_override_survives_the_terminal_suspend_fork() {
    let mut config = TuiConfig::default();
    config
        .keybinds
        .insert("input_undo".to_owned(), BindingValue::parse("alt+u"));
    let resolved = config
        .resolve(ResolveOptions {
            terminal_suspend: false,
        })
        .expect("resolve succeeds");
    let keymap = Keymap::from_config(&resolved).expect("keymap builds");
    assert_eq!(keymap.sequences("input_undo"), vec!["alt+u".to_owned()]);
}

#[test]
fn the_object_form_of_a_binding_carries_prevent_default() {
    let paste = definition("input_paste").expect("`input_paste` is in the table");
    assert_eq!(paste.prevent_default, Some(false));

    let mut keymap = keymap_with(&[(
        "input_submit",
        BindingValue::Keys(vec![BindingItem {
            key: "ctrl+alt+return".to_owned(),
            prevent_default: Some(true),
        }]),
    )]);
    let start = Instant::now();
    match keymap.resolve(
        &["input"],
        Chord::parse("ctrl+alt+return").expect("parses"),
        start,
    ) {
        Resolution::Action {
            definition,
            prevent_default,
        } => {
            assert_eq!(definition.name, "input_submit");
            assert_eq!(prevent_default, Some(true));
        }
        other => panic!("the override should resolve: {other:?}"),
    }
}

/// The key event a default terminal delivers for `chord`, or `None` when unmodelled.
///
/// Not the naive inverse of [`Chord::from_key_event`], and that distinction is the whole
/// value of this helper. A terminal in its default encoding does not report shift-tab as Tab
/// with a shift flag: it sends the legacy `CSI Z`, which crossterm surfaces as
/// [`KeyCode::BackTab`] — with `SHIFT` set, because the sequence implies it. So the spelling
/// `shift+tab` describes a chord the table can parse and a terminal will never send. Folding
/// that here is what makes the guard below able to fail; an inverse that simply mirrored the
/// parser would agree with every spelling by construction, including the broken one.
///
/// The Kitty keyboard protocol does disambiguate and report `Tab` with `SHIFT`, which is why
/// the guard asks only that *one* of an action's spellings survive this fold rather than all
/// of them: a table may carry the modern spelling as well, and should.
fn legacy_event_for(chord: &Chord) -> Option<KeyEvent> {
    let rendered = chord.to_string();
    let mut modifiers = crossterm::event::KeyModifiers::NONE;
    for (token, flag) in [
        ("ctrl+", crossterm::event::KeyModifiers::CONTROL),
        ("alt+", crossterm::event::KeyModifiers::ALT),
        ("shift+", crossterm::event::KeyModifiers::SHIFT),
        ("super+", crossterm::event::KeyModifiers::SUPER),
        ("hyper+", crossterm::event::KeyModifiers::HYPER),
        ("meta+", crossterm::event::KeyModifiers::META),
    ] {
        if rendered.contains(token) {
            modifiers |= flag;
        }
    }
    let last = rendered.rsplit('+').next().unwrap_or_default();
    let mut code = match last {
        "return" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        other if other.starts_with('f') && other.len() > 1 => KeyCode::F(other[1..].parse().ok()?),
        other => {
            let mut characters = other.chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) => KeyCode::Char(character),
                _ => return None,
            }
        }
    };
    if code == KeyCode::Tab && modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
        code = KeyCode::BackTab;
    }
    Some(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

#[test]
fn every_bound_action_has_a_spelling_a_default_terminal_can_actually_send() {
    // A spelling no terminal produces is a binding that resolves to `Unmatched` forever, and
    // nothing downstream can rescue it — which makes it the quietest member of the "built but
    // unreachable" family: the action exists, the scope is registered, the arm is written, and
    // the key still does nothing. `agent_cycle_reverse` shipped exactly so, spelled
    // `shift+tab`, against a terminal that sends `BackTab`.
    //
    // Nothing else in this file could see it. The fixture comparison checks this table against
    // upstream's *spellings*, so a spelling wrong on both sides agrees; the parser tests check
    // that a spelling parses, which this one does. The missing property is the round trip
    // through the event a terminal actually sends, and it is asserted per *action* rather than
    // per spelling so a table may also carry the Kitty-protocol form.
    let keymap = Keymap::defaults().expect("the shipped table builds");
    let mut unsendable = Vec::new();
    let mut checked = 0usize;
    for definition in DEFINITIONS {
        let spellings = keymap.sequences(definition.name);
        if spellings.is_empty() {
            continue;
        }
        let mut reports = Vec::new();
        let mut sendable = false;
        for spelling in &spellings {
            let Ok(sequence) = parse_sequence(spelling, keymap.leader()) else {
                continue;
            };
            let mut whole = true;
            for chord in sequence {
                let Some(event) = legacy_event_for(&chord) else {
                    continue;
                };
                checked += 1;
                let observed = Chord::from_key_event(&event);
                if observed != Some(chord) {
                    whole = false;
                    reports.push(format!(
                        "`{spelling}`'s chord `{chord}` arrives as {}",
                        observed.map_or_else(
                            || String::from("an unmodelled key"),
                            |chord| format!("`{chord}`")
                        )
                    ));
                }
            }
            if whole {
                sendable = true;
            }
        }
        if !sendable {
            unsendable.push(format!("{}: {}", definition.name, reports.join("; ")));
        }
    }
    assert!(
        checked >= 200,
        "the round-trip scan checked only {checked} chords, so it is not reaching the table"
    );
    assert!(
        unsendable.is_empty(),
        "these actions have no spelling a default terminal can send, so they can never \
         resolve:\n{}",
        unsendable.join("\n")
    );
}
