//! Question prompt tests, including the oracle wire-shape check.

use super::*;
use crate::app::{AppEvent, Component, EventResult, TerminalEvent, render_offscreen};
use crate::keybind::{ActionComponent, KeyDispatcher, Keymap};
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::sync::{Arc, Mutex};

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

fn deferred(step: DialogStep) -> Vec<Vec<String>> {
    match step {
        DialogStep::Resolved(DialogOutcome::QuestionDeferred(answers)) => answers,
        other => panic!("expected deferred question answers, got {other:?}"),
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
        joined.contains("Question 1/1 (1 unanswered) · Build fix"),
        "the progress header is missing:\n{joined}"
    );
    assert!(
        joined.contains("The build fails on Windows only"),
        "the question text is missing:\n{joined}"
    );
    for label in ["1. Rewrite", "2. Patch", "3. Skip"] {
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
        joined.contains("4. Other"),
        "the numbered custom-answer affordance is missing:\n{joined}"
    );
    assert!(
        !joined.lines().any(|line| line.trim() == UNANSWERED),
        "the old redundant unanswered body row is still rendered:\n{joined}"
    );
}

#[test]
fn views_question_closed_question_hides_the_typed_answer_row() {
    let mut request = QuestionRequest::new("Proceed?", "Plan", options());
    request.custom = Some(false);
    let joined = render(prompt(request), 50, 14).join("\n");
    assert!(
        !joined.contains("Other"),
        "a closed question offered a typed answer:\n{joined}"
    );
}

#[test]
fn views_question_multiple_renders_checkboxes() {
    let mut request = QuestionRequest::new("Pick any", "Tags", options());
    request.multiple = Some(true);
    let joined = render(prompt(request), 50, 16).join("\n");
    assert!(
        joined.contains("1. [ ] Rewrite"),
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
fn views_question_single_select_can_be_chosen_with_the_mouse() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    let body = Rect::new(10, 5, 40, 10);
    let step = prompt.handle_mouse(
        &MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 12,
            row: 9,
            modifiers: KeyModifiers::NONE,
        },
        body,
    );
    assert_eq!(answered(step), vec![vec![String::from("Patch")]]);
}

#[test]
fn views_question_number_keys_select_and_submit() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    let answers = answered(prompt.handle_typed(&press(KeyCode::Char('2'))));
    assert_eq!(answers, vec![vec![String::from("Patch")]]);
}

#[test]
fn views_question_vim_keys_move_the_option_cursor() {
    let mut prompt = prompt(QuestionRequest::new("q", "h", options()));
    prompt.handle_action(
        action("dialog.question.next_option"),
        &press(KeyCode::Char('j')),
    );
    assert_eq!(prompt.cursor(), 1);
    prompt.handle_action(
        action("dialog.question.prev_option"),
        &press(KeyCode::Char('k')),
    );
    assert_eq!(prompt.cursor(), 0);
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
    assert_eq!(prompt.title(), "Question 1/2 (2 unanswered) · Alpha");
    prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    assert_eq!(prompt.title(), "Question 2/2 (1 unanswered) · Beta");
}

#[test]
fn views_question_horizontal_navigation_preserves_each_questions_cursor() {
    let mut prompt = QuestionPrompt::new(
        ViewContext::defaults(),
        vec![
            QuestionRequest::new("a", "Alpha", options()),
            QuestionRequest::new("b", "Beta", options()),
        ],
    );
    prompt.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    assert_eq!(prompt.cursor(), 1);
    prompt.handle_action(
        action("dialog.question.next_question"),
        &press(KeyCode::Right),
    );
    assert_eq!(prompt.current(), 1);
    prompt.handle_action(action("dialog.select.end"), &press(KeyCode::End));
    assert_eq!(prompt.cursor(), 3);
    prompt.handle_action(
        action("dialog.question.prev_question"),
        &press(KeyCode::Left),
    );
    assert_eq!(prompt.current(), 0);
    assert_eq!(prompt.cursor(), 1);
    prompt.handle_action(
        action("dialog.question.next_question"),
        &press(KeyCode::Right),
    );
    assert_eq!(prompt.cursor(), 3);
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
    assert!(single.hints().contains(&("1-9", "choose")));
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

// ---------------------------------------------------------------------------
// Stored answers and explicit deferral
// ---------------------------------------------------------------------------

#[test]
fn views_question_prefilled_complete_answers_are_restored_before_explicit_submit() {
    let mut multiple = QuestionRequest::new("Which changes?", "Changes", options());
    multiple.multiple = Some(true);
    let answers = vec![
        vec![String::from("Patch")],
        vec![String::from("Rewrite"), String::from("Skip")],
    ];
    let mut prompt = QuestionPrompt::new(
        ViewContext::defaults(),
        vec![
            QuestionRequest::new("How?", "Approach", options()),
            multiple,
        ],
    )
    .with_answers(answers.clone());

    assert_eq!(prompt.current(), 0);
    assert_eq!(prompt.cursor(), 1);
    assert_eq!(prompt.answers(), answers);
    assert_eq!(prompt.title(), "Question 1/2 · Approach");
    assert_eq!(
        answered(prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter))),
        answers
    );
}

#[test]
fn views_question_partial_submit_preserves_skipped_slots_for_reopening() {
    let questions = vec![
        QuestionRequest::new("How?", "Approach", options()),
        QuestionRequest::new("Which?", "Change", options()),
    ];
    let mut prompt = QuestionPrompt::new(ViewContext::defaults(), questions.clone());
    prompt.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    prompt.handle_action(
        action("dialog.question.next_question"),
        &press(KeyCode::Right),
    );
    let answers = deferred(prompt.handle_typed(&press(KeyCode::Char('3'))));
    assert_eq!(answers, vec![vec![], vec![String::from("Skip")]]);

    let mut reopened =
        QuestionPrompt::new(ViewContext::defaults(), questions).with_answers(answers.clone());
    assert_eq!(reopened.current(), 0);
    assert_eq!(reopened.answers(), answers);
    assert_eq!(reopened.title(), "Question 1/2 (1 unanswered) · Approach");
    assert_eq!(
        answered(reopened.handle_typed(&press(KeyCode::Char('2')))),
        vec![vec![String::from("Patch")], vec![String::from("Skip")]]
    );
}

#[test]
fn views_question_defer_keeps_selected_answers_without_selecting_highlighted_choices() {
    let questions = vec![
        QuestionRequest::new("How?", "Approach", options()),
        QuestionRequest::new("Which?", "Change", options()),
    ];
    let answers = vec![vec![String::from("Skip")], vec![]];
    let mut prompt =
        QuestionPrompt::new(ViewContext::defaults(), questions).with_answers(answers.clone());
    assert_eq!(prompt.current(), 1);
    prompt.handle_action(
        action("dialog.question.prev_question"),
        &press(KeyCode::Left),
    );
    assert_eq!(prompt.cursor(), 2);
    prompt.handle_action(action("dialog.select.home"), &press(KeyCode::Home));
    prompt.handle_action(
        action("dialog.question.next_question"),
        &press(KeyCode::Right),
    );
    prompt.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    assert_eq!(
        deferred(prompt.handle_action(
            action("dialog.question.defer"),
            &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        )),
        answers
    );
}

#[test]
fn views_question_defer_is_distinct_even_with_no_answers_or_all_answers() {
    for answers in [vec![vec![]], vec![vec![String::from("Patch")]]] {
        let mut prompt =
            prompt(QuestionRequest::new("q", "h", options())).with_answers(answers.clone());
        assert_eq!(
            deferred(prompt.handle_action(
                action("dialog.question.defer"),
                &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            )),
            answers
        );
    }
}

#[test]
fn views_question_prefilled_multiple_answers_can_be_toggled_by_space_and_digit() {
    let mut request = QuestionRequest::new("q", "h", options());
    request.multiple = Some(true);
    let mut prompt =
        prompt(request).with_answers(vec![vec![String::from("Rewrite"), String::from("Patch")]]);
    assert_eq!(prompt.cursor(), 0);
    assert_eq!(
        prompt.handle_typed(&press(KeyCode::Char(' '))),
        DialogStep::Redraw
    );
    assert_eq!(
        prompt.handle_typed(&press(KeyCode::Char('3'))),
        DialogStep::Redraw
    );
    assert_eq!(
        answered(prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter))),
        vec![vec![String::from("Patch"), String::from("Skip")]]
    );
}

#[test]
fn views_question_clearing_a_reopened_custom_answer_keeps_it_unanswered() {
    for finish in ["dialog.question.defer", "dialog.prompt.submit"] {
        let mut prompt = prompt(QuestionRequest::new("q", "h", options()))
            .with_answers(vec![vec![String::from("old")]]);
        assert_eq!(prompt.cursor(), options().len());
        assert!(prompt.hints().contains(&("enter", "edit")));
        assert_eq!(
            prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)),
            DialogStep::Redraw
        );
        assert!(prompt.is_editing());
        for _ in 0..3 {
            prompt.handle_action(action("input_backspace"), &press(KeyCode::Backspace));
        }
        assert_eq!(
            deferred(prompt.handle_action(action(finish), &press(KeyCode::Null))),
            vec![Vec::<String>::new()],
            "{finish} restored the old answer after it was explicitly cleared"
        );
    }
}

#[test]
fn views_question_custom_answer_survives_navigation_and_partial_deferral() {
    let questions = vec![
        QuestionRequest::new("How?", "Approach", options()),
        QuestionRequest::new("Which?", "Change", options()),
    ];
    let mut prompt = QuestionPrompt::new(ViewContext::defaults(), questions.clone());
    prompt.handle_typed(&press(KeyCode::Char('4')));
    for character in "first".chars() {
        prompt.handle_typed(&press(KeyCode::Char(character)));
    }
    prompt.handle_action(action("input_newline"), &press(KeyCode::Enter));
    for character in "second".chars() {
        prompt.handle_typed(&press(KeyCode::Char(character)));
    }
    assert_eq!(
        prompt.handle_action(action("dialog.prompt.submit"), &press(KeyCode::Enter)),
        DialogStep::Redraw
    );
    assert_eq!(prompt.current(), 1);
    prompt.handle_action(
        action("dialog.question.prev_question"),
        &press(KeyCode::Left),
    );
    assert_eq!(prompt.current(), 0);
    assert!(!prompt.is_editing());
    prompt.handle_action(
        action("dialog.question.next_question"),
        &press(KeyCode::Right),
    );
    assert_eq!(prompt.current(), 1);
    let answers =
        deferred(prompt.handle_action(action("dialog.question.defer"), &press(KeyCode::Null)));
    assert_eq!(answers, vec![vec![String::from("first\nsecond")], vec![]]);

    let mut reopened =
        QuestionPrompt::new(ViewContext::defaults(), questions).with_answers(answers);
    assert_eq!(reopened.current(), 1);
    assert_eq!(
        answered(reopened.handle_typed(&press(KeyCode::Char('2')))),
        vec![
            vec![String::from("first\nsecond")],
            vec![String::from("Patch")],
        ]
    );
}

#[test]
fn views_question_stored_answer_slots_are_padded_or_truncated_positionally() {
    let questions = vec![
        QuestionRequest::new("How?", "Approach", options()),
        QuestionRequest::new("Which?", "Change", options()),
    ];
    let short = QuestionPrompt::new(ViewContext::defaults(), questions.clone())
        .with_answers(vec![vec![String::from("Skip")]]);
    assert_eq!(short.current(), 1);
    assert_eq!(short.answers(), &[vec![String::from("Skip")], vec![]]);

    let long = QuestionPrompt::new(ViewContext::defaults(), questions).with_answers(vec![
        vec![String::from("Skip")],
        vec![],
        vec![String::from("Rewrite")],
    ]);
    assert_eq!(long.answers(), short.answers());
}

fn plan_question() -> QuestionRequest {
    let mut question = QuestionRequest::new(
        "Start working on this plan?",
        "Plan",
        vec![
            QuestionOption::new("Yes", "Approve the plan"),
            QuestionOption::new("No", "Keep planning"),
        ],
    );
    question.custom = Some(false);
    question
}

#[test]
fn views_question_closed_confirmation_requires_an_explicit_approve_choice() {
    let mut prompt = prompt(plan_question());
    assert_eq!(prompt.answers(), &[Vec::<String>::new()]);
    assert_eq!(
        prompt.handle_typed(&press(KeyCode::Char('3'))),
        DialogStep::Ignored,
        "a closed confirmation offered a custom answer"
    );
    assert_eq!(
        answered(prompt.handle_typed(&press(KeyCode::Char('1')))),
        vec![vec![String::from("Yes")]]
    );
}

#[test]
fn views_question_closed_confirmation_does_not_restore_invalid_answers_as_approval() {
    for invalid in [
        vec![String::new()],
        vec![String::from(" \n ")],
        vec![String::from("unknown")],
        vec![String::from("Yes"), String::from("No")],
        vec![String::from("Yes"), String::from("unknown")],
    ] {
        let mut prompt = prompt(plan_question()).with_answers(vec![invalid]);
        assert_eq!(prompt.answers(), &[Vec::<String>::new()]);
        assert_eq!(
            deferred(prompt.handle_action(
                action("dialog.question.defer"),
                &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            )),
            vec![Vec::<String>::new()]
        );
    }
}

#[test]
fn views_question_closing_or_deferring_a_prefilled_approval_does_not_approve() {
    for finish in ["app_exit", "session_interrupt", "dialog.question.defer"] {
        let answers = vec![vec![String::from("Yes")]];
        let mut prompt = prompt(plan_question()).with_answers(answers.clone());
        let outcome = prompt.handle_action(action(finish), &press(KeyCode::Null));
        if finish == "dialog.question.defer" {
            assert_eq!(deferred(outcome), answers);
        } else {
            assert_eq!(outcome, DialogStep::Resolved(DialogOutcome::Cancelled));
        }
    }
}

#[test]
fn views_question_empty_requests_and_optionless_closed_questions_never_approve() {
    let mut empty = QuestionPrompt::new(ViewContext::defaults(), vec![])
        .with_answers(vec![vec![String::from("Yes")]]);
    assert_eq!(empty.title(), "Question (0 unanswered)");
    assert!(!empty.lines(40).is_empty());
    assert!(empty.hints().contains(&("ctrl+s", "answer later")));
    assert_eq!(
        deferred(empty.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter))),
        Vec::<Vec<String>>::new()
    );

    for options in [vec![], vec![QuestionOption::new(" \n ", "Blank label")]] {
        let mut request = plan_question();
        request.options = options;
        let mut prompt = prompt(request);
        assert_eq!(
            deferred(prompt.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter))),
            vec![Vec::<String>::new()]
        );
    }
}

#[test]
fn views_question_modified_digits_do_not_choose_or_submit_an_option() {
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        let mut prompt = prompt(plan_question());
        let key = KeyEvent::new(KeyCode::Char('1'), modifiers);
        assert_eq!(prompt.handle_typed(&key), DialogStep::Ignored);
        assert_eq!(
            prompt.handle_action(action("messages_next"), &key),
            DialogStep::Ignored
        );
        assert_eq!(prompt.answers(), &[Vec::<String>::new()]);
    }
}

#[test]
fn views_question_defer_hint_is_visible_in_existing_controls() {
    for width in [40, 56, 80] {
        let joined = render(prompt(plan_question()), width, 14).join("\n");
        assert!(
            joined.contains("ctrl+s") && joined.contains("answer later"),
            "the defer action is hidden at width {width}:\n{joined}"
        );
    }
}

#[derive(Default)]
struct QuestionObservations {
    outcomes: Vec<DialogOutcome>,
    base_actions: Vec<&'static str>,
}

struct QuestionObserver(Arc<Mutex<QuestionObservations>>);

impl Component for QuestionObserver {
    fn render(&mut self, _frame: &mut ratatui::Frame<'_>, _area: Rect) {}

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

impl ActionComponent for QuestionObserver {
    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> EventResult {
        self.0
            .lock()
            .expect("question observer")
            .base_actions
            .push(action.name);
        EventResult::IGNORED
    }

    fn apply_dialog_outcome(
        &mut self,
        dialog: &'static str,
        outcome: &DialogOutcome,
    ) -> EventResult {
        assert_eq!(dialog, DIALOG_ID);
        self.0
            .lock()
            .expect("question observer")
            .outcomes
            .push(outcome.clone());
        EventResult::REDRAW
    }
}

fn dispatched(prompt: QuestionPrompt) -> (KeyDispatcher, Arc<Mutex<QuestionObservations>>) {
    let observations = Arc::new(Mutex::new(QuestionObservations::default()));
    let mut host = DialogHost::new(
        ViewContext::defaults(),
        Box::new(QuestionObserver(Arc::clone(&observations))),
    );
    host.open(Box::new(prompt));
    (
        KeyDispatcher::new(
            Keymap::defaults().expect("conflict-free defaults"),
            vec![String::from("app")],
            Box::new(host),
        ),
        observations,
    )
}

fn send_key(dispatcher: &mut KeyDispatcher, key: KeyEvent) {
    dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
        CrosstermEvent::Key(key),
    )));
}

#[test]
fn views_question_dispatcher_defers_and_reopens_editable_multiline_custom_text() {
    let request = QuestionRequest::new("q", "h", options());
    let (mut dispatcher, observed) = dispatched(prompt(request.clone()));
    send_key(&mut dispatcher, press(KeyCode::Char('4')));
    for character in "hjkl123".chars() {
        send_key(&mut dispatcher, press(KeyCode::Char(character)));
    }
    send_key(
        &mut dispatcher,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
    );
    for character in "second!".chars() {
        send_key(&mut dispatcher, press(KeyCode::Char(character)));
    }
    send_key(&mut dispatcher, press(KeyCode::Backspace));
    send_key(
        &mut dispatcher,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
    );
    let answers = vec![vec![String::from("hjkl123\nsecond")]];
    {
        let observed = observed.lock().expect("question observer");
        assert_eq!(
            observed.outcomes,
            vec![DialogOutcome::QuestionDeferred(answers.clone())]
        );
        assert!(observed.base_actions.is_empty(), "defer armed interruption");
    }

    let reopened = prompt(request).with_answers(answers);
    assert_eq!(reopened.cursor(), 3);
    assert!(!reopened.is_editing());
    let (mut dispatcher, observed) = dispatched(reopened);
    let rendered = rows(&render_offscreen(&mut dispatcher, 80, 18).expect("infallible")).join("\n");
    assert!(rendered.contains("hjkl123") && rendered.contains("second"));
    send_key(&mut dispatcher, press(KeyCode::Enter));
    assert!(
        observed
            .lock()
            .expect("question observer")
            .outcomes
            .is_empty()
    );
    let rendered = rows(&render_offscreen(&mut dispatcher, 80, 18).expect("infallible")).join("\n");
    assert!(rendered.contains("ctrl+s") && rendered.contains("answer later"));
    assert!(rendered.contains("shift+enter") && rendered.contains("newline"));
    send_key(
        &mut dispatcher,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
    );
    for character in "third".chars() {
        send_key(&mut dispatcher, press(KeyCode::Char(character)));
    }
    send_key(&mut dispatcher, press(KeyCode::Enter));
    assert_eq!(
        observed.lock().expect("question observer").outcomes,
        vec![DialogOutcome::Question(vec![vec![String::from(
            "hjkl123\nsecond\nthird"
        )]])]
    );
}

#[test]
fn views_question_dispatcher_toggles_and_submits_multiple_choices() {
    let mut request = QuestionRequest::new("q", "h", options());
    request.multiple = Some(true);
    let (mut dispatcher, observed) = dispatched(prompt(request));
    for code in [KeyCode::Char(' '), KeyCode::Char('2'), KeyCode::Enter] {
        send_key(&mut dispatcher, press(code));
    }
    assert_eq!(
        observed.lock().expect("question observer").outcomes,
        vec![DialogOutcome::Question(vec![vec![
            String::from("Rewrite"),
            String::from("Patch"),
        ]])]
    );
}

#[test]
fn views_question_dispatcher_escape_still_closes_and_reaches_the_global_interrupt() {
    let (mut dispatcher, observed) = dispatched(prompt(plan_question()));
    send_key(&mut dispatcher, press(KeyCode::Esc));
    let observed = observed.lock().expect("question observer");
    assert_eq!(observed.outcomes, vec![DialogOutcome::Cancelled]);
    assert_eq!(observed.base_actions, vec!["session_interrupt"]);
}
