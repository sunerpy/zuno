use schemars::{JsonSchema, schema_for};
use zuno_application::{
    CreateSession, SessionPage, SessionSummary,
    api::{
        ActorView, ApprovalDecision, ApprovalView, InputVersionView, JobView, SubmitTurn,
        WorkspaceView,
    },
    control::{CancelJob, CancellationReceipt},
    learning_api::{
        CancelLearning, LearningCancellation, LearningJobView, LearningPage, LearningPageRequest,
    },
    mcp::McpCallView,
    shared_memory::{
        ConfigureSharedMemory, ProposeSharedMemory, ReviewSharedMemory, RevokeSharedEvidence,
        ShareMemoryEvidence, SharedEvidenceGrant, SharedEvidencePage, SharedMemoryChange,
        SharedMemoryPage, SharedMemorySpace,
    },
    skill::{
        ActivateSkill, InstallSkill, InstalledSkillDocument, InstalledSkillPage,
        InstalledSkillView, ProposeSkill, ReviewSkillEvaluation, RollbackSkill, SkillCandidateView,
    },
    workflow::WorkflowRunView,
    workspace_edit::WorkspaceEditView,
    workspace_import::{BeginWorkspaceImport, WorkspaceImportView},
    workspace_merge::{MergeContentRequest, WorkspaceMergeView},
};

#[derive(JsonSchema)]
#[allow(
    dead_code,
    reason = "schema-only root collects public application DTOs for generated clients"
)]
struct ApplicationProtocol {
    actor: ActorView,
    skill_candidate: SkillCandidateView,
    propose_skill: ProposeSkill,
    review_skill: ReviewSkillEvaluation,
    install_skill: InstallSkill,
    activate_skill: ActivateSkill,
    rollback_skill: RollbackSkill,
    installed_skill: InstalledSkillView,
    installed_skills: InstalledSkillPage,
    installed_skill_document: InstalledSkillDocument,
    shared_memory_space: SharedMemorySpace,
    shared_memory_page: SharedMemoryPage,
    shared_memory_change: SharedMemoryChange,
    configure_shared_memory: ConfigureSharedMemory,
    propose_shared_memory: ProposeSharedMemory,
    review_shared_memory: ReviewSharedMemory,
    share_evidence: ShareMemoryEvidence,
    revoke_evidence: RevokeSharedEvidence,
    evidence_grant: SharedEvidenceGrant,
    evidence_page: SharedEvidencePage,
    workspace: WorkspaceView,
    session: SessionSummary,
    sessions: SessionPage,
    create_session: CreateSession,
    job: JobView,
    submit_turn: SubmitTurn,
    input_version: InputVersionView,
    approval: ApprovalView,
    answer: ApprovalDecision,
    cancel: CancelJob,
    cancellation: CancellationReceipt,
    workflow: WorkflowRunView,
    workspace_merge: WorkspaceMergeView,
    workspace_edit: WorkspaceEditView,
    mcp_call: McpCallView,
    merge_content: MergeContentRequest,
    begin_workspace_import: BeginWorkspaceImport,
    workspace_import: WorkspaceImportView,
    learning_job: LearningJobView,
    learning_jobs: LearningPage,
    learning_query: LearningPageRequest,
    cancel_learning: CancelLearning,
    learning_cancellation: LearningCancellation,
}
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&schema_for!(ApplicationProtocol)).expect("schema serializes")
    );
}
