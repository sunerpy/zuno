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
import difflib
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

    def apply(
        self,
        text: str,
        *,
        version: str | None,
        path: str | None,
        text_only: bool = False,
    ) -> str:
        """Rewrite ``text`` line by line; ``text`` keeps its line endings.

        With ``text_only`` a source file (see ``CODE_SUFFIXES``) is rewritten only
        inside string literals and line comments, so an identifier such as a bare
        ``Codex`` type is never renamed. Other files are unaffected by the flag.
        """
        rules = [
            rule
            for rule in self.rules
            if rule.applies_to(path) and (version is not None or not rule.needs_version)
        ]
        if not rules:
            return text
        guard = text_only and path is not None and path.endswith(CODE_SUFFIXES)
        parts = text.split("\n")
        if not guard:
            return "\n".join(self._apply_line(line, rules, version) for line in parts)
        # String literals and block comments can span lines, so the lexer state
        # carries from one line to the next.
        state = RustLexState()
        rewritten: list[str] = []
        for line in parts:
            spans, state = rust_text_spans_from(line, state)
            rewritten.append(self._apply_line(line, rules, version, spans))
        return "\n".join(rewritten)

    def apply_to_lines(
        self,
        text: str,
        selected: set[int],
        *,
        version: str | None,
        path: str,
    ) -> str:
        """Rewrite only the lines of ``text`` whose indices are in ``selected``.

        In a source file (``CODE_SUFFIXES``) those lines are rewritten inside
        string literals only (not code, not comments), with the lexer state
        carried across every line, so a selected line inside a multi-line
        literal counts as text."""
        rules = [
            rule
            for rule in self.rules
            if rule.applies_to(path) and (version is not None or not rule.needs_version)
        ]
        if not rules or not selected:
            return text
        code = path.endswith(CODE_SUFFIXES)
        state = RustLexState()
        rewritten: list[str] = []
        for index, line in enumerate(text.split("\n")):
            spans: list[tuple[int, int]] | None = None
            if code:
                spans, state = rust_text_spans_from(line, state)
                # A trailing `//` comment is a text span too; leave it alone.
                spans = [(start, end) for start, end in spans if not line.startswith("//", start)]
            rewritten.append(self._apply_line(line, rules, version, spans) if index in selected else line)
        return "\n".join(rewritten)

    def _apply_line(
        self,
        line: str,
        rules: Sequence[Rule],
        version: str | None,
        allowed: list[tuple[int, int]] | None = None,
    ) -> str:
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
                and (
                    allowed is None
                    or any(start <= match.start() and match.end() <= end for start, end in allowed)
                )
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
                if allowed is not None:
                    # The span that contains the match grows or shrinks with it;
                    # spans after the match move as a whole.
                    allowed = [
                        (
                            start + delta if start >= match.end() else start,
                            end + delta if end >= match.end() else end,
                        )
                        for start, end in allowed
                    ]
        if text != line:
            text = restore_width(line, text)
        return text


# Source files whose new upstream lines are rebranded in text positions only.
CODE_SUFFIXES = (".rs",)


RAW_STRING_START = re.compile(r'(?:b|c)?r(#*)"')


@dataclass(frozen=True)
class RustLexState:
    """Lexer state carried across lines: the terminator of the string literal
    the line starts inside (``"`` or ``"#…`` for raw literals), and how deep
    inside nested block comments it starts."""

    string_terminator: str | None = None
    raw_string: bool = False
    block_comment_depth: int = 0


def rust_text_spans(line: str) -> list[tuple[int, int]]:
    """Spans of a single Rust source line that are text, lexed from a fresh state.

    See ``rust_text_spans_from`` for the rules; this form treats the line as if
    it started outside any string or comment."""
    return rust_text_spans_from(line, RustLexState())[0]


def rust_text_spans_from(line: str, state: RustLexState) -> tuple[list[tuple[int, int]], RustLexState]:
    """Spans of a Rust source line that are text, and the state the next line
    starts in.

    Text is string literal contents and a trailing ``//`` comment. Everything
    else is code and is never rebranded when the rules run in ``text_only`` mode;
    block comments count as code too, but their quotes open no string. Ordinary
    literals honour backslash escapes; raw literals (``r"..."``, ``r#"..."#``,
    ``br"..."``) do not and end at a quote followed by the same number of ``#``.
    A literal left open at the end of the line continues on the next one, so
    the lines of a multi-line literal (an inline insta snapshot ``@"…"``) are
    text as well."""
    spans: list[tuple[int, int]] = []
    length = len(line)
    index = 0
    terminator = state.string_terminator
    raw = state.raw_string
    depth = state.block_comment_depth
    start = 0
    while index < length:
        if depth:
            if line.startswith("*/", index):
                depth -= 1
                index += 2
            elif line.startswith("/*", index):
                depth += 1
                index += 2
            else:
                index += 1
            continue
        char = line[index]
        if terminator is not None:
            if raw:
                if line.startswith(terminator, index):
                    spans.append((start, index))
                    index += len(terminator)
                    terminator = None
                    raw = False
                    continue
                index += 1
                continue
            if char == "\\":
                index += 2
                continue
            if char == '"':
                spans.append((start, index))
                terminator = None
            index += 1
            continue
        raw_start = RAW_STRING_START.match(line, index)
        if raw_start is not None and (index == 0 or not (line[index - 1].isalnum() or line[index - 1] == "_")):
            terminator = '"' + raw_start.group(1)
            raw = True
            start = raw_start.end()
            index = raw_start.end()
            continue
        if char == '"':
            terminator = '"'
            raw = False
            start = index + 1
            index += 1
            continue
        if char == "'":
            # Char literals (`'"'`, `'\''`) must not toggle string state;
            # lifetimes (`'a`) do not match either shape.
            if index + 2 < length and line[index + 1] != "\\" and line[index + 2] == "'":
                index += 3
                continue
            if index + 3 < length and line[index + 1] == "\\" and line[index + 3] == "'":
                index += 4
                continue
        if char == "/" and line.startswith("//", index):
            spans.append((index, length))
            return spans, RustLexState()
        if char == "/" and line.startswith("/*", index):
            depth = 1
            index += 2
            continue
        index += 1
    if terminator is not None:
        spans.append((start, length))
    return spans, RustLexState(string_terminator=terminator, raw_string=raw, block_comment_depth=depth)


def rebrand_new_text(
    rebrand: Rebrand,
    base_text: str,
    upstream_text: str,
    *,
    version: str | None,
    path: str,
) -> tuple[str, bool]:
    """Rebrand ``upstream_text`` after its base passed the predicate.

    Lines that already existed in ``base_text`` were proven rename-only by the
    predicate and are rebranded in full. In a source file (``CODE_SUFFIXES``) the
    lines upstream added are rebranded in text positions only, so a new bare
    ``Codex`` identifier is left alone instead of being rewritten into code that
    may not compile where the PR gate does not build. Returns the text and
    whether that guard changed anything."""
    expected = rebrand.apply(upstream_text, version=version, path=path)
    if not path.endswith(CODE_SUFFIXES):
        return expected, False
    guarded = rebrand.apply(upstream_text, version=version, path=path, text_only=True)
    if guarded == expected:
        return expected, False
    base_lines = set(base_text.split("\n"))
    result: list[str] = []
    fired = False
    for original, full, safe in zip(
        upstream_text.split("\n"), expected.split("\n"), guarded.split("\n"), strict=True
    ):
        if full == safe or original in base_lines:
            result.append(full)
            continue
        result.append(safe)
        fired = True
    return "\n".join(result), fired


def upstream_new_lines(current: str, base: str | None, upstream: str, zuno: str | None) -> set[int]:
    """Indices of the lines of ``current`` that only the upstream release wrote.

    A line is new where upstream inserted or rewrote it relative to the Codex
    baseline (by position, so a new copy of a line the baseline already had
    counts), and it survives into ``current`` unchanged. Lines the Zuno source
    also has are left out: Zuno already decided on them. These are the lines no
    rule has been applied to yet, wherever the merge put them (a cleanly merged
    region of a conflicted file included)."""
    base_lines = base.split("\n") if base is not None else []
    upstream_lines = upstream.split("\n")
    current_lines = current.split("\n")
    zuno_lines = set(zuno.split("\n")) if zuno is not None else set()
    written: set[int] = set()
    matcher = difflib.SequenceMatcher(a=base_lines, b=upstream_lines, autojunk=False)
    for tag, _, _, upstream_start, upstream_end in matcher.get_opcodes():
        if tag in ("insert", "replace"):
            written.update(range(upstream_start, upstream_end))
    selected: set[int] = set()
    matcher = difflib.SequenceMatcher(a=upstream_lines, b=current_lines, autojunk=False)
    for tag, upstream_start, upstream_end, current_start, _ in matcher.get_opcodes():
        if tag != "equal":
            continue
        for offset in range(upstream_end - upstream_start):
            index = current_start + offset
            if upstream_start + offset in written and current_lines[index] not in zuno_lines:
                selected.add(index)
    return selected


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
    # Hunks proven rename-only (or the workspace version line).
    resolved: int = 0
    # Hunks that were not rename-only but had one mechanical answer: edits that
    # only touch different lines once the rebrand is normalised, insertions on
    # both sides at one point, or snapshot metadata (see ``merge_lines``).
    merged: int = 0
    remaining: int = 0
    # Resolved hunks in which a new upstream code line kept an identifier the
    # rules would otherwise have renamed (see ``rebrand_new_text``).
    guarded: int = 0
    # Upstream lines a structural merge rebranded and kept: unlike a rename-only
    # hunk, no predicate vouches for them (Zuno did not write them either).
    rewritten: list[str] = field(default_factory=list)
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
    """Resolve every hunk whose Zuno side is the rebrand of its base, and every
    hunk that has one mechanical answer.

    ``ours`` is the upstream release, ``theirs`` the Zuno source (the argument
    order ``git merge-tree`` was given). A hunk is resolved to the rebrand of the
    upstream side when ``rebrand(base, source_version) == theirs``. In
    ``codex-rs/Cargo.toml`` a hunk made only of the workspace ``version`` line
    resolves to the upstream side, because Zuno versions track Codex versions.
    Any other hunk is merged line by line once the Zuno renames are normalised
    (``merge_lines``); it stays a conflict when the two sides edit the same
    lines.
    """
    report = ResolutionReport()
    output: list[str] = []
    for segment in split_conflicts(text):
        if isinstance(segment, str):
            output.append(segment)
            continue
        resolution = _resolve_hunk(segment, rebrand, path, source_version, target_version)
        if resolution is None:
            report.remaining += 1
            output.append(_render_hunk(segment))
            continue
        replacement, guarded, structural, rewritten = resolution
        if structural is None:
            report.resolved += 1
        else:
            report.merged += 1
            report.reasons.append(structural)
            report.rewritten.extend(rewritten)
        if guarded:
            report.guarded += 1
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
) -> tuple[list[str], bool, str | None, list[str]] | None:
    """Return the resolved lines of ``hunk``, whether the code guard fired, the
    structural rule that resolved it (``None`` for a rename-only hunk) and the
    upstream lines that rule rebranded, or ``None`` when it needs a human."""
    if hunk.base is None:
        return None
    if path == WORKSPACE_MANIFEST and _is_version_hunk(hunk):
        return list(hunk.ours), False, None, []
    if rebrand.apply(hunk.text("base"), version=source_version, path=path) == hunk.text("theirs"):
        if not hunk.ours:
            return [], False, None, []
        resolved, guarded = rebrand_new_text(
            rebrand, hunk.text("base"), hunk.text("ours"), version=target_version, path=path
        )
        return resolved.split("\n"), guarded, None, []
    if path.endswith(SNAPSHOT_SUFFIX) and _is_snapshot_metadata_hunk(hunk):
        # insta never compares `assertion_line`; upstream's value is the current one.
        return list(hunk.ours), False, "snapshot metadata", []
    base, upstream, guarded, rewritten = _normalised_sides(
        hunk, rebrand, path, source_version, target_version
    )
    merged = merge_lines(base, upstream, list(hunk.theirs))
    if merged is None:
        return None
    lines, kind = merged
    # A rewrite Zuno wrote itself on its side of the hunk is vouched for.
    kept = [line for line in rewritten if line in lines and line not in hunk.theirs]
    return lines, guarded, kind, kept


SNAPSHOT_SUFFIX = ".snap"
SNAPSHOT_ASSERTION_LINE = re.compile(r"^assertion_line: [0-9]+$")


def _is_snapshot_metadata_hunk(hunk: ConflictHunk) -> bool:
    return all(
        SNAPSHOT_ASSERTION_LINE.match(line) is not None
        for side in (hunk.ours, hunk.base or [], hunk.theirs)
        for line in side
    )


def _normalised_sides(
    hunk: ConflictHunk,
    rebrand: Rebrand,
    path: str,
    source_version: str | None,
    target_version: str | None,
) -> tuple[list[str], list[str], bool, list[str]]:
    """The base and upstream sides of ``hunk`` with the Zuno renames applied.

    A base line is replaced by its rebrand only where Zuno carries exactly that
    rebrand, so a line Zuno deliberately kept (a ``Codex`` identifier) is not
    mistaken for a Zuno edit. Upstream lines equal to a base line take that
    line's normalised form; lines upstream changed or added are rebranded the
    way ``rebrand_new_text`` treats new lines (text positions only in source
    files). Also returns whether that code guard fired and the upstream lines
    the rules changed."""
    base = list(hunk.base or [])
    zuno = list(hunk.theirs)
    upstream = list(hunk.ours)
    normalised = list(base)
    matcher = difflib.SequenceMatcher(a=base, b=zuno, autojunk=False)
    for tag, base_start, base_end, zuno_start, zuno_end in matcher.get_opcodes():
        if tag != "replace":
            continue
        candidates = zuno[zuno_start:zuno_end]
        cursor = 0
        for index in range(base_start, base_end):
            rebranded = rebrand.apply(base[index], version=source_version, path=path)
            if rebranded == base[index]:
                continue
            # Zuno lines are matched in order, so a rename is never paired
            # with a Zuno line that comes before an earlier pairing.
            for offset in range(cursor, len(candidates)):
                if candidates[offset] == rebranded:
                    normalised[index] = rebranded
                    cursor = offset + 1
                    break
    result: list[str] = []
    rewritten: list[str] = []
    guarded = False
    matcher = difflib.SequenceMatcher(a=base, b=upstream, autojunk=False)
    for tag, base_start, base_end, upstream_start, upstream_end in matcher.get_opcodes():
        if tag == "equal":
            result.extend(normalised[base_start:base_end])
            continue
        added = upstream[upstream_start:upstream_end]
        if not added:
            continue
        text = "\n".join(added)
        full = rebrand.apply(text, version=target_version, path=path)
        if path.endswith(CODE_SUFFIXES):
            safe = rebrand.apply(text, version=target_version, path=path, text_only=True)
            guarded = guarded or safe != full
            full = safe
        lines = full.split("\n")
        rewritten += [new for old, new in zip(added, lines, strict=True) if old != new]
        result.extend(lines)
    return normalised, result, guarded, rewritten


# Lines that carry no content of their own: two insertions that share only
# these still count as disjoint.
STRUCTURAL_LINE = re.compile(r"^[\s{}()\[\],;]*$")


def merge_lines(
    base: list[str], upstream: list[str], zuno: list[str]
) -> tuple[list[str], str] | None:
    """Three-way merge of one conflict hunk at line granularity.

    Unlike git, edits on adjacent lines merge: after the Zuno renames are
    normalised, the conflict hunks git reports are mostly a Zuno edit next to an
    upstream edit. Two insertions at the same point are kept in upstream-then-Zuno
    order when they share no content line (``STRUCTURAL_LINE`` lines aside), or
    once when identical. Returns ``None`` when the sides change the same line,
    when one side inserts inside a block the other side changed, or when two
    insertions at one point overlap without being identical."""
    upstream_changes = _line_changes(base, upstream)
    zuno_changes = _line_changes(base, zuno)
    if not upstream_changes or not zuno_changes:
        # git only reports a conflict when both sides changed something here;
        # a side that normalises back to the base is a rename this rule did not see.
        return None
    kind = "merged adjacent edits"
    accepted: list[tuple[int, int, list[str], int]] = [
        (start, end, lines, 0) for start, end, lines in upstream_changes
    ]
    for change in zuno_changes:
        keep = True
        for other in upstream_changes:
            relation = _change_relation(change, other)
            if relation == "conflict":
                return None
            if relation in ("duplicate", "united"):
                if change[0] == change[1]:
                    kind = "united insertions"
                keep = keep and relation != "duplicate"
        if keep:
            start, end, lines = change
            accepted.append((start, end, lines, 1))
    merged: list[str] = []
    position = 0
    # Insertions come before a block that starts at the same line; at one point
    # upstream's insertion precedes Zuno's.
    for start, end, lines, side in sorted(accepted, key=lambda change: (change[0], change[1] > change[0], change[3])):
        merged.extend(base[position:start])
        merged.extend(lines)
        position = max(position, end)
    merged.extend(base[position:])
    return merged, kind


def _change_relation(
    change: tuple[int, int, list[str]], other: tuple[int, int, list[str]]
) -> str:
    """How a Zuno change relates to an upstream change of the same base:
    ``duplicate`` (the same edit), ``united`` (insertions at one point that share
    no content line), ``conflict``, or ``independent``."""
    start, end, lines = change
    other_start, other_end, other_lines = other
    if (start, end, lines) == (other_start, other_end, other_lines):
        return "duplicate"
    inserts, other_inserts = start == end, other_start == other_end
    if not inserts and not other_inserts:
        return "conflict" if start < other_end and other_start < end else "independent"
    if inserts and other_inserts:
        if start != other_start:
            return "independent"
        content = {line for line in lines if not STRUCTURAL_LINE.match(line)}
        other_content = {line for line in other_lines if not STRUCTURAL_LINE.match(line)}
        return "conflict" if content & other_content else "united"
    point, block_start, block_end = (start, other_start, other_end) if inserts else (other_start, start, end)
    return "conflict" if block_start < point < block_end else "independent"


def _line_changes(old: list[str], new: list[str]) -> list[tuple[int, int, list[str]]]:
    matcher = difflib.SequenceMatcher(a=old, b=new, autojunk=False)
    return [
        (old_start, old_end, new[new_start:new_end])
        for tag, old_start, old_end, new_start, new_end in matcher.get_opcodes()
        if tag != "equal"
    ]


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
