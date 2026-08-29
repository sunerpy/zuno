use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zuno_db::human_request::{
    HumanRequestKind, HumanRequestState, HumanRequestStore, NewHumanRequest,
};
use zuno_error::ToolError;
use zuno_goal::GoalStore;
use zuno_tools::question::{Answer, QuestionAsker, QuestionOutcome, QuestionRequest};
use zuno_tui::app::{EventResult, TerminalEvent};
use zuno_tui::views::ViewContext;
use zuno_tui::views::dialog::DialogHost;
use zuno_tui::views::question::{
    QuestionOption as TuiQuestionOption, QuestionPrompt, QuestionRequest as TuiQuestionRequest,
};

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct PendingQuestion {
    request_id: String,
    session_id: String,
    questions: Vec<TuiQuestionRequest>,
    answer: Option<oneshot::Sender<QuestionOutcome>>,
}

#[derive(Clone)]
struct DurableQuestions {
    store: HumanRequestStore,
    goals: Arc<GoalStore>,
    recovery_session_id: String,
}

const QUESTION_CHANNEL_CAPACITY: usize = 8;

pub(crate) struct QuestionBroker {
    waiting: mpsc::Sender<PendingQuestion>,
    pending: Mutex<mpsc::Receiver<PendingQuestion>>,
    wake: mpsc::Sender<TerminalEvent>,
    durable: Mutex<Option<DurableQuestions>>,
    live_ids: Mutex<HashSet<String>>,
}

impl QuestionBroker {
    pub(crate) fn new(wake: mpsc::Sender<TerminalEvent>) -> Self {
        let (waiting, pending) = mpsc::channel(QUESTION_CHANNEL_CAPACITY);
        Self {
            waiting,
            pending: Mutex::new(pending),
            wake,
            durable: Mutex::new(None),
            live_ids: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn attach_durable(
        &self,
        store: HumanRequestStore,
        goals: Arc<GoalStore>,
        recovery_session_id: impl Into<String>,
    ) {
        *locked(&self.durable) = Some(DurableQuestions {
            store,
            goals,
            recovery_session_id: recovery_session_id.into(),
        });
    }

    fn next_request(&self) -> Option<PendingQuestion> {
        locked(&self.pending).try_recv().ok()
    }

    fn next_recovered(&self) -> Option<PendingQuestion> {
        let durable = locked(&self.durable).clone()?;
        let live_ids = locked(&self.live_ids).clone();
        durable
            .store
            .pending(Some(&durable.recovery_session_id))
            .ok()?
            .into_iter()
            .find_map(|request| {
                if request.kind != HumanRequestKind::Input || live_ids.contains(&request.id) {
                    return None;
                }
                let questions = serde_json::from_value::<Vec<QuestionRequest>>(
                    request.payload.get("questions")?.clone(),
                )
                .ok()?
                .iter()
                .map(to_tui_request)
                .collect();
                Some(PendingQuestion {
                    request_id: request.id,
                    session_id: request.session_id,
                    questions,
                    answer: None,
                })
            })
    }

    fn finish(&self, active: ActiveQuestion, outcome: QuestionOutcome) -> bool {
        let persisted = locked(&self.durable).clone().is_none_or(|durable| {
            let now = zuno_db::message::now_millis();
            let response = match &outcome {
                QuestionOutcome::Answered(answers) => Some(json!({"answers": answers})),
                QuestionOutcome::Cancelled | QuestionOutcome::Expired | QuestionOutcome::Failed => {
                    None
                }
            };
            let state = match outcome {
                QuestionOutcome::Answered(_) => HumanRequestState::Answered,
                QuestionOutcome::Cancelled => HumanRequestState::Cancelled,
                QuestionOutcome::Expired => HumanRequestState::Expired,
                QuestionOutcome::Failed => HumanRequestState::Failed,
            };
            let settled = if active.recovered && state == HumanRequestState::Answered {
                durable
                    .store
                    .answer_with_input(
                        &active.request_id,
                        response.expect("answered questions carry a response"),
                        now,
                    )
                    .ok()
                    .flatten()
                    .is_some()
            } else {
                durable
                    .store
                    .resolve(&active.request_id, state, response.as_ref(), now)
                    .ok()
                    .flatten()
                    .is_some()
            };
            if settled && active.goal_owned && state == HumanRequestState::Answered {
                return durable.goals.resume_for_work(&active.session_id).is_ok();
            }
            settled
        });
        let delivered = active
            .answer
            .map_or(active.recovered, |answer| answer.send(outcome).is_ok());
        persisted && delivered
    }
}

#[async_trait]
impl QuestionAsker for QuestionBroker {
    async fn ask(
        &self,
        session_id: &str,
        questions: &[QuestionRequest],
        call: Option<(&str, &str)>,
    ) -> Result<QuestionOutcome, ToolError> {
        if questions.is_empty() {
            return Ok(QuestionOutcome::Answered(Vec::new()));
        }
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
                        "source": "question",
                        "questions": questions,
                    }),
                    message_id: call.map(|(message_id, _)| message_id.to_owned()),
                    call_id: call.map(|(_, call_id)| call_id.to_owned()),
                    time_created: zuno_db::message::now_millis(),
                })
                .map_err(question_store_error)?;
        }
        let questions = questions.iter().map(to_tui_request).collect();
        let (sender, receiver) = oneshot::channel();
        locked(&self.live_ids).insert(request_id.clone());
        if self
            .waiting
            .send(PendingQuestion {
                request_id: request_id.clone(),
                session_id: session_id.to_owned(),
                questions,
                answer: Some(sender),
            })
            .await
            .is_err()
        {
            locked(&self.live_ids).remove(&request_id);
            if let Some(durable) = locked(&self.durable).clone() {
                let _settled = durable.store.resolve(
                    &request_id,
                    HumanRequestState::Failed,
                    None,
                    zuno_db::message::now_millis(),
                );
            }
            return Err(ToolError::Failed {
                tool: String::from("question"),
                source: Box::new(std::io::Error::other("question UI queue is closed")),
            });
        }
        let _nudged = self.wake.try_send(TerminalEvent::Wake);
        let outcome = receiver.await.unwrap_or(QuestionOutcome::Failed);
        locked(&self.live_ids).remove(&request_id);
        if let Some(durable) = locked(&self.durable).clone() {
            let (state, response) = match &outcome {
                QuestionOutcome::Answered(answers) => (
                    HumanRequestState::Answered,
                    Some(json!({"answers": answers})),
                ),
                QuestionOutcome::Cancelled => (HumanRequestState::Cancelled, None),
                QuestionOutcome::Expired => (HumanRequestState::Expired, None),
                QuestionOutcome::Failed => (HumanRequestState::Failed, None),
            };
            let _settled = durable.store.resolve(
                &request_id,
                state,
                response.as_ref(),
                zuno_db::message::now_millis(),
            );
        }
        Ok(outcome)
    }
}

fn question_store_error(source: zuno_error::DbError) -> ToolError {
    ToolError::Failed {
        tool: String::from("question"),
        source: Box::new(source),
    }
}

fn to_tui_request(request: &QuestionRequest) -> TuiQuestionRequest {
    TuiQuestionRequest {
        question: request.question.clone(),
        header: request.header.clone(),
        options: request
            .options
            .iter()
            .map(|option| TuiQuestionOption {
                label: option.label.clone(),
                description: option.description.clone(),
            })
            .collect(),
        multiple: request.multiple,
        custom: request.custom,
    }
}

pub(crate) struct QuestionBridge {
    context: ViewContext,
    broker: Arc<QuestionBroker>,
    active: Option<ActiveQuestion>,
}

struct ActiveQuestion {
    request_id: String,
    session_id: String,
    answer: Option<oneshot::Sender<QuestionOutcome>>,
    recovered: bool,
    goal_owned: bool,
}

impl QuestionBridge {
    pub(crate) fn new(context: ViewContext, broker: Arc<QuestionBroker>) -> Self {
        Self {
            context,
            broker,
            active: None,
        }
    }

    pub(crate) fn resolve(&mut self, answers: Vec<Answer>) -> EventResult {
        let Some(active) = self.active.take() else {
            return EventResult::IGNORED;
        };
        let _settled = self
            .broker
            .finish(active, QuestionOutcome::Answered(answers));
        EventResult::REDRAW
    }

    pub(crate) fn cancel(&mut self) -> EventResult {
        let Some(active) = self.active.take() else {
            return EventResult::IGNORED;
        };
        let _settled = self.broker.finish(active, QuestionOutcome::Cancelled);
        EventResult::REDRAW
    }

    pub(crate) fn open_next(&mut self, host: &mut DialogHost) -> EventResult {
        if self.active.is_some() || host.is_open() {
            return EventResult::IGNORED;
        }
        let Some(request) = self
            .broker
            .next_request()
            .map(|request| (request, false))
            .or_else(|| self.broker.next_recovered().map(|request| (request, true)))
        else {
            return EventResult::IGNORED;
        };
        let (request, recovered) = request;
        let goal_owned = locked(&self.broker.durable)
            .clone()
            .and_then(|durable| durable.store.get(&request.request_id).ok().flatten())
            .is_some_and(|request| request.goal_id.is_some());
        self.active = Some(ActiveQuestion {
            request_id: request.request_id,
            session_id: request.session_id,
            answer: request.answer,
            recovered,
            goal_owned,
        });
        host.open(Box::new(QuestionPrompt::new(
            self.context.clone(),
            request.questions,
        )));
        EventResult::REDRAW
    }
}

#[cfg(test)]
#[path = "tui_question_tests.rs"]
mod tests;
