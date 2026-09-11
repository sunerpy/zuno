//! Durable questions and idempotent, revision-checked human responses.
//!
//! The store owns no sockets or waiting futures. The caller can extend these
//! transactions with Goal and Plan control; notification always follows commit.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zuno_error::DbError;
use zuno_tool::question::{QuestionError, QuestionResult};
use zuno_types::goal_resume::GoalResumeRequest;
use zuno_types::question::{
    PlanAuthorizationState, PlanQuestionBinding, PlanQuestionDecision, QuestionAction,
    QuestionAnswers, QuestionCommand, QuestionItem, QuestionMode, QuestionOrigin, QuestionPurpose,
    QuestionReceipt, QuestionRequest, QuestionSpec, QuestionState, QuestionView,
};

use crate::event_log::{NewSessionEvent, append_in};
use crate::human_request::{self, HumanRequest, HumanRequestKind, NewHumanRequest};
use crate::inbox::{InputDelivery, NewSessionInput, SessionInput, admit_in};
use crate::{Pool, open};

const TABLE: &str = "question_interaction";

/// The definition stays stable while response, mode, and consent state advance.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Definition {
    origin: QuestionOrigin,
    questions: Vec<QuestionItem>,
    plan: Option<PlanQuestionBinding>,
    #[serde(default)]
    initial_mode: QuestionMode,
    #[serde(default)]
    handoff_completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    goal_resume: Option<GoalResumeRequest>,
}

#[derive(Debug, Clone)]
pub struct QuestionStore {
    pool: Arc<Pool>,
}

impl QuestionStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn get(&self, session_id: &str, request_id: &str) -> QuestionResult<QuestionView> {
        let connection = self.pool.get()?;
        get_in(&connection, session_id, request_id)
    }

    pub fn pending(&self, session_id: &str) -> QuestionResult<Vec<QuestionView>> {
        let connection = self.pool.get()?;
        pending_in(&connection, session_id)
    }

    pub fn apply(
        &self,
        session_id: &str,
        request_id: &str,
        command: &QuestionCommand,
        now: i64,
    ) -> QuestionResult<QuestionReceipt> {
        self.pool
            .try_transaction(|tx| apply_in(tx, session_id, request_id, command, now))
    }
}

/// Create one question, deduplicating the exact originating tool call.
pub fn create_in(
    tx: &Transaction<'_>,
    request_id: &str,
    spec: &QuestionSpec,
    now: i64,
) -> QuestionResult<QuestionReceipt> {
    spec.validate()?;
    if let Some(existing) = existing_call_in(tx, spec)? {
        return Ok(QuestionReceipt {
            question: existing,
            input_id: None,
            duplicate: true,
        });
    }
    let payload = json!({
        "source": match spec.purpose {
            QuestionPurpose::Clarification => match spec.mode {
                QuestionMode::Blocking => "question",
                QuestionMode::Deferred => "question_async",
            },
            QuestionPurpose::RequiredInput if spec.origin.goal_id.is_some() => "goal_request_input",
            QuestionPurpose::RequiredInput => "question",
            QuestionPurpose::PlanAuthorization => "plan_exit",
            QuestionPurpose::GoalResume => "goal_resume",
        },
        "questions": spec.questions,
    });
    let request = human_request::create_in(
        tx,
        &NewHumanRequest {
            id: request_id.to_owned(),
            session_id: spec.origin.session_id.clone(),
            goal_id: spec.origin.goal_id.clone(),
            kind: HumanRequestKind::Input,
            payload,
            message_id: spec.origin.message_id.clone(),
            call_id: spec.origin.call_id.clone(),
            time_created: now,
        },
    )?;
    let definition = Definition {
        origin: spec.origin.clone(),
        questions: items(&spec.questions),
        plan: spec.plan.clone(),
        initial_mode: spec.mode,
        handoff_completed: false,
        goal_resume: if spec.purpose == QuestionPurpose::GoalResume {
            Some(GoalResumeRequest {
                session_id: spec.origin.session_id.clone(),
                goal_id: spec.origin.goal_id.clone().expect("validated Goal"),
                expected_revision: spec.expected_goal_revision.expect("validated revision"),
                input_id: spec.origin.message_id.clone(),
            })
        } else {
            None
        },
    };
    insert_definition_in(tx, &request.id, spec.purpose, spec.mode, &definition)?;
    let question = get_in(tx, &spec.origin.session_id, request_id)?;
    record_event(tx, "question.opened", &question)?;
    Ok(QuestionReceipt {
        question,
        input_id: None,
        duplicate: false,
    })
}

fn existing_call_in(
    tx: &Transaction<'_>,
    spec: &QuestionSpec,
) -> QuestionResult<Option<QuestionView>> {
    let (Some(message_id), Some(call_id)) = (
        spec.origin.message_id.as_deref(),
        spec.origin.call_id.as_deref(),
    ) else {
        return Ok(None);
    };
    let existing: Option<String> = tx
        .query_row(
            "SELECT h.id FROM human_request h JOIN question_interaction q ON q.request_id = h.id \
             WHERE h.session_id = ?1 AND h.message_id = ?2 AND h.call_id = ?3 \
             ORDER BY h.time_created, h.id LIMIT 1",
            params![spec.origin.session_id, message_id, call_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(open::map_error)?;
    let Some(id) = existing else {
        return Ok(None);
    };
    let view = get_in(tx, &spec.origin.session_id, &id)?;
    if view.purpose != spec.purpose
        || view.questions != items(&spec.questions)
        || view.origin != spec.origin
        || view.plan != spec.plan
    {
        return Err(QuestionError::Invalid(
            "one tool call cannot publish different questions".to_owned(),
        ));
    }
    Ok(Some(view))
}

fn items(questions: &[QuestionRequest]) -> Vec<QuestionItem> {
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| QuestionItem {
            id: format!("q{}", index + 1),
            question: question.clone(),
        })
        .collect()
}

fn insert_definition_in(
    tx: &Transaction<'_>,
    request_id: &str,
    purpose: QuestionPurpose,
    mode: QuestionMode,
    definition: &Definition,
) -> Result<(), DbError> {
    tx.execute(
        "INSERT INTO question_interaction (request_id,purpose,mode,definition) VALUES (?1,?2,?3,?4)",
        params![
            request_id,
            purpose.as_str(),
            mode.as_str(),
            encode(definition)?,
        ],
    )
    .map_err(open::map_error)?;
    Ok(())
}

/// Conservative format upgrade: keep old rows byte-for-byte, add only metadata.
///
/// Unknown custom request payloads remain untouched and available to their own
/// consumer. A migration must not guess a question shape or manufacture an answer.
pub fn backfill_legacy_in(tx: &Transaction<'_>) -> Result<(), DbError> {
    let ids = {
        let mut statement = tx
            .prepare(
                "SELECT h.id FROM human_request h \
                 WHERE h.kind = 'input' AND NOT EXISTS \
                 (SELECT 1 FROM question_interaction q WHERE q.request_id = h.id) \
                 ORDER BY h.time_created,h.id",
            )
            .map_err(open::map_error)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(open::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(open::map_error)?
    };
    for id in ids {
        let Some(request) = human_request::get_from(tx, &id)? else {
            continue;
        };
        let Some(value) = request.payload.get("questions") else {
            continue;
        };
        let Ok(questions) = serde_json::from_value::<Vec<QuestionRequest>>(value.clone()) else {
            continue;
        };
        if questions.is_empty()
            || questions
                .iter()
                .any(|question| question.validate().is_err())
        {
            continue;
        }
        let purpose = if request.goal_id.is_some() {
            QuestionPurpose::RequiredInput
        } else {
            QuestionPurpose::Clarification
        };
        let definition = Definition {
            origin: QuestionOrigin {
                session_id: request.session_id,
                message_id: request.message_id,
                call_id: request.call_id,
                turn_id: None,
                goal_id: request.goal_id,
            },
            questions: items(&questions),
            plan: None,
            initial_mode: QuestionMode::Blocking,
            handoff_completed: false,
            goal_resume: None,
        };
        insert_definition_in(tx, &id, purpose, QuestionMode::Blocking, &definition)?;
    }
    Ok(())
}

pub fn get_in(
    connection: &Connection,
    session_id: &str,
    request_id: &str,
) -> QuestionResult<QuestionView> {
    let request = human_request::get_from(connection, request_id)?
        .filter(|request| {
            request.session_id == session_id && request.kind == HumanRequestKind::Input
        })
        .ok_or_else(|| not_found(session_id, request_id))?;
    let metadata = connection
        .query_row(
            "SELECT purpose,mode,definition,decision,authorization \
             FROM question_interaction WHERE request_id = ?1",
            [request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| not_found(session_id, request_id))?;
    let (purpose, mode, definition, decision, authorization) = metadata;
    let definition: Definition = decode(&definition)?;
    if definition.origin.session_id != request.session_id
        || definition.origin.goal_id != request.goal_id
        || definition.origin.message_id != request.message_id
        || definition.origin.call_id != request.call_id
    {
        return Err(corrupt("question definition and durable owner disagree").into());
    }
    let purpose: QuestionPurpose = decode_enum(purpose)?;
    if (purpose == QuestionPurpose::PlanAuthorization) != definition.plan.is_some() {
        return Err(corrupt("question purpose and Plan binding disagree").into());
    }
    if (purpose == QuestionPurpose::GoalResume) != definition.goal_resume.is_some() {
        return Err(corrupt("question purpose and Goal resume binding disagree").into());
    }
    if let Some(binding) = &definition.goal_resume {
        binding.validate()?;
        if binding.session_id != definition.origin.session_id
            || Some(&binding.goal_id) != definition.origin.goal_id.as_ref()
            || binding.input_id != definition.origin.message_id
        {
            return Err(corrupt("Goal resume binding and question owner disagree").into());
        }
    }
    let answers = stored_answers(&request, &definition.questions)?;
    let draft_answers = request
        .response
        .as_ref()
        .and_then(|response| response.get("draftAnswers"))
        .map(|draft| serde_json::from_value(draft.clone()).map_err(decode_error))
        .transpose()?
        .unwrap_or_default();
    let view = QuestionView {
        id: request.id,
        origin: definition.origin,
        revision: request.revision,
        mode: decode_enum(mode)?,
        purpose,
        state: QuestionState::parse(request.state.as_str())
            .ok_or_else(|| corrupt("unknown question state"))?,
        questions: definition.questions,
        answers,
        draft_answers,
        plan: definition.plan,
        decision: decision.map(decode_enum).transpose()?,
        authorization: authorization.map(decode_enum).transpose()?,
        time_created: request.time_created,
        time_updated: request.time_updated,
    };
    view.validate_draft_answers(&view.draft_answers)
        .map_err(|error| corrupt(&format!("invalid stored question draft: {error}")))?;
    Ok(view)
}

pub fn pending_in(connection: &Connection, session_id: &str) -> QuestionResult<Vec<QuestionView>> {
    let mut statement = connection
        .prepare(
            "SELECT h.id FROM human_request h JOIN question_interaction q ON q.request_id = h.id \
             WHERE h.session_id = ?1 AND h.state = 'pending' ORDER BY h.time_created,h.id",
        )
        .map_err(open::map_error)?;
    let ids = statement
        .query_map([session_id], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    ids.into_iter()
        .map(|id| get_in(connection, session_id, &id))
        .collect()
}

/// Native Goal consent is bound in the immutable question definition.
pub fn goal_resume_request_in(
    connection: &Connection,
    session_id: &str,
    request_id: &str,
) -> QuestionResult<GoalResumeRequest> {
    let view = get_in(connection, session_id, request_id)?;
    if view.purpose != QuestionPurpose::GoalResume {
        return Err(QuestionError::Invalid(
            "question is not Goal resume consent".to_owned(),
        ));
    }
    let encoded: String = connection
        .query_row(
            "SELECT definition FROM question_interaction WHERE request_id=?1",
            [request_id],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    let definition: Definition = decode(&encoded)?;
    definition
        .goal_resume
        .ok_or_else(|| QuestionError::Invalid("Goal resume binding is missing".to_owned()))
}

/// Retire only a pending native offer whose original input can no longer run.
/// Terminal user choices remain authoritative and must never be superseded.
pub fn supersede_unusable_goal_resume_in(
    tx: &Transaction<'_>,
    session_id: &str,
    request_id: &str,
    now: i64,
) -> QuestionResult<bool> {
    let view = get_in(tx, session_id, request_id)?;
    if view.purpose != QuestionPurpose::GoalResume || view.state != QuestionState::Pending {
        return Ok(false);
    }
    let binding = goal_resume_request_in(tx, session_id, request_id)?;
    let Some(input_id) = binding.input_id.as_deref() else {
        return Ok(false);
    };
    if crate::inbox::read_in(tx, session_id, input_id)?.is_some_and(|input| {
        !matches!(
            input.state,
            crate::inbox::SubmissionState::Cancelled | crate::inbox::SubmissionState::Failed
        )
    }) {
        return Ok(false);
    }
    human_request::resolve_in(
        tx,
        request_id,
        human_request::HumanRequestState::Cancelled,
        Some(&json!({
            "outcome":"superseded",
            "reason":"input_unavailable",
            "resumeSuperseded":true,
        })),
        now,
    )?;
    record_event(
        tx,
        "question.cancelled",
        &get_in(tx, session_id, request_id)?,
    )?;
    Ok(true)
}

fn stored_answers(
    request: &HumanRequest,
    questions: &[QuestionItem],
) -> Result<QuestionAnswers, DbError> {
    let Some(response) = request.response.as_ref() else {
        return Ok(BTreeMap::new());
    };
    if let Some(answers) = response.get("answersById") {
        return serde_json::from_value(answers.clone()).map_err(decode_error);
    }
    // Published older formats stored positional answers. This projection is
    // read-only; the original response is never rewritten by the migration.
    let legacy = response.get("answers").unwrap_or(response);
    let Ok(answers) = serde_json::from_value::<Vec<Vec<String>>>(legacy.clone()) else {
        return Ok(BTreeMap::new());
    };
    Ok(questions
        .iter()
        .zip(answers)
        .filter(|(_, answers)| !answers.is_empty())
        .map(|(question, answers)| (question.id.clone(), answers))
        .collect())
}

/// Atomically record an action and its model-visible input. The service may add
/// Goal/Plan changes before the caller commits this same transaction.
pub fn apply_in(
    tx: &Transaction<'_>,
    session_id: &str,
    request_id: &str,
    command: &QuestionCommand,
    now: i64,
) -> QuestionResult<QuestionReceipt> {
    command.validate()?;
    let mut view = get_in(tx, session_id, request_id)?;
    if let Some(receipt) = receipt_in(tx, request_id, command)? {
        return Ok(receipt);
    }
    if view.revision != command.expected_revision {
        return Err(QuestionError::Conflict {
            request_id: request_id.to_owned(),
            expected: command.expected_revision,
            actual: view.revision,
        });
    }
    if view.state.is_terminal() {
        return Err(QuestionError::Closed {
            request_id: request_id.to_owned(),
            state: view.state,
        });
    }
    let mut risk_reason = None;
    let mut admit_response = false;
    match &command.action {
        QuestionAction::Answer { answers } => {
            view.validate_answers(answers)?;
            admit_response = view.purpose != QuestionPurpose::GoalResume
                && answers.values().any(|values| !values.is_empty());
            for (id, answers) in answers {
                // Only explicitly submitted keys leave draft state. Never
                // promote other saved form values on a partial/empty answer.
                view.draft_answers.remove(id);
                if answers.is_empty() {
                    view.answers.remove(id);
                } else {
                    view.answers.insert(id.clone(), answers.clone());
                }
            }
            if !answers.is_empty() && view.is_fully_answered() {
                view.state = QuestionState::Answered;
            }
        }
        QuestionAction::Defer { draft_answers } => {
            view.validate_draft_answers(draft_answers)?;
            view.draft_answers.extend(draft_answers.clone());
            view.state = QuestionState::Pending;
        }
        QuestionAction::Cancel => {
            admit_response = view.purpose != QuestionPurpose::GoalResume;
            view.state = QuestionState::Cancelled;
            if view.purpose == QuestionPurpose::PlanAuthorization {
                view.authorization = Some(PlanAuthorizationState::Invalidated);
            }
        }
        QuestionAction::PlanDecision {
            decision,
            risk_reason: reason,
        } => {
            if view.purpose != QuestionPurpose::PlanAuthorization || view.plan.is_none() {
                return Err(QuestionError::Invalid(
                    "a clarification cannot authorize Work".to_owned(),
                ));
            }
            view.decision = Some(*decision);
            view.state = QuestionState::Answered;
            view.authorization = Some(match decision {
                PlanQuestionDecision::Approve => PlanAuthorizationState::WaitingForHandoff,
                PlanQuestionDecision::Decline => PlanAuthorizationState::Invalidated,
            });
            risk_reason = reason.clone();
            // Approval becomes a source-keyed Start Work control only after the
            // service validates the successful handoff; it is not a generic steer.
            admit_response = *decision == PlanQuestionDecision::Decline;
        }
    }
    // Submitting a partial answer, or deferring, releases only the synchronous
    // waiter. Remaining items stay pending and answerable.
    view.mode = QuestionMode::Deferred;
    let expected = view.revision;
    view.revision = expected
        .checked_add(1)
        .ok_or_else(|| corrupt("question revision exhausted"))?;
    view.time_updated = now;
    let response = json!({
        "answersById": view.answers,
        "draftAnswers": view.draft_answers,
        "action": command.action,
    });
    let changed = tx
        .execute(
            "UPDATE human_request SET state=?1,response=?2,revision=?3,time_updated=?4,time_resolved=?5 \
             WHERE id=?6 AND session_id=?7 AND state='pending' AND revision=?8",
            params![
                view.state.as_str(),
                encode(&response)?,
                view.revision,
                now,
                view.state.is_terminal().then_some(now),
                request_id,
                session_id,
                expected,
            ],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        return Err(QuestionError::Conflict {
            request_id: request_id.to_owned(),
            expected,
            actual: get_in(tx, session_id, request_id)?.revision,
        });
    }
    tx.execute(
        "UPDATE question_interaction SET mode=?1,decision=?2,authorization=?3,risk_reason=?4 \
         WHERE request_id=?5",
        params![
            view.mode.as_str(),
            view.decision.map(|decision| match decision {
                PlanQuestionDecision::Approve => "approve",
                PlanQuestionDecision::Decline => "decline",
            }),
            view.authorization.map(authorization_name),
            risk_reason,
            request_id,
        ],
    )
    .map_err(open::map_error)?;
    let input = if admit_response {
        Some(admit_response_in(tx, &view, command, now)?)
    } else {
        None
    };
    let receipt = QuestionReceipt {
        question: view,
        input_id: input.map(|input| input.id),
        duplicate: false,
    };
    tx.execute(
        "INSERT INTO question_action_receipt \
         (request_id,command_id,command_json,receipt,time_created) VALUES (?1,?2,?3,?4,?5)",
        params![
            request_id,
            command.command_id,
            encode(command)?,
            encode(&receipt)?,
            now
        ],
    )
    .map_err(open::map_error)?;
    record_event(tx, "question.updated", &receipt.question)?;
    Ok(receipt)
}

pub fn receipt_in(
    connection: &Connection,
    request_id: &str,
    command: &QuestionCommand,
) -> QuestionResult<Option<QuestionReceipt>> {
    let stored: Option<(String, String)> = connection
        .query_row(
            "SELECT command_json,receipt FROM question_action_receipt \
             WHERE request_id=?1 AND command_id=?2",
            params![request_id, command.command_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(open::map_error)?;
    let Some((original, receipt)) = stored else {
        return Ok(None);
    };
    let original: QuestionCommand = decode(&original)?;
    if original != *command {
        return Err(QuestionError::CommandConflict {
            command_id: command.command_id.clone(),
        });
    }
    let mut receipt: QuestionReceipt = decode(&receipt)?;
    receipt.duplicate = true;
    Ok(Some(receipt))
}

fn admit_response_in(
    tx: &Transaction<'_>,
    view: &QuestionView,
    command: &QuestionCommand,
    now: i64,
) -> Result<SessionInput, DbError> {
    let outcome = match &command.action {
        QuestionAction::Answer { answers } if answers.values().any(|values| !values.is_empty()) => {
            "answered"
        }
        QuestionAction::Cancel => "cancelled",
        QuestionAction::PlanDecision {
            decision: PlanQuestionDecision::Decline,
            ..
        } => "declined",
        QuestionAction::Answer { .. }
        | QuestionAction::Defer { .. }
        | QuestionAction::PlanDecision {
            decision: PlanQuestionDecision::Approve,
            ..
        } => {
            return Err(corrupt(
                "unsubmitted question data cannot enter model input",
            ));
        }
    };
    // The match above excludes drafts and empty answers before serialization.
    // Only explicitly submitted answer keys or cancellation/decline can enter
    // the model inbox; the full client view remains in the durable event log.
    let model_response = serde_json::to_value(&command.action).map_err(decode_error)?;
    let prompt = json!({
        "kind": "humanRequestAnswer",
        "requestID": view.id,
        "requestRevision": view.revision,
        "humanRequestKind": "input",
        "questionPurpose": view.purpose,
        "outcome": outcome,
        "text": format!(
            "Human response to question `{}` ({}). Unanswered items are not consent or facts.\n\nQuestions:\n{}\n\nAction:\n{}",
            view.id, outcome, encode(&view.questions)?, encode(&model_response)?,
        ),
        "request": {"questions": view.questions, "origin": view.origin},
        "response": model_response,
    });
    admit_in(
        tx,
        NewSessionInput::new(
            format!("human_{}_{}", view.id, view.revision),
            &view.origin.session_id,
            prompt,
            InputDelivery::Queue,
            now,
        )
        .with_source_key(format!("question:{}:{}", view.id, command.command_id)),
    )
}

/// Handoff bookkeeping does not change the user-answer revision. Otherwise a
/// form opened before handoff could never submit its still-valid explicit choice.
pub fn mark_handoff_in(
    tx: &Transaction<'_>,
    session_id: &str,
    turn_id: &str,
) -> Result<Vec<String>, DbError> {
    let cycle_id =
        crate::session_execution::read_in(tx, session_id)?.and_then(|state| state.cycle_id);
    let ids = {
        let mut statement = tx
            .prepare(
                "SELECT q.request_id FROM question_interaction q \
             JOIN human_request h ON h.id=q.request_id \
             WHERE h.session_id=?1 AND q.purpose='plan_authorization' \
             AND (json_extract(q.definition,'$.origin.turnId')=?2 \
                  OR (?3 IS NOT NULL AND json_extract(q.definition,'$.plan.sourceCycleId')=?3)) \
             AND (q.authorization IS NULL OR q.authorization='waiting_for_handoff')",
            )
            .map_err(open::map_error)?;
        statement
            .query_map(params![session_id, turn_id, cycle_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(open::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(open::map_error)?
    };
    for id in &ids {
        tx.execute(
            "UPDATE question_interaction SET definition=json_set(definition,'$.handoffCompleted',json('true')) \
             WHERE request_id=?1",
            [id],
        ).map_err(open::map_error)?;
    }
    Ok(ids)
}

pub fn handoff_completed_in(connection: &Connection, request_id: &str) -> Result<bool, DbError> {
    connection
        .query_row(
            "SELECT COALESCE(json_extract(definition,'$.handoffCompleted'),0) \
         FROM question_interaction WHERE request_id=?1",
            [request_id],
            |row| row.get(0),
        )
        .map_err(open::map_error)
}

pub fn risk_reason_in(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<String>, DbError> {
    connection
        .query_row(
            "SELECT risk_reason FROM question_interaction WHERE request_id=?1",
            [request_id],
            |row| row.get(0),
        )
        .map_err(open::map_error)
}

pub fn set_authorization_in(
    tx: &Transaction<'_>,
    session_id: &str,
    request_id: &str,
    state: PlanAuthorizationState,
    input_id: Option<&str>,
) -> QuestionResult<QuestionView> {
    let current = get_in(tx, session_id, request_id)?;
    if current.purpose != QuestionPurpose::PlanAuthorization {
        return Err(QuestionError::Invalid(
            "request does not authorize a Plan".to_owned(),
        ));
    }
    tx.execute(
        "UPDATE question_interaction SET authorization=?1,authorization_input_id=?2 WHERE request_id=?3",
        params![authorization_name(state), input_id, request_id],
    ).map_err(open::map_error)?;
    let updated = get_in(tx, session_id, request_id)?;
    // Approval can settle after its HTTP/TUI command returned. Retrying that
    // command must report the resulting authority, not a forever-pending receipt.
    tx.execute(
        "UPDATE question_action_receipt \
         SET receipt=json_set(receipt,'$.question',json(?1),'$.inputId',?2) \
         WHERE request_id=?3 AND json_extract(receipt,'$.question.decision')='approve'",
        params![encode(&updated)?, input_id, request_id],
    )
    .map_err(open::map_error)?;
    record_event(tx, "question.authorization", &updated)?;
    Ok(updated)
}

/// Unapplied Plan requests, including approvals still awaiting source handoff.
pub fn active_plan_authorizations_in(
    connection: &Connection,
    session_id: &str,
) -> QuestionResult<Vec<QuestionView>> {
    let mut statement = connection
        .prepare(
            "SELECT h.id FROM human_request h JOIN question_interaction q ON q.request_id=h.id \
         WHERE h.session_id=?1 AND q.purpose='plan_authorization' \
         AND h.state IN ('pending','answered') \
         AND (q.authorization IS NULL OR q.authorization='waiting_for_handoff') \
         ORDER BY h.time_created,h.id",
        )
        .map_err(open::map_error)?;
    let ids = statement
        .query_map([session_id], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    ids.into_iter()
        .map(|id| get_in(connection, session_id, &id))
        .collect()
}

pub fn record_receipt_in(
    tx: &Transaction<'_>,
    command_id: &str,
    receipt: &QuestionReceipt,
) -> Result<(), DbError> {
    tx.execute(
        "UPDATE question_action_receipt SET receipt=?1 WHERE request_id=?2 AND command_id=?3",
        params![encode(receipt)?, receipt.question.id, command_id],
    )
    .map_err(open::map_error)?;
    Ok(())
}

fn record_event(
    tx: &Transaction<'_>,
    event_type: &str,
    view: &QuestionView,
) -> Result<(), DbError> {
    let properties = serde_json::Map::from_iter([(
        "question".to_owned(),
        serde_json::to_value(view).map_err(decode_error)?,
    )]);
    append_in(
        tx,
        &view.origin.session_id,
        NewSessionEvent::new(event_type, properties)?,
    )?;
    Ok(())
}

fn authorization_name(state: PlanAuthorizationState) -> &'static str {
    match state {
        PlanAuthorizationState::WaitingForHandoff => "waiting_for_handoff",
        PlanAuthorizationState::Applied => "applied",
        PlanAuthorizationState::Invalidated => "invalidated",
    }
}

fn not_found(session_id: &str, request_id: &str) -> QuestionError {
    QuestionError::NotFound {
        session_id: session_id.to_owned(),
        request_id: request_id.to_owned(),
    }
}

fn encode(value: &impl Serialize) -> Result<String, DbError> {
    serde_json::to_string(value).map_err(decode_error)
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, DbError> {
    serde_json::from_str(value).map_err(decode_error)
}

fn decode_enum<T: serde::de::DeserializeOwned>(value: String) -> Result<T, DbError> {
    serde_json::from_value(Value::String(value)).map_err(decode_error)
}

fn decode_error(source: serde_json::Error) -> DbError {
    DbError::Decode {
        table: TABLE.to_owned(),
        source,
    }
}

fn corrupt(detail: &str) -> DbError {
    crate::event_log::query_error(std::io::Error::other(detail.to_owned()))
}

#[cfg(test)]
#[path = "question_tests.rs"]
mod tests;
