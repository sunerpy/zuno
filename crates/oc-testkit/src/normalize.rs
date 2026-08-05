//! Text normalization for differential comparison — deliberately narrow.
//!
//! # Why this module is written so defensively
//!
//! A differential harness is only worth the bytes it occupies if a failure means
//! something. The single fastest way to destroy that is to widen normalization
//! until the diff goes green. `strip anything that looks volatile` and
//! `s/[0-9a-f]{8,}/<HASH>/` are the two commits that turn this crate into
//! decoration. So:
//!
//! - **Every rule is named.** [`DiffReport`](crate::DiffReport) prints which
//!   rules fired and how many spans each one masked, so masking is visible in
//!   the output rather than implied by a passing test.
//! - **Every rule carries its own justification** in [`NormalizationRule::why`],
//!   and is covered by a test that asserts what it matches *and* what it must
//!   not.
//! - **Rules are recognizers, not regexes.** Each one is a hand-written scanner
//!   that must match a fully-specified shape at an exact offset. There is no
//!   pattern language here to be loosened by one character.
//! - **The default set is pinned by a test.** `default_rule_names_are_pinned`
//!   fails if a rule is added, removed, or renamed, which makes widening a
//!   reviewed act instead of a diff nobody reads.
//! - **Volatile *paths* are literals, never patterns.** A run masks the exact
//!   temporary directory it created (see [`Normalizer::mask_literal`]). There is
//!   no `/tmp/.*` rule, because that would also swallow a subject that wrote to
//!   the wrong temporary file.
//!
//! # What is deliberately *not* normalized
//!
//! - **Line endings.** A subject emitting `\r\n` where the oracle emits `\n` is a
//!   real compatibility defect for anything piping output.
//! - **Whitespace and indentation.** Same reasoning; JSON indentation is
//!   observable.
//! - **Durations and elapsed times.** They look volatile and mostly are, but
//!   `"took 0ms"` versus `"took 900ms"` has caught real regressions elsewhere;
//!   a caller that genuinely needs this masks it explicitly with
//!   [`Normalizer::mask_literal`] or adds a justified rule here.
//! - **Numbers in general.** Ports are masked only in a loopback address
//!   position; process ids only in an explicitly labelled field.

use std::borrow::Cow;
use std::collections::BTreeMap;

/// The identifier prefixes the oracle mints IDs under.
///
/// Grounded in `packages/core/src/id/id.ts`, which enumerates exactly these.
const ID_PREFIXES: &[&str] = &[
    "job", "evt", "ses", "msg", "per", "que", "prt", "pty", "tool", "wrk",
];

/// How a rule recognizes the span it will replace.
enum Recognizer {
    /// An exact byte string, known at run time (a temporary directory, a home).
    Literal(String),
    /// A hand-written scanner: given the whole input and a start offset, return
    /// the length of the match starting exactly at that offset, or `None`.
    Scan(fn(&[u8], usize) -> Option<usize>),
}

/// One named, justified normalization rule.
///
/// Construct the built-ins through [`Normalizer::default`]; construct literal
/// masks through [`Normalizer::mask_literal`].
pub struct NormalizationRule {
    name: Cow<'static, str>,
    why: Cow<'static, str>,
    placeholder: Cow<'static, str>,
    recognizer: Recognizer,
}

impl std::fmt::Debug for NormalizationRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NormalizationRule")
            .field("name", &self.name)
            .field("placeholder", &self.placeholder)
            .finish_non_exhaustive()
    }
}

impl NormalizationRule {
    /// The stable identifier this rule is reported under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Why masking this span cannot hide a semantic difference.
    #[must_use]
    pub fn why(&self) -> &str {
        &self.why
    }

    /// The text a matched span is replaced by.
    #[must_use]
    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    fn match_at(&self, input: &[u8], at: usize) -> Option<usize> {
        match &self.recognizer {
            Recognizer::Literal(needle) => {
                let bytes = needle.as_bytes();
                (!bytes.is_empty() && input[at..].starts_with(bytes)).then_some(bytes.len())
            }
            Recognizer::Scan(scan) => scan(input, at),
        }
    }
}

/// An ordered set of [`NormalizationRule`]s applied to text before comparison.
///
/// Literal masks are always tried before the built-in recognizers, and longer
/// literals before shorter ones, so that a temporary path containing something
/// that looks like a timestamp is masked as a path rather than shredded.
pub struct Normalizer {
    literals: Vec<NormalizationRule>,
    builtins: Vec<NormalizationRule>,
}

impl std::fmt::Debug for Normalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Normalizer")
            .field("rules", &self.rule_names())
            .finish()
    }
}

impl Default for Normalizer {
    /// The pinned default rule set. See the module docs for the omissions.
    fn default() -> Self {
        Self {
            literals: Vec::new(),
            builtins: vec![
                NormalizationRule {
                    name: Cow::Borrowed("iso8601-timestamp"),
                    why: Cow::Borrowed(
                        "Wall-clock instants differ between two runs by construction. The shape \
                         matched is a full date-time (YYYY-MM-DDThh:mm:ss with optional fraction \
                         and zone), never a bare date or a bare number, so a differing *date* in \
                         a non-timestamp field still diverges.",
                    ),
                    placeholder: Cow::Borrowed("<TIMESTAMP>"),
                    recognizer: Recognizer::Scan(scan_iso8601),
                },
                NormalizationRule {
                    name: Cow::Borrowed("opencode-id"),
                    why: Cow::Borrowed(
                        "Session, message and part identifiers embed the mint time and 14 random \
                         base62 characters (packages/schema/src/identifier.ts), so they cannot \
                         agree across runs. The shape matched is one of the ten known prefixes, \
                         an underscore, exactly 12 lowercase hex characters, then exactly 14 \
                         base62 characters — narrow enough that no word, model name or path can \
                         satisfy it.",
                    ),
                    placeholder: Cow::Borrowed("<ID>"),
                    recognizer: Recognizer::Scan(scan_opencode_id),
                },
                NormalizationRule {
                    name: Cow::Borrowed("uuid"),
                    why: Cow::Borrowed(
                        "Randomly generated per run. The shape matched is the canonical hyphenated \
                         8-4-4-4-12 hex form only; an unhyphenated hex blob is left alone because \
                         content hashes look like that and a differing hash is a real difference.",
                    ),
                    placeholder: Cow::Borrowed("<UUID>"),
                    recognizer: Recognizer::Scan(scan_uuid),
                },
                NormalizationRule {
                    name: Cow::Borrowed("loopback-port"),
                    why: Cow::Borrowed(
                        "An ephemeral port is assigned by the kernel. Only the digits are masked, \
                         and only when they sit directly after a loopback authority and a colon \
                         (127.0.0.1:, localhost:, [::1]:, ::1:), so a differing configured port on \
                         any other host — or a differing host on loopback — still diverges. The \
                         wildcard bind 0.0.0.0 is deliberately excluded: it is a different address, \
                         not a loopback one.",
                    ),
                    placeholder: Cow::Borrowed("<PORT>"),
                    recognizer: Recognizer::Scan(scan_loopback_port),
                },
                NormalizationRule {
                    name: Cow::Borrowed("labelled-pid"),
                    why: Cow::Borrowed(
                        "Process ids are assigned by the kernel. Only the digits are masked, and \
                         only where they sit directly after an explicit pid label (\"pid\":, pid=, \
                         pid: ) and form a plausible pid (>= 2), so \"pid\":0, \"pid\":1 and \
                         \"pid\":null — which would indicate a subject that failed to record one — \
                         still diverge, and the label itself always stays visible in the diff.",
                    ),
                    placeholder: Cow::Borrowed("<PID>"),
                    recognizer: Recognizer::Scan(scan_labelled_pid),
                },
            ],
        }
    }
}

impl Normalizer {
    /// A normalizer that changes nothing, for byte-exact comparisons.
    ///
    /// This is the right choice whenever the two sides *should* agree byte for
    /// byte — a path dump, a JSON schema, a tool list. Reach for
    /// [`Normalizer::default`] only when a genuinely volatile span is present.
    #[must_use]
    pub fn none() -> Self {
        Self {
            literals: Vec::new(),
            builtins: Vec::new(),
        }
    }

    /// Mask an exact string, such as the temporary root a run created.
    ///
    /// This is the only sanctioned way to neutralize a volatile path. The caller
    /// supplies the literal it actually used, so there is no pattern that could
    /// also match a path the subject got wrong. `name` appears in the report.
    #[must_use]
    pub fn mask_literal(
        mut self,
        name: impl Into<String>,
        literal: impl Into<String>,
        placeholder: impl Into<String>,
    ) -> Self {
        let literal = literal.into();
        if literal.is_empty() {
            return self;
        }
        self.literals.push(NormalizationRule {
            name: Cow::Owned(name.into()),
            why: Cow::Borrowed(
                "An exact string this run created; supplied by the caller rather than matched by a \
                 pattern, so it cannot also mask a value the subject got wrong.",
            ),
            placeholder: Cow::Owned(placeholder.into()),
            recognizer: Recognizer::Literal(literal),
        });
        self.literals
            .sort_by_key(|rule| std::cmp::Reverse(literal_len(rule)));
        self
    }

    /// Every rule name in application order.
    #[must_use]
    pub fn rule_names(&self) -> Vec<&str> {
        self.rules().map(NormalizationRule::name).collect()
    }

    /// Every rule in application order: literal masks first, longest first.
    pub fn rules(&self) -> impl Iterator<Item = &NormalizationRule> {
        self.literals.iter().chain(self.builtins.iter())
    }

    /// Apply every rule, returning the normalized text and a per-rule tally of
    /// how many spans each one masked.
    #[must_use]
    pub fn apply(&self, input: &str) -> (String, BTreeMap<String, usize>) {
        let mut fired: BTreeMap<String, usize> = BTreeMap::new();
        if self.literals.is_empty() && self.builtins.is_empty() {
            return (input.to_owned(), fired);
        }

        let bytes = input.as_bytes();
        let mut out = String::with_capacity(input.len());
        let mut at = 0usize;
        while at < bytes.len() {
            let mut matched = false;
            for rule in self.rules() {
                if let Some(len) = rule.match_at(bytes, at) {
                    debug_assert!(len > 0, "a rule matched a zero-length span");
                    out.push_str(rule.placeholder());
                    *fired.entry(rule.name().to_owned()).or_default() += 1;
                    at += len;
                    matched = true;
                    break;
                }
            }
            if !matched {
                // Byte-wise copy is safe: every recognizer above starts on an
                // ASCII byte, and an ASCII byte never occurs inside a multi-byte
                // UTF-8 sequence, so no match can begin mid-character.
                let start = at;
                let mut end = at + 1;
                while end < bytes.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
                    end += 1;
                }
                out.push_str(&input[start..end]);
                at = end;
            }
        }
        (out, fired)
    }
}

fn literal_len(rule: &NormalizationRule) -> usize {
    match &rule.recognizer {
        Recognizer::Literal(l) => l.len(),
        Recognizer::Scan(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Recognizers. Each one answers a single question: "does the fully specified
// shape start at exactly this offset?" They never search forward, so a rule can
// only ever consume what it fully described.
// ---------------------------------------------------------------------------

fn digits(input: &[u8], at: usize, count: usize) -> bool {
    input.len() >= at + count && input[at..at + count].iter().all(u8::is_ascii_digit)
}

fn is_base62(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// `YYYY-MM-DDThh:mm:ss` plus an optional `.fff…` fraction and an optional
/// `Z` / `+hh:mm` / `-hh:mm` zone.
fn scan_iso8601(input: &[u8], at: usize) -> Option<usize> {
    if !digits(input, at, 4) {
        return None;
    }
    let mut i = at + 4;
    for (sep, width) in [(b'-', 2usize), (b'-', 2)] {
        if input.get(i) != Some(&sep) || !digits(input, i + 1, width) {
            return None;
        }
        i += 1 + width;
    }
    // Require the time component. A bare date is not masked: a differing date in
    // a non-timestamp field is a real difference.
    if !matches!(input.get(i), Some(b'T' | b't' | b' ')) || !digits(input, i + 1, 2) {
        return None;
    }
    i += 3;
    for _ in 0..2 {
        if input.get(i) != Some(&b':') || !digits(input, i + 1, 2) {
            return None;
        }
        i += 3;
    }
    if input.get(i) == Some(&b'.') {
        let frac = i + 1;
        let mut j = frac;
        while j < input.len() && input[j].is_ascii_digit() {
            j += 1;
        }
        if j == frac {
            return None;
        }
        i = j;
    }
    match input.get(i) {
        Some(b'Z' | b'z') => i += 1,
        Some(b'+' | b'-') if digits(input, i + 1, 2) => {
            i += 3;
            if input.get(i) == Some(&b':') && digits(input, i + 1, 2) {
                i += 3;
            } else if digits(input, i, 2) {
                i += 2;
            }
        }
        _ => {}
    }
    Some(i - at)
}

/// `<prefix>_` + 12 lowercase hex + 14 base62, bounded on both sides.
fn scan_opencode_id(input: &[u8], at: usize) -> Option<usize> {
    // The prefix must start a token, otherwise `sub_ses_00…` would half-match.
    if at > 0 && (is_base62(input[at - 1]) || input[at - 1] == b'_') {
        return None;
    }
    let prefix = ID_PREFIXES
        .iter()
        .find(|p| input[at..].starts_with(p.as_bytes()))?;
    let mut i = at + prefix.len();
    if input.get(i) != Some(&b'_') {
        return None;
    }
    i += 1;
    if input.len() < i + 26 {
        return None;
    }
    if !input[i..i + 12]
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return None;
    }
    if !input[i + 12..i + 26].iter().copied().all(is_base62) {
        return None;
    }
    let end = i + 26;
    if input.get(end).copied().is_some_and(is_base62) {
        return None;
    }
    Some(end - at)
}

/// Canonical hyphenated UUID, bounded on both sides.
fn scan_uuid(input: &[u8], at: usize) -> Option<usize> {
    if at > 0 && (input[at - 1].is_ascii_hexdigit() || input[at - 1] == b'-') {
        return None;
    }
    let mut i = at;
    for (idx, width) in [8usize, 4, 4, 4, 12].iter().enumerate() {
        if idx > 0 {
            if input.get(i) != Some(&b'-') {
                return None;
            }
            i += 1;
        }
        if input.len() < i + width || !input[i..i + width].iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        i += width;
    }
    if input
        .get(i)
        .copied()
        .is_some_and(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return None;
    }
    Some(i - at)
}

/// The run of digits in the port position of a loopback authority.
///
/// Matches the digits only; the host and colon stay in the diff, so the *host*
/// remains a compared value.
fn scan_loopback_port(input: &[u8], at: usize) -> Option<usize> {
    const HOSTS: &[&[u8]] = &[b"127.0.0.1", b"localhost", b"[::1]", b"::1"];
    let before = input.get(..at)?;
    let authority = before.strip_suffix(b":")?;
    if !HOSTS.iter().any(|h| authority.ends_with(h)) {
        return None;
    }
    let mut i = at;
    while i < input.len() && input[i].is_ascii_digit() {
        i += 1;
    }
    // A port is one to five digits; a longer run is some other number.
    if i == at || i - at > 5 {
        return None;
    }
    if input.get(i).copied().is_some_and(is_base62) {
        return None;
    }
    Some(i - at)
}

/// The run of digits directly after an explicit pid label, when plausible.
///
/// Matches the digits only; the label stays in the diff.
fn scan_labelled_pid(input: &[u8], at: usize) -> Option<usize> {
    const LABELS: &[&[u8]] = &[b"\"pid\":", b"\"pid\": ", b"pid=", b"pid: "];
    let before = input.get(..at)?;
    let label = LABELS.iter().copied().find(|l| before.ends_with(l))?;
    // Do not fire on `parentpid=` or `mypid=`.
    let head = &before[..before.len() - label.len()];
    if head
        .last()
        .copied()
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    let mut i = at;
    while i < input.len() && input[i].is_ascii_digit() {
        i += 1;
    }
    if i == at {
        return None;
    }
    let value: u64 = std::str::from_utf8(&input[at..i]).ok()?.parse().ok()?;
    if value < 2 {
        return None;
    }
    Some(i - at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin. Adding, removing or renaming a default rule must be a reviewed
    /// act, because widening normalization is the one change that can make this
    /// whole crate stop meaning anything.
    #[test]
    fn default_rule_names_are_pinned() {
        assert_eq!(
            Normalizer::default().rule_names(),
            vec![
                "iso8601-timestamp",
                "opencode-id",
                "uuid",
                "loopback-port",
                "labelled-pid",
            ],
            "the default rule set changed; justify it in the module docs and here"
        );
    }

    #[test]
    fn every_rule_documents_itself() {
        for rule in Normalizer::default().rules() {
            assert!(
                rule.why().len() > 40,
                "rule {} has no real justification",
                rule.name()
            );
            assert!(
                rule.placeholder().contains('<'),
                "rule {} placeholder should be visibly a placeholder",
                rule.name()
            );
        }
    }

    fn norm(input: &str) -> String {
        Normalizer::default().apply(input).0
    }

    #[test]
    fn none_changes_nothing() {
        let input = "ses_0197d5f0a1b2cdefghijKLMNop 2026-08-05T01:02:03.456Z 127.0.0.1:54321";
        let (out, fired) = Normalizer::none().apply(input);
        assert_eq!(out, input);
        assert!(fired.is_empty());
    }

    #[test]
    fn iso8601_masks_a_full_instant_only() {
        assert_eq!(norm("at 2026-04-28T21:18:45.535Z ok"), "at <TIMESTAMP> ok");
        assert_eq!(norm("at 2026-04-28T21:18:45Z ok"), "at <TIMESTAMP> ok");
        assert_eq!(norm("at 2026-04-28T21:18:45+02:00 ok"), "at <TIMESTAMP> ok");
        assert_eq!(norm("at 2026-04-28 21:18:45 ok"), "at <TIMESTAMP> ok");
        // A bare date is NOT a timestamp: a differing release date must diverge.
        assert_eq!(norm("released 2026-04-28"), "released 2026-04-28");
        assert_eq!(norm("1.18.13"), "1.18.13");
        assert_eq!(norm("2026-13"), "2026-13");
    }

    #[test]
    fn opencode_id_masks_only_the_real_shape() {
        // 12 lowercase hex + 14 base62, per packages/schema/src/identifier.ts.
        assert_eq!(norm("ses_0197d5f0a1b2cdefghijKLMNop done"), "<ID> done");
        assert_eq!(norm("part prt_0197d5f0a1b2cdefghijKLMNop!"), "part <ID>!");
        // Wrong length, wrong alphabet, unknown prefix, or glued to a token.
        assert_eq!(
            norm("ses_0197d5f0a1b2cdefghijKLMNo"),
            "ses_0197d5f0a1b2cdefghijKLMNo"
        );
        assert_eq!(
            norm("ses_ZZ97d5f0a1b2cdefghijKLMNop"),
            "ses_ZZ97d5f0a1b2cdefghijKLMNop"
        );
        assert_eq!(
            norm("nope_0197d5f0a1b2cdefghijKLMNop"),
            "nope_0197d5f0a1b2cdefghijKLMNop"
        );
        assert_eq!(
            norm("xses_0197d5f0a1b2cdefghijKLMNop"),
            "xses_0197d5f0a1b2cdefghijKLMNop"
        );
        // A model or tool name is untouched.
        assert_eq!(norm("tool bash"), "tool bash");
        assert_eq!(
            norm("claude-haiku-4-5-20251001"),
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn uuid_masks_the_canonical_form_only() {
        assert_eq!(
            norm("id 3f2504e0-4f89-11d3-9a0c-0305e82c3301."),
            "id <UUID>."
        );
        // An unhyphenated hex blob is a content hash; it must survive.
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(norm(sha), sha);
        assert_eq!(
            norm("3f2504e0-4f89-11d3-9a0c-0305e82c33"),
            "3f2504e0-4f89-11d3-9a0c-0305e82c33"
        );
    }

    #[test]
    fn loopback_port_masks_the_port_digits_and_keeps_the_host() {
        assert_eq!(
            norm("http://127.0.0.1:54321/x"),
            "http://127.0.0.1:<PORT>/x"
        );
        assert_eq!(norm("localhost:8080"), "localhost:<PORT>");
        assert_eq!(norm("http://[::1]:41234/"), "http://[::1]:<PORT>/");
        // A configured port on a real host is semantic and must diverge.
        assert_eq!(norm("api.anthropic.com:443"), "api.anthropic.com:443");
        assert_eq!(norm("\"port\": 4096"), "\"port\": 4096");
        assert_eq!(norm("127.0.0.1:"), "127.0.0.1:");
        // The wildcard bind is a different address, so its port is compared.
        assert_eq!(norm("0.0.0.0:8080"), "0.0.0.0:8080");
        // A differing loopback host still diverges because the host is kept.
        assert_ne!(norm("127.0.0.1:1"), norm("localhost:1"));
    }

    #[test]
    fn labelled_pid_masks_the_digits_and_keeps_the_label() {
        assert_eq!(norm("{\"pid\":48213}"), "{\"pid\":<PID>}");
        assert_eq!(norm("pid=48213 "), "pid=<PID> ");
        // A subject that failed to record a pid must still diverge.
        assert_eq!(norm("{\"pid\":0}"), "{\"pid\":0}");
        assert_eq!(norm("{\"pid\":1}"), "{\"pid\":1}");
        assert_eq!(norm("{\"pid\":null}"), "{\"pid\":null}");
        assert_eq!(norm("parentpid=48213"), "parentpid=48213");
    }

    #[test]
    fn literal_masks_are_exact_and_longest_first() {
        let n = Normalizer::default()
            .mask_literal("temp-root", "/tmp/oc-abc123", "<TEMP>")
            .mask_literal("temp-data", "/tmp/oc-abc123/data", "<DATA>");
        let (out, fired) = n.apply("/tmp/oc-abc123/data/x and /tmp/oc-abc123/y and /tmp/other");
        assert_eq!(out, "<DATA>/x and <TEMP>/y and /tmp/other");
        assert_eq!(fired.get("temp-data"), Some(&1));
        assert_eq!(fired.get("temp-root"), Some(&1));
    }

    #[test]
    fn a_literal_mask_does_not_become_a_pattern() {
        let n = Normalizer::none().mask_literal("temp", "/tmp/oc-abc123", "<TEMP>");
        // A neighbouring temp dir is a different place and must survive.
        assert_eq!(n.apply("/tmp/oc-abc124/x").0, "/tmp/oc-abc124/x");
    }

    #[test]
    fn apply_is_idempotent_and_reports_what_it_masked() {
        let n = Normalizer::default();
        let once = n.apply("ses_0197d5f0a1b2cdefghijKLMNop at 2026-04-28T21:18:45Z");
        let twice = n.apply(&once.0);
        assert_eq!(once.0, twice.0);
        assert_eq!(once.1.get("opencode-id"), Some(&1));
        assert_eq!(once.1.get("iso8601-timestamp"), Some(&1));
    }

    #[test]
    fn multibyte_text_survives_normalization() {
        let input = "路径 /配置 → ses_0197d5f0a1b2cdefghijKLMNop ✅";
        assert_eq!(norm(input), "路径 /配置 → <ID> ✅");
    }
}
