//! Question prompt tests, including the oracle wire-shape check.

use super::*;
use crate::app::render_offscreen;
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::KeyCode;

fn options() -> Vec<QuestionOption> {
    vec![
        QuestionOption::new("Rewrite", "Start from scratch"),
        QuestionOption::new("Patch", "Change the failing branch only"),
        QuestionOption::new("Skip", "Leave it as it is"),
    ]
}

fn prompt(request: QuestionRequest) -> QuestionPrompt {
    QuestionPrompt::new(ViewContext::defaults(), vec![request])
}

fn answered(step: DialogStep) -> Vec<Vec<String>> {
    match step {
        DialogStep::Resolved(DialogOutcome::Question(answers)) => answers,
        other => panic!("expected question answers, got {other:?}"),
    }
}

fn render(prompt: QuestionPrompt, width: u16, height: u16) -> Vec<String> {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context))),
    );
    host.open(Box::new(prompt));
    rows(&render_offscreen(&mut host, width, height).expect("infallible"))
}

// ---------------------------------------------------------------------------
// The wire shape is todo 43's
// ---------------------------------------------------------------------------

#[test]
fn views_question_deserializes_the_oracle_request_document() {
    // Exactly the shape `zuno-tools`'s `QuestionRequest` serializes. A field renamed
    // on either side breaks this, which is the point of duplicating the type rather
    // than depending on the tool crate.
    let document = r#"{
        "question": "How should the retry behave?",
        "header": "Retry policy",
        "options": [
            {"label": "Backoff", "description": "Exponential with jitter"},
            {"label": "Fail", "description": "Surface the error"}
        ],
        "multiple": false,
        "custom": false
    }"#;
    let request: QuestionRequest = serde_json::from_str(document).expect("the oracle shape parses");
    assert_eq!(request.header, "Retry policy");
    assert_eq!(request.options.len(), 2);
    assert_eq!(request.options[0].label, "Backoff");
    assert!(!request.is_multiple());
    assert!(
        !request.allows_custom(),
        "`custom: false` was ignored, so a closed question would offer a typed answer"
    );
}

#[test]
fn views_question_absent_flags_mean_single_select_with_a_typed_answer() {
    let request: QuestionRequest =
        serde_json::from_str(r#"{"question": "q", "header": "h", "options": []}"#).expect("parses");
    assert!(!request.is_multiple(), "absent `multiple` must mean single");
    assert!(
        request.allows_custom(),
        "absent `custom` must mean the client default, which is on"
    );
}

#[test]
fn views_question_serializes_without_the_absent_flags() {
    let request = QuestionRequest::new("q", "h", vec![QuestionOption::new("a", "b")]);
    let json = serde_json::to_string(&request).expect("serializes");
    assert!(
        !json.contains("multiple") && !json.contains("custom"),
        "an unset flag was written out as null: {json}"
    );
}

// ---------------------------------------------------------------------------
// The off-screen assertion
// ---------------------------------------------------------------------------

#[test]
fn views_question_prompt_renders_offscreen() {
    let joined = render(
        prompt(QuestionRequest::new(
            "The build fails on Windows only. How should it be fixed?",
            "Build fix",
            options(),
        )),
        56,
        16,
    )
    .join("\n");
    assert!(
        joined.contains("Build fix"),
        "the header is missing:\n{joined}"
    );
    assert!(
        joined.contains("The build fails on Windows only"),
        "the question text is missing:\n{joined}"
    );
    for label in ["Rewrite", "Patch", "Skip"] {
        assert!(
            joined.contains(label),
            "option {label:?} missing:\n{joined}"
        );
    }
    assert!(
        joined.contains("Start from scratch"),
        "an option description is missing, so the choice is unexplained:\n{joined}"
    );
    assert!(
        joined.contains("type your own answer"),
        "the typed-answer affordance is missing for an open question:\n{joined}"
    );
    assert!(
        joined.contains(UNANSWERED),
        "an unanswered question does not say so:\n{joined}"
    );
}

#[test]
fn views_question_closed_question_hides_the_typed_answer_row() {
    let mut request = QuestionRequest::new("Proceed?", "Plan", options());
    request.custom = Some(false);
    let joined = render(prompt(request), 50, 14).join("\n");
    assert!(
        !joined.contains("type your own answer"),
        "a closed question offered a typed answer:\n{joined}"
    );
}

#[test]
fn views_question_multiple_renders_checkboxes() {
    let mut request = QuestionRequest::new("Pick any", "Tags", options());
    request.multiple = Some(true);
    let joined = render(prompt(request), 50, 16).join("\n");
    assert!(
        joined.contains("[ ] Rewrite"),
        "a multi-select question has no checkboxes:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// Answering
// ---------------------------------------------------------------------------

#[test]
fn views_question_single_select_answers_with_one_label() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    let answers =
        answered(prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)));
    assert_eq!(answers, vec![vec![String::from("Patch")]]);
}

#[test]
fn views_question_multi_select_toggles_and_answers_with_every_label() {
    let mut request = QuestionRequest::new("q", "h", options());
    request.multiple = Some(true);
    let mut prompt = prompt(request);
    prompt.handle_action(action("dialog.mcp.toggle"), &press(KeyCode::Char(' ')));
    prompt.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    prompt.handle_action(action("dialog.mcp.toggle"), &press(KeyCode::Char(' ')));
    // Toggling twice deselects, which is what makes a checkbox a checkbox.
    prompt.handle_action(action("dialog.mcp.toggle"), &press(KeyCode::Char(' ')));
    prompt.handle_action(action("dialog.mcp.toggle"), &press(KeyCode::Char(' ')));
    let answers =
        answered(prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)));
    assert_eq!(
        answers,
        vec![vec![String::from("Rewrite"), String::from("Patch")]]
    );
}

#[test]
fn views_question_space_toggles_even_without_the_mcp_binding() {
    // `space` reaches a dialog as the `dialog.mcp.toggle` row, but a user who
    // rebound that row still expects space to toggle. The raw-key fallback covers it.
    let mut request = QuestionRequest::new("q", "h", options());
    request.multiple = Some(true);
    let mut prompt = prompt(request);
    prompt.handle_action(action("messages_next"), &press(KeyCode::Char(' ')));
    let answers =
        answered(prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)));
    assert_eq!(answers, vec![vec![String::from("Rewrite")]]);
}

#[test]
fn views_question_typed_answer_replaces_the_options() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    assert_eq!(
        prompt.cursor(),
        3,
        "the typed row is after the three options"
    );
    // The first submit enters the typed row rather than answering with an empty
    // string; that is what stops a stray enter from submitting nothing.
    prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    assert!(prompt.is_editing());
    for character in "revert it".chars() {
        prompt.handle_action(action("messages_next"), &press(KeyCode::Char(character)));
    }
    let answers =
        answered(prompt.handle_action(action("dialog.prompt.submit"), &press(KeyCode::Enter)));
    assert_eq!(answers, vec![vec![String::from("revert it")]]);
}

#[test]
fn views_question_newline_action_inserts_a_newline_and_submit_action_submits() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    for character in "first line".chars() {
        prompt.handle_action(action("messages_next"), &press(KeyCode::Char(character)));
    }

    assert_eq!(
        prompt.handle_action(action("input_newline"), &press(KeyCode::Enter)),
        DialogStep::Redraw
    );
    for character in "second line".chars() {
        prompt.handle_action(action("messages_next"), &press(KeyCode::Char(character)));
    }

    let rendered = render(prompt, 40, 16).join("\n");
    assert!(rendered.contains("first line"), "{rendered}");
    assert!(rendered.contains("second line"), "{rendered}");
}

#[test]
fn views_question_multiline_answer_preserves_the_newline_on_submit() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    for character in "first".chars() {
        prompt.handle_action(action("messages_next"), &press(KeyCode::Char(character)));
    }
    prompt.handle_action(action("input_newline"), &press(KeyCode::Enter));
    for character in "second".chars() {
        prompt.handle_action(action("messages_next"), &press(KeyCode::Char(character)));
    }

    let answers =
        answered(prompt.handle_action(action("dialog.prompt.submit"), &press(KeyCode::Enter)));
    assert_eq!(answers, vec![vec![String::from("first\nsecond")]]);
}

#[test]
fn views_question_escape_cancels_even_while_the_custom_answer_is_being_edited() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    prompt.handle_action(action("messages_next"), &press(KeyCode::Char('x')));
    let step = prompt.handle_action(action("session_interrupt"), &press(KeyCode::Esc));
    assert_eq!(step, DialogStep::Resolved(DialogOutcome::Cancelled));
}

#[test]
fn views_question_escape_cancels_instead_of_fabricating_an_unanswered_reply() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    assert_eq!(
        prompt.handle_action(action("session_interrupt"), &press(KeyCode::Esc)),
        DialogStep::Resolved(DialogOutcome::Cancelled)
    );
}

#[test]
fn views_question_several_questions_are_asked_in_order() {
    let mut prompt = QuestionPrompt::new(
        ViewContext::defaults(),
        vec![
            QuestionRequest::new("first?", "One", options()),
            QuestionRequest::new("second?", "Two", options()),
        ],
    );
    assert_eq!(prompt.current(), 0);
    let step = prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    assert_eq!(
        step,
        DialogStep::Redraw,
        "the first answer resolved the whole prompt instead of advancing"
    );
    assert_eq!(prompt.current(), 1);
    prompt.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    prompt.handle_action(action("dialog.select.prev"), &press(KeyCode::Up));
    let answers =
        answered(prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)));
    assert_eq!(
        answers,
        vec![vec![String::from("Rewrite")], vec![String::from("Skip")]]
    );
}

#[test]
fn views_question_title_counts_the_questions() {
    let mut prompt = QuestionPrompt::new(
        ViewContext::defaults(),
        vec![
            QuestionRequest::new("a", "Alpha", options()),
            QuestionRequest::new("b", "Beta", options()),
        ],
    );
    assert_eq!(prompt.title(), "Alpha (1/2)");
    prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    assert_eq!(prompt.title(), "Beta (2/2)");
}

#[test]
fn views_question_cursor_wraps_across_the_typed_row() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(action("dialog.select.prev"), &press(KeyCode::Up));
    assert_eq!(
        prompt.cursor(),
        3,
        "moving up from the first option did not wrap to the typed row"
    );
    prompt.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    assert_eq!(prompt.cursor(), 0);
}

#[test]
fn views_question_render_answer_joins_several_labels() {
    assert_eq!(
        render_answer(&[String::from("a"), String::from("b")]),
        "a, b"
    );
    assert_eq!(render_answer(&[]), UNANSWERED);
}

#[test]
fn views_question_hints_change_for_a_multi_select() {
    let single = prompt(QuestionRequest::new("q", "h", options()));
    assert!(
        !single.hints().iter().any(|(key, _)| *key == "space"),
        "a single-select question offered a toggle key"
    );
    let mut request = QuestionRequest::new("q", "h", options());
    request.multiple = Some(true);
    let multiple = prompt(request);
    assert!(multiple.hints().iter().any(|(key, _)| *key == "space"));
}

#[test]
fn views_question_typed_answer_hints_explain_newline_and_submit() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));

    assert!(prompt.hints().contains(&("shift+enter", "newline")));
    assert!(prompt.hints().contains(&("enter", "submit")));
}
