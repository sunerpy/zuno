use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use zuno_error::ToolError;
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
    questions: Vec<TuiQuestionRequest>,
    answer: oneshot::Sender<QuestionOutcome>,
}

const QUESTION_CHANNEL_CAPACITY: usize = 8;

pub(crate) struct QuestionBroker {
    waiting: mpsc::Sender<PendingQuestion>,
    pending: Mutex<mpsc::Receiver<PendingQuestion>>,
    wake: mpsc::Sender<TerminalEvent>,
}

impl QuestionBroker {
    pub(crate) fn new(wake: mpsc::Sender<TerminalEvent>) -> Self {
        let (waiting, pending) = mpsc::channel(QUESTION_CHANNEL_CAPACITY);
        Self {
            waiting,
            pending: Mutex::new(pending),
            wake,
        }
    }

    fn next_request(&self) -> Option<PendingQuestion> {
        locked(&self.pending).try_recv().ok()
    }
}

#[async_trait]
impl QuestionAsker for QuestionBroker {
    async fn ask(
        &self,
        _session_id: &str,
        questions: &[QuestionRequest],
        _call: Option<(&str, &str)>,
    ) -> Result<QuestionOutcome, ToolError> {
        if questions.is_empty() {
            return Ok(QuestionOutcome::Answered(Vec::new()));
        }
        let questions = questions.iter().map(to_tui_request).collect();
        let (sender, receiver) = oneshot::channel();
        self.waiting
            .send(PendingQuestion {
                questions,
                answer: sender,
            })
            .await
            .map_err(|_| ToolError::Failed {
                tool: String::from("question"),
                source: Box::new(std::io::Error::other("question UI queue is closed")),
            })?;
        let _nudged = self.wake.try_send(TerminalEvent::Wake);
        Ok(receiver.await.unwrap_or(QuestionOutcome::Failed))
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
    active: Option<oneshot::Sender<QuestionOutcome>>,
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
        let Some(answer) = self.active.take() else {
            return EventResult::IGNORED;
        };
        let _delivered = answer.send(QuestionOutcome::Answered(answers));
        EventResult::REDRAW
    }

    pub(crate) fn cancel(&mut self) -> EventResult {
        let Some(answer) = self.active.take() else {
            return EventResult::IGNORED;
        };
        let _delivered = answer.send(QuestionOutcome::Cancelled);
        EventResult::REDRAW
    }

    pub(crate) fn open_next(&mut self, host: &mut DialogHost) -> EventResult {
        if self.active.is_some() || host.is_open() {
            return EventResult::IGNORED;
        }
        let Some(request) = self.broker.next_request() else {
            return EventResult::IGNORED;
        };
        self.active = Some(request.answer);
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
