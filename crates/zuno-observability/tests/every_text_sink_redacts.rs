//! Keeps redaction attached to every text sink this crate will ever grow.
//!
//! `tests/stdout_purity.rs` proves the guarantee holds for the three sinks that
//! exist today by running a real process. This test protects the *next* sink: a
//! fourth `tracing_subscriber` text sink composed into `init` would keep the default
//! field formatter and print `prompt=`, `command=`, `token=`, and `stderr=` verbatim,
//! and the runtime test only catches that if someone remembers to extend it.
//!
//! It is the same shape as `tests/no_stdout_in_library.rs` — a textual scan that
//! reports a violation even in a crate that does not currently compile, which is
//! when the report is most useful.
//!
//! Four properties make the scan hard to walk past by accident:
//!
//! - it matches on the *constructor or type*, not on a fully-qualified path, because
//!   `use tracing_subscriber::fmt::Layer;` makes every path prefix optional;
//! - it also matches the `fmt::` path itself, so an **alias** is reported where it is
//!   introduced: `use ...::fmt::Layer as FmtLayer;` and `type Sink<S> = ...::fmt::Layer<S>;`
//!   both trip it even though neither the alias nor its later `FmtLayer::new()` spells a
//!   banned constructor;
//! - it bans the *one-word builders* that replace the field formatter as well as the
//!   formatter types themselves, because `redact::text_layer(w, false, s).pretty()` is a
//!   one-token edit to the sanctioned constructor that prints every field raw (see
//!   `BANNED_TOKENS`);
//! - it runs over the file with comments removed and whitespace collapsed, so a
//!   construction split across lines is still one token while prose about the rule is not
//!   code.
//!
//! # What this scan does not report
//!
//! Stating the holes beats implying there are none.
//!
//! - **It is crate-scoped.** A text sink built in `zuno-cli`, `zuno-server`, or
//!   `zuno-tui` is outside `CARGO_MANIFEST_DIR/src`, so `zuno-observability` has to
//!   remain the only crate that constructs a production sink.
//! - **A name that never spells `fmt::` in this crate's `src/` is not statically
//!   resolvable.** A path assembled by a macro, or an alias re-exported from a
//!   dependency, is not reported. An alias *introduced* here is, because the `use` or
//!   `type` that introduces it has to name `fmt::Layer` or `fmt::layer`.
//! - **`redact.rs` is exempt in full**, so the sanctioned module is trusted rather than
//!   checked. `src/redact.rs`'s own unit tests are what cover it.
//! - **Where a sink writes is a different property.** A `fmt` layer that never calls
//!   `with_writer` writes to stdout; that is the subject of
//!   `tests/no_stdout_in_library.rs` and `tests/stdout_purity.rs`, not of this scan.
//! - **A banned token inside a string literal is reported**, deliberately: the scan
//!   fails closed rather than guessing that a literal is inert.
//!
//! The SQLite sink needs no scan: its visitor has a single constructor that wraps
//! itself in the redaction proxy, so an unredacted pass there does not compile.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Relative to `src/`. The one module allowed to build a `fmt` sink, because it is
/// the module that attaches the redacting field formatter.
const EXEMPT: &[&str] = &["redact.rs"];

/// Constructors and types that build — or un-redact — a `tracing_subscriber` text
/// sink.
///
/// A match only counts when the character before it is not an identifier character,
/// so `StructuredLogLayer::new(` and `redact::text_layer(` do not trip the scan while
/// `fmt::Layer::new(`, `Layer::<S>::new(`, and a bare `layer()` after a `use` all do.
///
/// # Why the formatter *methods* are banned and not only the formatter types
///
/// Banning the type names `DefaultFields`/`PrettyFields`/`JsonFields` and the
/// `fmt_fields(` setter is not enough, and an earlier revision of this list learned that
/// the expensive way. `tracing_subscriber::fmt::Layer` also exposes one-word builders
/// that swap the field formatter without ever naming it:
/// `.pretty()` sets `fmt_fields: format::Pretty`, `.json()` sets
/// `fmt_fields: JsonFields`, and `.map_fmt_fields(..)` replaces it with whatever the
/// closure returns. `.event_format(..)` is worse still: the `Pretty` and `Json` event
/// formats build their own field visitor instead of calling `ctx.format_fields`, so they
/// bypass `RedactingFields` even while it is installed.
///
/// `redact::text_layer(w, false, s).pretty()` — one extra method call on the sanctioned
/// constructor — was measured printing `command`, `prompt`, and `stderr` verbatim, and
/// the scan reported nothing. So a ban list that enumerates forbidden *types* has to
/// enumerate the builder methods that install them too; each of the four below has its
/// own case in `the_scanner_detects_every_reviewed_spelling_of_a_text_sink`.
///
/// None of the four appears anywhere in this crate's `src/`, so none needs suppressing,
/// and `redact.rs` — the module that legitimately calls `fmt_fields(RedactingFields)` —
/// is exempt in full.
const BANNED_TOKENS: &[&str] = &[
    "layer(",
    "Layer::",
    "Subscriber::builder(",
    "SubscriberBuilder",
    "FmtSubscriber",
    "fmt()",
    // Replacing the field formatter by naming it.
    "fmt_fields(",
    "DefaultFields",
    "PrettyFields",
    "JsonFields",
    // Replacing the field formatter, or the whole event format, without naming it.
    "pretty()",
    "json()",
    "map_fmt_fields(",
    "event_format(",
    "with_writer(",
    "with_test_writer(",
];

/// Path segments that mean "the text sink" when they follow `fmt::`, and the token name
/// each is reported under.
///
/// This is the half of the scan that survives renaming. `BANNED_TOKENS` matches how a
/// sink is *built*; this matches how the type or constructor is *named*, which is what an
/// alias cannot avoid — `use tracing_subscriber::fmt::Layer as FmtLayer;` still says
/// `fmt::Layer`. Both spellings of the brace group are covered:
/// `fmt::{self, Layer}` and `fmt::{format::Format, Layer}`.
const FMT_SINK_SEGMENTS: &[(&str, &str)] = &[("Layer", "fmt::Layer"), ("layer", "fmt::layer")];

/// A floor, not an exact count, so a scan pointed at the wrong directory fails
/// loudly instead of passing vacuously.
const MINIMUM_SOURCE_FILES: usize = 5;

#[derive(Debug)]
struct Violation {
    file: PathBuf,
    line_number: usize,
    line: String,
    token: &'static str,
}

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// What the lexer is inside of.
///
/// Rust's grammar decides whether `//` starts a comment or is two bytes of a URL, so the
/// scan cannot use a `find("//")` heuristic: `let u = "http://x"; fmt::layer();` has to
/// stay a violation while `let x = "s"; // fmt::layer() in prose` must not become one.
#[derive(Clone, Copy)]
enum Lex {
    Code,
    /// `/* ... */`, which nests in Rust.
    Block(usize),
    /// `"..."`, with backslash escapes.
    Text,
    /// `r"..."` or `r#"..."#`: no escapes, closed by `"` plus that many `#`.
    Raw(usize),
}

/// A file's code with comments removed and every whitespace run collapsed to a
/// single space, plus the source line each byte came from.
///
/// Collapsing is what makes the scan immune to a line break inside a path: a space
/// survives only where it separates two identifier characters, so `Layer\n::new()`
/// reads as `Layer::new()` while `text_layer(` stays distinct from ` layer(`. It
/// cannot hide a violation either, because a Rust identifier can never contain
/// whitespace, so the character before a token is an identifier character in the
/// collapsed text exactly when it was one in the source.
struct Code {
    text: String,
    lines: Vec<usize>,
}

/// Splits `source` into code and comments, keeping string literals as code.
///
/// Every comment byte becomes a single space, so a token can never be formed by joining
/// the text on either side of a stripped comment.
fn strip_comments(source: &str) -> Vec<(u8, usize)> {
    let bytes = source.as_bytes();
    let mut kept: Vec<(u8, usize)> = Vec::with_capacity(bytes.len());
    let mut state = Lex::Code;
    let mut line = 1_usize;
    let mut at = 0_usize;
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        match state {
            Lex::Code => {
                if byte == b'/' && next == Some(b'/') {
                    while at < bytes.len() && bytes[at] != b'\n' {
                        at += 1;
                    }
                    kept.push((b' ', line));
                    continue;
                }
                if byte == b'/' && next == Some(b'*') {
                    state = Lex::Block(1);
                    kept.push((b' ', line));
                    at += 2;
                    continue;
                }
                if byte == b'"' {
                    state = Lex::Text;
                    kept.push((byte, line));
                    at += 1;
                    continue;
                }
                if byte == b'r'
                    && !at
                        .checked_sub(1)
                        .is_some_and(|before| is_identifier_byte(bytes[before]))
                    && let Some(hashes) = raw_string_hashes(bytes, at)
                {
                    state = Lex::Raw(hashes);
                    for offset in 0..hashes + 2 {
                        kept.push((bytes[at + offset], line));
                    }
                    at += hashes + 2;
                    continue;
                }
                // A `'` opens a character literal or a lifetime. `'"'` must not open a
                // string, and `Components<'name>` must not open a character literal.
                if byte == b'\''
                    && let Some(width) = char_literal_width(bytes, at)
                {
                    for offset in 0..width {
                        kept.push((bytes[at + offset], line));
                    }
                    at += width;
                    continue;
                }
                if byte == b'\n' {
                    kept.push((b' ', line));
                    line += 1;
                    at += 1;
                    continue;
                }
                kept.push((byte, line));
                at += 1;
            }
            Lex::Block(depth) => {
                if byte == b'/' && next == Some(b'*') {
                    state = Lex::Block(depth + 1);
                    at += 2;
                    continue;
                }
                if byte == b'*' && next == Some(b'/') {
                    state = if depth == 1 {
                        Lex::Code
                    } else {
                        Lex::Block(depth - 1)
                    };
                    kept.push((b' ', line));
                    at += 2;
                    continue;
                }
                if byte == b'\n' {
                    line += 1;
                }
                at += 1;
            }
            Lex::Text => {
                if byte == b'\\' {
                    kept.push((byte, line));
                    if let Some(escaped) = next {
                        kept.push((escaped, line));
                        if escaped == b'\n' {
                            line += 1;
                        }
                    }
                    at += 2;
                    continue;
                }
                if byte == b'"' {
                    state = Lex::Code;
                }
                if byte == b'\n' {
                    kept.push((b' ', line));
                    line += 1;
                    at += 1;
                    continue;
                }
                kept.push((byte, line));
                at += 1;
            }
            Lex::Raw(hashes) => {
                if byte == b'"' && closes_raw_string(bytes, at, hashes) {
                    state = Lex::Code;
                    for offset in 0..hashes + 1 {
                        kept.push((bytes[at + offset], line));
                    }
                    at += hashes + 1;
                    continue;
                }
                if byte == b'\n' {
                    kept.push((b' ', line));
                    line += 1;
                    at += 1;
                    continue;
                }
                kept.push((byte, line));
                at += 1;
            }
        }
    }
    kept
}

/// How many `#` follow the `r` at `at`, when it opens a raw string.
fn raw_string_hashes(bytes: &[u8], at: usize) -> Option<usize> {
    let mut hashes = 0_usize;
    while bytes.get(at + 1 + hashes) == Some(&b'#') {
        hashes += 1;
    }
    (bytes.get(at + 1 + hashes) == Some(&b'"')).then_some(hashes)
}

fn closes_raw_string(bytes: &[u8], at: usize, hashes: usize) -> bool {
    (1..=hashes).all(|offset| bytes.get(at + offset) == Some(&b'#'))
}

/// The byte width of the character literal starting at `at`, or `None` when the `'` is a
/// lifetime instead.
fn char_literal_width(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at + 1) == Some(&b'\\') {
        // `'\n'`, `'\\'`, `'\''`, `'\u{1F600}'`.
        let close = (at + 2..bytes.len().min(at + 12)).find(|&offset| bytes[offset] == b'\'')?;
        return Some(close - at + 1);
    }
    (bytes.get(at + 2) == Some(&b'\'')).then_some(3)
}

fn normalize(source: &str) -> Code {
    let mut collapsed: Vec<(u8, usize)> = Vec::with_capacity(source.len());
    for (byte, number) in strip_comments(source) {
        let byte = if byte.is_ascii_whitespace() {
            b' '
        } else {
            byte
        };
        if byte == b' ' && collapsed.last().is_some_and(|(last, _)| *last == b' ') {
            continue;
        }
        collapsed.push((byte, number));
    }

    // Keep a space only where it separates two identifier characters. That is the
    // only place a space carries meaning for this scan; dropping the rest joins a
    // path broken across lines back together.
    let mut text = Vec::with_capacity(collapsed.len());
    let mut lines = Vec::with_capacity(collapsed.len());
    for (position, (byte, number)) in collapsed.iter().copied().enumerate() {
        if byte == b' ' {
            let previous = position
                .checked_sub(1)
                .and_then(|at| collapsed.get(at))
                .map(|(byte, _)| *byte);
            let next = collapsed.get(position + 1).map(|(byte, _)| *byte);
            if !(previous.is_some_and(is_identifier_byte) && next.is_some_and(is_identifier_byte)) {
                continue;
            }
        }
        text.push(byte);
        lines.push(number);
    }
    Code {
        // Only ASCII comment and whitespace bytes were removed, so the remaining bytes
        // are the same UTF-8 sequences the source had.
        text: String::from_utf8(text).expect("stripping ASCII bytes keeps UTF-8 valid"),
        lines,
    }
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Every `fmt::` path in `code` that names the text sink type or its constructor, as
/// `(offset in the code text, token)`.
///
/// The region after each `fmt::` is the balanced brace group when the path opens one, and
/// otherwise the following `A-Za-z0-9_:` run — which stops before an ` as Alias`, because
/// the alias's own spelling is arbitrary and only the imported name is evidence. Every
/// path segment in that region is then checked, so item order inside a brace group does
/// not matter.
fn fmt_sink_paths(code: &Code) -> Vec<(usize, &'static str)> {
    let bytes = code.text.as_bytes();
    let mut found = Vec::new();
    let mut from = 0_usize;
    while let Some(offset) = code.text[from..].find("fmt::") {
        let after = from + offset + "fmt::".len();
        from = from + offset + 1;
        let end = if bytes.get(after) == Some(&b'{') {
            match brace_group_end(bytes, after) {
                Some(end) => end,
                None => continue,
            }
        } else {
            let mut at = after;
            while bytes
                .get(at)
                .is_some_and(|byte| is_identifier_byte(*byte) || *byte == b':')
            {
                at += 1;
            }
            at
        };

        let mut at = after;
        while at < end {
            if !is_identifier_byte(bytes[at]) {
                at += 1;
                continue;
            }
            let start = at;
            while at < end && is_identifier_byte(bytes[at]) {
                at += 1;
            }
            if let Some((_, token)) = FMT_SINK_SEGMENTS
                .iter()
                .find(|(candidate, _)| *candidate == &code.text[start..at])
            {
                found.push((start, *token));
            }
        }
    }
    found
}

/// The offset just past the `}` closing the brace group that opens at `at`.
fn brace_group_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, byte) in bytes.iter().enumerate().skip(at) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every banned token in `source`, as `(source line, token)`.
fn violations_in(source: &str) -> Vec<(usize, &'static str)> {
    let code = normalize(source);
    let bytes = code.text.as_bytes();
    let mut found = Vec::new();
    for token in BANNED_TOKENS {
        let mut from = 0;
        while let Some(offset) = code.text[from..].find(token) {
            let at = from + offset;
            if at == 0 || !is_identifier_byte(bytes[at - 1]) {
                found.push((code.lines.get(at).copied().unwrap_or(0), *token));
            }
            from = at + 1;
        }
    }
    for (at, token) in fmt_sink_paths(&code) {
        found.push((code.lines.get(at).copied().unwrap_or(0), token));
    }
    found.sort_unstable();
    found.dedup();
    found
}

fn scan() -> (Vec<Violation>, usize, usize) {
    let src = src_dir();
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    let mut exempted = 0_usize;

    for entry in WalkDir::new(&src).into_iter().flatten() {
        let path = entry.path();
        if !entry.file_type().is_file() || path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }

        let relative = path
            .strip_prefix(&src)
            .expect("walked under src")
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT.contains(&relative.as_str()) {
            exempted += 1;
            continue;
        }
        scanned += 1;

        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let source_lines = contents.lines().collect::<Vec<_>>();
        for (line_number, token) in violations_in(&contents) {
            violations.push(Violation {
                file: path.to_path_buf(),
                line_number,
                line: source_lines
                    .get(line_number.saturating_sub(1))
                    .unwrap_or(&"")
                    .trim()
                    .to_owned(),
                token,
            });
        }
    }

    (violations, scanned, exempted)
}

#[test]
fn no_source_file_builds_a_text_sink_outside_the_redaction_module() {
    let (violations, scanned, _) = scan();

    assert!(
        scanned >= MINIMUM_SOURCE_FILES,
        "scanned only {scanned} files under {}; the scan is looking in the wrong place \
         and would pass vacuously",
        src_dir().display()
    );

    if !violations.is_empty() {
        let mut report = format!(
            "{} source line(s) build a text sink, or replace its field formatter, \
             without the redacting one.\n\
             A default `fmt` sink prints prompt, command, output, credential, token, \
             and raw subprocess-stream fields verbatim, so the plaintext file and \
             stderr would disagree with the SQLite store about what is safe to \
             write. Build the sink with `redact::text_layer` instead, and leave its \
             field formatter alone.\n\
             Only {} may construct one.\n\n",
            violations.len(),
            EXEMPT.join(", ")
        );
        for violation in &violations {
            report.push_str(&format!(
                "  {}:{}  matched {:?}\n    {}\n",
                violation.file.display(),
                violation.line_number,
                violation.token,
                violation.line
            ));
        }
        panic!("{report}");
    }
}

/// The exemption has to exist. If the redaction module were renamed, the exemption
/// would silently stop matching anything and the guard would lose its subject.
#[test]
fn the_exempt_redaction_module_still_exists() {
    let (_, _, exempted) = scan();
    assert_eq!(
        exempted,
        EXEMPT.len(),
        "every exempt path must exist; {EXEMPT:?} matched {exempted} file(s) under {}",
        src_dir().display()
    );
}

/// A scanner that cannot detect a violation is worse than no scanner, because it reads
/// as a passing guarantee. Every case below is a spelling an adversarial review walked
/// past some earlier version of this scan; the module docs list what is still not
/// reported, which is deliberately not the same claim as "every way".
#[test]
fn the_scanner_detects_every_reviewed_spelling_of_a_text_sink() {
    for case in [
        "let layer = tracing_subscriber::fmt::layer().with_writer(writer);",
        "use tracing_subscriber::fmt; let l = fmt::layer();",
        "let l = tracing_subscriber::fmt::Layer::new();",
        "let l = tracing_subscriber::fmt::Layer::default();",
        "tracing_subscriber::fmt().with_writer(writer).init();",
        "let l = tracing_subscriber::fmt::Layer::<S>::new();",
        "use tracing_subscriber::fmt::Layer;\nlet l = Layer::new();",
        "use tracing_subscriber::fmt::layer;\nlet l = layer();",
        "let s = tracing_subscriber::fmt::Subscriber::builder().finish();",
        "let b = tracing_subscriber::fmt::SubscriberBuilder::default();",
        "let s = tracing_subscriber::FmtSubscriber::builder().finish();",
        // Un-redacting the sanctioned constructor by naming the replacement formatter.
        "let l = redact::text_layer(w, false, s).fmt_fields(DefaultFields::new());",
        "let l = sink().fmt_fields(PrettyFields::new());",
        "let l = sink().fmt_fields(JsonFields::new());",
        // Un-redacting the sanctioned constructor WITHOUT naming a formatter: each of
        // these one-word builders replaces the field formatter, or the event format that
        // would have called it. `.pretty()` is the measured leak — it printed `command`,
        // `prompt`, and `stderr` verbatim through `redact::text_layer` — and the other
        // three are the same bypass in a different spelling.
        "let l = redact::text_layer(w, false, s).pretty();",
        "let l = redact::text_layer(w, false, s).json();",
        "let l = redact::text_layer(w,false,s).map_fmt_fields(|_| format::Pretty::default());",
        "let l = redact::text_layer(w, false, s).event_format(tracing_subscriber::fmt::format().pretty());",
        // The same four with the spacing Rust also accepts, since the scan collapses
        // whitespace rather than requiring the canonical spelling.
        "let l = redact::text_layer(w, false, s)\n    .pretty();",
        "let l = redact::text_layer(w, false, s) . json ();",
        // A path broken across lines, which a line-based scan cannot see at all.
        "let l = tracing_subscriber::fmt::\n    layer()\n    .with_writer(writer);",
        "let l = tracing_subscriber::fmt::Layer\n    ::new();",
        // An alias: neither the alias nor its use spells a banned constructor, so the
        // `use`/`type` that introduces it is what has to be reported.
        "use tracing_subscriber::fmt::Layer as FmtLayer;\nlet l = FmtLayer::new().with_test_writer();",
        "type Sink<S> = tracing_subscriber::fmt::Layer<S>;\nlet l = Sink::default();",
        "use tracing_subscriber::fmt::layer as build_sink;\nlet l = build_sink();",
        // Brace groups, in both orders.
        "use tracing_subscriber::fmt::{self, Layer};",
        "use tracing_subscriber::fmt::{format::Format, Layer};",
        "use tracing_subscriber::fmt::{Layer, format::Format};",
        // A banned token inside a string literal is reported rather than assumed inert.
        "let name = \"tracing_subscriber::fmt::Layer\";",
    ] {
        assert!(
            !violations_in(case).is_empty(),
            "scanner missed a violation in {case:?}"
        );
    }
}

#[test]
fn the_scanner_ignores_prose_and_the_sanctioned_helper() {
    for case in [
        "//! fmt::layer() defaults to an unredacted field formatter",
        "        // never call tracing_subscriber::fmt::layer() here",
        "/// See `redact::text_layer`, the only sanctioned constructor.",
        // Prose about the formatter-replacing builders, which the ban list now covers.
        "//! never call `.pretty()` or `.json()` on the sanctioned layer",
        "        // .map_fmt_fields(..) and .event_format(..) would un-redact it",
        // A line comment after code that contains a quote. The old `find("//")` scan
        // refused to strip this one and reported prose as a violation.
        "let x = \"a quoted string\"; // fmt::layer() only in prose",
        // Block comments, including a nested one, are prose too.
        "/* mentions fmt::layer() */",
        "/* outer /* inner fmt::Layer::new() */ still prose */ let x = 1;",
        "let a = 1; /* fmt::layer()\n   across lines */ let b = 2;",
        // The `fmt::` path check must not fire on the other things under `fmt`.
        "use tracing_subscriber::fmt::format::FmtSpan;",
        "use tracing_subscriber::fmt::MakeWriter;",
        "use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt as _};",
        "use tracing_subscriber::Layer;",
        "use std::fmt::Write as _;",
        "let layer = redact::text_layer(writer, false, span_events);",
        "let stderr_layer = print_logs.then(|| redact::text_layer(w, a, s));",
        "let layer = store::StructuredLogLayer::new(sender, dropped, failures);",
        "impl<S> Layer<S> for StructuredLogLayer where S: Subscriber {}",
        "use tracing_subscriber::layer::SubscriberExt as _;",
        "let installed = registry().with(filter).with(store_layer).try_init();",
        "fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) }",
        "let (file_layer, file_guard) = (Some(layer), Some(guard));",
    ] {
        assert!(
            violations_in(case).is_empty(),
            "scanner reported a false violation in {case:?}: {:?}",
            violations_in(case)
        );
    }
}

/// The reported line number has to survive comment stripping and whitespace
/// collapsing, or the report points the next author at the wrong place.
#[test]
fn a_violation_is_reported_on_its_own_source_line() {
    let source = "//! fmt::layer() in prose\nfn build() {\n    let l = tracing_subscriber::fmt::\n        layer();\n}\n";
    let found = violations_in(source);
    assert_eq!(found, vec![(4, "fmt::layer"), (4, "layer(")], "{found:?}");
}

/// A `//` inside a string literal does not start a comment, so the code after it is
/// still code. This is the case a quote-counting heuristic gets backwards.
#[test]
fn a_slash_inside_a_string_literal_does_not_hide_the_code_after_it() {
    let source = "let u = \"http://example.invalid\";\nlet l = tracing_subscriber::fmt::layer();\n";
    let found = violations_in(source);
    assert_eq!(found, vec![(2, "fmt::layer"), (2, "layer(")], "{found:?}");
}

/// Two spellings the lexer has to get right or it silently stops seeing code: a raw
/// string containing an unbalanced quote, and a character literal that *is* a quote.
#[test]
fn a_raw_string_and_a_quote_character_do_not_derail_the_lexer() {
    for source in [
        "let json = r#\"{\"a\": 1}\"#;\nlet l = tracing_subscriber::fmt::layer();\n",
        "let quote = \'\"\';\nlet l = tracing_subscriber::fmt::layer();\n",
        "struct Components<\'name> { rest: &\'name str }\nlet l = tracing_subscriber::fmt::layer();\n",
    ] {
        assert_eq!(
            violations_in(source),
            vec![(2, "fmt::layer"), (2, "layer(")],
            "the lexer lost the code after {source:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The `message` field: the one name the redaction predicate is blind to.
// ---------------------------------------------------------------------------

/// Crates that emit a `message = …` tracing field today, keyed by the path under
/// `crates/`, with why each is known.
///
/// This is an allowlist, not an approval. Every entry occupies the bare `message` slot;
/// the remaining ones carry only Zuno-authored text.
///
/// `zuno-mcp` is deliberately absent. It held three entries — `src/stdio.rs` (a raw MCP
/// server stderr line, the measured leak), `src/oauth/discovery.rs`, and
/// `src/remote/exchange.rs` — until each emitter renamed its field to one the redaction
/// predicate classifies (`stderr`, `reason`, `stream_output`). With no entry, the scan
/// below is live for that crate: a `message` field reintroduced anywhere under
/// `crates/zuno-mcp/src` fails `no_crate_emits_an_unexpected_message_field`, and
/// `zuno_mcp_is_not_allowlisted_for_message_fields` keeps the entry from coming back.
const KNOWN_MESSAGE_FIELD_EMITTERS: &[(&str, &str)] = &[
    (
        "zuno-cli/src/cmd/product_agent.rs",
        "`%message` carries one of two Zuno-authored recovery sentences plus a job id, so \
         nothing external reaches the sink; the bare rendering is still misleading",
    ),
    (
        "zuno-cli/src/cmd/child_turn.rs",
        "`%message` carries one of two Zuno-authored recovery sentences plus a job id, so \
         nothing external reaches the sink; the bare rendering is still misleading",
    ),
];

/// A floor on the crates walked, so a scan pointed at the wrong directory fails loudly
/// instead of passing vacuously.
const MINIMUM_WORKSPACE_CRATES: usize = 15;

/// Every `message` tracing-field spelling in `code`, as offsets into the code text.
///
/// A tracing field key is preceded by `(` or `,` — ignoring the space the normalizer may
/// have kept — optionally carries a `%` or `?` sigil, and is followed by `=`. That shape
/// is what separates a field from the other ways `message` appears in Rust: `let message =`
/// and `self.message =` fail the preceding-character test, `Foo { message: x }` uses `:`,
/// and `"failed: {message}"` has `{` before it.
///
/// # What this does not detect
///
/// The bare shorthand `warn!(message, "…")` is textually identical to the positional
/// argument `warn!("{}", message)`, so it is left out rather than guessed at; the sigil
/// forms `%message` and `?message` are detected. `span.record("message", …)` is detected
/// separately below.
fn message_fields_in(code: &Code) -> Vec<usize> {
    let bytes = code.text.as_bytes();
    let mut found = Vec::new();
    let mut from = 0_usize;
    while let Some(offset) = code.text[from..].find("message") {
        let at = from + offset;
        from = at + 1;

        // Not part of a longer identifier.
        if at
            .checked_sub(1)
            .is_some_and(|before| is_identifier_byte(bytes[before]))
        {
            continue;
        }
        let after = at + "message".len();
        if bytes
            .get(after)
            .is_some_and(|byte| is_identifier_byte(*byte))
        {
            continue;
        }

        // Walk back over an optional sigil and an optional kept space.
        let mut before = at;
        let sigil = before
            .checked_sub(1)
            .is_some_and(|at| bytes[at] == b'%' || bytes[at] == b'?');
        if sigil {
            before -= 1;
        }
        if before.checked_sub(1).is_some_and(|at| bytes[at] == b' ') {
            before -= 1;
        }
        let opens_an_argument = before
            .checked_sub(1)
            .is_some_and(|at| bytes[at] == b'(' || bytes[at] == b',');
        if !opens_an_argument {
            continue;
        }

        // `message = value`, or a sigil form that needs no `=`.
        let mut end = after;
        if bytes.get(end) == Some(&b' ') {
            end += 1;
        }
        let assigns = bytes.get(end) == Some(&b'=') && bytes.get(end + 1) != Some(&b'=');
        if assigns || sigil {
            found.push(at);
        }
    }

    // `span.record("message", …)` names the same field through a string literal.
    let mut from = 0_usize;
    while let Some(offset) = code.text[from..].find("record(\"message\"") {
        found.push(from + offset);
        from = from + offset + 1;
    }

    found.sort_unstable();
    found.dedup();
    found
}

/// `message` is the field `tracing` uses for an event's own text, so
/// `redact::sensitive_field` lets it through and `DefaultVisitor` prints it *bare*,
/// without a `name=` prefix. An emitter that writes `debug!(message = %raw_stream, "…")`
/// therefore renders a payload exactly where the event text belongs, and it reads as prose
/// rather than as a field.
///
/// That gap cannot be closed here. Redacting `message` would replace every log line in the
/// plaintext file, on `--print-logs` stderr, and in the `message` column of `logs.sqlite`
/// with the placeholder — a confidentiality gain paid for with the whole log. So this is a
/// tripwire instead: the emitters that exist today are named, with the reason each is
/// known, and a *new* one fails here. The fix for a reported emitter is always in the
/// emitter, not in this crate — which is how the `zuno-mcp` entries left this list: the
/// MCP stderr drain that was measured leaking a peer's `Traceback: API_KEY=sk-live-abc123`
/// now records the line as `stderr`, a name the predicate redacts.
///
/// The assertion is one-directional on purpose. It fails when an unlisted emitter appears,
/// and it does *not* fail when a listed one is fixed, because a listed emitter is expected
/// to go away and this crate's gate must not be the thing that blocks the crate that fixes
/// it. A stale entry is documentation debt, which is why each entry carries its reason.
#[test]
fn no_crate_emits_an_unexpected_message_field() {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .to_path_buf();

    let mut crates = 0_usize;
    let mut unexpected: Vec<(String, usize, String)> = Vec::new();
    for crate_entry in std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", crates_dir.display()))
        .flatten()
    {
        let src = crate_entry.path().join("src");
        if !src.is_dir() {
            continue;
        }
        crates += 1;

        for entry in WalkDir::new(&src).into_iter().flatten() {
            let path = entry.path();
            if !entry.file_type().is_file() || path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let relative = path
                .strip_prefix(&crates_dir)
                .expect("walked under the crates directory")
                .to_string_lossy()
                .replace('\\', "/");
            if KNOWN_MESSAGE_FIELD_EMITTERS
                .iter()
                .any(|(known, _)| *known == relative)
            {
                continue;
            }

            let contents = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            let code = normalize(&contents);
            let source_lines = contents.lines().collect::<Vec<_>>();
            for at in message_fields_in(&code) {
                let line_number = code.lines.get(at).copied().unwrap_or(0);
                unexpected.push((
                    relative.clone(),
                    line_number,
                    source_lines
                        .get(line_number.saturating_sub(1))
                        .unwrap_or(&"")
                        .trim()
                        .to_owned(),
                ));
            }
        }
    }

    assert!(
        crates >= MINIMUM_WORKSPACE_CRATES,
        "walked only {crates} crate(s) under {}; the scan is looking in the wrong place and \
         would pass vacuously",
        crates_dir.display()
    );

    if !unexpected.is_empty() {
        let mut report = format!(
            "{} tracing callsite(s) record a field named `message`.\n\
             `message` is the field name `tracing` uses for an event's own text: \
             `zuno_observability`'s redaction predicate deliberately lets it through, and \
             the formatter prints it with no `name=` prefix. So whatever this field carries \
             is written verbatim to the plaintext log, to `--print-logs` stderr, and into \
             the `message` column of `logs.sqlite`, where it reads as the event message. A \
             prompt, a command, a credential, or a subprocess stream must not go here — \
             give it its own field name, which the redaction predicate can classify, or \
             keep only a bound (`bytes`, `limit`, `truncated`).\n\
             If this callsite is genuinely a short diagnostic string, add it to \
             KNOWN_MESSAGE_FIELD_EMITTERS in {} with the reason.\n\n",
            unexpected.len(),
            file!()
        );
        for (file, line_number, line) in &unexpected {
            report.push_str(&format!("  crates/{file}:{line_number}\n    {line}\n"));
        }
        panic!("{report}");
    }
}

/// The `message` detector has to see the spellings that record the field and ignore the
/// ones that do not, or the tripwire above is decoration.
///
/// The first two cases are the measured emitters as they stood in
/// `crates/zuno-mcp/src/stdio.rs` before the field was renamed to `stderr`: the first
/// rendered an MCP server's stderr line as
/// `DEBUG …: MCP server stderr server=probe-mcp Traceback: API_KEY=sk-…` through the
/// shipped `redact::text_layer`. They stay here verbatim because they are exactly what a
/// reintroduction would look like, and `zuno-mcp` is no longer allowlisted, so the
/// detector seeing them is what makes the live scan fail on it.
#[test]
fn the_message_detector_sees_a_field_and_not_a_local() {
    for records in [
        "tracing::debug!(%server, message = message.trim_end_matches(['\\r', '\\n']), \"MCP server stderr\");",
        "tracing::debug!(%server, bytes, limit = MAX, truncated = true, message = %message, \"line exceeded its bound\");",
        "tracing::debug!(server, what, message = %error.describe(what), \"skipping\");",
        "tracing::warn!(message = ?payload, \"x\");",
        "tracing::info!(\n    message = raw,\n    \"x\"\n);",
        "tracing::warn!(%message, \"x\");",
        "tracing::warn!(?message, \"x\");",
        "span.record(\"message\", &value);",
    ] {
        assert!(
            !message_fields_in(&normalize(records)).is_empty(),
            "the detector missed a `message` field in {records:?}"
        );
    }
    for benign in [
        "let message = String::from_utf8_lossy(&line);",
        "self.message = Some(value);",
        "*message = serde_json::json!(\"<redacted>\");",
        "let failure = ReaderFailure::Io { kind: error.kind(), message: Arc::from(text) };",
        "tracing::warn!(\"failed: {message}\");",
        "tracing::warn!(\"failed: {}\", message);",
        "assert_eq!(message, \"x\");",
        "if message == \"x\" { return; }",
        "let messages = vec![message.clone()];",
        "fn describe(message: &str) -> String { message.to_owned() }",
        "// message = raw would put a payload where the event text belongs",
        "/// `message = %stream` is exactly what this crate forbids",
    ] {
        assert!(
            message_fields_in(&normalize(benign)).is_empty(),
            "the detector reported a false `message` field in {benign:?}: {:?}",
            message_fields_in(&normalize(benign))
        );
    }
}

/// The MCP stderr drain was the live confidentiality leak behind this whole tripwire:
/// an MCP server writing `Traceback: API_KEY=sk-live-abc123` to stderr reached the
/// plaintext log, `--print-logs` stderr, and the `message` column of `logs.sqlite`
/// verbatim, because the drain named the line `message`. The emitter was fixed by
/// renaming the field, and the fix is only as durable as the scan being live for that
/// crate. An allowlist entry is how the scan is silenced, so this pins that `zuno-mcp`
/// has none: reintroducing the field *and* the entry in one change is the only way past
/// `no_crate_emits_an_unexpected_message_field`, and this test fails on the entry.
#[test]
fn zuno_mcp_is_not_allowlisted_for_message_fields() {
    let listed = KNOWN_MESSAGE_FIELD_EMITTERS
        .iter()
        .map(|(file, _)| *file)
        .filter(|file| file.starts_with("zuno-mcp/"))
        .collect::<Vec<_>>();
    assert!(
        listed.is_empty(),
        "zuno-mcp is allowlisted for a `message` field again: {listed:?}. Its emitters were \
         renamed so the redaction predicate can classify them (`stderr`, `reason`, \
         `stream_output`); a new `message` field there must be renamed, not listed."
    );
}
