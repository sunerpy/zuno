import { readFile, readdir, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const lock = JSON.parse(await readFile(resolve(root, "package-lock.json"), "utf8"));
const notices = ["Zuno enterprise Web — bundled third-party notices\n"];
for (const [path, entry] of Object.entries(lock.packages).sort(([a], [b]) => a.localeCompare(b))) {
  if (!path.startsWith("node_modules/") || entry.dev || entry.link) continue;
  const directory = resolve(root, path);
  const files = (await readdir(directory)).filter((file) => /^licen[cs]e(?:[.-].*)?$/i.test(file));
  notices.push(`\n${path.replace(/^node_modules\//, "")} ${entry.version ?? ""}\nLicense: ${entry.license ?? "see package notice"}\n`);
  for (const file of files.sort()) notices.push(await readFile(resolve(directory, file), "utf8"));
}
await writeFile(resolve(root, "dist/assets/licenses.txt"), notices.join("\n"));
