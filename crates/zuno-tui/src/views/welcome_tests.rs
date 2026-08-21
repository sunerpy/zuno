//! The welcome screen's two promises: it fills the frame, and its keys are real.

use super::*;
use crate::app::render_offscreen;
use crate::keybind::{ActionComponent as _, Keymap};
use crate::views::testkit::rows;
use crossterm::event::KeyCode;

fn keymap() -> Keymap {
    Keymap::defaults().expect("the shipped binding table resolves")
}

/// The agent and model this screen must **not** state, and the strip must.
const STRIP_AGENT: &str = "build";
const STRIP_MODEL: &str = "myopenai/claude-haiku-4-5";

/// The one row `WelcomeView::lines` emits unconditionally, used to bound the block.
const LEAD_LINE: &str = "type / for commands";

fn facts() -> WelcomeFacts {
    WelcomeFacts {
        directory: Some(String::from("~/src/zuno")),
        branch: Some(String::from("task-r17-solo")),
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

/// The whole surface as its owner composes it: the head above, the foot at the bottom.
///
/// [`Component::render`] draws the head alone, because the foot belongs on the far side of two
/// rows the session owns — the status strip and the prompt band. A test that rendered only the
/// head would therefore assert against half a screen and would call the lead line, the tip and
/// every hint "absent", so this makes the two calls the owner makes.
///
/// The strip and the band are *not* stood in for. Their heights belong to
/// [`crate::views::session::SessionScreen`] and are asserted there; what this fixture owes is
/// that every row the welcome surface states reaches cells, on the side of the input it belongs
/// to. The foot is bottom-anchored for the same reason the owner puts it in the last band: it
/// is what makes "below the input" checkable without this file knowing how tall the input is.
fn screen_buffer(view: &mut WelcomeView, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let foot = view.foot_rows(width).min(height);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("the test backend is infallible");
    terminal
        .draw(|frame| {
            let area = frame.area();
            let head = Rect {
                height: area.height - foot,
                ..area
            };
            let tail = Rect {
                y: head.height,
                height: foot,
                ..area
            };
            view.render(frame, head);
            view.render_foot(frame, tail);
        })
        .expect("the test backend is infallible");
    terminal.backend().buffer().clone()
}

/// How many of `height` rows carry at least one non-space character.
fn painted(view: &mut WelcomeView, width: u16, height: u16) -> usize {
    rows(&screen_buffer(view, width, height))
        .iter()
        .filter(|row| !row.trim().is_empty())
        .count()
}

/// Rows from the block's first painted row to its last, blank spacers included.
///
/// The measure a reader of the screen actually experiences: a spacer costs a row of
/// height exactly as much as a sentence does, so counting only painted rows would call a
/// block double-spaced into twice the height "the same size".
fn extent(view: &mut WelcomeView, width: u16, height: u16) -> usize {
    let rendered = rows(&screen_buffer(view, width, height));
    let painted = |row: &String| !row.trim().is_empty();
    let first = rendered.iter().position(painted);
    let last = rendered.iter().rposition(painted);
    match (first, last) {
        (Some(first), Some(last)) => last - first + 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// The defect this module exists to fix
// ---------------------------------------------------------------------------

#[test]
fn views_welcome_fills_a_large_frame_without_sprawling_across_it() {
    // Two bounds, because this screen has been wrong in both directions. The floor is the
    // original defect: two non-empty rows out of fifty, indistinguishable from a rendering
    // failure. The ceiling is the second one: twenty-two rows of vertical extent, four of
    // them restating what the strip and the sidebar already carried.
    //
    // A band rather than an equality so that editing a tip or a slash label cannot fail
    // this, and wide enough at the bottom that a screen which regressed to a couple of rows
    // still cannot pass. The ceiling is on *extent* rather than on painted rows because a
    // blank spacer occupies the screen exactly as much as a row of text does, and spacers
    // are how the previous version spent a third of its height.
    //
    // The ceiling moved 18 -> 14, and the reason is not editorial. Every row above the input
    // is a row the input sits further from the middle — the eighteen-row block was inside the
    // reference band and still produced the top-heavy screen that was reported twice. 14 is
    // what the current composition costs exactly, so this is a ratchet: any new row has to be
    // paid for by retiring one, and there is no slack left to grow into unnoticed.
    //
    // # It is measured as head + foot, not as extent, because the two halves are separated
    //
    // The surface now spans the input: `head_rows` above it and `foot_rows` below. Extent on a
    // composed frame therefore measures the *frame*, not the screen — the two halves are at
    // opposite ends of it — so an extent ceiling would pass on any composition whatsoever and
    // this ratchet would silently stop ratcheting. That is the "compared against nothing"
    // shape this crate has already been bitten by twice, so the total is taken from the two
    // functions that produce the rows.
    //
    // The head is bounded on its own as well, and more tightly. It is the half that has to fit
    // in the space above the band on a 24-row pane, which is the whole reason for the split:
    // rows in the foot cost the centring nothing, rows in the head cost it directly.
    let mut view = view();
    let painted_rows = painted(&mut view, 200, 50);
    assert!(
        painted_rows >= 10,
        "the welcome screen painted only {painted_rows} of 50 rows, which is the emptiness \
         it exists to replace"
    );

    let head = view.head_rows(200, 50);
    let foot = view.foot_rows(200);
    assert!(
        head + foot <= 14,
        "the welcome surface states {head} rows above the input and {foot} below, {} in all; \
         it spanned 22, then 18, and the references it follows spend 10 (`jcode`), \
         15 (`codex`) and 17 (`claw-code`)",
        head + foot
    );
    assert!(
        head <= 9,
        "the welcome screen states {head} rows above the input; past nine the input cannot \
         reach the middle of a 24-row pane, which is what `welcome_tail_rows` centres. Move \
         the row into `foot` rather than raising this"
    );

    // And every row the two halves state reaches cells, so the bounds above are on a screen
    // rather than on two vectors. Extent is the right measure for *that*: on a frame this tall
    // the head is bottom-anchored and the foot is at the very bottom, so the span between them
    // is the frame minus the rows above the head.
    let extent = extent(&mut view, 200, 50);
    assert!(
        extent >= usize::from(head + foot),
        "the surface claims {head} + {foot} rows but only {extent} rows of the frame carry \
         anything, so some of them were never painted"
    );
}

#[test]
fn views_welcome_hides_the_tip_row_until_it_is_asked_for() {
    // The cut, asserted from the shipped composition rather than from the constant. The tip
    // was the one block here carrying neither a fact nor a key, and the two rows it spent are
    // two rows the input sat further from the frame's middle.
    //
    // Hidden, not deleted: `tips_toggle` is a real binding in the shipped table, and a bound
    // key that reaches nothing is the defect class this whole surface exists to remove. So the
    // negative and the positive are one test — the row is absent by default *and* one call to
    // the thing the key calls brings it back.
    let mut view = view();
    assert!(
        !view.tips_visible(),
        "the shipped composition still shows the tip row"
    );
    let shipped = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    assert!(
        !shipped.contains("● tip"),
        "the tip row is drawn on a screen that reports it hidden:\n{shipped}"
    );

    view.next_tip();
    let asked = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    assert!(
        asked.contains("● tip"),
        "`tips_toggle` cannot bring the row back, so the binding reaches nothing:\n{asked}"
    );
    assert!(
        asked.contains(view.tip()),
        "the row came back without the tip it names:\n{asked}"
    );
}

#[test]
fn views_welcome_degrades_to_a_compact_brand_when_the_wordmark_cannot_fit() {
    // 30 columns cannot carry a 36-column wordmark, and 14 rows cannot carry six rows of
    // it plus the facts. Either alone is enough to fall back.
    let mut view = view();
    for (width, height) in [(30, 40), (200, 14)] {
        let narrow = rows(&screen_buffer(&mut view, width, height)).join("\n");
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
    for row in rows(&screen_buffer(&mut view, 80, 24)) {
        assert!(
            row.chars().count() <= 80,
            "a row overflowed 80 columns: {row:?}"
        );
    }
    let joined = rows(&screen_buffer(&mut view, 80, 24)).join("\n");
    assert!(
        joined.contains("~/src/zuno"),
        "80 columns lost the location row:\n{joined}"
    );
}

#[test]
fn views_welcome_draws_the_wordmark_when_it_fits() {
    let mut view = view();
    let wide = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
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
    let buffer = screen_buffer(&mut view, 200, 50);
    let brand = ratatui::style::Color::from(context.palette().primary);
    let shadow = ratatui::style::Color::from(crate::theme::tint(
        context.palette().background_panel,
        context.palette().primary,
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
fn views_welcome_states_every_fact_no_other_surface_keeps_at_every_width() {
    // The positive half of the trim. The list is shorter than it was by exactly the agent
    // and the model, which the negative test below now forbids; everything still here is a
    // fact that has *no* other carrier at some supported width — the sidebar vanishes below
    // `SIDEBAR_MIN_WIDTH`, and the strip drops the branch before anything else.
    let mut view = view();
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    for needle in [
        "~/src/zuno",
        "task-r17-solo",
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
fn views_welcome_does_not_restate_the_agent_and_model_the_status_strip_carries() {
    // The measured duplication this trim removes: at 120x34 the welcome screen's own row
    // read `build   ·   myopenai/claude-haiku-4-5` while the strip two rows below read
    // `build · myopenai/claude-haiku-4-5 · idle`.
    //
    // Asserted on the **composite**, not on `WelcomeView` alone, and that is the point. A
    // welcome-only assertion would pass the moment the row was deleted here even if a host
    // re-added it, and it could not show that the fact is still on screen. Both halves are
    // required together: absent from the welcome region, present on the strip.
    let mut screen = composed();
    let rendered = rows(&render_offscreen(&mut screen, 120, 34).expect("infallible"));
    let joined = rendered.join("\n");
    // Bracketed by both halves of the welcome surface rather than by one, which is stronger
    // than the version this replaces and is not a matter of taste: the surface now sits on
    // **both** sides of the strip — the census above, the lead line below — so a one-sided
    // "below the lead line" check would place the strip beneath the whole screen and hold for
    // rows that are on the welcome surface's own upper half. Two anchors pin the occurrence
    // into the strip-and-band region exactly.
    //
    // The strip's own words cannot be the anchor: it degrades through four tiers and at 40
    // columns prints `idl` rather than `idle`. Both anchors here are unconditional at this
    // width — the census is stated whenever any fact is known, and the lead line is the one
    // row `WelcomeView::foot` always emits.
    let census = rendered
        .iter()
        .position(|row| row.contains("zuno 0.1.0"))
        .expect("the welcome screen states the census whenever it knows a fact");
    let lead = rendered
        .iter()
        .position(|row| row.contains(LEAD_LINE))
        .expect("the welcome screen always teaches `/`");
    assert!(
        census < lead,
        "the census is meant to be above the input and the lead line below it, but they came \
         back in rows {census} and {lead}:\n{joined}"
    );

    for needle in [STRIP_AGENT, STRIP_MODEL] {
        let rows_with = rendered
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains(needle))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(
            rows_with.len(),
            1,
            "`{needle}` is stated on {} rows; it belongs on the status strip alone, which \
             is the one row never dropped at any width:\n{joined}",
            rows_with.len()
        );
        assert!(
            rows_with[0] > census && rows_with[0] < lead,
            "`{needle}` is stated on row {}, outside the strip-and-band region the welcome \
             surface brackets (census row {census}, lead line row {lead}), so the one \
             remaining copy is the welcome screen's own:\n{joined}",
            rows_with[0]
        );
    }
}

#[test]
fn views_welcome_keeps_the_branch_because_both_other_carriers_drop_it() {
    // The asymmetry that decides which duplicates may go. The agent and model are cut
    // because `StatusView::state` is padded or clipped but never omitted; the branch is
    // kept because it lives in `StatusView::trailers`, which is dropped front-first, and in
    // the sidebar, which is not drawn below `SIDEBAR_MIN_WIDTH`. Forty columns is where
    // both of those give out at once, so it is the width that proves the branch needs a
    // carrier here.
    let mut screen = composed();
    let rendered = rows(&render_offscreen(&mut screen, 40, 24).expect("infallible"));
    let joined = rendered.join("\n");
    let carriers = rendered
        .iter()
        .filter(|row| row.contains("task-r17-solo"))
        .collect::<Vec<_>>();
    assert_eq!(
        carriers.len(),
        1,
        "at 40 columns the branch should have exactly one carrier — more means the strip \
         did not drop its trailer and this test proves nothing, none means the trim took \
         the last one:\n{joined}"
    );
    // The surviving carrier has to be the welcome screen's location row, which is the row
    // that also names the directory. Counting alone would be satisfied by the strip keeping
    // it, which is the case this test exists to distinguish.
    assert!(
        carriers[0].contains("~/src/zuno"),
        "the branch survived on some row other than the welcome location row:\n{joined}"
    );
}

#[test]
fn views_welcome_omits_a_fact_it_does_not_have_rather_than_inventing_one() {
    let mut view = WelcomeView::new(ViewContext::defaults());
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
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
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    assert!(
        joined.contains("0 mcp") && joined.contains("0 skills"),
        "a zero count was dropped, which reads as `not measured`:\n{joined}"
    );
}

#[test]
fn views_welcome_every_hint_names_an_action_the_shipped_table_has() {
    // The guard against a dead hint: a typo'd action name would render no key at all,
    // and the row would silently vanish from the grid.
    for (action, label) in KEY_HINTS {
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
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    assert!(
        joined.contains("ctrl+j send"),
        "the grid advertised the default spelling after the user rebound it:\n{joined}"
    );
}

#[test]
fn views_welcome_leads_with_slash_commands_and_the_users_palette_binding() {
    use crate::config::BindingValue;
    let mut config = crate::config::ResolvedTuiConfig::default();
    config
        .keybinds
        .insert(String::from("command_list"), BindingValue::parse("ctrl+g"));
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);
    let context = ViewContext::new(&resolved, config);
    let mut view = WelcomeView::new(context).with_facts(facts());
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");

    assert!(joined.contains("type / for commands"), "{joined}");
    assert!(joined.contains("ctrl+g command palette"), "{joined}");
    assert!(!joined.contains("ctrl+p command palette"), "{joined}");
}

#[test]
fn views_welcome_advertises_its_capabilities_as_slash_commands_at_every_supported_width() {
    // The measured complaint, asserted: the grid used to say `<leader>m models`, and
    // nothing on the screen defined `<leader>`. Each capability is now spelled the way the
    // user types it, and the *whole* row is required at each width — a substring check on
    // the name alone would pass on a row the frame had clipped mid-label.
    let mut view = view();
    for (width, height) in [(120u16, 40u16), (80, 30), (60, 26)] {
        let joined = rows(&screen_buffer(&mut view, width, height)).join("\n");
        assert!(
            joined.contains("type / for commands"),
            "{width}x{height} does not teach `/` at all:\n{joined}"
        );
        for (name, label) in SLASH_HINTS {
            assert!(
                joined.contains(&format!("/{name} {label}")),
                "`/{name} {label}` is missing or clipped at {width}x{height}:\n{joined}"
            );
        }
    }
}

#[test]
fn views_welcome_advertises_three_commands_and_not_the_six_it_used_to() {
    // The cut, named. The extent ratchet would also fail if these came back, but it would
    // report "the block spans 15 rows" — true, and no help at all in finding out why. This
    // names the rows instead.
    //
    // The three that went are settings a user goes looking for *after* they know `/` exists,
    // which the row above has just told them; `/` lists all of them and the palette chord lists
    // every binding. So the requirement is not "these strings are absent" — it is that the path
    // to them is still on the screen, which is why the lead row is asserted in the same test.
    let mut view = view();
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    for retired in ["/agent", "/theme", "/mcp"] {
        assert!(
            !joined.contains(retired),
            "`{retired}` is advertised again; the grid is back to teaching a list `/` opens \
             in one keystroke:\n{joined}"
        );
    }
    assert!(
        joined.contains("type / for commands"),
        "the retired rows were cut without leaving the path that replaces them:\n{joined}"
    );
    assert_eq!(
        SLASH_HINTS.len(),
        3,
        "the grid grew back; every row here is a row the input sits further from the middle"
    );
}

#[test]
fn views_welcome_never_renders_the_literal_leader_token() {
    // `<leader>` is `ctrl+x` and nothing on this screen says so, which is what made the
    // earlier grid undecodable. Asserted across the widths the grid reflows at, because a
    // column count change is what decides which rows are drawn at all.
    //
    // Two configurations, not one. Under the shipped defaults no remaining hint is bound to
    // a leader sequence, so the default render alone would keep passing even if the
    // resolution were removed — measured, by reverting `spelling` to `key_label` and
    // watching this test stay green while the two rebind tests failed. The second view
    // binds a hint to a leader sequence, which is the only shape that can produce the token.
    use crate::config::BindingValue;
    let mut config = crate::config::ResolvedTuiConfig::default();
    config.keybinds.insert(
        String::from("input_submit"),
        BindingValue::parse("<leader>j"),
    );
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);

    for mut screen in [
        view(),
        WelcomeView::new(ViewContext::new(&resolved, config)).with_facts(facts()),
    ] {
        for (width, height) in [(200u16, 50u16), (120, 40), (80, 30), (60, 26)] {
            for row in rows(&render_offscreen(&mut screen, width, height).expect("infallible")) {
                assert!(
                    !row.contains(crate::keybind::LEADER_TOKEN),
                    "a row at {width}x{height} shows the literal leader token, which a user \
                     cannot decode: {row:?}"
                );
            }
        }
    }
}

#[test]
fn views_welcome_resolves_a_leader_sequence_to_the_chords_actually_pressed() {
    // The half of the fix that the curated hint list alone does not buy. A user is free to
    // bind a hinted action to a leader sequence, and `views::key_label` would hand the raw
    // `<leader>j` straight to the screen — so the spelling is taken from a resolved
    // `Keymap`, which substitutes the leader chord the way a real keypress resolves it.
    use crate::config::BindingValue;
    let mut config = crate::config::ResolvedTuiConfig::default();
    config.keybinds.insert(
        String::from("input_submit"),
        BindingValue::parse("<leader>j"),
    );
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);
    let mut view = WelcomeView::new(ViewContext::new(&resolved, config)).with_facts(facts());
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");

    assert!(
        joined.contains("ctrl+x j send"),
        "the leader sequence was not resolved to the chords a user presses:\n{joined}"
    );
    assert!(
        !joined.contains(crate::keybind::LEADER_TOKEN),
        "the raw leader token reached the screen:\n{joined}"
    );
}

#[test]
fn views_welcome_follows_the_users_own_leader_chord_rather_than_assuming_one() {
    // `ctrl+x` is not a fact about this program, it is a default. A spelling hard-coded
    // against it would read correctly today and lie the moment the leader was rebound —
    // which is exactly how a hard-coded exit key on the status strip went stale in this
    // project once overrides became real.
    use crate::config::BindingValue;
    let mut config = crate::config::ResolvedTuiConfig::default();
    // `ctrl+y` because the shipped table claims no binding for it. `ctrl+b` is
    // `session_background`, and a leader colliding with a real binding makes the keymap
    // refuse to build — which this screen degrades from rather than fails on, so the test
    // would have reported a missing row instead of a wrong chord.
    config
        .keybinds
        .insert(String::from("leader"), BindingValue::parse("ctrl+y"));
    config.keybinds.insert(
        String::from("input_submit"),
        BindingValue::parse("<leader>j"),
    );
    let registry = crate::theme::ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark);
    let mut view = WelcomeView::new(ViewContext::new(&resolved, config)).with_facts(facts());
    assert!(
        view.keymap().is_some(),
        "the override no longer builds a keymap, so this asserts the fallback path instead \
         of the leader substitution it was written for"
    );
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");

    assert!(
        joined.contains("ctrl+y j send"),
        "the grid ignored the user's own leader chord:\n{joined}"
    );
    assert!(
        !joined.contains("ctrl+x j"),
        "the grid advertised the default leader chord after it was rebound:\n{joined}"
    );
}

#[test]
fn views_welcome_spells_out_the_exit_key_down_to_sixty_columns() {
    // The regression the two-group layout was shaped to fix. The single eleven-entry grid
    // filled its column budget with leader rows and dropped `cancel / exit` entirely below
    // eighty columns — the one hint a stuck user needs, missing at the width most likely to
    // belong to somebody with a small window.
    let mut view = view();
    for (width, height) in [(120u16, 40u16), (80, 30), (60, 26)] {
        let joined = rows(&screen_buffer(&mut view, width, height)).join("\n");
        for (action, label) in KEY_HINTS {
            let spelling = view
                .spelling(view.keymap().as_ref(), action)
                .unwrap_or_else(|| panic!("`{action}` resolved to no spelling"));
            assert!(
                joined.contains(&format!("{spelling} {label}")),
                "`{spelling} {label}` is missing at {width}x{height}, so a key with no \
                 slash spelling is advertised nowhere:\n{joined}"
            );
        }
    }
}

#[test]
fn views_welcome_every_slash_hint_resolves_through_the_real_router() {
    // `advertised_actions` panics on a name the router does not route to a UI action, so
    // calling it is the assertion. The count is pinned as well, because a helper that
    // silently returned an empty set would also never panic.
    let advertised = advertised_actions();
    assert_eq!(
        advertised.len(),
        KEY_HINTS.len() + SLASH_HINTS.len(),
        "the advertised set lost an entry: {advertised:?}"
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
    // The same three claims as before — next advances, hide removes the row, next after hide
    // restores it — reordered for the composition that now starts hidden. The reorder is the
    // point: the first `next_tip` on a hidden row must *reveal* rather than advance, or a user
    // who pressed the key would silently skip a tip they never saw. That is the assertion the
    // old ordering could not make, because the row was already visible when it started.
    let mut view = view();
    assert!(!view.tips_visible(), "the shipped default is a hidden row");
    let first = view.tip();
    view.next_tip();
    assert!(view.tips_visible(), "the row did not come back");
    assert_eq!(
        view.tip(),
        first,
        "revealing the row also advanced it, so the first tip a user ever sees is the second one"
    );

    view.next_tip();
    assert_ne!(view.tip(), first, "the tip did not change on a visible row");

    view.hide_tips();
    assert!(!view.tips_visible());
    let hidden = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    assert!(
        !hidden.contains("● tip"),
        "the tip row is still drawn after being hidden:\n{hidden}"
    );
}

#[test]
fn views_welcome_tip_index_wraps_instead_of_panicking() {
    // `TIPS[self.tip % TIPS.len()]` on `usize::MAX`, and then on the wrap past it. The second
    // `next_tip` is what advances: the first only reveals the row, so a single call would leave
    // this asserting the same index twice and the wrapping arithmetic untested.
    let mut view = WelcomeView::new(ViewContext::defaults()).with_tip(usize::MAX);
    assert!(TIPS.contains(&view.tip()));
    view.next_tip();
    let revealed = view.tip();
    view.next_tip();
    assert!(TIPS.contains(&view.tip()));
    assert_ne!(
        view.tip(),
        revealed,
        "the index did not advance past usize::MAX"
    );
}

#[test]
fn views_welcome_facts_can_be_stated_after_construction() {
    // The carrier changed with the trim — `model` is no longer a field, because the status
    // strip states it — but the property is unchanged and still worth a test: a host that
    // resolves a fact *after* constructing the screen must see it rendered. `version` is
    // used because it is resolved on the same late path `model` was.
    let mut view = WelcomeView::new(ViewContext::defaults());
    view.facts_mut().version = Some(String::from("9.9.9-probe"));
    let joined = rows(&screen_buffer(&mut view, 200, 50)).join("\n");
    assert!(joined.contains("zuno 9.9.9-probe"), "{joined}");
}

#[test]
fn views_welcome_renders_into_a_degenerate_area_without_panicking() {
    // `20x10` is in the list because it is the frame where the hint block's row budget goes
    // negative: the brand, facts, lead line and tip already exceed ten rows, so the budget
    // arithmetic that splits rows between the two groups has to reach zero by saturating
    // rather than by wrapping.
    let mut view = view();
    for (width, height) in [(0, 0), (1, 1), (200, 1), (1, 50), (36, 20), (20, 10)] {
        let _ = screen_buffer(&mut view, width, height);
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
    for (action, label) in advertised_actions() {
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
                    reasoning: false,
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
    //
    // `/help` is still advertised, so the action behind its row has to open something. `/mcp`
    // is not, and it stays here for the opposite reason: the trim removed a *row*, not a
    // capability. `mcp_list` is still bound and `/mcp` still resolves, and a later change that
    // broke either while nothing advertised them would be invisible — which is why the
    // no-longer-advertised half is the half worth keeping under a guard.
    let (sender, _receiver) = crate::app::terminal_event_channel();
    let (mcp_toggles, _mcp_requests) = tokio::sync::mpsc::channel(1);
    let mut screen = crate::views::session::SessionScreen::new(ViewContext::defaults(), sender)
        .with_mcp_control(
            crate::views::picker::McpProjection::new(vec![crate::views::picker::McpServer {
                name: "alpha".to_owned(),
                state: crate::views::picker::McpState::Failed("handshake timed out".to_owned()),
                desired_enabled: true,
            }]),
            mcp_toggles,
        )
        .with_keymap(keymap());

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
    for (action, label) in KEY_HINTS {
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
    for (action, label) in KEY_HINTS {
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

/// Every action this screen advertises, paired with how the screen spells it.
///
/// The two hint lists reach an action by different routes — [`KEY_HINTS`] names it
/// outright, [`SLASH_HINTS`] names a command the real router resolves to one — so the
/// routing guard takes its set from here rather than from either list alone. A slash row
/// that reached nothing would be precisely the defect the key guards were written for,
/// wearing a different spelling.
///
/// Resolving through [`crate::views::slash::SlashRouter`] rather than deriving the action
/// name locally is the point: it is the same object the prompt submits through, so a
/// renamed command, a typo, and a name the router deliberately excludes from the slash
/// surface all fail here instead of rendering as an inert row.
fn advertised_actions() -> Vec<(&'static str, String)> {
    use crate::views::slash::{SlashRouter, SlashSubmission};
    let router = SlashRouter::default();
    let mut actions = KEY_HINTS
        .iter()
        .map(|(action, label)| (*action, (*label).to_owned()))
        .collect::<Vec<_>>();
    for (name, label) in SLASH_HINTS {
        match router.resolve(&format!("/{name}")) {
            SlashSubmission::UiAction(action) => {
                actions.push((action, format!("/{name} {label}")));
            }
            other => panic!(
                "the welcome grid advertises `/{name}` ({label}), which the slash router \
                 resolves to {other:?} rather than to a UI action"
            ),
        }
    }
    actions
}

// ---------------------------------------------------------------------------
// Printers
// ---------------------------------------------------------------------------

/// The whole composed screen, so duplication *between* surfaces is visible.
///
/// A welcome-only printer cannot answer the question this screen is judged on. The
/// complaint is that rows restate what the status strip and the sidebar already carry
/// permanently, and neither of those is part of [`WelcomeView`] — so the only rendering
/// that can show a repeat is the composite, at the widths where the sidebar appears and
/// disappears.
#[test]
#[ignore = "printer, not an assertion: run with --ignored --nocapture to eyeball the rendering"]
fn views_welcome_visual_probe() {
    for (width, height) in [
        (200u16, 50u16),
        (120, 32),
        (80, 24),
        (60, 30),
        (40, 24),
        (20, 10),
    ] {
        println!("\n=========== {width}x{height} ===========");
        let mut screen = composed();
        for (index, row) in rows(&render_offscreen(&mut screen, width, height).expect("infallible"))
            .iter()
            .enumerate()
        {
            println!("{:>2}|{}|", index, row.trim_end());
        }
    }
}

/// A session screen dressed the way a host dresses it on the first frame.
///
/// The facts are stated on all three surfaces — welcome, strip, sidebar — because that is
/// what the host does, and a probe that fed only the welcome screen would show no
/// duplication however much of it there was.
fn composed() -> crate::views::session::SessionScreen {
    let (sender, receiver) = crate::app::terminal_event_channel();
    std::mem::forget(receiver);
    let mut screen = crate::views::session::SessionScreen::new(ViewContext::defaults(), sender)
        .with_keymap(keymap());
    *screen.welcome_mut().facts_mut() = facts();
    screen.status_mut().set_configured_agent("build");
    screen
        .status_mut()
        .set_configured_model("myopenai/claude-haiku-4-5");
    screen.status_mut().set_git_branch("task-r17-solo");
    let ambient = screen.sidebar_mut().ambient_mut();
    ambient.directory = Some(String::from("~/src/zuno"));
    ambient.branch = Some(String::from("task-r17-solo"));
    ambient.agent = Some(String::from("build"));
    ambient.model = Some(String::from("myopenai/claude-haiku-4-5"));
    ambient.version = Some(String::from("0.1.0"));
    ambient.mcp = vec![crate::views::ambient::Service::new(
        "alpha",
        crate::views::ambient::Health::Ready,
    )];
    screen
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
                reasoning: false,
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
