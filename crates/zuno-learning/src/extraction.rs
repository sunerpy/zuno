use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionRequest {
    pub project_id: String,
    pub session_id: String,
    pub source_message_id: String,
    /// Reconstructable durable transcript and task evidence.
    #[serde(default)]
    pub transcript: String,
    /// Exact source addresses and bounded redacted contents selected by the host.
    #[serde(default)]
    pub sources: Vec<zuno_db::learning_source::LearningSource>,
    #[serde(default)]
    pub sources_truncated: bool,
    pub had_tool_calls: bool,
    pub had_artifacts: bool,
    pub recovered_from_error: bool,
    pub user_corrected: bool,
    pub explicit_feedback: bool,
}

impl ExtractionRequest {
    /// Present one unambiguous citation address without exposing the manifest's
    /// separate storage identifiers or host-only integrity digests. The durable
    /// request keeps the original manifest for strict settlement revalidation.
    pub(crate) fn model_input(&self) -> Value {
        let sources: Vec<_> = self
            .sources
            .iter()
            .map(|source| {
                serde_json::json!({
                    "source_id": source.reference_id,
                    "kind": source.kind,
                    "field": source.field,
                    "content": source.content,
                    "tool": source.tool,
                    "arguments": source.arguments,
                    "proves_success": source.proves_success,
                })
            })
            .collect();
        serde_json::json!({
            "sources": sources,
            "sources_truncated": self.sources_truncated,
            "had_tool_calls": self.had_tool_calls,
            "had_artifacts": self.had_artifacts,
            "recovered_from_error": self.recovered_from_error,
            "user_corrected": self.user_corrected,
            "explicit_feedback": self.explicit_feedback,
        })
    }

    /// Project the durable source manifest to one bounded request. Legacy
    /// transcript blobs are never sent alongside (or instead of) cited sources.
    pub fn bounded(mut self, max_bytes: usize) -> crate::Result<Self> {
        self.transcript.clear();
        let mut selected = Vec::new();
        let sources = std::mem::take(&mut self.sources);
        let base = serde_json::to_vec(&self)
            .expect("serializable request")
            .len();
        if base > max_bytes {
            return Err(crate::model::invalid(
                "learning input metadata exceeds its byte budget",
            ));
        }
        let mut used = base;
        for source in sources {
            let bytes = serde_json::to_vec(&source)
                .expect("serializable source")
                .len()
                + 1;
            if used.saturating_add(bytes) > max_bytes {
                self.sources_truncated = true;
                continue;
            }
            used += bytes;
            selected.push(source);
        }
        self.sources = selected;
        Ok(self)
    }
}

/// Why one durable extraction job was admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionTrigger {
    /// Automatic extraction after an eligible completed turn.
    AutomaticPostTurn,
    /// Explicit user-requested reflection.
    Manual,
    /// Bounded legacy repair; lower priority than newly completed turns.
    LegacyReprocess,
}

/// Durable extraction payload shared by automatic and explicit reflection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionJobPayload {
    pub trigger: ExtractionTrigger,
    pub request: ExtractionRequest,
    #[serde(
        default,
        rename = "sourceSnapshot",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_snapshot: Option<zuno_db::learning_source::LearningSourceSnapshot>,
}

impl ExtractionJobPayload {
    #[must_use]
    pub const fn automatic_post_turn(request: ExtractionRequest) -> Self {
        Self {
            trigger: ExtractionTrigger::AutomaticPostTurn,
            request,
            source_snapshot: None,
        }
    }

    #[must_use]
    pub const fn manual(request: ExtractionRequest) -> Self {
        Self {
            trigger: ExtractionTrigger::Manual,
            request,
            source_snapshot: None,
        }
    }

    #[must_use]
    pub fn into_request(self) -> ExtractionRequest {
        self.request
    }
}

/// Decode a durable extraction job, including the flat payload written by older
/// Zuno releases.
///
/// The legacy format did not record whether admission was automatic or manual.
/// It is classified as automatic because that was the default producer; this
/// helper only reconstructs execution input and does not re-run admission gates.
pub fn decode_extraction_job_payload(payload: Value) -> serde_json::Result<ExtractionJobPayload> {
    if payload.get("trigger").is_some() || payload.get("request").is_some() {
        serde_json::from_value(payload)
    } else {
        serde_json::from_value(payload).map(ExtractionJobPayload::automatic_post_turn)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LearningExtraction {
    #[serde(default)]
    pub experiences: Vec<ExtractedExperience>,
    #[serde(default)]
    pub memories: Vec<ExtractedMemory>,
}

impl LearningExtraction {
    pub fn validate_bounds(&self) -> crate::Result<()> {
        if self.experiences.len() > 32 || self.memories.len() > 16 {
            return Err(crate::model::invalid(
                "learning output exceeds the item limit",
            ));
        }
        for experience in &self.experiences {
            if experience.title.len() > 512
                || experience.summary.len() > 8192
                || experience
                    .resolution
                    .as_ref()
                    .is_some_and(|text| text.len() > 8192)
                || experience.evidence.len() > 16
                || experience.evidence.iter().any(|item| {
                    item.excerpt.len() > 4096
                        || item.source_id.as_ref().is_some_and(|id| id.len() > 512)
                })
            {
                return Err(crate::model::invalid(
                    "learning experience exceeds its field limit",
                ));
            }
        }
        for memory in &self.memories {
            if memory
                .content
                .as_ref()
                .is_some_and(|text| text.len() > 8192)
                || memory
                    .old_text
                    .as_ref()
                    .is_some_and(|text| text.len() > 8192)
                || memory.reason.len() > 2048
            {
                return Err(crate::model::invalid(
                    "learning memory exceeds its field limit",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedExperienceKind {
    Outcome,
    Problem,
    UnresolvedIssue,
    UserCorrection,
    ExplicitFeedback,
    Procedure,
}

impl From<ExtractedExperienceKind> for zuno_types::ExperienceKind {
    fn from(value: ExtractedExperienceKind) -> Self {
        match value {
            ExtractedExperienceKind::Outcome => Self::Outcome,
            ExtractedExperienceKind::Problem => Self::Problem,
            ExtractedExperienceKind::UnresolvedIssue => Self::UnresolvedIssue,
            ExtractedExperienceKind::UserCorrection => Self::UserCorrection,
            ExtractedExperienceKind::ExplicitFeedback => Self::ExplicitFeedback,
            ExtractedExperienceKind::Procedure => Self::Procedure,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtractedExperience {
    pub kind: ExtractedExperienceKind,
    pub title: String,
    pub summary: String,
    pub resolution: Option<String>,
    pub confidence: f64,
    #[serde(default)]
    pub evidence: Vec<ExtractedEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedEvidenceKind {
    Message,
    Tool,
    Feedback,
    Artifact,
    User,
}

impl From<ExtractedEvidenceKind> for zuno_db::experience::ExperienceEvidenceKind {
    fn from(value: ExtractedEvidenceKind) -> Self {
        match value {
            ExtractedEvidenceKind::Message => Self::Message,
            ExtractedEvidenceKind::Tool => Self::Tool,
            ExtractedEvidenceKind::Feedback => Self::Feedback,
            ExtractedEvidenceKind::Artifact => Self::Artifact,
            ExtractedEvidenceKind::User => Self::User,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtractedEvidence {
    pub kind: ExtractedEvidenceKind,
    pub source_id: Option<String>,
    pub excerpt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedMemoryScope {
    Global,
    Project,
}

impl From<ExtractedMemoryScope> for zuno_types::MemoryScope {
    fn from(value: ExtractedMemoryScope) -> Self {
        match value {
            ExtractedMemoryScope::Global => Self::Global,
            ExtractedMemoryScope::Project => Self::Project,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedMemoryAction {
    Add,
    Replace,
    Remove,
}

impl From<ExtractedMemoryAction> for zuno_types::MemoryAction {
    fn from(value: ExtractedMemoryAction) -> Self {
        match value {
            ExtractedMemoryAction::Add => Self::Add,
            ExtractedMemoryAction::Replace => Self::Replace,
            ExtractedMemoryAction::Remove => Self::Remove,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtractedMemory {
    /// Ordinal in `experiences` that supplies the evidence and promotion guard.
    pub experience_ordinal: usize,
    pub scope: ExtractedMemoryScope,
    pub action: ExtractedMemoryAction,
    pub content: Option<String>,
    pub old_text: Option<String>,
    pub reason: String,
    pub confidence: f64,
}

#[async_trait]
pub trait LearningExtractor: Send + Sync {
    fn version(&self) -> &str;

    /// Freeze the exact evidence admitted to the provider before the job executes.
    fn prepare_request(&self, request: ExtractionRequest) -> crate::Result<ExtractionRequest> {
        Ok(request)
    }

    /// Extract structured data. Implementations receive no tool registry, file
    /// handle, or network client from this interface.
    async fn extract(&self, request: ExtractionRequest) -> crate::Result<LearningExtraction>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> ExtractionRequest {
        ExtractionRequest {
            sources: Vec::new(),
            sources_truncated: false,
            project_id: "project-1".to_owned(),
            session_id: "session-1".to_owned(),
            source_message_id: "assistant-1".to_owned(),
            transcript: "durable transcript".to_owned(),
            had_tool_calls: true,
            had_artifacts: false,
            recovered_from_error: false,
            user_corrected: false,
            explicit_feedback: false,
        }
    }

    #[test]
    fn current_job_payload_round_trips_its_trigger_and_request() {
        for payload in [
            ExtractionJobPayload::automatic_post_turn(request()),
            ExtractionJobPayload::manual(request()),
        ] {
            let encoded = serde_json::to_value(&payload).expect("serialize payload");
            let decoded = decode_extraction_job_payload(encoded).expect("decode payload");
            assert_eq!(decoded, payload);
        }
    }

    #[test]
    fn legacy_flat_request_decodes_as_an_automatic_job() {
        let legacy = serde_json::to_value(request()).expect("serialize legacy request");
        let decoded = decode_extraction_job_payload(legacy).expect("decode legacy payload");

        assert_eq!(decoded.trigger, ExtractionTrigger::AutomaticPostTurn);
        assert_eq!(decoded.request, request());
    }

    #[test]
    fn malformed_job_payload_is_rejected() {
        let error = decode_extraction_job_payload(json!({
            "trigger": "automatic_post_turn",
            "request": {"session_id": "session-1"}
        }))
        .expect_err("incomplete request must fail");

        assert!(error.is_data());
    }
}
