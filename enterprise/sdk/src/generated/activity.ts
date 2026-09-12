/* Generated from zuno-types/activity.rs. Do not edit. */

export type CommittedEvent =
  | {
      kind: "upsert";
      position: Counter;
      record: ItemRecord;
    }
  | {
      id: string;
      kind: "remove";
    };
export type Counter = string;
export type UiAction =
  | {
      kind: "view";
      resourceId: string;
    }
  | {
      jobId: JobId;
      kind: "view_workflow";
    }
  | {
      approvalId: ApprovalId;
      kind: "view_workspace_merge";
    }
  | {
      approvalId: ApprovalId;
      kind: "approve";
    }
  | {
      approvalId: ApprovalId;
      kind: "reject";
    }
  | {
      kind: "answer";
      questionId: string;
    }
  | {
      jobId: JobId;
      kind: "interrupt";
      turnId: TurnId;
    }
  | {
      jobId: JobId;
      kind: "request_resume";
    };
export type JobId = string;
export type ApprovalId = string;
export type TurnId = string;
export type SessionItem =
  | {
      content: ContentBlock[];
      kind: "message";
      origin: MessageOrigin;
      role: MessageRole;
      state: MessageState;
      usage?: NormalizedUsage | null;
    }
  | {
      collapsed: boolean;
      kind: "thinking";
      text: string;
      truncated: boolean;
    }
  | {
      invocation: Invocation;
      kind: "invocation";
    }
  | {
      approvalId: ApprovalId;
      jobId: JobId;
      kind: "approval";
      presentation: ContentBlock[];
      status: ApprovalStatus;
    }
  | {
      kind: "plan";
      steps: PlanStep[];
    }
  | {
      kind: "goal";
      objective: string;
      state: WorkState;
    }
  | {
      automatic: boolean;
      kind: "compaction";
    }
  | {
      kind: "artifact";
      resource: ResourceRef;
    }
  | {
      jobId: JobId;
      kind: "background";
      label: string;
      state: WorkState;
    };
export type ContentBlock =
  | {
      kind: "text";
      text: string;
      truncated: boolean;
    }
  | {
      code: string;
      kind: "code";
      language?: string | null;
      truncated: boolean;
    }
  | {
      diff: string;
      kind: "diff";
      truncated: boolean;
    }
  | {
      channel: TerminalChannel;
      kind: "terminal";
      text: string;
      truncated: boolean;
    }
  | {
      kind: "structured";
      value: unknown;
    }
  | {
      alt: string;
      kind: "image";
      resource: ResourceRef;
    }
  | {
      kind: "resource";
      resource: ResourceRef;
    };
export type TerminalChannel = "stdout" | "stderr" | "combined";
export type MessageOrigin = "user_input" | "model" | "agent_report" | "runtime";
export type MessageRole = "user" | "assistant";
export type MessageState = "pending" | "complete" | "interrupted" | "failed";
export type DenialReason = "policy" | "permission" | "expired" | "unavailable" | "cancelled" | "unknown";
export type InvocationId = string;
export type Isolation = "enforced" | "unconfined" | "unavailable" | "unknown";
export type ExecutionLocation =
  | {
      kind: "local";
    }
  | {
      environmentId: EnvironmentId;
      kind: "enterprise";
    }
  | {
      kind: "external";
      name: ActivityName;
    }
  | {
      kind: "unknown";
    };
export type EnvironmentId = string;
export type ActivityName = string;
export type InvocationAction =
  | "file_read"
  | "file_list"
  | "file_search"
  | "file_edit"
  | "process"
  | "web_search"
  | "web_fetch"
  | "memory_read"
  | "memory_write"
  | "agent"
  | "workflow"
  | "council"
  | "tool";
export type InvocationSource =
  | {
      kind: "builtin";
    }
  | {
      exposure: McpExposure;
      kind: "mcp";
      server: ActivityName;
      tool: ActivityName;
    }
  | {
      extension: ActivityName;
      kind: "extension";
    }
  | {
      kind: "provider";
      provider: ActivityName;
    }
  | {
      agent: ActivityName;
      kind: "external_agent";
    }
  | {
      kind: "unknown";
    };
export type McpExposure = "exposed" | "deferred" | "unavailable";
export type InvocationState =
  "queued" | "waiting" | "running" | "succeeded" | "failed" | "denied" | "cancelled" | "uncertain";
export type WaitingFor =
  | {
      approvalId: ApprovalId;
      kind: "approval";
    }
  | {
      kind: "user_input";
      questionId: string;
    }
  | {
      jobId: JobId;
      kind: "child";
    }
  | {
      kind: "operation";
      operationId: OperationId;
    }
  | {
      deadline: Counter;
      kind: "timer";
    };
export type OperationId = string;
export type ApprovalStatus = "pending" | "approved" | "rejected" | "expired" | "revoked";
/**
 * One durable Plan step's lifecycle status.
 *
 * This type is shared by the Plan writer and Goal completion audit so the two
 * components cannot disagree about which states are terminal.
 */
export type PlanStepStatus = "pending" | "in_progress" | "completed" | "superseded";
export type WorkState =
  "pending" | "active" | "waiting" | "paused" | "completed" | "failed" | "cancelled" | "uncertain";
export type SessionId = string;
export type LiveEvent =
  | {
      items: LiveItem[];
      kind: "snapshot";
    }
  | {
      itemId: string;
      kind: "text_delta";
      text: string;
    }
  | {
      itemId: string;
      kind: "thinking_delta";
      text: string;
    }
  | {
      invocationId: InvocationId;
      kind: "invocation_progress";
      label: string;
    }
  | {
      kind: "reset";
    };
/**
 * Replaceable drafts have independent IDs and are discarded when their
 * generation ends. Only committed records enter durable conversation history.
 */
export type LiveItem =
  | {
      id: string;
      kind: "text";
      parentId?: string | null;
      text: string;
      truncated: boolean;
    }
  | {
      id: string;
      kind: "thinking";
      parentId?: string | null;
      text: string;
      truncated: boolean;
    }
  | {
      id: InvocationId;
      kind: "invocation";
      label: string;
    };

export interface ActivityProtocol {
  committed: CommittedFrame;
  frames: FramePage;
  history: HistoryPage;
  live: LiveFrame;
}
export interface CommittedFrame {
  event: CommittedEvent;
  sequence: Counter;
  sessionId: SessionId;
  version: number;
}
export interface ItemRecord {
  actions: UiAction[];
  createdAt: Counter;
  id: string;
  item: SessionItem;
  parentId?: string | null;
}
export interface ResourceRef {
  bytes?: Counter | null;
  id: string;
  mediaType?: string | null;
  name: string;
}
/**
 * Input counts exclude cache reads/writes; reasoning is included in output.
 * Unknown accounting is omitted instead of reporting fabricated zeros.
 */
export interface NormalizedUsage {
  cacheRead: Counter;
  cacheWrite: Counter;
  input: Counter;
  output: Counter;
  reasoning: Counter;
}
export interface Invocation {
  content: ContentBlock[];
  denial?: DenialReason | null;
  id: InvocationId;
  input: unknown;
  isolation: Isolation;
  location: ExecutionLocation;
  name: ActivityName;
  presentation: InvocationPresentation;
  state: InvocationState;
  waitingFor?: WaitingFor | null;
}
export interface InvocationPresentation {
  action: InvocationAction;
  source: InvocationSource;
}
export interface PlanStep {
  id: string;
  status: PlanStepStatus;
  text: string;
}
export interface FramePage {
  frames: CommittedFrame[];
  more: boolean;
  sessionId: SessionId;
  through: Counter;
  version: number;
}
export interface HistoryPage {
  before?: Counter | null;
  items: HistoryItem[];
  sessionId: SessionId;
  through: Counter;
  version: number;
}
export interface HistoryItem {
  position: Counter;
  record: ItemRecord;
  revision: Counter;
}
/**
 * A replaceable, bounded stream generation. It never substitutes for committed
 * history and cannot carry encrypted reasoning or authoritative usage totals.
 */
export interface LiveFrame {
  afterCommitted: Counter;
  event: LiveEvent;
  generation: string;
  sequence: Counter;
  sessionId: SessionId;
  turnId: TurnId;
  version: number;
}
