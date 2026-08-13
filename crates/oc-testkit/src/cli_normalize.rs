//! Normalizing one CLI invocation's streams so two *presentations* of the same
//! answer can be compared, and nothing else can.
//!
//! # Why this exists separately from [`crate::normalize`]
//!
//! [`crate::normalize`] masks values that are **volatile** — a timestamp, a
//! kernel-assigned port, a temporary path. Nothing here is volatile. Everything
//! here is a difference this port makes **on purpose**, every one of them is
//! declared in `docs/divergences.toml`, and every one of them is asserted to be
//! *live* by a named test. That distinction is the whole reason the two modules
//! are not merged: a rule in [`crate::normalize`] says "these bytes cannot agree
//! across runs", and a rule here says "these bytes disagree because of a decision
//! recorded in the allow-list".
//!
//! The consequence is a rule that is stated here can be **deleted** the day the
//! decision is reverted, and the parity comparison then fails until the reversion
//! is complete. A rule in a general-purpose smoother has no such lifetime.
//!
//! # The four transformations, and what each one refuses to do
//!
//! | rule | what it removes | what it must still let diverge |
//! |---|---|---|
//! | [`strip_sgr`] | `ESC [ … m` colour sequences | any other escape sequence, and every character the colour surrounded |
//! | [`strip_error_prefix`] | a line-leading `Error: ` | the message after it, and an `Error:` that is not at the start of a line |
//! | [`strip_prompt_chrome`] | `@clack/prompts` box-drawing gutter, and trailing blank lines | the text on each line, its order, and its indentation past the gutter |
//! | [`canonicalize_json`] | object **key order**, and the `.0` on integral floats | every key, every value, and every non-JSON byte |
//!
//! [`mask_program_name`] is separate because it is not a divergence at all: the
//! two binaries have different names, so a sentence in which each names *itself*
//! is the same sentence. It is an exact-literal replacement of the caller's own
//! program name followed by a space — never a pattern — so a hint that names a
//! *different* program still diverges, and the `opencode` inside a data-directory
//! path is untouched.
//!
//! # The order is load-bearing
//!
//! [`normalize_cli_stream`] applies them in the order above and it matters:
//! `strip_error_prefix` looks for a line-leading `Error: `, which is only at the
//! line start once the colour sequence in front of it is gone, and
//! `canonicalize_json` must run last so it sees the un-decorated text a JSON
//! decoder can actually parse.

use std::borrow::Cow;

use serde_json::Value;

/// The `@clack/prompts` gutter glyphs the released binary draws.
///
/// Grounded in what the binary emits, not in the library's source: `opencode mcp
/// list` prints `┌`, `│`, `▲` and `└` in column zero, each followed by two
/// spaces except the bare `│` continuation line.
const GUTTER_GLYPHS: &[char] = &['\u{250c}', '\u{2502}', '\u{2514}', '\u{25b2}'];

/// Upstream's top-level error prefix, emitted even under `NO_COLOR`.
const ERROR_PREFIX: &str = "Error: ";

/// Remove every SGR (`ESC [ … m`) sequence, leaving all other bytes in place.
///
/// # Why only SGR
///
/// A cursor move, an alternate-screen switch or an OSC title change is a
/// behavioural difference in a CLI, not a presentational one, so this recognizes
/// exactly the colour/attribute form: `ESC`, `[`, zero or more digits and
/// semicolons, then `m`. Anything else beginning with `ESC` is left for the
/// comparison to reject.
#[must_use]
pub fn strip_sgr(text: &str) -> Cow<'_, str> {
    if !text.contains('\u{1b}') {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] == 0x1b && bytes.get(at + 1) == Some(&b'[') {
            let mut end = at + 2;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                end += 1;
            }
            if bytes.get(end) == Some(&b'm') {
                at = end + 1;
                continue;
            }
        }
        let start = at;
        let mut end = at + 1;
        while end < bytes.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
            end += 1;
        }
        out.push_str(&text[start..end]);
        at = end;
    }
    Cow::Owned(out)
}

/// Remove a line-leading `Error: `.
///
/// Anchored at the start of a line, so `Error:` inside a message — for instance a
/// nested `ServeError: Error: …` — is not touched, and a subject that dropped a
/// whole line still diverges because the line itself survives.
#[must_use]
pub fn strip_error_prefix(text: &str) -> Cow<'_, str> {
    if !text.contains(ERROR_PREFIX) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line.strip_prefix(ERROR_PREFIX).unwrap_or(line));
    }
    Cow::Owned(out)
}

/// Remove the prompt gutter and any trailing blank lines.
///
/// Three shapes, and nothing else:
///
/// 1. a line consisting only of gutter glyphs and spaces is dropped — that is the
///    `│` continuation `@clack/prompts` prints between steps;
/// 2. a leading gutter glyph followed by exactly two spaces is removed from a
///    line that has content after it;
/// 3. blank lines at the very end of the stream are dropped, because the prompt
///    library terminates its box with one and a plain writer does not.
///
/// Rule 3 is unconditional, and that is deliberate. The released binary also writes
/// the box's closing colour reset to **stderr**, so after [`strip_sgr`] its stderr
/// for `mcp list` is a lone `"\n"` where this port's is empty — a stream carrying no
/// content at all. Guarding rule 3 on "does this stream look decorated?" made the
/// rule fire for stdout and not for stderr, which is an inconsistency rather than a
/// narrowing.
///
/// Blank lines *inside* the stream are preserved, and so is every line with content:
/// a subject that lost a paragraph break, gained a line, or reordered two lines has
/// to be able to fail.
#[must_use]
pub fn strip_prompt_chrome(text: &str) -> Cow<'_, str> {
    if text.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut lines: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim_end();
        let only_gutter = !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| c == ' ' || GUTTER_GLYPHS.contains(&c));
        if only_gutter {
            continue;
        }
        let mut kept = line;
        for glyph in GUTTER_GLYPHS {
            let mut prefix = glyph.to_string();
            prefix.push_str("  ");
            if let Some(rest) = line.strip_prefix(prefix.as_str()) {
                kept = rest;
                break;
            }
        }
        lines.push(kept);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Cow::Owned(out)
}

/// Replace an exact `"<program> "` literal with `<BIN> `.
///
/// The trailing space is what makes this a command token rather than a substring:
/// `opencode mcp add` is masked, and the `opencode` in `<DATA>/opencode/auth.json`
/// is not. The caller passes the name of the binary it just ran, so — exactly as
/// with [`crate::Normalizer::mask_literal`] — there is no pattern that could also
/// swallow a name the subject got wrong.
#[must_use]
pub fn mask_program_name<'a>(text: &'a str, program: &str) -> Cow<'a, str> {
    if program.is_empty() {
        return Cow::Borrowed(text);
    }
    let needle = format!("{program} ");
    if !text.contains(needle.as_str()) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace(needle.as_str(), "<BIN> "))
}

/// Re-serialize every decodable JSON value in `text` with sorted object keys and
/// integral floats spelled as integers.
///
/// # Why this is not a smoother
///
/// It reorders keys and it respells `1024.0` as `1024`. It cannot add, drop or
/// change a key or a value: the text is decoded by `serde_json` and re-encoded, so
/// a differing key name, a differing value, a differing array order or a differing
/// nesting all survive into the comparison. Bytes that are not the start of a
/// decodable JSON value are copied unchanged, so surrounding prose, table
/// alignment and indentation are still compared.
///
/// # Why it is needed
///
/// Both facts were measured, not assumed. Todo 116 found the released binary's
/// `export` and this port's differing at byte 70 on **key order alone**, and
/// differing again on `1024.0` versus `1024` because JavaScript has one number
/// type and `JSON.stringify` writes an integral double without a fraction. The
/// same two differences appear in `agent list`'s embedded permission arrays.
#[must_use]
pub fn canonicalize_json(text: &str) -> Cow<'_, str> {
    if !text.contains('{') && !text.contains('[') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while !rest.is_empty() {
        let Some(offset) = rest.find(['{', '[']) else {
            out.push_str(rest);
            break;
        };
        let (head, tail) = rest.split_at(offset);
        out.push_str(head);
        let column = out.len() - out.rfind('\n').map_or(0, |index| index + 1);
        let mut stream = serde_json::Deserializer::from_str(tail).into_iter::<Value>();
        match stream.next() {
            Some(Ok(value)) => {
                let consumed = stream.byte_offset();
                let sorted = sort_keys(value);
                let rendered = serde_json::to_string_pretty(&sorted)
                    .unwrap_or_else(|_| tail[..consumed].to_owned());
                out.push_str(&reindent(&rendered, column));
                rest = &tail[consumed..];
                changed = true;
            }
            Some(Err(_)) | None => {
                let mut chars = tail.chars();
                let first = chars.next().unwrap_or('{');
                out.push(first);
                rest = chars.as_str();
            }
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// Rebuild every object with its keys in sorted order, and every integral float
/// as an integer.
fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(String, Value)> = map.into_iter().collect();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(key, nested)| (key, sort_keys(nested)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        Value::Number(number) => integral_float_to_integer(number),
        other => other,
    }
}

/// `1024.0` becomes `1024`; a fraction, and a magnitude no `i64` holds, are left
/// exactly as they were rather than being truncated.
fn integral_float_to_integer(number: serde_json::Number) -> Value {
    if let Some(float) = number.as_f64()
        && float.fract() == 0.0
        && float >= -(2f64.powi(63))
        && float < 2f64.powi(63)
        && number.as_i64().is_none()
        && number.as_u64().is_none()
    {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the bounds above put the value inside i64, and fract()==0 makes the cast exact"
        )]
        return Value::from(float as i64);
    }
    Value::Number(number)
}

/// Indent every line after the first by `column` spaces.
fn reindent(rendered: &str, column: usize) -> String {
    if column == 0 || !rendered.contains('\n') {
        return rendered.to_owned();
    }
    let pad = " ".repeat(column);
    rendered.replace('\n', &format!("\n{pad}"))
}

/// Apply every rule in the documented order.
///
/// `program` is the name of the binary whose output this is, for
/// [`mask_program_name`].
#[must_use]
pub fn normalize_cli_stream(text: &str, program: &str) -> String {
    let stripped = strip_sgr(text);
    let unprefixed = strip_error_prefix(&stripped);
    let plain = strip_prompt_chrome(&unprefixed);
    let named = mask_program_name(&plain, program);
    canonicalize_json(&named).into_owned()
}

/// The names of the rules [`normalize_cli_stream`] applies, in order.
///
/// Pinned by `cli_rule_names_are_pinned` for the same reason
/// [`crate::normalize`]'s default set is pinned: adding a rule here widens what
/// two binaries are permitted to disagree about, and that must be a reviewed edit
/// rather than a diff that turned a failure green.
pub const CLI_RULE_NAMES: &[&str] = &[
    "sgr-colour",
    "error-prefix",
    "prompt-chrome",
    "self-program-name",
    "json-key-order",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_rule_names_are_pinned() {
        assert_eq!(
            CLI_RULE_NAMES,
            [
                "sgr-colour",
                "error-prefix",
                "prompt-chrome",
                "self-program-name",
                "json-key-order",
            ],
            "the CLI presentation rule set changed; every rule must correspond to an entry in \
             docs/divergences.toml and to a test asserting the difference is still live"
        );
    }

    #[test]
    fn sgr_is_removed_and_other_escapes_are_not() {
        assert_eq!(
            strip_sgr("\u{1b}[91m\u{1b}[1mError: \u{1b}[0mnope").as_ref(),
            "Error: nope"
        );
        // A cursor move is behaviour, not colour, so it must still diverge.
        assert_eq!(strip_sgr("\u{1b}[2Jclear").as_ref(), "\u{1b}[2Jclear");
        assert_eq!(
            strip_sgr("\u{1b}]0;title\u{7}").as_ref(),
            "\u{1b}]0;title\u{7}"
        );
        // An incomplete sequence is left alone rather than swallowing the rest.
        assert_eq!(strip_sgr("\u{1b}[91").as_ref(), "\u{1b}[91");
        assert_eq!(strip_sgr("plain").as_ref(), "plain");
        assert_eq!(strip_sgr("路径 \u{1b}[0m✅").as_ref(), "路径 ✅");
    }

    #[test]
    fn the_error_prefix_is_removed_only_at_a_line_start() {
        assert_eq!(
            strip_error_prefix("Error: File not found: x\n").as_ref(),
            "File not found: x\n"
        );
        assert_eq!(
            strip_error_prefix("first\nError: second\n").as_ref(),
            "first\nsecond\n"
        );
        // Mid-line, it is part of the message.
        assert_eq!(
            strip_error_prefix("wrapped Error: inner").as_ref(),
            "wrapped Error: inner"
        );
        // A different prefix is a different message.
        assert_eq!(strip_error_prefix("error: lower").as_ref(), "error: lower");
        assert_eq!(
            strip_error_prefix("Error:no-space").as_ref(),
            "Error:no-space"
        );
    }

    #[test]
    fn prompt_chrome_is_removed_without_touching_the_text() {
        let clack = "\u{250c}  MCP Servers\n\u{2502}\n\u{25b2}  No MCP servers configured\n\u{2502}\n\u{2514}  Add servers with: opencode mcp add\n\n";
        assert_eq!(
            strip_prompt_chrome(clack).as_ref(),
            "MCP Servers\nNo MCP servers configured\nAdd servers with: opencode mcp add\n"
        );
        // A blank line in the middle is content and survives.
        assert_eq!(
            strip_prompt_chrome("a\n\nb\n").as_ref(),
            "a\n\nb\n",
            "an interior paragraph break must still be compared"
        );
        // A glyph with the wrong spacing is not the gutter.
        assert_eq!(
            strip_prompt_chrome("\u{250c} one space\n").as_ref(),
            "\u{250c} one space\n"
        );
        // Indentation past the gutter is preserved.
        assert_eq!(
            strip_prompt_chrome("\u{2502}    indented\n").as_ref(),
            "  indented\n"
        );
    }

    /// Rule 3's exact reach: trailing blank lines, and nothing else.
    ///
    /// The measured case is the released binary's stderr for `mcp list`, which is a
    /// lone colour reset — `"\n"` once [`strip_sgr`] has run — against this port's
    /// empty stderr.
    #[test]
    fn only_trailing_blank_lines_are_dropped() {
        assert_eq!(strip_prompt_chrome("\n").as_ref(), "");
        assert_eq!(strip_prompt_chrome("\n\n\n").as_ref(), "");
        assert_eq!(strip_prompt_chrome("").as_ref(), "");
        // Content is never touched, and neither is its single trailing newline.
        assert_eq!(strip_prompt_chrome("done\n").as_ref(), "done\n");
        assert_eq!(strip_prompt_chrome("done").as_ref(), "done\n");
        // A gained line, a lost line and a swapped pair all still diverge.
        assert_ne!(
            strip_prompt_chrome("a\nb\n"),
            strip_prompt_chrome("a\nb\nc\n")
        );
        assert_ne!(strip_prompt_chrome("a\nb\n"), strip_prompt_chrome("b\na\n"));
        assert_ne!(strip_prompt_chrome("a\nb\n"), strip_prompt_chrome("a\n"));
        // Trailing whitespace on a content line is not a blank line.
        assert_eq!(strip_prompt_chrome("a  \n").as_ref(), "a  \n");
    }

    #[test]
    fn the_program_mask_is_a_literal_and_spares_paths() {
        assert_eq!(
            mask_program_name("Add servers with: opencode mcp add", "opencode").as_ref(),
            "Add servers with: <BIN> mcp add"
        );
        assert_eq!(
            mask_program_name("Add servers with: opencode-rust mcp add", "opencode-rust").as_ref(),
            "Add servers with: <BIN> mcp add"
        );
        // The data directory is not the program name.
        assert_eq!(
            mask_program_name("<DATA>/opencode/auth.json", "opencode").as_ref(),
            "<DATA>/opencode/auth.json"
        );
        // A hint naming some other program still diverges.
        assert_eq!(
            mask_program_name("run: opencodex mcp add", "opencode").as_ref(),
            "run: opencodex mcp add"
        );
    }

    #[test]
    fn json_keys_are_sorted_and_nothing_else_moves() {
        assert_eq!(
            canonicalize_json("{\"b\":1,\"a\":2}").as_ref(),
            "{\n  \"a\": 2,\n  \"b\": 1\n}"
        );
        // Array order is data and must not be sorted.
        let ordered = canonicalize_json("[3,1,2]");
        assert!(
            ordered.contains("3") && ordered.find('3') < ordered.find('1'),
            "{ordered}"
        );
        // Integral floats are respelled; fractions are not.
        assert_eq!(
            canonicalize_json("{\"n\":1024.0}").as_ref(),
            "{\n  \"n\": 1024\n}"
        );
        assert_eq!(
            canonicalize_json("{\"n\":0.5}").as_ref(),
            "{\n  \"n\": 0.5\n}"
        );
        // Non-JSON prose survives byte for byte.
        assert_eq!(
            canonicalize_json("build (primary)\nno json here").as_ref(),
            "build (primary)\nno json here"
        );
        // A `{` that starts nothing decodable is copied.
        assert_eq!(canonicalize_json("{ not json").as_ref(), "{ not json");
    }

    /// The negative control: reordering keys must be the **only** thing this
    /// forgives. A renamed key, a changed value, a dropped key and a reordered
    /// array all have to survive canonicalization as differences.
    #[test]
    fn canonicalization_does_not_make_unequal_json_equal() {
        let base = "{\"action\":\"allow\",\"pattern\":\"*\"}";
        let reordered = "{\"pattern\":\"*\",\"action\":\"allow\"}";
        assert_eq!(canonicalize_json(base), canonicalize_json(reordered));

        for mutated in [
            "{\"action\":\"deny\",\"pattern\":\"*\"}",
            "{\"action\":\"allow\",\"pattern\":\"**\"}",
            "{\"effect\":\"allow\",\"pattern\":\"*\"}",
            "{\"action\":\"allow\"}",
            "{\"action\":\"allow\",\"pattern\":\"*\",\"extra\":1}",
        ] {
            assert_ne!(
                canonicalize_json(base),
                canonicalize_json(mutated),
                "canonicalization erased a real difference in {mutated}"
            );
        }
    }

    /// The negative control for the whole pipeline, on the real shapes measured
    /// from release 1.18.18: the presentation differences collapse and every
    /// difference in the *message* survives.
    #[test]
    fn the_pipeline_collapses_presentation_and_keeps_every_other_difference() {
        let oracle = "Exporting session: ses_x\n\u{1b}[91m\u{1b}[1mError: \u{1b}[0mSession not found: ses_x\n";
        let subject = "Exporting session: ses_x\nSession not found: ses_x\n";
        assert_eq!(
            normalize_cli_stream(oracle, "opencode"),
            normalize_cli_stream(subject, "opencode-rust")
        );

        for wrong in [
            "Exporting session: ses_x\nSession not found: ses_y\n",
            "Exporting session: ses_x\nsession not found: ses_x\n",
            "Session not found: ses_x\n",
            "Exporting session: ses_x\nSession not found: ses_x\nextra\n",
            "Exporting session: ses_x\n\nSession not found: ses_x\n",
        ] {
            assert_ne!(
                normalize_cli_stream(oracle, "opencode"),
                normalize_cli_stream(wrong, "opencode-rust"),
                "the pipeline erased a real difference in {wrong:?}"
            );
        }
    }

    /// Line endings are **not** normalized, because a subject emitting `\r\n`
    /// where the oracle emits `\n` breaks every consumer that pipes the output.
    #[test]
    fn carriage_returns_are_not_forgiven() {
        assert_ne!(
            normalize_cli_stream("done\n", "opencode"),
            normalize_cli_stream("done\r\n", "opencode-rust")
        );
    }
}
