import type { HistoryItem, Invocation, LiveItem } from "@zuno/enterprise-sdk";
import type { ReactNode } from "react";
import { Badge, Button } from "./Primitives";
import { Content } from "./Content";

const actions: Record<string, string> = {
  file_read: "读取", file_list: "目录", file_search: "搜索", file_edit: "编辑",
  process: "命令", web_search: "网络搜索", web_fetch: "网页", memory_read: "读取记忆",
  memory_write: "更新记忆", agent: "Agent", workflow: "Workflow", council: "Council", tool: "工具",
};
function Source({ invocation }: { invocation: Invocation }) {
  const source = invocation.presentation.source;
  const name = source.kind === "mcp" ? `MCP · ${source.server}` : source.kind === "extension" ? source.extension
    : source.kind === "external_agent" ? source.agent : source.kind === "provider" ? source.provider
    : source.kind === "builtin" ? "内置" : "来源未记录";
  return <span className="source">{name}</span>;
}

export function Conversation({ items, live, select, approve, cancel }: {
  items: HistoryItem[]; live: LiveItem[];
  select: (item: HistoryItem) => void;
  approve: (id: string) => void;
  cancel: (job: string, turn: string) => void;
}) {
  const byId = new Map(items.map((row) => [row.record.id, row]));
  const children = new Map<string, HistoryItem[]>();
  const roots: HistoryItem[] = [];
  for (const row of items) {
    let parent = row.record.parentId;
    const seen = new Set([row.record.id]);
    let cursor = parent;
    for (let depth = 0; cursor && byId.has(cursor); depth++) {
      if (seen.has(cursor) || depth > 16) { parent = null; break; }
      seen.add(cursor); cursor = byId.get(cursor)?.record.parentId;
    }
    if (parent && parent !== row.record.id && byId.has(parent)) {
      const group = children.get(parent) ?? []; group.push(row); children.set(parent, group);
    } else roots.push(row);
  }
  function render(row: HistoryItem, seen = new Set<string>(), depth = 0): ReactNode {
    if (seen.has(row.record.id) || depth > 16) return null;
    const nextSeen = new Set(seen); nextSeen.add(row.record.id);
    const value = row.record.item;
    const nested = children.get(row.record.id) ?? [];
    let body: ReactNode = null;
    switch (value.kind) {
      case "message": body = <section className={`message message--${value.role}`}>
        {row.record.parentId?.startsWith("message:") && value.content.length > 0 ? null :
          <header className="message-heading"><strong>{value.origin === "agent_report" ? "Agent 报告" : value.role === "user" ? "你" : "Zuno"}</strong>
            {value.state === "pending" && <Badge state="pending" />}</header>}
        <Content blocks={value.content} />
        {nested.map((child) => <div key={child.record.id}>{render(child, nextSeen, depth + 1)}</div>)}
      </section>; break;
      case "thinking": body = <details className="thinking"><summary>思考过程</summary><div>{value.text}</div>{value.truncated && <small>显示截断预览</small>}</details>; break;
      case "invocation": body = <div className="invocation">
        <button className="invocation-main" onClick={() => select(row)}>
          <span className="action-mark">{actions[value.invocation.presentation.action] ?? "工具"}</span>
          <span className="invocation-name">{value.invocation.name}<Source invocation={value.invocation} /></span>
          <Badge state={value.invocation.state} /><span aria-hidden="true">›</span>
        </button>
        {value.invocation.waitingFor?.kind === "approval" && <Button onClick={() => approve(value.invocation.waitingFor!.kind === "approval" ? value.invocation.waitingFor!.approvalId : "")}>查看审批</Button>}
      </div>; break;
      case "approval": body = <div className="approval-notice"><Badge state={value.status} /><span>操作审批</span><Button onClick={() => approve(value.approvalId)}>查看请求</Button></div>; break;
      case "background": body = <div className="run-status"><Badge state={value.state} />
        <span>{value.label}</span>{row.record.actions.map((action) => action.kind === "interrupt"
          ? <Button key={action.jobId} onClick={() => cancel(action.jobId, action.turnId)}>停止</Button> : null)}</div>; break;
      case "compaction": body = <div className="timeline-note">上下文已压缩{value.automatic ? " · 自动" : ""}</div>; break;
      case "plan": body = <ol className="plan">{value.steps.map((step) => <li key={step.id}><Badge state={step.status} />{step.text}</li>)}</ol>; break;
      case "goal": body = <div className="goal"><Badge state={value.state} />{value.objective}</div>; break;
      case "artifact": body = <div className="resource-label">{value.resource.name}</div>; break;
    }
    return body;
  }
  return <div className="conversation" aria-label="会话历史">
    {roots.map((row) => <div key={row.record.id} className="conversation-item">{render(row)}</div>)}
    {live.length > 0 && <section className="live-draft" aria-label="实时草稿">
      <span className="live-label">正在生成</span>
      {live.map((item) => item.kind === "invocation"
        ? <div key={`call:${item.id}`}>{item.label}</div>
        : item.kind === "thinking" ? <details key={`draft:${item.id}`} className="thinking"><summary>思考中</summary>{item.text}</details>
        : <Content key={`draft:${item.id}`} blocks={[{ kind: "text", text: item.text, truncated: item.truncated }]} />)}
    </section>}
  </div>;
}
