//! Picker tests: one off-screen assertion per picker, plus filter and paging.

use super::*;
use crate::app::render_offscreen;
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
