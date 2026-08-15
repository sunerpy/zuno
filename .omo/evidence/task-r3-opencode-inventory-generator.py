from __future__ import annotations

import hashlib
import re
import sys
from collections import Counter
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
OUTPUT = Path(__file__).with_name("task-r3-opencode-inventory.tsv")
PLUGIN_PATHS = (
    "crates/oc-plugin/src/",
    "crates/oc-plugin-sdk/src/",
    "crates/oc-server/src/compat_v1.rs",
    "crates/oc-cli/src/version.rs",
    "crates/oc-cli/src/environment.rs",
)
PLUGIN_TOKENS = (
    "engines.opencode",
    "@opencode-ai/plugin",
    "@opencode-ai/sdk",
    "opencode-antigravity",
    "opencode-kiro",
    "customize-opencode",
    "compatible-opencode",
    "Plugin requires opencode",
    "plugin_abi_names_keep_only_their_opencode_spelling",
)
UPSTREAM_ARTIFACT_PATHS = (
    "crates/oc-catalog/src/skill/customize-opencode.md",
    "crates/oc-testkit/src/",
    "crates/oc-tui/src/snapshots/oc_tui__theme__tests__opencode.snap",
)
UPSTREAM_REFERENCE_TOKENS = (
    "packages/opencode",
    "github.com/anomalyco/opencode",
    "oracle",
    "Oracle",
    "upstream",
    "Upstream",
    "TypeScript",
    "typescript",
    "against opencode",
    "from opencode",
    "in opencode",
    "inside opencode",
    "real opencode",
    "installed opencode",
    "released opencode",
    "opencode 1.",
    "opencode v1.",
    "opencode's",
)
UPSTREAM_IDENTIFIER_TOKENS = (
    ".opencode",
    "/opencode",
    "opencode/",
    "opencode\\",
    "opencode-",
    "opencode_",
    "_opencode",
    "opencode.",
    "@opencode",
    "`opencode",
    '"opencode',
    "OPENCODE_",
    "OpenCode",
)
HISTORICAL_TOKENS = (
    ".omo/",
    "/tmp/opencode",
    "/config/.config/opencode",
    "sunerpy/opencode-rust",
    "opencode-rust",
)
STALE_IDENTITY = ("opencode Rust API", "opencode server listening")

# --- zuno-owned-stale-identity contract ----------------------------------------
#
# `zuno-owned-stale-identity` means: Zuno's OWN identity is still spelled with
# the upstream name. It is decided by what a line DOES with the string, never by
# which file the line sits in. Keying on a filename or a hand-copied substring is
# incidental — such a rule can only ever re-report occurrences somebody already
# found, it cannot discover a new one, and it goes silent when the named lines
# are merely reworded, which is indistinguishable from the defect being fixed.
#
#   Clause M — migration machinery, NOT stale identity. The legacy name is
#       present so Zuno can detect unmigrated state and refuse, naming both paths.
#       The vocabulary is load-bearing: a legacy-path diagnostic cannot be written
#       without saying which path is legacy and what to do about it. Such an
#       occurrence is a pre-rename citation, which is what `historical-citation`
#       already covers. Flagging it would invite a reader to "clean it up" and
#       reintroduce the silent overwrite of a document the user cannot see is
#       being ignored. Ranked below `plugin-abi`, because `engines.opencode`
#       contains the legacy directory name as a substring: the plugin ABI is a
#       live contract with someone else's code and outranks every other reading.
#
#   Clause A — Zuno WRITES the old name as its own project directory: a
#       project-directory-family constant bound to the legacy literal, where the
#       constant does not declare itself legacy. Every consumer of Zuno's project
#       directory reaches it through the single `oc-paths::PROJECT_DIRECTORY`
#       definition, so guarding the definition guards every consumer; a stale
#       write can only reappear by re-introducing a local copy of that constant,
#       which this clause matches in any file.
#
#   Clause B — Zuno CLAIMS to be opencode: an unretracted drop-in or
#       compatibility promise. Judged over the surrounding sentence, not the
#       physical line, because prose wraps and the retraction lands elsewhere: in
#       `crates/oc-testkit/src/lib.rs` the claim phrases sit on lines 4 and 6
#       while "does **not**" and "neither ... nor" sit on lines 3 and 5. A
#       line-scoped negation guard reports that corrected text as a defect.
#
# Not claimed: free prose asserting Zuno's own state lives under `.opencode`
# without either migration vocabulary or a compatibility promise. That is a
# doc-vs-code divergence rather than identity staleness, and no clause here
# pretends to catch it.
#
# A zero count is only evidence when paired with the injection proof recorded in
# `.omo/notepads/opencode-rust/learnings.md`: a rule that fires on nothing looks
# identical to a rule that cannot fire.
LEGACY_PROJECT_DIRECTORY = ".opencode"

# Every category is reported even at zero: an absent line reads as "this class was
# never considered", which is the reading a resolved class must not invite.
CATEGORIES = (
    "historical-citation",
    "plugin-abi",
    "unclassified",
    "upstream-artifact-reference",
    "zuno-owned-stale-identity",
)

# Each entry must be unusable in a sentence that endorses the old path.
MIGRATION_VOCABULARY = (
    "LEGACY_",
    "legacy",
    "Legacy",
    "unmigrated",
    "pre-rename",
    "predates",
    "does not read",
    "neither reads",
    "move it with",
    "move the file",
    "hard cut",
)

# A project-directory-family constant bound to the legacy literal. `name` is
# captured so a constant that declares itself legacy can be exempted.
PROJECT_DIRECTORY_DEFINITION = re.compile(
    r"(?:const|static)\s+(?P<name>[A-Z0-9_]+)\s*:\s*&(?:'static\s+)?str\s*=\s*"
    rf'"{re.escape(LEGACY_PROJECT_DIRECTORY)}"\s*;'
)

# Affirmative compatibility promises, and the negations that retract them.
COMPATIBILITY_CLAIM = (
    "drop-in replacement",
    "drop-in for opencode",
    "fully compatible with opencode",
    "reads the old paths",
    "imports opencode sessions",
    "imports opencode state",
)
CLAIM_NEGATION = ("not", "n't", "no longer", "never", "neither", "nor", "gone")
COMMENT_PREFIX = re.compile(r"^\s*(?://+[!/]?|#|\*)\s?")
SENTENCE_SPLIT = re.compile(r"(?<=[.;:])\s")


def sentence_around(lines: list[str], index: int, needle: str) -> str:
    """The sentence containing `needle`, reassembled across wrapped comment lines.

    A compatibility promise is a sentence-level assertion, so its negation is
    routinely on a different physical line than the token that matched. The
    window is deliberately small; splitting on punctuation-then-space leaves
    `.opencode` and `1.18.12` intact because neither has a space after the dot.
    """
    window = lines[max(0, index - 3) : index + 2]
    joined = " ".join(COMMENT_PREFIX.sub("", entry).strip() for entry in window)
    for sentence in SENTENCE_SPLIT.split(joined):
        if needle in sentence:
            return sentence
    return joined


def zuno_owned_stale_identity(
    line: str, lines: list[str], index: int
) -> tuple[str, str] | None:
    """Classify an occurrence as Zuno's own stale identity, or decline."""
    if any(token in line for token in MIGRATION_VOCABULARY):
        return None

    definition = PROJECT_DIRECTORY_DEFINITION.search(line)
    if definition and "LEGACY" not in definition.group("name"):
        return (
            "zuno-owned-stale-identity",
            f"`{definition.group('name')}` binds Zuno's own project directory to the "
            "pre-rename name",
        )

    for claim in COMPATIBILITY_CLAIM:
        if claim not in line:
            continue
        sentence = sentence_around(lines, index, claim)
        if any(negation in sentence for negation in CLAIM_NEGATION):
            continue
        return (
            "zuno-owned-stale-identity",
            f"unretracted compatibility promise (`{claim}`) presents Zuno as opencode",
        )

    return None


def migration_machinery(line: str) -> tuple[str, str] | None:
    """Legacy name present only to detect or instruct migration away from it."""
    if LEGACY_PROJECT_DIRECTORY not in line:
        return None
    if not any(token in line for token in MIGRATION_VOCABULARY):
        return None
    return (
        "historical-citation",
        "pre-rename path named only to detect or instruct migration away from it",
    )
EXPLICIT_UPSTREAM_IDENTIFIERS = {
    "crates/oc-config/src/schema.rs": ("parsed opencode config file",),
    "crates/oc-llm/src/catalog.rs": ("let opencode: CatalogProvider",),
    "crates/oc-mcp/src/stdio.rs": ("without opencode namespacing",),
    "crates/oc-paths/src/env.rs": ("opencode debug paths",),
    "crates/oc-paths/src/node_path.rs": ("opencode debug paths",),
    "crates/oc-search/src/embedded.rs": ("opencode debug rg files",),
    "crates/oc-tools/src/session_search.rs": (
        "opencode database file",
        "opencode database path",
    ),
    "crates/oc-tui/src/attention_tests.rs": (";notify;opencode;Session done",),
}


def classify(path: str, line: str, lines: list[str], index: int) -> tuple[str, str]:
    if any(token in line for token in STALE_IDENTITY):
        raise RuntimeError(f"unfixed Zuno presentation identity in {path}: {line.strip()}")
    if verdict := zuno_owned_stale_identity(line, lines, index):
        return verdict
    if path.startswith(PLUGIN_PATHS) or any(token in line for token in PLUGIN_TOKENS):
        return "plugin-abi", "retained plugin package, SDK, host, or semver contract"
    if verdict := migration_machinery(line):
        return verdict
    if any(token in line for token in HISTORICAL_TOKENS):
        return "historical-citation", "immutable plan, evidence, checkout, or pre-rename citation"
    if any(token in line for token in EXPLICIT_UPSTREAM_IDENTIFIERS.get(path, ())):
        return "upstream-artifact-reference", "explicit upstream command, config, protocol, provider, database, or fixture identifier"
    if path.startswith(UPSTREAM_ARTIFACT_PATHS):
        return "upstream-artifact-reference", "upstream oracle harness, bundled skill, or captured snapshot"
    if any(token in line for token in UPSTREAM_REFERENCE_TOKENS):
        return "upstream-artifact-reference", "explicit upstream implementation or oracle reference"
    if any(token in line for token in UPSTREAM_IDENTIFIER_TOKENS):
        return "upstream-artifact-reference", "retained upstream wire, path, provider, fixture, or persisted identifier"
    return "unclassified", "no explicit classification rule matched"


def main() -> int:
    source_files = sorted(path for path in (ROOT / "crates").glob("*/src/**/*") if path.is_file())
    digest = hashlib.sha256()
    rows: list[tuple[str, int, int, str, str, str]] = []
    counts: Counter[str] = Counter()

    for source_file in source_files:
        relative = source_file.relative_to(ROOT).as_posix()
        data = source_file.read_bytes()
        digest.update(relative.encode())
        digest.update(b"\0")
        digest.update(data)
        file_lines = data.decode(errors="replace").splitlines()
        for line_number, line in enumerate(file_lines, 1):
            start = 0
            while (column := line.find("opencode", start)) != -1:
                category, reason = classify(relative, line, file_lines, line_number - 1)
                counts[category] += 1
                rows.append(
                    (
                        relative,
                        line_number,
                        column + 1,
                        category,
                        reason,
                        line.replace("\t", "\\t"),
                    )
                )
                start = column + len("opencode")

    unclassified = counts["unclassified"]
    classified = len(rows) - unclassified
    lines = [
        "# Generated by .omo/evidence/task-r3-opencode-inventory-generator.py",
        "# Scope: case-sensitive lowercase `opencode` in crates/*/src/**/*",
        f"# Source snapshot SHA-256: {digest.hexdigest()}",
        f"# Total occurrences: {len(rows)}",
        f"# Classified occurrences: {classified}",
        f"# Unclassified occurrences: {unclassified}",
    ]
    lines.extend(
        f"# {category}: {counts[category]}"
        for category in sorted(set(CATEGORIES) | set(counts))
    )
    lines.append("path\tline\tcolumn\tclass\treason\tsource")
    lines.extend("\t".join(map(str, row)) for row in rows)
    OUTPUT.write_text("\n".join(lines) + "\n")
    print(
        f"wrote {OUTPUT.relative_to(ROOT)}: {len(rows)} occurrences; "
        + ", ".join(f"{category}={count}" for category, count in sorted(counts.items()))
    )
    if unclassified:
        print(f"error: {unclassified} occurrence(s) remain unclassified", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
