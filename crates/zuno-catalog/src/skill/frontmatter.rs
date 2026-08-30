//! A `gray-matter`-compatible frontmatter splitter for `SKILL.md`.
//!
//! Port of the two layers the oracle stacks on every skill file:
//! `packages/core/src/config/markdown.ts` (`parse` → `matter`, retry with
//! `sanitize`) and `gray-matter@4`'s own delimiter handling. Only two keys are
//! recognized — `name` and `description` (`skill/index.ts:53-59`) — so this
//! module deliberately does *not* deserialize into a struct: everything else in
//! the block is ignored, exactly as `isSkillFrontmatter` ignores it.
//!
//! # Why the delimiter rules are spelled out here
//!
//! `content` is handed to the model verbatim, and the delimiter arithmetic
//! decides where it starts. Every rule below was measured against
//! `opencode debug skill` 1.18.13 on a crafted fixture rather than read off the
//! `gray-matter` source, because the two disagree in places:
//!
//! | fixture                          | oracle result                       |
//! |----------------------------------|-------------------------------------|
//! | `----\nname: x\n----\n`          | no frontmatter, skill skipped        |
//! | `---\n# comment only\n---\n`     | empty data, skill skipped            |
//! | `---\nname: x\ndesc...` (no close) | frontmatter parsed, `content` empty |
//! | `---\r\n...\r\n---\r\nBody\r\n`  | `content` is `Body\r\n`             |
//! | `---yaml\nname: x\n---\nB\n`     | parsed by the YAML engine           |
//! | `---json\n{"name":"x"}\n---\nB\n`| parsed by the JSON engine           |
//! | `name:` (null)                   | not a string, skill skipped          |
//! | `description:` (null)            | not a string, skill skipped          |
//! | `name: yes`                      | the string `"yes"`, skill loaded     |
//! | `name: true`                     | a boolean, skill skipped             |
//!
//! # Scalar resolution
//!
//! `gray-matter` runs js-yaml 4, whose default schema is YAML 1.2 core: only
//! `true`/`false` (and case variants) are booleans, so `yes` stays a string.
//! [`yaml_rust2`] resolves the same way, which is why it was chosen over
//! `serde_yaml` (libyaml, YAML 1.1, where `yes` *is* a boolean).

use std::fmt;

use yaml_rust2::{Yaml, YamlLoader};

/// The delimiter `gray-matter` uses by default, and the only one skills use.
const DELIMITER: &str = "---";

/// One recognized frontmatter key, resolved the way JavaScript would see it.
///
/// The oracle's guard is `typeof data.name === "string"` and
/// `data.description === undefined || typeof data.description === "string"`
/// (`skill/index.ts:53-59`). Three states are therefore distinguishable and all
/// three matter: absent, a string, and present-but-not-a-string. Collapsing the
/// last two into `None` would silently load a skill the oracle drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// The key is not in the mapping — JavaScript's `undefined`.
    Absent,
    /// The key holds a YAML/JSON string.
    Text(String),
    /// The key is present but is a number, boolean, null, sequence, or mapping.
    NotAString,
}

impl Field {
    /// The string, when this is one.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Absent | Self::NotAString => None,
        }
    }

    /// Whether the key is present but of the wrong type.
    #[must_use]
    pub fn is_wrong_type(&self) -> bool {
        matches!(self, Self::NotAString)
    }
}

/// A split `SKILL.md`: the two recognized keys plus the body, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// The `name` key.
    pub name: Field,
    /// The `description` key.
    pub description: Field,
    /// Everything after the closing delimiter, byte-for-byte
    /// (`skill/index.ts:138` stores `md.content` unchanged).
    pub content: String,
}

/// Why a frontmatter block could not be turned into a mapping.
///
/// Every variant is a case where the oracle throws out of `matter()`, hits the
/// `catch` in `config/markdown.ts`, and publishes a load error for the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterError {
    /// The YAML block is not well-formed, even after the sanitize retry.
    Yaml(String),
    /// A `---json` block is not well-formed JSON.
    Json(String),
    /// A language tag `gray-matter` has no engine for — `---toml`, `---ini`.
    /// `gray-matter` throws `engine "x" is not registered`.
    UnknownEngine(String),
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yaml(detail) => write!(f, "invalid YAML frontmatter: {detail}"),
            Self::Json(detail) => write!(f, "invalid JSON frontmatter: {detail}"),
            Self::UnknownEngine(name) => {
                write!(f, "frontmatter engine \"{name}\" is not registered")
            }
        }
    }
}

impl std::error::Error for FrontmatterError {}

/// Split a markdown source into recognized frontmatter keys and a body.
///
/// A source with no frontmatter is not an error: `gray-matter` returns
/// `{ data: {} }` and the whole file as content, and the oracle then drops the
/// skill for having no `name`. That distinction is the caller's to report, so it
/// gets [`Field::Absent`], not an `Err`.
pub fn parse(source: &str) -> Result<Document, FrontmatterError> {
    let Some(split) = split(source) else {
        return Ok(Document {
            name: Field::Absent,
            description: Field::Absent,
            content: source.to_string(),
        });
    };

    let (name, description) = match split.engine.as_deref() {
        None | Some("yaml") | Some("yml") => parse_yaml(&split.block, source)?,
        Some("json") => parse_json(&split.block)?,
        Some(other) => return Err(FrontmatterError::UnknownEngine(other.to_string())),
    };

    Ok(Document {
        name,
        description,
        content: split.content,
    })
}

/// The raw pieces `gray-matter` carves out before any engine runs.
struct Split {
    /// The language tag on the opening delimiter line, when there is one.
    engine: Option<String>,
    /// The frontmatter block, delimiters excluded.
    block: String,
    /// The body, with at most one leading `\r` and one leading `\n` removed.
    content: String,
}

/// `gray-matter`'s delimiter arithmetic, and only that.
///
/// Returns `None` when the source has no frontmatter at all, which is the two
/// early `return file` branches in `gray-matter`: the source does not open with
/// `---`, or the character right after it is another `-` (so `----` is a
/// horizontal rule, not a delimiter).
fn split(source: &str) -> Option<Split> {
    if !source.starts_with(DELIMITER) {
        return None;
    }
    let rest = &source[DELIMITER.len()..];
    if rest.starts_with('-') {
        return None;
    }

    // `gray-matter` reads to the end of the opening line; anything non-blank
    // there is an engine name.
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let tag = rest[..line_end].trim();
    let (engine, rest) = if tag.is_empty() {
        (None, rest)
    } else {
        (Some(tag.to_lowercase()), &rest[line_end..])
    };

    // The closing delimiter must start a line, so `gray-matter` searches for
    // `"\n---"` rather than `"---"`.
    let close = format!("\n{DELIMITER}");
    match rest.find(&close) {
        // No closing delimiter: the whole remainder is frontmatter and the body
        // is empty. Measured, not inferred -- see the module table.
        None => Some(Split {
            engine,
            block: rest.to_string(),
            content: String::new(),
        }),
        Some(index) => {
            let block = rest[..index].to_string();
            let mut content = &rest[index + close.len()..];
            content = content.strip_prefix('\r').unwrap_or(content);
            content = content.strip_prefix('\n').unwrap_or(content);
            Some(Split {
                engine,
                block,
                content: content.to_string(),
            })
        }
    }
}

/// Whether a block is "empty" by `gray-matter`'s definition: nothing left once
/// full-line comments are stripped and the remainder is trimmed.
///
/// `gray-matter` does this before calling the engine, so a comment-only block
/// yields `data = {}` instead of a parse error.
fn is_blank_block(block: &str) -> bool {
    block
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .all(|line| line.trim().is_empty())
}

fn parse_yaml(block: &str, source: &str) -> Result<(Field, Field), FrontmatterError> {
    if is_blank_block(block) {
        return Ok((Field::Absent, Field::Absent));
    }

    let docs = match YamlLoader::load_from_str(block) {
        Ok(docs) => docs,
        Err(first) => {
            // `config/markdown.ts` retries the *whole file* through `sanitize`,
            // which rewrites unquoted-colon values as block scalars, then splits
            // again. Reproduce that, including the re-split.
            let sanitized = sanitize(source);
            let retry = split(&sanitized).map(|split| split.block);
            match retry.as_deref().map(YamlLoader::load_from_str) {
                Some(Ok(docs)) => docs,
                _ => return Err(FrontmatterError::Yaml(first.to_string())),
            }
        }
    };

    let Some(Yaml::Hash(map)) = docs.first() else {
        // A sequence or bare scalar document fails the oracle's `isRecord`
        // guard, which is indistinguishable from having no keys.
        return Ok((Field::Absent, Field::Absent));
    };

    Ok((yaml_field(map, "name"), yaml_field(map, "description")))
}

fn yaml_field(map: &yaml_rust2::yaml::Hash, key: &str) -> Field {
    match map.get(&Yaml::String(key.to_string())) {
        None => Field::Absent,
        Some(Yaml::String(value)) => Field::Text(value.clone()),
        Some(_) => Field::NotAString,
    }
}

fn parse_json(block: &str) -> Result<(Field, Field), FrontmatterError> {
    if is_blank_block(block) {
        return Ok((Field::Absent, Field::Absent));
    }
    let value: serde_json::Value = serde_json::from_str(block.trim())
        .map_err(|err| FrontmatterError::Json(err.to_string()))?;
    let serde_json::Value::Object(map) = value else {
        return Ok((Field::Absent, Field::Absent));
    };
    Ok((json_field(&map, "name"), json_field(&map, "description")))
}

fn json_field(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Field {
    match map.get(key) {
        None => Field::Absent,
        Some(serde_json::Value::String(value)) => Field::Text(value.clone()),
        Some(_) => Field::NotAString,
    }
}

/// `ConfigMarkdown.sanitize` (`packages/core/src/config/markdown.ts`).
///
/// Other coding agents write `description: Use when: X`, which is invalid YAML.
/// The oracle rewrites exactly those lines as block scalars and reparses. The
/// key pattern is `[a-zA-Z_][a-zA-Z0-9_]*`, so a hyphenated key such as
/// `allowed-tools` is deliberately left alone.
#[must_use]
pub fn sanitize(source: &str) -> String {
    let Some((start, end, block)) = first_block(source) else {
        return source.to_string();
    };

    let rewritten = block
        .split('\n')
        .flat_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() || line.starts_with([' ', '\t']) {
                return vec![line.to_string()];
            }
            let Some((key, value)) = simple_entry(line) else {
                return vec![line.to_string()];
            };
            let value = value.trim();
            if value.is_empty()
                || value == ">"
                || value == "|"
                || value.starts_with('"')
                || value.starts_with('\'')
                || !value.contains(':')
            {
                return vec![line.to_string()];
            }
            vec![format!("{key}: |-"), format!("  {value}")]
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut out = String::with_capacity(source.len() + rewritten.len());
    out.push_str(&source[..start]);
    out.push_str(&rewritten);
    out.push_str(&source[end..]);
    out
}

/// The byte range and text of the first `---`-delimited block, mirroring
/// `sanitize`'s `/^---\r?\n([\s\S]*?)\r?\n---/`.
fn first_block(source: &str) -> Option<(usize, usize, &str)> {
    let after_open = if let Some(rest) = source.strip_prefix("---\r\n") {
        source.len() - rest.len()
    } else {
        let rest = source.strip_prefix("---\n")?;
        source.len() - rest.len()
    };

    let tail = &source[after_open..];
    let (offset, len) = tail
        .match_indices("\n---")
        .map(|(index, _)| (index, 4))
        .chain(tail.match_indices("\r\n---").map(|(index, _)| (index, 5)))
        .min_by_key(|(index, _)| *index)?;
    let _ = len;
    Some((after_open, after_open + offset, &tail[..offset]))
}

/// `/^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$/` — the only lines `sanitize` touches.
fn simple_entry(line: &str) -> Option<(&str, &str)> {
    let mut chars = line.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut end = line.len();
    for (index, ch) in chars {
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            end = index;
            break;
        }
    }
    let key = &line[..end];
    let rest = line[end..].trim_start_matches([' ', '\t']);
    let value = rest.strip_prefix(':')?;
    Some((key, value.trim_start_matches([' ', '\t'])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(source: &str) -> Document {
        parse(source).expect("frontmatter parses")
    }

    #[test]
    fn strips_one_newline_after_the_closing_delimiter() {
        // Measured against the oracle: a blank line between `---` and the body
        // survives as a single leading newline.
        let parsed = doc("---\nname: a\ndescription: d\n---\n\n# Body\n");
        assert_eq!(parsed.content, "\n# Body\n");
        assert_eq!(parsed.name.text(), Some("a"));
        assert_eq!(parsed.description.text(), Some("d"));
    }

    #[test]
    fn crlf_body_keeps_its_carriage_returns() {
        let parsed = doc("---\r\nname: a\r\ndescription: d\r\n---\r\nBody\r\n");
        assert_eq!(parsed.content, "Body\r\n");
        assert_eq!(parsed.description.text(), Some("d"));
    }

    #[test]
    fn four_dashes_is_not_a_delimiter() {
        let parsed = doc("----\nname: a\n----\nB\n");
        assert_eq!(parsed.name, Field::Absent);
        assert_eq!(parsed.content, "----\nname: a\n----\nB\n");
    }

    #[test]
    fn missing_closing_delimiter_empties_the_body() {
        let parsed = doc("---\nname: a\ndescription: d\n");
        assert_eq!(parsed.name.text(), Some("a"));
        assert_eq!(parsed.content, "");
    }

    #[test]
    fn comment_only_block_has_no_keys() {
        let parsed = doc("---\n# just a comment\n---\nB\n");
        assert_eq!(parsed.name, Field::Absent);
    }

    #[test]
    fn null_scalars_are_present_but_not_strings() {
        assert!(doc("---\nname:\n---\nB\n").name.is_wrong_type());
        assert!(
            doc("---\nname: a\ndescription:\n---\nB\n")
                .description
                .is_wrong_type()
        );
    }

    #[test]
    fn yaml_12_core_schema_keeps_yes_a_string() {
        assert_eq!(doc("---\nname: yes\n---\nB\n").name.text(), Some("yes"));
        assert!(doc("---\nname: true\n---\nB\n").name.is_wrong_type());
        assert!(doc("---\nname: 123\n---\nB\n").name.is_wrong_type());
        assert!(
            doc("---\nname: a\ndescription: 42\n---\nB\n")
                .description
                .is_wrong_type()
        );
    }

    #[test]
    fn folded_scalars_fold_the_way_the_oracle_reports() {
        let parsed = doc(
            "---\nname: f\ndescription: >\n  line one\n  line two\n\n  para two\n---\nBody here\n",
        );
        assert_eq!(
            parsed.description.text(),
            Some("line one line two\npara two\n")
        );
        assert_eq!(parsed.content, "Body here\n");
    }

    #[test]
    fn unquoted_colon_survives_the_sanitize_retry() {
        let parsed =
            doc("---\nname: coloned\ndescription: Use when: you need X. Also: Y\n---\nB\n");
        assert_eq!(
            parsed.description.text(),
            Some("Use when: you need X. Also: Y")
        );
    }

    #[test]
    fn sequence_document_has_no_keys() {
        assert_eq!(doc("---\n- a\n- b\n---\nB\n").name, Field::Absent);
    }

    #[test]
    fn unknown_keys_are_ignored_not_rejected() {
        let parsed = doc(
            "---\nname: extra\ndescription: d\nlicense: MIT\nallowed-tools: [shell]\nversion: 2\n---\nB\n",
        );
        assert_eq!(parsed.name.text(), Some("extra"));
        assert_eq!(parsed.description.text(), Some("d"));
    }

    #[test]
    fn json_engine_is_supported() {
        let parsed = doc("---json\n{\"name\":\"j\",\"description\":\"d\"}\n---\nB\n");
        assert_eq!(parsed.name.text(), Some("j"));
        assert_eq!(parsed.content, "B\n");
    }

    #[test]
    fn yaml_language_tag_is_supported() {
        let parsed = doc("---yaml\nname: y\ndescription: d\n---\nB\n");
        assert_eq!(parsed.name.text(), Some("y"));
    }

    #[test]
    fn unregistered_engine_is_an_error() {
        assert_eq!(
            parse("---toml\nname = \"t\"\n---\nB\n"),
            Err(FrontmatterError::UnknownEngine("toml".to_string()))
        );
    }

    #[test]
    fn broken_yaml_is_an_error() {
        let err = parse("---\nname: [unclosed\n  - nope: : :\n---\nB\n").expect_err("must fail");
        assert!(matches!(err, FrontmatterError::Yaml(_)), "{err:?}");
    }

    #[test]
    fn sanitize_leaves_hyphenated_keys_and_quoted_values_alone() {
        let source = "---\nallowed-tools: a: b\nquoted: \"x: y\"\nfolded: >\n  a: b\n---\nB\n";
        assert_eq!(sanitize(source), source);
    }

    #[test]
    fn sanitize_rewrites_only_the_offending_line() {
        let sanitized = sanitize("---\nname: n\ndescription: a: b\n---\nB\n");
        assert_eq!(sanitized, "---\nname: n\ndescription: |-\n  a: b\n---\nB\n");
    }
}
