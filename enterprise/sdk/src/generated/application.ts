/* Generated from zuno-application/api.rs. Do not edit. */

export type Counter = string;
export type RequestId = string;
export type ClientId = string;
/**
 * How the host obtained a subject. This is diagnostic data, not a permission.
 */
export type PrincipalKind = "local" | "user" | "application" | "workload";
export type PrincipalId = string;
export type TenantId = string;
export type ApprovalAnswer = "approve" | "reject";
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
export type ApprovalState = "pending" | "automatic" | "approved" | "rejected" | "expired" | "invalidated";
export type SharedMemoryRole = "reader" | "contributor" | "reviewer";
export type WorkspaceId = string;
export type SharedEvidenceKind = "user_statement" | "successful_operation";
export type MemorySpaceId = string;
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
export type LearningStage = "extraction" | "maintenance" | "skill_evaluation";
export type LearningState = "queued" | "running" | "completed" | "skipped" | "failed" | "cancelled" | "uncertain";
export type ActivityName = string;
export type WorkspacePath = string;
export type MergeContentSide = "base" | "parent" | "child";
export type SharedMemoryEdit =
  | {
      content: string;
      kind: "add";
    }
  | {
      content: string;
      kind: "replace";
      oldText: string;
    }
  | {
      kind: "remove";
      oldText: string;
    };
export type SkillCaseKind = "failure" | "protection" | "general";
export type SharedMemoryDecision = "apply" | "reject" | "undo";
export type SharedMemoryChangeState = "pending" | "applied" | "rejected" | "undone" | "invalidated";
export type ConfigurationId = string;
export type SkillEvaluationState = "pending_review" | "evaluating" | "passed" | "failed" | "cancelled";
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
export type WorkspaceImportId = string;
export type WorkspaceImportState = "uploading" | "initializing" | "ready" | "cancelled";
export type WorkspaceEntry =
  | {
      gid: number;
      kind: "directory";
      mode: number;
      uid: number;
    }
  | {
      bytes: Counter;
      gid: number;
      kind: "file";
      mode: number;
      sha256: string;
      uid: number;
    }
  | {
      gid: number;
      kind: "symlink";
      target: string;
      uid: number;
    }
  | {
      kind: "hardlink";
      target: WorkspacePath;
    };
export type MergeChoice = "parent" | "child" | "conflict";

export interface ApplicationProtocol {
  activate_skill: ActivateSkill;
  actor: ActorView;
  answer: ApprovalDecision;
  approval: ApprovalView;
  begin_workspace_import: BeginWorkspaceImport;
  cancel: CancelJob;
  cancel_learning: CancelLearning;
  cancellation: CancellationReceipt;
  configure_shared_memory: ConfigureSharedMemory;
  create_session: CreateSession;
  evidence_grant: SharedEvidenceGrant;
  evidence_page: SharedEvidencePage;
  input_version: InputVersionView;
  install_skill: InstallSkill;
  installed_skill: InstalledSkillView;
  installed_skill_document: InstalledSkillDocument;
  installed_skills: InstalledSkillPage;
  job: JobView;
  learning_cancellation: LearningCancellation;
  learning_job: LearningJobView;
  learning_jobs: LearningPage;
  learning_query: LearningPageRequest;
  mcp_call: McpCallView;
  merge_content: MergeContentRequest;
  propose_shared_memory: ProposeSharedMemory;
  propose_skill: ProposeSkill;
  review_shared_memory: ReviewSharedMemory;
  review_skill: ReviewSkillEvaluation;
  revoke_evidence: RevokeSharedEvidence;
  rollback_skill: RollbackSkill;
  session: SessionSummary;
  sessions: SessionPage;
  share_evidence: ShareMemoryEvidence;
  shared_memory_change: SharedMemoryChange;
  shared_memory_page: SharedMemoryPage;
  shared_memory_space: SharedMemorySpace;
  skill_candidate: SkillCandidateView;
  submit_turn: SubmitTurn;
  workflow: WorkflowRunView;
  workspace: WorkspaceView;
  workspace_edit: WorkspaceEditView;
  workspace_import: WorkspaceImportView;
  workspace_merge: WorkspaceMergeView;
}
export interface ActivateSkill {
  active: boolean;
  expectedRevision: Counter;
  requestId: RequestId;
}
/**
 * Public authenticated actor coordinates. No bearer token or service grant.
 */
export interface ActorView {
  clientId?: ClientId | null;
  kind: PrincipalKind;
  owner: PrincipalKey;
}
/**
 * A private resource's stable owner. This value is not a permission grant.
 */
export interface PrincipalKey {
  principalId: PrincipalId;
  tenantId: TenantId;
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
export interface BeginWorkspaceImport {
  bytes: Counter;
  expectedInputVersion: Counter;
  requestId: RequestId;
  sha256: string;
}
export interface CancelJob {
  expectedTurnId: TurnId;
  reason: string;
  requestId: RequestId;
}
export interface CancelLearning {
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
export interface ConfigureSharedMemory {
  characterLimit: number;
  enabled: boolean;
  expectedRevision: Counter;
  members: SharedMemoryMember[];
  requestId: RequestId;
  title: string;
  workspaceId: WorkspaceId;
}
export interface SharedMemoryMember {
  principalId: PrincipalId;
  role: SharedMemoryRole;
}
export interface CreateSession {
  requestId: RequestId;
  title: string;
  workspaceId: WorkspaceId;
}
export interface SharedEvidenceGrant {
  active: boolean;
  author: PrincipalId;
  current: boolean;
  evidenceDigest: string;
  excerpt: string;
  id: RequestId;
  kind: SharedEvidenceKind;
  revision: Counter;
  spaceId: MemorySpaceId;
}
export interface SharedEvidencePage {
  after?: RequestId | null;
  items: SharedEvidenceGrant[];
}
export interface InputVersionView {
  version: string;
}
export interface InstallSkill {
  description: string;
  expectedDigest: string;
  expectedRevision: Counter;
  requestId: RequestId;
}
export interface InstalledSkillView {
  active: boolean;
  candidateId: RequestId;
  contentDigest: string;
  description: string;
  id: RequestId;
  name: string;
  revision: Counter;
  source: string;
  workspaceId: WorkspaceId;
}
export interface InstalledSkillDocument {
  content: string;
  skill: InstalledSkillView;
}
export interface InstalledSkillPage {
  after?: RequestId | null;
  items: InstalledSkillView[];
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
export interface LearningCancellation {
  job: LearningJobView;
  requestId: RequestId;
}
export interface LearningJobView {
  attempts: Counter;
  budget: LearningBudgetView;
  canCancel: boolean;
  createdAtMs: Counter;
  deadlineAtMs?: Counter | null;
  failure?: LearningFailureView | null;
  id: JobId;
  readyAtMs?: Counter | null;
  sessionId: SessionId;
  sourceJobId: JobId;
  stage: LearningStage;
  state: LearningState;
  updatedAtMs: Counter;
  workspaceId: WorkspaceId;
}
export interface LearningBudgetView {
  charged: Counter;
  limit: Counter;
  modelRequests: Counter;
  reserved: Counter;
  unconfirmedRequests: Counter;
}
export interface LearningFailureView {
  code: string;
  message?: string | null;
}
export interface LearningPage {
  before?: LearningCursor | null;
  items: LearningJobView[];
}
export interface LearningCursor {
  createdAtMs: Counter;
  jobId: JobId;
}
export interface LearningPageRequest {
  before?: LearningCursor | null;
  limit?: number;
  stage?: LearningStage | null;
  state?: LearningState | null;
}
/**
 * Human review of the full frozen declaration, target and arguments. No
 * execution lease, token or gateway credential is exposed.
 */
export interface McpCallView {
  admitted: boolean;
  approvalId: ApprovalId;
  arguments: unknown;
  definition: unknown;
  endpoint: string;
  operationId: OperationId;
  server: ActivityName;
  tool: ActivityName;
}
export interface MergeContentRequest {
  approvalId: ApprovalId;
  path: WorkspacePath;
  side: MergeContentSide;
}
export interface ProposeSharedMemory {
  edits: SharedMemoryEdit[];
  evidence?: SharedEvidenceBinding[];
  expectedRevision: Counter;
  reason: string;
  requestId: RequestId;
}
export interface SharedEvidenceBinding {
  content: string;
  grants: RequestId[];
}
export interface ProposeSkill {
  baselineContent: string;
  cases: SkillEvaluationCase[];
  name: string;
  proposedContent: string;
  requestId: RequestId;
}
export interface SkillEvaluationCase {
  calls: SkillRecordedCall[];
  expected: string;
  id: RequestId;
  kind: SkillCaseKind;
  prompt: string;
  weight: number;
}
export interface SkillRecordedCall {
  arguments: unknown;
  isError: boolean;
  name: string;
  output: string;
}
export interface ReviewSharedMemory {
  changeId: RequestId;
  decision: SharedMemoryDecision;
  expectedState: string;
  requestId: RequestId;
}
export interface ReviewSkillEvaluation {
  expectedDigest: string;
  requestId: RequestId;
}
export interface RevokeSharedEvidence {
  expectedRevision: Counter;
  requestId: RequestId;
}
export interface RollbackSkill {
  expectedRevision: Counter;
  requestId: RequestId;
  targetRevision: Counter;
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
export interface ShareMemoryEvidence {
  evidenceId: string;
  expectedDigest: string;
  requestId: RequestId;
}
export interface SharedMemoryChange {
  after: string[];
  appliedRevision?: Counter | null;
  author: PrincipalId;
  baseRevision: Counter;
  before: string[];
  decidedBy?: PrincipalId | null;
  evidence?: SharedEvidenceTransition | null;
  id: RequestId;
  policyRevision: Counter;
  reason: string;
  spaceId: MemorySpaceId;
  state: SharedMemoryChangeState;
  stateDigest: string;
}
export interface SharedEvidenceTransition {
  after: SharedEvidenceBinding[];
  before: SharedEvidenceBinding[];
}
export interface SharedMemoryPage {
  after?: MemorySpaceId | null;
  items: SharedMemorySpace[];
}
export interface SharedMemorySpace {
  characterLimit: number;
  digest: string;
  documentRevision: Counter;
  enabled: boolean;
  entries: string[];
  id: MemorySpaceId;
  policyRevision: Counter;
  role: SharedMemoryRole;
  /**
   * Reviewed entries retained for history but not eligible for current recall.
   */
  suppressed?: string[];
  title: string;
  workspaceId: WorkspaceId;
}
export interface SkillCandidateView {
  baselineContent: string;
  cases: SkillEvaluationCase[];
  digest: string;
  evaluation: ConfigurationRef;
  id: RequestId;
  jobId?: JobId | null;
  name: string;
  proposedContent: string;
  report?: SkillEvaluationReport | null;
  sourceJobId: JobId;
  state: SkillEvaluationState;
}
export interface ConfigurationRef {
  id: ConfigurationId;
  sha256: string;
  version: number;
}
export interface SkillEvaluationReport {
  baselineMetric: number;
  candidateMetric: number;
  cases: SkillCaseResult[];
  passed: boolean;
}
export interface SkillCaseResult {
  baseline: SkillCaseObservation;
  candidate: SkillCaseObservation;
  caseId: RequestId;
}
export interface SkillCaseObservation {
  criticalFailure: boolean;
  details: unknown;
  passed: boolean;
  score: number;
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
export interface WorkspaceEditView {
  admitted: boolean;
  approvalId: ApprovalId;
  operationId: OperationId;
  review: WorkspaceEditReview[];
}
export interface WorkspaceEditReview {
  after?: string | null;
  before?: string | null;
  path: WorkspacePath;
}
export interface WorkspaceImportView {
  bytes: Counter;
  createdAt: Counter;
  id: WorkspaceImportId;
  sessionId: SessionId;
  sha256: string;
  state: WorkspaceImportState;
}
export interface WorkspaceMergeView {
  admitted: boolean;
  approvalId: ApprovalId;
  childJobId: JobId;
  operationId: OperationId;
  plan: WorkspaceMergePlan;
}
export interface WorkspaceMergePlan {
  baseTree: string;
  changes: WorkspaceChange[];
  childTree: string;
  parentTree: string;
}
export interface WorkspaceChange {
  base?: WorkspaceEntry | null;
  child?: WorkspaceEntry | null;
  choice: MergeChoice;
  parent?: WorkspaceEntry | null;
  path: WorkspacePath;
}
