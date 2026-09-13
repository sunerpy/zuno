import { ActivityClient } from "./client.js";
import type {
  ActorView, WorkspaceView, SessionPage, SessionSummary, CreateSession, JobView,
  QuotaPolicy, QuotaSnapshot, ReplaceQuotaPolicy,
  SubmitTurn, InputVersionView, ApprovalView, ApprovalDecision, CancelJob, CancellationReceipt, WorkflowRunView, WorkspaceMergeView, MergeContentSide, BeginWorkspaceImport, WorkspaceImportView,
  LearningJobView, LearningPage, LearningPageRequest, CancelLearning, LearningCancellation,
  WorkspaceEditView, McpCallView,
  SharedMemorySpace, SharedMemoryPage, SharedMemoryChange, ConfigureSharedMemory, ProposeSharedMemory, ReviewSharedMemory,
  ShareMemoryEvidence, RevokeSharedEvidence, SharedEvidenceGrant, SharedEvidencePage,
  ProposeSkill, ReviewSkillEvaluation, SkillCandidateView,
  InstallSkill, ActivateSkill, RollbackSkill, InstalledSkillView, InstalledSkillPage, InstalledSkillDocument,
} from "./generated/application.js";
import {
  validateActorView, validateWorkspaceView, validateSessionPage, validateSessionSummary, validateJobView,
  validateQuotaPolicy, validateQuotaSnapshot, validateReplaceQuotaPolicy,
  validateInputVersionView, validateApprovalView, validateCancellationReceipt, validateWorkflowRunView, validateWorkspaceMergeView, validateMergeContentRequest, validateWorkspaceImportView,
  validateLearningJobView, validateLearningPage, validateLearningPageRequest, validateLearningCancellation,
  validateWorkspaceEditView, validateMcpCallView,
  validateSharedMemorySpace, validateSharedMemoryPage, validateSharedMemoryChange,
  validateConfigureSharedMemory, validateProposeSharedMemory, validateReviewSharedMemory,
  validateShareMemoryEvidence, validateRevokeSharedEvidence, validateSharedEvidenceGrant, validateSharedEvidencePage,
  validateProposeSkill,validateReviewSkillEvaluation,validateSkillCandidateView,
  validateInstallSkill,validateActivateSkill,validateRollbackSkill,validateInstalledSkillView,validateInstalledSkillPage,validateInstalledSkillDocument,
} from "./generated/application-validators.mjs";

function checked<T>(value: unknown, validate: (value: unknown) => unknown): T {
  if (!validate(value)) throw new Error("Invalid enterprise application response");
  return value as T;
}
function id(value: string): string {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(value)) throw new Error("Invalid resource ID");
  return encodeURIComponent(value);
}

export class EnterpriseClient extends ActivityClient {
  async quotas(signal?: AbortSignal): Promise<QuotaSnapshot> {
    return checked<QuotaSnapshot>(await this.get(new URL("quotas",this.base),signal),validateQuotaSnapshot);
  }
  async replaceQuotas(request: ReplaceQuotaPolicy, signal?: AbortSignal): Promise<QuotaPolicy> {
    if(!validateReplaceQuotaPolicy(request)) throw new Error("Invalid quota replacement");
    const value=checked<QuotaPolicy>(await this.get(new URL("quotas",this.base),signal,"PUT",request),validateQuotaPolicy);
    if(BigInt(value.revision)!==BigInt(request.expectedRevision)+1n || Object.entries(request.limits).some(([key,limit])=>value.limits[key as keyof typeof value.limits]!==limit)) throw new Error("Quota revision mismatch");
    return value;
  }
  async shareMemoryEvidence(space: string, request: ShareMemoryEvidence, signal?: AbortSignal): Promise<SharedEvidenceGrant> {
    if(!validateShareMemoryEvidence(request)) throw new Error("Invalid shared evidence request");
    const value=checked<SharedEvidenceGrant>(await this.get(new URL(`memory/spaces/${id(space)}/evidence`,this.base),signal,"POST",request),validateSharedEvidenceGrant);
    if(value.spaceId!==space || value.evidenceDigest!==request.expectedDigest) throw new Error("Shared evidence identity mismatch");
    return value;
  }
  async sharedMemoryEvidence(space: string, after?: string, signal?: AbortSignal): Promise<SharedEvidencePage> {
    const url=new URL(`memory/spaces/${id(space)}/evidence`,this.base);
    if(after) url.searchParams.set("after",id(after));
    const page=checked<SharedEvidencePage>(await this.get(url,signal),validateSharedEvidencePage);
    if(page.items.some(value=>value.spaceId!==space) || (page.after && page.items.at(-1)?.id!==page.after)) throw new Error("Shared evidence identity mismatch");
    return page;
  }
  async revokeSharedEvidence(space: string, evidence: string, request: RevokeSharedEvidence, signal?: AbortSignal): Promise<SharedEvidenceGrant> {
    if(!validateRevokeSharedEvidence(request)) throw new Error("Invalid evidence revocation");
    const value=checked<SharedEvidenceGrant>(await this.get(new URL(`memory/spaces/${id(space)}/evidence/${id(evidence)}/revoke`,this.base),signal,"POST",request),validateSharedEvidenceGrant);
    if(value.id!==evidence || value.spaceId!==space || value.active || value.current || BigInt(value.revision)!==BigInt(request.expectedRevision)+1n) throw new Error("Shared evidence identity mismatch");
    return value;
  }
  async identity(signal?: AbortSignal): Promise<ActorView> {
    return checked<ActorView>(await this.get(new URL("identity",this.base),signal),validateActorView);
  }
  async installSkill(candidate: string, request: InstallSkill, signal?: AbortSignal): Promise<InstalledSkillView> {
    if (!validateInstallSkill(request)) throw new Error("Invalid Skill installation");
    const value=checked<InstalledSkillView>(await this.get(new URL(`skills/${id(candidate)}/install`,this.base),signal,"POST",request),validateInstalledSkillView);
    if(value.candidateId!==candidate || value.active || BigInt(value.revision)!==BigInt(request.expectedRevision)+1n) throw new Error("Skill installation identity mismatch");
    return value;
  }
  async installedSkills(workspace: string, after?: string, signal?: AbortSignal): Promise<InstalledSkillPage> {
    const url=new URL(`workspaces/${id(workspace)}/skills`,this.base);
    if(after) url.searchParams.set("after",id(after));
    const page=checked<InstalledSkillPage>(await this.get(url,signal),validateInstalledSkillPage);
    if(page.items.some(value=>value.workspaceId!==workspace)) throw new Error("Skill workspace identity mismatch");
    if(page.after && page.items.at(-1)?.id!==page.after) throw new Error("Skill cursor identity mismatch");
    return page;
  }
  async installedSkill(skill: string, signal?: AbortSignal): Promise<InstalledSkillDocument> {
    const value=checked<InstalledSkillDocument>(await this.get(new URL(`installed-skills/${id(skill)}`,this.base),signal),validateInstalledSkillDocument);
    if(value.skill.id!==skill) throw new Error("Skill installation identity mismatch");
    return value;
  }
  async activateSkill(skill: string, request: ActivateSkill, signal?: AbortSignal): Promise<InstalledSkillView> {
    if(!validateActivateSkill(request)) throw new Error("Invalid Skill activation");
    const value=checked<InstalledSkillView>(await this.get(new URL(`installed-skills/${id(skill)}/activation`,this.base),signal,"POST",request),validateInstalledSkillView);
    if(value.id!==skill || value.active!==request.active || BigInt(value.revision)!==BigInt(request.expectedRevision)+1n) throw new Error("Skill activation identity mismatch");
    return value;
  }
  async rollbackSkill(skill: string, request: RollbackSkill, signal?: AbortSignal): Promise<InstalledSkillView> {
    if(!validateRollbackSkill(request)) throw new Error("Invalid Skill rollback");
    const value=checked<InstalledSkillView>(await this.get(new URL(`installed-skills/${id(skill)}/rollback`,this.base),signal,"POST",request),validateInstalledSkillView);
    if(value.id!==skill || value.active || BigInt(value.revision)!==BigInt(request.expectedRevision)+1n) throw new Error("Skill rollback identity mismatch");
    return value;
  }
  async proposeSkill(sourceJob: string, request: ProposeSkill, signal?: AbortSignal): Promise<SkillCandidateView> {
    if(!validateProposeSkill(request)) throw new Error("Invalid Skill proposal");
    const value=checked<SkillCandidateView>(await this.get(new URL(`jobs/${id(sourceJob)}/skills`,this.base),signal,"POST",request),validateSkillCandidateView);
    if(value.sourceJobId!==sourceJob) throw new Error("Skill source identity mismatch");
    return value;
  }
  async skillCandidate(candidate: string, signal?: AbortSignal): Promise<SkillCandidateView> {
    const value=checked<SkillCandidateView>(await this.get(new URL(`skills/${id(candidate)}`,this.base),signal),validateSkillCandidateView);
    if(value.id!==candidate) throw new Error("Skill candidate identity mismatch");
    return value;
  }
  async evaluateSkill(candidate: string, request: ReviewSkillEvaluation, signal?: AbortSignal): Promise<SkillCandidateView> {
    if(!validateReviewSkillEvaluation(request)) throw new Error("Invalid Skill evaluation review");
    const value=checked<SkillCandidateView>(await this.get(new URL(`skills/${id(candidate)}/evaluate`,this.base),signal,"POST",request),validateSkillCandidateView);
    if(value.id!==candidate || value.digest!==request.expectedDigest) throw new Error("Skill review identity mismatch");
    return value;
  }
  async sharedMemorySpaces(workspace: string, after?: string, signal?: AbortSignal): Promise<SharedMemoryPage> {
    const url=new URL(`workspaces/${id(workspace)}/memory/spaces`,this.base);
    if (after) url.searchParams.set("after",id(after));
    const page=checked<SharedMemoryPage>(await this.get(url,signal),validateSharedMemoryPage);
    if (page.items.some(space=>space.workspaceId!==workspace)) throw new Error("Shared Memory workspace mismatch");
    if (page.after && page.items.at(-1)?.id!==page.after) throw new Error("Shared Memory cursor mismatch");
    return page;
  }
  async sharedMemorySpace(space: string, signal?: AbortSignal): Promise<SharedMemorySpace> {
    const value=checked<SharedMemorySpace>(await this.get(new URL(`memory/spaces/${id(space)}`,this.base),signal),validateSharedMemorySpace);
    if(value.id!==space) throw new Error("Shared Memory identity mismatch");
    return value;
  }
  async configureSharedMemory(space: string, request: ConfigureSharedMemory, signal?: AbortSignal): Promise<SharedMemorySpace> {
    if(!validateConfigureSharedMemory(request)) throw new Error("Invalid shared Memory configuration");
    const value=checked<SharedMemorySpace>(await this.get(new URL(`memory/spaces/${id(space)}`,this.base),signal,"PUT",request),validateSharedMemorySpace);
    if(value.id!==space || value.workspaceId!==request.workspaceId) throw new Error("Shared Memory identity mismatch");
    return value;
  }
  async proposeSharedMemory(space: string, request: ProposeSharedMemory, signal?: AbortSignal): Promise<SharedMemoryChange> {
    if(!validateProposeSharedMemory(request)) throw new Error("Invalid shared Memory proposal");
    const value=checked<SharedMemoryChange>(await this.get(new URL(`memory/spaces/${id(space)}/changes`,this.base),signal,"POST",request),validateSharedMemoryChange);
    if(value.spaceId!==space) throw new Error("Shared Memory proposal identity mismatch");
    return value;
  }
  async sharedMemoryChange(space: string, change: string, signal?: AbortSignal): Promise<SharedMemoryChange> {
    const value=checked<SharedMemoryChange>(await this.get(new URL(`memory/spaces/${id(space)}/changes/${id(change)}`,this.base),signal),validateSharedMemoryChange);
    if(value.spaceId!==space || value.id!==change) throw new Error("Shared Memory proposal identity mismatch");
    return value;
  }
  async reviewSharedMemory(space: string, request: ReviewSharedMemory, signal?: AbortSignal): Promise<SharedMemoryChange> {
    if(!validateReviewSharedMemory(request)) throw new Error("Invalid shared Memory review");
    const value=checked<SharedMemoryChange>(await this.get(new URL(`memory/spaces/${id(space)}/review`,this.base),signal,"POST",request),validateSharedMemoryChange);
    if(value.spaceId!==space || value.id!==request.changeId) throw new Error("Shared Memory review identity mismatch");
    return value;
  }
  async mcpReview(approval: string, signal?: AbortSignal): Promise<McpCallView> {
    const value=checked<McpCallView>(await this.get(new URL(`approvals/${id(approval)}/mcp`,this.base),signal),validateMcpCallView);
    if (value.approvalId!==approval) throw new Error("MCP review identity mismatch");
    return value;
  }
  async editReview(approval: string, signal?: AbortSignal): Promise<WorkspaceEditView> {
    const value=checked<WorkspaceEditView>(await this.get(new URL(`approvals/${id(approval)}/edit`,this.base),signal),validateWorkspaceEditView);
    if (value.approvalId!==approval) throw new Error("Edit review identity mismatch");
    return value;
  }
  async learningJobs(workspace: string, query: LearningPageRequest = {}, signal?: AbortSignal): Promise<LearningPage> {
    if (!validateLearningPageRequest(query)) throw new Error("Invalid learning page request");
    const url = new URL(`workspaces/${id(workspace)}/learning/jobs`, this.base);
    url.searchParams.set("limit", (query.limit ?? 50).toString());
    if (query.before) {
      url.searchParams.set("beforeCreatedAtMs", query.before.createdAtMs);
      url.searchParams.set("beforeJobId", query.before.jobId);
    }
    if (query.stage) url.searchParams.set("stage", query.stage);
    if (query.state) url.searchParams.set("state", query.state);
    const value = checked<LearningPage>(await this.get(url, signal), validateLearningPage);
    if (value.items.length > (query.limit ?? 50) || value.items.some((job) => job.workspaceId !== workspace)) throw new Error("Learning page identity mismatch");
    if (value.before) {
      const last = value.items.at(-1);
      if (!last || value.before.createdAtMs !== last.createdAtMs || value.before.jobId !== last.id) throw new Error("Learning cursor identity mismatch");
    }
    return value;
  }
  async learningJob(job: string, signal?: AbortSignal): Promise<LearningJobView> {
    const value = checked<LearningJobView>(await this.get(new URL(`learning/jobs/${id(job)}`, this.base), signal), validateLearningJobView);
    if (value.id !== job) throw new Error("Learning job identity mismatch");
    return value;
  }
  async cancelLearning(job: string, request: CancelLearning, signal?: AbortSignal): Promise<LearningCancellation> {
    const value = checked<LearningCancellation>(await this.get(new URL(`learning/jobs/${id(job)}/cancel`, this.base), signal, "POST", request), validateLearningCancellation);
    if (value.requestId !== request.requestId || value.job.id !== job) throw new Error("Learning cancellation identity mismatch");
    return value;
  }
  async beginWorkspaceImport(session: string, request: BeginWorkspaceImport, signal?: AbortSignal): Promise<WorkspaceImportView> {
    const value = checked<WorkspaceImportView>(await this.get(new URL(`sessions/${id(session)}/workspace/imports`, this.base), signal, "POST", request), validateWorkspaceImportView);
    if (value.sessionId !== session || value.sha256 !== request.sha256 || value.bytes !== request.bytes) throw new Error("Workspace import identity mismatch");
    return value;
  }
  async workspaceImport(session: string, upload: string, signal?: AbortSignal): Promise<WorkspaceImportView> {
    const value = checked<WorkspaceImportView>(await this.get(new URL(`sessions/${id(session)}/workspace/imports/${id(upload)}`, this.base), signal), validateWorkspaceImportView);
    if (value.sessionId !== session || value.id !== upload) throw new Error("Workspace import identity mismatch");
    return value;
  }
  async cancelWorkspaceImport(session: string, upload: string, signal?: AbortSignal): Promise<WorkspaceImportView> {
    const value = checked<WorkspaceImportView>(await this.get(new URL(`sessions/${id(session)}/workspace/imports/${id(upload)}`, this.base), signal, "DELETE"), validateWorkspaceImportView);
    if (value.sessionId !== session || value.id !== upload) throw new Error("Workspace import identity mismatch");
    return value;
  }
  async uploadWorkspaceArchive(session: string, upload: string, archive: Blob, signal?: AbortSignal): Promise<WorkspaceImportView> {
    if (archive.size <= 0 || archive.size > 512 * 1024 * 1024) throw new Error("Workspace archive exceeds its bound");
    const response = await this.response(new URL(`sessions/${id(session)}/workspace/imports/${id(upload)}/archive`, this.base),
      signal, "PUT", undefined, "application/json", 600000, archive);
    const value = checked<WorkspaceImportView>(await this.json(response), validateWorkspaceImportView);
    if (value.sessionId !== session || value.id !== upload || value.bytes !== archive.size.toString()) throw new Error("Workspace import identity mismatch");
    return value;
  }
  async mergeReview(approval: string, signal?: AbortSignal): Promise<WorkspaceMergeView> {
    const value = checked<WorkspaceMergeView>(await this.get(new URL(`approvals/${id(approval)}/merge`, this.base), signal), validateWorkspaceMergeView);
    if (value.approvalId !== approval) throw new Error("Merge review identity mismatch");
    return value;
  }
  /** Stream reviewed bytes through the authenticated API; the caller owns consumption and cancellation. */
  async mergeContent(approval: string, side: MergeContentSide, path: string, signal?: AbortSignal): Promise<Response> {
    if (!validateMergeContentRequest({ approvalId: approval, side, path })) throw new Error("Invalid merge content request");
    const url = new URL(`approvals/${id(approval)}/merge/content`, this.base);
    url.searchParams.set("side", side); url.searchParams.set("path", path);
    const response = await this.response(url, signal, "GET", undefined, "application/octet-stream", 600000);
    const length = response.headers.get("content-length") ?? "";
    if (response.headers.get("content-type") !== "application/octet-stream" || !response.body ||
        !/^(0|[1-9][0-9]*)$/.test(length) || BigInt(length) > 512n * 1024n * 1024n ||
        !/^[0-9a-f]{64}$/.test(response.headers.get("x-zuno-content-sha256") ?? "")) {
      await response.body?.cancel();
      throw new Error("Invalid merge content response");
    }
    return response;
  }
  async workflow(job: string, signal?: AbortSignal): Promise<WorkflowRunView> {
    const value = checked<WorkflowRunView>(await this.get(new URL(`jobs/${id(job)}/workflow`, this.base), signal), validateWorkflowRunView);
    if (value.jobId !== job) throw new Error("Workflow response identity mismatch");
    return value;
  }
  async workspaces(signal?: AbortSignal): Promise<WorkspaceView[]> {
    const value = await this.get(new URL("workspaces", this.base), signal);
    if (!Array.isArray(value) || value.length > 128) throw new Error("Invalid workspace list");
    return value.map((item) => checked<WorkspaceView>(item, validateWorkspaceView));
  }
  async sessions(signal?: AbortSignal, before?: { updatedAt: number; sessionId: string }): Promise<SessionPage> {
    const url = new URL("sessions", this.base);
    url.searchParams.set("limit", "50");
    if (before) {
      url.searchParams.set("beforeUpdatedAt", before.updatedAt.toString());
      url.searchParams.set("beforeSessionId", before.sessionId);
    }
    return checked(await this.get(url, signal), validateSessionPage);
  }
  async createSession(request: CreateSession, signal?: AbortSignal): Promise<SessionSummary> {
    const value = checked<SessionSummary>(await this.get(new URL("sessions", this.base), signal, "POST", request), validateSessionSummary);
    if (value.workspaceId !== request.workspaceId) throw new Error("Created session belongs to another workspace");
    return value;
  }
  async inputVersion(session: string, signal?: AbortSignal): Promise<InputVersionView> {
    return checked(await this.get(new URL(`sessions/${id(session)}/input-version`, this.base), signal), validateInputVersionView);
  }
  async submit(session: string, request: SubmitTurn, signal?: AbortSignal): Promise<JobView> {
    const value = checked<JobView>(await this.get(new URL(`sessions/${id(session)}/turns`, this.base), signal, "POST", request), validateJobView);
    if (value.sessionId !== session) throw new Error("Submission response belongs to another session");
    return value;
  }
  async submission(session: string, request: string, signal?: AbortSignal): Promise<JobView> {
    const value = checked<JobView>(await this.get(new URL(`sessions/${id(session)}/requests/${id(request)}`, this.base), signal), validateJobView);
    if (value.sessionId !== session) throw new Error("Submission receipt belongs to another session");
    return value;
  }
  async job(job: string, signal?: AbortSignal): Promise<JobView> {
    const value = checked<JobView>(await this.get(new URL(`jobs/${id(job)}`, this.base), signal), validateJobView);
    if (value.id !== job) throw new Error("Job response identity mismatch");
    return value;
  }
  async cancel(job: string, request: CancelJob, signal?: AbortSignal): Promise<CancellationReceipt> {
    const value = checked<CancellationReceipt>(await this.get(new URL(`jobs/${id(job)}/cancel`, this.base), signal, "POST", request), validateCancellationReceipt);
    if (value.jobId !== job || value.turnId !== request.expectedTurnId || value.requestId !== request.requestId) throw new Error("Cancellation receipt identity mismatch");
    return value;
  }
  async approval(approval: string, signal?: AbortSignal): Promise<ApprovalView> {
    const value = checked<ApprovalView>(await this.get(new URL(`approvals/${id(approval)}`, this.base), signal), validateApprovalView);
    if (value.id !== approval) throw new Error("Approval response identity mismatch");
    return value;
  }
  async answer(approval: string, request: ApprovalDecision, signal?: AbortSignal): Promise<ApprovalView> {
    const value = checked<ApprovalView>(await this.get(new URL(`approvals/${id(approval)}/answer`, this.base), signal, "POST", request), validateApprovalView);
    if (value.id !== approval) throw new Error("Approval answer identity mismatch");
    return value;
  }
}
