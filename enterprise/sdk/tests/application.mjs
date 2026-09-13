import test from "node:test";
import assert from "node:assert/strict";
import { EnterpriseClient, EnterpriseHttpError } from "../dist/src/index.js";

const job = { id: "job", sessionId: "session", turnId: "turn", inputId: "input", phase: "ready", inputVersion: "1", waits: [], stopRequested: false, pendingOperations: [] };
const response = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });

test("public identity contains only validated actor coordinates",async()=>{
  const actor={owner:{tenantId:"tenant",principalId:"alice"},kind:"user",clientId:"web"};
  const client=value=>new EnterpriseClient({baseUrl:"https://enterprise.example/api/v1/",accessToken:async()=>"token",fetch:async()=>response(value)});
  assert.deepEqual(await client(actor).identity(),actor);
  await assert.rejects(client({...actor,accessToken:"private"}).identity(),/Invalid enterprise application response/);
});

test("Skill activation preserves owner resource identity and exact revision without exposing a lease", async () => {
  const value={id:"installed",workspaceId:"workspace",candidateId:"candidate",name:"proof",description:"Read proof",
    revision:"9007199254740993",source:"enterprise-skill://installed/9007199254740993",contentDigest:"a".repeat(64),active:true};
  const client=body=>new EnterpriseClient({baseUrl:"https://enterprise.example/api/v1/",accessToken:async()=>"test-token",fetch:async()=>response(body)});
  const request={requestId:"activate",expectedRevision:"9007199254740992",active:true};
  assert.equal((await client(value).activateSkill("installed",request)).revision,value.revision);
  await assert.rejects(client({...value,id:"foreign"}).activateSkill("installed",request),/identity mismatch/);
  await assert.rejects(client({...value,revision:request.expectedRevision}).activateSkill("installed",request),/identity mismatch/);
  await assert.rejects(client({...value,lease:"internal"}).activateSkill("installed",request),/Invalid enterprise application response/);
  await assert.rejects(client(value).rollbackSkill("installed",{requestId:"rollback",expectedRevision:request.expectedRevision,targetRevision:"1"}),/identity mismatch/);
  await assert.rejects(client({items:[value],after:"foreign"}).installedSkills("workspace"),/cursor identity mismatch/);
  await assert.rejects(client({items:[{...value,workspaceId:"foreign"}],after:null}).installedSkills("workspace"),/workspace identity mismatch/);
});

test("Skill evaluation responses preserve reviewed candidate identity and bounded scores", async () => {
  const value={id:"skill",sourceJobId:"source",name:"Skill",baselineContent:"before",proposedContent:"after",
    cases:[{id:"case",prompt:"scenario",expected:"expected",calls:[],kind:"failure",weight:1}],
    digest:"a".repeat(64),evaluation:{id:"model",version:1,sha256:"b".repeat(64)},state:"pending_review",jobId:null,report:null};
  const client=(body)=>new EnterpriseClient({baseUrl:"https://enterprise.example/api/v1/",
    accessToken:async()=>"test-token",fetch:async()=>response(body)});
  assert.equal((await client(value).skillCandidate("skill")).state,"pending_review");
  await assert.rejects(client({...value,id:"other"}).skillCandidate("skill"),/identity mismatch/);
  await assert.rejects(client({...value,digest:"c".repeat(64)}).evaluateSkill("skill",{requestId:"review",expectedDigest:value.digest}),/identity mismatch/);
  const observation={score:255,passed:true,criticalFailure:false,details:{}};
  await assert.rejects(client({...value,report:{passed:true,baselineMetric:0,candidateMetric:255,
    cases:[{caseId:"case",baseline:observation,candidate:observation}]}}).skillCandidate("skill"),/Invalid enterprise application response/);
});

test("shared Memory keeps namespace, workspace, revision and review identity", async () => {
  const space={id:"runbooks",workspaceId:"workspace",title:"Runbooks",enabled:true,policyRevision:"1",
    documentRevision:"9007199254740993",entries:["reviewed"],digest:"a".repeat(64),role:"reader",characterLimit:3000};
  const change={id:"change",spaceId:"runbooks",author:"alice",baseRevision:"1",policyRevision:"1",before:[],after:["reviewed"],
    reason:"Record the runbook",state:"pending",stateDigest:"b".repeat(64),decidedBy:null,appliedRevision:null};
  const client=(value)=>new EnterpriseClient({baseUrl:"https://enterprise.example/api/v1/",accessToken:async()=>"test-token",fetch:async()=>response(value)});
  assert.equal((await client(space).sharedMemorySpace("runbooks")).documentRevision,"9007199254740993");
  await assert.rejects(client({...space,id:"foreign"}).sharedMemorySpace("runbooks"),/identity mismatch/);
  await assert.rejects(client({items:[{...space,workspaceId:"other"}],after:null}).sharedMemorySpaces("workspace"),/workspace mismatch/);
  await assert.rejects(client({...change,spaceId:"foreign"}).sharedMemoryChange("runbooks","change"),/identity mismatch/);
  await assert.rejects(client({...change,id:"different"}).reviewSharedMemory("runbooks",
    {requestId:"review",changeId:"change",expectedState:change.stateDigest,decision:"apply"}),/identity mismatch/);
});

test("MCP review retains exact declared arguments without granting private execution authority", async () => {
  const review={approvalId:"approval",operationId:"mcp-operation",server:"server",tool:"apply",endpoint:"https://mcp.example/tool",
    definition:{name:"apply",inputSchema:{type:"object"}},arguments:{value:"reviewed"},admitted:false};
  const client=(value)=>new EnterpriseClient({baseUrl:"https://enterprise.example/api/v1/",
    accessToken:async()=>"test-token",fetch:async()=>response(value)});
  assert.deepEqual((await client(review).mcpReview("approval")).arguments,review.arguments);
  await assert.rejects(client({...review,approvalId:"other"}).mcpReview("approval"),/identity mismatch/);
  await assert.rejects(client({...review,lease:{epoch:1}}).mcpReview("approval"),/Invalid enterprise application response/);
});

test("edit review preserves exact before/after text and rejects foreign approval or private fields", async () => {
  const review = { approvalId: "approval", operationId: "edit", admitted: false,
    review: [{ path: "src/file.rs", before: "before\n", after: "after\n" }] };
  const client = (value) => new EnterpriseClient({ baseUrl: "https://enterprise.example/api/v1/",
    accessToken: async () => "test-token", fetch: async () => response(value) });
  assert.deepEqual((await client(review).editReview("approval")).review, review.review);
  await assert.rejects(client({ ...review, approvalId: "foreign" }).editReview("approval"), /identity mismatch/);
  await assert.rejects(client({ ...review, lease: "private" }).editReview("approval"), /Invalid enterprise application response/);
});

const learning = {
  id: "learn-one", workspaceId: "workspace", sessionId: "session", sourceJobId: "root",
  stage: "extraction", state: "running", attempts: "2",
  budget: { limit: "9007199254740993", charged: "120", reserved: "1000", modelRequests: "2", unconfirmedRequests: "1" },
  createdAtMs: "1000", updatedAtMs: "1005", readyAtMs: null, deadlineAtMs: "4000", failure: null, canCancel: true,
};

test("learning management preserves exact counters, scoped cursors and cancellation identity", async () => {
  const calls = [];
  const client = new EnterpriseClient({
    baseUrl: "https://enterprise.example/app/api/v1/",
    browserContext: () => '["tenant","alice","web"]',
    fetch: async (url, options) => {
      calls.push({ url, options });
      if (url.pathname.endsWith("/cancel")) {
        return response({ requestId: "cancel-one", job: { ...learning, state: "cancelled", canCancel: false, budget: { ...learning.budget, charged: "1120", reserved: "0" } } });
      }
      if (url.pathname.endsWith("/jobs")) return response({ items: [learning], before: { createdAtMs: "1000", jobId: learning.id } });
      return response(learning);
    },
  });
  const page = await client.learningJobs("workspace", { limit: 1, before: { createdAtMs: "2000", jobId: "later" }, stage: "extraction" });
  assert.equal(page.items[0].budget.limit, "9007199254740993");
  assert.equal(calls[0].url.searchParams.get("beforeCreatedAtMs"), "2000");
  assert.equal(calls[0].url.searchParams.get("beforeJobId"), "later");
  assert.equal((await client.learningJob(learning.id)).id, learning.id);
  const cancelled = await client.cancelLearning(learning.id, { requestId: "cancel-one" });
  assert.equal(cancelled.job.state, "cancelled");
  assert.deepEqual(JSON.parse(calls[2].options.body), { requestId: "cancel-one" });
  assert.equal(calls[2].options.headers.get("x-zuno-csrf"), "1");
  assert.equal(calls[2].options.headers.get("x-zuno-browser-context"), '["tenant","alice","web"]');
});

test("learning responses cannot substitute a job, workspace, cursor or cancellation receipt", async () => {
  const client = (value) => new EnterpriseClient({ baseUrl: "https://enterprise.example/api/v1/", accessToken: async () => "test-token", fetch: async () => response(value) });
  await assert.rejects(client({ ...learning, id: "other" }).learningJob(learning.id), /identity mismatch/);
  await assert.rejects(client({ items: [{ ...learning, workspaceId: "other" }], before: null }).learningJobs("workspace"), /identity mismatch/);
  await assert.rejects(client({ items: [learning], before: { createdAtMs: "1001", jobId: "other" } }).learningJobs("workspace"), /cursor identity mismatch/);
  await assert.rejects(client({ requestId: "different", job: learning }).cancelLearning(learning.id, { requestId: "expected" }), /identity mismatch/);
  await assert.rejects(client({ ...learning, grant: "private-credential" }).learningJob(learning.id), /Invalid enterprise application response/);
});

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

test("merge review is approval-bound and content streams preserve BFF identity without exposing a Worker grant", async () => {
  const value = { approvalId: "approval", operationId: "operation", childJobId: "child", admitted: false,
    plan: { baseTree: "a".repeat(64), parentTree: "b".repeat(64), childTree: "c".repeat(64), changes: [] } };
  const requests = [];
  const client = new EnterpriseClient({ baseUrl: "https://enterprise.example/app/api/v1/", browserContext: () => "context",
    fetch: async (url, init) => {
      requests.push({ url: new URL(url), init });
      if (new URL(url).pathname.endsWith("/content")) return new Response(new Uint8Array([0, 255]), { headers: {
        "content-type": "application/octet-stream", "content-length": "2", "x-zuno-content-sha256": "a".repeat(64),
      } });
      return response(value);
    } });
  assert.equal((await client.mergeReview("approval")).operationId, "operation");
  await assert.rejects(client.mergeReview("different"), /identity mismatch/);
  const downloaded = await client.mergeContent("approval", "child", "folder/file\nname");
  assert.deepEqual([...new Uint8Array(await downloaded.arrayBuffer())], [0, 255]);
  const request = requests.at(-1);
  assert.equal(request.url.searchParams.get("path"), "folder/file\nname");
  assert.equal(request.init.headers.get("x-zuno-browser-context"), "context");
  assert.equal(request.init.headers.has("x-zuno-job-grant"), false);
  assert.equal(request.init.redirect, "error");
  await assert.rejects(client.mergeContent("approval", "child", "../secret"), /Invalid merge content/);
});

test("workspace archive upload keeps raw bytes and request identity with explicit BFF CSRF", async () => {
  const value = { id: "import", sessionId: "session", state: "uploading", sha256: "a".repeat(64), bytes: "4", createdAt: "1" };
  const calls = [];
  const client = new EnterpriseClient({ baseUrl: "https://enterprise.example/app/api/v1/", browserContext: () => "context",
    fetch: async (url, init) => {
      calls.push({ url: new URL(url), init });
      return response({ ...value, state: init.method === "PUT" ? "ready" : value.state });
    } });
  const prepared = await client.beginWorkspaceImport("session", { requestId: "request", expectedInputVersion: "0", sha256: value.sha256, bytes: "4" });
  assert.equal(prepared.id, "import");
  const bytes = new Blob([new Uint8Array([0, 1, 2, 255])]);
  assert.equal((await client.uploadWorkspaceArchive("session", "import", bytes)).state, "ready");
  const call = calls.at(-1);
  assert.equal(call.init.method, "PUT");
  assert.equal(call.init.headers.get("content-type"), "application/x-tar");
  assert.equal(call.init.headers.get("x-zuno-csrf"), "1");
  assert.equal(call.init.headers.get("x-zuno-browser-context"), "context");
  assert.deepEqual([...new Uint8Array(await call.init.body.arrayBuffer())], [0, 1, 2, 255]);
  await assert.rejects(client.workspaceImport("other-session", "import"), /identity mismatch/);
  await assert.rejects(client.uploadWorkspaceArchive("session", "import", new Blob([])), /bound/);
});
