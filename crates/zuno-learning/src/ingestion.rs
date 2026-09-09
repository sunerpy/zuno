use crate::{CompletedTaskSignals, ExtractionRequest, LearningScheduleOutcome, LearningScheduler};
use std::sync::Arc;
use zuno_db::learning_source::{LearningSourceField, LearningSourceKind, LearningSourceStore};

#[derive(Clone)]
pub struct LearningIngestion {
    sources: LearningSourceStore,
}

impl LearningIngestion {
    pub fn new(pool: Arc<zuno_db::Pool>) -> Self {
        Self {
            sources: LearningSourceStore::new(pool),
        }
    }

    pub fn latest_assistant(&self, session_id: &str) -> crate::Result<String> {
        self.sources.latest_assistant(session_id)?.ok_or_else(|| {
            crate::model::invalid("the session has no completed assistant turn to reflect")
        })
    }

    pub fn request(
        &self,
        project_id: &str,
        session_id: &str,
        end_message_id: &str,
        session_wide: bool,
        redact: &dyn Fn(&str) -> String,
    ) -> crate::Result<(ExtractionRequest, CompletedTaskSignals)> {
        let window = if session_wide {
            self.sources
                .for_session(session_id, end_message_id, redact)?
        } else {
            self.sources.for_turn(session_id, end_message_id, redact)?
        };
        let had_tool_calls = window
            .sources
            .iter()
            .any(|source| source.kind == LearningSourceKind::Tool);
        let had_artifacts = window
            .sources
            .iter()
            .any(|source| source.kind == LearningSourceKind::Artifact);
        let explicit_feedback = window
            .sources
            .iter()
            .any(|source| source.kind == LearningSourceKind::Feedback);
        let recovered_from_error = window
            .sources
            .iter()
            .any(|source| source.field == LearningSourceField::Error)
            && window.sources.iter().any(|source| source.proves_success);
        let user_corrected = window.sources.iter().any(|source| {
            source.kind == LearningSourceKind::User && looks_like_correction(&source.content)
        });
        let signals = CompletedTaskSignals {
            completed: true,
            had_tool_calls,
            had_artifacts,
            explicit_feedback,
            recovered_from_error,
            user_corrected,
            external_context: window.external_context,
        };
        Ok((
            ExtractionRequest {
                project_id: project_id.to_owned(),
                session_id: session_id.to_owned(),
                source_message_id: end_message_id.to_owned(),
                transcript: String::new(),
                sources: window.sources,
                sources_truncated: window.truncated,
                had_tool_calls,
                had_artifacts,
                explicit_feedback,
                recovered_from_error,
                user_corrected,
            },
            signals,
        ))
    }

    pub fn catch_up(
        &self,
        project_id: &str,
        scheduler: &LearningScheduler,
        now: i64,
        redact: &dyn Fn(&str) -> String,
    ) -> crate::Result<usize> {
        let mut queued = 0;
        for (session, message, completed) in self
            .sources
            .unlearned_turns(project_id, now.saturating_sub(7 * 86_400_000))?
        {
            let (request, signals) = self.request(project_id, &session, &message, false, redact)?;
            if matches!(
                scheduler.schedule_post_turn(request, signals, completed)?,
                LearningScheduleOutcome::Queued(_)
            ) {
                queued += 1;
            }
        }
        Ok(queued)
    }

    pub fn upgrade_legacy_job(
        &self,
        job: zuno_db::learning_job::LearningJobRecord,
        scheduler: &LearningScheduler,
        redact: &dyn Fn(&str) -> String,
    ) -> crate::Result<zuno_db::learning_job::LearningJobRecord> {
        if job.kind != zuno_db::learning_job::LearningJobKind::Extraction {
            return Ok(job);
        }
        let Some(payload) = job.payload.as_ref() else {
            return Ok(job);
        };
        let mut payload =
            crate::decode_extraction_job_payload(payload.clone()).map_err(|error| {
                crate::model::invalid(&format!("corrupt extraction payload: {error}"))
            })?;
        if !payload.request.sources.is_empty() {
            return Ok(job);
        }
        let (request, signals) = self.request(
            &payload.request.project_id,
            &payload.request.session_id,
            &payload.request.source_message_id,
            false,
            redact,
        )?;
        if signals.external_context && scheduler.excludes_external_context() {
            return Ok(job);
        }
        payload.request = request;
        scheduler.refresh_legacy_input(
            &job.id,
            &job.lease()?,
            &serde_json::to_value(payload).expect("extraction input"),
            zuno_db::message::now_millis(),
        )
    }
}

fn looks_like_correction(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "不对",
        "不是说",
        "我说的是",
        "纠正",
        "更正",
        "that's wrong",
        "that is wrong",
        "not what i",
        "correction:",
        "actually,",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}
