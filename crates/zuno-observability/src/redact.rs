//! One redaction policy, shared by every sink.
//!
//! # Why not per sink
//!
//! `tracing` gives each layer its own [`Visit`] pass over the same event, and a
//! layer cannot rewrite an event for the layers behind it. So there is no single
//! point in the subscriber pipeline where a record can be scrubbed once for
//! everybody, and redaction written "at the sink" means one copy of the rule per
//! sink — three today, plus whatever the next author adds. That is exactly how the
//! SQLite store came to redact `command` while the plaintext file and stderr
//! printed it verbatim.
//!
//! This module is the alternative: the rule exists once, and each sink is wired so
//! that an unredacted field pass is not expressible.
//!
//! - The SQLite store's visitor has no constructor other than one that wraps
//!   itself in [`Redacting`], so a new call site there cannot skip it.
//! - [`text_layer`] is the only way this crate builds a `fmt` sink, and it pins
//!   [`RedactingFields`] as the field formatter.
//! - `tests/every_text_sink_redacts.rs` fails if any other file in this crate builds
//!   a `fmt` sink instead of going through [`text_layer`], or re-installs a field
//!   formatter on one. That guard is deliberately crate-scoped: it cannot see a sink
//!   built in another crate, so `zuno-observability` has to stay the only crate that
//!   constructs a production `tracing_subscriber` sink.
//!
//! A fourth sink in this crate therefore either reuses one of those two entry points
//! or trips a test; it cannot quietly ship a third spelling of the rule.

use std::fmt;

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::field::{RecordFields, VisitOutput as _};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::{DefaultVisitor, FmtSpan, Format, FormatFields, Writer};
use tracing_subscriber::registry::LookupSpan;

/// What replaces a sensitive value, in every sink.
///
/// Public because it is operator-visible output: a client that reads a plaintext log
/// or a stored record can recognize a scrubbed value without hardcoding the spelling.
/// Integration tests build their expected substrings from it for the same reason —
/// three sinks agreeing on a literal that only lives in this file is a coincidence,
/// not a guarantee.
///
/// Distinct from `zuno_auth::REDACTED`, which is `<redacted>` and stands in for a
/// secret's `Display`. The two spellings are intentional: this one marks a log field
/// the observability policy scrubbed, that one marks a value the secret type never
/// reveals. Import either by path rather than by bare name.
pub const REDACTED: &str = "[redacted]";

/// Field-name words that mean "this value is a payload".
///
/// A name matches when one of these appears as a whole component run, so `prompt`,
/// `system.prompt`, `rendered_prompt`, `prompt_text`, and `promptText` all match.
/// Multi-word entries are spelled in `snake_case` and matched word by word.
///
/// Every entry is spelled in the **singular**, and the matcher also accepts the
/// regular English plural of the entry's last word — `cookie` matches `cookies`,
/// `body` matches `bodies`, `api_key` matches `api_keys`. A plural is the natural Rust
/// spelling for a collection of the same payload (`tracing::warn!(cookies = %jar)`),
/// so a singular-only rule would leave the commonest spelling of a whole class in the
/// clear. `plural_spellings_of_every_payload_word_are_matched` fails if an entry's
/// plural is irregular enough that the rule misses it.
///
/// One plural is genuinely ambiguous rather than covered: `tokens` is both the plural
/// of a credential and the unit a prompt is measured in. [`denies`] resolves it — the
/// measurement reading wins only after a [token-accounting
/// word](TOKEN_ACCOUNTING_WORDS), so `prompt_tokens` stays readable while `tokens`,
/// `auth_tokens`, `total_tokens`, `secret_tokens`, and `credential_tokens` are all
/// redacted.
/// `the_only_plural_that_collides_with_a_measurement_word_is_token` fails if a future
/// entry introduces a second collision, because that entry would otherwise inherit
/// `tokens`'s resolution without anyone deciding it should.
///
/// The list plus the plural rule covers every class `docs/logging.md` promises is
/// scrubbed — prompt, command, request body, raw tool input, output, credential,
/// token, cookie — in both the singular and the plural, plus the raw subprocess
/// streams, which are tool output under a different name. A bare `key`/`keys` is
/// deliberately absent: `zuno-config` logs a configuration key name under exactly that
/// field name, so the bare word does not classify a payload.
///
/// # What a name-based rule cannot cover
///
/// This vocabulary classifies the field *name*, so an emitter that gives a payload a
/// name outside it is not reached. `message` is the specific gap worth naming: it is the
/// field `tracing` uses for the event's own text, [`sensitive_field`] deliberately lets
/// it through (redacting it would blank every log line in every sink), and
/// `DefaultVisitor` prints it *bare*, without a `name=` prefix. So
/// `debug!(message = %raw_stderr, "…")` renders the payload where the event text belongs.
/// `no_crate_emits_an_unexpected_message_field` in `tests/every_text_sink_redacts.rs` is
/// the tripwire for that, and it is a tripwire rather than a fix because the fix belongs
/// in the emitter.
const PAYLOAD_WORDS: &[&str] = &[
    "authorization",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "private_key",
    "password",
    "passphrase",
    "secret",
    "credential",
    "cookie",
    "token",
    "bearer",
    "signature",
    "prompt",
    "content",
    "body",
    "command",
    "input",
    "output",
    "report",
    "stdin",
    "stdout",
    "stderr",
];

/// Words that turn a payload name into a bounded measurement of that payload.
///
/// A caller who must not log a payload is supposed to log its length, its digest, or
/// a correlation id instead; if those were redacted too, the safe alternative would
/// be worthless. So this list narrows a match — but only after the payload word (see
/// [`sensitive_field`]), which is what keeps it from ever un-redacting a name the
/// payload words alone would deny.
///
/// A word belongs here only if a field ending in it cannot plausibly hold the payload
/// itself. `output_bytes` is a count; `output_lines` and `output_text` are the thing
/// itself, so `lines` and `text` are deliberately absent. Digest words are absent
/// too: a digest of a low-entropy secret is not a safe thing to publish.
///
/// `tokens` is the one word here that is also the plural of a [payload
/// word](PAYLOAD_WORDS). [`denies`] resolves that collision in favour of the payload
/// reading unless an earlier component is a [token-accounting
/// word](TOKEN_ACCOUNTING_WORDS).
const MEASUREMENT_WORDS: &[&str] = &[
    "bytes",
    "len",
    "length",
    "size",
    "count",
    "tokens",
    "budget",
    "limit",
    "remaining",
    "total",
    "max",
    "min",
    "id",
    "ids",
    "uuid",
    "index",
    "ordinal",
    "seq",
    "attempt",
    "ms",
    "millis",
    "micros",
    "nanos",
    "secs",
    "seconds",
    "elapsed",
    "duration",
];

/// The [payload words](PAYLOAD_WORDS) whose token count is a thing a caller legitimately
/// logs, and therefore the only ones that license the measurement reading of `tokens`.
///
/// # Why this is not "any payload word"
///
/// The carve-out exists for one purpose: provider and compaction accounting reports how
/// many tokens a prompt, an output, an input, a piece of content, or a command occupied.
/// An earlier revision licensed the measurement reading after *any* payload word, which
/// inverted the severity ordering the rule is supposed to enforce — the more explicitly
/// credential-named the prefix, the more likely the value passed. Measured through the
/// shipped predicate at the time: `secret_token` was redacted but `secret_tokens` was
/// not, and the same held for `credential_tokens`, `bearer_tokens`, `password_tokens`,
/// `api_key_tokens`, `cookie_tokens`, `passphrase_tokens`, `private_key_tokens`, and
/// `signature_tokens`, while `auth_tokens` and `id_tokens` *were* redacted precisely
/// because their prefix is not a payload word at all.
///
/// Narrowing the licence to this list is a pure narrowing of the one denial-removing rule
/// in [`denies`], so it can only add denials. What it adds is over-denial on names like
/// `stdout_tokens`, `body_tokens`, and `report_tokens`: spell the accounting class —
/// `output_tokens` — to log the count. Nothing in the workspace emits any `*_tokens`
/// tracing field today, so nothing legitimate is affected.
///
/// Every entry must also appear in [`PAYLOAD_WORDS`], which
/// `every_token_accounting_word_is_a_payload_word` pins: a licence granted by a word the
/// payload vocabulary does not know would never fire.
const TOKEN_ACCOUNTING_WORDS: &[&str] = &["prompt", "output", "input", "content", "command"];

/// Whether a field's value must never reach a sink.
///
/// The name is read as a sequence of components (see [`Components`]) and denied when
/// a [payload word](PAYLOAD_WORDS) appears as a component run, in the singular or the
/// regular plural. Only a run followed entirely by [measurement
/// words](MEASUREMENT_WORDS) is allowed through, so `command`, `command_line`, and
/// `commands` are redacted while `command_bytes` — a length, not a payload — is not.
///
/// # Reduction direction
///
/// This predicate performs four reductions on a field name: lowercasing, folding
/// punctuation to a component separator, splitting camelCase, and reading a trailing
/// `s`/`ies` as the plural of a payload word. Every one of them is on the **deny** side,
/// and the shape of the code is what enforces that rather than a convention:
///
/// - Each reduction only ever produces more *candidate payload runs* — a reading of the
///   name in which a payload word appears.
/// - [`denies`] judges every candidate run on its own tail and returns on the first run
///   that is not fully measured. So an additional run can only add a denial; it can never
///   lengthen the run the measurement carve-out is applied to, which is how a reduction
///   would widen an allow. `authorization_input_bytes` is the case that pins this: reading
///   `input` as a payload word too must not turn "authorization, then something that is
///   not a measurement" into "an `authorization_input`, measured in bytes".
/// - The single exception, and therefore the only rule here that can *remove* a
///   denial, is the `tokens` resolution in [`denies`]. It can only fire on a
///   one-component run that matched *only* through the plural reading, is spelled as
///   a [measurement word](MEASUREMENT_WORDS), and follows a [token-accounting
///   word](TOKEN_ACCOUNTING_WORDS) — a case no earlier rule matched at all — so it cannot
///   un-deny a name either shipped rule denied. Its licence is deliberately *not* "any
///   payload word": granting it that widely inverted the severity ordering, letting
///   `secret_tokens` and `credential_tokens` through while their singulars were denied.
/// - The same licence governs the measurement carve-out's tail (see
///   [`measures_a_payload`]), because the reduction can otherwise widen an allow *without*
///   any rule removing a denial: `tokens` reads as a payload plural at one position and as
///   a measurement word at another, so an unlicensed `tokens` used to lend its measurement
///   spelling to the run in front of it and allow `secret_tokens_bytes` while
///   `secret_token_bytes` was denied. Pluralizing a component must not buy an allow.
///
/// A sweep over the payload, measurement, and filler vocabulary in two- and three-word
/// combinations is what found that second case, and after the licence the only remaining
/// "singular denied, plural allowed" pairs are the documented carve-out itself — the
/// accounting classes, `prompt_token` denied and `prompt_tokens` readable.
///
/// `widening_never_un_redacts_a_name_the_shipped_rule_matched` pins all of that against
/// both shipped rules over a generated corpus that carries plural spellings.
///
/// The non-ASCII check below is not a reduction at all — it resolves nothing and only
/// returns a denial, so it cannot participate in either direction.
///
/// A name that cannot be statically resolved against the vocabulary is denied rather
/// than waved through: one with no ASCII alphanumeric component at all, and one that
/// contains any non-ASCII byte (see the note in the body).
pub(crate) fn sensitive_field(name: &str) -> bool {
    // The vocabulary is ASCII, and [`Components`] treats every byte of a non-ASCII
    // character as a separator — so a non-ASCII byte *inside* a word hides the payload
    // spelling it interrupts. `p\u{0430}ssword` (Cyrillic `а`) reads as `p` then `ssword`,
    // and `pass\u{200b}word` as `pass` then `word`; neither spells a payload word. That is
    // not a name that can be shown to be safe, so it fails closed. Deny-side only: a
    // non-ASCII name whose ASCII components already spell a payload word was denied below
    // anyway, so this can add a denial and never remove one.
    if !name.is_ascii() {
        return true;
    }
    // Two component readings of the same name; either one denying is enough. The folded
    // reading keeps `apikey` and `sha256` whole, the camelCase reading splits
    // `accessToken`, and a union of denials means neither reading can excuse the other.
    denies(Components::folded(name)) || denies(Components::camel_split(name))
}

/// Whether one reading of a field name denies the value.
///
/// Every [payload word](PAYLOAD_WORDS) run in the name is judged separately: the value
/// is denied unless *each* run is followed only by [measurement
/// words](MEASUREMENT_WORDS). Judging each run rather than only the last one is what
/// makes the vocabulary safe to extend — see the reduction note on [`sensitive_field`].
fn denies(components: Components<'_>) -> bool {
    let total = components.count();
    let mut start = components;
    let mut position = 0_usize;
    let mut counted_a_payload = false;
    loop {
        let mut next = start;
        let Some(component) = next.next() else {
            break;
        };
        // `tokens` is both the plural of `token` and the unit a prompt is measured in.
        // The measurement reading wins only once an earlier component named one of the
        // token-accounting classes the carve-out exists for, so `prompt_tokens` and
        // `output_tokens` count while bare `tokens`, `auth_tokens`, `id_tokens`,
        // `secret_tokens`, and `credential_tokens` are credentials. Two deliberate
        // over-denials follow from that: a counting qualifier alone does not name a
        // payload (`total_tokens`, `max_tokens`), and neither does a payload class that is
        // not token-accounted (`stdout_tokens`, `body_tokens`). Spell the accounting class
        // — `prompt_tokens`, `output_tokens` — to log the count.
        let measures = counted_a_payload && is_measurement_word(component);
        for word in PAYLOAD_WORDS {
            if let Some(run) = payload_run(start, word) {
                // A multi-component run spells the payload out — `access_tokens` says
                // "access token" — so only a lone ambiguous component defers.
                if run.plural && run.length == 1 && measures {
                    continue;
                }
                counted_a_payload |= is_token_accounting_word(word);
                let end = position + run.length;
                // The payload word is the last thing the name says, or something after
                // it does not measure it. The licence travels into the tail check too:
                // without it, `tokens` would lend its measurement spelling to an earlier
                // run and read `secret_tokens_bytes` as "a byte count of a measured
                // secret" while `secret_token_bytes` was denied.
                let licensed = counted_a_payload;
                if end == total
                    || !components
                        .skip(end)
                        .all(|component| measures_a_payload(component, licensed))
                {
                    return true;
                }
            }
        }
        start = next;
        position += 1;
    }

    // Nothing classifiable: fail closed rather than pass silently.
    total == 0
}

/// A [payload word](PAYLOAD_WORDS) matched at the front of a component sequence.
#[derive(Clone, Copy)]
struct Run {
    /// How many components the word consumed.
    length: usize,
    /// Whether the last component matched only through the plural reading.
    plural: bool,
}

/// The run `word` consumes at the front of `components`, or `None` when it does not
/// match there.
fn payload_run(components: Components<'_>, word: &str) -> Option<Run> {
    let mut components = components;
    let mut length = 0_usize;
    let mut plural = false;
    let mut parts = word.split('_').peekable();
    while let Some(part) = parts.next() {
        let component = components.next()?;
        match spells(component, part) {
            Some(Number::Singular) => {}
            // Only the entry's last word pluralizes: `api_keys`, never `apis_key`.
            Some(Number::Plural) if parts.peek().is_none() => plural = true,
            _ => return None,
        }
        length += 1;
    }
    Some(Run { length, plural })
}

/// Which spelling of a payload word a component matched.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Number {
    Singular,
    Plural,
}

/// Whether `component` spells `word`, in the singular or the regular English plural.
///
/// Case-insensitive and allocation-free: `word` is an ASCII-lowercase constant and a
/// component is a run of ASCII alphanumerics, so both are compared as byte slices.
fn spells(component: &str, word: &str) -> Option<Number> {
    let component = component.as_bytes();
    let word = word.as_bytes();
    if component.eq_ignore_ascii_case(word) {
        return Some(Number::Singular);
    }
    // A bare `s` covers every regular plural here except the consonant-`y` class, so
    // `body` -> `bodies` is checked as well. Both readings are accepted rather than
    // chosen between: guessing the class wrong would leave a payload spelling
    // unclassified, while over-matching a spelling nobody writes (`bodys`) can only add
    // a denial.
    let s_plural = component.len() == word.len() + 1
        && component[word.len()..].eq_ignore_ascii_case(b"s")
        && component[..word.len()].eq_ignore_ascii_case(word);
    let ies_plural = word.last() == Some(&b'y')
        && component.len() == word.len() + 2
        && component[word.len() - 1..].eq_ignore_ascii_case(b"ies")
        && component[..word.len() - 1].eq_ignore_ascii_case(&word[..word.len() - 1]);
    (s_plural || ies_plural).then_some(Number::Plural)
}

fn is_measurement_word(component: &str) -> bool {
    MEASUREMENT_WORDS
        .iter()
        .any(|word| component.eq_ignore_ascii_case(word))
}

/// Whether a matched [payload word](PAYLOAD_WORDS) licenses the measurement reading of
/// `tokens`.
///
/// The argument is a [`PAYLOAD_WORDS`] entry, not a component off the field name, so this
/// is an exact comparison between two ASCII-lowercase constants.
fn is_token_accounting_word(word: &str) -> bool {
    TOKEN_ACCOUNTING_WORDS.contains(&word)
}

/// Whether `component` reads as a bounded measurement of the payload run before it.
///
/// [`MEASUREMENT_WORDS`] holds one word — `tokens` — that is also the plural of a [payload
/// word](PAYLOAD_WORDS), and a word that can be the payload cannot also be trusted to
/// measure it. So an ambiguous spelling only measures under `licensed`: some earlier
/// component named a [token-accounting class](TOKEN_ACCOUNTING_WORDS).
///
/// This is the second half of the same resolution [`denies`] applies to a lone `tokens`
/// run, and it exists because the two halves disagreeing produced a real asymmetry:
/// `secret_token_bytes` was denied while `secret_tokens_bytes` was allowed, because
/// `tokens` — but not `token` — extended the tail the measurement carve-out approved for
/// the `secret` run in front of it. Pluralizing a component must not buy an allow.
///
/// The ambiguity is derived from the two vocabularies rather than spelled out, so a future
/// payload word whose plural lands in [`MEASUREMENT_WORDS`] gets the same treatment
/// automatically; `the_only_plural_that_collides_with_a_measurement_word_is_token` still
/// fails on it, because whether the new word belongs in [`TOKEN_ACCOUNTING_WORDS`] is a
/// decision for a person.
fn measures_a_payload(component: &str, licensed: bool) -> bool {
    is_measurement_word(component) && (licensed || !is_plural_of_a_payload_word(component))
}

/// Whether `component` is the plural spelling of a whole [payload word](PAYLOAD_WORDS).
///
/// Multi-word entries cannot match: [`spells`] compares the entry as one string, and a
/// component never contains a separator.
fn is_plural_of_a_payload_word(component: &str) -> bool {
    PAYLOAD_WORDS
        .iter()
        .any(|word| spells(component, word) == Some(Number::Plural))
}

/// One reading of a field name as a sequence of components.
///
/// A component is a run of ASCII alphanumerics. Every other character — `_`, `-`,
/// `.`, `:`, `/`, whitespace, and every byte of a non-ASCII character — separates, so
/// no punctuation spelling hides a payload word. Nothing is allocated: the components
/// are borrowed slices of the field name, which is a `&'static str` from the callsite
/// metadata and is visited once per sink on every event.
#[derive(Clone, Copy)]
struct Components<'name> {
    rest: &'name str,
    camel_split: bool,
}

impl<'name> Components<'name> {
    /// Splits on punctuation only, keeping `apikey` whole.
    fn folded(name: &'name str) -> Self {
        Self {
            rest: name,
            camel_split: false,
        }
    }

    /// Also cuts at camelCase and letter/digit boundaries, so `accessToken` reads as
    /// `access` then `token`.
    fn camel_split(name: &'name str) -> Self {
        Self {
            rest: name,
            camel_split: true,
        }
    }
}

impl<'name> Iterator for Components<'name> {
    type Item = &'name str;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.rest.as_bytes();
        let start = bytes.iter().position(u8::is_ascii_alphanumeric)?;
        let mut end = start + 1;
        while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
            if self.camel_split && cuts_before(bytes, end) {
                break;
            }
            end += 1;
        }
        // `start` and `end` both sit just after an ASCII byte, so they are character
        // boundaries even when the name contains multi-byte characters.
        let component = &self.rest[start..end];
        self.rest = &self.rest[end..];
        Some(component)
    }
}

/// Whether a camelCase reading cuts a component before `at`.
fn cuts_before(bytes: &[u8], at: usize) -> bool {
    let previous = bytes[at - 1];
    let current = bytes[at];
    let next_is_lower = bytes.get(at + 1).is_some_and(u8::is_ascii_lowercase);
    (current.is_ascii_uppercase()
        && (previous.is_ascii_lowercase()
            || previous.is_ascii_digit()
            || (previous.is_ascii_uppercase() && next_is_lower)))
        || (current.is_ascii_digit() && previous.is_ascii_alphabetic())
        || (current.is_ascii_alphabetic() && previous.is_ascii_digit())
}

/// Forwards a field pass to `inner`, substituting [`REDACTED`] for sensitive names.
///
/// Every typed `record_*` method is forwarded in its own type rather than allowed
/// to collapse onto `record_debug`, because the store's visitor builds JSON from
/// these calls: a string arriving as `Debug` would be stored with its quotes baked
/// in, and an integer would stop being a number.
pub(crate) struct Redacting<'inner> {
    inner: &'inner mut dyn Visit,
}

impl<'inner> Redacting<'inner> {
    pub(crate) fn new(inner: &'inner mut dyn Visit) -> Self {
        Self { inner }
    }
}

/// Forwards one `Visit` method, or records the placeholder for a sensitive field.
macro_rules! forward {
    ($($method:ident($value:ty)),+ $(,)?) => {
        $(
            fn $method(&mut self, field: &Field, value: $value) {
                if sensitive_field(field.name()) {
                    self.inner.record_str(field, REDACTED);
                } else {
                    self.inner.$method(field, value);
                }
            }
        )+
    };
}

impl Visit for Redacting<'_> {
    forward!(
        record_f64(f64),
        record_i64(i64),
        record_u64(u64),
        record_i128(i128),
        record_u128(u128),
        record_bool(bool),
        record_str(&str),
        record_bytes(&[u8]),
        record_error(&(dyn std::error::Error + 'static)),
        record_debug(&dyn fmt::Debug),
    );
}

/// The field formatter every text sink in this crate uses.
///
/// It keeps [`DefaultVisitor`]'s wire format — the same `key=value` rendering,
/// ANSI handling, and bare `message` — and changes only which values reach it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RedactingFields;

impl<'writer> FormatFields<'writer> for RedactingFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'writer>, fields: R) -> fmt::Result {
        // `true` is what `DefaultFields` passes: the default `add_fields` has
        // already written the separating space when appending to existing fields.
        let mut inner = DefaultVisitor::new(writer, true);
        fields.record(&mut Redacting::new(&mut inner));
        inner.finish()
    }
}

/// Builds a redacting text sink.
///
/// Every plaintext and terminal sink in this crate is constructed here so the
/// field formatter cannot be forgotten at a call site.
pub(crate) fn text_layer<S, W>(
    writer: W,
    ansi: bool,
    span_events: FmtSpan,
) -> tracing_subscriber::fmt::Layer<S, RedactingFields, Format, W>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    W: for<'writer> MakeWriter<'writer> + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(ansi)
        .with_target(true)
        .with_level(true)
        .with_span_events(span_events)
        .fmt_fields(RedactingFields)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    /// A sink that can be read back after the subscriber is gone.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            let bytes = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for Capture {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    /// The rendering a text sink produces for a scrubbed field, built from the one
    /// constant rather than spelled again.
    fn redacted(field: &str) -> String {
        format!("{field}=\"{REDACTED}\"")
    }

    /// Runs `body` against a subscriber wired the way [`crate::init`] wires its text
    /// sinks, so each assertion below covers the shipped code path.
    fn captured(body: impl FnOnce()) -> String {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(text_layer(
            capture.clone(),
            false,
            FmtSpan::NEW | FmtSpan::CLOSE,
        ));
        tracing::subscriber::with_default(subscriber, body);
        capture.text()
    }

    #[test]
    fn a_text_sink_redacts_event_fields_and_keeps_the_rest() {
        let text = captured(|| {
            tracing::info!(
                marker = "redaction-probe",
                command = "never-print-this-command",
                command_bytes = 24,
                "sensitive event"
            );
        });

        assert!(
            !text.contains("never-print-this-command"),
            "the raw command reached a text sink:\n{text}"
        );
        assert!(text.contains(&redacted("command")), "{text}");
        assert!(text.contains("command_bytes=24"), "{text}");
        assert!(text.contains("marker=\"redaction-probe\""), "{text}");
        assert!(text.contains("sensitive event"), "{text}");
    }

    #[test]
    fn a_text_sink_redacts_span_fields_including_late_recorded_ones() {
        let text = captured(|| {
            let span = tracing::info_span!(
                "redaction_probe",
                prompt = "never-print-this-prompt",
                turn_id = "turn_probe",
                report = tracing::field::Empty,
            );
            span.record("report", "never-print-this-report");
            let _entered = span.enter();
            tracing::info!("event under a sensitive span");
        });

        for raw in ["never-print-this-prompt", "never-print-this-report"] {
            assert!(!text.contains(raw), "{raw} reached a text sink:\n{text}");
        }
        assert!(text.contains(&redacted("prompt")), "{text}");
        assert!(text.contains(&redacted("report")), "{text}");
        assert!(text.contains("turn_id=\"turn_probe\""), "{text}");
    }

    /// A sensitive field can arrive typed rather than as a string, and the
    /// placeholder has to win over the `Display` rendering of the error.
    #[test]
    fn a_typed_error_field_is_redacted_by_name() {
        let text = captured(|| {
            let failure = std::io::Error::other("never-print-this-output");
            tracing::error!(
                output = &failure as &(dyn std::error::Error + 'static),
                "failed"
            );
        });

        assert!(
            !text.contains("never-print-this-output"),
            "an error-typed sensitive field reached a text sink:\n{text}"
        );
        assert!(text.contains(&redacted("output")), "{text}");
    }

    #[test]
    fn the_needle_list_matches_compounds_and_not_metadata() {
        for sensitive in [
            "prompt",
            "system.prompt",
            "rendered_prompt",
            "Authorization",
            "api_key",
            "api-key",
            "request.access_token",
            "tool_command",
            "raw_input",
            "report",
            "tool_output",
            // Was pinned here as *benign* before the plural rule existed. A field named
            // `outputs` holds the same payload `output` does.
            "outputs",
        ] {
            assert!(sensitive_field(sensitive), "{sensitive} must be redacted");
        }
        for benign in [
            "command_bytes",
            "prompt_tokens",
            "reporter",
            "session_id",
            "turn_id",
            "provider",
            // `message` is readable on purpose and the reason is an availability one, not
            // a confidentiality one: it is the field `tracing` uses for an event's own
            // text, so redacting it would replace every log line in the plaintext file, on
            // `--print-logs` stderr, and in the `message` column of `logs.sqlite` with the
            // placeholder. That is not a trade worth making.
            //
            // It is also the one field name this predicate is blind to *and* that
            // `DefaultVisitor` renders without a `name=` prefix, so an emitter that writes
            // `debug!(message = %raw_stream, "…")` puts a payload exactly where the event
            // text belongs and it reads as prose. Measured through `text_layer`, an MCP
            // server's stderr line rendered as
            // `DEBUG …: MCP server stderr server=probe-mcp Traceback: API_KEY=sk-…` until
            // that emitter (`zuno-mcp/src/stdio.rs`) renamed the field to `stderr`, which
            // this predicate redacts; `tests/stdout_purity.rs` drives that exact shape
            // through every sink. `no_crate_emits_an_unexpected_message_field` in
            // `tests/every_text_sink_redacts.rs` is the tripwire for the next one, and
            // closing a reported emitter always belongs in the emitter, not here.
            "message",
        ] {
            assert!(!sensitive_field(benign), "{benign} must not be redacted");
        }
    }

    /// Every spelling an adversarial review pushed through this predicate and found
    /// unmatched, plus the names `docs/logging.md` already promises are scrubbed.
    /// Each case is the reviewer's literal input, not a paraphrase of it.
    #[test]
    fn every_name_the_documented_policy_promises_is_redacted() {
        for sensitive in [
            // Named verbatim by docs/logging.md: "credential, token, cookie".
            "token",
            "credential",
            "credentials",
            // Bare `token` inside a compound.
            "auth_token",
            "session_token",
            "id_token",
            // The other credential spellings the review enumerated.
            "bearer",
            "passphrase",
            "private_key",
            "signature",
            "secret_key",
            // Raw subprocess streams. A live emitter outside this crate logs
            // `stderr = %output.stderr` at WARN, inside the default INFO filter.
            "stdout",
            "stderr",
            "stdin",
            // Prefix compounds: the payload word is not the last component.
            "prompt_text",
            "command_line",
            "output_preview",
            // camelCase multi-word names.
            "accessToken",
            "authToken",
            "toolCommand",
        ] {
            assert!(
                sensitive_field(sensitive),
                "{sensitive} reaches every sink verbatim, and the documented policy \
                 says it must not"
            );
        }
    }

    /// Every PLURAL spelling of every documented class, which the singular-only rule
    /// left in the clear. Each case is an adversarial review's literal input, and the
    /// review's own emitters for two of them were `tracing::warn!(cookies = %header_jar)`
    /// and `tracing::info!(commands = ?argv)`.
    #[test]
    fn every_plural_spelling_of_a_documented_class_is_redacted() {
        for sensitive in [
            "cookies",
            "secrets",
            "passwords",
            "tokens",
            "commands",
            "prompts",
            "reports",
            "outputs",
            "inputs",
            "bodies",
            "contents",
        ] {
            assert!(
                sensitive_field(sensitive),
                "{sensitive} is the plural of a class docs/logging.md promises is \
                 scrubbed, and it reaches every sink verbatim"
            );
        }
        // The same reading applied to the multi-word and camelCase entries.
        for sensitive in [
            "api_keys",
            "access_tokens",
            "refresh_tokens",
            "private_keys",
            "auth_tokens",
            "session_tokens",
            "id_tokens",
            "credentials",
            "passphrases",
            "authorizations",
            "signatures",
            "raw_inputs",
            "accessTokens",
            "authTokens",
            "toolCommands",
            "requestBodies",
        ] {
            assert!(sensitive_field(sensitive), "{sensitive} must be redacted");
        }
    }

    /// The plural an author would actually write, for use as a tripwire below.
    fn plural_of(word: &str) -> String {
        let (prefix, last) = match word.rsplit_once('_') {
            Some((prefix, last)) => (format!("{prefix}_"), last),
            None => (String::new(), word),
        };
        let plural = match last.strip_suffix('y') {
            Some(stem) if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) => format!("{stem}ies"),
            _ => format!("{last}s"),
        };
        format!("{prefix}{plural}")
    }

    /// Every plural spelling [`spells`] accepts for `word`, which is deliberately more
    /// than the one plural an author would write.
    ///
    /// `spells` takes both readings for a word ending in `y` — `bodys` as well as
    /// `bodies` — because guessing the class wrong would leave a payload spelling
    /// unclassified. A tripwire that checks only [`plural_of`] therefore checks less than
    /// the matcher accepts, so both tripwires below iterate this instead.
    fn plural_spellings_of(word: &str) -> Vec<String> {
        let (prefix, last) = match word.rsplit_once('_') {
            Some((prefix, last)) => (format!("{prefix}_"), last),
            None => (String::new(), word),
        };
        let mut spellings = vec![format!("{prefix}{last}s")];
        if let Some(stem) = last.strip_suffix('y') {
            spellings.push(format!("{prefix}{stem}ies"));
        }
        spellings
    }

    /// The plural rule is spelled as two suffix readings, not as a list, so a future
    /// entry with an irregular plural would be silently singular-only. This fails when
    /// that happens instead of leaving the plural spelling of a payload class in the
    /// clear.
    #[test]
    fn plural_spellings_of_every_payload_word_are_matched() {
        for word in PAYLOAD_WORDS {
            // The plural an author writes has to match, and so does every other reading
            // `spells` accepts, or the two disagree about what the vocabulary covers.
            for plural in std::iter::once(plural_of(word)).chain(plural_spellings_of(word)) {
                assert!(
                    sensitive_field(&plural),
                    "{plural:?} is a plural of the payload word {word:?} that `spells` \
                     accepts, and it is not matched; the rule in `spells` needs to learn \
                     its plural class"
                );
            }
        }
    }

    /// `tokens` is the only spelling that is both the plural of a payload word and a
    /// measurement word, and [`denies`] hard-codes the resolution for it. A second
    /// collision would inherit that resolution without anyone deciding it should.
    ///
    /// The tripwire iterates every plural reading [`spells`] accepts, not only the one
    /// [`plural_of`] produces: a future payload word ending in a consonant plus `y` whose
    /// over-generated bare-`s` form happened to be a measurement word would otherwise
    /// inherit `tokens`'s resolution without tripping anything.
    #[test]
    fn the_only_plural_that_collides_with_a_measurement_word_is_token() {
        for word in PAYLOAD_WORDS {
            for plural in std::iter::once(plural_of(word)).chain(plural_spellings_of(word)) {
                if is_measurement_word(&plural) {
                    assert_eq!(
                        *word, "token",
                        "{plural:?} is both a measurement word and a plural of the payload \
                         word {word:?} that `spells` accepts; extend the resolution rule in \
                         `denies` and this test together"
                    );
                }
            }
        }
    }

    /// Every entry in [`TOKEN_ACCOUNTING_WORDS`] has to be a [`PAYLOAD_WORDS`] entry,
    /// because [`denies`] consults it with a payload-word constant. A word that is not in
    /// the payload vocabulary would silently license nothing.
    #[test]
    fn every_token_accounting_word_is_a_payload_word() {
        for word in TOKEN_ACCOUNTING_WORDS {
            assert!(
                PAYLOAD_WORDS.contains(word),
                "{word:?} licenses the measurement reading of `tokens` but is not a payload \
                 word, so the licence can never fire"
            );
        }
    }

    /// The resolution: `tokens` measures only after a [token-accounting
    /// word](TOKEN_ACCOUNTING_WORDS). A naive trailing-`s` strip would redact the
    /// documented `prompt_tokens` carve-out; leaving `tokens` singular-only would pass a
    /// list of bearer tokens through.
    ///
    /// The redacted half below is the uncomfortable direction, and it is the half an
    /// earlier revision got backwards: licensing the measurement reading after *any*
    /// payload word made the rule read `secret_tokens`, `credential_tokens`,
    /// `bearer_tokens`, `password_tokens`, `api_key_tokens`, `cookie_tokens`,
    /// `passphrase_tokens`, `private_key_tokens`, and `signature_tokens` in the clear while
    /// every one of their singulars was redacted — the more explicitly credential-named the
    /// prefix, the more likely the value passed. Each of those nine is an adversarial
    /// review's literal measured input.
    #[test]
    fn the_ambiguous_plural_measures_only_after_a_token_accounting_word() {
        for measurement in [
            "prompt_tokens",
            "promptTokens",
            "output_tokens",
            "input_tokens",
            "command_tokens",
            "content_tokens",
            "prompt_tokens_total",
        ] {
            assert!(
                !sensitive_field(measurement),
                "{measurement} names the token-accounting class it counts and must stay \
                 readable"
            );
        }
        for credential in [
            // Nothing says what is being counted, so the credential reading wins.
            "tokens",
            "auth_tokens",
            "session_tokens",
            "id_tokens",
            "tokens_used",
            // The severity inversion, in both spellings. A credential-named prefix must
            // never be the thing that licenses the measurement reading.
            "secret_tokens",
            "credential_tokens",
            "bearer_tokens",
            "password_tokens",
            "api_key_tokens",
            "cookie_tokens",
            "passphrase_tokens",
            "private_key_tokens",
            "signature_tokens",
            "authorization_tokens",
            "apiKeyTokens",
            "secretTokens",
            // Deliberate over-denial: a counting qualifier alone does not name a
            // payload, so these are redacted. Spell the accounting class —
            // `prompt_tokens`, `output_tokens` — to log the count. Nothing in the
            // workspace emits them today; `docs/logging.md` records the rule.
            "total_tokens",
            "max_tokens",
            // Deliberate over-denial of the second kind: a payload class that is not
            // token-accounted does not license the measurement reading either.
            "stdout_tokens",
            "stderr_tokens",
            "body_tokens",
            "report_tokens",
        ] {
            assert!(
                sensitive_field(credential),
                "{credential} does not name a token-accounting class, so it must fail \
                 closed"
            );
        }
        // The singular of every credential-named case above is redacted, so the plural
        // must be too: a rule whose plural is more permissive than its singular has the
        // severity ordering inverted, whatever the two verdicts happen to be.
        for stem in [
            "secret",
            "credential",
            "bearer",
            "password",
            "api_key",
            "cookie",
            "passphrase",
            "private_key",
            "signature",
        ] {
            let singular = format!("{stem}_token");
            let plural = format!("{stem}_tokens");
            assert!(sensitive_field(&singular), "{singular} must be redacted");
            assert!(
                sensitive_field(&plural),
                "{singular} is redacted but {plural} is not; the plural reading must never \
                 be more permissive than the singular it reduces to"
            );
        }
    }

    /// The measurement carve-out's *tail* obeys the same licence, because the ambiguous
    /// spelling can widen an allow from a position where no rule removes a denial.
    ///
    /// `tokens` is both a [`MEASUREMENT_WORDS`] entry and the plural of a
    /// [`PAYLOAD_WORDS`] entry. Before [`measures_a_payload`] took the licence into
    /// account, an unlicensed `tokens` still counted as a measurement word when the run in
    /// front of it asked whether its tail was all measurement: `secret_token_bytes` was
    /// denied and `secret_tokens_bytes` was allowed. That is the reduction rule's failure
    /// shape exactly — pluralizing a component bought an allow — and it is a distinct
    /// defect from the one
    /// `the_ambiguous_plural_measures_only_after_a_token_accounting_word` pins, which is
    /// about the `tokens` run itself.
    #[test]
    fn the_ambiguous_plural_cannot_lend_its_measurement_reading_to_an_earlier_run() {
        for credential in [
            "secret_tokens_bytes",
            "secret_token_bytes",
            "credential_tokens_bytes",
            "cookie_tokens_len",
            "authorization_tokens_bytes",
            "password_tokens_count",
            "secretTokensBytes",
            "apiKeyTokensLen",
        ] {
            assert!(
                sensitive_field(credential),
                "{credential} names a credential and no token-accounting class, so a \
                 trailing `tokens` must not read as a bound"
            );
        }
        for accounting in [
            "prompt_tokens_bytes",
            "output_tokens_total",
            "input_tokens_count",
            "promptTokensTotal",
        ] {
            assert!(
                !sensitive_field(accounting),
                "{accounting} names the accounting class it counts and must stay readable"
            );
        }
        // Same pairwise ordering as above, one component further out: adding `_bytes` to
        // an already-redacted name must not reveal it.
        for stem in [
            "secret",
            "credential",
            "bearer",
            "password",
            "api_key",
            "cookie",
            "passphrase",
            "private_key",
            "signature",
        ] {
            for tail in ["bytes", "len", "count", "total"] {
                let singular = format!("{stem}_token_{tail}");
                let plural = format!("{stem}_tokens_{tail}");
                assert!(sensitive_field(&singular), "{singular} must be redacted");
                assert!(
                    sensitive_field(&plural),
                    "{singular} is redacted but {plural} is not; a plural component must \
                     never lend a measurement reading that its singular does not"
                );
            }
        }
    }

    /// The measurement of a payload is the thing a caller is told to log *instead* of
    /// the payload. Redacting it too would leave no safe way to report a bound.
    #[test]
    fn a_bounded_measurement_of_a_sensitive_value_stays_visible() {
        for measurement in [
            "command_bytes",
            "prompt_tokens",
            "promptTokens",
            "token_budget",
            "output_len",
            "stdout_size",
            "stderr_bytes",
            "credential_id",
            "report_bytes",
            "body_size",
            "content_length",
            "output_count_total",
        ] {
            assert!(
                !sensitive_field(measurement),
                "{measurement} measures a payload and must stay readable"
            );
        }
    }

    /// A field name with no classifiable component cannot be shown to be safe, so it
    /// is denied instead of quietly passing.
    #[test]
    fn a_name_with_nothing_classifiable_fails_closed() {
        for opaque in ["", "_", "...", "??", "。。"] {
            assert!(
                sensitive_field(opaque),
                "{opaque:?} is not statically classifiable and must fail closed"
            );
        }
    }

    /// A non-ASCII byte inside a word breaks the payload spelling it interrupts, and no
    /// reading of the remaining pieces can show the name is safe. These are the two
    /// spellings that matter — a homoglyph and a zero-width space — written as escapes so
    /// the input is unambiguous in the source.
    #[test]
    fn a_name_that_is_not_statically_resolvable_fails_closed() {
        for unresolvable in [
            // `password` with a Cyrillic `а`. Splits as `p` then `ssword`.
            "p\u{0430}ssword",
            // A zero-width space inside `password`.
            "pass\u{200b}word",
            // The same trick on the words a live emitter uses.
            "std\u{0435}rr",
            "c\u{043e}mmand",
            "pr\u{043e}mpt",
        ] {
            assert!(
                sensitive_field(unresolvable),
                "{unresolvable:?} hides a payload word behind a non-ASCII byte and must \
                 fail closed"
            );
        }
        // The check is on the name, not on the value: an ASCII name still decides both
        // ways, so this is not a blanket denial.
        assert!(!sensitive_field("command_bytes"));
        assert!(sensitive_field("command"));
    }

    /// The two vocabularies decide opposite things, so a word in both would make the
    /// outcome depend on list order.
    #[test]
    fn the_payload_and_measurement_vocabularies_are_disjoint() {
        for payload in PAYLOAD_WORDS {
            for word in payload.split('_') {
                assert!(
                    !is_measurement_word(word),
                    "{word:?} is both a payload word and a measurement word"
                );
            }
        }
    }

    /// The rule as it shipped one revision ago: the same component reading, without the
    /// plural spellings. Kept as the second baseline the widened rule has to dominate,
    /// because the plural rule is a *reduction* — it maps `cookies` onto `cookie` — and a
    /// reduction may only ever add a denial.
    ///
    /// # This baseline is a transcription, not history
    ///
    /// `src/redact.rs` has never been committed in an intermediate state, so there is no
    /// revision a reader can diff this against. Both baselines here — this one and
    /// [`wave_one_sensitive_field`] — are the author's transcription of an unversioned
    /// revision, kept for the dominance *property* rather than as an authoritative record
    /// of what once shipped. An assertion against a transcribed baseline is only as
    /// accurate as the transcription, so the dominance argument does not rest on it alone:
    /// it also rests on the structure of [`denies`], where the only denial-removing rule is
    /// the `tokens` resolution and its precondition is a shape no other rule matches. Treat
    /// a failure here as "the widened rule is more permissive than a rule of this shape",
    /// not as "a released version behaved differently".
    fn round_two_sensitive_field(name: &str) -> bool {
        fn exact_run_length(components: Components<'_>, word: &str) -> Option<usize> {
            let mut components = components;
            let mut length = 0_usize;
            for part in word.split('_') {
                if !components.next()?.eq_ignore_ascii_case(part) {
                    return None;
                }
                length += 1;
            }
            Some(length)
        }

        fn denies_exactly(components: Components<'_>) -> bool {
            let mut start = components;
            let mut position = 0_usize;
            let mut payload_end: Option<usize> = None;
            loop {
                let mut next = start;
                if next.next().is_none() {
                    break;
                }
                for word in ROUND_TWO_PAYLOAD_WORDS {
                    if let Some(length) = exact_run_length(start, word) {
                        let end = position + length;
                        payload_end = Some(payload_end.map_or(end, |best: usize| best.max(end)));
                    }
                }
                start = next;
                position += 1;
            }

            match payload_end {
                None => position == 0,
                Some(end) if end == position => true,
                Some(end) => !components.skip(end).all(is_measurement_word),
            }
        }

        denies_exactly(Components::folded(name)) || denies_exactly(Components::camel_split(name))
    }

    /// The payload words as they shipped one revision ago.
    const ROUND_TWO_PAYLOAD_WORDS: &[&str] = &[
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "private_key",
        "password",
        "passphrase",
        "secret",
        "credential",
        "credentials",
        "cookie",
        "token",
        "bearer",
        "signature",
        "prompt",
        "content",
        "body",
        "command",
        "raw_input",
        "output",
        "report",
        "stdin",
        "stdout",
        "stderr",
    ];

    /// The rule this predicate replaced, kept only as the baseline the widened rule
    /// has to dominate.
    fn wave_one_sensitive_field(name: &str) -> bool {
        let normalized = name.to_ascii_lowercase().replace(['-', '.'], "_");
        [
            "authorization",
            "api_key",
            "apikey",
            "access_token",
            "refresh_token",
            "password",
            "secret",
            "cookie",
            "prompt",
            "content",
            "body",
            "command",
            "raw_input",
            "output",
            "report",
        ]
        .iter()
        .any(|needle| normalized == *needle || normalized.ends_with(&format!("_{needle}")))
    }

    fn capitalized(word: &str) -> String {
        let mut characters = word.chars();
        match characters.next() {
            Some(first) => first.to_ascii_uppercase().to_string() + characters.as_str(),
            None => String::new(),
        }
    }

    /// The measurement carve-out is the only rule here that can *narrow* a match, and it
    /// may only narrow names an earlier rule never matched. Anything else would be a
    /// reduction that widens an allow — the specific failure this test exists to catch is
    /// a plural extending the payload run so the carve-out gets a longer tail to approve
    /// (`secret_cookies_bytes` reading as "a byte count of secret_cookies").
    ///
    /// Both shipped rules are baselines: the wave-one exact/suffix rule and the
    /// round-two component rule without plurals. The corpus carries plural spellings, so
    /// the plural reading is actually exercised rather than assumed.
    #[test]
    fn widening_never_un_redacts_a_name_the_shipped_rule_matched() {
        let plurals = PAYLOAD_WORDS
            .iter()
            .map(|word| plural_of(word))
            .collect::<Vec<_>>();
        let vocabulary = PAYLOAD_WORDS
            .iter()
            .chain(MEASUREMENT_WORDS.iter())
            .copied()
            .chain([
                "x", "tool", "last", "rendered", "system", "request", "preview", "text", "line",
                "raw", "input", "json", "list", "reporter",
            ])
            .chain(plurals.iter().map(String::as_str))
            .collect::<Vec<_>>();

        let mut checked = 0_usize;
        let mut wave_one_denials = 0_usize;
        let mut round_two_denials = 0_usize;
        let mut newly_denied = 0_usize;
        for first in &vocabulary {
            for second in &vocabulary {
                for name in [
                    format!("{first}_{second}"),
                    format!("{first}.{second}"),
                    format!("{first}-{second}"),
                    format!("{first}{}", capitalized(second)),
                    format!("{}_{second}", first.to_ascii_uppercase()),
                    format!("{first}_{second}_bytes"),
                    format!("{first}_{second}_prompt"),
                ] {
                    checked += 1;
                    let now = sensitive_field(&name);
                    if wave_one_sensitive_field(&name) {
                        wave_one_denials += 1;
                        assert!(
                            now,
                            "{name} was redacted by the wave-one rule and is not now"
                        );
                    }
                    if round_two_sensitive_field(&name) {
                        round_two_denials += 1;
                        assert!(
                            now,
                            "{name} was redacted by the round-two rule and is not now; the \
                             plural reduction widened an allow"
                        );
                    } else if now {
                        newly_denied += 1;
                    }
                }
            }
        }

        assert!(
            checked > 10_000,
            "corpus too small to mean anything: {checked}"
        );
        assert!(
            wave_one_denials > 1_000 && round_two_denials > 1_000,
            "the corpus barely exercises the baselines ({wave_one_denials} wave-one, \
             {round_two_denials} round-two denials), so the implication is close to vacuous"
        );
        assert!(
            newly_denied > 1_000,
            "only {newly_denied} names are newly denied, so the plural rule is not \
             actually being exercised by this corpus"
        );
    }

    /// The classification through the shipped sink constructor, not just the
    /// predicate: a raw subprocess stream and a camelCase credential name.
    #[test]
    fn a_text_sink_redacts_a_subprocess_stream_and_a_camel_case_credential() {
        let text = captured(|| {
            tracing::warn!(
                hash = "0f1e2d3c",
                stderr = "never-print-this-git-stderr",
                accessToken = "never-print-this-camel-token",
                stdout_bytes = 12,
                "failed to get diff"
            );
        });

        for raw in [
            "never-print-this-git-stderr",
            "never-print-this-camel-token",
        ] {
            assert!(!text.contains(raw), "{raw} reached a text sink:\n{text}");
        }
        assert!(text.contains(&redacted("stderr")), "{text}");
        assert!(text.contains(&redacted("accessToken")), "{text}");
        assert!(text.contains("stdout_bytes=12"), "{text}");
        assert!(text.contains("hash=\"0f1e2d3c\""), "{text}");
    }

    /// Through the shipped sink constructor, with the two emitters an adversarial review
    /// wrote out literally: a plural is the natural Rust spelling for a collection, and a
    /// singular-only rule printed both of these verbatim to the plaintext file, the
    /// `--print-logs` stderr stream, and `logs.sqlite`.
    #[test]
    fn a_text_sink_redacts_a_plural_payload_field() {
        let header_jar = "session=never-print-this-cookie-jar";
        let argv = ["git", "never-print-this-argv"];
        let text = captured(|| {
            tracing::warn!(cookies = %header_jar, "cookie jar rejected");
            tracing::info!(commands = ?argv, "running");
            tracing::info!(
                prompt_tokens = 1_024,
                outputs = "never-print-this-outputs",
                "turn accounting"
            );
        });

        for raw in [
            "never-print-this-cookie-jar",
            "never-print-this-argv",
            "never-print-this-outputs",
        ] {
            assert!(!text.contains(raw), "{raw} reached a text sink:\n{text}");
        }
        for field in ["cookies", "commands", "outputs"] {
            assert!(text.contains(&redacted(field)), "{field}:\n{text}");
        }
        assert!(
            text.contains("prompt_tokens=1024"),
            "the documented token-count carve-out was redacted too:\n{text}"
        );
    }

    /// Why `pretty()` is on `tests/every_text_sink_redacts.rs`'s ban list: it is a
    /// one-token edit to the sanctioned constructor that un-redacts every field.
    ///
    /// `Layer::pretty()` does not decorate the field formatter, it *replaces* it — the
    /// builder sets `fmt_fields: format::Pretty`, so [`RedactingFields`] is gone and
    /// nothing in this module runs. `.json()` and `.map_fmt_fields(..)` replace it the same
    /// way, and `.event_format(fmt::format().pretty())` bypasses it even while it is still
    /// installed, because the `Pretty` and `Json` event formats build their own field
    /// visitor instead of calling `ctx.format_fields`.
    ///
    /// This test asserts the leak rather than the fix on purpose. The fix is the textual
    /// ban, which cannot be verified from inside the type system, and a ban whose cost is
    /// unexplained is a ban the next author removes. If a future `tracing-subscriber`
    /// makes `pretty()` compose with a custom field formatter instead of discarding it,
    /// this test fails and the ban can be revisited with evidence.
    ///
    /// Only `pretty()` is exercised: `json()` needs the `json` feature, which this
    /// workspace does not enable, so it cannot be compiled here even as a demonstration.
    #[test]
    fn the_banned_pretty_builder_really_does_un_redact_every_field() {
        let capture = Capture::default();
        // The measured input, verbatim: ONE extra method call on `text_layer`.
        let layer = text_layer(capture.clone(), false, FmtSpan::NONE).pretty();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                command = "never-print-this-command",
                prompt = "never-print-this-prompt",
                stderr = "never-print-this-git-stderr",
                "probe"
            );
        });
        let text = capture.text();

        for raw in [
            "never-print-this-command",
            "never-print-this-prompt",
            "never-print-this-git-stderr",
        ] {
            assert!(
                text.contains(raw),
                "`pretty()` no longer discards the redacting field formatter, so {raw} \
                 stayed out of the sink. That is a better world than the one this ban was \
                 written for: re-check `tracing_subscriber::fmt::Layer::pretty` and, if it \
                 now composes with a custom `FormatFields`, drop `pretty()` from \
                 `BANNED_TOKENS` in `tests/every_text_sink_redacts.rs` and delete this \
                 test.\n{text}"
            );
        }
        assert!(
            !text.contains(REDACTED),
            "the placeholder appeared, so something still redacted:\n{text}"
        );
    }
}
