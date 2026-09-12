/* Generated from zuno-application/api.rs. Do not edit. */

export type ApprovalAnswer = "approve" | "reject";
export type RequestId = string;
export type ApprovalAudience = "requester" | "designatedApprover";
/**
 * Execution semantics supplied by a registered handler, never a UI hint or
 * the `readOnlyHint` annotation of an arbitrary remote MCP tool.
 */
export type EffectKind =
  | "fileRead"
  | "fileList"
  | "fileSearch"
  | "fileWrite"
  | "process"
  | "network"
  | "externalTool"
  | "memoryRead"
  | "memoryWrite"
  | "unknown";
export type InvocationId = string;
export type JobId = string;
export type OperationId = string;
export type SessionId = string;
export type TurnId = string;
export type ApprovalId = string;
export type PrincipalId = string;
export type TenantId = string;
export type ApprovalState = "pending" | "automatic" | "approved" | "rejected" | "expired" | "invalidated";
export type WorkspaceId = string;
export type InputId = string;
export type JobPhase = "ready" | "running" | "waiting" | "paused" | "completed" | "failed" | "cancelled" | "uncertain";
/**
 * Scheduling and UI can distinguish causes without inspecting tool names.
 */
export type WaitTarget =
  | {
      approval_id: ApprovalId;
      kind: "approval";
    }
  | {
      kind: "user_input";
      request_id: RequestId;
    }
  | {
      job_id: JobId;
      kind: "child";
    }
  | {
      kind: "operation";
      operation_id: OperationId;
    }
  | {
      deadline_ms: number;
      kind: "timer";
    };
export type Counter = string;
export type CouncilPhase = "seats" | "stopping" | "synthesis" | "completed" | "failed" | "cancelled" | "uncertain";
export type CouncilSeatState =
  | "pending"
  | "running"
  | "waiting"
  | "retrying"
  | "completed"
  | "invalid"
  | "failed"
  | "timed_out"
  | "cancelled"
  | "uncertain";
export type WorkflowRunId = string;
export type WorkflowKind = "workflow" | "council";
export type NodeRunId = string;
export type InvocationState =
  "queued" | "waiting" | "running" | "succeeded" | "failed" | "denied" | "cancelled" | "uncertain";
export type WorkflowState = "preparing" | "prepared" | "active" | "completed" | "failed" | "cancelled" | "uncertain";

export interface ApplicationProtocol {
  answer: ApprovalDecision;
  approval: ApprovalView;
  cancel: CancelJob;
  cancellation: CancellationReceipt;
  create_session: CreateSession;
  input_version: InputVersionView;
  job: JobView;
  session: SessionSummary;
  sessions: SessionPage;
  submit_turn: SubmitTurn;
  workflow: WorkflowRunView;
  workspace: WorkspaceView;
}
export interface ApprovalDecision {
  answer: ApprovalAnswer;
  requestId: RequestId;
}
export interface ApprovalView {
  audience: ApprovalAudience;
  binding: ApprovalBinding;
  expiresAtMs: number;
  id: ApprovalId;
  presentation: unknown;
  requester: PrincipalKey;
  state: ApprovalState;
}
/**
 * Stable authorization target. Attempt/worker/epoch are intentionally absent.
 * The execution permission check binds the current lease separately.
 */
export interface ApprovalBinding {
  argumentsSha256: string;
  effect: EffectKind;
  invocationId: InvocationId;
  jobId: JobId;
  operationId: OperationId;
  /**
   * Digest of the gateway's resolved resource identities and versions.
   */
  resourcesSha256: string;
  sessionId: SessionId;
  turnId: TurnId;
}
/**
 * A private resource's stable owner. This value is not a permission grant.
 */
export interface PrincipalKey {
  principalId: PrincipalId;
  tenantId: TenantId;
}
export interface CancelJob {
  expectedTurnId: TurnId;
  reason: string;
  requestId: RequestId;
}
/**
 * The logical tree is fenced when this receipt commits. External operations
 * have their own observed receipts; this does not claim their processes stopped.
 */
export interface CancellationReceipt {
  jobId: JobId;
  pendingOperations: OperationId[];
  requestId: RequestId;
  stoppedJobs: JobId[];
  turnId: TurnId;
}
export interface CreateSession {
  requestId: RequestId;
  title: string;
  workspaceId: WorkspaceId;
}
export interface InputVersionView {
  version: string;
}
export interface JobView {
  id: JobId;
  inputId: InputId;
  /**
   * Decimal strings preserve exact counters in JavaScript clients.
   */
  inputVersion: string;
  pendingOperations: OperationId[];
  phase: JobPhase;
  sessionId: SessionId;
  stopRequested: boolean;
  turnId: TurnId;
  /**
   * Public waiting coordinates, without arguments, checkpoints or grants.
   */
  waits: JobWaitView[];
}
export interface JobWaitView {
  invocationId: InvocationId;
  target: WaitTarget;
}
/**
 * A session's public summary. Filesystem locations remain backend-owned.
 */
export interface SessionSummary {
  createdAt: number;
  id: SessionId;
  title: string;
  updatedAt: number;
  workspaceId?: WorkspaceId | null;
}
export interface SessionPage {
  items: SessionSummary[];
  next?: SessionCursor | null;
}
/**
 * Both ordering keys are needed when sessions have equal timestamps.
 */
export interface SessionCursor {
  sessionId: SessionId;
  updatedAt: number;
}
export interface SubmitTurn {
  expectedInputVersion: string;
  requestId: RequestId;
  text: string;
}
export interface WorkflowRunView {
  council?: CouncilView | null;
  id: WorkflowRunId;
  jobId: JobId;
  kind: WorkflowKind;
  name: string;
  nodes: NodeRunView[];
  state: WorkflowState;
}
export interface CouncilView {
  deadline: Counter;
  phase: CouncilPhase;
  preset: string;
  quorum: number;
  seatDeadline: Counter;
  seats: CouncilSeatView[];
  synthesisDeadline?: Counter | null;
}
export interface CouncilSeatView {
  attempts: number;
  id: string;
  jobId: JobId;
  state: CouncilSeatState;
}
export interface NodeRunView {
  dependsOn: string[];
  id: NodeRunId;
  jobId: JobId;
  nodeId: string;
  state: InvocationState;
  waits: JobWaitView[];
}
export interface WorkspaceView {
  id: WorkspaceId;
  title: string;
}
