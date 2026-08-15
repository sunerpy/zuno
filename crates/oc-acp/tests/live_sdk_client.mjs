import assert from "node:assert/strict"
import { spawn } from "node:child_process"
import { pathToFileURL } from "node:url"
import { readFile } from "node:fs/promises"

const [binary, sdkEntry] = process.argv.slice(2)
const sdk = await import(pathToFileURL(sdkEntry).href)
const packageJson = JSON.parse(
  await readFile(new URL("../package.json", pathToFileURL(sdkEntry)), "utf8"),
)
const child = spawn(binary, [], { stdio: ["pipe", "pipe", "pipe"] })
const rawStdout = []
const rawStderr = []
child.stderr.on("data", (chunk) => rawStderr.push(Buffer.from(chunk)))

const capturedOutput = new TransformStream({
  transform(chunk, controller) {
    rawStdout.push(Buffer.from(chunk))
    controller.enqueue(chunk)
  },
})
const input = /** @type {ReadableStream<Uint8Array>} */ (
  import("node:stream").then(({ Readable }) => Readable.toWeb(child.stdout)).then((stream) =>
    stream.pipeThrough(capturedOutput),
  )
)
const { Readable, Writable } = await import("node:stream")
const updates = []
let permissionRequests = 0
const connection = new sdk.ClientSideConnection(
  () => ({
    async sessionUpdate(notification) {
      updates.push(notification)
    },
    async requestPermission(request) {
      permissionRequests += 1
      assert.equal(request.sessionId.startsWith("ses_"), true)
      assert.equal(request.toolCall.toolCallId, "call_permission")
      assert.equal(request.options.some((option) => option.optionId === "allow_once"), true)
      return { outcome: { outcome: "selected", optionId: "allow_once" } }
    },
  }),
  sdk.ndJsonStream(
    Writable.toWeb(child.stdin),
    await input,
  ),
)

const initialized = await connection.initialize({
  protocolVersion: 1,
  clientInfo: { name: "oc-acp-live-test", version: "1" },
  clientCapabilities: {},
})
assert.equal(initialized.protocolVersion, 1)
assert.equal(initialized.agentCapabilities.loadSession, true)
assert.deepEqual(initialized.agentCapabilities.sessionCapabilities.close, {})
assert.equal(initialized.authMethods[0].id, "zuno-login")

await connection.authenticate({ methodId: initialized.authMethods[0].id })
const created = await connection.newSession({ cwd: process.cwd(), mcpServers: [] })
assert.equal(typeof created.sessionId, "string")
await connection.listSessions({ cwd: process.cwd() })
await connection.resumeSession({ cwd: process.cwd(), sessionId: created.sessionId, mcpServers: [] })
await connection.loadSession({ cwd: process.cwd(), sessionId: created.sessionId, mcpServers: [] })
await connection.setSessionMode({ sessionId: created.sessionId, modeId: "build" })
await connection.unstable_setSessionModel({ sessionId: created.sessionId, modelId: "test/model" })
await connection.setSessionConfigOption({ sessionId: created.sessionId, configId: "mode", value: "build" })

const normal = await connection.prompt({
  sessionId: created.sessionId,
  messageId: "00000000-0000-4000-8000-000000000001",
  prompt: [{ type: "text", text: "permission turn" }],
})
assert.equal(normal.stopReason, "end_turn")
assert.equal(permissionRequests, 1)
assert.equal(updates.some(({ update }) => update.sessionUpdate === "tool_call"), true)
assert.equal(updates.some(({ update }) => update.sessionUpdate === "tool_call_update" && update.status === "completed"), true)

const beforeCancel = updates.length
const cancelledPromise = connection.prompt({
  sessionId: created.sessionId,
  messageId: "00000000-0000-4000-8000-000000000002",
  prompt: [{ type: "text", text: "wait for cancellation" }],
})
while (!updates.slice(beforeCancel).some(({ update }) => update._meta?.phase === "cancel-ready")) {
  await new Promise((resolve) => setTimeout(resolve, 5))
}
await connection.cancel({ sessionId: created.sessionId })
const cancelled = await cancelledPromise
const cancelUpdates = updates.slice(beforeCancel)
const finalIndex = cancelUpdates.findIndex(({ update }) => update._meta?.phase === "cancel-final")
assert.equal(cancelled.stopReason, "cancelled")
assert.notEqual(finalIndex, -1)

const forked = await connection.unstable_forkSession({
  cwd: process.cwd(),
  sessionId: created.sessionId,
  mcpServers: [],
})
assert.notEqual(forked.sessionId, created.sessionId)
await connection.closeSession({ sessionId: forked.sessionId })
await connection.closeSession({ sessionId: created.sessionId })

child.stdin.end()
await connection.closed
await new Promise((resolve, reject) => {
  child.once("exit", (code) => code === 0 ? resolve() : reject(new Error(`agent exited ${code}`)))
})
const stdout = Buffer.concat(rawStdout).toString("utf8")
const lines = stdout.split("\n").filter((line) => line.trim())
for (const line of lines) JSON.parse(line)
const stderr = Buffer.concat(rawStderr).toString("utf8")

process.stdout.write(JSON.stringify({
  sdkVersion: packageJson.version,
  protocolVersion: initialized.protocolVersion,
  permissionRequests,
  normalStopReason: normal.stopReason,
  cancelStopReason: cancelled.stopReason,
  cancelFinalUpdateBeforeResponse: finalIndex >= 0,
  frames: lines.length,
  stdoutWasPureNdjson: true,
  stderrWasNonEmpty: stderr.length > 0,
}))
