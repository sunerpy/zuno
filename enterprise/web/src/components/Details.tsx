import { useEffect, useRef, useState } from "react";
import type { ApprovalView, HistoryItem } from "@zuno/enterprise-sdk";
import { client, errorText, requestId } from "../api";
import type { BrowserIdentity } from "../api";
import { Alert, Badge, Button, Loading } from "./Primitives";
import { Content } from "./Content";
import { containFocus, useMedia } from "../useMedia";

export function Details({ item, approvalId, identity, close }: {
  item: HistoryItem | null; approvalId: string | null; identity: BrowserIdentity; close: () => void;
}) {
  const [approval, setApproval] = useState<ApprovalView | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const pending = useRef<{ id: string; answer: "approve" | "reject" } | null>(null);
  const gate = useRef(false);
  const currentApproval = useRef(approvalId); currentApproval.current = approvalId;
  const closeButton = useRef<HTMLButtonElement>(null);
  const overlay = useMedia("(max-width: 1100px)");
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    closeButton.current?.focus();
    return () => queueMicrotask(() => { if (previous?.isConnected) previous.focus(); });
  }, []);
  useEffect(() => {
    setApproval(null); setError(""); pending.current = null;
    if (!approvalId) return;
    const abort = new AbortController();
    void client.approval(approvalId, abort.signal).then((value) => { if (!abort.signal.aborted) setApproval(value); }).catch((error) => {
      if (!abort.signal.aborted) setError(errorText(error));
    });
    return () => abort.abort();
  }, [approvalId]);
  async function answer(answer: "approve" | "reject") {
    if (!approvalId || gate.current) return;
    const target = approvalId;
    gate.current = true; setBusy(true); setError("");
    if (pending.current?.answer !== answer) pending.current = { id: requestId(), answer };
    try {
      const record = await client.answer(approvalId, { requestId: pending.current.id, answer });
      if (currentApproval.current === target) { setApproval(record); pending.current = null; }
    } catch (error) {
      if (currentApproval.current === target) setError(errorText(error));
      try { const record = await client.approval(target); if (currentApproval.current === target) setApproval(record); } catch { /* original error stays visible */ }
    } finally { gate.current = false; setBusy(false); }
  }
  const canAnswer = approval?.state === "pending" && (
    approval.audience === "requester" ? approval.requester.principalId === identity.principalId
      : approval.requester.principalId !== identity.principalId
  );
  return <aside className="details-panel" role={overlay ? "dialog" : undefined} aria-modal={overlay || undefined} aria-label={approvalId ? "操作审批" : "执行详情"} onKeyDown={(event) => { if (overlay) containFocus(event); if (event.key === "Escape") close(); }}>
    <header><h2>{approvalId ? "操作审批" : "执行详情"}</h2><button ref={closeButton} className="icon-button" aria-label="关闭详情" onClick={close}>×</button></header>
    {error && <Alert>{error}</Alert>}
    {approvalId ? approval ? <>
      <div className="detail-status"><Badge state={approval.state} /></div>
      <p>{approval.audience === "designatedApprover" ? "此操作需要指定审批人确认。" : "确认下方操作后，Agent 才能继续执行。"}</p>
      <h3>请求内容</h3><pre>{JSON.stringify(approval.presentation, null, 2)}</pre>
      <dl><dt>操作</dt><dd>{approval.binding.effect}</dd><dt>有效期</dt><dd>{new Date(approval.expiresAtMs).toLocaleString()}</dd><dt>Job</dt><dd>{approval.binding.jobId}</dd></dl>
      {canAnswer && <div className="approval-buttons"><Button tone="danger" disabled={busy} onClick={() => void answer("reject")}>拒绝</Button><Button tone="primary" disabled={busy} onClick={() => void answer("approve")}>{busy ? "正在提交…" : "批准此操作"}</Button></div>}
    </> : !error && <Loading /> : item?.record.item.kind === "invocation" ? <>
      <h3>{item.record.item.invocation.name}</h3><Badge state={item.record.item.invocation.state} />
      <dl><dt>动作</dt><dd>{item.record.item.invocation.presentation.action}</dd><dt>来源</dt><dd>{item.record.item.invocation.presentation.source.kind}</dd>
        <dt>隔离状态</dt><dd>{item.record.item.invocation.isolation}</dd></dl>
      <h3>参数</h3><pre>{JSON.stringify(item.record.item.invocation.input, null, 2)}</pre>
      <h3>结果</h3><Content blocks={item.record.item.invocation.content} />
    </> : null}
  </aside>;
}
