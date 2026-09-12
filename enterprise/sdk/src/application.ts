import { ActivityClient } from "./client.js";
import type {
  WorkspaceView, SessionPage, SessionSummary, CreateSession, JobView,
  SubmitTurn, InputVersionView, ApprovalView, ApprovalDecision, CancelJob, CancellationReceipt,
} from "./generated/application.js";
import {
  validateWorkspaceView, validateSessionPage, validateSessionSummary, validateJobView,
  validateInputVersionView, validateApprovalView, validateCancellationReceipt,
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
