//! The welcome screen's two promises: it fills the frame, and its keys are real.

use super::*;
use crate::app::render_offscreen;
use crate::views::testkit::rows;

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
