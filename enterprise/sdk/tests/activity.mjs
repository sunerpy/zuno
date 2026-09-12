import test from "node:test";
import assert from "node:assert/strict";
import { ActivityClient, ActivityState, counter, decodeFramePage, EnterpriseHttpError } from "../dist/src/index.js";

const record = (id, text = id) => ({
  id, parentId: null, createdAt: "1", actions: [],
  item: { kind: "message", role: "assistant", origin: "model", state: "complete",
    content: [{ kind: "text", text, truncated: false }], usage: null },
});
const frame = (sequence, id, text = id, position = sequence) => ({
  version: 1, sessionId: "session", sequence: String(sequence),
  event: { kind: "upsert", position: String(position), record: record(id, text) },
});
const page = (frames, through = frames.at(-1)?.sequence ?? "0") => ({
  version: 1, sessionId: "session", frames, through: String(through), more: false,
});
const history = (items, through) => ({
  version: 1, sessionId: "session", through: String(through), before: null, items,
});
const item = (position, revision, id, text = id) => ({
  position: String(position), revision: String(revision), record: record(id, text),
});
const response = (value, status = 200) => new Response(JSON.stringify(value), {
  status, headers: { "content-type": "application/json" },
});

test("frames are deduplicated and a conflicting immutable frame is rejected", () => {
  const state = new ActivityState("session");
  const first = page([frame(1, "answer")]);
  state.apply(first);
  state.apply(structuredClone(first));
  assert.equal(state.cursor, "1");
  assert.equal(state.items().length, 1);
  assert.throws(() => state.apply(page([frame(1, "answer", "changed")])), /committed frame changed/);
  assert.equal(state.items()[0].record.item.content[0].text, "answer");
});

test("a gap or wrong owner anywhere in a batch causes no partial mutation", () => {
  const state = new ActivityState("session");
  assert.throws(() => state.apply(page([frame(1, "first"), frame(3, "gap")])), /missing committed/);
  assert.equal(state.cursor, "0");
  assert.deepEqual(state.items(), []);
  const wrong = frame(2, "other"); wrong.sessionId = "another-session";
  assert.throws(() => state.apply(page([frame(1, "first"), wrong])), /another session/);
  assert.equal(state.cursor, "0");
});

test("late history pages cannot overwrite newer committed updates", () => {
  const state = new ActivityState("session");
  state.replaceHistory(history([item(2, 2, "new")], 2));
  state.apply(page([frame(3, "old", "updated", 1)]));
  state.mergeHistory(history([item(1, 1, "old", "stale")], 2));
  assert.deepEqual(state.items().map((row) => row.record.id), ["old", "new"]);
  assert.equal(state.items()[0].record.item.content[0].text, "updated");
  assert.equal(state.cursor, "3");
  assert.throws(() => state.mergeHistory(history([], 3)), /different snapshot/);
});

test("a bounded history window preserves the committed cursor", () => {
  const state = new ActivityState("session", 2);
  state.apply(page([frame(1, "a"), frame(2, "b"), frame(3, "c")]));
  assert.deepEqual(state.items().map((row) => row.record.id), ["b", "c"]);
  assert.equal(state.cursor, "3");
  state.apply(page([frame(1, "a")]));
  assert.deepEqual(state.items().map((row) => row.record.id), ["b", "c"]);
});

test("invalid protocol data and private envelope fields are rejected", () => {
  for (const change of [
    (value) => { value.version = 2; },
    (value) => { value.frames[0].event.record.lease = "must-not-reach-ui"; },
    (value) => { value.frames[0].event.record.item.kind = "private_provider_snapshot"; },
    (value) => { value.through = "00"; },
  ]) {
    const value = page([frame(1, "a")]); change(value);
    assert.throws(() => decodeFramePage(value));
  }
  assert.equal(counter("9007199254740993"), 9007199254740993n);
  assert.equal(counter("18446744073709551615"), 18446744073709551615n);
  assert.throws(() => counter("18446744073709551616"), /overflow/);
});

test("the BFF keeps authentication same-origin and never follows redirects", async () => {
  let request;
  const client = new ActivityClient({
    baseUrl: "https://enterprise.example/app/api/v1/",
    fetch: async (url, options) => { request = { url, options }; return response(page([])); },
  });
  await client.frames("session", "0");
  assert.equal(request.options.credentials, "same-origin");
  assert.equal(request.options.redirect, "error");
  assert.equal(request.options.headers.get("authorization"), null);
  assert.equal(request.url.pathname, "/app/api/v1/sessions/session/frames");
  assert.throws(() => new ActivityClient({ baseUrl: "http://enterprise.example/app/api/v1/" }), /HTTPS/);
  assert.throws(() => new ActivityClient({ baseUrl: "https://enterprise.example/app/api/v1/", accessToken: async () => "secret" }), /explicitly/);
});

test("API bearer credentials are scoped to one explicit HTTPS endpoint", async () => {
  let request;
  const client = new ActivityClient({
    baseUrl: "https://enterprise.example/api/v1/", accessToken: async () => "test-access-token",
    fetch: async (url, options) => { request = { url, options }; return response(page([])); },
  });
  await client.frames("session", "0");
  assert.equal(request.options.credentials, "omit");
  assert.equal(request.options.headers.get("authorization"), "Bearer test-access-token");
  assert.throws(() => new ActivityClient({ baseUrl: "https://user:secret@enterprise.example/api/v1/", accessToken: async () => "token" }), /credential-free/);
});

test("HTTP errors do not echo server secrets and broken cursors do not advance", async () => {
  const options = { baseUrl: "https://enterprise.example/app/api/v1/" };
  const denied = new ActivityClient({ ...options, fetch: async () => response({ secret: "private-server-context" }, 403) });
  await assert.rejects(denied.frames("session", "0"), (error) => error instanceof EnterpriseHttpError && error.status === 403 && !error.message.includes("private"));
  const invalid = new ActivityClient({ ...options, fetch: async () => response(page([], 10)) });
  await assert.rejects(invalid.frames("session", "0"), /invalid continuation/);
});

test("oversized responses cancel the reader before unbounded allocation", async () => {
  let cancelled = false;
  const body = new ReadableStream({
    pull(controller) { controller.enqueue(new Uint8Array(600000)); },
    cancel() { cancelled = true; },
  });
  const client = new ActivityClient({
    baseUrl: "https://enterprise.example/app/api/v1/",
    fetch: async () => new Response(body, { headers: { "content-type": "application/json" } }),
  });
  await assert.rejects(client.frames("session", "0"), /exceeds its limit/);
  assert.equal(cancelled, true);
});

test("watch cancellation interrupts its wait and cannot start another request", async () => {
  let requests = 0;
  const abort = new AbortController();
  const client = new ActivityClient({
    baseUrl: "https://enterprise.example/app/api/v1/",
    fetch: async () => { requests++; return response(page([frame(1, "a")])); },
  });
  const stream = client.watch("session", "0", { signal: abort.signal, intervalMs: 30000 });
  assert.equal((await stream.next()).value.through, "1");
  abort.abort(new Error("test cancelled"));
  await assert.rejects(stream.next(), /test cancelled/);
  assert.equal(requests, 1);
});
