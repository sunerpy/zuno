use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use uuid::Uuid;
use zuno_db::human_request::{
    HumanRequestKind, HumanRequestState, HumanRequestStore, NewHumanRequest,
};
use zuno_error::ToolError;
use zuno_goal::GoalStore;
use zuno_tools::question::{Answer, QuestionAsker, QuestionOutcome, QuestionRequest};

use crate::ClientConnection;
use crate::settlement::Settlement;

const QUESTION_TOOL: &str = "question";

/// ACP-backed structured question delivery.
///
/// The caller must only install this asker after the client advertises
/// `clientCapabilities.elicitation.form`.
#[derive(Debug, Clone)]
pub struct AcpQuestionAsker {
    client: ClientConnection,
    route: Option<Arc<crate::AcpSessionRoute>>,
    durable: Arc<Mutex<Option<DurableQuestions>>>,
}

#[derive(Debug, Clone)]
struct DurableQuestions {
    store: HumanRequestStore,
    goals: Arc<GoalStore>,
}

impl AcpQuestionAsker {
    #[must_use]
    pub fn new(client: ClientConnection) -> Self {
        Self {
            client,
            route: None,
            durable: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn with_route(client: ClientConnection, route: Arc<crate::AcpSessionRoute>) -> Self {
        Self {
            client,
            route: Some(route),
            durable: Arc::new(Mutex::new(None)),
        }
    }

    pub fn attach_durable(&self, store: HumanRequestStore, goals: Arc<GoalStore>) {
        *locked(&self.durable) = Some(DurableQuestions { store, goals });
    }

    /// Present and settle one request that survived its originating process.
    ///
    /// Every reply the client returns — an answer, a decline, or a form this build
    /// cannot read — is admitted to the durable inbox before this returns, because the
    /// caller turns a `true` into a hard failure when the settlement produced no
    /// model-visible input. The abandoned provider/tool request is never revived.
    ///
    /// `false` means this surface settled nothing and the row is untouched: it is
    /// still pending, still the only evidence of what was asked, and still answerable
    /// by another surface.
    pub async fn answer_pending(&self, request_id: &str) -> Result<bool, ToolError> {
        let durable = locked(&self.durable)
            .clone()
            .ok_or_else(|| question_failure("durable question store is not attached"))?;
        // A row another surface settled between the caller's `pending()` read and this
        // call is not this surface's work. Reporting it as a failure would turn a
        // benign cross-surface race into a failed `session/prompt`.
        let Some(request) = durable
            .store
            .get(request_id)
            .map_err(question_store_failure)?
            .filter(|request| {
                request.kind == HumanRequestKind::Input
                    && request.state == HumanRequestState::Pending
            })
        else {
            return Ok(false);
        };
        // A stored question this surface cannot present is skipped, never settled and
        // never rewritten. `Ok(false)` already stops recovery from re-failing the
        // prompt; resolving the row on top of that would drop it out of
        // `HumanRequestStore::pending`, so the TUI and the HTTP broker could no longer
        // answer a request the user is still waiting on, and a goal-attached row would
        // be parked behind `resume_for_work`'s answered-only guard. The payload stays
        // exactly as written, which is the recovery evidence.
        let Some(questions) = stored_questions(&request.payload) else {
            return Ok(false);
        };
        let call = request
            .message_id
            .as_deref()
            .zip(request.call_id.as_deref());
        let Ok(elicitation) = self.elicitation(&request.session_id, &questions, call) else {
            return Ok(false);
        };
        // A form that never reached the client is not an outcome. Recovery owns a row
        // it did not create, so an undeliverable re-presentation leaves it pending and
        // answerable by the TUI, the HTTP broker, or the next attempt, instead of
        // discarding the question the user is still waiting on.
        let Some(outcome) = self.deliver(elicitation, &questions).await else {
            return Ok(false);
        };
        // Everything the client actually replied — answered, declined, withdrawn,
        // expired, or unreadable — is a decision the loop has to be told about, so all
        // of it settles and resumes through the one rule.
        self.settle(&durable, request_id, &outcome, Settlement::DurableInput)
    }

    /// Record one settled question through the crate's single settlement rule.
    ///
    /// The rule — every terminal outcome resolves `answered` and resumes whatever Goal
    /// the settled row itself carries — lives in [`crate::settlement::settle`], shared
    /// with [`crate::AcpPermissionAsker`]. This wrapper only chooses the durable
    /// response and labels a failure with this tool.
    fn settle(
        &self,
        durable: &DurableQuestions,
        request_id: &str,
        outcome: &QuestionOutcome,
        settlement: Settlement,
    ) -> Result<bool, ToolError> {
        let settled = crate::settlement::settle(
            &durable.store,
            &durable.goals,
            request_id,
            settled_response(outcome),
            settlement,
        )
        .map_err(|source| ToolError::Failed {
            tool: QUESTION_TOOL.to_owned(),
            source,
        })?;
        Ok(settled.is_some())
    }

    /// Build the client form, rejecting a question that cannot be rendered.
    fn elicitation(
        &self,
        session_id: &str,
        questions: &[QuestionRequest],
        call: Option<(&str, &str)>,
    ) -> Result<Value, ToolError> {
        let routed = self.route.as_ref().map_or_else(
            || crate::RoutedSession::direct(session_id),
            |route| route.resolve(session_id),
        );
        let mut request = elicitation_request(routed.wire_session_id(), questions, call)?;
        if let Some(child_session_id) = routed.child_session_id() {
            request["_meta"] = json!({
                "zuno": {
                    "childSessionId": child_session_id,
                },
            });
        }
        Ok(request)
    }

    /// Deliver a built form, reporting `None` when it never reached the client.
    ///
    /// A transport failure is never an error here: the caller still owns a durable
    /// row, and only the caller knows whether an undelivered form must be settled
    /// (its own live call is being failed) or left pending (recovery holds a row
    /// another surface may still present).
    async fn deliver(
        &self,
        elicitation: Value,
        questions: &[QuestionRequest],
    ) -> Option<QuestionOutcome> {
        match self.client.request("elicitation/create", elicitation).await {
            Ok(response) => Some(elicitation_outcome(&response, questions)),
            Err(_) => None,
        }
    }
}

#[async_trait]
impl QuestionAsker for AcpQuestionAsker {
    async fn ask(
        &self,
        session_id: &str,
        questions: &[QuestionRequest],
        call: Option<(&str, &str)>,
    ) -> Result<QuestionOutcome, ToolError> {
        // Render before persisting. A durable row created for a question the client
        // can never be shown stays pending, and every later recovery pass in the
        // session re-fails on that one row.
        let elicitation = self.elicitation(session_id, questions, call)?;
        let request_id = format!("que_{}", Uuid::new_v4().simple());
        if let Some(durable) = locked(&self.durable).clone() {
            durable
                .store
                .create(NewHumanRequest {
                    id: request_id.clone(),
                    session_id: session_id.to_owned(),
                    goal_id: None,
                    kind: HumanRequestKind::Input,
                    payload: json!({
                        "source": QUESTION_TOOL,
                        "questions": questions,
                    }),
                    message_id: call.map(|(message_id, _)| message_id.to_owned()),
                    call_id: call.map(|(_, call_id)| call_id.to_owned()),
                    time_created: zuno_db::message::now_millis(),
                })
                .map_err(question_store_failure)?;
        }
        // This process owns the row it just created, so an undelivered form settles
        // here rather than staying pending: the tool call it belongs to is ending, and
        // a row left open would have ACP recovery re-present a question already
        // answered by its own failure.
        let outcome = self
            .deliver(elicitation, questions)
            .await
            .unwrap_or(QuestionOutcome::Failed);
        if let Some(durable) = locked(&self.durable).clone() {
            let _settled = self.settle(&durable, &request_id, &outcome, Settlement::ResolveOnly)?;
        }
        Ok(outcome)
    }
}

/// The durable response recorded for one settled question.
///
/// An answered question keeps the plain `{"answers": …}` shape every other surface
/// writes. Every other terminal outcome records no answers plus the discriminator,
/// so durable history never claims a withdrawn, expired, or unreadable form was an
/// answer the user gave — the state column cannot carry that distinction, because
/// `GoalStore::resume_for_work` accepts only `answered`.
///
/// This is the only place a [`QuestionOutcome`] is inspected on the settlement path.
/// It decides the recorded payload and nothing else: a fifth outcome variant would
/// have to compile against this one exhaustive match, and whatever it recorded, the
/// row would still settle and its Goal would still resume, because neither of those
/// is expressed per arm.
fn settled_response(outcome: &QuestionOutcome) -> Value {
    match outcome {
        QuestionOutcome::Answered(answers) => json!({"answers": answers}),
        QuestionOutcome::Cancelled => withdrawn_response("cancelled"),
        QuestionOutcome::Expired => withdrawn_response("expired"),
        QuestionOutcome::Failed => withdrawn_response("failed"),
    }
}

/// A settled question that produced no answer, and why.
fn withdrawn_response(outcome: &str) -> Value {
    json!({"answers": [], "outcome": outcome})
}

/// The questions a stored payload holds, or `None` when this build cannot read it.
///
/// A payload written by a different `QuestionRequest` shape is not corruption to
/// repair; it is a row this surface has no form for. The caller skips it.
fn stored_questions(payload: &Value) -> Option<Vec<QuestionRequest>> {
    serde_json::from_value::<Vec<QuestionRequest>>(payload.get("questions")?.clone()).ok()
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn question_store_failure(error: zuno_error::DbError) -> ToolError {
    ToolError::Failed {
        tool: QUESTION_TOOL.to_owned(),
        source: Box::new(error),
    }
}

fn question_failure(message: impl Into<String>) -> ToolError {
    ToolError::Failed {
        tool: QUESTION_TOOL.to_owned(),
        source: Box::new(std::io::Error::other(message.into())),
    }
}

fn elicitation_request(
    session_id: &str,
    questions: &[QuestionRequest],
    call: Option<(&str, &str)>,
) -> Result<Value, ToolError> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        for field in property_schemas(index, question)? {
            if field.required {
                required.push(field.name.clone());
            }
            properties.insert(field.name, field.schema);
        }
    }
    let mut requested_schema = json!({
        "type": "object",
        "title": "Questions",
        "description": "Answer each question in order.",
        "properties": properties,
    });
    if !required.is_empty() {
        requested_schema["required"] = json!(required);
    }
    let mut request = json!({
        "mode": "form",
        "message": request_message(questions),
        "sessionId": session_id,
        "requestedSchema": requested_schema,
    });
    if let Some((_, tool_call_id)) = call {
        request["toolCallId"] = Value::String(tool_call_id.to_owned());
    }
    Ok(request)
}

fn request_message(questions: &[QuestionRequest]) -> String {
    match questions {
        [question] => question.question.clone(),
        _ => format!("Please answer {} questions.", questions.len()),
    }
}

struct FormField {
    name: String,
    schema: Value,
    required: bool,
}

fn property_schemas(index: usize, question: &QuestionRequest) -> Result<Vec<FormField>, ToolError> {
    let base_name = format!("q{index}");
    let custom = allows_custom(question);
    if question.options.is_empty() {
        if !custom {
            return Err(invalid_request(
                "a question with custom answers disabled must offer at least one option",
            ));
        }
        return Ok(vec![FormField {
            name: base_name,
            schema: custom_schema(question, false),
            required: true,
        }]);
    }

    validate_options(question)?;
    let choice = choice_schema(question);
    if custom {
        Ok(vec![
            FormField {
                name: format!("{base_name}_choice"),
                schema: choice,
                required: false,
            },
            FormField {
                name: format!("{base_name}_custom"),
                schema: custom_schema(question, true),
                required: false,
            },
        ])
    } else {
        Ok(vec![FormField {
            name: base_name,
            schema: choice,
            required: true,
        }])
    }
}

fn custom_schema(question: &QuestionRequest, alongside_choices: bool) -> Value {
    let title = if alongside_choices {
        format!("{} — Other", question.header)
    } else {
        question.header.clone()
    };
    let description = if alongside_choices {
        format!(
            "{}\n\nType a custom answer instead of the listed choices.",
            question.question
        )
    } else {
        question.question.clone()
    };
    json!({
            "type": "string",
            "title": title,
            "description": description,
            "minLength": 1,
    })
}

fn choice_schema(question: &QuestionRequest) -> Value {
    let choices = question
        .options
        .iter()
        .map(|option| {
            json!({
                "const": option.label,
                "title": option.label,
                "description": option.description,
            })
        })
        .collect::<Vec<_>>();
    if is_multiple(question) {
        json!({
            "type": "array",
            "title": question.header,
            "description": question.question,
            "minItems": 1,
            "items": {
                "anyOf": choices,
            },
        })
    } else {
        json!({
            "type": "string",
            "title": question.header,
            "description": question.question,
            "oneOf": choices,
        })
    }
}

fn validate_options(question: &QuestionRequest) -> Result<(), ToolError> {
    let mut labels = HashSet::with_capacity(question.options.len());
    for option in &question.options {
        if !labels.insert(option.label.as_str()) {
            return Err(invalid_request(format!(
                "question options must have unique labels; duplicate `{}`",
                option.label
            )));
        }
    }
    Ok(())
}

fn invalid_request(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: QUESTION_TOOL.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message.into(),
        )),
    }
}

fn elicitation_outcome(response: &Value, questions: &[QuestionRequest]) -> QuestionOutcome {
    match response.get("action").and_then(Value::as_str) {
        Some("accept") => accepted_answers(response, questions)
            .map(QuestionOutcome::Answered)
            .unwrap_or(QuestionOutcome::Failed),
        // ACP distinguishes a deliberate decline from cancellation, while Zuno's
        // durable question state has one human-withdrawal terminal. Neither is a
        // transport failure and neither may reopen the question.
        Some("decline" | "cancel") => QuestionOutcome::Cancelled,
        _ => QuestionOutcome::Failed,
    }
}

fn accepted_answers(response: &Value, questions: &[QuestionRequest]) -> Option<Vec<Answer>> {
    let content = response.get("content")?.as_object()?;
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| accepted_answer(index, question, content))
        .collect()
}

fn accepted_answer(
    index: usize,
    question: &QuestionRequest,
    content: &Map<String, Value>,
) -> Option<Answer> {
    let base_name = format!("q{index}");
    if question.options.is_empty() {
        let answer = content.get(&base_name)?.as_str()?;
        return (!answer.is_empty()).then(|| vec![answer.to_owned()]);
    }

    if allows_custom(question) {
        if let Some(custom) = content.get(&format!("{base_name}_custom")) {
            let custom = custom.as_str()?;
            if !custom.is_empty() {
                return Some(vec![custom.to_owned()]);
            }
        }
        return content.get(&format!("{base_name}_choice")).map_or_else(
            || Some(Vec::new()),
            |value| accepted_choice(question, value, true),
        );
    }

    accepted_choice(question, content.get(&base_name)?, false)
}

fn accepted_choice(question: &QuestionRequest, value: &Value, optional: bool) -> Option<Answer> {
    if is_multiple(question) {
        let values = value.as_array()?;
        if values.is_empty() {
            return optional.then(Vec::new);
        }
        values
            .iter()
            .map(|value| {
                let value = value.as_str()?;
                is_offered(question, value).then(|| value.to_owned())
            })
            .collect()
    } else {
        let value = value.as_str()?;
        is_offered(question, value).then(|| vec![value.to_owned()])
    }
}

fn is_multiple(question: &QuestionRequest) -> bool {
    question.multiple.unwrap_or(false)
}

fn allows_custom(question: &QuestionRequest) -> bool {
    question.custom.unwrap_or(true)
}

fn is_offered(question: &QuestionRequest, value: &str) -> bool {
    question.options.iter().any(|option| option.label == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::test_client::ScriptedClient;
    use zuno_tools::question::QuestionOption;

    fn option(label: &str, description: &str) -> QuestionOption {
        QuestionOption::new(label, description)
    }

    struct Durable {
        _spill: tempfile::TempDir,
        goals: Arc<GoalStore>,
        store: HumanRequestStore,
    }

    /// Insert the `project` and `session` rows the durable inbox has a foreign key on.
    ///
    /// `answer_with_input` admits a `session_input` row in the same transaction, and
    /// that table references `session`. Without these rows a recovery test fails on
    /// the constraint instead of on the behaviour under test.
    fn materialize_session(goals: &GoalStore, session_id: &str) {
        let connection = goals.pool().get().expect("check out a connection");
        connection
            .execute(
                "INSERT OR IGNORE INTO project \
                 (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
                  time_updated,time_initialized,sandboxes,commands) \
                 VALUES ('acp-fixture','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
                (),
            )
            .expect("insert the fixture project");
        connection
            .execute(
                &format!(
                    "INSERT OR IGNORE INTO session \
                     (id,project_id,slug,directory,title,version,time_created,time_updated) \
                     VALUES ('{session_id}','acp-fixture','{session_id}','/tmp',\
                             '{session_id}','test',1,1)"
                ),
                (),
            )
            .expect("insert the fixture session");
    }

    fn durable_store() -> Durable {
        let spill = tempfile::tempdir().expect("spill directory");
        let goals = Arc::new(
            GoalStore::open_memory(spill.path().to_path_buf()).expect("in-memory goal store"),
        );
        let store = goals.human_requests();
        Durable {
            _spill: spill,
            goals,
            store,
        }
    }

    /// A pending, goal-attached question row, as a dead process left it.
    ///
    /// `request_human_input` is the path a Goal blocker takes, so the Goal is really
    /// paused on this request and only a settlement `resume_for_work` accepts can
    /// lift that pause.
    fn abandoned_question(durable: &Durable, session_id: &str) -> String {
        materialize_session(&durable.goals, session_id);
        let goal = durable
            .goals
            .create_goal(session_id, "recover an abandoned question", None)
            .expect("create an active goal");
        durable
            .goals
            .request_human_input(
                session_id,
                goal.revision,
                format!("que_{session_id}"),
                json!({ "source": QUESTION_TOOL, "questions": [strict_single()] }),
                None,
                None,
            )
            .expect("pause the goal on a pending question")
            .id
    }

    fn goal_status(durable: &Durable, session_id: &str) -> zuno_goal::GoalStatus {
        durable
            .goals
            .goal(session_id)
            .expect("read the goal")
            .expect("the goal exists")
            .status
    }

    /// The durable inbox ids admitted for one session.
    ///
    /// `zuno-cli` turns a `true` from `answer_pending` into a hard `-32603` when the
    /// settlement admitted no durable input, so the admission is part of the contract
    /// rather than an implementation detail.
    fn admitted_inputs(durable: &Durable, session_id: &str) -> Vec<String> {
        let connection = durable.goals.pool().get().expect("check out a connection");
        let mut statement = connection
            .prepare("SELECT id FROM session_input WHERE session_id = ?1 ORDER BY id")
            .expect("prepare the inbox read");
        statement
            .query_map([session_id], |row| row.get::<_, String>(0))
            .expect("read the admitted inputs")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode the admitted inputs")
    }

    /// The id of the one question row a session holds.
    ///
    /// `ask` generates its own request id, so a live-path assertion has to find the row
    /// rather than name it.
    fn only_request_id(durable: &Durable, session_id: &str) -> String {
        let connection = durable.goals.pool().get().expect("check out a connection");
        connection
            .query_row(
                "SELECT id FROM human_request WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .expect("exactly one question row exists")
    }

    fn duplicate_labels() -> QuestionRequest {
        let mut question = strict_single();
        question.options.push(option("Stable", "Duplicate label."));
        question
    }

    fn strict_single() -> QuestionRequest {
        QuestionRequest {
            question: "Choose a release channel.".to_owned(),
            header: "Channel".to_owned(),
            options: vec![
                option("Stable", "Use the production channel."),
                option("Preview", "Use the preview channel."),
            ],
            multiple: None,
            custom: Some(false),
        }
    }

    fn strict_multiple() -> QuestionRequest {
        QuestionRequest {
            question: "Choose target platforms.".to_owned(),
            header: "Platforms".to_owned(),
            options: vec![
                option("Linux", "Build the Linux target."),
                option("Windows", "Build the Windows target."),
            ],
            multiple: Some(true),
            custom: Some(false),
        }
    }

    #[test]
    fn request_uses_stable_form_schema_and_tool_scope() {
        let request = elicitation_request(
            "ses-1",
            &[strict_single(), strict_multiple()],
            Some(("msg-1", "call-1")),
        )
        .expect("valid form request");

        assert_eq!(request["mode"], "form");
        assert_eq!(request["sessionId"], "ses-1");
        assert_eq!(request["toolCallId"], "call-1");
        assert_eq!(request["requestedSchema"]["type"], "object");
        assert_eq!(request["requestedSchema"]["required"], json!(["q0", "q1"]));
        assert_eq!(
            request["requestedSchema"]["properties"]["q0"],
            json!({
                "type": "string",
                "title": "Channel",
                "description": "Choose a release channel.",
                "oneOf": [
                    {
                        "const": "Stable",
                        "title": "Stable",
                        "description": "Use the production channel.",
                    },
                    {
                        "const": "Preview",
                        "title": "Preview",
                        "description": "Use the preview channel.",
                    },
                ],
            })
        );
        assert_eq!(
            request["requestedSchema"]["properties"]["q1"],
            json!({
                "type": "array",
                "title": "Platforms",
                "description": "Choose target platforms.",
                "minItems": 1,
                "items": {
                    "anyOf": [
                        {
                            "const": "Linux",
                            "title": "Linux",
                            "description": "Build the Linux target.",
                        },
                        {
                            "const": "Windows",
                            "title": "Windows",
                            "description": "Build the Windows target.",
                        },
                    ],
                },
            })
        );
    }

    #[test]
    fn custom_answers_keep_native_choices_and_add_an_other_field() {
        let mut question = strict_multiple();
        question.custom = None;
        let request =
            elicitation_request("ses-1", &[question], None).expect("valid custom request");
        let schema = &request["requestedSchema"];
        assert!(schema.get("required").is_none());
        let properties = schema["properties"].as_object().expect("form properties");
        assert_eq!(properties.len(), 2);

        let choices = &properties["q0_choice"];
        assert_eq!(choices["type"], "array");
        assert_eq!(choices["title"], "Platforms");
        assert_eq!(choices["minItems"], 1);
        assert_eq!(choices["items"]["anyOf"][0]["const"], "Linux");
        assert_eq!(choices["items"]["anyOf"][1]["const"], "Windows");

        let custom = &properties["q0_custom"];
        assert_eq!(custom["type"], "string");
        assert_eq!(custom["title"], "Platforms — Other");
        assert_eq!(custom["minLength"], 1);
        assert_eq!(
            custom["description"],
            "Choose target platforms.\n\nType a custom answer instead of the listed choices."
        );
        assert!(request.get("toolCallId").is_none());
    }

    #[test]
    fn parses_single_select_answer_positionally() {
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0": "Preview" },
                }),
                &[strict_single()],
            ),
            QuestionOutcome::Answered(vec![vec!["Preview".to_owned()]])
        );
    }

    #[test]
    fn parses_multi_select_answer_positionally() {
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0": ["Linux", "Windows"] },
                }),
                &[strict_multiple()],
            ),
            QuestionOutcome::Answered(vec![vec!["Linux".to_owned(), "Windows".to_owned(),]])
        );
    }

    #[test]
    fn custom_question_accepts_a_native_choice() {
        let mut question = strict_single();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0_choice": "Preview" },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Preview".to_owned()]])
        );
    }

    #[test]
    fn custom_multi_select_question_accepts_native_choices() {
        let mut question = strict_multiple();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0_choice": ["Linux", "Windows"] },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Linux".to_owned(), "Windows".to_owned()]])
        );
    }

    #[test]
    fn custom_answer_takes_precedence_over_a_native_choice() {
        let mut question = strict_single();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": {
                        "q0_choice": "Preview",
                        "q0_custom": "Nightly",
                    },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Nightly".to_owned()]])
        );
    }

    #[test]
    fn free_text_only_question_uses_one_required_field() {
        let question = QuestionRequest {
            question: "Name the release.".to_owned(),
            header: "Release".to_owned(),
            options: Vec::new(),
            multiple: None,
            custom: None,
        };
        let request =
            elicitation_request("ses-1", std::slice::from_ref(&question), None).expect("request");
        let schema = &request["requestedSchema"];
        assert_eq!(schema["required"], json!(["q0"]));
        assert_eq!(schema["properties"]["q0"]["type"], "string");
        assert_eq!(schema["properties"]["q0"]["title"], "Release");
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0": "Canary" },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Canary".to_owned()]])
        );
    }

    #[test]
    fn custom_question_may_be_submitted_unanswered() {
        let mut question = strict_single();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": {},
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![Vec::new()])
        );
    }

    #[test]
    fn decline_and_cancel_are_terminal_cancellations() {
        for action in ["decline", "cancel"] {
            assert_eq!(
                elicitation_outcome(&json!({ "action": action }), &[strict_single()]),
                QuestionOutcome::Cancelled
            );
        }
    }

    #[test]
    fn malformed_and_unknown_responses_fail_closed() {
        for response in [
            json!({}),
            json!({ "action": "future-action" }),
            json!({ "action": "accept" }),
            json!({ "action": "accept", "content": null }),
            json!({ "action": "accept", "content": {} }),
            json!({ "action": "accept", "content": { "q0": 7 } }),
            json!({ "action": "accept", "content": { "q0": "Nightly" } }),
        ] {
            assert_eq!(
                elicitation_outcome(&response, &[strict_single()]),
                QuestionOutcome::Failed,
                "response should fail closed: {response}"
            );
        }
    }

    #[test]
    fn duplicate_strict_labels_are_rejected_before_delivery() {
        let mut question = strict_single();
        question.options.push(option("Stable", "Duplicate label."));
        let error = elicitation_request("ses-1", &[question], None)
            .expect_err("duplicate enum labels make oneOf ambiguous");
        assert_eq!(error.tool(), QUESTION_TOOL);
    }

    #[test]
    fn duplicate_custom_labels_are_rejected_before_delivery() {
        let mut question = strict_single();
        question.custom = None;
        question.options.push(option("Stable", "Duplicate label."));
        let error = elicitation_request("ses-1", &[question], None)
            .expect_err("duplicate enum labels make oneOf ambiguous");
        assert_eq!(error.tool(), QUESTION_TOOL);
    }

    #[tokio::test]
    async fn an_unrenderable_question_is_rejected_before_it_is_persisted() {
        let durable = durable_store();
        let client = ScriptedClient::unreachable();
        let asker = AcpQuestionAsker::new(client.connection());
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));

        let error = asker
            .ask(
                "ses_reject",
                &[duplicate_labels()],
                Some(("msg_reject", "call_reject")),
            )
            .await
            .expect_err("a duplicate label cannot be rendered");

        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
        assert!(
            durable
                .store
                .pending(Some("ses_reject"))
                .expect("read pending questions")
                .is_empty(),
            "a rejected question must leave no pending row for recovery to re-fail on"
        );
        assert!(client.methods().is_empty(), "{:?}", client.methods());
    }

    /// A stored question this surface cannot present must survive untouched.
    ///
    /// Recovery has to advance, but the row is durable user state: the payload is the
    /// only evidence of what was asked, another surface must still be able to answer
    /// it, and its Goal must still be resumable afterwards.
    #[tokio::test]
    async fn a_stored_question_that_cannot_be_presented_is_skipped_not_destroyed() {
        for payload in [
            json!({ "source": QUESTION_TOOL, "questions": [duplicate_labels()] }),
            json!({ "source": QUESTION_TOOL }),
            json!({ "source": QUESTION_TOOL, "questions": "not an array" }),
        ] {
            let durable = durable_store();
            let client = ScriptedClient::unreachable();
            let asker = AcpQuestionAsker::new(client.connection());
            asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
            materialize_session(&durable.goals, "ses_recover");
            let goal = durable
                .goals
                .create_goal("ses_recover", "recover an abandoned question", None)
                .expect("create an active goal");
            durable
                .goals
                .request_human_input(
                    "ses_recover",
                    goal.revision,
                    "que_stored".to_owned(),
                    payload.clone(),
                    None,
                    None,
                )
                .expect("pause the goal on a pending question");

            let answered = asker
                .answer_pending("que_stored")
                .await
                .expect("recovery must advance instead of failing the prompt");

            assert!(!answered, "an unpresented question admits no durable input");
            let stored = durable
                .store
                .get("que_stored")
                .expect("read the stored question")
                .expect("the row survives");
            assert_eq!(
                stored.state,
                HumanRequestState::Pending,
                "the row must stay answerable: {payload}"
            );
            assert_eq!(
                stored.payload, payload,
                "the stored payload is the recovery evidence"
            );
            assert_eq!(stored.response, None, "{stored:?}");
            assert_eq!(
                durable
                    .store
                    .pending(Some("ses_recover"))
                    .expect("read pending questions")
                    .len(),
                1,
                "another surface must still find the row: {payload}"
            );
            durable
                .store
                .answer_with_input("que_stored", json!({"answers": [["ok"]]}), 2_000)
                .expect("another surface answers the skipped row")
                .expect("the skipped row is still pending");
            assert_eq!(
                durable
                    .goals
                    .resume_for_work("ses_recover")
                    .expect("resume the goal")
                    .expect("the goal exists")
                    .status,
                zuno_goal::GoalStatus::Active,
                "a skipped row leaves its Goal resumable: {payload}"
            );
            assert!(client.methods().is_empty(), "{:?}", client.methods());
        }
    }

    /// A row another surface already answered is skipped, not reported as a failure.
    ///
    /// The caller reads `pending()` before calling this, so the TUI or the HTTP broker
    /// may settle the row in between. Failing here turned that benign race into a
    /// `-32603` on the user's `session/prompt` and re-presented a settled question.
    #[tokio::test]
    async fn a_question_another_surface_already_answered_is_skipped() {
        let durable = durable_store();
        let client = ScriptedClient::unreachable();
        let asker = AcpQuestionAsker::new(client.connection());
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        materialize_session(&durable.goals, "ses_raced");
        let goal = durable
            .goals
            .create_goal("ses_raced", "recover a raced question", None)
            .expect("create an active goal");
        durable
            .goals
            .request_human_input(
                "ses_raced",
                goal.revision,
                "que_raced".to_owned(),
                json!({ "source": QUESTION_TOOL, "questions": [strict_single()] }),
                None,
                None,
            )
            .expect("pause the goal on a pending question");
        let elsewhere = json!({"answers": [["left"]]});
        durable
            .store
            .answer_with_input("que_raced", elsewhere.clone(), 2_000)
            .expect("another surface answers the row")
            .expect("the pending row is settled there");

        let answered = asker
            .answer_pending("que_raced")
            .await
            .expect("a settled row is not this surface's work");

        assert!(!answered);
        let stored = durable
            .store
            .get("que_raced")
            .expect("read the stored question")
            .expect("the row survives");
        assert_eq!(stored.state, HumanRequestState::Answered);
        assert_eq!(
            stored.response,
            Some(elsewhere),
            "the answer the user gave elsewhere must not be overwritten"
        );
        assert!(client.methods().is_empty(), "{:?}", client.methods());
    }

    /// A declined recovery is a decision the Goal must continue past.
    ///
    /// The exact input: the client answers `elicitation/create` with
    /// `{"action":"decline"}` for a row `request_human_input` created, so the Goal is
    /// genuinely paused on it. `resume_for_work` lifts a human-input pause only while
    /// its request is exactly `answered`, so recording the withdrawal as `cancelled`
    /// left the Goal paused with no route out and no pending row for any surface to
    /// answer — the user's objective silently abandoned by one remote reply.
    #[tokio::test]
    async fn a_declined_recovered_question_resumes_the_paused_goal() {
        let durable = durable_store();
        let client = ScriptedClient::new(|_method, _params| Ok(json!({ "action": "decline" })));
        let asker = AcpQuestionAsker::new(client.connection());
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let request_id = abandoned_question(&durable, "ses_declined");

        let settled = asker
            .answer_pending(&request_id)
            .await
            .expect("present the recovered question");

        // Liveness first: this is the assertion the durable label exists to serve.
        assert_eq!(
            goal_status(&durable, "ses_declined"),
            zuno_goal::GoalStatus::Active,
            "a declined question must not park the Goal"
        );
        assert_eq!(
            durable
                .goals
                .pause_state("ses_declined")
                .expect("read the pause state"),
            None,
            "the human-input pause must be consumed"
        );
        assert!(settled, "a decided dialog is settled work, not a skip");
        assert_eq!(
            admitted_inputs(&durable, "ses_declined"),
            vec![format!("human_{request_id}")],
            "a `true` with no durable input is a -32603 in zuno-cli"
        );
        let stored = durable
            .store
            .get(&request_id)
            .expect("read the stored question")
            .expect("the row survives");
        assert_eq!(stored.state, HumanRequestState::Answered);
        assert_eq!(
            stored.response,
            Some(json!({"answers": [], "outcome": "cancelled"})),
            "durable history must not claim the user answered"
        );
        assert!(
            durable
                .store
                .pending(Some("ses_declined"))
                .expect("read pending questions")
                .is_empty(),
            "a decided question is not re-presented"
        );
        assert_eq!(client.methods(), vec![String::from("elicitation/create")]);
    }

    /// A recovery the client cannot receive leaves the row answerable elsewhere.
    ///
    /// The exact input: the client answers `elicitation/create` with a JSON-RPC error.
    /// A form that never reached the client is not a decision, so nothing is written —
    /// settling it would drop the row out of `pending` and discard a question the user
    /// is still waiting on, while its Goal stays resumable through the answer that row
    /// is still owed.
    #[tokio::test]
    async fn an_undeliverable_recovered_question_leaves_the_row_pending() {
        let durable = durable_store();
        let client = ScriptedClient::unreachable();
        let asker = AcpQuestionAsker::new(client.connection());
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let request_id = abandoned_question(&durable, "ses_unreachable");

        let settled = asker
            .answer_pending(&request_id)
            .await
            .expect("recovery must advance instead of failing the prompt");

        assert!(!settled);
        let stored = durable
            .store
            .get(&request_id)
            .expect("read the stored question")
            .expect("the row survives");
        assert_eq!(
            stored.state,
            HumanRequestState::Pending,
            "a client that could not be reached decided nothing"
        );
        assert_eq!(stored.response, None, "{stored:?}");
        assert!(
            admitted_inputs(&durable, "ses_unreachable").is_empty(),
            "a skip admits no durable input"
        );
        assert_eq!(
            durable
                .store
                .pending(Some("ses_unreachable"))
                .expect("read pending questions")
                .len(),
            1,
            "another surface must still find the row"
        );
        durable
            .store
            .answer_with_input(&request_id, json!({"answers": [["ok"]]}), 2_000)
            .expect("another surface answers the row")
            .expect("the skipped row is still pending");
        assert_eq!(
            durable
                .goals
                .resume_for_work("ses_unreachable")
                .expect("resume the goal")
                .expect("the goal exists")
                .status,
            zuno_goal::GoalStatus::Active,
            "the Goal is resumable through the answer the row is still owed"
        );
        assert_eq!(client.methods(), vec![String::from("elicitation/create")]);
    }

    /// The live sibling of the recovery path settles the same way.
    ///
    /// `ask` creates its own row, so a dismissed form settles here rather than staying
    /// pending for recovery to re-present. It records the withdrawal, not an answer,
    /// and it still returns the outcome the tool reports to the model itself.
    #[tokio::test]
    async fn a_dismissed_live_question_is_settled_as_a_withdrawal() {
        let durable = durable_store();
        let client = ScriptedClient::new(|_method, _params| Ok(json!({ "action": "cancel" })));
        let asker = AcpQuestionAsker::new(client.connection());
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));

        let outcome = asker
            .ask(
                "ses_live",
                &[strict_single()],
                Some(("msg_live", "call_live")),
            )
            .await
            .expect("a dismissed form is an outcome, not an error");

        assert_eq!(outcome, QuestionOutcome::Cancelled);
        assert!(
            durable
                .store
                .pending(Some("ses_live"))
                .expect("read pending questions")
                .is_empty(),
            "the live row is settled, not left for recovery"
        );
        let stored = durable
            .store
            .get(&only_request_id(&durable, "ses_live"))
            .expect("read the settled question")
            .expect("the row survives");
        assert_eq!(stored.state, HumanRequestState::Answered);
        assert_eq!(
            stored.response,
            Some(json!({"answers": [], "outcome": "cancelled"})),
            "durable history must not claim the user answered"
        );
    }

    /// A row the released build already resolved still reads, and is left alone.
    ///
    /// 0.6.6 recorded a dismissed ACP question as `cancelled` with no response. Those
    /// rows are on disk now: this build must read them, skip them, and never rewrite
    /// them. A read that refused, or a repair pass, would be the same harm as a failed
    /// migration.
    #[tokio::test]
    async fn a_question_the_released_build_resolved_still_reads_and_is_skipped() {
        for (session_id, state) in [
            ("ses_legacy_cancelled", HumanRequestState::Cancelled),
            ("ses_legacy_expired", HumanRequestState::Expired),
            ("ses_legacy_failed", HumanRequestState::Failed),
        ] {
            let durable = durable_store();
            let client = ScriptedClient::unreachable();
            let asker = AcpQuestionAsker::new(client.connection());
            asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
            let request_id = abandoned_question(&durable, session_id);
            let payload = durable
                .store
                .get(&request_id)
                .expect("read the stored question")
                .expect("the row survives")
                .payload;
            durable
                .store
                .resolve(&request_id, state, None, 2_000)
                .expect("the released build resolved this row")
                .expect("the pending row is resolved there");

            let settled = asker
                .answer_pending(&request_id)
                .await
                .expect("a row this build did not settle is not an error");

            assert!(!settled);
            let stored = durable
                .store
                .get(&request_id)
                .expect("read the stored question")
                .expect("the row survives");
            assert_eq!(
                stored.state, state,
                "an already-settled row is not rewritten"
            );
            assert_eq!(stored.response, None, "{stored:?}");
            assert_eq!(stored.payload, payload, "the payload is untouched");
            assert!(client.methods().is_empty(), "{:?}", client.methods());
        }
    }
}
