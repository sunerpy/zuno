// Verbatim transcription of the oracle's command argument expansion.
//
// Source: /config/workspace/ProdDir/AI/opencode (pinned aefaf140c1, v1.18.13)
//   packages/opencode/src/session/prompt.ts:1372-1395  (the expansion body)
//   packages/opencode/src/session/prompt.ts:1594-1596  (the three regexes)
//   packages/opencode/src/command/index.ts:36-43       (hints)
//
// Nothing below is paraphrased. The only edits are dropping the `yield*`
// effect wrappers and the `!`bash`` substitution (a separate concern that
// spawns processes), and reading `cmd.template` as a plain string.

const argsRegex = /(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)/gi
const placeholderRegex = /\$(\d+)/g
const quoteTrimRegex = /^["']|["']$/g

function expand(templateCommand, inputArguments) {
  const raw = inputArguments.match(argsRegex) ?? []
  const args = raw.map((arg) => arg.replace(quoteTrimRegex, ""))

  const placeholders = templateCommand.match(placeholderRegex) ?? []
  let last = 0
  for (const item of placeholders) {
    const value = Number(item.slice(1))
    if (value > last) last = value
  }

  const withArgs = templateCommand.replaceAll(placeholderRegex, (_, index) => {
    const position = Number(index)
    const argIndex = position - 1
    if (argIndex >= args.length) return ""
    if (position === last) return args.slice(argIndex).join(" ")
    return args[argIndex]
  })
  const usesArgumentsPlaceholder = templateCommand.includes("$ARGUMENTS")
  let template = withArgs.replaceAll("$ARGUMENTS", inputArguments)

  if (placeholders.length === 0 && !usesArgumentsPlaceholder && inputArguments.trim()) {
    template = template + "\n\n" + inputArguments
  }

  template = template.trim()
  return template
}

function hints(template) {
  const result = []
  const numbered = template.match(/\$\d+/g)
  if (numbered) {
    for (const match of [...new Set(numbered)].sort()) result.push(match)
  }
  if (template.includes("$ARGUMENTS")) result.push("$ARGUMENTS")
  return result
}

function tokenize(inputArguments) {
  const raw = inputArguments.match(argsRegex) ?? []
  return raw.map((arg) => arg.replace(quoteTrimRegex, ""))
}

const cases = JSON.parse(require("fs").readFileSync(process.argv[2], "utf8"))
const out = cases.map((c) => ({
  id: c.id,
  template: c.template,
  arguments: c.arguments,
  expanded: expand(c.template, c.arguments),
  hints: hints(c.template),
  tokens: tokenize(c.arguments),
}))
process.stdout.write(JSON.stringify(out, null, 2) + "\n")
