use crate::{LearningServiceError, Result};
use std::sync::Arc;
use zuno_db::feedback::{FeedbackStore, FeedbackUpdate, FeedbackWrite};
use zuno_error::LearningError;
use zuno_types::{FeedbackRating, MessageFeedbackProjection};

#[derive(Clone)]
pub struct FeedbackService {
    store: FeedbackStore,
}

impl FeedbackService {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>) -> Self {
        Self {
            store: FeedbackStore::new(pool),
        }
    }

    pub fn set(
        &self,
        message_id: &str,
        rating: FeedbackRating,
        note: Option<&str>,
        expected_revision: i64,
        now: i64,
    ) -> Result<MessageFeedbackProjection> {
        let note = note
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(str::to_owned);
        match self.store.set(FeedbackUpdate {
            message_id: message_id.to_owned(),
            rating,
            note,
            expected_revision,
            time_updated: now,
        })? {
            FeedbackWrite::Applied(feedback) => Ok(feedback),
            FeedbackWrite::Stale(current) => Err(LearningServiceError::Learning(
                LearningError::FeedbackRevisionConflict {
                    message_id: message_id.to_owned(),
                    expected_revision,
                    current_revision: current.revision,
                },
            )),
        }
    }

    pub fn get(&self, message_id: &str) -> Result<Option<MessageFeedbackProjection>> {
        self.store.get(message_id).map_err(Into::into)
    }

    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<MessageFeedbackProjection>> {
        self.store.list_for_session(session_id).map_err(Into::into)
    }
}
