//! The ambient panel's promise: it states what is running, and says so honestly.

use super::*;
use crate::app::render_offscreen;
use crate::views::message::TokenUsage;
use crate::views::testkit::rows;

fn ambient() -> Ambient {
    Ambient {
        directory: Some(String::from("~/src/zuno")),
        branch: Some(String::from("task-r17-solo")),
        agent: Some(String::from("build")),
        model: Some(String::from("myopenai/claude-haiku-4-5")),
        tokens: TokenUsage {
            input: 12_000,
            output: 3_400,
            cache_read: 800,
            cache_write: 200,
        },
        context_used: Some(64),
        lsp: vec![
            Service::new("rust-analyzer", Health::Ready).detailed("/config/workspace/zuno"),
            Service::new("typescript-language-server", Health::Pending).detailed("starting"),
        ],
        mcp: vec![
            Service::new("alpha", Health::Ready).detailed("connected"),
            Service::new("beta", Health::Faulted).detailed("handshake timed out"),
        ],
        skills: vec![
            SkillSummary {
                name: String::from("commit-msg"),
                description: String::from("conventional commits"),
            },
            SkillSummary {
                name: String::from("codegraph"),
                description: String::from("code navigation"),
            },
        ],
        version: Some(String::from("0.1.0")),
    }
}

fn view() -> SidebarView {
    let mut view = SidebarView::new(ViewContext::defaults());
    *view.ambient_mut() = ambient();
    view
}

fn drawn(view: &mut SidebarView) -> String {
    rows(&render_offscreen(view, SIDEBAR_WIDTH, 40).expect("infallible")).join("\n")
}

// ---------------------------------------------------------------------------
// The facts
// ---------------------------------------------------------------------------

#[test]
fn views_sidebar_states_tokens_servers_and_skills() {
    let mut view = view();
    let joined = drawn(&mut view);
    for needle in [
        "Context",
        "16,400",
        "LSP",
        "rust-analyzer",
        "MCP",
        "alpha",
        "Skills",
    ] {
        assert!(joined.contains(needle), "`{needle}` is missing:\n{joined}");
    }
}

#[test]
fn views_sidebar_reports_a_failed_server_in_its_section_summary() {
    // The summary is what a user scanning for trouble reads, so it must count the
    // failure rather than only colouring the row that scrolled out of view.
    let mut view = view();
    let joined = drawn(&mut view);
    assert!(
        joined.contains("1 up, 1 failed"),
        "the MCP heading does not report the failure:\n{joined}"
    );
}

#[test]
fn views_sidebar_distinguishes_empty_from_broken() {
    // "No servers configured" and "servers failed" must never render the same, which is
    // the "no results versus cannot see the data" confusion.
    let mut view = SidebarView::new(ViewContext::defaults());
    let empty = drawn(&mut view);
    assert!(
        empty.contains("none configured"),
        "an unconfigured MCP section says nothing at all:\n{empty}"
    );
    // Was "starts as files are read", which is false for an empty list: with no server
    // enabled nothing will ever start no matter what is read, and that copy made "no `lsp`
    // key at all" indistinguishable from "configured and merely idle".
    assert!(
        empty.contains(SidebarView::NO_LSP_CONFIGURED.trim()),
        "an empty LSP section does not explain itself:\n{empty}"
    );
    assert!(
        crate::views::display_width(SidebarView::NO_LSP_CONFIGURED) < usize::from(SIDEBAR_WIDTH),
        "the explanation is wider than the panel, so it renders cut off mid-word"
    );
    assert!(
        !empty.contains("starts as files are read"),
        "an empty section promised a start that will never happen:\n{empty}"
    );
    assert!(
        !empty.contains("failed"),
        "an empty panel claimed something failed:\n{empty}"
    );
}

/// The other half of the same distinction: a configured server that *can* start says so,
/// and one that cannot is a fault rather than silence.
#[test]
fn views_sidebar_separates_a_server_that_will_start_from_one_that_cannot() {
    let mut view = SidebarView::new(ViewContext::defaults());
    view.ambient_mut().lsp = vec![
        Service::new(String::from("rust"), Health::Pending)
            .detailed("starts on first matching file"),
        Service::new(String::from("gopls"), Health::Faulted).detailed("gopls not found on PATH"),
    ];
    let drawn = drawn(&mut view);
    assert!(
        !drawn.contains(SidebarView::NO_LSP_CONFIGURED.trim()),
        "two configured servers rendered as none:\n{drawn}"
    );
    // The tails, not the whole detail: a 34-column panel elides the middle of a long
    // detail, so asserting the full sentence would fail on a frame that is in fact correct.
    assert!(drawn.contains("first matching file"), "{drawn}");
    assert!(
        drawn.contains("not found on PATH"),
        "a server that can never start was not distinguished from an idle one:\n{drawn}"
    );
    assert!(
        drawn.contains(Health::Faulted.glyph()) && drawn.contains(Health::Pending.glyph()),
        "the two states share a gutter glyph, so only the elided text tells them apart:\n{drawn}"
    );
}

#[test]
fn views_sidebar_says_so_when_no_usage_has_arrived() {
    let mut view = SidebarView::new(ViewContext::defaults());
    let joined = drawn(&mut view);
    assert!(
        joined.contains("no usage reported yet"),
        "a zero token count rendered as a real measurement:\n{joined}"
    );
    assert!(
        !joined.contains("0 tokens"),
        "an unmeasured session claimed zero tokens, which is a different statement:\n{joined}"
    );
}

#[test]
fn views_sidebar_warns_once_the_context_window_is_nearly_full() {
    let mut view = view();
    view.ambient_mut().context_used = Some(91);
    let buffer = render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible");
    let warning = ratatui::style::Color::from(ViewContext::defaults().palette().warning);
    let row = (0..40)
        .find(|y| {
            (0..SIDEBAR_WIDTH)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("91%")
        })
        .expect("the percentage row is drawn");
    let coloured = (0..SIDEBAR_WIDTH).any(|x| buffer[(x, row)].fg == warning);
    assert!(
        coloured,
        "a nearly-full context window is not visually distinguished"
    );
}

// ---------------------------------------------------------------------------
// Collapsing
// ---------------------------------------------------------------------------

#[test]
fn views_sidebar_sections_collapse_and_report_that_they_are_collapsed() {
    let mut view = view();
    assert!(view.expanded().lsp);
    assert!(!view.expanded().skills, "skills default to collapsed");

    let open = drawn(&mut view);
    assert!(open.contains(&format!("{OPEN_GLYPH} LSP")), "{open}");
    assert!(open.contains("rust-analyzer"));

    view.toggle_lsp();
    let closed = drawn(&mut view);
    assert!(
        closed.contains(&format!("{CLOSED_GLYPH} LSP")),
        "the closed section does not show the closed glyph:\n{closed}"
    );
    assert!(
        !closed.contains("rust-analyzer"),
        "a collapsed section still drew its rows:\n{closed}"
    );
    assert!(
        closed.contains("1/2"),
        "a collapsed section must still carry its count:\n{closed}"
    );
}

#[test]
fn views_sidebar_skill_names_appear_only_once_expanded() {
    let mut view = view();
    assert!(!drawn(&mut view).contains("commit-msg"));
    view.toggle_skills();
    let joined = drawn(&mut view);
    assert!(joined.contains("commit-msg"), "{joined}");
    assert!(joined.contains("codegraph"), "{joined}");
}

#[test]
fn views_sidebar_toggle_mcp_is_independent_of_the_other_sections() {
    let mut view = view();
    view.toggle_mcp();
    assert!(!view.expanded().mcp);
    assert!(view.expanded().lsp, "toggling MCP closed the LSP section");
}

/// The row each section's heading was drawn on, measured out of the frame.
///
/// Read from the rendered rows rather than from `rows()`'s own header indices, because the
/// indices are what the hit map is built from — locating the row that way would make every
/// assertion below true by construction, including under an implementation that recorded
/// the wrong `y`. The label is the anchor and it is unconditional: `heading` always emits
/// `{glyph} {label}`, so unlike the status strip's state word this text cannot degrade away.
fn heading_rows(view: &mut SidebarView, width: u16, height: u16) -> Vec<(u16, String)> {
    rows(&render_offscreen(view, width, height).expect("infallible"))
        .into_iter()
        .enumerate()
        .filter(|(_, row)| {
            ["LSP", "MCP", "Skills"]
                .iter()
                .any(|label| row.contains(label))
        })
        .map(|(index, row)| {
            (
                u16::try_from(index).expect("a frame is under 65536 rows"),
                row,
            )
        })
        .collect()
}

#[test]
fn views_sidebar_a_click_on_each_section_heading_toggles_that_section_and_only_it() {
    // The defect: the headings have drawn a disclosure triangle since the panel was written
    // and no mouse event reached this file at all, so the one advertised interaction did
    // nothing. `▾` that does not answer a click is worse than no `▾` — it invites a gesture
    // and reports nothing.
    //
    // Every heading in one test, driven off the rows actually painted, because the hazard is
    // per-section: an off-by-one in the recorded `y` toggles the section above or below the
    // one aimed at, which a single-section test on the first heading cannot see.
    let mut view = view();
    let headings = heading_rows(&mut view, SIDEBAR_WIDTH, 40);
    assert_eq!(
        headings.len(),
        3,
        "the frame does not show three section headings, so the coordinates below are \
         guesses: {headings:?}"
    );

    for (row, label) in headings {
        let before = view.expanded();
        assert!(
            view.click(0, row),
            "a click on the {label:?} heading at row {row} was not claimed"
        );
        let after = view.expanded();
        let flipped = [
            (before.lsp != after.lsp, "LSP"),
            (before.mcp != after.mcp, "MCP"),
            (before.skills != after.skills, "Skills"),
        ];
        let changed: Vec<&str> = flipped
            .iter()
            .filter(|(changed, _)| *changed)
            .map(|(_, name)| *name)
            .collect();
        assert_eq!(
            changed.len(),
            1,
            "a click on {label:?} changed {changed:?} rather than exactly one section"
        );
        assert!(
            label.contains(changed[0]),
            "a click on the {label:?} row toggled {} instead, so the recorded rows are off \
             by one section",
            changed[0]
        );
        // Put it back, so the next heading is tested against the layout this frame drew
        // rather than against one a previous collapse shifted upwards.
        view.click(0, row);
    }
}

#[test]
fn views_sidebar_a_click_away_from_a_heading_changes_nothing_and_is_not_claimed() {
    let mut view = view();
    let headings: Vec<u16> = heading_rows(&mut view, SIDEBAR_WIDTH, 40)
        .into_iter()
        .map(|(row, _)| row)
        .collect();
    let before = view.expanded();

    // Every row of the frame that is not a heading, rather than one hand-picked coordinate:
    // a hit map recorded with the wrong height — a `Rect` spanning to the panel's bottom
    // instead of one row — passes any single-point test that happens to miss.
    for row in 0..40 {
        if headings.contains(&row) {
            continue;
        }
        assert!(
            !view.click(0, row),
            "row {row} is not a heading but claimed the click"
        );
    }
    // A column past the panel's right edge, which is where the transcript lives when this
    // panel is drawn beside one.
    assert!(!view.click(SIDEBAR_WIDTH + 5, headings[0]));
    assert_eq!(
        before,
        view.expanded(),
        "a click that hit no heading still changed a section"
    );
}

#[test]
fn views_sidebar_advertises_no_disclosure_triangle_when_the_mouse_is_switched_off() {
    // A click is the only way to actuate a section, so with `mouse = false` the triangle
    // advertises a gesture the build has switched off. It is withdrawn rather than left
    // drawn-but-dead, which is the same call `StatusView` makes about a key it cannot spell.
    //
    // Both halves are asserted. Without the second, an implementation that stopped drawing
    // the whole heading — losing the label and the count with it — would pass: the section's
    // *facts* must survive, only the affordance goes.
    let context = ViewContext::new(
        &crate::theme::ThemeRegistry::new()
            .resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark),
        crate::config::ResolvedTuiConfig {
            mouse: false,
            ..crate::config::ResolvedTuiConfig::default()
        },
    );
    let mut view = SidebarView::new(context);
    *view.ambient_mut() = ambient();
    let joined = drawn(&mut view);

    assert!(
        !joined.contains(OPEN_GLYPH) && !joined.contains(CLOSED_GLYPH),
        "a disclosure triangle is still advertised with the mouse off:\n{joined}"
    );
    for needle in ["LSP", "MCP", "Skills", "1/2", "rust-analyzer"] {
        assert!(
            joined.contains(needle),
            "withdrawing the triangle also withdrew `{needle}`, which is a fact rather than \
             an affordance:\n{joined}"
        );
    }
    // And nothing is clickable, because no target was recorded.
    for row in 0..40 {
        assert!(
            !view.click(0, row),
            "row {row} answers a click although the build reports no mouse"
        );
    }
}

#[test]
fn views_sidebar_forgets_its_click_targets_once_it_stops_being_drawn() {
    // The panel's targets are frame geometry, so the frame that stops drawing it has to
    // retract them. Otherwise hiding the sidebar leaves its old rows swallowing clicks aimed
    // at the transcript that took those columns.
    let mut view = view();
    let row = heading_rows(&mut view, SIDEBAR_WIDTH, 40)[0].0;
    assert!(view.click(0, row), "the target was never recorded");

    view.forget_hit_targets();
    assert!(
        !view.click(0, row),
        "a retracted target still answered a click"
    );

    // A zero-width frame is the other way the panel stops being drawn, and it must retract
    // them by itself rather than relying on the owner to remember.
    let _ = render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible");
    assert!(view.click(0, row), "the redraw did not restore the target");
    let _ = render_offscreen(&mut view, 0, 40).expect("infallible");
    assert!(
        !view.click(0, row),
        "a frame that drew nothing left its previous targets live"
    );
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[test]
fn views_sidebar_pins_the_location_and_version_to_the_bottom() {
    let mut view = view();
    let lines = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible"));
    let last = lines
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .expect("something was drawn");
    let tail = lines[last.saturating_sub(1)..=last].join("\n");
    assert!(
        tail.contains("zuno 0.1.0"),
        "the version is not at the foot of the panel:\n{tail}"
    );
}

#[test]
fn views_sidebar_never_overflows_its_column() {
    let mut view = view();
    view.toggle_skills();
    for row in rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible")) {
        assert!(
            row.chars().count() <= usize::from(SIDEBAR_WIDTH),
            "a row overflowed the panel: {row:?}"
        );
    }
}

#[test]
fn views_sidebar_truncates_a_long_detail_from_the_left() {
    // A path is identified by its tail, so a truncated root keeps the part that
    // distinguishes it and marks the cut with an ellipsis.
    let mut view = SidebarView::new(ViewContext::defaults());
    view.ambient_mut().lsp = vec![
        Service::new("gopls", Health::Ready)
            .detailed("/a/very/long/workspace/root/that/will/not/fit/in/the/panel"),
    ];
    let joined = drawn(&mut view);
    assert!(
        joined.contains('…'),
        "an over-long detail was not marked as truncated:\n{joined}"
    );
    assert!(
        joined.contains("panel"),
        "truncation kept the wrong end of the path:\n{joined}"
    );
}

#[test]
fn views_sidebar_renders_into_a_degenerate_area_without_panicking() {
    let mut view = view();
    for (width, height) in [(0, 0), (1, 1), (SIDEBAR_WIDTH, 1), (4, 40)] {
        let _ = render_offscreen(&mut view, width, height).expect("infallible");
    }
}

#[test]
fn views_sidebar_health_glyphs_are_all_distinct() {
    // Four states rendered by the same glyph would make the panel unreadable in the
    // one situation it exists for.
    let glyphs = [
        Health::Ready.glyph(),
        Health::Pending.glyph(),
        Health::Faulted.glyph(),
        Health::Disabled.glyph(),
    ];
    for (index, left) in glyphs.iter().enumerate() {
        for right in glyphs.iter().skip(index + 1) {
            assert_ne!(left, right, "two health states share a glyph");
        }
    }
}

#[test]
fn views_ambient_compact_abbreviates_at_each_magnitude() {
    assert_eq!(compact(0), "0");
    assert_eq!(compact(999), "999");
    assert_eq!(compact(1_000), "1.0k");
    assert_eq!(compact(12_345), "12.3k");
    assert_eq!(compact(2_500_000), "2.5m");
}

#[test]
fn views_ambient_elide_left_keeps_the_identifying_tail() {
    assert_eq!(elide_left("short", 10), "short");
    assert_eq!(elide_left("abcdefghij", 5), "…ghij");
    assert_eq!(elide_left("abc", 1), "a");
    assert_eq!(elide_left("abc", 0), "");
    let wide = elide_left("日本語テスト", 4);
    assert_eq!(wide, "…ト", "the identifying tail was not preserved");
    assert!(
        display_width(&wide) <= 4,
        "the elided text still occupies {} columns: {wide:?}",
        display_width(&wide)
    );
}

#[test]
fn views_sidebar_drops_a_detail_that_would_become_a_stub() {
    // The failure this replaces rendered `aws-knowledge-mcp-server …d` on a real
    // terminal: a one-character fragment of `configured`, which says nothing the glyph
    // did not already say.
    let mut view = SidebarView::new(ViewContext::defaults());
    view.ambient_mut().mcp =
        vec![Service::new("aws-knowledge-mcp-server", Health::Pending).detailed("configured")];
    let joined = drawn(&mut view);
    assert!(joined.contains("aws-knowledge-mcp-server"), "{joined}");
    assert!(
        !joined.contains("…d"),
        "a detail was abbreviated into a stub:\n{joined}"
    );
}

#[test]
fn views_sidebar_footer_path_is_cut_at_the_front_not_the_end() {
    let mut view = view();
    view.ambient_mut().directory = Some(String::from(
        "~/workspace/ProdDir/AI/oc-wt/r17-solo/crates/zuno-tui",
    ));
    let lines = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible"));
    let footer = lines.join("\n");
    assert!(
        footer.contains("zuno-tui"),
        "the identifying tail of the path was discarded:\n{footer}"
    );
    assert!(footer.contains('…'), "the cut was not marked:\n{footer}");
}
