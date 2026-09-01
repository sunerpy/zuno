use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zuno_error::BoxSource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionRequest {
    pub project_id: String,
    pub session_id: String,
    pub source_message_id: String,
    /// Reconstructable durable transcript and task evidence.
    pub transcript: String,
    pub had_tool_calls: bool,
    pub had_artifacts: bool,
    pub recovered_from_error: bool,
    pub user_corrected: bool,
    pub explicit_feedback: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningExtraction {
    #[serde(default)]
    pub experiences: Vec<ExtractedExperience>,
    #[serde(default)]
    pub memories: Vec<ExtractedMemory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedExperience {
    pub kind: ExtractedExperienceKind,
    pub title: String,
    pub summary: String,
    pub resolution: Option<String>,
    pub confidence: f64,
    #[serde(default)]
    pub evidence: Vec<ExtractedEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedEvidence {
    pub kind: ExtractedEvidenceKind,
    pub source_id: Option<String>,
    pub excerpt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// Extract structured data. Implementations receive no tool registry, file
    /// handle, or network client from this interface.
    async fn extract(&self, request: ExtractionRequest) -> Result<LearningExtraction, BoxSource>;
}
