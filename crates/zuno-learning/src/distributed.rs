//! Data-owner contracts for scoped background extraction and maintenance.
//! These types register no worker, HTTP route, model or automatic behavior.
use serde::{Deserialize, Serialize};
use zuno_application::{learning::LearningExecutionLease, runtime::ConfigurationRef};
use zuno_types::identity::{JobId, PrincipalScope, SessionId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningPhase {
    Extraction,
    Maintenance,
    SkillEvaluation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "phase",
    content = "input",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LearningInput {
    Extraction(crate::ExtractionRequest),
    Maintenance(crate::MemoryConsolidationRequest),
    SkillEvaluation(SkillEvaluationInput),
}

impl LearningInput {
    pub fn phase(&self) -> LearningPhase {
        match self {
            Self::Extraction(_) => LearningPhase::Extraction,
            Self::Maintenance(_) => LearningPhase::Maintenance,
            Self::SkillEvaluation(_) => LearningPhase::SkillEvaluation,
        }
    }
    pub fn session(&self) -> &str {
        match self {
            Self::Extraction(input) => &input.session_id,
            Self::Maintenance(input) => &input.session_id,
            Self::SkillEvaluation(input) => &input.session_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "phase",
    content = "result",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LearningOutput {
    Extraction(crate::LearningExtraction),
    Maintenance(crate::MemoryConsolidation),
    SkillEvaluation(zuno_application::skill::SkillEvaluationReport),
}
impl LearningOutput {
    pub fn decode(phase: LearningPhase, text: &str) -> Result<Self, serde_json::Error> {
        let text = crate::model::strip_json_fence(text);
        match phase {
            LearningPhase::Extraction => serde_json::from_str(text).map(Self::Extraction),
            LearningPhase::Maintenance => serde_json::from_str(text).map(Self::Maintenance),
            LearningPhase::SkillEvaluation => serde_json::from_str(text).map(Self::SkillEvaluation),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningExecutionLimits {
    pub maximum_input_bytes: u32,
    pub maximum_output_tokens: u32,
    pub request_tokens: u64,
    pub total_tokens: u64,
    pub maximum_attempts: u32,
    pub duration_ms: u64,
}
impl LearningExecutionLimits {
    pub fn validate(&self) -> Result<(), zuno_application::ApplicationError> {
        if !(8192..=131072).contains(&self.maximum_input_bytes)
            || !(128..=8192).contains(&self.maximum_output_tokens)
            || self.request_tokens < u64::from(self.maximum_output_tokens)
            || self.total_tokens < self.request_tokens
            || self.total_tokens > 10_000_000
            || !(1..=3).contains(&self.maximum_attempts)
            || !(1000..=600000).contains(&self.duration_ms)
        {
            return Err(zuno_application::ApplicationError::Invalid(
                "invalid learning execution limits".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Created only from validated installed definitions in the control plane.
#[derive(Debug, Clone)]
pub struct MemoryLearningGrant {
    pub source: ConfigurationRef,
    pub extraction: ConfigurationRef,
    pub maintenance: ConfigurationRef,
    pub workspace: WorkspaceId,
    pub extraction_limits: LearningExecutionLimits,
    pub maintenance_limits: LearningExecutionLimits,
    pub extraction_model: crate::LearningModelIdentity,
    pub maintenance_model: crate::LearningModelIdentity,
}

#[derive(Debug, Clone)]
pub struct SkillLearningGrant {
    pub source: ConfigurationRef,
    pub evaluation: ConfigurationRef,
    pub workspace: WorkspaceId,
    pub limits: LearningExecutionLimits,
    pub model: crate::LearningModelIdentity,
    pub maximum_steps: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillEvaluationInput {
    pub session_id: String,
    pub candidate_id: zuno_types::identity::RequestId,
    pub baseline_content: String,
    pub proposed_content: String,
    pub cases: Vec<zuno_application::skill::SkillEvaluationCase>,
    pub maximum_steps: u32,
}
impl SkillEvaluationInput {
    pub fn validate(&self) -> Result<(), zuno_application::ApplicationError> {
        zuno_types::identity::SessionId::new(&self.session_id)
            .map_err(zuno_application::ApplicationError::storage)?;
        if !(1..=8).contains(&self.maximum_steps) {
            return Err(zuno_application::ApplicationError::Invalid(
                "invalid Skill evaluation step bound".to_owned(),
            ));
        }
        zuno_application::skill::ProposeSkill {
            request_id: self.candidate_id.clone(),
            name: "evaluation".to_owned(),
            baseline_content: self.baseline_content.clone(),
            proposed_content: self.proposed_content.clone(),
            cases: self.cases.clone(),
        }
        .validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningExecution {
    pub id: JobId,
    pub principal: PrincipalScope,
    pub workspace: WorkspaceId,
    pub session: SessionId,
    pub configuration: ConfigurationRef,
    pub input: LearningInput,
    pub input_digest: String,
    pub limits: LearningExecutionLimits,
    pub deadline_ms: i64,
    pub tokens_charged: u64,
    pub cached_output: Option<LearningOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimedLearning {
    pub lease: LearningExecutionLease,
    pub execution: LearningExecution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum LearningStop {
    Retry {
        after_ms: Option<u64>,
        detail: String,
    },
    Failed {
        code: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningCompletion {
    pub lease: LearningExecutionLease,
    pub result: LearningOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningJournalRequest {
    pub lease: LearningExecutionLease,
    pub record: crate::LearningModelRecord,
}
