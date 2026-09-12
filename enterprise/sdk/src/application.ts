import { ActivityClient } from "./client.js";
import type {
  WorkspaceView, SessionPage, SessionSummary, CreateSession, JobView,
  SubmitTurn, InputVersionView, ApprovalView, ApprovalDecision, CancelJob, CancellationReceipt, WorkflowRunView, WorkspaceMergeView, MergeContentSide, BeginWorkspaceImport, WorkspaceImportView,
} from "./generated/application.js";
import {
  validateWorkspaceView, validateSessionPage, validateSessionSummary, validateJobView,
  validateInputVersionView, validateApprovalView, validateCancellationReceipt, validateWorkflowRunView, validateWorkspaceMergeView, validateMergeContentRequest, validateWorkspaceImportView,
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
