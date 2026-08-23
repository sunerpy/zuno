//! What the two troubleshooting panels state, and that a person can reach and read it.
//!
//! Assertions go through [`DialogHost`] rather than at a panel directly, for the reason
//! `basics_tests` records: a dialog tested in isolation can pass while being unreachable,
//! and a frame assertion can pass *vacuously* when another surface owns the row under
//! test. Rows are located by content, never by index.

use super::*;
use crate::app::render_offscreen;
use crate::keybind::ActionComponent;
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};

fn host() -> (DialogHost, ViewContext) {
    let context = ViewContext::defaults();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    (DialogHost::new(context.clone(), Box::new(base)), context)
}

fn send(host: &mut DialogHost, name: &'static str) {
    host.handle_action(action(name), &press(KeyCode::Null));
}

fn frame(host: &mut DialogHost, width: u16, height: u16) -> Vec<String> {
    rows(&render_offscreen(host, width, height).expect("infallible"))
}

fn census() -> Vec<Group> {
    vec![
        Group::new(
            "MCP servers",
            vec![
                Service::new("filesystem", Health::Ready).detailed("● Connected"),
                Service::new("flaky", Health::Faulted).detailed("✗ Failed · handshake timed out"),
            ],
        ),
        Group::new(
            "LSP servers",
            vec![Service::new("rust", Health::Pending).detailed("starts on first matching file")],
        ),
        Group::new("Plugins", Vec::new()),
    ]
}

fn facts() -> DebugFacts {
    DebugFacts {
        build: Some(String::from("0.4.1")),
        version: Some(String::from("0.4.1")),
        channel: Some(String::from("local")),
        os: Some(String::from("linux x86_64")),
        terminal: Some(String::from("tmux")),
        session: Some(String::from("ses_abc123")),
        model: Some(String::from("example/example-large")),
        agent: Some(String::from("build")),
        directory: Some(String::from("~/work/zuno")),
    }
}

// ---------------------------------------------------------------------------
// StatusPanel (D15)
// ---------------------------------------------------------------------------

#[test]
fn views_status_panel_groups_every_census_member_under_its_heading() {
    let (mut host, context) = host();
    host.open(Box::new(StatusPanel::new(context, census())));

    let drawn = frame(&mut host, 130, 20).join("\n");
    for needle in [
        "MCP servers",
        "filesystem",
        "LSP servers",
        "rust",
        "Plugins",
    ] {
        assert!(
            drawn.contains(needle),
            "the census omitted `{needle}`:\n{drawn}"
        );
    }
}

#[test]
fn views_status_panel_states_the_reason_a_server_failed() {
    // `§8.4`: "不显示原因的失败状态等于没有状态" — a failure without its reason is not a
    // status. This is the whole value of the panel over the sidebar's health glyph, so it
    // is asserted on the rendered frame rather than on the row model.
    let (mut host, context) = host();
    host.open(Box::new(StatusPanel::new(context, census())));

    let drawn = frame(&mut host, 130, 20).join("\n");
    assert!(
        drawn.contains("handshake timed out"),
        "the failed server's reason is not on screen:\n{drawn}"
    );
}

#[test]
fn views_status_panel_says_none_rather_than_leaving_a_heading_bare() {
    let (mut host, context) = host();
    host.open(Box::new(StatusPanel::new(context, census())));

    let drawn = frame(&mut host, 130, 20);
    let plugins = drawn
        .iter()
        .position(|row| row.contains("Plugins"))
        .expect("the plugins heading is drawn");
    assert!(
        drawn
            .get(plugins + 1)
            .is_some_and(|row| row.contains(EMPTY)),
        "the empty group's heading has nothing under it, which reads as a panel that \
         failed to load:\n{}",
        drawn.join("\n")
    );
}

#[test]
fn views_status_panel_health_colour_comes_from_the_palette() {
    let context = ViewContext::defaults();
    let mut panel = StatusPanel::new(context.clone(), census());
    let lines = panel.lines(120);

    let failed = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("handshake timed out"))
        })
        .expect("the failed row is rendered");
    let glyph = failed.spans.first().expect("the row has a gutter span");
    assert_eq!(
        glyph.style.fg,
        context.error().fg,
        "a faulted member is not drawn in the palette's error colour"
    );

    let ready = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("filesystem"))
        })
        .expect("the ready row is rendered");
    assert_eq!(
        ready.spans.first().expect("a gutter span").style.fg,
        context.success().fg,
        "a ready member is not drawn in the palette's success colour"
    );
}

#[test]
fn views_status_panel_scrolls_to_a_group_below_the_first_screenful() {
    let (mut host, context) = host();
    let long = (0..40)
        .map(|index| Service::new(format!("server-{index}"), Health::Ready))
        .collect();
    host.open(Box::new(StatusPanel::new(
        context,
        vec![
            Group::new("MCP servers", long),
            Group::new("LSP servers", vec![Service::new("rust", Health::Pending)]),
        ],
    )));

    let before = frame(&mut host, 130, 20).join("\n");
    assert!(
        !before.contains("LSP servers"),
        "the second group is already visible, so this cannot prove scrolling:\n{before}"
    );
    for _ in 0..40 {
        send(&mut host, "dialog.select.page_down");
    }
    let after = frame(&mut host, 130, 20).join("\n");
    assert!(
        after.contains("LSP servers"),
        "paging down never reached the last group:\n{after}"
    );
}

#[test]
fn views_status_and_debug_panels_scroll_with_the_mouse_wheel() {
    let event = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    let body = ratatui::layout::Rect::new(0, 0, 100, 20);

    let mut status = StatusPanel::new(ViewContext::defaults(), census());
    assert_eq!(status.handle_mouse(&event, body), DialogStep::Redraw);
    assert_eq!(status.offset, 1);

    let mut debug = DebugPanel::new(ViewContext::defaults(), facts());
    assert_eq!(debug.handle_mouse(&event, body), DialogStep::Redraw);
    assert_eq!(debug.offset, 1);
}

#[test]
fn views_status_panel_offers_no_selection_because_it_has_none() {
    // `§6.2` gives D15 no confirm semantics. A footer advertising `enter select` on a
    // read-only panel is the same defect as a hint naming a key that resolves elsewhere.
    let context = ViewContext::defaults();
    let panel = StatusPanel::new(context, census());
    let hints = panel.hints();
    assert!(
        !hints.iter().any(|(_, label)| *label == "select"),
        "the read-only census advertises a selection: {hints:?}"
    );
    assert!(
        hints.iter().any(|(key, _)| *key == "esc"),
        "the census does not say how to close it: {hints:?}"
    );
}

#[test]
fn views_status_panel_closes_on_escape_and_on_its_own_key() {
    for closer in ["session_interrupt", "status_view"] {
        let (mut host, context) = host();
        host.open(Box::new(StatusPanel::new(context, census())));
        assert!(host.is_open(), "the census did not open");
        send(&mut host, closer);
        assert!(
            !host.is_open(),
            "`{closer}` left the census on the stack, so the key does nothing"
        );
    }
}

// ---------------------------------------------------------------------------
// DebugPanel (D16)
// ---------------------------------------------------------------------------

#[test]
fn views_debug_panel_states_every_resolved_fact() {
    let (mut host, context) = host();
    host.open(Box::new(DebugPanel::new(context, facts())));

    let drawn = frame(&mut host, 130, 20).join("\n");
    for needle in [
        "version",
        "channel",
        "local",
        "os",
        "linux x86_64",
        "terminal",
        "session",
        "ses_abc123",
        "model",
        "example/example-large",
    ] {
        assert!(
            drawn.contains(needle),
            "the debug report omitted `{needle}`:\n{drawn}"
        );
    }
}

#[test]
fn views_debug_panel_omits_a_fact_the_host_could_not_resolve() {
    // `§8.7`: a field with no data source is not shown. A placeholder is
    // indistinguishable from a fact that failed to load, which is the one ambiguity a
    // troubleshooting surface must not have.
    let facts = DebugFacts {
        version: Some(String::from("0.4.1")),
        ..DebugFacts::default()
    };
    let (mut host, context) = host();
    host.open(Box::new(DebugPanel::new(context, facts)));

    let drawn = frame(&mut host, 130, 20).join("\n");
    assert!(drawn.contains("version"), "the one known fact is missing");
    for absent in ["channel", "terminal", "session"] {
        assert!(
            !drawn.contains(absent),
            "`{absent}` was never resolved yet the report has a row for it:\n{drawn}"
        );
    }
}

#[test]
fn views_debug_report_is_pasteable_rather_than_the_padded_rows() {
    let report = facts().report();
    assert!(
        report.contains("version: 0.4.1") && report.contains("session: ses_abc123"),
        "the copied report is not `label: value` per line:\n{report}"
    );
    for line in report.lines() {
        assert_eq!(
            line.trim_end(),
            line,
            "a copied line carries trailing padding, which somebody has to clean up \
             out of an issue report:\n{report}"
        );
        assert!(
            !line.contains('●'),
            "the copied report carries a health glyph that means nothing in text:\n{report}"
        );
    }
}

#[test]
fn views_debug_panel_enter_emits_the_report_and_stays_open() {
    let (mut host, context) = host();
    host.open(Box::new(DebugPanel::new(context, facts())));

    send(&mut host, "dialog.select.submit");
    let outcomes = host.drain_outcomes();
    assert_eq!(
        outcomes,
        vec![(
            DEBUG_DIALOG_ID,
            DialogOutcome::Submitted {
                dialog: DEBUG_DIALOG_ID,
                text: facts().report(),
            }
        )],
        "enter did not emit the report for the screen to copy"
    );
    // Copying is not finishing: a panel that vanished would leave the second press
    // landing on whatever is behind it.
    assert!(
        host.is_open(),
        "the report closed itself after being copied"
    );
    assert!(
        frame(&mut host, 130, 20).join("\n").contains("ses_abc123"),
        "the report is no longer readable after copying it"
    );
}

#[test]
fn views_debug_panel_advertises_the_copy_it_actually_performs() {
    let context = ViewContext::defaults();
    let panel = DebugPanel::new(context, facts());
    assert!(
        panel
            .hints()
            .iter()
            .any(|(key, label)| *key == "enter" && *label == "copy"),
        "the report does not say that enter copies it: {:?}",
        panel.hints()
    );
}

// ---------------------------------------------------------------------------
// Width, wide characters, and the 20×10 floor
// ---------------------------------------------------------------------------

#[test]
fn views_diagnostics_rows_never_exceed_the_width_they_were_given() {
    let context = ViewContext::defaults();
    for width in [20_u16, 40, 60, 88, 116, 200] {
        let mut panel = StatusPanel::new(context.clone(), census());
        for line in panel.lines(width) {
            let measured = line
                .spans
                .iter()
                .map(|span| display_width(&span.content))
                .sum::<usize>();
            assert!(
                measured <= usize::from(width),
                "a census row is {measured} columns wide at width {width}"
            );
        }
        let mut debug = DebugPanel::new(context.clone(), facts());
        for line in debug.lines(width) {
            let measured = line
                .spans
                .iter()
                .map(|span| display_width(&span.content))
                .sum::<usize>();
            assert!(
                measured <= usize::from(width),
                "a debug row is {measured} columns wide at width {width}"
            );
        }
    }
}

#[test]
fn views_diagnostics_pads_a_wide_name_by_columns_not_by_characters() {
    // A CJK name is two terminal columns per glyph and one `char`. Padding with
    // `{:<width$}` counts characters, so the detail column would start early by the
    // difference and every row below a wide name would be out of line with it. This is
    // asserted at the span layer because ratatui has already clipped the overflow by the
    // time it reaches a frame — the misalignment is invisible there.
    let context = ViewContext::defaults();
    let mut panel = StatusPanel::new(
        context,
        vec![Group::new(
            "MCP servers",
            vec![
                Service::new("日本語サーバ", Health::Ready).detailed("wide"),
                Service::new("ascii", Health::Ready).detailed("narrow"),
            ],
        )],
    );
    let lines = panel.lines(120);
    let head_width = |needle: &str| {
        lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains(needle)))
            .map(|line| display_width(&line.spans[0].content))
            .expect("the row is rendered")
    };
    assert_eq!(
        head_width("wide"),
        head_width("narrow"),
        "the detail column starts at a different place for a wide name than for an \
         ASCII one, so the two rows do not line up"
    );
}

#[test]
fn views_diagnostics_survive_the_twenty_by_ten_floor() {
    // `§11.6`'s acceptance case. Both panels sit at the widest tier, so this is the pair
    // most likely to draw a `Rect` wider than the frame.
    for panel in [
        Box::new(StatusPanel::new(ViewContext::defaults(), census())) as Box<dyn Dialog>,
        Box::new(DebugPanel::new(ViewContext::defaults(), facts())),
    ] {
        let (mut host, _) = host();
        host.open(panel);
        let drawn = frame(&mut host, 20, 10);
        assert_eq!(drawn.len(), 10, "the frame is not ten rows tall");
        for row in &drawn {
            assert!(
                display_width(row) <= 20 || row.chars().count() <= 20,
                "a row escaped a twenty-column frame: {row:?}"
            );
        }
    }
}

#[test]
fn views_diagnostics_offset_past_the_end_does_not_blank_the_panel() {
    // The census is live: the MCP group shrinks when a server is removed while the panel
    // is open, and an offset left past the new end would render nothing at all.
    let context = ViewContext::defaults();
    let mut panel = StatusPanel::new(context, census());
    for _ in 0..50 {
        panel.handle_action(action("dialog.select.page_down"), &press(KeyCode::Null));
    }
    assert!(
        !panel.lines(120).is_empty(),
        "scrolling past the end left the census blank"
    );
}
