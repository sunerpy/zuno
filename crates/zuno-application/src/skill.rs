//! Reviewed Skill candidates and reproducible, offline evaluation inputs.
//! No request can select a host path, live tool registry or model credential.
use crate::{ApplicationError, runtime::ConfigurationRef};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zuno_types::identity::PrincipalScope;
use zuno_types::identity::{JobId, RequestId};
use zuno_types::{activity::Counter, identity::WorkspaceId};

#[async_trait]
pub trait SkillApplication: Send + Sync {
    async fn propose(
        &self,
        actor: &PrincipalScope,
        source: &JobId,
        request: ProposeSkill,
    ) -> Result<SkillCandidateView, ApplicationError>;
    async fn candidate(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
    ) -> Result<SkillCandidateView, ApplicationError>;
    async fn evaluate(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
        request: ReviewSkillEvaluation,
    ) -> Result<SkillCandidateView, ApplicationError>;
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallSkill {
    pub request_id: RequestId,
    pub expected_digest: String,
    pub expected_revision: Counter,
    pub description: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivateSkill {
    pub request_id: RequestId,
    pub expected_revision: Counter,
    pub active: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RollbackSkill {
    pub request_id: RequestId,
    pub expected_revision: Counter,
    pub target_revision: Counter,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstalledSkillView {
    pub id: RequestId,
    pub workspace_id: WorkspaceId,
    pub candidate_id: RequestId,
    pub name: String,
    pub description: String,
    pub revision: Counter,
    pub source: String,
    pub content_digest: String,
    pub active: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstalledSkillPage {
    pub items: Vec<InstalledSkillView>,
    pub after: Option<RequestId>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstalledSkillDocument {
    pub skill: InstalledSkillView,
    pub content: String,
}
#[async_trait]
pub trait SkillLibrary: Send + Sync {
    async fn install(
        &self,
        actor: &PrincipalScope,
        candidate: &RequestId,
        request: InstallSkill,
    ) -> Result<InstalledSkillView, ApplicationError>;
    async fn activate(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
        request: ActivateSkill,
    ) -> Result<InstalledSkillView, ApplicationError>;
    async fn rollback(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
        request: RollbackSkill,
    ) -> Result<InstalledSkillView, ApplicationError>;
    async fn list(
        &self,
        actor: &PrincipalScope,
        workspace: &WorkspaceId,
        after: Option<&RequestId>,
        limit: crate::PageSize,
    ) -> Result<InstalledSkillPage, ApplicationError>;
    async fn document(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
    ) -> Result<InstalledSkillDocument, ApplicationError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillExecutionRequest {
    Catalog,
    Invoke {
        invocation_id: zuno_types::identity::InvocationId,
        arguments: Value,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillExecutionReply {
    Catalog {
        revision_digest: String,
        index: String,
    },
    Output {
        output: Box<zuno_tool::ToolOutput>,
        is_error: bool,
    },
}
#[async_trait]
pub trait SkillExecutionReader: Send + Sync {
    async fn active_documents(
        &self,
        lease: &crate::runtime::ExecutionLease,
    ) -> Result<Vec<InstalledSkillDocument>, ApplicationError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillCaseKind {
    Failure,
    Protection,
    General,
}
/// Shared policy for native and distributed paired evaluation. Scoring itself
/// comes from actual model observations; this only classifies their comparison.
pub fn skill_case_policy(
    kind: SkillCaseKind,
    baseline_score: i64,
    candidate_score: i64,
    baseline_passed: bool,
    candidate_passed: bool,
    candidate_critical: bool,
) -> (bool, bool) {
    (
        kind != SkillCaseKind::Failure || candidate_passed,
        kind == SkillCaseKind::Protection
            && baseline_passed
            && (!candidate_passed || candidate_critical || candidate_score < baseline_score),
    )
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillRecordedCall {
    pub name: String,
    pub arguments: Value,
    pub output: String,
    pub is_error: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillEvaluationCase {
    pub id: RequestId,
    pub prompt: String,
    pub expected: String,
    pub calls: Vec<SkillRecordedCall>,
    pub kind: SkillCaseKind,
    pub weight: u32,
}
impl SkillEvaluationCase {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.prompt.trim().is_empty()
            || self.expected.trim().is_empty()
            || self.prompt.len() > 16384
            || self.expected.len() > 16384
            || !(1..=10).contains(&self.weight)
            || self.calls.len() > 16
            || self.calls.iter().any(|call| {
                call.name.is_empty()
                    || call.name.len() > 128
                    || !call.arguments.is_object()
                    || call.output.len() > 16384
            })
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded Skill evaluation case".to_owned(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeSkill {
    pub request_id: RequestId,
    pub name: String,
    pub baseline_content: String,
    pub proposed_content: String,
    pub cases: Vec<SkillEvaluationCase>,
}
impl ProposeSkill {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let mut cases = std::collections::BTreeSet::new();
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.baseline_content.len() > 32768
            || self.proposed_content.trim().is_empty()
            || self.proposed_content.len() > 32768
            || self.cases.is_empty()
            || self.cases.len() > 8
            || self.cases.iter().any(|case| !cases.insert(case.id.clone()))
            || serde_json::to_vec(self)
                .map_err(ApplicationError::storage)?
                .len()
                > 131072
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded Skill candidate".to_owned(),
            ));
        }
        for case in &self.cases {
            case.validate()?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSkillEvaluation {
    pub request_id: RequestId,
    pub expected_digest: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvaluationState {
    PendingReview,
    Evaluating,
    Passed,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillCandidateView {
    pub id: RequestId,
    pub source_job_id: JobId,
    pub name: String,
    pub baseline_content: String,
    pub proposed_content: String,
    pub cases: Vec<SkillEvaluationCase>,
    pub digest: String,
    pub evaluation: ConfigurationRef,
    pub state: SkillEvaluationState,
    pub job_id: Option<JobId>,
    pub report: Option<SkillEvaluationReport>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillCaseObservation {
    #[schemars(range(min = 0, max = 100))]
    pub score: u8,
    pub passed: bool,
    pub critical_failure: bool,
    pub details: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillCaseResult {
    pub case_id: RequestId,
    pub baseline: SkillCaseObservation,
    pub candidate: SkillCaseObservation,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillEvaluationReport {
    pub passed: bool,
    pub baseline_metric: i64,
    pub candidate_metric: i64,
    pub cases: Vec<SkillCaseResult>,
}
impl SkillEvaluationReport {
    pub fn from_cases(
        cases: &[SkillEvaluationCase],
        results: Vec<SkillCaseResult>,
    ) -> Result<Self, ApplicationError> {
        if cases.is_empty() || cases.len() != results.len() {
            return Err(ApplicationError::Conflict);
        }
        let mut baseline_metric = 0i64;
        let mut candidate_metric = 0i64;
        let mut passed = true;
        for (case, result) in cases.iter().zip(&results) {
            case.validate()?;
            if case.id != result.case_id
                || result.baseline.score > 100
                || result.candidate.score > 100
            {
                return Err(ApplicationError::Conflict);
            }
            baseline_metric += i64::from(result.baseline.score) * i64::from(case.weight);
            candidate_metric += i64::from(result.candidate.score) * i64::from(case.weight);
            let (fixed, regressed) = skill_case_policy(
                case.kind,
                i64::from(result.baseline.score),
                i64::from(result.candidate.score),
                result.baseline.passed,
                result.candidate.passed,
                result.candidate.critical_failure,
            );
            passed &= fixed && !regressed;
        }
        passed &= candidate_metric >= baseline_metric;
        Ok(Self {
            passed,
            baseline_metric,
            candidate_metric,
            cases: results,
        })
    }
}
