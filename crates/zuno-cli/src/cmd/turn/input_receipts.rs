//! Native logical-turn receipt ownership, independent of any client observer.

use super::{TurnFailure, TurnOutcome};
use zuno_db::input_receipt::InputReceiptStore;
use zuno_types::admission::InputStopReason;

pub(super) struct ReceiptCycle {
    store: InputReceiptStore,
    database: std::sync::Arc<zuno_db::Pool>,
    session_id: String,
    current_turn: Option<String>,
}

impl ReceiptCycle {
    pub(super) fn new(database: std::sync::Arc<zuno_db::Pool>, session_id: &str) -> Self {
        Self {
            store: InputReceiptStore::new(database.clone()),
            database,
            session_id: session_id.to_owned(),
            current_turn: None,
        }
    }

    pub(super) fn control_inputs(&self, cycle_id: &str) -> Result<Vec<String>, TurnFailure> {
        let connection = self.database.get().map_err(TurnFailure::Database)?;
        let mut query = connection
            .prepare(
                "SELECT i.id FROM session_input i JOIN session_input_receipt r ON r.input_id=i.id
             WHERE i.session_id=?1 AND i.cycle_id=?2 AND i.state='consumed'
               AND json_extract(i.prompt,'$.kind')='sessionControl'
               AND r.state='recorded' AND r.turn_id IS NULL ORDER BY i.admitted_seq",
            )
            .map_err(zuno_db::map_error)
            .map_err(TurnFailure::Database)?;
        query
            .query_map(rusqlite::params![self.session_id, cycle_id], |row| {
                row.get(0)
            })
            .map_err(zuno_db::map_error)
            .map_err(TurnFailure::Database)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(zuno_db::map_error)
            .map_err(TurnFailure::Database)
    }

    pub(super) fn begin(&mut self, turn_id: &str, input_ids: &[String]) -> Result<(), TurnFailure> {
        let now = zuno_db::message::now_millis();
        // This object is scoped to one native logical drive. Only its explicit
        // recovery transfers ownership, atomically with the next input binding.
        self.store
            .begin_turn(
                &self.session_id,
                self.current_turn.as_deref(),
                input_ids,
                turn_id,
                now,
            )
            .map_err(TurnFailure::Database)?;
        self.current_turn = Some(turn_id.to_owned());
        Ok(())
    }

    pub(super) fn finish(
        &self,
        outcome: &Result<Option<TurnOutcome>, TurnFailure>,
        credential: Option<&str>,
    ) -> Result<(), TurnFailure> {
        let Some(turn_id) = &self.current_turn else {
            return Ok(());
        };
        let (reason, error) = match outcome {
            Ok(Some(TurnOutcome::Completed {
                assistant_message_id,
                ..
            })) => {
                let connection = self.database.get().map_err(TurnFailure::Database)?;
                let needs_completion: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM session_input_receipt r
                     JOIN session_input i ON i.id=r.input_id WHERE i.session_id=?1
                     AND r.turn_id=?2 AND (r.state='applied' OR
                        (r.state='recorded' AND json_extract(i.prompt,'$.kind')='sessionControl')))",
                    rusqlite::params![self.session_id,turn_id], |row| row.get(0),
                ).map_err(zuno_db::map_error).map_err(TurnFailure::Database)?;
                if !needs_completion {
                    // An embedded driver with no provider-applied inputs has
                    // no processing success to attest. Release only bindings.
                    drop(connection);
                    return self
                        .store
                        .finish_turn(
                            &self.session_id,
                            turn_id,
                            Some(InputStopReason::EndTurn),
                            None,
                            zuno_db::message::now_millis(),
                        )
                        .map_err(TurnFailure::Database);
                }
                let message = zuno_db::message::MessageStore::new(&connection)
                    .message(assistant_message_id)
                    .map_err(TurnFailure::Database)?;
                (
                    Some(
                        match message
                            .data
                            .get("finish")
                            .and_then(serde_json::Value::as_str)
                        {
                            Some(reason)
                                if reason == zuno_llm::event::FinishReason::Length.as_str() =>
                            {
                                InputStopReason::MaxTokens
                            }
                            Some(reason)
                                if reason
                                    == zuno_llm::event::FinishReason::ContentFilter.as_str() =>
                            {
                                InputStopReason::Refusal
                            }
                            _ => InputStopReason::EndTurn,
                        },
                    ),
                    None,
                )
            }
            Ok(Some(TurnOutcome::WaitingForHuman { .. })) => (Some(InputStopReason::EndTurn), None),
            Ok(Some(TurnOutcome::Interrupted { .. })) => (Some(InputStopReason::Cancelled), None),
            Ok(None) => return Ok(()),
            Err(error) => {
                let detail = match error {
                    TurnFailure::Engine(error) => error.to_string(),
                    TurnFailure::Database(error) => error.to_string(),
                    TurnFailure::Host(detail)
                    | TurnFailure::EventConsumer(detail)
                    | TurnFailure::GoalRecovery {
                        message: detail, ..
                    } => detail.clone(),
                };
                (None, Some(super::redact_learning_text(&detail, credential)))
            }
        };
        self.store
            .finish_turn(
                &self.session_id,
                turn_id,
                reason,
                error.as_deref(),
                zuno_db::message::now_millis(),
            )
            .map_err(TurnFailure::Database)
    }
}
