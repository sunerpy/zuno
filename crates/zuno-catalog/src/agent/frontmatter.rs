//! Markdown frontmatter: the `---`-delimited YAML head, and the body after it.
//!
//! Oracle: `packages/core/src/config/markdown.ts` (gray-matter, plus the
//! `sanitize` retry at `:22-35`) and `packages/opencode/src/config/markdown.ts`
//! (the `FrontmatterError` wrapper at `:26-38`).
//!
//! # Why this is hand-written
//!
//! No YAML parser is pinned in the workspace's `[workspace.dependencies]`, and
//! adding one there is out of this task's scope. So this module implements the
//! subset of YAML that agent frontmatter actually uses, and that subset was
//! chosen by probing the real `opencode` 1.18.12 binary rather than by reading
//! the YAML spec — every construct below is one the oracle was observed to accept
//! in an `agent/*.md` file:
//!
//! | construct | probe |
//! |---|---|
//! | plain scalar (`mode: subagent`) | `agent/dq.md` family |
//! | double- and single-quoted scalars | `mode: "subagent"`, `mode: 'subagent'` |
//! | nested block map (`permission.rules:` then indented keys) | emitted `edit`/`shell` rules |
//! | flow map (`permission: { mode: standard, rules: {...} }`) | emitted the same policy |
//! | block scalar (`description: \|`) | agent loaded, `mode` after it still read |
//! | comments and blank lines inside the head | agent loaded |
//! | an unquoted colon in a value | agent loaded via the `sanitize` retry |
//! | no frontmatter at all | agent loaded, whole file became the prompt |
//!
//! Anything outside that subset is a parse error rather than a silent
//! misreading: a frontmatter key that quietly becomes the wrong value would
//! reach the provider as a wrong model or a wrong permission.
//!
//! # Deliberate omissions
//!
//! Anchors and aliases (`&a`/`*a`), tags (`!!str`), multi-document streams,
//! complex keys (`? `), and explicit typing are all rejected. None of them appear
//! in any agent definition the oracle ships or documents, and supporting them
//! half-way is worse than refusing them.

use serde_json::{Map, Number, Value};
use std::fmt;

/// A parsed Markdown file: its frontmatter data, and everything after the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// The frontmatter object, empty when the file has no `---` head.
    pub data: Map<String, Value>,
    /// The body, verbatim and untrimmed. Callers that need the oracle's
    /// `md.content.trim()` do the trimming themselves.
    pub content: String,
}

/// Why a frontmatter head could not be read.
///
/// Every variant names the 1-based line within the frontmatter head, because a
/// message that says only "invalid YAML" sends the reader back to a file they
/// have already looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterError {
    /// A line is neither a comment, a `key: value`, nor a `- item`.
    Unparseable {
        /// 1-based line number within the frontmatter head.
        line: usize,
        /// The offending line, trimmed of trailing whitespace.
        text: String,
    },
    /// Indentation stepped out to a column that opens no enclosing block.
    Indentation {
        /// 1-based line number within the frontmatter head.
        line: usize,
        /// The offending line, trimmed of trailing whitespace.
        text: String,
    },
    /// A `key:` opened a block, and the next line is a sibling rather than a
    /// child, so the key has neither a scalar nor a collection.
    MissingValue {
        /// 1-based line number within the frontmatter head.
        line: usize,
        /// The key with no value.
        key: String,
    },
    /// The same key appears twice at the same level.
    DuplicateKey {
        /// 1-based line number within the frontmatter head.
        line: usize,
        /// The repeated key.
        key: String,
    },
    /// A flow collection (`{...}` or `[...]`) is malformed.
    Flow {
        /// 1-based line number within the frontmatter head.
        line: usize,
        /// What was wrong with it.
        detail: String,
    },
    /// A construct this parser deliberately does not support.
    Unsupported {
        /// 1-based line number within the frontmatter head.
        line: usize,
        /// The construct, named.
        construct: &'static str,
    },
    /// A sequence entry appeared where a mapping was already being built, or the
    /// reverse.
    MixedCollection {
        /// 1-based line number within the frontmatter head.
        line: usize,
    },
    /// The top level of the frontmatter head is not a mapping.
    NotAMapping,
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unparseable { line, text } => {
                write!(f, "frontmatter line {line} is not `key: value`: {text}")
            }
            Self::Indentation { line, text } => write!(
                f,
                "frontmatter line {line} is indented to a column that opens no block: {text}"
            ),
            Self::MissingValue { line, key } => {
                write!(f, "frontmatter key `{key}` on line {line} has no value")
            }
            Self::DuplicateKey { line, key } => {
                write!(
                    f,
                    "frontmatter key `{key}` is set twice, again on line {line}"
                )
            }
            Self::Flow { line, detail } => {
                write!(
                    f,
                    "frontmatter line {line} has a malformed flow collection: {detail}"
                )
            }
            Self::Unsupported { line, construct } => write!(
                f,
                "frontmatter line {line} uses {construct}, which agent frontmatter does not support"
            ),
            Self::MixedCollection { line } => write!(
                f,
                "frontmatter line {line} mixes a sequence entry into a mapping"
            ),
            Self::NotAMapping => f.write_str("frontmatter is not a mapping of keys to values"),
        }
    }
}

impl std::error::Error for FrontmatterError {}

/// Split a Markdown file into frontmatter data and body.
///
/// A file that does not open with a `---` line has no frontmatter: `data` is
/// empty and `content` is the whole file. That mirrors gray-matter, and it is why
/// a plain `.md` file with no head is still a valid agent whose prompt is its
/// entire text.
///
/// # Errors
///
/// [`FrontmatterError`] when a `---` head exists but its contents cannot be read,
/// even after the [`sanitize`] retry.
pub fn parse(text: &str) -> Result<Document, FrontmatterError> {
    // gray-matter strips a UTF-8 BOM before looking for the delimiter.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let Some(split) = split_head(text) else {
        return Ok(Document {
            data: Map::new(),
            content: text.to_owned(),
        });
    };

    match parse_head(split.head, split.head_offset) {
        Ok(data) => Ok(Document {
            data,
            content: split.body.to_owned(),
        }),
        Err(first) => {
            // markdown.ts:5-10 — retry the whole document through `sanitize`,
            // which rewrites values containing an unquoted colon as block
            // scalars. Other coding agents emit those, so real agent files
            // depend on this path.
            let sanitized = sanitize(text);
            let Some(retry) = split_head(&sanitized) else {
                return Err(first);
            };
            // Report the *first* error, not the sanitized one: the sanitizer's
            // rewrite makes line numbers and text unrecognizable to the reader.
            let data = parse_head(retry.head, retry.head_offset).map_err(|_| first)?;
            Ok(Document {
                data,
                content: split.body.to_owned(),
            })
        }
    }
}

struct Split<'a> {
    head: &'a str,
    body: &'a str,
    /// How many physical lines precede the head, so error line numbers can name
    /// the line the author actually wrote. Always 1: the opening `---`.
    head_offset: usize,
}

/// gray-matter's delimiter scan: the file must open with `---` on its own line,
/// and the head ends at the next line that is exactly `---`.
fn split_head(text: &str) -> Option<Split<'_>> {
    let after_open = strip_delimiter_line(text)?;
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let head = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return Some(Split {
                head,
                body,
                head_offset: 1,
            });
        }
        offset += line.len();
    }
    None
}

/// Consume a leading `---` line, returning the remainder. `None` when the text
/// does not open with one.
fn strip_delimiter_line(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    if let Some(rest) = rest.strip_prefix("\r\n") {
        return Some(rest);
    }
    if let Some(rest) = rest.strip_prefix('\n') {
        return Some(rest);
    }
    // A bare `---` with nothing after it has no closing delimiter anyway.
    rest.is_empty().then_some(rest)
}

/// The oracle's `sanitize` (`core/src/config/markdown.ts:22-35`), line for line.
///
/// Any top-level `key: value` whose value contains a further colon and is not
/// already quoted or a block scalar is rewritten as `key: |-` plus an indented
/// value line. That is how `description: Use when: X` survives.
#[must_use]
pub fn sanitize(text: &str) -> String {
    let Some(split) = split_head(text) else {
        return text.to_owned();
    };
    let head = split.head;

    let mut rewritten = Vec::new();
    for line in head.lines() {
        let trimmed = line.trim();
        // `line.trim().startsWith("#") || line.trim() === "" || /^\s+/.test(line)`
        if trimmed.starts_with('#') || trimmed.is_empty() || starts_with_whitespace(line) {
            rewritten.push(line.to_owned());
            continue;
        }
        let Some((key, value)) = split_plain_entry(line) else {
            rewritten.push(line.to_owned());
            continue;
        };
        let value = value.trim();
        if value.is_empty()
            || value == ">"
            || value == "|"
            || value.starts_with('"')
            || value.starts_with('\'')
            || !value.contains(':')
        {
            rewritten.push(line.to_owned());
            continue;
        }
        rewritten.push(format!("{key}: |-"));
        rewritten.push(format!("  {value}"));
    }

    // `content.replace(frontmatter, ...)` — a single replacement of the head.
    // The oracle's capture group `([\s\S]*?)` sits between `^---\r?\n` and
    // `\r?\n---`, so it excludes the newline before the closing delimiter while
    // `split_head` includes it. Replacing the trimmed span keeps that newline in
    // place instead of swallowing it, which would weld the head to the `---`.
    let head = head.strip_suffix('\n').unwrap_or(head);
    let head = head.strip_suffix('\r').unwrap_or(head);
    let replacement = rewritten.join("\n");
    text.replacen(head, &replacement, 1)
}

fn starts_with_whitespace(line: &str) -> bool {
    line.chars().next().is_some_and(char::is_whitespace)
}

/// The oracle's `/^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$/`, without a regex crate.
fn split_plain_entry(line: &str) -> Option<(&str, &str)> {
    let mut chars = line.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut key_end = line.len();
    for (index, ch) in chars {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            continue;
        }
        key_end = index;
        break;
    }
    let key = &line[..key_end];
    let rest = line[key_end..].trim_start();
    let value = rest.strip_prefix(':')?;
    Some((key, value.trim_start()))
}

// ---------------------------------------------------------------------------
// The YAML subset.
// ---------------------------------------------------------------------------

/// One significant line of the head, with its indentation measured.
struct Line<'a> {
    /// 1-based within the whole file, so a message names the line the author wrote.
    number: usize,
    indent: usize,
    text: &'a str,
    /// Blank lines immediately above this one. They are dropped from the line list
    /// because no mapping or sequence cares about them, but a block scalar does:
    /// a blank line inside one is a paragraph break, and folding it away turns two
    /// paragraphs into one sentence.
    blanks_before: usize,
}

fn parse_head(head: &str, line_offset: usize) -> Result<Map<String, Value>, FrontmatterError> {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut pending_blanks = 0usize;
    for (index, raw) in head.lines().enumerate() {
        let without_comment = strip_trailing_comment(raw);
        let trimmed = without_comment.trim_end();
        if trimmed.trim_start().is_empty() {
            pending_blanks += 1;
            continue;
        }
        lines.push(Line {
            number: index + 1 + line_offset,
            indent: trimmed.len() - trimmed.trim_start().len(),
            text: trimmed.trim_start(),
            blanks_before: std::mem::take(&mut pending_blanks),
        });
    }

    if lines.is_empty() {
        return Ok(Map::new());
    }

    let mut cursor = 0usize;
    let base = lines[0].indent;
    let value = parse_block(&lines, &mut cursor, base)?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(FrontmatterError::NotAMapping),
    }
}

/// A `#` starts a comment only at the start of a line or after whitespace, and
/// never inside a quoted scalar. Anything stricter would truncate a value such as
/// `description: fix issue #25713`.
fn strip_trailing_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut previous_was_space = true;
    for (index, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'#' if previous_was_space => return &line[..index],
                _ => {}
            },
        }
        previous_was_space = byte.is_ascii_whitespace();
    }
    line
}

/// Parse the block starting at `cursor`, whose entries all sit at column
/// `indent`. Returns a mapping or a sequence.
fn parse_block(
    lines: &[Line<'_>],
    cursor: &mut usize,
    indent: usize,
) -> Result<Value, FrontmatterError> {
    let first = &lines[*cursor];
    if first.text.starts_with("- ") || first.text == "-" {
        parse_sequence(lines, cursor, indent)
    } else {
        parse_mapping(lines, cursor, indent)
    }
}

fn parse_mapping(
    lines: &[Line<'_>],
    cursor: &mut usize,
    indent: usize,
) -> Result<Value, FrontmatterError> {
    let mut map = Map::new();
    while *cursor < lines.len() {
        let line = &lines[*cursor];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(FrontmatterError::Indentation {
                line: line.number,
                text: line.text.to_owned(),
            });
        }
        if line.text.starts_with("- ") || line.text == "-" {
            return Err(FrontmatterError::MixedCollection { line: line.number });
        }
        reject_unsupported(line)?;

        let (key, rest) = split_key(line)?;
        *cursor += 1;

        let value = if rest.is_empty() {
            parse_child_block(lines, cursor, indent, line, &key)?
        } else if let Some(header) = BlockScalarHeader::parse(rest) {
            Value::String(parse_block_scalar(lines, cursor, indent, &header))
        } else {
            parse_inline_scalar(rest, line.number)?
        };

        if map.insert(key.clone(), value).is_some() {
            return Err(FrontmatterError::DuplicateKey {
                line: line.number,
                key,
            });
        }
    }
    Ok(Value::Object(map))
}

/// A `key:` with nothing after it: the value is the more-indented block that
/// follows, or an error when nothing follows.
fn parse_child_block(
    lines: &[Line<'_>],
    cursor: &mut usize,
    indent: usize,
    key_line: &Line<'_>,
    key: &str,
) -> Result<Value, FrontmatterError> {
    let Some(next) = lines.get(*cursor) else {
        return Err(FrontmatterError::MissingValue {
            line: key_line.number,
            key: key.to_owned(),
        });
    };
    // A sequence may sit at the parent's own column; a mapping may not.
    let is_sequence = next.text.starts_with("- ") || next.text == "-";
    if next.indent > indent || (is_sequence && next.indent == indent) {
        let child_indent = next.indent;
        return parse_block(lines, cursor, child_indent);
    }
    Err(FrontmatterError::MissingValue {
        line: key_line.number,
        key: key.to_owned(),
    })
}

fn parse_sequence(
    lines: &[Line<'_>],
    cursor: &mut usize,
    indent: usize,
) -> Result<Value, FrontmatterError> {
    let mut items = Vec::new();
    while *cursor < lines.len() {
        let line = &lines[*cursor];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(FrontmatterError::Indentation {
                line: line.number,
                text: line.text.to_owned(),
            });
        }
        let Some(rest) = strip_dash(line.text) else {
            break;
        };
        reject_unsupported(line)?;
        *cursor += 1;

        if rest.is_empty() {
            let Some(next) = lines.get(*cursor) else {
                items.push(Value::Null);
                continue;
            };
            if next.indent > indent {
                let child_indent = next.indent;
                items.push(parse_block(lines, cursor, child_indent)?);
            } else {
                items.push(Value::Null);
            }
            continue;
        }

        // `- key: value` opens a nested mapping whose column is the dash's
        // column plus the dash and its space.
        if let Some(nested) = nested_mapping_after_dash(lines, cursor, line, rest)? {
            items.push(nested);
            continue;
        }

        if let Some(header) = BlockScalarHeader::parse(rest) {
            items.push(Value::String(parse_block_scalar(
                lines, cursor, indent, &header,
            )));
        } else {
            items.push(parse_inline_scalar(rest, line.number)?);
        }
    }
    Ok(Value::Array(items))
}

fn strip_dash(text: &str) -> Option<&str> {
    if text == "-" {
        return Some("");
    }
    text.strip_prefix("- ").map(str::trim_start)
}

/// `- key: value`, possibly with further keys indented under it.
fn nested_mapping_after_dash(
    lines: &[Line<'_>],
    cursor: &mut usize,
    dash_line: &Line<'_>,
    rest: &str,
) -> Result<Option<Value>, FrontmatterError> {
    if rest.starts_with(['{', '[', '"', '\'', '|', '>']) {
        return Ok(None);
    }
    let Some((key, value)) = split_plain_key(rest) else {
        return Ok(None);
    };
    let inner_indent = dash_line.indent + (dash_line.text.len() - rest.len());
    let mut map = Map::new();
    let first = if value.is_empty() {
        let synthetic = Line {
            number: dash_line.number,
            indent: inner_indent,
            text: rest,
            blanks_before: 0,
        };
        parse_child_block(lines, cursor, inner_indent, &synthetic, key)?
    } else if let Some(header) = BlockScalarHeader::parse(value) {
        Value::String(parse_block_scalar(lines, cursor, inner_indent, &header))
    } else {
        parse_inline_scalar(value, dash_line.number)?
    };
    map.insert(key.to_owned(), first);

    // Continue with any sibling keys at the same inner column.
    while *cursor < lines.len() && lines[*cursor].indent == inner_indent {
        let line = &lines[*cursor];
        if line.text.starts_with("- ") || line.text == "-" {
            break;
        }
        reject_unsupported(line)?;
        let (key, value_rest) = split_key(line)?;
        *cursor += 1;
        let value = if value_rest.is_empty() {
            parse_child_block(lines, cursor, inner_indent, line, &key)?
        } else if let Some(header) = BlockScalarHeader::parse(value_rest) {
            Value::String(parse_block_scalar(lines, cursor, inner_indent, &header))
        } else {
            parse_inline_scalar(value_rest, line.number)?
        };
        if map.insert(key.clone(), value).is_some() {
            return Err(FrontmatterError::DuplicateKey {
                line: line.number,
                key,
            });
        }
    }
    Ok(Some(Value::Object(map)))
}

fn reject_unsupported(line: &Line<'_>) -> Result<(), FrontmatterError> {
    let construct = if line.text.starts_with("? ") {
        Some("an explicit complex key")
    } else if line.text.starts_with("<<:") {
        Some("a merge key")
    } else if line.text.starts_with("...") {
        Some("a document end marker")
    } else if line.text.starts_with("---") {
        Some("a nested document marker")
    } else {
        None
    };
    match construct {
        Some(construct) => Err(FrontmatterError::Unsupported {
            line: line.number,
            construct,
        }),
        None => Ok(()),
    }
}

/// `key:` or `key: rest`, where the key may be quoted (`"*": ask`).
fn split_key<'a>(line: &Line<'a>) -> Result<(String, &'a str), FrontmatterError> {
    split_plain_key(line.text)
        .map(|(key, rest)| (unquote_key(key), rest))
        .ok_or_else(|| FrontmatterError::Unparseable {
            line: line.number,
            text: line.text.to_owned(),
        })
}

/// Find the `:` that separates key from value, skipping colons inside quotes and
/// requiring the separator to be followed by whitespace or end-of-line — so
/// `description: 12:30` keys on the first colon while `"a:b": c` keys on the
/// second.
fn split_plain_key(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    for (index, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' if index == 0 => quote = Some(byte),
                b':' => {
                    let followed_by_space = bytes
                        .get(index + 1)
                        .is_none_or(|next| next.is_ascii_whitespace());
                    if !followed_by_space {
                        continue;
                    }
                    let key = text[..index].trim_end();
                    if key.is_empty() {
                        return None;
                    }
                    return Some((key, text[index + 1..].trim_start()));
                }
                _ => {}
            },
        }
    }
    None
}

fn unquote_key(key: &str) -> String {
    for quote in ['"', '\''] {
        if key.len() >= 2 && key.starts_with(quote) && key.ends_with(quote) {
            return unescape(&key[1..key.len() - 1], quote);
        }
    }
    key.to_owned()
}

// ---------------------------------------------------------------------------
// Block scalars.
// ---------------------------------------------------------------------------

/// A `|`, `|-`, `>`, or `>-` header, with its chomping mode.
struct BlockScalarHeader {
    folded: bool,
    /// `-` strips the trailing newline, `+` keeps every one, absent clips to one.
    chomp: Chomp,
}

#[derive(PartialEq, Eq)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

impl BlockScalarHeader {
    fn parse(text: &str) -> Option<Self> {
        let mut chars = text.chars();
        let folded = match chars.next()? {
            '|' => false,
            '>' => true,
            _ => return None,
        };
        let rest = chars.as_str();
        let (chomp, rest) = match rest.strip_prefix('-') {
            Some(rest) => (Chomp::Strip, rest),
            None => match rest.strip_prefix('+') {
                Some(rest) => (Chomp::Keep, rest),
                None => (Chomp::Clip, rest),
            },
        };
        // An explicit indentation indicator (`|2`) is out of the accepted subset;
        // treating it as a plain scalar would be a silent misread, so refuse to
        // recognize the header and let the scalar path report it.
        if !rest.trim().is_empty() {
            return None;
        }
        Some(Self { folded, chomp })
    }
}

/// Consume the indented lines that make up a block scalar's body.
///
/// `parent_indent` is the column of the key that opened it; every body line must
/// be more indented than that.
fn parse_block_scalar(
    lines: &[Line<'_>],
    cursor: &mut usize,
    parent_indent: usize,
    header: &BlockScalarHeader,
) -> String {
    let mut body: Vec<String> = Vec::new();
    // The first body line sets the block's own indentation; deeper lines keep the
    // difference, which is what makes an indented list inside a prompt survive.
    let mut block_indent = None;
    while let Some(line) = lines.get(*cursor) {
        if line.indent <= parent_indent {
            break;
        }
        let indent = *block_indent.get_or_insert(line.indent);
        if !body.is_empty() {
            for _ in 0..line.blanks_before {
                body.push(String::new());
            }
        }
        let extra = line.indent.saturating_sub(indent);
        if extra == 0 {
            body.push(line.text.to_owned());
        } else {
            body.push(" ".repeat(extra) + line.text);
        }
        *cursor += 1;
    }
    finish_block_scalar(body, header)
}

fn finish_block_scalar(body: Vec<String>, header: &BlockScalarHeader) -> String {
    let joined = if header.folded {
        fold(&body)
    } else {
        body.join("\n")
    };
    match header.chomp {
        Chomp::Strip => joined,
        Chomp::Clip | Chomp::Keep if joined.is_empty() => joined,
        Chomp::Clip | Chomp::Keep => joined + "\n",
    }
}

/// A folded scalar joins consecutive non-empty lines with a space and keeps blank
/// lines as paragraph breaks.
fn fold(body: &[String]) -> String {
    let mut out = String::new();
    let mut previous_blank = true;
    for line in body {
        if line.trim().is_empty() {
            out.push('\n');
            previous_blank = true;
            continue;
        }
        if !previous_blank {
            out.push(' ');
        }
        out.push_str(line);
        previous_blank = false;
    }
    out
}

// ---------------------------------------------------------------------------
// Scalars and flow collections.
// ---------------------------------------------------------------------------

fn parse_inline_scalar(text: &str, line: usize) -> Result<Value, FrontmatterError> {
    let text = text.trim();
    if text.starts_with('{') {
        return parse_flow_map(text, line);
    }
    if text.starts_with('[') {
        return parse_flow_sequence(text, line);
    }
    if text.starts_with('&') || text.starts_with('*') {
        return Err(FrontmatterError::Unsupported {
            line,
            construct: "an anchor or alias",
        });
    }
    if text.starts_with("!!") || text.starts_with('!') {
        return Err(FrontmatterError::Unsupported {
            line,
            construct: "an explicit type tag",
        });
    }
    Ok(scalar(text))
}

/// A plain, single-quoted, or double-quoted scalar, resolved to the YAML 1.1 core
/// types gray-matter's `js-yaml` produces.
fn scalar(text: &str) -> Value {
    if let Some(inner) = strip_quotes(text, '"') {
        return Value::String(unescape(inner, '"'));
    }
    if let Some(inner) = strip_quotes(text, '\'') {
        // A single-quoted scalar escapes only `''`.
        return Value::String(inner.replace("''", "'"));
    }
    match text {
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Value::Bool(true),
        "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => {
            Value::Bool(false)
        }
        _ => number(text).unwrap_or_else(|| Value::String(text.to_owned())),
    }
}

fn strip_quotes(text: &str, quote: char) -> Option<&str> {
    if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
        return Some(&text[1..text.len() - 1]);
    }
    None
}

fn number(text: &str) -> Option<Value> {
    // A leading `+`, a leading zero, or an underscore separator are all YAML but
    // are not JSON numbers; parsing them through Rust's own parsers keeps the two
    // sets aligned and rejects the rest as strings.
    if let Ok(integer) = text.parse::<i64>() {
        return Some(Value::Number(Number::from(integer)));
    }
    if let Ok(unsigned) = text.parse::<u64>() {
        return Some(Value::Number(Number::from(unsigned)));
    }
    let float = text.parse::<f64>().ok()?;
    // `Number::from_f64` refuses NaN and infinity, which JSON cannot hold; a
    // frontmatter `.inf` therefore stays a string rather than becoming null.
    Number::from_f64(float).map(Value::Number)
}

fn unescape(text: &str, quote: char) -> String {
    if quote != '"' {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn parse_flow_map(text: &str, line: usize) -> Result<Value, FrontmatterError> {
    let inner = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| FrontmatterError::Flow {
            line,
            detail: "unbalanced `{}`".to_owned(),
        })?;
    let mut map = Map::new();
    for entry in split_flow(inner, line)? {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, value) = split_plain_key(entry).ok_or_else(|| FrontmatterError::Flow {
            line,
            detail: format!("entry `{entry}` is not `key: value`"),
        })?;
        let value = parse_inline_scalar(value, line)?;
        if map.insert(unquote_key(key), value).is_some() {
            return Err(FrontmatterError::DuplicateKey {
                line,
                key: unquote_key(key),
            });
        }
    }
    Ok(Value::Object(map))
}

fn parse_flow_sequence(text: &str, line: usize) -> Result<Value, FrontmatterError> {
    let inner = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| FrontmatterError::Flow {
            line,
            detail: "unbalanced `[]`".to_owned(),
        })?;
    let mut items = Vec::new();
    for entry in split_flow(inner, line)? {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        items.push(parse_inline_scalar(entry, line)?);
    }
    Ok(Value::Array(items))
}

/// Split a flow collection's interior on top-level commas, respecting nesting and
/// quotes.
fn split_flow(inner: &str, line: usize) -> Result<Vec<&str>, FrontmatterError> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut start = 0usize;
    for (index, &byte) in inner.as_bytes().iter().enumerate() {
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(FrontmatterError::Flow {
                            line,
                            detail: "a closing bracket has no opener".to_owned(),
                        });
                    }
                }
                b',' if depth == 0 => {
                    parts.push(&inner[start..index]);
                    start = index + 1;
                }
                _ => {}
            },
        }
    }
    if depth != 0 || quote.is_some() {
        return Err(FrontmatterError::Flow {
            line,
            detail: "an opening bracket or quote is never closed".to_owned(),
        });
    }
    parts.push(&inner[start..]);
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(text: &str) -> Value {
        Value::Object(parse(text).expect("frontmatter should parse").data)
    }

    #[test]
    fn a_file_without_a_head_has_no_data_and_keeps_its_whole_body() {
        let doc = parse("Just a body, no frontmatter.\n").expect("no head is not a failure");
        assert!(doc.data.is_empty());
        assert_eq!(doc.content, "Just a body, no frontmatter.\n");
    }

    #[test]
    fn plain_quoted_and_flow_forms_all_reach_the_same_value() {
        for text in [
            "---\nmode: subagent\n---\nbody\n",
            "---\nmode: \"subagent\"\n---\nbody\n",
            "---\nmode: 'subagent'\n---\nbody\n",
        ] {
            assert_eq!(data(text), json!({ "mode": "subagent" }), "for {text:?}");
        }
    }

    #[test]
    fn a_nested_block_map_becomes_a_nested_object() {
        let text = "---\nmode: subagent\npermission:\n  mode: standard\n  rules:\n    edit: deny\n    shell:\n      \"git push\": ask\n---\nbody\n";
        assert_eq!(
            data(text),
            json!({
                "mode": "subagent",
                "permission": {
                    "mode": "standard",
                    "rules": { "edit": "deny", "shell": { "git push": "ask" } },
                },
            })
        );
    }

    #[test]
    fn a_flow_map_matches_the_equivalent_block_map() {
        let flow = data(
            "---\npermission: { mode: standard, rules: { edit: deny, webfetch: allow } }\n---\nb\n",
        );
        let block = data(
            "---\npermission:\n  mode: standard\n  rules:\n    edit: deny\n    webfetch: allow\n---\nb\n",
        );
        assert_eq!(flow, block);
    }

    #[test]
    fn a_literal_block_scalar_keeps_its_newlines_and_inner_colons() {
        let text =
            "---\ndescription: |\n  line one\n  line two: with colon\nmode: subagent\n---\nbody\n";
        assert_eq!(
            data(text),
            json!({
                "description": "line one\nline two: with colon\n",
                "mode": "subagent",
            })
        );
    }

    #[test]
    fn a_stripped_block_scalar_drops_the_trailing_newline() {
        let text = "---\ndescription: |-\n  one\n  two\n---\nb\n";
        assert_eq!(data(text), json!({ "description": "one\ntwo" }));
    }

    #[test]
    fn a_folded_block_scalar_joins_lines_with_a_space() {
        let text = "---\ndescription: >-\n  one\n  two\n\n  three\n---\nb\n";
        assert_eq!(data(text), json!({ "description": "one two\nthree" }));
    }

    #[test]
    fn comments_and_blank_lines_inside_the_head_are_ignored() {
        let text = "---\n# a comment\n\nmode: subagent\n---\nbody\n";
        assert_eq!(data(text), json!({ "mode": "subagent" }));
    }

    #[test]
    fn a_hash_after_whitespace_starts_a_comment_even_in_a_plain_scalar() {
        // Verified against opencode 1.18.12: `color: #ff5733` unquoted made the
        // binary report `got null color`, so the value really was consumed as a
        // comment. A hex colour therefore has to be quoted, and this parser must
        // agree or it would accept files the oracle rejects.
        let text = "---\ndescription: fixes issue #25713\n---\nb\n";
        assert_eq!(data(text), json!({ "description": "fixes issue" }));
    }

    #[test]
    fn a_quoted_hash_survives_so_a_hex_colour_can_be_written() {
        let text = "---\ncolor: \"#ff5733\"\n---\nb\n";
        assert_eq!(data(text), json!({ "color": "#ff5733" }));
    }

    #[test]
    fn a_hash_with_no_leading_whitespace_is_part_of_the_value() {
        let text = "---\ndescription: issue#25713\n---\nb\n";
        assert_eq!(data(text), json!({ "description": "issue#25713" }));
    }

    #[test]
    fn an_unquoted_colon_survives_through_the_sanitize_retry() {
        // Oracle: markdown.ts:5-10 retries the whole document through `sanitize`.
        let text =
            "---\ndescription: Use this when: you need security\nmode: subagent\n---\nbody\n";
        assert_eq!(
            data(text),
            json!({
                "description": "Use this when: you need security",
                "mode": "subagent",
            })
        );
    }

    #[test]
    fn the_body_is_returned_untrimmed_so_the_caller_decides() {
        let doc = parse("---\nmode: all\n---\n\n  body with space  \n\n").expect("parses");
        assert_eq!(doc.content, "\n  body with space  \n\n");
        assert_eq!(doc.content.trim(), "body with space");
    }

    #[test]
    fn scalars_resolve_to_yaml_core_types() {
        let text =
            "---\na: true\nb: false\nc: 5\nd: 0.25\ne: null\nf: ~\ng: yes\nh: text\n---\nb\n";
        assert_eq!(
            data(text),
            json!({
                "a": true, "b": false, "c": 5, "d": 0.25,
                "e": null, "f": null, "g": true, "h": "text",
            })
        );
    }

    #[test]
    fn a_quoted_number_stays_a_string() {
        assert_eq!(data("---\nsteps: \"5\"\n---\nb\n"), json!({"steps": "5"}));
    }

    #[test]
    fn block_and_flow_sequences_both_produce_arrays() {
        let block = data("---\nitems:\n  - one\n  - two\n---\nb\n");
        let flow = data("---\nitems: [one, two]\n---\nb\n");
        assert_eq!(block, json!({ "items": ["one", "two"] }));
        assert_eq!(block, flow);
    }

    #[test]
    fn a_sequence_of_mappings_parses_each_entry() {
        let text = "---\nitems:\n  - name: a\n    value: 1\n  - name: b\n    value: 2\n---\nb\n";
        assert_eq!(
            data(text),
            json!({ "items": [
                { "name": "a", "value": 1 },
                { "name": "b", "value": 2 },
            ]})
        );
    }

    #[test]
    fn a_sequence_may_sit_at_its_parent_key_column() {
        let text = "---\nitems:\n- one\n- two\n---\nb\n";
        assert_eq!(data(text), json!({ "items": ["one", "two"] }));
    }

    #[test]
    fn crlf_line_endings_parse_the_same_as_lf() {
        let lf = data("---\nmode: subagent\n---\nbody\n");
        let crlf = data("---\r\nmode: subagent\r\n---\r\nbody\r\n");
        assert_eq!(lf, crlf);
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_head() {
        assert_eq!(
            data("\u{feff}---\nmode: all\n---\nb\n"),
            json!({ "mode": "all" })
        );
    }

    #[test]
    fn an_empty_head_is_an_empty_object_not_an_error() {
        let doc = parse("---\n---\nbody\n").expect("an empty head is legal");
        assert!(doc.data.is_empty());
        assert_eq!(doc.content, "body\n");
    }

    #[test]
    fn a_head_that_is_never_closed_is_not_a_head_at_all() {
        let doc = parse("---\nmode: all\nbody with no closing delimiter\n").expect("no head");
        assert!(doc.data.is_empty());
        assert!(doc.content.starts_with("---"));
    }

    #[test]
    fn a_duplicate_key_is_reported_rather_than_silently_overwritten() {
        let err = parse("---\nmode: all\nmode: primary\n---\nb\n").expect_err("duplicate");
        assert_eq!(
            err,
            FrontmatterError::DuplicateKey {
                line: 3,
                key: "mode".to_owned(),
            }
        );
        assert!(err.to_string().contains("set twice"), "{err}");
    }

    #[test]
    fn a_key_with_no_value_names_the_key() {
        let err = parse("---\nmode: all\npermission:\n---\nb\n").expect_err("no value");
        assert_eq!(
            err,
            FrontmatterError::MissingValue {
                line: 3,
                key: "permission".to_owned(),
            }
        );
        assert!(err.to_string().contains("permission"), "{err}");
    }

    #[test]
    fn an_anchor_is_refused_rather_than_read_as_a_string() {
        let err = parse("---\nmode: &anchor all\n---\nb\n").expect_err("anchor");
        assert_eq!(
            err,
            FrontmatterError::Unsupported {
                line: 2,
                construct: "an anchor or alias",
            }
        );
    }

    #[test]
    fn an_unbalanced_flow_map_becomes_a_string_via_sanitize_not_a_parse_error() {
        // Verified against opencode 1.18.12. gray-matter fails on the unclosed
        // `{`, retries through `sanitize`, and gets a block scalar — so the value
        // survives as the literal text and is rejected one layer later, by the
        // schema. The binary's own message is
        // `Expected PermissionActionConfig, got "{ edit: deny" permission`,
        // which only makes sense if the YAML layer let the string through.
        let doc = parse("---\npermission: { edit: deny\n---\nb\n").expect("sanitize rescues it");
        assert_eq!(
            Value::Object(doc.data),
            json!({ "permission": "{ edit: deny" })
        );
    }

    #[test]
    fn an_unbalanced_flow_map_with_no_inner_colon_is_a_flow_error() {
        // `sanitize` only rewrites values containing a colon, so this one reaches
        // the flow parser and fails there.
        let err = parse("---\nitems: [one, two\n---\nb\n").expect_err("unbalanced");
        assert!(
            matches!(err, FrontmatterError::Flow { line: 2, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_first_error_is_reported_not_the_sanitized_one() {
        // `sanitize` cannot repair a duplicate key, and the message must still
        // point at the line the author wrote.
        let err = parse("---\na: x: y\na: q: r\n---\nb\n").expect_err("duplicate survives");
        assert!(
            matches!(err, FrontmatterError::DuplicateKey { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn sanitize_leaves_a_value_without_a_colon_untouched() {
        let text = "---\ndescription: plain value\n---\nbody\n";
        assert_eq!(sanitize(text), text);
    }

    #[test]
    fn sanitize_rewrites_only_the_head() {
        let text = "---\na: x: y\n---\nbody has x: y too\n";
        let out = sanitize(text);
        assert!(out.contains("a: |-\n  x: y"), "{out}");
        assert!(out.ends_with("body has x: y too\n"), "{out}");
    }
}
