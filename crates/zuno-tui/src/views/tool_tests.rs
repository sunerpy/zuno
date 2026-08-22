//! Per-tool summary and output-budget tests.

use crate::views::message::{ToolDisplay, ToolStatus, tool_affordance};
use crate::views::tool::{
    COLLAPSED_CHARS, EXPANDED_CHARS, EXPANDED_ROWS, Elide, READ_EXPANDED_CHARS, READ_EXPANDED_ROWS,
    SUMMARISED, output_budget, status_style, summary,
};
use crate::views::{ViewContext, display_width};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Enumeration: the table is checked against the registry, not against itself
// ---------------------------------------------------------------------------

/// The `zuno-tools` sources that between them name every built-in wire id.
///
/// Read as *text* rather than by depending on the crate. `zuno-tui` does not link the tool
/// stack — adding `zuno-tools` to a render crate to learn twenty-one strings would pull its
/// whole transitive graph into every TUI build — and this is the technique
/// [`crate::views::views_tests`] already uses to enforce the colour and keybind
/// disciplines against sources it likewise does not import.
fn registry_sources() -> Vec<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf();
    vec![
        workspace.join("crates/zuno-tools/src/registry.rs"),
        workspace.join("crates/zuno-tools/src/memory.rs"),
        workspace.join("crates/zuno-goal/src/tools.rs"),
    ]
}

/// One double-quoted literal from `code`, if it has one.
fn literal(code: &str) -> Option<String> {
    let start = code.find('"')?;
    let rest = &code[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Every wire id the registry can hand a transcript, read out of the registry's own source.
///
/// Two shapes are recognised, because the registry declares its tools in two places:
/// `BuiltinSlot::wire_id`'s `Self::X => "name"` arms for the seventeen slots, and
/// `pub const *_TOOL_ID: &str = "name"` for the built-ins registered outside the slot table
/// (`memory_propose` and the three goal tools).
fn registry_wire_ids() -> Vec<String> {
    let mut ids = Vec::new();
    for path in registry_sources() {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let mut in_wire_id = false;
        for line in source.lines() {
            let code = line.trim();
            if code.starts_with("//") {
                continue;
            }
            if code.contains("fn wire_id(") {
                in_wire_id = true;
            } else if in_wire_id && code == "}" {
                in_wire_id = false;
            }
            // The two shapes are named rather than branched on separately: they differ in
            // where the id is declared, not in what is done with it, and clippy correctly
            // objected to two arms with one body.
            let slot_arm = in_wire_id && code.starts_with("Self::") && code.contains("=>");
            let tool_id_const = code.starts_with("pub const") && code.contains("_TOOL_ID: &str");
            if slot_arm || tool_id_const {
                ids.extend(literal(code));
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

#[test]
fn tool_summaries_cover_every_tool_the_registry_can_expose() {
    let ids = registry_wire_ids();
    // A floor, so the scan cannot pass by finding nothing — the same guard the colour and
    // keybind scans carry. Seventeen slots plus `memory_propose` plus three goal tools is 21.
    assert!(
        ids.len() >= 21,
        "the registry scan found only {} wire ids, so it is reading the wrong files and \
         would pass while covering nothing: {ids:?}",
        ids.len()
    );
    let missing = ids
        .iter()
        .filter(|id| !SUMMARISED.contains(&id.as_str()))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "the registry exposes tools this view has no summary rule for, so each would render \
         as a bare `⚙` row indistinguishable from the others: {missing:?}"
    );
    let stale = SUMMARISED
        .iter()
        .filter(|name| !ids.iter().any(|id| id == *name))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these names have summary rules but the registry no longer has them, so the rule is \
         dead code that will never match — which is how `\"patch\"` sat here unreachable \
         while the registry called it `apply_patch`: {stale:?}"
    );
}

#[test]
fn tool_every_summarised_tool_has_an_icon_of_its_own() {
    // The complement: a summary with no icon still renders as a `⚙` row, so covering one
    // and not the other buys half the legibility.
    let generic = tool_affordance("a tool that certainly does not exist").0;
    let unlabelled = SUMMARISED
        .iter()
        .filter(|name| tool_affordance(name).0 == generic)
        .collect::<Vec<_>>();
    assert!(
        unlabelled.is_empty(),
        "these tools fall through to the generic icon, so a transcript of them is a column \
         of identical glyphs: {unlabelled:?}"
    );
}

#[test]
fn tool_every_icon_is_one_column_so_a_column_of_rows_aligns() {
    for name in SUMMARISED {
        let (icon, _) = tool_affordance(name);
        assert_eq!(
            display_width(icon),
            1,
            "`{name}`'s icon {icon:?} is {} columns, so its row starts one column off from \
             every other tool's",
            display_width(icon)
        );
    }
}

// ---------------------------------------------------------------------------
// Per-tool summaries
// ---------------------------------------------------------------------------

#[test]
fn tool_summary_quotes_the_argument_that_identifies_each_call() {
    // One case per tool that takes arguments, asserting the *discriminating* field reaches
    // the row: this is the whole of §7.5, and the failure it guards against is six tool
    // rows in a turn that a reader cannot tell apart.
    let cases: &[(&str, &str, &str)] = &[
        ("bash", r#"{"command":"cargo test"}"#, "cargo test"),
        ("read", r#"{"filePath":"src/main.rs"}"#, "src/main.rs"),
        (
            "write",
            r#"{"filePath":"src/new.rs","content":"x"}"#,
            "src/new.rs",
        ),
        (
            "edit",
            r#"{"filePath":"src/e.rs","oldString":"a","newString":"b"}"#,
            "src/e.rs",
        ),
        ("glob", r#"{"pattern":"**/*.rs"}"#, "\"**/*.rs\""),
        ("grep", r#"{"pattern":"fn main"}"#, "\"fn main\""),
        (
            "webfetch",
            r#"{"url":"https://example.com/a"}"#,
            "https://example.com/a",
        ),
        (
            "web_search",
            r#"{"queries":["ratatui wrap","ratatui layout"]}"#,
            "ratatui wrap, ratatui layout",
        ),
        (
            "task",
            r#"{"subagent_type":"explore","description":"find the parser"}"#,
            "explore: find the parser",
        ),
        (
            "todowrite",
            r#"{"todos":[{"content":"wire the tree","status":"pending","priority":"high"}]}"#,
            "1 items · wire the tree",
        ),
        (
            "question",
            r#"{"questions":[{"question":"which theme?","header":"h","options":[]}]}"#,
            "which theme?",
        ),
        ("skill", r#"{"name":"codegraph"}"#, "codegraph"),
        ("lsp", r#"{"action":"diagnostics"}"#, "diagnostics"),
        (
            "apply_patch",
            "{\"patchText\":\"*** Update File: src/a.rs\\n@@\\n-a\\n+b\\n\"}",
            "src/a.rs",
        ),
        (
            "execute",
            r#"{"tool_calls":[{"tool":"read","intent":"look"}]}"#,
            "1 calls · read",
        ),
        (
            "invalid",
            r#"{"tool":"nosuchtool","error":"unknown"}"#,
            "nosuchtool",
        ),
        (
            "memory_propose",
            r#"{"target":"project","action":"add","content":"run cargo fmt"}"#,
            "add project: run cargo fmt",
        ),
        ("create_goal", r#"{"objective":"ship P2-4"}"#, "ship P2-4"),
        (
            "update_goal",
            r#"{"status":"blocked","blocking_condition":"no key"}"#,
            "blocked: no key",
        ),
    ];
    for (name, arguments, expected) in cases {
        let produced = summary(name, arguments)
            .unwrap_or_else(|| panic!("`{name}` produced no summary from {arguments}"));
        assert_eq!(
            produced.fit(200),
            *expected,
            "`{name}` summarised {arguments} as {:?}",
            produced.fit(200)
        );
    }
}

#[test]
fn tool_summary_is_absent_rather_than_invented_when_there_is_nothing_to_say() {
    // Three different reasons to say nothing, all rendering the same honest way: the row
    // states the tool and claims nothing about its input.
    assert_eq!(
        summary("plan_exit", "{}"),
        None,
        "plan_exit takes no arguments"
    );
    assert_eq!(
        summary("get_goal", "{}"),
        None,
        "get_goal takes no arguments"
    );
    assert_eq!(
        summary("some_mcp_tool", r#"{"whatever":1}"#),
        None,
        "an MCP tool's argument shape is not knowable here, so nothing may be guessed"
    );
}

#[test]
fn tool_summary_treats_a_half_streamed_argument_as_not_yet_rather_than_as_an_error() {
    // A prefix of a JSON object is what a `Pending` call has, on every provider. Returning
    // `None` is what makes the row fall back to the placeholder instead of shouting about a
    // parse failure that is about to fix itself.
    assert_eq!(summary("read", ""), None);
    assert_eq!(summary("read", r#"{"filePath":"src/ma"#), None);
    // And the moment it completes, the path appears.
    assert!(summary("read", r#"{"filePath":"src/main.rs"}"#).is_some());
}

#[test]
fn tool_summary_ignores_an_empty_argument_the_way_the_strip_ignores_an_empty_branch() {
    // A blank where an argument should be is indistinguishable from one that never
    // arrived, and it still costs the separator's columns.
    assert_eq!(summary("bash", r#"{"command":""}"#), None);
    assert_eq!(summary("read", r#"{"filePath":""}"#), None);
}

#[test]
fn tool_summary_states_the_read_window_only_when_the_model_asked_for_one() {
    let plain = summary("read", r#"{"filePath":"src/main.rs"}"#).expect("a summary");
    assert_eq!(
        plain.fit(200),
        "src/main.rs",
        "an unwindowed read named a window"
    );
    let windowed = summary(
        "read",
        r#"{"filePath":"src/main.rs","offset":10,"limit":20}"#,
    )
    .expect("a summary");
    assert_eq!(windowed.fit(200), "src/main.rs [offset=10,limit=20]");
    let half = summary("read", r#"{"filePath":"src/main.rs","limit":20}"#).expect("a summary");
    assert_eq!(half.fit(200), "src/main.rs [limit=20]");
}

#[test]
fn tool_summary_drops_the_window_before_it_cuts_the_path() {
    // The eye-caught defect that split `Summary` into `text` and `detail`. Rendered as one
    // string, a narrow row cut from the left and kept `…iff.rs [offset=1,limit=162]` — the
    // optional window survived while the filename, the only part that says which call this
    // is, was what paid for it.
    let windowed = summary(
        "read",
        r#"{"filePath":"crates/zuno-tui/src/views/diff.rs","offset":1,"limit":162}"#,
    )
    .expect("a summary");
    let narrow = windowed.fit(30);
    assert!(
        !narrow.contains("offset="),
        "the window outlived the path it qualifies: {narrow:?}"
    );
    assert!(
        narrow.ends_with("diff.rs"),
        "the basename did not survive, so the row names no file: {narrow:?}"
    );
    assert!(
        display_width(&narrow) <= 30,
        "{narrow:?} overran 30 columns"
    );
}

#[test]
fn tool_summary_cuts_a_path_from_the_left_and_a_command_from_the_right() {
    // Both answers are wrong for the other tool: a path is identified by its basename, a
    // command by its verb.
    let path = summary(
        "read",
        r#"{"filePath":"crates/zuno-tui/src/views/diff.rs"}"#,
    )
    .expect("a summary");
    assert_eq!(path.elide, Elide::Head);
    let cut = path.fit(20);
    assert!(
        cut.ends_with("diff.rs"),
        "the basename was cut off: {cut:?}"
    );
    assert!(cut.starts_with('…'), "the cut was not marked: {cut:?}");

    let command =
        summary("bash", r#"{"command":"cargo test --workspace --offline"}"#).expect("a summary");
    assert_eq!(command.elide, Elide::Tail);
    let cut = command.fit(20);
    assert!(cut.starts_with("cargo"), "the verb was cut off: {cut:?}");
    assert!(cut.ends_with('…'), "the cut was not marked: {cut:?}");
}

#[test]
fn tool_summary_fits_a_cjk_path_in_terminal_columns_not_characters() {
    // The §11.5 rule, on the one surface where a character count comes back "short enough"
    // and then overruns the frame by one column per glyph.
    let summarised =
        summary("read", r#"{"filePath":"crates/文档/说明书/読み方.rs"}"#).expect("a summary");
    for room in [8_usize, 12, 20, 30] {
        let fitted = summarised.fit(room);
        assert!(
            display_width(&fitted) <= room,
            "a CJK path fitted to {room} columns measured {}: {fitted:?}",
            display_width(&fitted)
        );
    }
}

#[test]
fn tool_summary_states_the_first_patched_file_and_counts_the_rest() {
    let one = summary(
        "apply_patch",
        "{\"patchText\":\"*** Update File: src/a.rs\\n@@\\n-a\\n+b\\n\"}",
    )
    .expect("a summary");
    assert_eq!(one.fit(200), "src/a.rs");
    let many = summary(
        "apply_patch",
        "{\"patchText\":\"*** Update File: src/a.rs\\n*** Add File: src/b.rs\\n*** Delete File: src/c.rs\\n\"}",
    )
    .expect("a summary");
    assert_eq!(
        many.fit(200),
        "src/a.rs +2 more",
        "a multi-file patch did not say how many files it touched"
    );
}

#[test]
fn tool_summary_marks_a_background_command_because_its_result_means_something_else() {
    let foreground = summary("bash", r#"{"command":"ls","background":false}"#).expect("a summary");
    assert_eq!(foreground.fit(200), "ls");
    let background = summary("bash", r#"{"command":"ls","background":true}"#).expect("a summary");
    assert_eq!(background.fit(200), "ls &");
}

// ---------------------------------------------------------------------------
// The output budget
// ---------------------------------------------------------------------------

#[test]
fn tool_output_budget_gives_read_a_higher_ceiling_than_every_other_tool() {
    // §7.5's own fork: a `read` result *is* the answer, so it is worth more rows than a
    // result that is merely evidence about one.
    let read = output_budget("read", ToolDisplay::Expanded);
    assert_eq!(read.rows, READ_EXPANDED_ROWS);
    assert_eq!(read.chars, READ_EXPANDED_CHARS);
    let other = output_budget("bash", ToolDisplay::Expanded);
    assert_eq!(other.rows, EXPANDED_ROWS);
    assert_eq!(other.chars, EXPANDED_CHARS);
    assert!(
        read.rows > other.rows && read.chars > other.chars,
        "the per-tool fork collapsed, so §7.5's read exception is not being applied"
    );
}

#[test]
fn tool_output_budget_collapses_to_the_same_small_preview_for_every_tool() {
    // Collapsed is a preview, and a preview whose size depended on the tool would make the
    // transcript's row arithmetic vary with its content.
    for name in ["read", "bash", "grep", "some_mcp_tool"] {
        let budget = output_budget(name, ToolDisplay::Collapsed);
        assert_eq!(
            budget.rows,
            crate::views::message::TOOL_OUTPUT_PREVIEW_ROWS,
            "`{name}` collapsed to a different number of rows"
        );
        assert_eq!(budget.chars, COLLAPSED_CHARS);
    }
}

#[test]
fn tool_output_budget_is_not_the_tool_layers_own_cap() {
    // Two different contracts that must not be conflated: `zuno-tool`'s 2,000 lines /
    // 51,200 bytes decides whether a result reaches the *model* and refuses on breach,
    // while this decides how many rows a *frame* spends and keeps the text. A future reader
    // who merged them would make the transcript refuse to show output the model already
    // has.
    let widest = output_budget("read", ToolDisplay::Expanded);
    assert!(
        widest.rows < 2_000 && widest.chars < 51_200,
        "the display budget has grown into the tool layer's cap, which is a different \
         promise: {widest:?}"
    );
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

#[test]
fn tool_status_style_paints_from_the_palette_and_separates_the_terminal_states() {
    let context = ViewContext::defaults();
    let generic = zuno_tool::ToolUiIntent::Generic;
    let error = status_style(ToolStatus::Error, generic, &context);
    let completed = status_style(ToolStatus::Completed, generic, &context);
    let delegated = status_style(
        ToolStatus::Completed,
        zuno_tool::ToolUiIntent::Subagent,
        &context,
    );
    let running = status_style(ToolStatus::Running, generic, &context);
    assert_ne!(
        error.fg, completed.fg,
        "a failed call and a successful one paint the same, so a reader scanning for red \
         cannot find the failure"
    );
    assert_ne!(
        running.fg, completed.fg,
        "an in-flight call paints as finished"
    );
    assert_eq!(
        error.fg,
        Some(ratatui::style::Color::from(context.palette().error)),
        "the error style did not come from the palette's error colour"
    );
    assert_eq!(
        running.fg,
        status_style(ToolStatus::Pending, generic, &context).fg,
        "pending and running differ in glyph, not in colour — both are simply not done"
    );
    assert_eq!(
        completed.fg,
        Some(ratatui::style::Color::from(context.palette().border_active)),
        "ordinary completed tools must use the readable accent, not success green"
    );
    assert_eq!(
        delegated.fg,
        Some(ratatui::style::Color::from(context.palette().secondary)),
        "delegated work must remain visually distinct from ordinary tools"
    );
}
