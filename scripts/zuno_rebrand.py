#!/usr/bin/env python3
"""Replay the Zuno rebrand onto Codex text during an upstream sync.

The rules live in ``FORK_REBRAND.toml``. ``Rebrand.apply`` rewrites one text
with them; ``resolve_conflicts`` walks the ``diff3`` conflict hunks that
``git merge-tree`` produced and resolves every hunk whose Zuno side is exactly
the rebrand of its base. ``audit`` measures how much of the Zuno delta the rules
reproduce, so rule drift is visible before it turns into conflicts.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Sequence
import unicodedata


MANIFEST_NAME = "FORK_REBRAND.toml"
VERSION_PLACEHOLDER = "${version}"
CONFLICT_START = "<<<<<<< "
CONFLICT_BASE = "||||||| "
CONFLICT_SEPARATOR = "======="
CONFLICT_END = ">>>>>>> "
WORKSPACE_MANIFEST = "codex-rs/Cargo.toml"
WORKSPACE_VERSION_LINE = re.compile(r'^version = "(?P<version>[0-9]+\.[0-9]+\.[0-9]+)"$')
# Trailing padding that keeps rendered width stable: spaces at the end of the
# line, before a closing string quote (optionally followed by a comma), or
# before a box-drawing border.
PADDING_ANCHOR = re.compile(r'( +)("?,?|│)$')


class RebrandError(RuntimeError):
    """The rebrand manifest is malformed."""


@dataclass(frozen=True)
class Rule:
    pattern: re.Pattern[str]
    replace: str
    paths: tuple[str, ...]
    suffix: str = ""

    def applies_to(self, path: str | None) -> bool:
        if path is None:
            return True
        if self.suffix and not path.endswith(self.suffix):
            return False
        if not self.paths:
            return True
        return any(path == prefix.rstrip("/") or path.startswith(prefix) for prefix in self.paths)

    @property
    def needs_version(self) -> bool:
        return VERSION_PLACEHOLDER in self.replace


@dataclass(frozen=True)
class AddedScope:
    prefix: str
    suffix: str

    def matches(self, path: str) -> bool:
        return path.startswith(self.prefix) and path.endswith(self.suffix)


@dataclass(frozen=True)
class Rebrand:
    protect: tuple[str, ...]
    rules: tuple[Rule, ...]
    added: tuple[AddedScope, ...] = ()

    def applies_to_added(self, path: str) -> bool:
        """Whether a file upstream adds at ``path`` is rebranded without a predicate."""
        return any(scope.matches(path) for scope in self.added)

    @classmethod
    def load(cls, manifest: Path) -> "Rebrand":
        try:
            data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError) as error:
            raise RebrandError(f"cannot read {manifest}: {error}") from error
        if data.get("schema_version") != 1:
            raise RebrandError(f"{manifest}: unsupported schema_version {data.get('schema_version')!r}")
        protect = data.get("protect", [])
        if not isinstance(protect, list) or not all(isinstance(item, str) and item for item in protect):
            raise RebrandError(f"{manifest}: protect must be a list of non-empty strings")
        rules: list[Rule] = []
        for index, entry in enumerate(data.get("rule", [])):
            if not isinstance(entry, dict):
                raise RebrandError(f"{manifest}: rule #{index + 1} must be a table")
            find = entry.get("find")
            regex = entry.get("regex")
            replace = entry.get("replace")
            if (find is None) == (regex is None):
                raise RebrandError(f"{manifest}: rule #{index + 1} needs exactly one of find/regex")
            if not isinstance(replace, str):
                raise RebrandError(f"{manifest}: rule #{index + 1} needs a string replace")
            if find is not None:
                if not isinstance(find, str) or not find:
                    raise RebrandError(f"{manifest}: rule #{index + 1} find must be a non-empty string")
                pattern = re.compile(re.escape(find))
            else:
                if not isinstance(regex, str) or not regex:
                    raise RebrandError(f"{manifest}: rule #{index + 1} regex must be a non-empty string")
                try:
                    pattern = re.compile(regex)
                except re.error as error:
                    raise RebrandError(f"{manifest}: rule #{index + 1} regex is invalid: {error}") from error
            paths = entry.get("paths", [])
            if not isinstance(paths, list) or not all(isinstance(item, str) and item for item in paths):
                raise RebrandError(f"{manifest}: rule #{index + 1} paths must be a list of strings")
            suffix = entry.get("suffix", "")
            if not isinstance(suffix, str):
                raise RebrandError(f"{manifest}: rule #{index + 1} suffix must be a string")
            rules.append(Rule(pattern=pattern, replace=replace, paths=tuple(paths), suffix=suffix))
        if not rules:
            raise RebrandError(f"{manifest}: no rules defined")
        added: list[AddedScope] = []
        for index, entry in enumerate(data.get("added", [])):
            if not isinstance(entry, dict) or not isinstance(entry.get("prefix"), str) or not entry["prefix"]:
                raise RebrandError(f"{manifest}: added #{index + 1} needs a non-empty prefix")
            suffix = entry.get("suffix", "")
            if not isinstance(suffix, str):
                raise RebrandError(f"{manifest}: added #{index + 1} suffix must be a string")
            added.append(AddedScope(prefix=entry["prefix"], suffix=suffix))
        return cls(protect=tuple(protect), rules=tuple(rules), added=tuple(added))

    def apply(self, text: str, *, version: str | None, path: str | None) -> str:
        """Rewrite ``text`` line by line; ``text`` keeps its line endings."""
        rules = [
            rule
            for rule in self.rules
            if rule.applies_to(path) and (version is not None or not rule.needs_version)
        ]
        if not rules:
            return text
        parts = text.split("\n")
        return "\n".join(self._apply_line(line, rules, version) for line in parts)

    def _apply_line(self, line: str, rules: Sequence[Rule], version: str | None) -> str:
        if not line:
            return line
        # Frozen spans are never matched again: protected phrases and the text a
        # rule already produced. Lookarounds still see the whole line.
        frozen: list[tuple[int, int]] = []
        text = line
        for phrase in self.protect:
            start = 0
            while True:
                index = text.find(phrase, start)
                if index < 0:
                    break
                frozen.append((index, index + len(phrase)))
                start = index + len(phrase)
        for rule in rules:
            replacement_template = rule.replace
            if version is not None:
                replacement_template = replacement_template.replace(VERSION_PLACEHOLDER, version)
            matches = [
                match
                for match in rule.pattern.finditer(text)
                if match.end() > match.start()
                and not any(match.start() < end and start < match.end() for start, end in frozen)
            ]
            for match in reversed(matches):
                # Replacements are literal text: no group references, no escapes,
                # so a rule can produce `\n` inside a Rust string literal.
                replacement = replacement_template
                delta = len(replacement) - (match.end() - match.start())
                text = text[: match.start()] + replacement + text[match.end() :]
                frozen = [
                    (start + delta, end + delta) if start >= match.end() else (start, end)
                    for start, end in frozen
                ]
                frozen.append((match.start(), match.start() + len(replacement)))
        if text != line:
            text = restore_width(line, text)
        return text


def display_width(text: str) -> int:
    width = 0
    for char in text:
        if unicodedata.combining(char):
            continue
        width += 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
    return width


def restore_width(original: str, rewritten: str) -> str:
    """Re-pad ``rewritten`` so it renders as wide as ``original`` when it ends in padding."""
    delta = display_width(original) - display_width(rewritten)
    if delta == 0:
        return rewritten
    anchor = PADDING_ANCHOR.search(rewritten)
    if anchor is None:
        return rewritten
    spaces = len(anchor.group(1)) + delta
    if spaces < 1:
        return rewritten
    return rewritten[: anchor.start(1)] + " " * spaces + anchor.group(2)


@dataclass(frozen=True)
class ConflictHunk:
    ours: list[str]
    base: list[str] | None
    theirs: list[str]
    labels: tuple[str, str, str]

    def text(self, side: str) -> str:
        lines = getattr(self, side)
        return "\n".join(lines) if lines is not None else ""


@dataclass
class ResolutionReport:
    resolved: int = 0
    remaining: int = 0
    reasons: list[str] = field(default_factory=list)


def split_conflicts(text: str) -> list[str | ConflictHunk]:
    """Split a file into plain text and ``diff3`` conflict hunks (2-way hunks keep ``base=None``)."""
    segments: list[str | ConflictHunk] = []
    plain: list[str] = []
    lines = text.split("\n")
    index = 0
    while index < len(lines):
        line = lines[index]
        if not line.startswith(CONFLICT_START):
            plain.append(line)
            index += 1
            continue
        ours: list[str] = []
        base: list[str] | None = None
        theirs: list[str] = []
        labels = [line[len(CONFLICT_START) :], "", ""]
        section = "ours"
        index += 1
        closed = False
        while index < len(lines):
            current = lines[index]
            if section == "ours" and current.startswith(CONFLICT_BASE):
                base = []
                labels[1] = current[len(CONFLICT_BASE) :]
                section = "base"
            elif section in ("ours", "base") and current == CONFLICT_SEPARATOR:
                section = "theirs"
            elif section == "theirs" and current.startswith(CONFLICT_END):
                labels[2] = current[len(CONFLICT_END) :]
                closed = True
                index += 1
                break
            elif section == "ours":
                ours.append(current)
            elif section == "base":
                assert base is not None
                base.append(current)
            else:
                theirs.append(current)
            index += 1
        if not closed:
            raise RebrandError("unterminated conflict hunk")
        if plain:
            segments.append("\n".join(plain))
            plain = []
        segments.append(
            ConflictHunk(ours=ours, base=base, theirs=theirs, labels=(labels[0], labels[1], labels[2]))
        )
    if plain:
        segments.append("\n".join(plain))
    return segments


def has_conflict_markers(text: str) -> bool:
    return any(
        line.startswith(CONFLICT_START) or line.startswith(CONFLICT_END)
        for line in text.split("\n")
    )


def resolve_conflicts(
    text: str,
    rebrand: Rebrand,
    *,
    path: str,
    source_version: str | None,
    target_version: str | None,
) -> tuple[str, ResolutionReport]:
    """Resolve every hunk whose Zuno side is the rebrand of its base.

    ``ours`` is the upstream release, ``theirs`` the Zuno source (the argument
    order ``git merge-tree`` was given). A hunk is resolved to the rebrand of the
    upstream side when ``rebrand(base, source_version) == theirs``. In
    ``codex-rs/Cargo.toml`` a hunk made only of the workspace ``version`` line
    resolves to the upstream side, because Zuno versions track Codex versions.
    """
    report = ResolutionReport()
    output: list[str] = []
    for segment in split_conflicts(text):
        if isinstance(segment, str):
            output.append(segment)
            continue
        replacement = _resolve_hunk(segment, rebrand, path, source_version, target_version)
        if replacement is None:
            report.remaining += 1
            output.append(_render_hunk(segment))
            continue
        report.resolved += 1
        if replacement:
            output.append("\n".join(replacement))
        else:
            report.reasons.append("hunk deleted upstream")
    return "\n".join(output), report


def _resolve_hunk(
    hunk: ConflictHunk,
    rebrand: Rebrand,
    path: str,
    source_version: str | None,
    target_version: str | None,
) -> list[str] | None:
    """Return the resolved lines of ``hunk`` or ``None`` when it needs a human."""
    if hunk.base is None:
        return None
    if path == WORKSPACE_MANIFEST and _is_version_hunk(hunk):
        return list(hunk.ours)
    if rebrand.apply(hunk.text("base"), version=source_version, path=path) != hunk.text("theirs"):
        return None
    if not hunk.ours:
        return []
    return rebrand.apply(hunk.text("ours"), version=target_version, path=path).split("\n")


def _is_version_hunk(hunk: ConflictHunk) -> bool:
    return all(
        len(side) == 1 and WORKSPACE_VERSION_LINE.match(side[0]) is not None
        for side in (hunk.ours, hunk.base or [], hunk.theirs)
    )


def _render_hunk(hunk: ConflictHunk) -> str:
    lines = [f"{CONFLICT_START}{hunk.labels[0]}", *hunk.ours]
    if hunk.base is not None:
        lines += [f"{CONFLICT_BASE}{hunk.labels[1]}", *hunk.base]
    lines += [CONFLICT_SEPARATOR, *hunk.theirs, f"{CONFLICT_END}{hunk.labels[2]}"]
    return "\n".join(lines)


def _git_blob(repo: Path, revision: str, path: str) -> str | None:
    result = subprocess.run(
        ["git", "-C", str(repo), "show", f"{revision}:{path}"],
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    try:
        return result.stdout.decode("utf-8")
    except UnicodeDecodeError:
        return None


def audit(repo: Path, manifest: Path, baseline: str, source: str, version: str | None) -> dict[str, object]:
    rebrand = Rebrand.load(manifest)
    changed = subprocess.run(
        ["git", "-C", str(repo), "diff", "--name-only", "--no-renames", baseline, source],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split("\n")
    exact: list[str] = []
    diverged: list[dict[str, object]] = []
    added: list[str] = []
    removed: list[str] = []
    binary: list[str] = []
    for path in sorted(filter(None, changed)):
        base = _git_blob(repo, baseline, path)
        current = _git_blob(repo, source, path)
        if base is None and current is None:
            binary.append(path)
        elif base is None:
            added.append(path)
        elif current is None:
            removed.append(path)
        else:
            replayed = rebrand.apply(base, version=version, path=path)
            if replayed == current:
                exact.append(path)
            else:
                replayed_lines = replayed.split("\n")
                current_lines = current.split("\n")
                if len(replayed_lines) == len(current_lines):
                    differing = sum(1 for a, b in zip(replayed_lines, current_lines) if a != b)
                else:
                    differing = abs(len(replayed_lines) - len(current_lines)) + len(replayed_lines)
                diverged.append({"path": path, "differing_lines": differing})
    diverged.sort(key=lambda item: (item["differing_lines"], item["path"]))
    return {
        "baseline": baseline,
        "source": source,
        "version": version,
        "modified": len(exact) + len(diverged),
        "reproduced": len(exact),
        "diverged": diverged,
        "added": added,
        "removed": removed,
        "binary": binary,
    }


def parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", type=Path, default=Path.cwd(), help="Zuno repository root")
    parser.add_argument("--manifest", type=Path, default=None, help=f"rule file (default: <repo>/{MANIFEST_NAME})")
    subparsers = parser.add_subparsers(dest="command", required=True)
    apply_parser = subparsers.add_parser("apply", help="rewrite a file in place (or stdin to stdout)")
    apply_parser.add_argument("path", help="repository-relative path used for path-scoped rules")
    apply_parser.add_argument("--version", help="workspace version substituted for ${version}")
    apply_parser.add_argument("--stdin", action="store_true", help="read stdin and write stdout instead of editing the file")
    audit_parser = subparsers.add_parser("audit", help="report which Zuno-changed files the rules reproduce")
    audit_parser.add_argument("--baseline", required=True, help="Codex baseline revision (for example rust-v0.154.0)")
    audit_parser.add_argument("--source", default="HEAD", help="Zuno revision to compare (default HEAD)")
    audit_parser.add_argument("--version", help="workspace version of --source substituted for ${version}")
    audit_parser.add_argument("--json", action="store_true", help="machine-readable output")
    audit_parser.add_argument("--list-diverged", action="store_true", help="print every diverged path")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    repo: Path = args.repo.resolve()
    manifest: Path = args.manifest or repo / MANIFEST_NAME
    try:
        if args.command == "apply":
            rebrand = Rebrand.load(manifest)
            if args.stdin:
                sys.stdout.write(rebrand.apply(sys.stdin.read(), version=args.version, path=args.path))
                return 0
            target = repo / args.path
            text = target.read_text(encoding="utf-8")
            target.write_text(rebrand.apply(text, version=args.version, path=args.path), encoding="utf-8")
            return 0
        report = audit(repo, manifest, args.baseline, args.source, args.version)
    except (RebrandError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(report, indent=2))
        return 0
    modified = report["modified"]
    reproduced = report["reproduced"]
    percent = (100 * reproduced // modified) if modified else 100
    print(
        f"{reproduced}/{modified} modified files ({percent}%) are reproduced by {manifest.name}; "
        f"{len(report['added'])} added, {len(report['removed'])} removed, {len(report['binary'])} binary"
    )
    if args.list_diverged:
        for item in report["diverged"]:
            print(f"  {item['differing_lines']:>5}  {item['path']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
