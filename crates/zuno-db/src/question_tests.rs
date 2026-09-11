use super::*;
use crate::{migration, session};
use zuno_paths::DbLocation;
use zuno_types::execution::TurnExecutionIdentity;

const SESSION: &str = "ses_question";

fn fixture() -> (Arc<Pool>, QuestionStore) {
    fixture_at(&DbLocation::Memory)
}

fn fixture_at(location: &DbLocation) -> (Arc<Pool>, QuestionStore) {
    let pool = Arc::new(Pool::open(location).expect("pool"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        connection
            .execute(
                "INSERT INTO project (id,worktree,time_created,time_updated,sandboxes) \
             VALUES ('project','/workspace',1,1,'[]')",
                [],
            )
            .expect("project");
    }
    pool.transaction(|tx| {
        session::create(
            tx,
            &session::SessionCreate::new(
                SESSION,
                "question",
                "project",
                "/workspace",
                "/workspace",
                "Questions",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("session");
    (Arc::clone(&pool), QuestionStore::new(pool))
}

fn spec() -> QuestionSpec {
    QuestionSpec {
        origin: QuestionOrigin {
            session_id: SESSION.to_owned(),
            message_id: None,
            call_id: None,
            turn_id: Some("turn-question".to_owned()),
            goal_id: None,
        },
        mode: QuestionMode::Deferred,
        purpose: QuestionPurpose::Clarification,
        questions: vec![
            QuestionRequest {
                question: "Which environment?".to_owned(),
                header: "Environment".to_owned(),
                options: Vec::new(),
                multiple: None,
                custom: None,
            },
            QuestionRequest {
                question: "Which terminal?".to_owned(),
                header: "Terminal".to_owned(),
                options: Vec::new(),
                multiple: None,
                custom: None,
            },
        ],
        expected_goal_revision: None,
        plan: None,
    }
}

fn create(pool: &Pool, spec: &QuestionSpec) -> QuestionView {
    pool.try_transaction(|tx| create_in(tx, "que_test", spec, 10))
        .expect("open")
        .question
}

fn command(id: &str, revision: i64, action: QuestionAction) -> QuestionCommand {
    QuestionCommand {
        command_id: id.to_owned(),
        expected_revision: revision,
        action,
    }
}

fn answer(id: &str, revision: i64, item: &str, value: &str) -> QuestionCommand {
    command(
        id,
        revision,
        QuestionAction::Answer {
            answers: BTreeMap::from([(item.to_owned(), vec![value.to_owned()])]),
        },
    )
}

fn inbox_count(pool: &Pool) -> usize {
    let connection = pool.get().expect("connection");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM session_input WHERE session_id=?1",
            [SESSION],
            |row| row.get(0),
        )
        .expect("input count");
    usize::try_from(count).expect("non-negative input count")
}

fn draft(entries: &[(&str, &[&str])]) -> QuestionAnswers {
    entries
        .iter()
        .map(|(id, values)| {
            (
                (*id).to_owned(),
                values.iter().map(|value| (*value).to_owned()).collect(),
            )
        })
        .collect()
}

fn input_prompt(pool: &Pool, receipt: &QuestionReceipt) -> Value {
    let connection = pool.get().expect("connection");
    let prompt: String = connection
        .query_row(
            "SELECT prompt FROM session_input WHERE id=?1",
            [receipt.input_id.as_ref().expect("input")],
            |row| row.get(0),
        )
        .expect("input");
    serde_json::from_str(&prompt).expect("prompt")
}

#[test]
fn full_draft_survives_reopen_and_only_explicit_answer_keys_are_committed() {
    let directory = tempfile::tempdir().expect("directory");
    let location = DbLocation::File(directory.path().join("questions.db"));
    let drafts = draft(&[
        ("q1", &["PRIVATE_DRAFT_ENV"]),
        ("q2", &["PRIVATE_DRAFT_SHELL"]),
    ]);
    let submitted = command(
        "save-draft",
        1,
        QuestionAction::Defer {
            draft_answers: drafts.clone(),
        },
    );
    let saved = {
        let (pool, store) = fixture_at(&location);
        let opened = create(&pool, &spec());
        let saved = store
            .apply(SESSION, &opened.id, &submitted, 20)
            .expect("save draft");
        assert_eq!(saved.question.state, QuestionState::Pending);
        assert_eq!(saved.question.mode, QuestionMode::Deferred);
        assert!(!saved.question.is_fully_answered());
        assert!(saved.question.answers.is_empty());
        assert_eq!(saved.question.draft_answers, drafts);
        assert!(saved.question.decision.is_none());
        assert!(saved.input_id.is_none());
        assert_eq!(inbox_count(&pool), 0);
        saved
    };
    let pool = Arc::new(Pool::open(&location).expect("reopen pool"));
    let store = QuestionStore::new(Arc::clone(&pool));
    assert_eq!(
        store.get(SESSION, &saved.question.id).expect("recovered"),
        saved.question
    );
    let retry = store
        .apply(SESSION, &saved.question.id, &submitted, 21)
        .expect("retry");
    assert!(retry.duplicate);
    assert_eq!(retry.input_id, saved.input_id);
    assert_eq!(inbox_count(&pool), 0);
    let empty = store
        .apply(
            SESSION,
            &saved.question.id,
            &command(
                "empty-answer",
                2,
                QuestionAction::Answer {
                    answers: QuestionAnswers::new(),
                },
            ),
            22,
        )
        .expect("empty answer");
    assert_eq!(empty.question.state, QuestionState::Pending);
    assert!(empty.question.answers.is_empty());
    assert_eq!(empty.question.draft_answers, drafts);
    assert!(empty.input_id.is_none());
    assert_eq!(inbox_count(&pool), 0);

    let first = store
        .apply(
            SESSION,
            &saved.question.id,
            &answer("first-explicit", 3, "q1", "CONFIRMED_ENV"),
            23,
        )
        .expect("explicit first answer");
    assert_eq!(first.question.state, QuestionState::Pending);
    assert_eq!(first.question.answers, draft(&[("q1", &["CONFIRMED_ENV"])]));
    assert_eq!(
        first.question.draft_answers,
        draft(&[("q2", &["PRIVATE_DRAFT_SHELL"])])
    );
    let prompt = input_prompt(&pool, &first).to_string();
    assert!(prompt.contains("CONFIRMED_ENV"));
    assert!(!prompt.contains("PRIVATE_DRAFT"));
    let last = store
        .apply(
            SESSION,
            &saved.question.id,
            &answer("last-explicit", 4, "q2", "CONFIRMED_SHELL"),
            24,
        )
        .expect("explicit last answer");
    assert_eq!(last.question.state, QuestionState::Answered);
    assert_eq!(
        last.question.answers,
        draft(&[("q1", &["CONFIRMED_ENV"]), ("q2", &["CONFIRMED_SHELL"])])
    );
    assert!(last.question.draft_answers.is_empty());
    assert_eq!(inbox_count(&pool), 2);
}

#[test]
fn draft_merges_preserve_confirmed_answers_and_explicit_blank_slots() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    store
        .apply(
            SESSION,
            &opened.id,
            &answer("confirmed", 1, "q1", "confirmed"),
            20,
        )
        .expect("confirm first");
    let edited = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "draft",
                2,
                QuestionAction::Defer {
                    draft_answers: draft(&[
                        ("q1", &["edited but unsubmitted"]),
                        ("q2", &["second draft"]),
                    ]),
                },
            ),
            21,
        )
        .expect("edit draft");
    assert_eq!(edited.question.answers, draft(&[("q1", &["confirmed"])]));
    let unchanged = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "later",
                3,
                QuestionAction::Defer {
                    draft_answers: QuestionAnswers::new(),
                },
            ),
            22,
        )
        .expect("empty map preserves draft");
    assert_eq!(
        unchanged.question.draft_answers,
        edited.question.draft_answers
    );
    let blank = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "blank",
                4,
                QuestionAction::Defer {
                    draft_answers: draft(&[("q1", &[])]),
                },
            ),
            23,
        )
        .expect("blank draft");
    assert_eq!(blank.question.answers, draft(&[("q1", &["confirmed"])]));
    assert_eq!(
        blank.question.draft_answers,
        draft(&[("q1", &[]), ("q2", &["second draft"])])
    );
    assert_eq!(blank.question.state, QuestionState::Pending);
    let cleared = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "explicit-clear",
                5,
                QuestionAction::Answer {
                    answers: draft(&[("q1", &[])]),
                },
            ),
            24,
        )
        .expect("explicit answer clears only that item");
    assert!(cleared.question.answers.is_empty());
    assert_eq!(
        cleared.question.draft_answers,
        draft(&[("q2", &["second draft"])])
    );
    assert!(cleared.input_id.is_none());
    assert_eq!(
        inbox_count(&pool),
        1,
        "only the original confirmed answer entered the model inbox"
    );
}

#[test]
fn invalid_drafts_and_reused_commands_cannot_change_the_stored_form() {
    let (pool, store) = fixture();
    let mut definition = spec();
    definition.questions[0] = QuestionRequest::closed(
        "Choose",
        "Choice",
        vec![
            zuno_types::question::QuestionOption::new("first", ""),
            zuno_types::question::QuestionOption::new("second", ""),
        ],
    );
    let opened = create(&pool, &definition);
    for values in [
        draft(&[("unknown", &["first"])]),
        draft(&[("q1", &["unoffered"])]),
        draft(&[("q1", &["first", "second"])]),
        draft(&[("q2", &[" "])]),
        draft(&[("q2", &["duplicate", "duplicate"])]),
    ] {
        assert!(matches!(
            store.apply(
                SESSION,
                &opened.id,
                &command(
                    "invalid",
                    1,
                    QuestionAction::Defer {
                        draft_answers: values
                    }
                ),
                20
            ),
            Err(QuestionError::Invalid(_))
        ));
        assert_eq!(store.get(SESSION, &opened.id).expect("unchanged"), opened);
        assert_eq!(inbox_count(&pool), 0);
    }
    let saved = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "save",
                1,
                QuestionAction::Defer {
                    draft_answers: draft(&[("q1", &["first"])]),
                },
            ),
            20,
        )
        .expect("valid draft");
    assert!(matches!(
        store.apply(
            SESSION,
            &opened.id,
            &command(
                "save",
                1,
                QuestionAction::Defer {
                    draft_answers: draft(&[("q1", &["second"])])
                }
            ),
            21
        ),
        Err(QuestionError::CommandConflict { .. })
    ));
    assert!(matches!(
        store.apply(
            SESSION,
            &opened.id,
            &command(
                "stale",
                1,
                QuestionAction::Defer {
                    draft_answers: draft(&[("q1", &["second"])])
                }
            ),
            21
        ),
        Err(QuestionError::Conflict { .. })
    ));
    assert_eq!(
        store.get(SESSION, &opened.id).expect("still saved"),
        saved.question
    );
    assert_eq!(inbox_count(&pool), 0);
}

#[test]
fn plan_drafts_never_become_decisions_or_model_visible_choices() {
    let (pool, store) = fixture();
    let mut definition = spec();
    definition.purpose = QuestionPurpose::PlanAuthorization;
    definition.questions = vec![QuestionRequest::closed(
        "Start Work?",
        "Plan",
        vec![
            zuno_types::question::QuestionOption::new("approve", ""),
            zuno_types::question::QuestionOption::new("decline", ""),
        ],
    )];
    definition.plan = Some(PlanQuestionBinding {
        plan_id: "plan-one".to_owned(),
        plan_revision: 1,
        source_cycle_id: Some("logical-plan-cycle".to_owned()),
        title: "Implement".to_owned(),
        completed_steps: 0,
        total_steps: 1,
        work_identity: TurnExecutionIdentity::new("build", "provider", "model"),
        review_gate: json!({"status": "unbound"}),
    });
    let opened = create(&pool, &definition);
    let saved = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "save-plan-draft",
                1,
                QuestionAction::Defer {
                    draft_answers: draft(&[("q1", &["approve"])]),
                },
            ),
            20,
        )
        .expect("save Plan draft");
    assert_eq!(saved.question.state, QuestionState::Pending);
    assert!(saved.question.answers.is_empty());
    assert!(saved.question.decision.is_none());
    assert!(saved.question.authorization.is_none());
    assert_eq!(saved.question.plan, opened.plan);
    assert_eq!(
        store.get(SESSION, &opened.id).expect("stored draft"),
        saved.question
    );
    assert!(saved.input_id.is_none());
    assert_eq!(inbox_count(&pool), 0);
    assert!(matches!(
        store.apply(
            SESSION,
            &opened.id,
            &command(
                "empty-plan-answer",
                2,
                QuestionAction::Answer {
                    answers: QuestionAnswers::new()
                }
            ),
            21
        ),
        Err(QuestionError::Invalid(_))
    ));
    assert_eq!(inbox_count(&pool), 0);
}

#[test]
fn a_failed_outer_transaction_rolls_back_drafts_receipts_and_input() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    let result: QuestionResult<()> = pool.try_transaction(|tx| {
        apply_in(
            tx,
            SESSION,
            &opened.id,
            &command(
                "rollback-draft",
                1,
                QuestionAction::Defer {
                    draft_answers: draft(&[("q1", &["must roll back"])]),
                },
            ),
            20,
        )?;
        Err(QuestionError::Invalid(
            "outer transaction failed".to_owned(),
        ))
    });
    assert!(result.is_err());
    assert_eq!(store.get(SESSION, &opened.id).expect("unchanged"), opened);
    assert_eq!(inbox_count(&pool), 0);
    assert_eq!(
        pool.get()
            .expect("connection")
            .query_row("SELECT COUNT(*) FROM question_action_receipt", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("receipts"),
        0
    );
}

#[test]
fn corrupt_stored_draft_ids_fail_closed_without_mutating_the_row() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    let response =
        json!({"answersById": {}, "draftAnswers": {"wrong-id": ["untrusted"]}}).to_string();
    let connection = pool.get().expect("connection");
    connection
        .execute(
            "UPDATE human_request SET response=?1 WHERE id=?2",
            params![response, opened.id],
        )
        .expect("corrupt fixture");
    assert!(matches!(
        store.get(SESSION, &opened.id),
        Err(QuestionError::Database(_))
    ));
    let preserved: String = connection
        .query_row(
            "SELECT response FROM human_request WHERE id=?1",
            [&opened.id],
            |row| row.get(0),
        )
        .expect("original bytes");
    assert_eq!(preserved, response);
    assert_eq!(inbox_count(&pool), 0);
}

#[test]
fn a_definition_round_trips_and_opening_does_not_answer_or_enqueue_input() {
    let (pool, store) = fixture();
    let view = create(&pool, &spec());
    assert_eq!(view, store.get(SESSION, &view.id).expect("read"));
    assert_eq!(view.questions[0].id, "q1");
    assert_eq!(view.questions[1].id, "q2");
    assert_eq!(view.state, QuestionState::Pending);
    assert!(view.answers.is_empty());
    assert_eq!(inbox_count(&pool), 0);
    assert_eq!(store.pending(SESSION).expect("pending"), vec![view]);
}

#[test]
fn partial_answers_and_defer_remain_answerable_by_stable_id() {
    let (pool, store) = fixture();
    let mut definition = spec();
    definition.mode = QuestionMode::Blocking;
    let opened = create(&pool, &definition);
    let first = store
        .apply(
            SESSION,
            &opened.id,
            &answer("first", 1, "q2", "PowerShell 7.6.6"),
            20,
        )
        .expect("answer second item");
    assert_eq!(first.question.state, QuestionState::Pending);
    assert_eq!(first.question.mode, QuestionMode::Deferred);
    assert!(!first.question.answers.contains_key("q1"));
    let deferred = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "later",
                2,
                QuestionAction::Defer {
                    draft_answers: BTreeMap::new(),
                },
            ),
            21,
        )
        .expect("defer");
    assert_eq!(deferred.question.state, QuestionState::Pending);
    let last = store
        .apply(SESSION, &opened.id, &answer("last", 3, "q1", "Windows"), 22)
        .expect("finish remaining item");
    assert_eq!(last.question.state, QuestionState::Answered);
    assert_eq!(last.question.answers["q2"], ["PowerShell 7.6.6"]);
    assert_eq!(last.question.answers["q1"], ["Windows"]);
    assert!(store.pending(SESSION).expect("pending").is_empty());
    assert_eq!(inbox_count(&pool), 2);
}

#[test]
fn an_empty_submit_defers_and_never_manufactures_an_answer() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    let receipt = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "empty",
                1,
                QuestionAction::Answer {
                    answers: BTreeMap::new(),
                },
            ),
            20,
        )
        .expect("empty submit");
    assert_eq!(receipt.question.state, QuestionState::Pending);
    assert_eq!(receipt.question.mode, QuestionMode::Deferred);
    assert!(receipt.question.answers.is_empty());
    assert!(receipt.input_id.is_none());
    assert_eq!(inbox_count(&pool), 0);
}

#[test]
fn retries_are_idempotent_and_conflicts_do_not_change_state() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    let submitted = answer("first", 1, "q1", "Windows");
    let receipt = store
        .apply(SESSION, &opened.id, &submitted, 20)
        .expect("answer");
    let repeated = store
        .apply(SESSION, &opened.id, &submitted, 21)
        .expect("retry");
    assert!(repeated.duplicate);
    assert_eq!(receipt.input_id, repeated.input_id);
    assert_eq!(inbox_count(&pool), 1);
    assert!(matches!(
        store.apply(SESSION, &opened.id, &answer("first", 1, "q1", "Linux"), 22),
        Err(QuestionError::CommandConflict { .. })
    ));
    assert!(matches!(
        store.apply(SESSION, &opened.id, &answer("stale", 1, "q2", "pwsh"), 23),
        Err(QuestionError::Conflict { .. })
    ));
    assert_eq!(
        store.get(SESSION, &opened.id).expect("view"),
        receipt.question
    );
    assert_eq!(inbox_count(&pool), 1);
}

#[test]
fn invalid_and_cross_session_replies_leave_the_question_open() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    assert!(matches!(
        store.apply(
            SESSION,
            &opened.id,
            &answer("bad", 1, "not-a-question", "x"),
            20
        ),
        Err(QuestionError::Invalid(_))
    ));
    assert!(matches!(
        store.apply(
            "ses_foreign",
            &opened.id,
            &answer("foreign", 1, "q1", "x"),
            20
        ),
        Err(QuestionError::NotFound { .. })
    ));
    assert_eq!(store.get(SESSION, &opened.id).expect("view"), opened);
    assert_eq!(inbox_count(&pool), 0);
}

#[test]
fn cancel_is_terminal_but_not_an_answer() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    let receipt = store
        .apply(
            SESSION,
            &opened.id,
            &command("cancel", 1, QuestionAction::Cancel),
            20,
        )
        .expect("cancel");
    assert_eq!(receipt.question.state, QuestionState::Cancelled);
    assert!(receipt.question.answers.is_empty());
    assert!(matches!(
        store.apply(SESSION, &opened.id, &answer("late", 2, "q1", "x"), 21),
        Err(QuestionError::Closed {
            state: QuestionState::Cancelled,
            ..
        })
    ));
}

#[test]
fn outer_transaction_failure_rolls_back_answer_receipt_and_input_together() {
    let (pool, store) = fixture();
    let opened = create(&pool, &spec());
    let failed: QuestionResult<()> = pool.try_transaction(|tx| {
        apply_in(
            tx,
            SESSION,
            &opened.id,
            &answer("rollback", 1, "q1", "Windows"),
            20,
        )?;
        Err(QuestionError::Invalid(
            "later control gate failed".to_owned(),
        ))
    });
    assert!(failed.is_err());
    assert_eq!(store.get(SESSION, &opened.id).expect("view"), opened);
    assert_eq!(inbox_count(&pool), 0);
    let connection = pool.get().expect("connection");
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM question_action_receipt", [], |row| {
                row.get::<_, i64>(0)
            },)
            .expect("receipt count"),
        0
    );
}

#[test]
fn a_plan_needs_a_typed_decision_and_a_matching_handoff() {
    let (pool, store) = fixture();
    let mut definition = spec();
    definition.purpose = QuestionPurpose::PlanAuthorization;
    definition.plan = Some(PlanQuestionBinding {
        plan_id: "plan-one".to_owned(),
        plan_revision: 1,
        source_cycle_id: None,
        title: "Implement".to_owned(),
        completed_steps: 0,
        total_steps: 1,
        work_identity: TurnExecutionIdentity::new("deep", "provider", "model"),
        review_gate: json!({"state": "unbound"}),
    });
    let opened = create(&pool, &definition);
    assert!(matches!(
        store.apply(
            SESSION,
            &opened.id,
            &command(
                "empty",
                1,
                QuestionAction::Answer {
                    answers: BTreeMap::new(),
                }
            ),
            20
        ),
        Err(QuestionError::Invalid(_))
    ));
    let receipt = store
        .apply(
            SESSION,
            &opened.id,
            &command(
                "approve",
                1,
                QuestionAction::PlanDecision {
                    decision: PlanQuestionDecision::Approve,
                    risk_reason: None,
                },
            ),
            21,
        )
        .expect("explicit approval");
    assert_eq!(
        receipt.question.authorization,
        Some(PlanAuthorizationState::WaitingForHandoff)
    );
    assert!(receipt.input_id.is_none());
    assert_eq!(inbox_count(&pool), 0);
    pool.transaction(|tx| {
        assert!(mark_handoff_in(tx, SESSION, "other-turn")?.is_empty());
        assert!(!handoff_completed_in(tx, &opened.id)?);
        assert_eq!(
            mark_handoff_in(tx, SESSION, "turn-question")?,
            vec![opened.id.clone()]
        );
        assert!(handoff_completed_in(tx, &opened.id)?);
        Ok(())
    })
    .expect("handoff bookkeeping");
    assert_eq!(
        store.get(SESSION, &opened.id).expect("view").revision,
        2,
        "handoff bookkeeping must not invalidate an unchanged user form"
    );
}

#[test]
fn old_pending_questions_gain_ids_without_rewriting_the_original_row() {
    let (pool, store) = fixture();
    let before = pool
        .transaction(|tx| {
            human_request::create_in(
                tx,
                &NewHumanRequest {
                    id: "que_legacy".to_owned(),
                    session_id: SESSION.to_owned(),
                    goal_id: None,
                    kind: HumanRequestKind::Input,
                    payload: json!({"source":"question","questions":spec().questions}),
                    message_id: None,
                    call_id: None,
                    time_created: 5,
                },
            )
        })
        .expect("legacy");
    pool.transaction(backfill_legacy_in).expect("backfill");
    let connection = pool.get().expect("connection");
    assert_eq!(
        human_request::get_from(&connection, &before.id).expect("read"),
        Some(before.clone())
    );
    let view = store.get(SESSION, &before.id).expect("upgraded view");
    assert_eq!(view.mode, QuestionMode::Blocking);
    assert_eq!(view.questions[1].id, "q2");
    assert_eq!(view.revision, 1);
}
