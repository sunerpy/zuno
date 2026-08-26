//! The ambient panel's promise: it states what is running, and says so honestly.

use super::*;
use crate::app::render_offscreen;
use crate::views::message::{ContextWindowUsage, TokenUsage, UsageState};
use crate::views::testkit::rows;

fn ambient() -> Ambient {
    Ambient {
        // Unnamed in the shared fixture, so every assertion written before the name
        // existed still describes the panel it was written against. The tests that care
        // about the name set it themselves.
        title: None,
        directory: Some(String::from("~/src/zuno")),
        branch: Some(String::from("task-r17-solo")),
        agent: Some(String::from("build")),
        model: Some(String::from("myopenai/claude-haiku-4-5")),
        tokens: TokenUsage {
            input: 12_000,
            output: 3_400,
            reasoning: 0,
            cache_read: 800,
            cache_write: 200,
            unclassified: 0,
        },
        usage_state: UsageState::Known,
        failed_turns: 0,
        context: Some(ContextWindowUsage {
            prompt_tokens: 64_000,
            limit: 100_000,
            estimated: false,
        }),
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
                source: String::from("/skills/commit-msg/SKILL.md"),
                description: String::from("conventional commits"),
                loaded: false,
            },
            SkillSummary {
                name: String::from("codegraph"),
                source: String::from("/skills/codegraph/SKILL.md"),
                description: String::from("code navigation"),
                loaded: false,
            },
        ],
        agents: Vec::new(),
        work: zuno_types::WorkStateProjection::default(),
        version: Some(String::from("0.1.0")),
    }
}

fn view() -> SidebarView {
    let context = ViewContext::new(
        &crate::theme::ThemeRegistry::new()
            .resolve(crate::theme::DEFAULT_THEME, crate::theme::Mode::Dark),
        crate::config::ResolvedTuiConfig {
            mouse: true,
            ..crate::config::ResolvedTuiConfig::default()
        },
    );
    let mut view = SidebarView::new(context);
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
        "64.0k / 100.0k current prompt",
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
fn views_sidebar_prioritizes_the_live_context_over_cumulative_accounting() {
    let mut view = view();
    let joined = drawn(&mut view);

    assert!(
        joined.contains("64.0k / 100.0k current prompt"),
        "the live prompt size is missing:\n{joined}"
    );
    assert!(
        joined.contains("64.0% of model window"),
        "the model-window pressure is missing:\n{joined}"
    );
    for cumulative in [
        "session total",
        "input ·",
        "output",
        "cache read",
        "cache write",
    ] {
        assert!(
            !joined.contains(cumulative),
            "the sidebar still exposes cumulative accounting `{cumulative}`:\n{joined}"
        );
    }
}

#[test]
fn views_sidebar_projects_goal_todos_jobs_and_reviewable_memory() {
    let mut view = view();
    view.ambient_mut().work = zuno_types::WorkStateProjection {
        goal: Some(zuno_types::GoalStateProjection {
            id: "goal_1".to_owned(),
            revision: 3,
            objective: "finish the durable runtime upgrade".to_owned(),
            success_criteria: vec!["workspace gates pass".to_owned()],
            status: "active".to_owned(),
            blocked_reason: None,
            span: zuno_types::ExecutionSpan::from_aggregate(1, None, 42_000, 1_200, true),
            token_budget: Some(5_000),
            time_created: 1,
            time_updated: 2,
        }),
        plan: Some(zuno_types::PlanProjection {
            id: "plan_1".to_owned(),
            goal_id: Some("goal_1".to_owned()),
            revision: 2,
            title: "Release hardening".to_owned(),
            steps: vec![zuno_types::PlanStepProjection {
                id: "verify".to_owned(),
                title: "Run workspace gates".to_owned(),
                status: "in_progress".to_owned(),
            }],
            span: zuno_types::ExecutionSpan::from_aggregate(1, None, 21_000, 600, true),
            time_created: 1,
            time_updated: 2,
        }),
        todos: vec![zuno_types::TodoProjection {
            id: "todo_1".to_owned(),
            goal_id: Some("goal_1".to_owned()),
            plan_step_id: Some("verify".to_owned()),
            parent_id: None,
            subject: "run the workspace gates".to_owned(),
            description: "validate the release surface".to_owned(),
            active_form: Some("Running workspace gates".to_owned()),
            status: "in_progress".to_owned(),
            priority: "high".to_owned(),
            dependencies: Vec::new(),
            owner: Some("build".to_owned()),
            revision: 4,
            span: zuno_types::ExecutionSpan::from_aggregate(1, None, 21_000, 600, true),
            time_created: 1,
            time_updated: 2,
        }],
        background_executions: vec![zuno_types::BackgroundExecutionProjection {
            id: "bg_1".to_owned(),
            title: "preview server".to_owned(),
            command: "python3 -m http.server 4173".to_owned(),
            status: "running".to_owned(),
            pid: Some(4173),
            exit_code: None,
            timed_out: false,
            error: None,
            span: zuno_types::ExecutionSpan::from_aggregate(1, None, 18_000, 0, false),
            time_created: 1,
            time_completed: None,
        }],
        jobs: vec![
            zuno_types::JobProjection {
                id: "job_queued".to_owned(),
                subject: zuno_types::JobSubjectProjection::ChildSession {
                    session_id: "ses_waiting".to_owned(),
                },
                status: "queued".to_owned(),
                report_delivery: "quiet".to_owned(),
                result: None,
                error: None,
                span: zuno_types::ExecutionSpan::from_aggregate(1, None, 5_000, 0, false),
                children: Vec::new(),
                time_created: 1,
                time_completed: None,
            },
            zuno_types::JobProjection {
                id: "job_1".to_owned(),
                subject: zuno_types::JobSubjectProjection::ProductAgent {
                    run_id: "run_1".to_owned(),
                    product: "codex".to_owned(),
                    instance: "review patch".to_owned(),
                    tool: "subagent_codex".to_owned(),
                },
                status: "running".to_owned(),
                report_delivery: "quiet".to_owned(),
                result: None,
                error: None,
                span: zuno_types::ExecutionSpan::from_aggregate(1, None, 21_000, 0, false),
                children: Vec::new(),
                time_created: 1,
                time_completed: None,
            },
        ],
        memory_candidates: vec![zuno_types::MemoryCandidateProjection {
            id: "mem_1".to_owned(),
            scope: zuno_types::MemoryScope::Project,
            action: zuno_types::MemoryAction::Add,
            content: Some("run cargo fmt before commit".to_owned()),
            old_text: None,
            reason: "verified repository gate".to_owned(),
            confidence: 9_500,
            source: zuno_types::MemorySource::Reflection,
            source_session_id: Some("ses_1".to_owned()),
            source_message_id: Some("msg_1".to_owned()),
            status: zuno_types::MemoryCandidateStatus::Pending,
            error: None,
            time_created: 1,
            time_updated: 1,
        }],
        memory_entries: vec![zuno_types::MemoryEntryProjection {
            scope: zuno_types::MemoryScope::Global,
            content: "prefer concise explanations".to_owned(),
        }],
    };
    view.toggle(Section::Memory);
    let joined = view
        .lines(SIDEBAR_WIDTH)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");

    for expected in [
        "Goal",
        "finish the durable runtime upgrade",
        "Plan",
        "Release hardening · r2",
        "Todos",
        "Running workspace gates",
        "21s · 600 tokens",
        "Background",
        "preview server · running",
        "/ps to inspect output",
        "Jobs",
        "1 queued · 1 running",
        "child ses_waiting",
        "queued · 5s · — tokens",
        "codex · review patch",
        "/subagent",
        "Memory",
        "1 review · 1 saved",
        "run cargo fmt",
        "prefer concise",
    ] {
        assert!(
            normalized.contains(expected),
            "missing `{expected}`:\n{joined}"
        );
    }
}

#[test]
fn views_sidebar_projects_council_seats_and_the_real_subagent_shortcut() {
    let mut view = view();
    view.ambient_mut().work.jobs = vec![zuno_types::JobProjection {
        id: "job_council".to_owned(),
        subject: zuno_types::JobSubjectProjection::Council {
            run_id: "run_council".to_owned(),
            preset: "balanced-review".to_owned(),
        },
        status: "running".to_owned(),
        report_delivery: "nextStep".to_owned(),
        result: None,
        error: None,
        span: zuno_types::ExecutionSpan::from_aggregate(1_000, None, 8_000, 500, true),
        children: vec![
            zuno_types::JobChildProjection {
                id: "work_run_council:node:0".to_owned(),
                subject: "evidence".to_owned(),
                owner: Some("explorer".to_owned()),
                status: "completed".to_owned(),
                span: zuno_types::ExecutionSpan::from_aggregate(
                    1_000,
                    Some(4_000),
                    3_000,
                    200,
                    true,
                ),
            },
            zuno_types::JobChildProjection {
                id: "work_run_council:node:1".to_owned(),
                subject: "judgment".to_owned(),
                owner: Some("oracle".to_owned()),
                status: "in_progress".to_owned(),
                span: zuno_types::ExecutionSpan::from_aggregate(1_000, None, 5_000, 300, true),
            },
            zuno_types::JobChildProjection {
                id: "work_run_council:node:2".to_owned(),
                subject: "dissent".to_owned(),
                owner: Some("general".to_owned()),
                status: "pending".to_owned(),
                span: zuno_types::ExecutionSpan::default(),
            },
        ],
        time_created: 1_000,
        time_completed: None,
    }];

    let shortcut = crate::views::pressable_label("session_child_first", &view.context)
        .unwrap_or_else(|| "/subagent".to_owned());
    let body = view
        .lines(SIDEBAR_WIDTH)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "council · balanced-review",
        "running · 8s · 500 tokens",
        "1/3 seats done · 1 running",
        "evidence · completed",
        "explorer · 3s · 200 tokens",
        shortcut.as_str(),
    ] {
        assert!(
            normalized.contains(expected),
            "missing `{expected}`:\n{body}"
        );
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
fn views_sidebar_hides_an_unconfigured_lsp_section() {
    // "No servers configured" and "servers failed" must never render the same, which is
    // the "no results versus cannot see the data" confusion.
    let mut view = SidebarView::new(ViewContext::defaults());
    let empty = drawn(&mut view);
    assert!(
        empty.contains("none configured"),
        "an unconfigured MCP section says nothing at all:\n{empty}"
    );
    assert!(
        !empty.contains("LSP"),
        "an unconfigured capability was rendered as an action item:\n{empty}"
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
        drawn.contains("LSP"),
        "configured language servers have no section:\n{drawn}"
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
fn views_sidebar_labels_unconfirmed_prompt_usage_as_an_estimate() {
    let mut view = view();
    view.ambient_mut().usage_state = UsageState::NotReported;
    view.ambient_mut().context = Some(ContextWindowUsage {
        prompt_tokens: 81_000,
        limit: 100_000,
        estimated: true,
    });
    let joined = drawn(&mut view);
    assert!(
        joined.contains("≈81.0k / 100.0k estimate"),
        "the local estimate is not labelled clearly:\n{joined}"
    );
    assert!(joined.contains("≈81.0% of model window"));
    assert!(!joined.contains("no usage reported yet"));
}

#[test]
fn views_sidebar_warns_once_the_context_window_is_nearly_full() {
    let mut view = view();
    view.ambient_mut().context = Some(ContextWindowUsage {
        prompt_tokens: 91_000,
        limit: 100_000,
        estimated: false,
    });
    let buffer = render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible");
    let warning = ratatui::style::Color::from(ViewContext::defaults().palette().warning);
    let row = (0..40)
        .find(|y| {
            (0..SIDEBAR_WIDTH)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("91.0%")
        })
        .expect("the percentage row is drawn");
    let coloured = (0..SIDEBAR_WIDTH).any(|x| buffer[(x, row)].fg == warning);
    assert!(
        coloured,
        "a nearly-full context window is not visually distinguished"
    );
}

#[test]
fn views_sidebar_colours_only_a_ready_services_status_glyph_green() {
    let view = view();
    let row = view
        .lines(SIDEBAR_WIDTH)
        .into_iter()
        .find(|line| line.to_string().contains("rust-analyzer"))
        .expect("the ready service row");
    let text = view.context.text().fg;
    let success = view.context.success().fg;
    let name = row
        .spans
        .iter()
        .find(|span| span.content.contains("rust-analyzer"))
        .expect("service name span");
    assert_eq!(
        name.style.fg, text,
        "a ready service painted its whole name green instead of only its status glyph"
    );
    assert!(
        row.spans.iter().any(|span| {
            span.content.contains(Health::Ready.glyph()) && span.style.fg == success
        }),
        "the ready state lost its compact success marker"
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
fn views_sidebar_groups_loaded_skills_first_and_sorts_each_group() {
    let mut view = view();
    view.ambient_mut().skills = vec![
        SkillSummary {
            name: String::from("zeta-unloaded"),
            source: String::from("/skills/zeta-unloaded/SKILL.md"),
            description: String::new(),
            loaded: false,
        },
        SkillSummary {
            name: String::from("zeta-loaded"),
            source: String::from("/skills/zeta-loaded/SKILL.md"),
            description: String::new(),
            loaded: true,
        },
        SkillSummary {
            name: String::from("alpha-unloaded"),
            source: String::from("/skills/alpha-unloaded/SKILL.md"),
            description: String::new(),
            loaded: false,
        },
        SkillSummary {
            name: String::from("alpha-loaded"),
            source: String::from("/skills/alpha-loaded/SKILL.md"),
            description: String::new(),
            loaded: true,
        },
    ];
    view.toggle_skills();

    let joined = drawn(&mut view);
    let at = |needle: &str| {
        joined
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is missing:\n{joined}"))
    };
    assert!(
        at("Loaded (2)") < at("alpha-loaded · loaded")
            && at("alpha-loaded · loaded") < at("zeta-loaded · loaded")
            && at("zeta-loaded · loaded") < at("Not loaded (2)")
            && at("Not loaded (2)") < at("alpha-unloaded")
            && at("alpha-unloaded") < at("zeta-unloaded"),
        "skills were not grouped loaded-first and sorted within each group:\n{joined}"
    );
}

#[test]
fn views_sidebar_scrolls_its_body_independently_and_keeps_the_footer_pinned() {
    let mut view = view();
    view.ambient_mut().title = Some(String::from("Investigating the frozen turn"));
    view.ambient_mut().skills = (0..24)
        .map(|index| SkillSummary {
            name: format!("skill-{index:02}"),
            source: format!("/skills/skill-{index:02}/SKILL.md"),
            description: String::new(),
            loaded: index == 23,
        })
        .collect();
    view.toggle_skills();

    let before = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 12).expect("infallible"));
    let footer = before.last().cloned().expect("version footer");
    assert!(
        footer.contains("● zuno 0.1.0"),
        "the version footer is not pinned before scrolling: {footer}"
    );

    assert!(
        view.scroll_lines(isize::MAX),
        "the sidebar reported no scrollable body"
    );
    let after = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 12).expect("infallible"));
    assert_ne!(before, after, "scrolling did not change the sidebar body");
    assert!(
        after.join("\n").contains("skill-22"),
        "scrolling did not reach the tail of the not-loaded group:\n{}",
        after.join("\n")
    );
    assert!(
        after.join("\n").contains("Investigating the frozen turn"),
        "scrolling the sidebar moved the current session title off its fixed header:\n{}",
        after.join("\n")
    );
    assert_eq!(
        after.last(),
        Some(&footer),
        "scrolling moved the pinned footer"
    );
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

// ---------------------------------------------------------------------------
// The session's name
// ---------------------------------------------------------------------------

/// The row index of the first line containing `needle`, or a failure naming the panel.
fn row_of(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` is not on the panel:\n{}", lines.join("\n")))
}

#[test]
fn views_sidebar_states_the_session_name_above_the_context_block() {
    // Given: a named session.
    let mut view = view();
    view.ambient_mut().title = Some(String::from("Refactoring user service"));

    // When: the panel is drawn at the width it is allotted.
    let lines = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible"));

    // Then: the name is on the panel, and above `Context` rather than below it. The
    // ordering is the assertion — a name rendered under the token figures reads as another
    // measurement rather than as the thing being measured.
    assert!(
        row_of(&lines, "Refactoring user service") < row_of(&lines, "Context"),
        "the session name must precede the Context block:\n{}",
        lines.join("\n")
    );
}

#[test]
fn views_sidebar_says_nothing_at_all_when_the_session_has_no_name_yet() {
    // Given: a session that has not been named — the state every session starts in.
    let mut view = view();
    view.ambient_mut().title = None;

    // When: the panel is drawn.
    let lines = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible"));

    // Then: `Context` is the panel's first row. No placeholder, and no blank spacer left
    // behind by one — a reserved row would make the panel reflow the moment the name
    // arrives, which is the flicker `Ambient::title` documents.
    assert_eq!(
        row_of(&lines, "Context"),
        0,
        "an unnamed session must not reserve a row:\n{}",
        lines.join("\n")
    );
}

#[test]
fn views_sidebar_keeps_a_long_session_name_inside_the_panel_and_marks_the_cut() {
    // Given: a name at the length the generator permits, which no panel this wide can show
    // whole.
    let mut view = view();
    let long = "Investigate why the nightly integration suite intermittently fails on the \
                macOS runner but never on Linux";
    view.ambient_mut().title = Some(String::from(long));

    let lines = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible"));

    // Then: every row the panel composes fits the columns it was given. Asserted against
    // `lines`, the panel's own output, and not against the rendered buffer: `testkit::rows`
    // reconstructs a row cell by cell, and a wide glyph's blank continuation cell comes back
    // as a space — so the reconstruction is wider than the panel by one per wide glyph and
    // could never fail this. The buffer proof is the ordering assertion below.
    for line in view.lines(SIDEBAR_WIDTH) {
        let width = crate::views::display_width(&line.to_string());
        assert!(
            width <= usize::from(SIDEBAR_WIDTH),
            "a composed row overran the panel at {width} columns: {line:?}"
        );
    }

    // And: the name is bounded to its row budget rather than pushing the sections off. The
    // budget is `TITLE_MAX_ROWS` rows plus the one spacer, so `Context` lands at a known
    // index no matter how long the name is — an unbounded wrap would push it further with
    // every extra word.
    assert_eq!(
        row_of(&lines, "Context"),
        TITLE_MAX_ROWS + 1,
        "an over-long name spent more than its row budget:\n{}",
        lines.join("\n")
    );

    // And: the discarded tail is marked, so the reader knows the name continues.
    assert!(
        lines.join("\n").contains('…'),
        "the truncation was silent:\n{}",
        lines.join("\n")
    );
}

#[test]
fn views_sidebar_wraps_a_cjk_session_name_by_columns_not_characters() {
    // Given: a name with no spaces in it at all, which is the ordinary case in Chinese and
    // the one a word-only wrapper returns as a single over-long row.
    let mut view = view();
    view.ambient_mut().title = Some(String::from(
        "重构用户服务并修复登录接口的并发缺陷以及补齐相关的回归测试",
    ));

    let lines = rows(&render_offscreen(&mut view, SIDEBAR_WIDTH, 40).expect("infallible"));

    // Composed rows rather than reconstructed ones, for the reason the long-name test
    // records: a cell-by-cell reconstruction gains a space per wide glyph.
    for line in view.lines(SIDEBAR_WIDTH) {
        let width = crate::views::display_width(&line.to_string());
        assert!(
            width <= usize::from(SIDEBAR_WIDTH),
            "a wide-glyph row overran the panel at {width} columns: {line:?}"
        );
    }
    // A single glyph as the needle: `testkit::rows` reads the buffer cell by cell and a wide
    // glyph's continuation cell comes back as a space, so `"重构"` is `"重 构"` here and a
    // multi-glyph needle can never match however correct the panel is.
    assert!(
        row_of(&lines, "重") < row_of(&lines, "Context"),
        "the name must still precede Context:\n{}",
        lines.join("\n")
    );
}

#[test]
fn views_session_title_projection_advances_only_on_a_real_change() {
    let projection = SessionTitle::default();
    assert_eq!(projection.observe(), (0, None));

    projection.replace(Some(String::from("A name")));
    let (first, title) = projection.observe();
    assert_eq!(title.as_deref(), Some("A name"));
    assert_eq!(first, 1);

    // Republishing the same name must not spend a frame repainting identical bytes.
    projection.replace(Some(String::from("A name")));
    assert_eq!(
        projection.generation(),
        first,
        "an identical title advanced the generation"
    );

    projection.replace(Some(String::from("Another name")));
    assert_eq!(projection.generation(), first + 1);
}
