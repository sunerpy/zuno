//! Picker tests: one off-screen assertion per picker, plus filter and paging.

use super::*;
use crate::app::render_offscreen;
use crate::keybind::ActionComponent;
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::KeyCode;

fn render(dialog: SelectDialog, width: u16, height: u16) -> Vec<String> {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context))),
    );
    host.open(Box::new(dialog));
    rows(&render_offscreen(&mut host, width, height).expect("infallible"))
}

fn sessions() -> Vec<SessionEntry> {
    vec![
        SessionEntry {
            id: String::from("ses_1"),
            title: String::from("Port the keybind table"),
            when: String::from("2 hours ago"),
        },
        SessionEntry {
            id: String::from("ses_2"),
            title: String::from("Theme resolution"),
            when: String::from("yesterday"),
        },
    ]
}

fn models() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            id: String::from("anthropic/claude-x"),
            name: String::from("Claude X"),
            provider: String::from("Anthropic"),
        },
        ModelEntry {
            id: String::from("openai/gpt-y"),
            name: String::from("GPT Y"),
            provider: String::from("OpenAI"),
        },
    ]
}

fn agents() -> Vec<AgentEntry> {
    vec![
        AgentEntry {
            name: String::from("build"),
            description: String::from("The default agent"),
        },
        AgentEntry {
            name: String::from("explore"),
            description: String::from("Read-only investigation"),
        },
    ]
}

fn selected(step: DialogStep) -> String {
    match step {
        DialogStep::Resolved(DialogOutcome::Selected { value, .. }) => value,
        other => panic!("expected a selection, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// One off-screen assertion per picker
// ---------------------------------------------------------------------------

#[test]
fn views_session_picker_renders_offscreen() {
    let joined = render(session_picker(ViewContext::defaults(), sessions()), 56, 10).join("\n");
    assert!(joined.contains("Sessions (2)"), "{joined}");
    assert!(
        joined.contains("Port the keybind table"),
        "a session title is missing:\n{joined}"
    );
    assert!(
        joined.contains("2 hours ago"),
        "the session age is missing, so the list cannot be ordered by eye:\n{joined}"
    );
    assert!(
        joined.contains("> Port"),
        "the cursor marker is missing:\n{joined}"
    );
}

#[test]
fn views_model_picker_renders_offscreen() {
    let joined = render(model_picker(ViewContext::defaults(), models()), 56, 10).join("\n");
    assert!(joined.contains("Models (2)"), "{joined}");
    assert!(
        joined.contains("Claude X") && joined.contains("Anthropic"),
        "{joined}"
    );
}

#[test]
fn views_agent_picker_renders_offscreen() {
    let joined = render(agent_picker(ViewContext::defaults(), agents()), 56, 10).join("\n");
    assert!(joined.contains("Agents (2)"), "{joined}");
    assert!(
        joined.contains("explore") && joined.contains("Read-only investigation"),
        "{joined}"
    );
}

fn mcp_servers() -> Vec<McpServer> {
    vec![
        McpServer {
            name: "context7".to_owned(),
            state: McpState::Connected,
            desired_enabled: true,
        },
        McpServer {
            name: "codegraph".to_owned(),
            state: McpState::Failed("handshake failed".to_owned()),
            desired_enabled: false,
        },
    ]
}

#[test]
fn views_mcp_dialog_renders_live_states_and_toggle_hint() {
    let dialog = mcp_list(ViewContext::defaults(), McpProjection::new(mcp_servers()));
    let joined = render_mcp(dialog, 72, 10).join("\n");
    assert!(
        joined.contains("context7") && joined.contains("Connected"),
        "{joined}"
    );
    assert!(
        joined.contains("codegraph") && joined.contains("handshake failed"),
        "{joined}"
    );
    assert!(
        joined.contains("space") && joined.contains("toggle"),
        "{joined}"
    );
}

fn render_mcp(dialog: McpDialog, width: u16, height: u16) -> Vec<String> {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context))),
    );
    host.open(Box::new(dialog));
    rows(&render_offscreen(&mut host, width, height).expect("infallible"))
}

#[test]
fn views_mcp_space_emits_explicit_target_and_keeps_dialog_open() {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context.clone()))),
    );
    host.open(Box::new(mcp_list(
        context,
        McpProjection::new(mcp_servers()),
    )));

    let result = host.handle_action(action("dialog.mcp.toggle"), &press(KeyCode::Char(' ')));

    assert!(result.redraw);
    assert_eq!(host.active(), Some(MCP_DIALOG_ID));
    assert_eq!(
        host.drain_outcomes(),
        vec![(
            MCP_DIALOG_ID,
            DialogOutcome::McpToggle(McpToggleRequest {
                server: "codegraph".to_owned(),
                desired_enabled: true,
            })
        )]
    );
}

#[test]
fn views_mcp_dialog_reads_replaced_projection_without_reopening() {
    let projection = McpProjection::new(vec![McpServer {
        name: "context7".to_owned(),
        state: McpState::Connecting,
        desired_enabled: true,
    }]);
    let mut dialog = mcp_list(ViewContext::defaults(), projection.clone());
    assert!(dialog.lines(60)[0].to_string().contains("Connecting"));

    projection.replace(vec![McpServer {
        name: "context7".to_owned(),
        state: McpState::Connected,
        desired_enabled: true,
    }]);

    assert!(dialog.lines(60)[0].to_string().contains("Connected"));
}

#[test]
fn views_theme_picker_renders_offscreen_with_a_palette_preview() {
    let registry = ThemeRegistry::new();
    let dialog = theme_picker(ViewContext::defaults(), &registry, Mode::Dark);
    let joined = render(dialog, 60, 20).join("\n");
    assert!(
        joined.contains(&format!("Themes ({})", registry.names().len())),
        "the theme count is wrong:\n{joined}"
    );
    assert!(
        joined.contains(crate::theme::DEFAULT_THEME),
        "the default theme is not listed:\n{joined}"
    );
    assert!(
        joined.contains("primary") && joined.contains("accent"),
        "the palette preview is missing, so a theme name is all the user sees:\n{joined}"
    );
}

#[test]
fn views_theme_picker_preview_paints_the_theme_it_previews() {
    let registry = ThemeRegistry::new();
    let context = ViewContext::defaults();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, Mode::Dark);
    let lines = preview_lines(&resolved, &context);
    assert_eq!(lines.len(), 2, "the preview lost a row");
    let swatch_backgrounds = lines[1]
        .spans
        .iter()
        .filter_map(|span| span.style.bg)
        .collect::<Vec<_>>();
    assert!(
        swatch_backgrounds.contains(&ratatui::style::Color::from(resolved.palette.primary)),
        "the preview does not paint the theme's own primary colour"
    );
    assert_eq!(
        swatch_backgrounds.len(),
        6,
        "the preview shows a different number of swatches than it documents"
    );
}

#[test]
fn views_theme_picker_starts_on_the_active_theme() {
    let registry = ThemeRegistry::new();
    let dialog = theme_picker(ViewContext::defaults(), &registry, Mode::Dark);
    assert_eq!(
        dialog.selected().map(|item| item.value.clone()),
        Some(crate::theme::DEFAULT_THEME.to_owned()),
        "the picker opened on a theme other than the active one"
    );
}

#[test]
fn views_theme_picker_opening_paints_nothing_new() {
    // The cursor starts on the theme already showing, so the hook must not fire on the
    // way in. Guards the builder order inside `theme_picker`: attaching the highlight
    // before `selecting` would announce the first row in the list instead.
    let context = ViewContext::defaults();
    let registry = ThemeRegistry::new();
    let before = context.theme();
    let _dialog = theme_picker(context.clone(), &registry, Mode::Dark);
    assert_eq!(
        context.theme().name,
        before.name,
        "merely opening the theme picker changed the theme"
    );
}

#[test]
fn views_theme_picker_moving_the_cursor_repaints_the_shared_context() {
    // The behaviour the whole change exists for: the choice is applied as the cursor
    // arrives, not on submit, so a user judges a theme by the screen rather than by six
    // swatches. Asserted against the context the *caller* still holds, which is the part
    // that proves the switch is not confined to the dialog's own copy.
    let context = ViewContext::defaults();
    let registry = ThemeRegistry::new();
    let mut dialog = theme_picker(context.clone(), &registry, Mode::Dark);
    let before = context.theme();

    assert_eq!(
        dialog.handle_action(action("dialog.select.next"), &press(KeyCode::Down)),
        DialogStep::Redraw
    );

    let moved = dialog
        .selected()
        .expect("the picker has 33 rows")
        .value
        .clone();
    assert_ne!(moved, before.name, "the cursor did not move");
    let after = context.theme();
    assert_eq!(
        after.name, moved,
        "the highlighted theme was not applied to the context every view reads"
    );
    assert_eq!(
        after.palette,
        registry.resolve(&moved, Mode::Dark).palette,
        "the applied palette is not the highlighted theme's"
    );
}

#[test]
fn views_theme_picker_filtering_to_a_theme_applies_it() {
    // The second way the selection changes. `set_filter` re-ranks, so the cursor index
    // can stay put while a different row lands under it — which is why the hook compares
    // the selected *value* rather than the index.
    let context = ViewContext::defaults();
    let registry = ThemeRegistry::new();
    let mut dialog = theme_picker(context.clone(), &registry, Mode::Dark);
    for character in "gruvbox".chars() {
        assert_eq!(
            dialog.handle_typed(&press(KeyCode::Char(character))),
            DialogStep::Redraw
        );
    }
    assert_eq!(
        dialog.selected().map(|item| item.value.clone()),
        Some(String::from("gruvbox")),
        "typing the theme's name did not select it"
    );
    assert_eq!(
        context.theme().name,
        "gruvbox",
        "a filter that changed the highlighted row did not repaint"
    );
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

#[test]
fn views_picker_filter_narrows_by_typing_and_widens_on_backspace() {
    let mut dialog = agent_picker(ViewContext::defaults(), agents());
    assert_eq!(dialog.visible().len(), 2);
    for character in "expl".chars() {
        dialog.handle_action(action("messages_next"), &press(KeyCode::Char(character)));
    }
    assert_eq!(dialog.filter(), "expl");
    assert_eq!(dialog.visible().len(), 1);
    assert_eq!(dialog.visible()[0].label, "explore");

    dialog.handle_action(action("input_backspace"), &press(KeyCode::Backspace));
    assert_eq!(dialog.filter(), "exp");
    dialog.set_filter("");
    assert_eq!(dialog.visible().len(), 2);
}

#[test]
fn views_picker_filter_searches_the_description_too() {
    let mut dialog = agent_picker(ViewContext::defaults(), agents());
    dialog.set_filter("investigation");
    assert_eq!(
        dialog.visible().len(),
        1,
        "the description was not searched, so a user who knows what a thing does cannot find it"
    );
    assert_eq!(dialog.visible()[0].label, "explore");
}

#[test]
fn views_picker_filter_with_no_matches_says_so_offscreen() {
    let mut dialog = agent_picker(ViewContext::defaults(), agents());
    dialog.set_filter("zzzzz");
    assert!(dialog.visible().is_empty());
    let joined = render(dialog, 40, 6).join("\n");
    assert!(joined.contains("no matches"), "{joined}");
}

#[test]
fn views_picker_submitting_with_no_matches_is_ignored() {
    let mut dialog = agent_picker(ViewContext::defaults(), agents());
    dialog.set_filter("zzzzz");
    assert_eq!(
        dialog.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)),
        DialogStep::Ignored,
        "an empty list resolved to something"
    );
}

#[test]
fn views_picker_title_shows_the_filter() {
    let mut dialog = agent_picker(ViewContext::defaults(), agents());
    assert_eq!(dialog.title(), "Agents (2)");
    dialog.set_filter("bu");
    assert_eq!(dialog.title(), "Agents (1) — bu");
}

// ---------------------------------------------------------------------------
// Movement and selection
// ---------------------------------------------------------------------------

#[test]
fn views_picker_reports_the_value_not_the_label() {
    // A session's title is not its id, and a model's name is not `provider/model`.
    let mut dialog = session_picker(ViewContext::defaults(), sessions());
    dialog.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    let value =
        selected(dialog.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)));
    assert_eq!(value, "ses_2");

    let mut models = model_picker(ViewContext::defaults(), models());
    let value =
        selected(models.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)));
    assert_eq!(
        value, "anthropic/claude-x",
        "the model picker reported a bare model id, which the model policy treats as unavailable"
    );
}

#[test]
fn views_picker_cursor_wraps() {
    let mut dialog = agent_picker(ViewContext::defaults(), agents());
    dialog.handle_action(action("dialog.select.prev"), &press(KeyCode::Up));
    assert_eq!(dialog.cursor(), 1, "moving up from the top did not wrap");
    dialog.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    assert_eq!(dialog.cursor(), 0);
}

#[test]
fn views_picker_home_end_and_paging() {
    let items = (0..40)
        .map(|index| Item::new(format!("item {index:02}")))
        .collect::<Vec<_>>();
    let mut dialog =
        SelectDialog::new("probe", "Items", ViewContext::defaults(), items).with_rows(10);
    dialog.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    assert_eq!(dialog.cursor(), 39);
    dialog.handle_action(action("dialog.select.home"), &press(KeyCode::Home));
    assert_eq!(dialog.cursor(), 0);
    dialog.handle_action(action("dialog.select.page_down"), &press(KeyCode::PageDown));
    assert_eq!(dialog.cursor(), 10);
    dialog.handle_action(action("dialog.select.page_up"), &press(KeyCode::PageUp));
    assert_eq!(dialog.cursor(), 0);
}

#[test]
fn views_picker_window_follows_the_cursor_offscreen() {
    let items = (0..40)
        .map(|index| Item::new(format!("item {index:02}")))
        .collect::<Vec<_>>();
    let mut dialog =
        SelectDialog::new("probe", "Items", ViewContext::defaults(), items).with_rows(5);
    dialog.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    let joined = render(dialog, 30, 8).join("\n");
    assert!(
        joined.contains("item 39"),
        "the cursor scrolled out of the window:\n{joined}"
    );
    assert!(
        !joined.contains("item 00"),
        "the window did not scroll:\n{joined}"
    );
}

#[test]
fn views_picker_escape_cancels() {
    let mut dialog = agent_picker(ViewContext::defaults(), agents());
    assert_eq!(
        dialog.handle_action(action("app_exit"), &press(KeyCode::Esc)),
        DialogStep::Resolved(DialogOutcome::Cancelled)
    );
}

#[test]
fn views_picker_selecting_positions_the_cursor() {
    let dialog = session_picker(ViewContext::defaults(), sessions()).selecting("ses_2");
    assert_eq!(dialog.cursor(), 1);
    let missing = session_picker(ViewContext::defaults(), sessions()).selecting("nope");
    assert_eq!(
        missing.cursor(),
        0,
        "selecting a value that is not present moved the cursor somewhere"
    );
}

#[test]
fn views_picker_hints_name_the_paging_keys() {
    let dialog = agent_picker(ViewContext::defaults(), agents());
    assert!(dialog.hints().iter().any(|(key, _)| key.contains("pg")));
}

#[test]
fn views_picker_ids_are_distinct() {
    let ids = [
        SESSION_DIALOG_ID,
        MODEL_DIALOG_ID,
        AGENT_DIALOG_ID,
        THEME_DIALOG_ID,
        MCP_DIALOG_ID,
    ];
    let unique = ids.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        ids.len(),
        "two pickers share a dialog id, so an outcome cannot be routed"
    );
}

#[test]
fn views_picker_escape_cancels_because_its_footer_says_it_does() {
    // Observed on a real terminal: the footer read `esc cancel` and escape did nothing,
    // so the only way out of a picker was to choose something. `escape` resolves to
    // `session_interrupt`, which the dialog previously ignored and the host absorbed.
    let mut picker = model_picker(
        ViewContext::defaults(),
        vec![ModelEntry {
            id: String::from("prov/one"),
            name: String::from("one"),
            provider: String::from("prov"),
        }],
    );
    assert!(
        picker.hints().iter().any(|(key, _)| *key == "esc"),
        "the footer no longer advertises escape, so this test guards nothing"
    );
    assert_eq!(
        picker.handle_action(
            crate::views::testkit::action("session_interrupt"),
            &crate::views::testkit::press(crossterm::event::KeyCode::Esc),
        ),
        DialogStep::Resolved(DialogOutcome::Cancelled),
        "escape did not cancel the picker"
    );
}

#[test]
fn picker_finds_a_model_by_the_id_the_engine_takes_not_only_by_its_display_name() {
    // Measured: filtering the live model picker for `haiku-4-5` — the form `--model` and
    // the config file use — reported `Models (0) — haiku-4-5` while the model was present
    // under the label `Claude Haiku 4.5`.
    let mut dialog = model_picker(
        ViewContext::defaults(),
        vec![
            ModelEntry {
                id: String::from("myopenai/global.anthropic.claude-haiku-4-5-20251001-v1:0"),
                name: String::from("Claude Haiku 4.5"),
                provider: String::from("myopenai"),
            },
            ModelEntry {
                id: String::from("amazon-bedrock/amazon.nova-2-lite-v1:0"),
                name: String::from("Nova 2 Lite"),
                provider: String::from("amazon-bedrock"),
            },
        ],
    );
    dialog.set_filter("haiku-4-5");
    let found = dialog.selected().expect("the id form matched nothing");
    assert_eq!(
        found.value,
        "myopenai/global.anthropic.claude-haiku-4-5-20251001-v1:0"
    );
    // The display name still works, and still wins when both could match.
    dialog.set_filter("Nova 2");
    assert_eq!(
        dialog.selected().expect("the label matched nothing").value,
        "amazon-bedrock/amazon.nova-2-lite-v1:0"
    );
}

#[test]
fn picker_ranks_a_label_match_above_a_value_match() {
    // Otherwise a query that appears in one model's id and another's name would surface
    // the id match first, which is the less likely intent.
    let mut dialog = model_picker(
        ViewContext::defaults(),
        vec![
            ModelEntry {
                id: String::from("p/contains-sonnet-inside-the-id"),
                name: String::from("Something Else"),
                provider: String::from("p"),
            },
            ModelEntry {
                id: String::from("p/anthropic.x"),
                name: String::from("Sonnet"),
                provider: String::from("p"),
            },
        ],
    );
    dialog.set_filter("sonnet");
    assert_eq!(
        dialog.selected().expect("nothing matched").label,
        "Sonnet",
        "a value match outranked a label match"
    );
}

// ---------------------------------------------------------------------------
// Grouping: the model picker states its provider once per run
// ---------------------------------------------------------------------------

fn two_providers() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            id: String::from("openai/gpt-5-mini"),
            name: String::from("gpt-5-mini"),
            provider: String::from("openai"),
        },
        ModelEntry {
            id: String::from("amazon-bedrock/anthropic.claude-opus-4-6-v1"),
            name: String::from("anthropic.claude-opus-4-6-v1"),
            provider: String::from("amazon-bedrock"),
        },
        ModelEntry {
            id: String::from("openai/gpt-5-codex"),
            name: String::from("gpt-5-codex"),
            provider: String::from("openai"),
        },
        ModelEntry {
            id: String::from("amazon-bedrock/amazon.nova-lite-v1:0"),
            name: String::from("amazon.nova-lite-v1:0"),
            provider: String::from("amazon-bedrock"),
        },
    ]
}

/// Each provider is named once, above its own contiguous run of models.
#[test]
fn the_model_picker_groups_by_provider_and_sorts_names_inside_a_group() {
    let dialog = model_picker(ViewContext::defaults(), two_providers()).with_rows(10);
    let rendered = render(dialog, 88, 16);
    let body: Vec<&String> = rendered
        .iter()
        .filter(|row| !row.trim().is_empty())
        .collect();

    let joined = body
        .iter()
        .map(|row| row.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    // Named once, which is the complaint: the flat list repeated the provider on all 114 rows.
    assert_eq!(
        joined.matches("amazon-bedrock").count(),
        1,
        "the provider is still repeated per row:\n{joined}"
    );
    assert_eq!(
        joined.matches("openai").count(),
        1,
        "the provider is still repeated per row:\n{joined}"
    );

    let position = |needle: &str| {
        body.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle} is not on screen:\n{joined}"))
    };
    // The heading introduces its run, and the run is alphabetical inside it.
    assert!(
        position("amazon-bedrock") < position("amazon.nova-lite-v1:0"),
        "a provider's models render above its heading:\n{joined}"
    );
    assert!(
        position("amazon.nova-lite-v1:0") < position("anthropic.claude-opus-4-6-v1"),
        "models are not name-sorted inside their provider:\n{joined}"
    );
    assert!(
        position("anthropic.claude-opus-4-6-v1") < position("openai"),
        "the groups are interleaved rather than contiguous:\n{joined}"
    );
    assert!(
        position("gpt-5-codex") < position("gpt-5-mini"),
        "models are not name-sorted inside their provider:\n{joined}"
    );
}

/// A heading is not a row the cursor can reach, whatever it is asked to do.
#[test]
fn a_group_heading_is_never_selectable() {
    let mut dialog = model_picker(ViewContext::defaults(), two_providers());
    let providers = ["openai", "amazon-bedrock"];

    // Every navigation action, from every position, and the filter's re-ranking too: the cursor
    // indexes the filtered items and a heading is never among them, so there is no sequence of
    // keys that can land on one. Walking the whole list is what makes that a claim about the
    // design rather than about one lucky starting point.
    let steps = [
        "dialog.select.next",
        "dialog.select.prev",
        "dialog.select.page_down",
        "dialog.select.page_up",
        "dialog.select.home",
        "dialog.select.end",
    ];
    for step in steps {
        for _ in 0..(two_providers().len() + 2) {
            dialog.handle_action(action(step), &press(KeyCode::Down));
            let selected = dialog
                .selected()
                .expect("a non-empty list always has a selection");
            assert!(
                !providers.contains(&selected.label.as_str()),
                "`{step}` put the cursor on the `{}` heading",
                selected.label
            );
            assert!(
                selected.value.contains('/'),
                "`{step}` selected something that is not a `provider/model` value: {selected:?}"
            );
        }
    }

    // And submitting can only ever answer with a model.
    dialog.set_filter("amazon");
    let step = dialog.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    match step {
        DialogStep::Resolved(DialogOutcome::Selected { value, .. }) => assert!(
            value.starts_with("amazon-bedrock/"),
            "submitting answered with {value:?}, which is not a model"
        ),
        other => panic!("submitting a filtered grouped list did not resolve: {other:?}"),
    }
}

/// A query that matches two providers keeps both headings, and each still leads its own run.
#[test]
fn filtering_across_providers_keeps_a_heading_over_every_run() {
    let mut dialog = model_picker(ViewContext::defaults(), two_providers()).with_rows(10);
    // `a` is in a model of both providers — `amazon.nova…` and `gpt-5-codex` — so the filter
    // spans them. A query matching only one provider would not test the interleaving question.
    dialog.set_filter("a");
    let visible: Vec<&Item> = dialog.visible();
    assert!(
        visible.iter().any(|item| item.group == "openai")
            && visible.iter().any(|item| item.group == "amazon-bedrock"),
        "the fixture query does not span both providers: {visible:?}"
    );

    let rendered = render(dialog, 88, 16);
    let body: Vec<&String> = rendered
        .iter()
        .filter(|row| !row.trim().is_empty())
        .collect();
    let joined = body
        .iter()
        .map(|row| row.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for provider in ["openai", "amazon-bedrock"] {
        assert_eq!(
            joined.matches(provider).count(),
            1,
            "under a filter the `{provider}` heading is missing or repeated:\n{joined}"
        );
    }

    // Contiguity under a filter is the property that makes the heading mean anything: a heading
    // followed by one row of its own and then a row of the other provider's is a mislabel.
    let mut seen: Vec<String> = Vec::new();
    for row in &body {
        let owner = ["openai", "amazon-bedrock"]
            .into_iter()
            .find(|provider| row.contains(provider));
        if let Some(owner) = owner {
            assert!(
                !seen.iter().any(|group| group == owner),
                "the `{owner}` group is split into two runs under a filter:\n{joined}"
            );
            seen.push(owner.to_owned());
        }
    }
    assert_eq!(
        seen.len(),
        2,
        "a heading went missing under a filter:\n{joined}"
    );
}

/// Typing a provider's name still finds its models after the provider left the rows.
#[test]
fn a_provider_name_is_still_searchable_once_it_is_only_a_heading() {
    let mut dialog = model_picker(ViewContext::defaults(), two_providers());
    dialog.set_filter("bedrock");
    let visible = dialog.visible();
    assert!(
        !visible.is_empty(),
        "searching the provider name found nothing once it moved into the heading"
    );
    assert!(
        visible.iter().all(|item| item.group == "amazon-bedrock"),
        "a provider query matched another provider's models: {visible:?}"
    );
}

/// The cursor stays inside the row budget even though headings spend rows.
#[test]
fn a_grouped_window_keeps_the_cursor_on_screen() {
    let mut dialog = model_picker(ViewContext::defaults(), two_providers()).with_rows(3);
    dialog.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    let selected = dialog
        .selected()
        .expect("a non-empty list has a selection")
        .clone();
    let rendered = render(dialog, 88, 10);
    let joined = rendered.join("\n");
    assert!(
        joined.contains(&selected.label),
        "the cursor is on a row the window does not show, so the arrows look dead:\n{joined}"
    );
}
