import { useCallback, useEffect, useRef, useState } from "react";
import { EnterpriseHttpError } from "@zuno/enterprise-sdk";
import type { CreateSession, HistoryItem, SessionPage, SessionSummary, SubmitTurn, WorkspaceView } from "@zuno/enterprise-sdk";
import { bindIdentity, client, errorText, identity as loadIdentity, login, logout, requestId } from "./api";
import type { BrowserIdentity } from "./api";
import { Alert, Button, Empty, Loading } from "./components/Primitives";
import { Conversation } from "./components/Conversation";
import { Details } from "./components/Details";
import { useSession } from "./useSession";
import { containFocus, useMedia } from "./useMedia";

export default function App() {
  const [identity, setIdentity] = useState<BrowserIdentity | null>(null);
  const [authLoading, setAuthLoading] = useState(true);
  const [error, setError] = useState("");
  const [workspaces, setWorkspaces] = useState<WorkspaceView[]>([]);
  const [workspace, setWorkspace] = useState("");
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [nextSessions, setNextSessions] = useState<SessionPage["next"]>(null);
  const [listing, setListing] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [nav, setNav] = useState(false);
  const [detail, setDetail] = useState<HistoryItem | null>(null);
  const [approval, setApproval] = useState<string | null>(null);
  const [newActivity, setNewActivity] = useState(false);
  const pending = useRef<{ session: string; request: SubmitTurn } | null>(null);
  const creation = useRef<CreateSession | null>(null);
  const gate = useRef(false);
  const listGate = useRef(false);
  const cancellation = useRef(new Map<string, string>());
  const epoch = useRef(0);
  const authRun = useRef(0);
  const scroll = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const small = useMedia("(max-width: 700px)");
  const overlay = useMedia("(max-width: 1100px)");
  const navRef = useRef<HTMLElement>(null);
  useEffect(() => {
    if (!nav || !small) return;
    const previous = document.activeElement as HTMLElement | null;
    navRef.current?.querySelector<HTMLElement>("select")?.focus();
    return () => queueMicrotask(() => { if (previous?.isConnected) previous.focus(); });
  }, [nav, small]);
  const clear = useCallback(() => {
    epoch.current++; authRun.current++; gate.current = false; setBusy(false); setAuthLoading(false);
    bindIdentity(null);
    setIdentity(null); setSelected(null); setSessions([]); setNextSessions(null); setListing(false); listGate.current = false;
    setWorkspaces([]); setDraft(""); cancellation.current.clear();
    setDetail(null); setApproval(null); pending.current = null; creation.current = null;
  }, []);
  const authenticate = useCallback(async () => {
    const run = ++authRun.current;
    setAuthLoading(true); setError("");
    try {
      const account = await loadIdentity();
      if (run !== authRun.current) return;
      bindIdentity(account);
      const [spaces, page] = await Promise.all([client.workspaces(), client.sessions()]);
      if (run !== authRun.current) return;
      setIdentity(account); setWorkspaces(spaces); setWorkspace(spaces[0]?.id ?? ""); setSessions(page.items); setNextSessions(page.next);
    } catch (error) {
      if (run !== authRun.current) return;
      clear();
      if (!(error instanceof Error && error.message === "signed_out")) setError(errorText(error));
    } finally { if (run === authRun.current) setAuthLoading(false); }
  }, [clear]);
  useEffect(() => { void authenticate(); return () => { authRun.current++; }; }, [authenticate]);
  const unavailable = useCallback(() => { clear(); void authenticate(); }, [authenticate, clear]);
  const activity = useSession(identity ? selected : null, unavailable);
  useEffect(() => {
    if (!scroll.current) return;
    if (atBottom.current) scroll.current.scrollTop = scroll.current.scrollHeight;
    else if (!activity.paging) setNewActivity(true);
  }, [activity.items, activity.live]);
  const current = sessions.find((session) => session.id === selected);

  function choose(id: string | null) {
    if (gate.current || pending.current) return;
    epoch.current++;
    setSelected(id); setDetail(null); setApproval(null); setNav(false); setError(""); setDraft("");
    pending.current = null; creation.current = null; atBottom.current = true; setNewActivity(false);
  }
  async function submit() {
    if (!draft.trim() || !workspace || gate.current) return;
    const run = epoch.current;
    gate.current = true; setBusy(true); setError("");
    try {
      let session = selected;
      if (!session) {
        if (!creation.current || creation.current.workspaceId !== workspace) creation.current = { requestId: requestId(), workspaceId: workspace, title: draft.trim().slice(0, 80) };
        const created = await client.createSession(creation.current);
        if (run !== epoch.current) return;
        setSessions((rows) => [created, ...rows.filter((row) => row.id !== created.id)]); session = created.id; setSelected(session);
        creation.current = null;
      }
      if (!pending.current || pending.current.session !== session || pending.current.request.text !== draft) {
        const version = await client.inputVersion(session);
        if (run !== epoch.current) return;
        pending.current = { session, request: { requestId: requestId(), expectedInputVersion: version.version, text: draft } };
      }
      try {
        await client.submit(session, pending.current.request);
      } catch (error) {
        if (run !== epoch.current) return;
        try {
          await client.submission(session, pending.current.request.requestId);
        } catch (lookup) {
          if (error instanceof EnterpriseHttpError && [400, 403, 404, 409, 422].includes(error.status) && lookup instanceof EnterpriseHttpError && [403, 404].includes(lookup.status)) {
            pending.current = null;
          }
          if (error instanceof EnterpriseHttpError && error.status === 401) unavailable();
          throw error;
        }
      }
      if (run !== epoch.current) return;
      pending.current = null; setDraft(""); atBottom.current = true;
    } catch (error) { if (run === epoch.current) setError(errorText(error)); }
    finally { if (run === epoch.current) { gate.current = false; setBusy(false); } }
  }
  async function cancel(job: string, turn: string) {
    const run = epoch.current;
    const key = `${job}:${turn}`;
    if (!cancellation.current.has(key)) cancellation.current.set(key, requestId());
    setError("");
    try { await client.cancel(job, { requestId: cancellation.current.get(key)!, expectedTurnId: turn, reason: "用户在会话工作台请求停止任务" }); }
    catch (error) { if (run === epoch.current) setError(errorText(error)); }
  }
  async function loadSessions() {
    if (!nextSessions || listGate.current) return;
    const run = authRun.current;
    listGate.current = true; setListing(true);
    try {
      const page = await client.sessions(undefined, nextSessions);
      if (run !== authRun.current) return;
      setSessions((rows) => [...rows, ...page.items.filter((item) => !rows.some((row) => row.id === item.id))]);
      setNextSessions(page.next);
    } catch (error) { if (run === authRun.current) setError(errorText(error)); }
    finally { if (run === authRun.current) { listGate.current = false; setListing(false); } }
  }
  async function loadOlder() {
    const element = scroll.current;
    if (!element) return;
    const run = epoch.current;
    const height = element.scrollHeight;
    const top = element.scrollTop;
    await activity.loadOlder();
    requestAnimationFrame(() => {
      if (run === epoch.current && scroll.current === element) element.scrollTop = top + element.scrollHeight - height;
    });
  }
  async function signOut() {
    try { await logout(); clear(); } catch (error) { setError(errorText(error)); }
  }
  if (authLoading) return <main className="auth-page"><Loading label="正在连接企业工作区…" /></main>;
  if (!identity) return <main className="auth-page"><section className="login-panel">
    <div className="brand">Zuno <span>Enterprise</span></div><h1>进入你的工作区</h1>
    <p>使用企业账号登录，继续任务、检查执行记录并处理审批。</p>
    {error && <Alert>{error}</Alert>}
    <Button tone="primary" onClick={() => void login().catch((error) => setError(error instanceof Error ? error.message : "登录失败"))}>使用企业账号登录</Button>
    <p className="license-link"><a href="/app/assets/licenses.txt" target="_blank" rel="noopener noreferrer">开源许可</a></p>
  </section></main>;
  return <div className={`workbench ${detail || approval ? "workbench--details" : ""}`}>
    {nav && <button className="drawer-backdrop" aria-label="关闭导航" onClick={() => setNav(false)} />}
    <nav ref={navRef} className={`sidebar ${nav ? "sidebar--open" : ""}`} inert={small && !nav || overlay && Boolean(detail || approval)}
      role={small && nav ? "dialog" : undefined} aria-modal={small && nav || undefined} aria-label="工作区导航"
      onKeyDown={(event) => { if (small && nav) containFocus(event); if (event.key === "Escape") setNav(false); }}>
      <div className="brand">Zuno <span>Enterprise</span></div>
      <label className="field-label" htmlFor="workspace">工作区</label>
      <select id="workspace" value={workspace} disabled={busy || Boolean(pending.current)} onChange={(event) => { setWorkspace(event.target.value); choose(null); }}>
        {workspaces.map((space) => <option value={space.id} key={space.id}>{space.title}</option>)}
      </select>
      <Button tone="primary" disabled={busy || Boolean(pending.current)} onClick={() => choose(null)}>＋ 新建会话</Button>
      <div className="sidebar-heading">会话</div>
      <div className="session-list">{sessions.filter((session) => session.workspaceId === workspace).map((session) =>
        <button key={session.id} disabled={busy || Boolean(pending.current)} aria-current={session.id === selected ? "page" : undefined} className={`session-row ${session.id === selected ? "selected" : ""}`} onClick={() => choose(session.id)} title={session.title}>
          <span aria-hidden="true">○</span><span>{session.title}</span>
        </button>)}
        {nextSessions && <Button disabled={listing} onClick={() => void loadSessions()}>{listing ? "正在加载…" : "更多会话"}</Button>}
      </div>
      <div className="account"><span className="account-avatar" aria-hidden="true">你</span><span>企业账号<small>独立会话与工作区</small></span><button className="icon-button" aria-label="退出登录" onClick={() => void signOut()}>↪</button></div>
    </nav>
    <main className="session-main" inert={small && nav || overlay && Boolean(detail || approval)}>
      <header className="session-header">
        <button className="icon-button mobile-menu" aria-label="打开导航" onClick={() => setNav(true)}>☰</button>
        <div><h1>{current?.title ?? "新建会话"}</h1><span>{workspaces.find((item) => item.id === workspace)?.title}</span></div>
        <span className={`connection ${activity.connected ? "" : "connection--lost"}`}>{activity.connected ? "已连接" : "正在重连"}</span>
      </header>
      <div className="transcript-scroll" ref={scroll} onScroll={() => {
        const element = scroll.current; if (!element) return;
        atBottom.current = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
        if (atBottom.current) setNewActivity(false);
      }}>
        {activity.loading ? <Loading /> : selected ? <>
          {activity.older && <Button disabled={activity.paging} onClick={() => void loadOlder()}>加载更早记录</Button>}
          <Conversation items={activity.items} live={activity.live} select={(item) => { setDetail(item); setApproval(null); }} approve={(id) => { setApproval(id); setDetail(null); }} cancel={(job, turn) => void cancel(job, turn)} />
        </> : <Empty title="开始一项任务">描述目标、背景与期望结果。执行需要确认的操作时，你会收到审批请求。</Empty>}
        {(error || activity.error) && <Alert>{error || activity.error}</Alert>}
      </div>
      {(newActivity || activity.historical) && <Button className="new-activity" disabled={activity.paging} onClick={() => {
        atBottom.current = true; setNewActivity(false);
        if (activity.historical) void activity.latest();
        else if (scroll.current) scroll.current.scrollTop = scroll.current.scrollHeight;
      }}>{activity.historical ? "返回最新记录 ↓" : "查看新活动 ↓"}</Button>}
      <form className="composer" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
        <label className="visually-hidden" htmlFor="prompt">任务内容</label>
        <textarea id="prompt" placeholder="描述任务，或补充新的输入…" value={draft} onChange={(event) => setDraft(event.target.value)} disabled={busy || Boolean(pending.current)}
          onKeyDown={(event) => { if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); void submit(); } }} />
        <footer><span>{pending.current && !busy ? "尚未确认接纳结果，重试会核对同一项请求。" : "Ctrl / ⌘ Enter 发送 · 操作按企业策略审批"}</span><Button type="submit" tone="primary" disabled={busy || !draft.trim() || !workspace}>{busy ? "正在接纳…" : pending.current ? "重试发送" : "发送 ↑"}</Button></footer>
      </form>
    </main>
    {(detail || approval) && <Details item={detail} approvalId={approval} identity={identity} close={() => { setDetail(null); setApproval(null); }} />}
  </div>;
}
