import test from "node:test";
import assert from "node:assert/strict";
import { EnterpriseClient, EnterpriseHttpError } from "../dist/src/index.js";

const job = { id: "job", sessionId: "session", turnId: "turn", inputId: "input", phase: "ready", inputVersion: "1", waits: [], stopRequested: false, pendingOperations: [] };
const response = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });

test("mutations preserve request identity, send CSRF/context and do not mechanically retry", async () => {
  const calls = [];
  const client = new EnterpriseClient({
    baseUrl: "https://enterprise.example/app/api/v1/",
    browserContext: () => '["tenant","alice","web"]',
    fetch: async (url, options) => {
      calls.push({ url, options });
      return url.pathname.endsWith("/turns") ? response({}, 503) : response(job);
    },
  });
  const request = { requestId: "request", expectedInputVersion: "0", text: "Inspect" };
  await assert.rejects(client.submit("session", request), EnterpriseHttpError);
  assert.equal(calls.length, 1);
  assert.deepEqual(JSON.parse(calls[0].options.body), request);
  assert.equal(calls[0].options.headers.get("x-zuno-csrf"), "1");
  assert.equal(calls[0].options.headers.get("x-zuno-browser-context"), '["tenant","alice","web"]');
  assert.equal((await client.submission("session", "request")).id, "job");
  assert.equal(calls[1].url.pathname, "/app/api/v1/sessions/session/requests/request");
});

test("well-formed responses cannot silently switch resources", async () => {
  const client = new EnterpriseClient({
    baseUrl: "https://enterprise.example/app/api/v1/",
    fetch: async () => response({ ...job, sessionId: "another", id: "other-job" }),
  });
  await assert.rejects(client.job("job"), /identity mismatch/);
  await assert.rejects(client.submission("session", "request"), /another session/);
});

test("creation and cancellation responses retain workspace and turn identity", async () => {
  const client = new EnterpriseClient({
    baseUrl: "https://enterprise.example/app/api/v1/",
    fetch: async (url) => url.pathname.endsWith("/cancel")
      ? response({ requestId: "cancel", jobId: "job", turnId: "other-turn", stoppedJobs: [], pendingOperations: [] })
      : response({ id: "session", workspaceId: "other", title: "Task", createdAt: 1, updatedAt: 1 }),
  });
  await assert.rejects(client.createSession({ requestId: "create", workspaceId: "workspace", title: "Task" }), /another workspace/);
  await assert.rejects(client.cancel("job", { requestId: "cancel", expectedTurnId: "turn", reason: "Stop" }), /identity mismatch/);
});

test("workflow views preserve typed node dependencies and refuse a different run owner", async () => {
  const value = { id: "run", kind: "workflow", jobId: "job", state: "active", name: "inspection", nodes: [
    { id: "node", nodeId: "review", jobId: "node-job", state: "waiting", dependsOn: ["scan"],
      waits: [{ invocationId: "call", target: { kind: "approval", approval_id: "approval" } }] },
  ] };
  const client = new EnterpriseClient({ baseUrl: "https://enterprise.example/app/api/v1/", fetch: async () => response(value) });
  assert.deepEqual((await client.workflow("job")).nodes[0].dependsOn, ["scan"]);
  await assert.rejects(client.workflow("other-job"), /identity mismatch/);
  value.nodes[0].lease = "private-worker-state";
  await assert.rejects(client.workflow("job"), /Invalid enterprise application/);
});

test("Council phases, timeout outcomes and exact deadlines remain typed and private", async () => {
  const value = { id: "run", kind: "council", jobId: "job", state: "active", name: "council:inspect", nodes: [],
    council: { preset: "inspect", quorum: 1, phase: "stopping", seatDeadline: "9007199254740993", deadline: "9007199254741993", synthesisDeadline: null,
      seats: [{ id: "seat", jobId: "seat-job", state: "timed_out", attempts: 2 }] } };
  const client = new EnterpriseClient({ baseUrl: "https://enterprise.example/app/api/v1/", fetch: async () => response(value) });
  assert.equal((await client.workflow("job")).council.seatDeadline, "9007199254740993");
  value.council.seats[0].state = "invented-state";
  await assert.rejects(client.workflow("job"), /Invalid enterprise application/);
  value.council.seats[0].state = "timed_out";
  value.council.lease = "private-worker-state";
  await assert.rejects(client.workflow("job"), /Invalid enterprise application/);
});
