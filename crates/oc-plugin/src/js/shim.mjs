// The resident JavaScript half of the compat host.
//
// This file is embedded in the Rust binary with `include_str!` and written to a
// temporary directory at spawn time, so it must be self-contained: no imports
// other than Node/Bun built-ins and whatever the plugin's own install tree
// already provides.
//
// # Why a socket instead of stdio
//
// stdin belongs to the terminal. `opencode-antigravity-auth` prompts through
// `node:readline/promises` (`dist/src/plugin/cli.js:1`) and a plugin that must
// read a device code cannot have a JSON protocol on its stdin. So the protocol
// runs over a loopback socket whose port and token arrive in the environment,
// and fd 0/1/2 stay inherited. The terminal-lease handshake below is what makes
// reading fd 0 safe: the shim asks before it prompts, the Rust host takes the
// lease from `oc_engine::terminal_lease`, the TUI steps aside, and the lease is
// returned afterwards.
//
// # Why callables become handles instead of data
//
// An auth hook's substance is closures: `validate`, `loader`, `authorize`, and
// the `callback` an `authorize` result carries. Marshalling the hook object once
// would drop exactly the parts that do the work. So every function in a value
// crossing the boundary is retained here in `handles` and replaced with
// `{ "$fn": id }`; the Rust side round-trips `call` frames against that id for
// as long as this process lives. That is what "resident" means.

import net from "node:net";
import path from "node:path";
import { Buffer } from "node:buffer";
import { pathToFileURL } from "node:url";
import { createRequire } from "node:module";

const PROTOCOL_VERSION = "js-compat-1";

// Protocol frames go to the socket; anything a plugin prints goes to stderr.
// A plugin that writes to stdout must not be able to corrupt a frame, and
// several of them do write (progress lines, warnings), so console is rebound
// before the first plugin module is imported.
const stderrWrite = process.stderr.write.bind(process.stderr);
function toStderr(...parts) {
  try {
    stderrWrite(
      parts
        .map((part) =>
          typeof part === "string" ? part : safeInspect(part),
        )
        .join(" ") + "\n",
    );
  } catch {
    // A closed stderr must not take the host down.
  }
}
function safeInspect(value) {
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}
console.log = toStderr;
console.info = toStderr;
console.debug = toStderr;
console.warn = toStderr;
console.error = toStderr;
console.trace = toStderr;

const port = Number(process.env.OC_JS_HOST_PORT);
const token = process.env.OC_JS_HOST_TOKEN ?? "";
if (!Number.isInteger(port) || port <= 0 || token === "") {
  toStderr("oc-js-shim: OC_JS_HOST_PORT and OC_JS_HOST_TOKEN are required");
  process.exit(64);
}

// ---------------------------------------------------------------------------
// Handle registry
// ---------------------------------------------------------------------------

/** @type {Map<number, Function>} */
const handles = new Map();
/** Reverse map so the same closure keeps one id across repeated encodings. */
const handleIds = new WeakMap();
let nextHandle = 1;

function retain(fn) {
  const existing = handleIds.get(fn);
  if (existing !== undefined) return existing;
  const id = nextHandle++;
  handles.set(id, fn);
  handleIds.set(fn, id);
  return id;
}

// Depth cap rather than full generality: an unbounded walk over a plugin's
// internal object graph is how a bounded-memory host stops being bounded.
const MAX_DEPTH = 16;

function childPointer(path, key) {
  const segment = String(key).replaceAll("~", "~0").replaceAll("/", "~1");
  return `${path}/${segment}`;
}

function hostBoundaries(
  value,
  depth = 0,
  seen = new Set(),
  path = "",
  boundaries = new Map(),
) {
  if (value === null || typeof value !== "object") return boundaries;
  if (depth >= MAX_DEPTH) {
    boundaries.set(path || "/", value);
    return boundaries;
  }
  if (seen.has(value)) return boundaries;
  if (value instanceof Error || value instanceof URL || value instanceof Date) {
    return boundaries;
  }
  seen.add(value);
  try {
    if (Array.isArray(value)) {
      value.forEach((item, index) =>
        hostBoundaries(
          item,
          depth + 1,
          seen,
          childPointer(path, index),
          boundaries,
        ),
      );
      return boundaries;
    }
    for (const key of Object.keys(value)) {
      let member;
      try {
        member = value[key];
      } catch {
        continue;
      }
      hostBoundaries(
        member,
        depth + 1,
        seen,
        childPointer(path, key),
        boundaries,
      );
    }
    return boundaries;
  } finally {
    seen.delete(value);
  }
}

function mutationTouchesBoundary(mutations, pointer) {
  if (!mutations) return false;
  if (pointer === "/") return mutations.size > 0;
  const prefix = `${pointer}/`;
  for (const path of mutations) {
    if (path === pointer || path.startsWith(prefix)) return true;
  }
  return false;
}

function encode(
  value,
  depth = 0,
  seen = new Set(),
  path = "",
  boundaries,
  mutations,
  truncations,
) {
  if (value === null || value === undefined) return null;
  const kind = typeof value;
  if (kind === "function") return { $fn: retain(value), $arity: value.length };
  if (kind === "bigint") return { $bigint: value.toString() };
  if (kind !== "object") return value;
  if (depth >= MAX_DEPTH) {
    const pointer = path || "/";
    const source =
      boundaries?.get(pointer) === value &&
      !mutationTouchesBoundary(mutations, pointer)
        ? "host"
        : "plugin";
    truncations?.push({ path: pointer, source });
    return { $truncated: true, $path: pointer, $source: source };
  }
  if (seen.has(value)) return { $cycle: true };
  if (value instanceof Error) {
    return { $error: value.message ?? String(value), $name: value.name };
  }
  if (value instanceof URL) return value.toString();
  if (value instanceof Date) return value.toISOString();
  seen.add(value);
  try {
    if (Array.isArray(value)) {
      return value.map((item, index) =>
        encode(
          item,
          depth + 1,
          seen,
          childPointer(path, index),
          boundaries,
          mutations,
          truncations,
        ),
      );
    }
    const out = {};
    for (const key of Object.keys(value)) {
      let member;
      try {
        member = value[key];
      } catch {
        continue; // A throwing getter is not worth failing the whole encode.
      }
      out[key] = encode(
        member,
        depth + 1,
        seen,
        childPointer(path, key),
        boundaries,
        mutations,
        truncations,
      );
    }
    return out;
  } finally {
    seen.delete(value);
  }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

const socket = net.connect({ host: "127.0.0.1", port });
socket.setNoDelay(true);

let pendingId = 1;
/** @type {Map<number, {resolve: Function, reject: Function}>} */
const pendingRequests = new Map();

function send(frame) {
  socket.write(JSON.stringify(frame) + "\n");
}

function respond(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

function respondError(id, message) {
  send({ jsonrpc: "2.0", id, error: { code: -32000, message } });
}

/** Ask the Rust host something and await its answer. */
function request(method, params) {
  const id = pendingId++;
  return new Promise((resolve, reject) => {
    pendingRequests.set(id, { resolve, reject });
    send({ jsonrpc: "2.0", id, method, params });
  });
}

let buffer = "";
socket.on("data", (chunk) => {
  buffer += chunk.toString("utf8");
  for (;;) {
    const newline = buffer.indexOf("\n");
    if (newline < 0) break;
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    if (line.trim() === "") continue;
    let frame;
    try {
      frame = JSON.parse(line);
    } catch (error) {
      toStderr("oc-js-shim: malformed host frame:", String(error));
      continue;
    }
    dispatch(frame);
  }
});
socket.on("error", (error) => {
  toStderr("oc-js-shim: socket error:", String(error));
  process.exit(70);
});
socket.on("close", () => process.exit(0));
socket.on("connect", () => send({ jsonrpc: "2.0", method: "hello", params: { token, protocol: PROTOCOL_VERSION, pid: process.pid } }));

function dispatch(frame) {
  if (frame.id !== undefined && frame.method === undefined) {
    const waiter = pendingRequests.get(frame.id);
    pendingRequests.delete(frame.id);
    if (!waiter) return;
    if (frame.error) waiter.reject(new Error(frame.error.message ?? "host error"));
    else waiter.resolve(frame.result);
    return;
  }
  if (frame.method === undefined) return;
  handleRequest(frame).catch((error) => {
    if (frame.id !== undefined) {
      respondError(frame.id, error?.message ?? String(error));
    }
  });
}

// ---------------------------------------------------------------------------
// Terminal lease
// ---------------------------------------------------------------------------

// Exposed so an interactive plugin flow can be gated on the lease without the
// plugin knowing the protocol exists. `readline` is wrapped below.
async function withTerminal(purpose, body) {
  const grant = await request("terminal.acquire", { purpose });
  if (!grant?.granted) {
    throw new Error(
      grant?.detail ?? "the terminal was not granted to this plugin",
    );
  }
  try {
    return await body();
  } finally {
    if (grant.managed) {
      try {
        await request("terminal.release", {});
      } catch (error) {
        toStderr("oc-js-shim: terminal release failed:", String(error));
      }
    }
  }
}

// A plugin reaching for `node:readline/promises` is the case the lease exists
// for, so the module is patched before any plugin can import it. The patch is
// applied to the module's `createInterface` so the plugin's own code is
// untouched: it calls `rl.question(...)` and the handshake happens underneath.
async function patchReadline() {
  let readline;
  try {
    readline = await import("node:readline/promises");
  } catch {
    return; // No readline in this runtime; nothing to gate.
  }
  const original = readline.default?.createInterface ?? readline.createInterface;
  if (typeof original !== "function") return;
  const patched = function createInterface(...args) {
    const rl = original.apply(this, args);
    const question = rl.question?.bind(rl);
    if (typeof question === "function") {
      rl.question = (query, ...rest) =>
        withTerminal(String(query).slice(0, 120), () => question(query, ...rest));
    }
    return rl;
  };
  try {
    if (readline.default) readline.default.createInterface = patched;
  } catch {
    // A frozen namespace is not fatal; the explicit path below still works.
  }
  globalThis.__ocWithTerminal = withTerminal;
}

// ---------------------------------------------------------------------------
// PluginInput construction
// ---------------------------------------------------------------------------

function resolveSdk(entry, explicit) {
  const candidates = [];
  if (explicit) candidates.push(explicit);
  // Walk up from the entry point: a plugin's own install tree is where its own
  // SDK version lives, and using a different copy would hand the plugin a
  // client shape it was not built against.
  let dir = path.dirname(entry);
  for (let i = 0; i < 12; i++) {
    candidates.push(path.join(dir, "node_modules", "@opencode-ai", "sdk"));
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return candidates;
}

async function loadSdk(entry, explicit) {
  const errors = [];
  for (const candidate of resolveSdk(entry, explicit)) {
    for (const suffix of ["", "/dist/index.js", "/src/index.ts"]) {
      const target = candidate + suffix;
      try {
        const mod = await import(pathToFileURL(target).href);
        if (typeof mod.createOpencodeClient === "function") {
          return { createOpencodeClient: mod.createOpencodeClient, from: target };
        }
      } catch (error) {
        errors.push(`${target}: ${error?.message ?? String(error)}`);
      }
    }
  }
  const err = new Error(
    "could not locate a real @opencode-ai/sdk with createOpencodeClient; " +
      "refusing to substitute a hand-rolled client. Tried:\n" +
      errors.slice(0, 6).join("\n"),
  );
  err.code = "SDK_NOT_FOUND";
  throw err;
}

async function makeShell(_entry) {
  // Bun's real `$` when available. Under node there is no Bun shell, and a
  // fabricated one would silently change what a plugin's shell calls do, so the
  // stub refuses loudly instead.
  try {
    const bun = await import("bun");
    if (bun?.$) return bun.$;
  } catch {
    // Not Bun.
  }
  const refuse = () => {
    throw new Error(
      "PluginInput.$ (the Bun shell) is unavailable: this plugin host is " +
        "running under node. Install bun to use plugins that shell out.",
    );
  };
  const shell = (..._args) => refuse();
  shell.cwd = refuse;
  shell.env = refuse;
  shell.braces = refuse;
  shell.escape = refuse;
  shell.nothrow = refuse;
  shell.throws = refuse;
  return shell;
}

/** Every `experimental_workspace.register` call, reported back after init. */
const workspaceRegistrations = [];

async function buildInput(params) {
  let client;
  let from = null;
  try {
    const sdk = await loadSdk(params.entry, params.sdkModule);
    from = sdk.from;
    const password = process.env.OPENCODE_SERVER_PASSWORD;
    const headers = password
      ? {
          Authorization: `Basic ${Buffer.from(
            `${process.env.OPENCODE_SERVER_USERNAME ?? "opencode"}:${password}`,
            "utf8",
          ).toString("base64")}`,
        }
      : undefined;
    client = sdk.createOpencodeClient({
      baseUrl: params.serverUrl,
      directory: params.directory,
      headers,
    });
  } catch (error) {
    if (!String(params.spec).startsWith("file:")) throw error;
    const unavailable = () => {
      throw new Error("@opencode-ai/sdk is unavailable to this file plugin fixture");
    };
    client = new Proxy(unavailable, {
      get: unavailable,
      apply: unavailable,
    });
  }
  return {
    input: {
      client,
      project: params.project,
      directory: params.directory,
      worktree: params.worktree,
      serverUrl: new URL(params.serverUrl),
      experimental_workspace: {
        register(type, adapter) {
          workspaceRegistrations.push({
            type: String(type),
            adapter: adapter === undefined ? null : encode(adapter),
          });
        },
      },
      $: await makeShell(params.entry),
    },
    sdkFrom: from,
  };
}

// ---------------------------------------------------------------------------
// Module shape resolution — mirrors the oracle exactly
// ---------------------------------------------------------------------------

function isRecord(value) {
  return typeof value === "object" && value !== null;
}

/**
 * `packages/opencode/src/plugin/shared.ts` `readV1Plugin`, in "detect" mode.
 * Returns the default-export module object when it carries `id`/`server`/`tui`.
 */
function readV1Plugin(mod, spec, kind) {
  const value = mod.default;
  if (!isRecord(value)) return undefined;
  if (!("id" in value) && !("server" in value) && !("tui" in value)) {
    return undefined;
  }
  const server = "server" in value ? value.server : undefined;
  const tui = "tui" in value ? value.tui : undefined;
  if (server !== undefined && typeof server !== "function") {
    throw new TypeError(`Plugin ${spec} has invalid server export`);
  }
  if (tui !== undefined && typeof tui !== "function") {
    throw new TypeError(`Plugin ${spec} has invalid tui export`);
  }
  if (server !== undefined && tui !== undefined) {
    throw new TypeError(
      `Plugin ${spec} must default export either server() or tui(), not both`,
    );
  }
  if (kind === "server" && server === undefined) {
    throw new TypeError(`Plugin ${spec} must default export an object with server()`);
  }
  if (kind === "tui" && tui === undefined) {
    throw new TypeError(`Plugin ${spec} must default export an object with tui()`);
  }
  return value;
}

/**
 * `packages/opencode/src/plugin/index.ts:98-109` `getLegacyPlugins`: every
 * exported function is a plugin factory, deduplicated by identity. Antigravity
 * relies on this — it has no default export, and `GoogleOAuthPlugin` is the
 * same function object as `AntigravityCLIOAuthPlugin`, so identity dedup is
 * what stops its hooks being registered twice.
 */
function legacyPlugins(mod) {
  const seen = new Set();
  const result = [];
  for (const [name, entry] of Object.entries(mod)) {
    if (seen.has(entry)) continue;
    seen.add(entry);
    const fn =
      typeof entry === "function"
        ? entry
        : isRecord(entry) && typeof entry.server === "function"
          ? entry.server
          : undefined;
    if (!fn) throw new TypeError("Plugin export is not a function");
    result.push({ name, fn });
  }
  return result;
}

// ---------------------------------------------------------------------------
// Config-directory tools
// ---------------------------------------------------------------------------

function isZodType(value) {
  return isRecord(value) && "_zod" in value;
}

function isPluginTool(value) {
  return (
    isRecord(value) &&
    "args" in value &&
    "description" in value &&
    "execute" in value
  );
}

function isJsonSchemaDefinition(value) {
  return (
    typeof value === "boolean" ||
    (isRecord(value) && !Array.isArray(value))
  );
}

function legacyJsonSchema(entries) {
  const properties = Object.fromEntries(
    entries.filter((entry) => isJsonSchemaDefinition(entry[1])),
  );
  return {
    type: "object",
    properties,
    required: Object.keys(properties),
  };
}

async function loadZod(entry) {
  const require = createRequire(pathToFileURL(entry));
  let target;
  try {
    target = require.resolve("zod");
  } catch (error) {
    throw new Error(
      `config tool ${entry} uses Zod arguments, but its install tree has no resolvable zod package: ${error?.message ?? String(error)}`,
    );
  }
  const mod = await import(pathToFileURL(target).href);
  const z = mod.z ?? mod.default ?? mod;
  if (typeof z.object !== "function" || typeof z.toJSONSchema !== "function") {
    throw new Error(`config tool ${entry} resolved an incompatible zod package at ${target}`);
  }
  return z;
}

function zodMetadataRegistry(z, schema) {
  const registry = z.registry();
  const seen = new WeakSet();
  const collect = (value) => {
    if (!isRecord(value) || seen.has(value)) return;
    seen.add(value);
    if (isZodType(value)) {
      const metadata = typeof value.meta === "function" ? value.meta() : undefined;
      const description =
        typeof value.description === "string" ? value.description : undefined;
      const merged = {
        ...(isRecord(metadata) ? metadata : {}),
        ...(description ? { description } : {}),
      };
      if (Object.keys(merged).length) registry.add(value, merged);
      collect(value._zod.def);
      return;
    }
    for (const item of Object.values(value)) collect(item);
  };
  collect(schema);
  return registry;
}

function normalizeZodJsonSchema(value) {
  if (Array.isArray(value)) return value.map(normalizeZodJsonSchema);
  if (!isRecord(value)) return value;
  return Object.fromEntries(
    Object.entries(value)
      .filter(([key, item]) =>
        (key === "exclusiveMaximum" || key === "exclusiveMinimum") &&
        typeof item === "boolean"
          ? false
          : true,
      )
      .map(([key, item]) => [key, normalizeZodJsonSchema(item)]),
  );
}

function zodJsonSchema(z, schema) {
  const result = normalizeZodJsonSchema(
    z.toJSONSchema(schema, {
      io: "input",
      metadata: zodMetadataRegistry(z, schema),
    }),
  );
  if (!isRecord(result)) {
    throw new Error("plugin tool Zod schema produced a non-object JSON Schema");
  }
  const { $defs, ...rest } = result;
  return isRecord($defs) ? { ...rest, definitions: $defs } : rest;
}

async function describeConfigTools(mod, entry) {
  const tools = [];
  for (const [exportID, definition] of Object.entries(mod)) {
    if (!isPluginTool(definition)) continue;
    const args = definition.args ?? {};
    const entries = Object.entries(args);
    const allZod = entries.every((entry) => isZodType(entry[1]));
    let parameters;
    let validate = (value) => value;
    if (allZod) {
      const z = await loadZod(entry);
      const schema = z.object(args);
      parameters = zodJsonSchema(z, schema);
      validate = (value) => {
        const parsed = schema.safeParse(value);
        if (parsed.success) return parsed.data;
        const error = new Error(parsed.error.message);
        error.ocToolErrorKind = "invalid_args";
        throw error;
      };
    } else {
      parameters = legacyJsonSchema(entries);
    }
    const execute = async (value, context) => {
      if (typeof definition.execute !== "function") {
        throw new TypeError(`config tool export ${exportID} has a non-function execute member`);
      }
      return definition.execute(validate(value), context);
    };
    tools.push({
      export: exportID,
      description:
        typeof definition.description === "string"
          ? definition.description
          : String(definition.description ?? ""),
      parameters,
      execute: encode(execute),
    });
  }
  return tools;
}

// ---------------------------------------------------------------------------
// Hook table
// ---------------------------------------------------------------------------

/** Exactly `HookName::ALL` from `crates/oc-plugin/src/manifest.rs`. */
const HOOK_NAMES = [
  "dispose",
  "event",
  "config",
  "tool",
  "auth",
  "provider",
  "chat.message",
  "chat.params",
  "chat.headers",
  "permission.ask",
  "command.execute.before",
  "tool.execute.before",
  "shell.env",
  "tool.execute.after",
  "experimental.chat.messages.transform",
  "experimental.chat.system.transform",
  "experimental.provider.small_model",
  "experimental.session.compacting",
  "experimental.compaction.autocontinue",
  "experimental.text.complete",
  "tool.definition",
];

/** @type {Array<Record<string, unknown>>} */
const loadedHooks = [];

/**
 * A tool definition is described shallowly, on purpose.
 *
 * `tool({ args: { query: tool.schema.string() } })` puts a zod schema in `args`,
 * and a zod schema is an object graph of ~30 methods per node. Deep-encoding
 * antigravity's single `google_search` tool retained **351** closures and did it
 * for values no caller can use. So only `execute` becomes a handle, and the
 * argument surface is reported as its key names. Full schema translation needs
 * the plugin's own `z.toJSONSchema`, which is not reachable from here; the gap
 * is recorded rather than papered over with a fabricated schema.
 */
function describeTool(id, definition) {
  if (!isRecord(definition)) return { id, description: null, args: [], execute: null };
  let args = [];
  try {
    if (isRecord(definition.args)) args = Object.keys(definition.args);
  } catch {
    args = [];
  }
  return {
    id,
    description:
      typeof definition.description === "string" ? definition.description : null,
    args,
    execute:
      typeof definition.execute === "function" ? encode(definition.execute) : null,
  };
}

function describeHooks() {
  const present = [];
  const auth = [];
  const provider = [];
  const tools = [];
  const callbacks = {};
  for (const hooks of loadedHooks) {
    if (!isRecord(hooks)) continue;
    for (const name of HOOK_NAMES) {
      const member = hooks[name];
      if (member === undefined || member === null) continue;
      if (!present.includes(name)) present.push(name);
      if (name === "auth") {
        for (const item of Array.isArray(member) ? member : [member]) {
          auth.push(encode(item));
        }
        continue;
      }
      if (name === "provider") {
        for (const item of Array.isArray(member) ? member : [member]) {
          provider.push(encode(item));
        }
        continue;
      }
      if (name === "tool") {
        for (const [id, definition] of Object.entries(member)) {
          tools.push(describeTool(id, definition));
        }
        continue;
      }
      if (typeof member === "function") {
        (callbacks[name] ??= []).push(retain(member));
      }
    }
  }
  return { hooks: present, auth, provider, tools, callbacks };
}

// ---------------------------------------------------------------------------
// Request handlers
// ---------------------------------------------------------------------------

let initialized = false;

async function handleRequest(frame) {
  const { id, method, params } = frame;
  switch (method) {
    case "init": {
      if (initialized) throw new Error("init was already performed");
      initialized = true;
      await patchReadline();
      if (params.kind === "config-tool") {
        const href = pathToFileURL(params.entry).href;
        const mod = await import(href);
        if (!mod) throw new Error(`Config tool ${params.entry} module is empty`);
        respond(id, {
          id: null,
          exports: Object.keys(mod),
          sdk: null,
          runtime: typeof Bun === "undefined" ? "node" : "bun",
          workspace: [],
          hooks: [],
          auth: [],
          provider: [],
          callbacks: {},
          tools: await describeConfigTools(mod, params.entry),
        });
        return;
      }
      const { input, sdkFrom } = await buildInput(params);
      const href = pathToFileURL(params.entry).href;
      const mod = await import(href);
      if (!mod) throw new Error(`Plugin ${params.spec} module is empty`);
      const kind = params.kind === "tui" ? "tui" : "server";
      const v1 = readV1Plugin(mod, params.spec, kind);
      const ids = [];
      const factories = [];
      if (v1) {
        if (v1.id !== undefined) ids.push(String(v1.id));
        factories.push({ name: kind, fn: kind === "tui" ? v1.tui : v1.server });
      } else {
        for (const candidate of legacyPlugins(mod)) factories.push(candidate);
      }
      const exports = [];
      for (const factory of factories) {
        const hooks = await factory.fn(input, params.options ?? undefined);
        exports.push(factory.name);
        if (isRecord(hooks)) loadedHooks.push(hooks);
      }
      respond(id, {
        id: ids[0] ?? null,
        exports,
        sdk: sdkFrom,
        runtime: typeof Bun === "undefined" ? "node" : "bun",
        workspace: workspaceRegistrations,
        ...describeHooks(),
      });
      return;
    }
    case "call": {
      const fn = handles.get(params.handle);
      if (!fn) throw new Error(`handle ${params.handle} is no longer retained`);
      const mutations = (params.args ?? []).map(() => new Set());
      const args = (params.args ?? []).map((argument, index) =>
        decodeArgument(argument, mutations[index]),
      );
      const boundaries = args.map((argument) => hostBoundaries(argument));
      const value = await fn(...args);
      const encoded = args.map((argument, index) => {
        const truncations = [];
        return {
          value: encode(
            argument,
            0,
            new Set(),
            "",
            boundaries[index],
            mutations[index],
            truncations,
          ),
          truncations,
        };
      });
      respond(id, {
        value: encode(value),
        args: encoded.map((argument) => argument.value),
        truncations: encoded.map((argument) => argument.truncations),
      });
      return;
    }
    case "tool.call": {
      const fn = handles.get(params.handle);
      if (!fn) throw new Error(`handle ${params.handle} is no longer retained`);
      const reported = { title: "", metadata: {} };
      const controller = new AbortController();
      if (params.context?.aborted) controller.abort();
      const context = {
        sessionID: String(params.context?.sessionID ?? ""),
        messageID: String(params.context?.messageID ?? ""),
        agent: String(params.context?.agent ?? ""),
        directory: String(params.context?.directory ?? ""),
        worktree: String(params.context?.worktree ?? ""),
        abort: controller.signal,
        metadata(input) {
          if (!isRecord(input)) return;
          if (typeof input.title === "string") reported.title = input.title;
          if (isRecord(input.metadata)) Object.assign(reported.metadata, input.metadata);
        },
        ask(input) {
          return request("tool.ask", {
            context: params.contextID,
            input: isRecord(input) ? input : {},
          });
        },
      };
      try {
        const value = await fn(params.args ?? {}, context);
        if (typeof value === "string") {
          respond(id, {
            result: {
              title: reported.title,
              output: value,
              metadata: reported.metadata,
              attachments: [],
            },
          });
          return;
        }
        if (!isRecord(value)) {
          throw new TypeError("config tool returned neither a string nor a result object");
        }
        respond(id, {
          result: {
            title: typeof value.title === "string" ? value.title : reported.title,
            output: typeof value.output === "string" ? value.output : String(value.output ?? ""),
            metadata: {
              ...reported.metadata,
              ...(isRecord(value.metadata) ? value.metadata : {}),
            },
            attachments: Array.isArray(value.attachments) ? value.attachments : [],
          },
        });
      } catch (error) {
        respond(id, {
          error: {
            kind:
              error?.ocToolErrorKind === "invalid_args"
                ? "invalid_args"
                : "failed",
            message: error?.message ?? String(error),
          },
        });
      }
      return;
    }
    case "release": {
      for (const handle of params.handles ?? []) handles.delete(handle);
      respond(id, null);
      return;
    }
    case "stats": {
      const usage = process.memoryUsage?.() ?? {};
      respond(id, {
        handles: handles.size,
        rss: usage.rss ?? null,
        heap_used: usage.heapUsed ?? null,
      });
      return;
    }
    case "shutdown": {
      respond(id, null);
      socket.end();
      setTimeout(() => process.exit(0), 50);
      return;
    }
    default:
      throw new Error(`unknown method \`${method}\``);
  }
}

// A host-supplied argument may itself name a handle the plugin gave us earlier
// (an auth `loader` receives a `getAuth` callable). Rebuilding it as a real
// function keeps the plugin's own call shape intact.
function mutationProxy(value, mutations, path) {
  if (!mutations) return value;
  return new Proxy(value, {
    set(target, key, member, receiver) {
      mutations.add(childPointer(path, key));
      return Reflect.set(target, key, member, receiver);
    },
    deleteProperty(target, key) {
      mutations.add(childPointer(path, key));
      return Reflect.deleteProperty(target, key);
    },
    defineProperty(target, key, descriptor) {
      mutations.add(childPointer(path, key));
      return Reflect.defineProperty(target, key, descriptor);
    },
  });
}

function decodeArgument(value, mutations, path = "") {
  if (isRecord(value) && typeof value.$hostFn === "number") {
    const target = value.$hostFn;
    return async (...args) => {
      const result = await request("host.call", { handle: target, args: args.map((a) => encode(a)) });
      return result?.value ?? null;
    };
  }
  if (Array.isArray(value)) {
    const decoded = value.map((member, index) =>
      decodeArgument(member, mutations, childPointer(path, index)),
    );
    return mutationProxy(decoded, mutations, path);
  }
  if (isRecord(value)) {
    const out = {};
    for (const [key, member] of Object.entries(value)) {
      out[key] = decodeArgument(
        member,
        mutations,
        childPointer(path, key),
      );
    }
    return mutationProxy(out, mutations, path);
  }
  return value;
}

process.on("unhandledRejection", (reason) => {
  toStderr("oc-js-shim: unhandled rejection:", String(reason));
});
process.on("uncaughtException", (error) => {
  toStderr("oc-js-shim: uncaught exception:", String(error?.stack ?? error));
});

// `createRequire` is retained so a plugin that relies on CJS resolution from its
// own directory keeps working; touching it here is what forces the reference.
void createRequire;
