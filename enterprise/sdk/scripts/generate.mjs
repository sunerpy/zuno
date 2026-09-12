import { execFileSync } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import { compile } from "json-schema-to-typescript";
import Ajv from "ajv/dist/2020.js";
import standalone from "ajv/dist/standalone/index.js";
import { build } from "esbuild";

const root = fileURLToPath(new URL("../../..", import.meta.url));
const directory = fileURLToPath(new URL("../src/generated", import.meta.url));
const checking = process.argv.includes("--check");
const source = execFileSync(
  "cargo", ["run", "--quiet", "-p", "zuno-types", "--example", "activity_schema"],
  { cwd: root, encoding: "utf8", maxBuffer: 16 * 1024 * 1024 },
);
const schema = JSON.parse(source);
const id = "zuno-activity-v1";
schema.$id = id;
const types = await compile(schema, "ActivityProtocol", {
  bannerComment: "/* Generated from zuno-types/activity.rs. Do not edit. */",
  unknownAny: true,
  additionalProperties: false,
});
const ajv = new Ajv({ strict: true, code: { source: true, esm: true }, allErrors: false });
const validationSchema = structuredClone(schema);
function portableFormats(value) {
  if (!value || typeof value !== "object") return;
  // Schemars emits a Rust numeric annotation. Express its constraint in portable
  // JSON Schema instead of disabling unknown-format validation globally.
  if (value.format === "uint32") {
    if (value.type !== "integer") throw new Error("uint32 must be an integer");
    value.minimum = Math.max(value.minimum ?? 0, 0);
    value.maximum = Math.min(value.maximum ?? 4294967295, 4294967295);
    delete value.format;
  }
  for (const child of Object.values(value)) portableFormats(child);
}
portableFormats(validationSchema);
ajv.addSchema(validationSchema, id);
const roots = ["HistoryPage", "FramePage", "CommittedFrame", "LiveFrame"];
const exports = Object.fromEntries(roots.map((name) => [`validate${name}`, `${id}#/$defs/${name}`]));
const validators = standalone(ajv, exports);
// Bundle generated helpers at build time. Browsers need no eval, runtime schema
// compiler, Node globals, or dynamic imports to validate a frame.
const bundle = await build({
  stdin: { contents: validators, resolveDir: resolve(directory, "../.."), sourcefile: "validators.mjs", loader: "js" },
  bundle: true, format: "esm", platform: "browser", target: "es2022",
  minify: true, write: false, legalComments: "inline",
});
await mkdir(directory, { recursive: true });
for (const [name, content] of [
  ["activity.schema.json", `${JSON.stringify(schema, null, 2)}\n`],
  ["activity.ts", types],
  ["validators.mjs", `// Generated from the Rust client schema. Do not edit.\n${bundle.outputFiles[0].text}`],
]) {
  const path = resolve(directory, name);
  if (checking) {
    if (await readFile(path, "utf8") !== content) throw new Error(`${name} is out of date`);
  } else {
    await writeFile(path, content);
  }
}
