//! `{env:VAR}` and `{file:path}` substitution over raw config text.
//!
//! Oracle: `packages/opencode/src/config/variable.ts:33-90`. Call sites:
//! `packages/opencode/src/config/config.ts:213-227` (`opencode.json`, default
//! `missing: "error"`) and `packages/opencode/src/config/tui.ts:100-103`
//! (`tui.json`, `missing: "empty"`).
//!
//! # Substitution happens on text, not on values
//!
//! The oracle runs `substitute` on the file's **bytes** and hands the result to
//! `ConfigParse.jsonc`, so a token is expanded wherever it appears — inside a key,
//! inside a string, spanning a quote, even spanning a line break. Substituting
//! parsed values instead would change which tokens are visible and how a
//! multi-line file body lands in the document, so [`Substitution::apply`] takes
//! and returns text.
//!
//! # The two passes are not symmetric
//!
//! `{env:...}` is a single unconditional regex replace over the whole text.
//! `{file:...}` is then scanned over the *result*, and only that second pass
//! skips `//` comment lines. Both halves of that asymmetry are load-bearing and
//! both were measured against the TypeScript module (see the table in
//! [`Substitution::apply`]):
//!
//! * `{env:FOO}` inside a `// comment` **is** substituted.
//! * An `{env:...}` whose value contains `{file:...}` **does** get the file read,
//!   because the file pass runs over the already-expanded text.
//! * A file body is not rescanned for either token.
//!
//! # Comment-skip rule, stated exactly
//!
//! For a `{file:...}` token at byte offset `i`: take the text from the start of
//! `i`'s line (the byte after the previous `\n`, or 0) up to `i`, drop leading
//! JavaScript whitespace, and skip the token when what remains starts with `//`.
//! Consequences, all verified:
//!
//! | text | `{file:}` substituted? |
//! | --- | --- |
//! | `  // {file:x}` | no |
//! | `/// {file:x}` | no |
//! | `\u{feff}// {file:x}` | no — `trimStart` drops U+FEFF |
//! | `{"a":1} // {file:x}` | **yes** — a trailing comment is not a comment line |
//! | `{"a":"// {file:x}"}` | **yes** — `//` inside a string value is not either |
//! | `/* {file:x} */` | **yes** — block comments are not recognized at all |
//! | `{\r  // {file:x}\r}` | **yes** — the scan looks for `\n`, so a CR-only line ending hides nothing |
//! | `// {file:a} and {file:b}` | no, for both — every token on the line is skipped |
//!
//! A skipped token is never read, so a missing file inside a comment cannot fail
//! the load.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use zuno_error::{ConfigError, ConfigIssue};
use zuno_paths::env::{Env, HOME};
use zuno_paths::node_path;

/// The opening of an `{env:NAME}` token.
const ENV_PREFIX: &str = "{env:";
/// The opening of a `{file:PATH}` token.
const FILE_PREFIX: &str = "{file:";

/// What to do when a `{file:...}` target cannot be read.
///
/// The oracle's `missing` option (`variable.ts:34,72`). It covers *every* read
/// failure, not just an absent file: a `{file:}` pointing at a directory is
/// swallowed by [`Missing::Empty`] exactly as an absent one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Missing {
    /// Fail the load, naming the token and the path. The oracle's default, used
    /// for `opencode.json`.
    #[default]
    Error,
    /// Substitute nothing and carry on. Used for `tui.json`.
    Empty,
}

/// Where the text came from, which decides both the base for relative
/// `{file:...}` paths and the path named in an error.
///
/// The oracle's `ParseSource` (`variable.ts:5-16`).
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// A config file on disk. Relative paths resolve against its **directory**,
    /// never the process working directory.
    File(&'a Path),
    /// Text with no file of its own — a remote config body. `label` names it in
    /// errors and `dir` is the base for relative paths.
    Virtual {
        /// The name to report as the failing config "path".
        label: &'a str,
        /// The directory relative `{file:...}` paths resolve against.
        dir: &'a Path,
    },
}

impl<'a> Source<'a> {
    /// The oracle's `dir(input)` (`variable.ts:29-31`).
    fn dir(&self) -> String {
        match *self {
            Self::File(path) => node_path::dirname(&path.to_string_lossy()),
            Self::Virtual { dir, .. } => dir.to_string_lossy().into_owned(),
        }
    }

    /// The oracle's `source(input)` (`variable.ts:25-27`), as the `path` an error
    /// reports. A virtual label is not a filesystem path, but the oracle reports
    /// it in the same field, so it is carried in the same field here.
    fn error_path(&self) -> PathBuf {
        match *self {
            Self::File(path) => path.to_path_buf(),
            Self::Virtual { label, .. } => PathBuf::from(label),
        }
    }
}

/// One configured substitution pass.
///
/// Build it with [`Substitution::for_file`] or [`Substitution::for_virtual`],
/// adjust it, then call [`apply`](Substitution::apply):
///
/// ```
/// use zuno_config::variable::Substitution;
/// use zuno_paths::env::Env;
/// use std::path::Path;
///
/// let env = Env::empty().with("ZUNO_SAMPLE_MODEL", "anthropic/claude-sonnet-4-5");
/// let text = r#"{"model": "{env:ZUNO_SAMPLE_MODEL}"}"#;
/// let out = Substitution::for_file(Path::new("/repo/opencode.json"))
///     .with_env(&env)
///     .apply(text)
///     .expect("no file tokens, so nothing can fail");
/// assert_eq!(out, r#"{"model": "anthropic/claude-sonnet-4-5"}"#);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Substitution<'a> {
    source: Source<'a>,
    missing: Missing,
    env: Option<&'a Env>,
    process_env: Option<&'a Env>,
}

impl<'a> Substitution<'a> {
    /// Substitute the text of a config file on disk.
    #[must_use]
    pub const fn for_file(path: &'a Path) -> Self {
        Self::new(Source::File(path))
    }

    /// Substitute text that has no file of its own, resolving relative
    /// `{file:...}` paths against `dir` and reporting `label` on failure.
    #[must_use]
    pub const fn for_virtual(label: &'a str, dir: &'a Path) -> Self {
        Self::new(Source::Virtual { label, dir })
    }

    /// Substitute text from an explicit [`Source`].
    #[must_use]
    pub const fn new(source: Source<'a>) -> Self {
        Self {
            source,
            missing: Missing::Error,
            env: None,
            process_env: None,
        }
    }

    /// Supply the oracle's `input.env`: consulted first, before the process
    /// environment. A name present here with an empty value shadows a non-empty
    /// process value, because the oracle reaches for `??` and an empty string is
    /// not nullish.
    #[must_use]
    pub const fn with_env(mut self, env: &'a Env) -> Self {
        self.env = Some(env);
        self
    }

    /// Stand in for `process.env` — and, through `HOME`, for `os.homedir()`.
    ///
    /// Mutating the real environment is `unsafe` and forbidden in this workspace,
    /// so a test that needs a specific `HOME` (to exercise `{file:~/...}`) or a
    /// specific fallback variable injects it here instead. Left unset, the real
    /// process environment is used, snapshotted once.
    #[must_use]
    pub const fn with_process_env(mut self, env: &'a Env) -> Self {
        self.process_env = Some(env);
        self
    }

    /// Choose what an unreadable `{file:...}` target does. Defaults to
    /// [`Missing::Error`], as the oracle does.
    #[must_use]
    pub const fn on_missing(mut self, missing: Missing) -> Self {
        self.missing = missing;
        self
    }

    /// Expand every `{env:...}` and `{file:...}` token in `text`.
    ///
    /// Returns the expanded text. The only failure is an unreadable
    /// `{file:...}` target under [`Missing::Error`], reported as
    /// [`ConfigError::Invalid`] naming both the token and the resolved path.
    ///
    /// Tokens are matched textually, exactly as the oracle's two regexes do, so
    /// none of the following is an error — each was measured against the
    /// TypeScript module and each is reproduced:
    ///
    /// | text | result |
    /// | --- | --- |
    /// | `{env:ABSENT}` | `` — a missing variable is an empty string |
    /// | `{env:}` | `{env:}` — `[^}]+` needs a name, so this is not a token |
    /// | `{env:FOO` | unchanged — no closing brace, no token |
    /// | `{"a":"{env:FOO"}` | `{"a":"` — the match runs past the quote to the next `}` |
    /// | `{env:{env:A}}` | value of the variable literally named `{env:A`, then `}` |
    /// | `{file:}` | `{file:}` — likewise not a token |
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when a `{file:...}` target cannot be read and
    /// `missing` is [`Missing::Error`].
    pub fn apply(&self, text: &str) -> Result<String, ConfigError> {
        let expanded = self.apply_env(text);
        // The oracle returns early when no file token is present (`variable.ts:41`),
        // which matters only for allocation, not for the result.
        if expanded.contains(FILE_PREFIX) {
            self.apply_files(&expanded)
        } else {
            Ok(expanded)
        }
    }

    /// `variable.ts:36-39` — `text.replace(/\{env:([^}]+)\}/g, ...)`.
    fn apply_env(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(ENV_PREFIX) {
            // `find` returns a char boundary and `ENV_PREFIX` is ASCII, so
            // `body` starts on a boundary too. The same holds for every offset
            // below: each is a boundary plus the length of an ASCII literal.
            let body = &rest[start + ENV_PREFIX.len()..];
            match body.find('}') {
                // `[^}]+` requires at least one character, so `{env:}` is not a
                // match. A regex engine that fails here advances one character
                // and keeps scanning; emitting the prefix and resuming after it
                // has the same effect, since no shorter overlapping prefix
                // exists.
                Some(0) | None => {
                    out.push_str(&rest[..start + ENV_PREFIX.len()]);
                    rest = body;
                }
                Some(end) => {
                    out.push_str(&rest[..start]);
                    out.push_str(self.env_value(&body[..end]));
                    rest = &body[end + 1..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// `variable.ts:37-38` — `(input.env?.[name] ?? process.env[name]) || ""`.
    fn env_value(&self, name: &str) -> &str {
        self.env
            .and_then(|env| env.value(name))
            .or_else(|| self.process().value(name))
            .unwrap_or("")
    }

    /// `variable.ts:41-88` — the `{file:...}` pass, with the comment skip.
    fn apply_files(&self, text: &str) -> Result<String, ConfigError> {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        while let Some(offset) = text[cursor..].find(FILE_PREFIX) {
            let start = cursor + offset;
            let body_start = start + FILE_PREFIX.len();
            let Some(end) = text[body_start..].find('}') else {
                // No closing brace remains anywhere ahead, so no later token can
                // complete either: every candidate after this point would need a
                // `}` that this search would have found.
                break;
            };
            if end == 0 {
                // `{file:}` is not a token; resume scanning after the prefix.
                out.push_str(&text[cursor..body_start]);
                cursor = body_start;
                continue;
            }
            let token_end = body_start + end + 1;
            let token = &text[start..token_end];
            out.push_str(&text[cursor..start]);
            if starts_a_comment_line(text, start) {
                out.push_str(token);
            } else if let Some(content) =
                self.read_reference(&text[body_start..body_start + end], token)?
            {
                out.push_str(&escape_json_string_body(&content));
            }
            cursor = token_end;
        }
        out.push_str(&text[cursor..]);
        Ok(out)
    }

    /// Read one `{file:...}` target, trimmed. `Ok(None)` is
    /// [`Missing::Empty`] swallowing a read failure.
    fn read_reference(&self, spec: &str, token: &str) -> Result<Option<String>, ConfigError> {
        let resolved = self.resolve_reference(spec);
        match std::fs::read(&resolved) {
            // `readFile(p, "utf-8")` replaces invalid bytes rather than failing,
            // so a file that is not UTF-8 yields U+FFFD, not an error.
            Ok(bytes) => Ok(Some(js_trim(&String::from_utf8_lossy(&bytes)).to_owned())),
            Err(_) if self.missing == Missing::Empty => Ok(None),
            Err(error) => Err(self.bad_reference(token, &resolved, &error)),
        }
    }

    /// `variable.ts:64-70`. Three shapes, and the differences between them are
    /// observable in the path an error reports:
    ///
    /// * `~/x` — `path.join(homedir, "x")`, which **normalizes**.
    /// * an absolute path — used **verbatim**, `..` and `//` and all. The oracle
    ///   hands it to `readFile` unchanged, so `{file:/a/../b}` reports
    ///   `/a/../b`, and the `..` is resolved by the kernel (symlink-aware)
    ///   rather than textually.
    /// * anything else — `path.resolve(configDir, spec)`, which normalizes.
    fn resolve_reference(&self, spec: &str) -> PathBuf {
        let expanded = match spec.strip_prefix("~/") {
            Some(rest) => node_path::join(self.home(), rest),
            None => spec.to_owned(),
        };
        if node_path::is_absolute(&expanded) {
            return PathBuf::from(expanded);
        }
        PathBuf::from(node_path::resolve(&self.source.dir(), &[&expanded]))
    }

    /// `variable.ts:74-86`. Only an absent file earns the `does not exist`
    /// suffix; every other read failure reports the token alone, which is what
    /// the oracle's `ENOENT` check produces.
    fn bad_reference(&self, token: &str, resolved: &Path, error: &std::io::Error) -> ConfigError {
        let mut detail = format!("bad file reference: \"{token}\"");
        if error.kind() == std::io::ErrorKind::NotFound {
            detail.push(' ');
            detail.push_str(&resolved.to_string_lossy());
            detail.push_str(" does not exist");
        }
        ConfigError::Invalid {
            path: self.source.error_path(),
            // The oracle sets `message` and leaves `issues` unset here
            // (`variable.ts:76-85`): the fault is the file's, not a key's. An
            // empty key path is how that "nowhere in particular" is spelled.
            issues: vec![ConfigIssue::new(Vec::<String>::new(), detail)],
        }
    }

    /// `os.homedir()` as `variable.ts:66` uses it — `$HOME` when set and
    /// non-empty.
    ///
    /// Deliberately *not* `zuno_paths::home()`: that is `Global.Path.home`, which
    /// `ZUNO_TEST_HOME` overrides. `variable.ts` calls `os.homedir()`
    /// directly and never sees that override.
    ///
    /// With no usable `HOME`, Node falls through to the password database; there
    /// is no dependency here that can, so this yields `""`. That is not a
    /// silent wrong answer: `path.join("", "x")` is `"x"`, so the reference
    /// simply stays relative and resolves against the config directory — the
    /// same thing Node does when `os.homedir()` returns an empty string.
    fn home(&self) -> &str {
        self.process().truthy_value(HOME).unwrap_or("")
    }

    /// The stand-in for `process.env`, snapshotted once when not injected. The
    /// environment cannot change under it: mutating it is `unsafe`, and this
    /// workspace forbids `unsafe`.
    fn process(&self) -> &Env {
        static PROCESS: OnceLock<Env> = OnceLock::new();
        self.process_env
            .unwrap_or_else(|| PROCESS.get_or_init(Env::from_process))
    }
}

/// Is the `{file:...}` token at `index` on a line that is a `//` comment?
///
/// `variable.ts:53-55`. The search is for `\n` specifically, so a lone `\r` does
/// not start a new line here.
fn starts_a_comment_line(text: &str, index: usize) -> bool {
    let line_start = text[..index].rfind('\n').map_or(0, |newline| newline + 1);
    js_trim_start(&text[line_start..index]).starts_with("//")
}

/// `JSON.stringify(s).slice(1, -1)` — the content escaped as the *body* of a JSON
/// string, so that dropping it between two existing quotes still leaves valid
/// JSON.
///
/// A file holding `He said "hi"` would otherwise close the string it was
/// substituted into and corrupt the rest of the document.
fn escape_json_string_body(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for character in content.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            // Everything else below U+0020 has no short form. U+007F and the
            // non-ASCII control characters are *not* escaped by
            // `JSON.stringify`, and are not escaped here either.
            control if control < '\u{20}' => {
                out.push_str("\\u00");
                let byte = control as u32;
                out.push(char::from_digit(byte >> 4, 16).unwrap_or('0'));
                out.push(char::from_digit(byte & 0xf, 16).unwrap_or('0'));
            }
            other => out.push(other),
        }
    }
    out
}

/// The characters `String.prototype.trim` removes: ECMA-262 `WhiteSpace` plus
/// `LineTerminator`.
///
/// This is deliberately not [`char::is_whitespace`], which implements the Unicode
/// `White_Space` property. The two differ at both ends, and both differences show
/// up in real files:
///
/// * **U+FEFF** (byte-order mark) is JavaScript whitespace but not Unicode
///   `White_Space`. A prompt file saved with a BOM has it trimmed by the oracle,
///   and a `\u{feff}// {file:x}` line is a comment to the oracle.
/// * **U+0085** (NEL) is Unicode `White_Space` but not JavaScript whitespace, so
///   the oracle keeps it.
const fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        // WhiteSpace: TAB, VT, FF, SP, NBSP, ZWNBSP …
        '\u{9}' | '\u{b}' | '\u{c}' | '\u{20}' | '\u{a0}' | '\u{feff}'
        // … and the rest of Unicode Space_Separator.
        | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
        // LineTerminator: LF, CR, LS, PS.
        | '\u{a}' | '\u{d}' | '\u{2028}' | '\u{2029}'
    )
}

/// `String.prototype.trim`.
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// `String.prototype.trimStart`.
fn js_trim_start(text: &str) -> &str {
    text.trim_start_matches(is_js_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A config directory with the fixture files the tests read, plus a fake
    /// `HOME` that is *not* the real one, so `{file:~/...}` is exercised without
    /// touching the developer's home directory.
    struct Fixture {
        root: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().expect("temp dir");
            fs::create_dir_all(root.path().join("cfg")).expect("cfg dir");
            fs::create_dir_all(root.path().join("home")).expect("home dir");
            let fixture = Self { root };
            fixture.write("cfg/rel.md", "  relative-content  \n");
            fixture.write("outside.md", "abs-content\n");
            fixture.write("home/notes.md", "  home-content  \n");
            fixture
        }

        fn write(&self, relative: &str, content: &str) {
            fs::write(self.root.path().join(relative), content).expect("write fixture");
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.root.path().join(relative)
        }

        fn config(&self) -> PathBuf {
            self.path("cfg/opencode.json")
        }

        fn home_env(&self) -> Env {
            Env::empty().with(HOME, self.path("home").to_string_lossy())
        }
    }

    /// A substitution rooted at the fixture's config file, with an empty process
    /// environment so nothing ambient can leak in.
    fn at<'a>(fixture: &'a Fixture, process: &'a Env) -> Substitution<'a> {
        Substitution::for_file(leak(fixture.config())).with_process_env(process)
    }

    /// `Substitution` borrows its path; tests build owned ones. Leaking a handful
    /// of `PathBuf`s in a test binary is cheaper than threading lifetimes through
    /// every case.
    fn leak(path: PathBuf) -> &'static Path {
        Box::leak(path.into_boxed_path())
    }

    fn detail(error: &ConfigError) -> String {
        let ConfigError::Invalid { issues, .. } = error else {
            panic!("expected Invalid, got {error:?}");
        };
        assert_eq!(issues.len(), 1, "one issue per bad reference");
        assert!(
            issues[0].key_path.is_empty(),
            "a bad file reference has no key path"
        );
        issues[0].detail.clone()
    }

    // ---- {env:VAR} ----------------------------------------------------------

    #[test]
    fn env_token_is_replaced_from_the_injected_map() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("ZUNO_SAMPLE_MODEL", "anthropic/claude-sonnet-4-5");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply(r#"{"model":"{env:ZUNO_SAMPLE_MODEL}"}"#)
                .expect("no file tokens"),
            r#"{"model":"anthropic/claude-sonnet-4-5"}"#
        );
    }

    #[test]
    fn env_falls_back_to_the_process_environment() {
        let fixture = Fixture::new();
        let process = Env::empty().with("ZUNO_SAMPLE_MODEL", "from-process");
        let env = Env::empty().with("OTHER", "x");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply("{env:ZUNO_SAMPLE_MODEL}")
                .expect("no file tokens"),
            "from-process"
        );
    }

    #[test]
    fn an_injected_empty_value_shadows_the_process_environment() {
        // `??` does not fall through on an empty string, so the injected "" wins
        // and `|| ""` then makes it empty. Measured: `env-empty-injected-shadows-proc`.
        let fixture = Fixture::new();
        let process = Env::empty().with("ZUNO_SAMPLE_MODEL", "from-process");
        let env = Env::empty().with("ZUNO_SAMPLE_MODEL", "");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply("[{env:ZUNO_SAMPLE_MODEL}]")
                .expect("no file tokens"),
            "[]"
        );
    }

    #[test]
    fn a_missing_env_variable_becomes_an_empty_string() {
        let fixture = Fixture::new();
        let process = Env::empty();
        assert_eq!(
            at(&fixture, &process)
                .apply(r#"{"model":"{env:ZUNO_ABSENT}"}"#)
                .expect("a missing variable is not an error"),
            r#"{"model":""}"#
        );
    }

    #[test]
    fn env_tokens_in_a_comment_line_are_substituted_too() {
        // The oracle's env pass has no comment check at all — only the file pass
        // does. Measured: `env-in-line-comment` gives "{\n  // v1\n  \"a\":1\n}".
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("FOO", "v1");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply("{\n  // {env:FOO}\n  \"a\":1\n}")
                .expect("no file tokens"),
            "{\n  // v1\n  \"a\":1\n}"
        );
    }

    #[test]
    fn malformed_env_tokens_are_left_alone() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let subject = at(&fixture, &process);
        // `[^}]+` needs a name.
        assert_eq!(subject.apply("{env:}").expect("literal"), "{env:}");
        // No closing brace, no match.
        assert_eq!(subject.apply("{env:FOO").expect("literal"), "{env:FOO");
        // Two unterminated openings in a row still emit unchanged.
        assert_eq!(
            subject.apply("{env:{env:A").expect("literal"),
            "{env:{env:A"
        );
        // `{env:}` does not hide a real token that follows it.
        let env = Env::empty().with("A", "ok");
        assert_eq!(
            subject.with_env(&env).apply("{env:}{env:A}").expect("both"),
            "{env:}ok"
        );
    }

    #[test]
    fn an_env_match_runs_past_a_closing_quote_to_the_next_brace() {
        // Purely textual: `{"a":"{env:FOO"}` matches `{env:FOO"}` with the name
        // `FOO"`. Measured: `env-malformed-noclose` gives "{\"a\":\"".
        let fixture = Fixture::new();
        let process = Env::empty();
        assert_eq!(
            at(&fixture, &process)
                .apply(r#"{"a":"{env:FOO"}"#)
                .expect("no file tokens"),
            r#"{"a":""#
        );
    }

    #[test]
    fn a_nested_env_token_resolves_the_inner_name_literally() {
        // Greedy `[^}]+` stops at the first `}`, so the name is `{env:A`.
        // Measured: `env-nested` gives "{\"a\":\"INNER}\"}".
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("{env:A", "INNER");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply(r#"{"a":"{env:{env:A}}"}"#)
                .expect("no file tokens"),
            r#"{"a":"INNER}"}"#
        );
    }

    #[test]
    fn an_env_token_may_span_a_line_break() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("OC\nX", "MULTI");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply("{env:OC\nX}")
                .expect("no file tokens"),
            "MULTI"
        );
    }

    // ---- {file:path} resolution ---------------------------------------------

    #[test]
    fn a_relative_file_path_resolves_against_the_config_directory() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let subject = at(&fixture, &process);
        for token in ["{file:./rel.md}", "{file:rel.md}"] {
            assert_eq!(
                subject.apply(token).expect("fixture exists"),
                "relative-content",
                "{token} should resolve next to the config file"
            );
        }
    }

    #[test]
    fn a_relative_file_path_ignores_the_working_directory() {
        // The proof that the base is the config file's directory and not the
        // process cwd: `rel.md` exists only under `cfg/`, and the test runs from
        // the crate root. A cwd-relative resolution would fail to find it, and a
        // config rooted somewhere else must not find it either.
        let fixture = Fixture::new();
        let process = Env::empty();
        let cwd = std::env::current_dir().expect("cwd");
        assert!(
            !cwd.join("rel.md").exists(),
            "the test would not prove anything if rel.md existed in the cwd"
        );
        assert_eq!(
            at(&fixture, &process)
                .apply("{file:rel.md}")
                .expect("found next to the config file"),
            "relative-content"
        );

        let elsewhere = Substitution::for_file(leak(fixture.path("opencode.json")))
            .with_process_env(&process)
            .apply("{file:rel.md}")
            .expect_err("a config one directory up must not see cfg/rel.md");
        assert!(
            detail(&elsewhere).contains(&format!(
                "{} does not exist",
                fixture.path("rel.md").display()
            )),
            "resolved against the wrong directory: {}",
            detail(&elsewhere)
        );
    }

    #[test]
    fn an_absolute_file_path_is_read_as_written() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let absolute = fixture.path("outside.md");
        assert_eq!(
            at(&fixture, &process)
                .apply(&format!("{{file:{}}}", absolute.display()))
                .expect("fixture exists"),
            "abs-content"
        );
    }

    #[test]
    fn an_absolute_path_is_not_normalized_before_it_is_reported() {
        // `path.isAbsolute(p) ? p : path.resolve(...)` hands an absolute path
        // to `readFile` untouched, so the error shows the `..` and the `//`.
        // Measured: `abs-unnormalized`, `abs-double-slash`.
        let fixture = Fixture::new();
        let process = Env::empty();
        let raw = format!("{}/x/../../absent.md", fixture.path("cfg").display());
        let error = at(&fixture, &process)
            .apply(&format!("{{file:{raw}}}"))
            .expect_err("absent");
        assert_eq!(
            detail(&error),
            format!("bad file reference: \"{{file:{raw}}}\" {raw} does not exist")
        );
    }

    #[test]
    fn a_tilde_path_expands_against_the_home_directory() {
        let fixture = Fixture::new();
        let process = fixture.home_env();
        assert_eq!(
            at(&fixture, &process)
                .apply(r#"{"instructions":["{file:~/notes.md}"]}"#)
                .expect("fixture exists"),
            r#"{"instructions":["home-content"]}"#
        );
    }

    #[test]
    fn tilde_expansion_normalizes_but_a_bare_tilde_does_not_expand() {
        let fixture = Fixture::new();
        let process = fixture.home_env();
        // `path.join(home, "../absent.md")` normalizes away the `..`.
        let error = at(&fixture, &process)
            .apply("{file:~/../absent.md}")
            .expect_err("absent");
        assert_eq!(
            detail(&error),
            format!(
                "bad file reference: \"{{file:~/../absent.md}}\" {} does not exist",
                fixture.path("absent.md").display()
            )
        );
        // Only the `~/` prefix is special; a bare `~` is an ordinary relative name.
        // Measured: `file-tilde-bare`.
        let bare = at(&fixture, &process)
            .apply("{file:~}")
            .expect_err("absent");
        assert_eq!(
            detail(&bare),
            format!(
                "bad file reference: \"{{file:~}}\" {} does not exist",
                fixture.path("cfg/~").display()
            )
        );
    }

    #[test]
    fn without_a_usable_home_a_tilde_path_stays_relative() {
        // `os.homedir()` returning "" makes `path.join("", "notes.md")` just
        // "notes.md", which then resolves against the config directory. An empty
        // HOME is the same case, because `os.homedir()` treats it as unset.
        let fixture = Fixture::new();
        for process in [Env::empty(), Env::empty().with(HOME, "")] {
            let error = at(&fixture, &process)
                .apply("{file:~/notes.md}")
                .expect_err("absent");
            assert_eq!(
                detail(&error),
                format!(
                    "bad file reference: \"{{file:~/notes.md}}\" {} does not exist",
                    fixture.path("cfg/notes.md").display()
                )
            );
        }
    }

    #[test]
    fn a_virtual_source_resolves_against_its_directory_and_is_named_in_errors() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let dir = fixture.path("cfg");
        let subject =
            Substitution::for_virtual("remote:opencode.json", &dir).with_process_env(&process);
        assert_eq!(
            subject.apply("{file:./rel.md}").expect("fixture exists"),
            "relative-content"
        );
        let error = subject.apply("{file:./absent.md}").expect_err("absent");
        let ConfigError::Invalid { path, .. } = &error else {
            panic!("expected Invalid");
        };
        assert_eq!(path, Path::new("remote:opencode.json"));
    }

    // ---- comment skipping ---------------------------------------------------

    #[test]
    fn file_tokens_on_a_comment_line_are_left_untouched() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let subject = at(&fixture, &process);
        for text in [
            "{\n  // {file:./rel.md}\n}",
            "{\n\t\t// {file:./rel.md}\n}",
            "{\n/// {file:./rel.md}\n}",
            "{\n\u{feff}// {file:./rel.md}\n}",
            "{\n\u{a0}// {file:./rel.md}\n}",
            "{\r\n  // {file:./rel.md}\r\n}",
            "{\n//  x \"{file:./rel.md}\"\n}",
        ] {
            assert_eq!(
                subject.apply(text).expect("nothing is read"),
                text,
                "should have been skipped: {text:?}"
            );
        }
    }

    #[test]
    fn every_file_token_on_a_comment_line_is_skipped() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let text = format!(
            "{{\n// {{file:./rel.md}} and {{file:{}}}\n}}",
            fixture.path("outside.md").display()
        );
        assert_eq!(
            at(&fixture, &process)
                .apply(&text)
                .expect("nothing is read"),
            text
        );
    }

    #[test]
    fn a_missing_file_inside_a_comment_cannot_fail_the_load() {
        let fixture = Fixture::new();
        let process = Env::empty();
        assert_eq!(
            at(&fixture, &process)
                .apply("{\"a\":1,\n// {file:/nonexistent/q.md}\n\"b\":2}")
                .expect("a skipped token is never read"),
            "{\"a\":1,\n// {file:/nonexistent/q.md}\n\"b\":2}"
        );
    }

    #[test]
    fn only_a_comment_at_the_start_of_the_line_skips() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let subject = at(&fixture, &process);
        // A trailing comment is not a comment *line*.
        assert_eq!(
            subject
                .apply("{\"a\":1} // {file:./rel.md}")
                .expect("substituted"),
            "{\"a\":1} // relative-content"
        );
        // `//` inside a string value does not make the line a comment either.
        assert_eq!(
            subject
                .apply(r#"{"a":"// {file:./rel.md}"}"#)
                .expect("substituted"),
            r#"{"a":"// relative-content"}"#
        );
        // Block comments are not recognized at all.
        assert_eq!(
            subject.apply("/* {file:./rel.md} */").expect("substituted"),
            "/* relative-content */"
        );
        // A single slash is not a comment.
        assert_eq!(
            subject
                .apply("{\n / {file:./rel.md}\n}")
                .expect("substituted"),
            "{\n / relative-content\n}"
        );
        // The line scan looks for `\n`; a lone `\r` does not start a line.
        assert_eq!(
            subject
                .apply("{\r  // {file:./rel.md}\r}")
                .expect("substituted"),
            "{\r  // relative-content\r}"
        );
    }

    // ---- reading, trimming, escaping ---------------------------------------

    #[test]
    fn file_content_is_trimmed_and_json_escaped_so_the_document_still_parses() {
        let fixture = Fixture::new();
        let process = Env::empty();
        fixture.write(
            "cfg/nasty.md",
            "  He said \"hi\"\\path\ttab\r\nline2\u{1}ctl\u{b}vt  ",
        );
        let out = at(&fixture, &process)
            .apply(r#"{"instructions":["{file:./nasty.md}"]}"#)
            .expect("fixture exists");
        assert_eq!(
            out,
            r#"{"instructions":["He said \"hi\"\\path\ttab\r\nline2\u0001ctl\u000bvt"]}"#
        );
        let parsed = serde_json::from_str::<serde_json::Value>(&out)
            .expect("substitution must leave valid JSON");
        assert_eq!(
            parsed["instructions"][0],
            serde_json::json!("He said \"hi\"\\path\ttab\r\nline2\u{1}ctl\u{b}vt")
        );
    }

    #[test]
    fn trimming_follows_javascript_and_not_unicode_white_space() {
        let fixture = Fixture::new();
        let process = Env::empty();
        // Every character `String.prototype.trim` removes, on both sides.
        let padding = "\u{9}\u{b}\u{c}\u{20}\u{a0}\u{feff}\u{1680}\u{2000}\u{200a}\u{202f}\u{205f}\u{3000}\u{a}\u{d}\u{2028}\u{2029}";
        // U+0085 is Unicode White_Space but not JavaScript whitespace, so it
        // survives at the edge where `char::is_whitespace` would have eaten it.
        fixture.write("cfg/ws.md", &format!("{padding}\u{85}X\u{85}{padding}"));
        assert_eq!(
            at(&fixture, &process)
                .apply("{file:./ws.md}")
                .expect("fixture exists"),
            "\u{85}X\u{85}"
        );
        assert!(
            '\u{85}'.is_whitespace() && !is_js_whitespace('\u{85}'),
            "the delta this test exists for"
        );
        assert!(
            !'\u{feff}'.is_whitespace() && is_js_whitespace('\u{feff}'),
            "the other delta"
        );
    }

    #[test]
    fn a_file_of_only_whitespace_substitutes_nothing() {
        let fixture = Fixture::new();
        let process = Env::empty();
        fixture.write("cfg/blank.md", " \n\t ");
        assert_eq!(
            at(&fixture, &process)
                .apply("A{file:./blank.md}B")
                .expect("fixture exists"),
            "AB"
        );
    }

    #[test]
    fn invalid_utf8_is_replaced_rather_than_rejected() {
        // `readFile(p, "utf-8")` is lossy. Measured: `badutf8` gives "A\u{fffd}\u{fffd}B".
        let fixture = Fixture::new();
        let process = Env::empty();
        fs::write(fixture.path("cfg/bad.md"), [0x41, 0xff, 0xfe, 0x42]).expect("write");
        assert_eq!(
            at(&fixture, &process)
                .apply("{file:./bad.md}")
                .expect("not an error"),
            "A\u{fffd}\u{fffd}B"
        );
    }

    // ---- failures -----------------------------------------------------------

    #[test]
    fn an_absent_file_is_an_error_naming_the_token_and_the_path() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let error = at(&fixture, &process)
            .apply(r#"{"instructions":["{file:/nonexistent/zzz.md}"]}"#)
            .expect_err("must not be silently empty");
        assert_eq!(
            detail(&error),
            "bad file reference: \"{file:/nonexistent/zzz.md}\" /nonexistent/zzz.md does not exist"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "config file {} failed validation (1 issue(s))",
                fixture.config().display()
            )
        );
    }

    #[test]
    fn a_read_failure_that_is_not_absence_omits_the_does_not_exist_suffix() {
        // A directory is readable-but-not-a-file: the oracle's `ENOENT` branch
        // does not fire. Measured: `file-is-dir`.
        let fixture = Fixture::new();
        let process = Env::empty();
        let error = at(&fixture, &process)
            .apply("{file:./}")
            .expect_err("a directory cannot be read as text");
        assert_eq!(detail(&error), "bad file reference: \"{file:./}\"");
    }

    #[test]
    fn missing_empty_swallows_every_read_failure() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let subject = at(&fixture, &process).on_missing(Missing::Empty);
        assert_eq!(
            subject
                .apply(r#"{"a":"{file:/nonexistent/zzz.md}"}"#)
                .expect("swallowed"),
            r#"{"a":""}"#
        );
        // Not just absence: a directory too.
        assert_eq!(
            subject.apply(r#"{"a":"{file:./}"}"#).expect("swallowed"),
            r#"{"a":""}"#
        );
    }

    #[test]
    fn malformed_file_tokens_are_left_alone() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let subject = at(&fixture, &process);
        assert_eq!(subject.apply("{file:}").expect("literal"), "{file:}");
        assert_eq!(
            subject.apply("{file:no-brace").expect("literal"),
            "{file:no-brace"
        );
        // An empty token does not hide a real one behind it.
        assert_eq!(
            subject
                .apply("{file:}{file:./rel.md}")
                .expect("second one reads"),
            "{file:}relative-content"
        );
    }

    // ---- interaction between the two passes ---------------------------------

    #[test]
    fn the_env_pass_runs_first_so_its_value_can_produce_a_file_token() {
        // Measured: `env-value-makes-file-token`.
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("ZUNO_REF", "{file:./rel.md}");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply(r#"{"a":"{env:ZUNO_REF}"}"#)
                .expect("fixture exists"),
            r#"{"a":"relative-content"}"#
        );
    }

    #[test]
    fn the_env_pass_runs_first_so_a_file_path_can_be_built_from_a_variable() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("ZUNO_DIR", fixture.path("cfg").to_string_lossy());
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply("{file:{env:ZUNO_DIR}/rel.md}")
                .expect("fixture exists"),
            "relative-content"
        );
    }

    #[test]
    fn a_file_body_is_not_rescanned_for_tokens() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("ZUNO_INNER", "expanded");
        fixture.write("cfg/tokens.md", "{env:ZUNO_INNER} {file:./rel.md}");
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply("{file:./tokens.md}")
                .expect("fixture exists"),
            "{env:ZUNO_INNER} {file:./rel.md}"
        );
    }

    #[test]
    fn several_tokens_on_one_line_are_all_substituted() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let env = Env::empty().with("ZUNO_SAMPLE_MODEL", "m");
        let text = format!(
            r#"{{"model":"{{env:ZUNO_SAMPLE_MODEL}}","a":"{{file:./rel.md}}","b":"{{file:{}}}"}}"#,
            fixture.path("outside.md").display()
        );
        assert_eq!(
            at(&fixture, &process)
                .with_env(&env)
                .apply(&text)
                .expect("fixtures exist"),
            r#"{"model":"m","a":"relative-content","b":"abs-content"}"#
        );
    }

    #[test]
    fn text_without_tokens_is_returned_unchanged() {
        let fixture = Fixture::new();
        let process = Env::empty();
        let text = "{\n  // a comment\n  \"model\": \"anthropic/x\"\n}\n";
        assert_eq!(
            at(&fixture, &process).apply(text).expect("nothing to do"),
            text
        );
    }

    // ---- escaping, proved against serde_json --------------------------------

    #[test]
    fn the_escape_matches_serde_json_on_the_characters_that_matter() {
        for sample in [
            "",
            "plain",
            "quote\"inside",
            "back\\slash",
            "tab\there",
            "nl\nhere",
            "cr\rhere",
            "\u{8}\u{c}",
            "\u{0}\u{1}\u{1f}",
            "del\u{7f}and\u{85}nel",
            "slash/not/escaped",
            "unicode \u{4e2d}\u{6587} \u{1f600}",
        ] {
            let reference = serde_json::to_string(sample).expect("string serializes");
            assert_eq!(
                escape_json_string_body(sample),
                reference[1..reference.len() - 1],
                "escaping diverged on {sample:?}"
            );
        }
    }

    proptest::proptest! {
        /// The escape must agree with `serde_json` for every string, since the
        /// whole point is that the surrounding document still parses.
        #[test]
        fn escaping_agrees_with_serde_json_for_any_string(sample: String) {
            let reference = serde_json::to_string(&sample).expect("string serializes");
            proptest::prop_assert_eq!(
                escape_json_string_body(&sample),
                &reference[1..reference.len() - 1]
            );
        }

        /// No input may panic, whatever the byte offsets of the tokens land on.
        /// Multi-byte characters next to a token are the hazard here.
        #[test]
        fn arbitrary_text_never_panics(sample: String) {
            let process = Env::empty();
            let env = Env::empty().with("A", "\u{4e2d}");
            let result = Substitution::for_file(Path::new("/nonexistent/opencode.json"))
                .with_process_env(&process)
                .with_env(&env)
                .on_missing(Missing::Empty)
                .apply(&sample);
            proptest::prop_assert!(result.is_ok());
        }
    }
}
