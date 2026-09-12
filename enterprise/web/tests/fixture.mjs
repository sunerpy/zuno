// Browser-only fixture. Runtime builds never import or serve this module.
import { createServer } from "node:https";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export async function fixture({ capture = false } = {}) {
  const directory = await mkdtemp(join(tmpdir(), "zuno-web-test-"));
  execFileSync("openssl", ["req", "-x509", "-newkey", "rsa:2048", "-nodes",
    "-keyout", join(directory, "key.pem"), "-out", join(directory, "cert.pem"),
    "-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"],
  { stdio: "ignore" });
  const root = fileURLToPath(new URL("../dist/", import.meta.url));
  const state = {
    signedIn: true, subject: "alice", losesCreate: false, losesSubmit: false,
    createRequests: [], submissions: [], answers: [], escapedImageRequests: 0,
    sessions: [{ id: "session", workspaceId: "workspace", title: "检查数据库迁移与恢复", createdAt: 1000, updatedAt: 1000 }],
    requestReceipts: new Map(), creationReceipts: new Map(), approval: "pending",
    delayedAnswer: 0, slowSession: 0, historyCount: 0, historyReads: 0, frameFailures: 0, lookupFailures: 0,
    hiddenSessions: [],
  };
  const item = (id, kind, parentId = null) => ({ id, parentId, createdAt: "1000", actions: [], item: kind });
  function history(session, query) {
    state.historyReads++;
    if (state.historyCount) {
      const end = Math.min(Number(query.get("before") ?? state.historyCount + 1) - 1, state.historyCount);
      const start = Math.max(1, end - 19);
      return {
        version: 1, sessionId: session, through: String(state.historyCount), before: start > 1 ? String(start) : null,
        items: Array.from({ length: end - start + 1 }, (_, index) => {
          const position = String(start + index);
          return { position, revision: position, record: item(`message:${position}`, {
            kind: "message", role: "user", origin: "user_input", state: "complete",
            content: [{ kind: "text", text: `历史记录 ${position} — 检查保留的输入和执行结果。`, truncated: false }], usage: null,
          }) };
        }),
      };
    }
    const text = session === "session" ? "请检查迁移逻辑，并列出验证步骤。" : "另一个会话的内容";
    const records = [
      item("message:user", { kind: "message", role: "user", origin: "user_input", state: "complete", content: [{ kind: "text", text, truncated: false }], usage: null }),
      item("message:assistant", { kind: "message", role: "assistant", origin: "model", state: "complete", content: [], usage: null }, "message:user"),
      item("part:thinking", { kind: "thinking", text: "先确认迁移边界，再检查故障恢复证据。", collapsed: true, truncated: false }, "message:assistant"),
      item("part:text", { kind: "message", role: "assistant", origin: "model", state: "complete", content: [{ kind: "text",
        text: "已检查原始数据保留与事务边界。接下来运行验证命令。\n\n![不得自动加载](https://external.invalid/pixel)\n\n`crates/zuno-postgres/src/runtime/children/workspace.rs`", truncated: false }], usage: null }, "message:assistant"),
      item("part:call", { kind: "invocation", invocation: {
        id: "call", name: "Environment command", presentation: { action: "process", source: { kind: "builtin" } },
        state: state.approval === "pending" ? "waiting" : "succeeded", input: { argv: ["cargo", "test", "-p", "zuno-postgres"] }, content: [],
        waitingFor: state.approval === "pending" ? { kind: "approval", approvalId: "approval" } : null,
        location: { kind: "enterprise", environmentId: "environment" }, isolation: "enforced", denial: null,
      } }, "message:assistant"),
      item("approval:request", { kind: "approval", approvalId: "approval", jobId: "job", status: state.approval, presentation: [] }),
    ];
    return { version: 1, sessionId: session, through: "6", before: null,
      items: records.map((record, i) => ({ position: String(i + 1), revision: String(i + 1), record })) };
  }
  const approval = () => ({
    id: "approval", binding: { jobId: "job", sessionId: "session", turnId: "turn", invocationId: "call", operationId: "operation",
      argumentsSha256: "a".repeat(64), resourcesSha256: "b".repeat(64), effect: "process" },
    requester: { tenantId: "tenant", principalId: "alice" }, audience: "requester", state: state.approval,
    presentation: { argv: ["cargo", "test", "-p", "zuno-postgres"] }, expiresAtMs: Date.now() + 300000,
  });
  const json = (response, value, status = 200) => {
    response.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
    response.end(JSON.stringify(value));
  };
  const server = createServer({
    key: await readFile(join(directory, "key.pem")), cert: await readFile(join(directory, "cert.pem")),
  }, async (request, response) => {
    try {
      const url = new URL(request.url, "https://localhost");
      const path = url.pathname;
      if (capture && path === "/capture-select.js") {
        response.writeHead(200, { "content-type": "text/javascript" });
        response.end("const timer=setInterval(()=>{const row=document.querySelector('.session-row');if(row){row.click();clearInterval(timer)}},50);");
        return;
      }
      if (path === "/app/" || path.startsWith("/app/assets/")) {
        const name = path === "/app/" ? "index.html" : path.slice("/app/".length);
        const file = resolve(root, name);
        if (!file.startsWith(resolve(root) + "/")) { response.writeHead(404).end(); return; }
        const type = name.endsWith(".html") ? "text/html" : name.endsWith(".txt") ? "text/plain" : name.endsWith(".css") ? "text/css" : name.endsWith(".woff2") ? "font/woff2" : "text/javascript";
        response.writeHead(200, { "content-type": type, ...(capture ? {} : { "content-security-policy": "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self'; base-uri 'none'; frame-ancestors 'none'" }) });
        let content = await readFile(file);
        if (capture && name === "index.html") content = content.toString().replace("</body>", '<script src="/capture-select.js"></script><script src="https://mcp.figma.com/mcp/html-to-design/capture.js" async></script></body>');
        response.end(content); return;
      }
      if (path === "/auth/session") {
        json(response, state.signedIn ? { tenantId: "tenant", principalId: state.subject, clientId: "web", expiresAtSeconds: 4102444800 } : {}, state.signedIn ? 200 : 401); return;
      }
      if (request.method === "POST" && request.headers["x-zuno-csrf"] !== "1") { json(response, {}, 403); return; }
      if (path === "/auth/login") { json(response, { authorizationUrl: `${origin}/test-login` }); return; }
      if (path === "/test-login") { state.signedIn = true; response.writeHead(303, { location: "/app/" }).end(); return; }
      if (path === "/auth/logout") { state.signedIn = false; response.writeHead(204).end(); return; }
      if (!state.signedIn) { json(response, {}, 401); return; }
      const expected = request.headers["x-zuno-browser-context"];
      if (expected && expected !== JSON.stringify(["tenant", state.subject, "web"])) { json(response, {}, 401); return; }
      let body = "";
      for await (const chunk of request) body += chunk;
      const input = body ? JSON.parse(body) : {};
      const api = path.replace("/app/api/v1/", "");
      if (api === "workspaces") { json(response, [{ id: "workspace", title: "研发工作区" }]); return; }
      if (api === "sessions" && request.method === "GET") {
        const more = url.searchParams.has("beforeSessionId");
        json(response, { items: state.subject === "alice" ? more ? state.hiddenSessions : state.sessions : [],
          next: !more && state.hiddenSessions.length ? { updatedAt: 1000, sessionId: "session" } : null }); return;
      }
      if (api === "sessions" && request.method === "POST") {
        state.createRequests.push(input.requestId);
        let session = state.creationReceipts.get(input.requestId);
        if (!session) {
          session = { id: `session-${state.creationReceipts.size + 1}`, workspaceId: input.workspaceId, title: input.title, createdAt: 2000, updatedAt: 2000 };
          state.creationReceipts.set(input.requestId, session); state.sessions.unshift(session);
        }
        if (state.losesCreate) { state.losesCreate = false; json(response, { error: "response_unavailable" }, 503); return; }
        json(response, session); return;
      }
      const match = api.match(/^sessions\/([^/]+)\/(.+)$/);
      if (match) {
        const [, session, operation] = match;
        if (operation === "history") {
          if (state.slowSession) await new Promise((resolve) => setTimeout(resolve, state.slowSession));
          json(response, history(session, url.searchParams)); return;
        }
        if (operation === "frames") {
          if (state.frameFailures) { state.frameFailures--; json(response, {}, 409); return; }
          json(response, { version: 1, sessionId: session, frames: [], through: url.searchParams.get("after") ?? "0", more: false }); return;
        }
        if (operation === "live") { json(response, null); return; }
        if (operation === "input-version") { json(response, { version: "0" }); return; }
        if (operation === "turns") {
          state.submissions.push(input.requestId);
          const value = { id: `job-${input.requestId}`, sessionId: session, turnId: "turn", inputId: "input", phase: "ready", inputVersion: "1", waits: [], stopRequested: false, pendingOperations: [] };
          state.requestReceipts.set(input.requestId, value);
          if (state.losesSubmit) { state.losesSubmit = false; json(response, { error: "response_unavailable" }, 503); return; }
          json(response, value); return;
        }
        if (operation.startsWith("requests/")) {
          if (state.lookupFailures) { state.lookupFailures--; json(response, {}, 503); return; }
          const value = state.requestReceipts.get(operation.slice("requests/".length));
          json(response, value ?? {}, value ? 200 : 404); return;
        }
      }
      if (api === "approvals/approval") { json(response, approval()); return; }
      if (api === "approvals/approval/answer") {
        state.answers.push(input);
        if (state.delayedAnswer) await new Promise((resolve) => setTimeout(resolve, state.delayedAnswer));
        state.approval = input.answer === "approve" ? "approved" : "rejected"; json(response, approval()); return;
      }
      json(response, {}, 404);
    } catch { if (!response.headersSent) response.writeHead(500); response.end(); }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = `https://127.0.0.1:${server.address().port}`;
  return { origin, state, close: async () => { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); await rm(directory, { recursive: true, force: true }); } };
}
