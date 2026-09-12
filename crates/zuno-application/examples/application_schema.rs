use schemars::{JsonSchema, schema_for};
use zuno_application::{
    CreateSession, SessionPage, SessionSummary,
    api::{ApprovalDecision, ApprovalView, InputVersionView, JobView, SubmitTurn, WorkspaceView},
    control::{CancelJob, CancellationReceipt},
    workflow::WorkflowRunView,
};

#[derive(JsonSchema)]
#[allow(
    dead_code,
    reason = "schema-only root collects public application DTOs for generated clients"
)]
struct ApplicationProtocol {
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
}
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&schema_for!(ApplicationProtocol)).expect("schema serializes")
    );
}
