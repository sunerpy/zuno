//! What a tool returns, and how oversized output is detected and preserved.
//!
//! # Scope
//!
//! This module **detects** that output exceeds the configured thresholds and
//! reports the verdict alongside the limits that produced it. It does not decide
//! what the model is shown as a result. That decision — refuse and offer the
//! `accept_large_output` opt-in, rather than hand back a silently truncated prefix
//! — belongs to the output-policy layer (todo 72), and nothing here may presume it.
//! Two layers asserting the same policy is how the two behaviours end up
//! contradicting each other; measurement and policy are split so exactly one place
//! owns the answer.
//!
//! # Matching the oracle's arithmetic
//!
//! [`measure`] reproduces `tool-output-store.ts`'s counting exactly, because a
//! disagreement means output that the TypeScript binary stores and this one does
//! not, or the reverse:
//!
//! - lines are `'\n'` occurrences plus one, so `""` is one line and `"a\n"` is two;
//! - bytes are UTF-8 bytes (`Buffer.byteLength(text, "utf-8")`), not chars;
//! - either limit alone being exceeded is enough.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zuno_config::schema::ToolOutputConfig;

/// Lines of output before it is considered oversized. `tool-output-store.ts:13`.
pub const DEFAULT_MAX_LINES: usize = 2_000;

/// Bytes of output before it is considered oversized. `50 * 1024`, `:14`.
pub const DEFAULT_MAX_BYTES: usize = 51_200;

/// The metadata key recording where full output was persisted.
///
/// Named for the oracle's persisted tool state, which carries `outputPaths` as an
/// array (`session/message-updater.ts:313`). An array because a single result can
/// spill more than once over its lifetime, and because the reader is the oracle's
/// own shape.
pub const METADATA_OUTPUT_PATHS_KEY: &str = "outputPaths";

/// The metadata key recording which files this call wrote to disk.
///
/// An array because one call can write several — `apply_patch` applies a whole patch
/// set — and the reader needs every path, not a representative one.
///
/// A tool records this *where it writes*, so no downstream consumer has to recognise a
/// writing tool by name. That mattered: the TUI's language-server hook kept a list of
/// tool ids and the list did not contain `apply_patch`, so on the models that expose
/// only `read` and `apply_patch` a successful patch was never checked. A path the tool
/// itself reported cannot fall out of a list nobody remembered to update.
pub const METADATA_WRITTEN_PATHS_KEY: &str = "writtenPaths";

/// The metadata key carrying structured file pre/post images.
///
/// This is the durable source for client surfaces that can render native diffs. The
/// existing `diff` metadata remains the compact unified patch for terminals and logs;
/// keeping both lets each consumer use the representation it actually understands.
pub const METADATA_FILE_DIFFS_KEY: &str = "fileDiffs";

/// Typed mutation-conflict details persisted for replay-capable clients.
pub const METADATA_MUTATION_CONFLICT_KEY: &str = "mutationConflict";

/// Durable human request that caused a tool-owned turn suspension.
pub const METADATA_HUMAN_REQUEST_ID_KEY: &str = "humanRequestID";

/// How the host should proceed after this successful tool result is persisted.
///
/// The default is the ordinary model tool loop. Suspension variants are reserved
/// for tools that already committed the durable state that will wake a future
/// turn. Keeping this typed and tool-owned avoids asking the model to spend
/// another provider request merely to say that it is waiting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolContinuation {
    /// Append the result and continue the ordinary model tool loop.
    #[default]
    Continue,
    /// End the current turn unless a new durable input is already available.
    YieldUntilInput,
    /// End the turn because a named durable human request is pending.
    ///
    /// The request id is carried in [`METADATA_HUMAN_REQUEST_ID_KEY`].
    WaitingForHuman,
}

impl ToolContinuation {
    const fn is_continue(&self) -> bool {
        matches!(self, Self::Continue)
    }
}

/// Terminal outcome of a structured question tool call.
///
/// This is separate from free-form tool metadata so live clients do not need to
/// parse private JSON keys or rendered output to decide how a question settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionResultStatus {
    Answered,
    Cancelled,
    Expired,
    Failed,
}

impl QuestionResultStatus {
    /// Stable spelling shared by durable metadata and client projections.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }

    /// Short human-readable label used in transcript titles.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Answered => "Answered",
            Self::Cancelled => "Cancelled",
            Self::Expired => "Expired",
            Self::Failed => "Failed",
        }
    }
}

/// Typed, host-facing result of a structured question.
///
/// The same information is also persisted in the tool result metadata. This
/// typed form is intentionally carried beside the live result so a client
/// projector can render an exact answer without receiving the tool's entire
/// private metadata map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionResultPresentation {
    status: QuestionResultStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answers: Option<Vec<Vec<String>>>,
    question_count: usize,
    elapsed_ms: u64,
}

impl QuestionResultPresentation {
    /// Creates a typed question result.
    #[must_use]
    pub fn new(
        status: QuestionResultStatus,
        answers: Option<Vec<Vec<String>>>,
        question_count: usize,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            status,
            answers,
            question_count,
            elapsed_ms,
        }
    }

    #[must_use]
    pub const fn status(&self) -> QuestionResultStatus {
        self.status
    }

    #[must_use]
    pub fn answers(&self) -> Option<&[Vec<String>]> {
        self.answers.as_deref()
    }

    #[must_use]
    pub const fn question_count(&self) -> usize {
        self.question_count
    }

    #[must_use]
    pub const fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }
}

/// Stable client spelling for a mutation conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationConflictPresentationKind {
    ReadRequired,
    StaleRead,
    ContextMismatch,
    IdenticalReplay,
}

impl From<zuno_error::ToolMutationConflictKind> for MutationConflictPresentationKind {
    fn from(value: zuno_error::ToolMutationConflictKind) -> Self {
        match value {
            zuno_error::ToolMutationConflictKind::ReadRequired => Self::ReadRequired,
            zuno_error::ToolMutationConflictKind::StaleRead => Self::StaleRead,
            zuno_error::ToolMutationConflictKind::ContextMismatch => Self::ContextMismatch,
            zuno_error::ToolMutationConflictKind::IdenticalReplay => Self::IdenticalReplay,
        }
    }
}

/// Typed client presentation for a mutation that was refused before writing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MutationConflictPresentation {
    kind: MutationConflictPresentationKind,
    resource: String,
    operation_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hunk_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hunk_header: Option<String>,
    required_action: String,
}

impl MutationConflictPresentation {
    #[must_use]
    pub fn from_conflict(conflict: &zuno_error::ToolMutationConflict) -> Self {
        Self {
            kind: conflict.kind.into(),
            resource: conflict.resource.clone(),
            operation_digest: conflict.operation_digest.clone(),
            observed_digest: conflict.observed_digest.clone(),
            hunk_index: conflict.hunk_index,
            hunk_header: conflict.hunk_header.clone(),
            required_action: conflict.required_action().to_owned(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> MutationConflictPresentationKind {
        self.kind
    }

    #[must_use]
    pub fn resource(&self) -> &str {
        &self.resource
    }

    #[must_use]
    pub fn operation_digest(&self) -> &str {
        &self.operation_digest
    }

    #[must_use]
    pub fn observed_digest(&self) -> Option<&str> {
        self.observed_digest.as_deref()
    }

    #[must_use]
    pub const fn hunk_index(&self) -> Option<usize> {
        self.hunk_index
    }

    #[must_use]
    pub fn hunk_header(&self) -> Option<&str> {
        self.hunk_header.as_deref()
    }

    #[must_use]
    pub fn required_action(&self) -> &str {
        &self.required_action
    }
}

/// Typed client presentation for a mutation whose observed side effects require inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UncertainMutationPresentation {
    applied_paths: Vec<String>,
}

impl UncertainMutationPresentation {
    #[must_use]
    pub fn new(applied_paths: Vec<String>) -> Self {
        Self { applied_paths }
    }

    #[must_use]
    pub fn applied_paths(&self) -> &[String] {
        &self.applied_paths
    }
}

/// Typed client presentation attached to one tool result.
///
/// Variants belong here rather than in a surface adapter: the producing tool is
/// the only component that can state the result authoritatively, while ACP, TUI,
/// and future clients may choose different renderings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolResultPresentation {
    Question(QuestionResultPresentation),
    MutationConflict(MutationConflictPresentation),
    UncertainMutation(UncertainMutationPresentation),
}

/// One text file modification produced by a tool call.
///
/// Paths are absolute at this boundary because ACP clients resolve edits outside the
/// agent process. `old_text` is absent only for a newly-created file; deletion is an
/// existing `old_text` with an empty `new_text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileDiff {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old_text: Option<String>,
    new_text: String,
}

impl FileDiff {
    /// Creates a changed absolute file image.
    ///
    /// Returns `None` for a relative path or an existing file whose pre/post images
    /// are identical. A new empty file remains a real change because `None` and `""`
    /// describe different filesystem states.
    #[must_use]
    pub fn new(path: &std::path::Path, old_text: Option<String>, new_text: String) -> Option<Self> {
        let diff = Self {
            path: zuno_paths::wire_path(path),
            old_text,
            new_text,
        };
        diff.is_valid().then_some(diff)
    }

    /// Absolute modified path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Original text, absent for a newly-created file.
    #[must_use]
    pub fn old_text(&self) -> Option<&str> {
        self.old_text.as_deref()
    }

    /// Text after the tool completed.
    #[must_use]
    pub fn new_text(&self) -> &str {
        &self.new_text
    }

    fn is_valid(&self) -> bool {
        std::path::Path::new(&self.path).is_absolute()
            && self.old_text.as_deref() != Some(self.new_text.as_str())
    }

    fn into_value(self) -> Value {
        let mut value = Map::new();
        value.insert("path".to_owned(), Value::String(self.path));
        if let Some(old_text) = self.old_text {
            value.insert("oldText".to_owned(), Value::String(old_text));
        }
        value.insert("newText".to_owned(), Value::String(self.new_text));
        Value::Object(value)
    }
}

/// A file a tool produced alongside its text.
///
/// The oracle types these as `Omit<FilePart, "id" | "sessionID" | "messageID">`
/// (`tool.ts:52`): the three identifiers are assigned when the part is persisted,
/// so a tool never supplies them.
///
/// `source` stays a [`Value`]. It is the `FileSource | SymbolSource` union owned by
/// the message-part layer; re-declaring it here would be a second copy of a type
/// this crate does not own, and it is pass-through payload rather than anything
/// this crate reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "file")]
pub struct Attachment {
    /// The media type, spelled `mime` on the wire.
    pub mime: String,
    /// The display name, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Where the bytes are: a `file://` path or a data URL.
    pub url: String,
    /// Provenance for a citation, when the producer knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
}

impl Attachment {
    /// An attachment with no filename and no source.
    #[must_use]
    pub fn new(mime: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            mime: mime.into(),
            filename: None,
            url: url.into(),
            source: None,
        }
    }
}

/// The result of a successful tool execution.
///
/// Its durable/model-facing fields mirror the oracle's `ExecuteResult`
/// (`tool.ts:48-53`): a `title` for the transcript, the `output` the model reads,
/// free-form `metadata`, and any `attachments`. Host continuation and typed live
/// presentation remain outside that durable payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The one-line summary shown in the transcript.
    pub title: String,
    /// The text handed to the model.
    pub output: String,
    /// Tool-specific detail for renderers and later turns.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Files produced by this call.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Typed host-facing presentation for results whose lifecycle cannot be
    /// reconstructed safely from prose.
    #[serde(skip)]
    pub presentation: Option<ToolResultPresentation>,
    /// Host-owned continuation behavior after a successful dispatch.
    #[serde(default, skip_serializing_if = "ToolContinuation::is_continue")]
    pub continuation: ToolContinuation,
}

impl ToolOutput {
    /// Text output with no metadata and no attachments.
    #[must_use]
    pub fn text(title: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            output: output.into(),
            metadata: Map::new(),
            attachments: Vec::new(),
            presentation: None,
            continuation: ToolContinuation::Continue,
        }
    }

    /// Adds one metadata entry, chaining.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Attaches this call's verification evidence, chaining.
    ///
    /// Hosts persist the receipt and gate later success claims on it, so only
    /// attach one when the tool genuinely observed the outcome it reports.
    #[must_use]
    pub fn with_verification(self, receipt: &crate::VerificationReceipt) -> Self {
        self.with_metadata(
            crate::VERIFICATION_METADATA_KEY,
            receipt.to_metadata_value(),
        )
    }

    /// Adds one attachment, chaining.
    #[must_use]
    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Attaches one typed client presentation, chaining.
    #[must_use]
    pub fn with_presentation(mut self, presentation: ToolResultPresentation) -> Self {
        self.presentation = Some(presentation);
        self
    }

    /// Requests host-managed suspension after this result is durably appended.
    #[must_use]
    pub fn with_continuation(mut self, continuation: ToolContinuation) -> Self {
        self.continuation = continuation;
        self
    }

    /// Adds one structured file diff, chaining.
    #[must_use]
    pub fn with_file_diff(mut self, diff: FileDiff) -> Self {
        let entry = diff.into_value();
        match self.metadata.get_mut(METADATA_FILE_DIFFS_KEY) {
            Some(Value::Array(diffs)) => diffs.push(entry),
            Some(_) | None => {
                self.metadata.insert(
                    METADATA_FILE_DIFFS_KEY.to_owned(),
                    Value::Array(vec![entry]),
                );
            }
        }
        self
    }

    /// Valid structured file diffs recorded on this result.
    ///
    /// Durable or extension-provided metadata is validated again here: malformed
    /// entries and relative paths never cross into a client protocol projection.
    #[must_use]
    pub fn file_diffs(&self) -> Vec<FileDiff> {
        self.metadata
            .get(METADATA_FILE_DIFFS_KEY)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| serde_json::from_value::<FileDiff>(value.clone()).ok())
            .filter(FileDiff::is_valid)
            .collect()
    }

    /// Measures this output against `limits`.
    #[must_use]
    pub fn measure(&self, limits: OutputLimits) -> SizeMeasurement {
        measure(&self.output, limits)
    }

    /// Appends a persisted-output path to [`METADATA_OUTPUT_PATHS_KEY`].
    ///
    /// Bookkeeping, not policy: it records where the full text can be found and says
    /// nothing about what the model is shown. Existing entries are preserved, and a
    /// key holding a non-array is replaced, since no reader could use it.
    pub fn record_output_path(&mut self, path: &std::path::Path) {
        let entry = Value::String(zuno_paths::wire_path(path));
        match self.metadata.get_mut(METADATA_OUTPUT_PATHS_KEY) {
            Some(Value::Array(paths)) => paths.push(entry),
            Some(_) | None => {
                self.metadata.insert(
                    METADATA_OUTPUT_PATHS_KEY.to_owned(),
                    Value::Array(vec![entry]),
                );
            }
        }
    }

    /// The persisted-output paths recorded on this result, in the order added.
    #[must_use]
    pub fn output_paths(&self) -> Vec<&str> {
        self.metadata
            .get(METADATA_OUTPUT_PATHS_KEY)
            .and_then(Value::as_array)
            .map(|paths| paths.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// Records that this call wrote `path`, chaining.
    ///
    /// Repeats are dropped so a tool that writes and then re-reads the same file reports
    /// it once; a key holding a non-array is replaced, since no reader could use it.
    /// Deletions are deliberately not recorded — see [`METADATA_WRITTEN_PATHS_KEY`].
    #[must_use]
    pub fn with_written_path(mut self, path: &std::path::Path) -> Self {
        let entry = Value::String(zuno_paths::wire_path(path));
        match self.metadata.get_mut(METADATA_WRITTEN_PATHS_KEY) {
            Some(Value::Array(paths)) => {
                if !paths.contains(&entry) {
                    paths.push(entry);
                }
            }
            Some(_) | None => {
                self.metadata.insert(
                    METADATA_WRITTEN_PATHS_KEY.to_owned(),
                    Value::Array(vec![entry]),
                );
            }
        }
        self
    }

    /// The files this call wrote, in the order written.
    #[must_use]
    pub fn written_paths(&self) -> Vec<&str> {
        self.metadata
            .get(METADATA_WRITTEN_PATHS_KEY)
            .and_then(Value::as_array)
            .map(|paths| paths.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
}

/// The thresholds above which output is oversized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimits {
    /// Maximum lines, inclusive.
    pub max_lines: usize,
    /// Maximum UTF-8 bytes, inclusive.
    pub max_bytes: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl OutputLimits {
    /// Resolves limits from configuration, defaulting each field independently.
    ///
    /// The oracle applies its defaults with per-field `??` at read time
    /// (`tool-output-store.ts:126`) rather than at parse time, so a config that sets
    /// only `max_lines` keeps the default `max_bytes`. Both keys and the block itself
    /// are optional, which is why every level here is an `Option`.
    #[must_use]
    pub fn from_config(config: Option<&ToolOutputConfig>) -> Self {
        let widen =
            |value: std::num::NonZeroU32| usize::try_from(value.get()).unwrap_or(usize::MAX);
        Self {
            max_lines: config
                .and_then(|c| c.max_lines)
                .map_or(DEFAULT_MAX_LINES, widen),
            max_bytes: config
                .and_then(|c| c.max_bytes)
                .map_or(DEFAULT_MAX_BYTES, widen),
        }
    }
}

/// Which threshold a measurement crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitExceeded {
    /// Too many lines; the byte count is within its limit.
    Lines,
    /// Too many bytes; the line count is within its limit.
    Bytes,
    /// Both thresholds crossed.
    Both,
}

/// Whether output fits.
///
/// Deliberately not `bool`: a caller reporting *why* output was withheld needs to
/// name the threshold, and reconstructing it from the numbers at the reporting site
/// is how the reported reason drifts from the decided one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeVerdict {
    /// Within both thresholds.
    WithinLimits,
    /// Over at least one threshold.
    Oversized(LimitExceeded),
}

impl SizeVerdict {
    /// Whether this verdict is over a threshold.
    #[must_use]
    pub fn is_oversized(self) -> bool {
        match self {
            Self::WithinLimits => false,
            Self::Oversized(_) => true,
        }
    }
}

/// A size verdict together with the numbers and limits that produced it.
///
/// Carries the limits so a caller can state the threshold it applied without
/// re-reading configuration, which is the only way the number in a message and the
/// number in the decision are guaranteed to be the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeMeasurement {
    /// Lines counted, `'\n'` occurrences plus one.
    pub lines: usize,
    /// UTF-8 bytes counted.
    pub bytes: usize,
    /// The thresholds applied.
    pub limits: OutputLimits,
    /// The verdict.
    pub verdict: SizeVerdict,
}

impl SizeMeasurement {
    /// Whether the measured text is over a threshold.
    #[must_use]
    pub fn is_oversized(&self) -> bool {
        self.verdict.is_oversized()
    }
}

/// Measures text against `limits` without altering it.
///
/// Returns the counts, the limits and the verdict. It never truncates, never
/// allocates a preview, and never decides what a caller should do about the answer.
#[must_use]
pub fn measure(text: &str, limits: OutputLimits) -> SizeMeasurement {
    let lines = line_count(text);
    let bytes = text.len();
    let over_lines = lines > limits.max_lines;
    let over_bytes = bytes > limits.max_bytes;
    let verdict = match (over_lines, over_bytes) {
        (false, false) => SizeVerdict::WithinLimits,
        (true, false) => SizeVerdict::Oversized(LimitExceeded::Lines),
        (false, true) => SizeVerdict::Oversized(LimitExceeded::Bytes),
        (true, true) => SizeVerdict::Oversized(LimitExceeded::Both),
    };
    SizeMeasurement {
        lines,
        bytes,
        limits,
        verdict,
    }
}

/// `'\n'` occurrences plus one, matching `tool-output-store.ts:105-109`.
#[must_use]
pub fn line_count(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use std::path::Path;

    fn limits(max_lines: usize, max_bytes: usize) -> OutputLimits {
        OutputLimits {
            max_lines,
            max_bytes,
        }
    }

    #[test]
    fn line_count_matches_the_oracles_arithmetic() {
        assert_eq!(line_count(""), 1);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 2);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("\n\n"), 3);
    }

    #[test]
    fn bytes_are_utf8_bytes_not_characters() {
        // Four CJK characters are twelve UTF-8 bytes. Counting chars would let
        // output three times over a byte budget through.
        let text = "工具输出";
        assert_eq!(text.chars().count(), 4);

        let measurement = measure(text, limits(10, 8));

        assert_eq!(measurement.bytes, 12);
        assert_eq!(
            measurement.verdict,
            SizeVerdict::Oversized(LimitExceeded::Bytes)
        );
    }

    #[test]
    fn verdict_names_which_threshold_was_crossed() {
        assert_eq!(
            measure("a\nb\nc", limits(2, 1_000)).verdict,
            SizeVerdict::Oversized(LimitExceeded::Lines)
        );
        assert_eq!(
            measure("abcdef", limits(1_000, 3)).verdict,
            SizeVerdict::Oversized(LimitExceeded::Bytes)
        );
        assert_eq!(
            measure("a\nb\nc", limits(2, 3)).verdict,
            SizeVerdict::Oversized(LimitExceeded::Both)
        );
        assert_eq!(
            measure("a\nb", limits(2, 3)).verdict,
            SizeVerdict::WithinLimits
        );
    }

    #[test]
    fn limits_are_inclusive() {
        // Exactly at the limit fits; the oracle compares with `<=`.
        let measurement = measure("a\nb", limits(2, 3));
        assert_eq!(measurement.lines, 2);
        assert_eq!(measurement.bytes, 3);
        assert!(!measurement.is_oversized());
    }

    #[test]
    fn typed_presentation_is_live_only_and_not_part_of_durable_tool_output() {
        let output = ToolOutput::text("question", "accepted").with_presentation(
            ToolResultPresentation::Question(QuestionResultPresentation::new(
                QuestionResultStatus::Answered,
                Some(vec![vec!["SQLite".to_owned()]]),
                1,
                12,
            )),
        );

        let durable = serde_json::to_value(output).expect("serialize durable tool output");
        assert!(durable.get("presentation").is_none());
    }

    #[test]
    fn measurement_reports_the_limits_it_applied() {
        let applied = limits(7, 11);
        let measurement = measure("a", applied);

        assert_eq!(measurement.limits, applied);
    }

    #[test]
    fn config_defaults_each_field_independently() {
        assert_eq!(OutputLimits::from_config(None), OutputLimits::default());
        assert_eq!(OutputLimits::default().max_lines, 2_000);
        assert_eq!(OutputLimits::default().max_bytes, 51_200);

        let only_lines = ToolOutputConfig {
            max_lines: NonZeroU32::new(10),
            max_bytes: None,
        };
        let resolved = OutputLimits::from_config(Some(&only_lines));
        assert_eq!(resolved.max_lines, 10);
        assert_eq!(resolved.max_bytes, DEFAULT_MAX_BYTES);

        let both = ToolOutputConfig {
            max_lines: NonZeroU32::new(10),
            max_bytes: NonZeroU32::new(20),
        };
        assert_eq!(OutputLimits::from_config(Some(&both)), limits(10, 20),);
    }

    #[test]
    fn output_paths_accumulate_in_order() {
        let mut output = ToolOutput::text("shell", "hello");
        assert!(output.output_paths().is_empty());

        output.record_output_path(Path::new("/data/tool-output/tool_a"));
        output.record_output_path(Path::new("/data/tool-output/tool_b"));

        assert_eq!(
            output.output_paths(),
            vec!["/data/tool-output/tool_a", "/data/tool-output/tool_b"]
        );
    }

    #[test]
    fn output_paths_replaces_a_key_no_reader_could_use() {
        let mut output =
            ToolOutput::text("shell", "hello").with_metadata(METADATA_OUTPUT_PATHS_KEY, "oops");

        output.record_output_path(Path::new("/data/tool_a"));

        assert_eq!(output.output_paths(), vec!["/data/tool_a"]);
    }

    #[test]
    fn attachment_serializes_to_the_oracles_file_part_shape() {
        let attachment = Attachment {
            mime: "image/png".to_owned(),
            filename: Some("plot.png".to_owned()),
            url: "file:///tmp/plot.png".to_owned(),
            source: None,
        };

        let json = serde_json::to_value(&attachment).expect("serializable");

        assert_eq!(
            json,
            serde_json::json!({
                "type": "file",
                "mime": "image/png",
                "filename": "plot.png",
                "url": "file:///tmp/plot.png",
            }),
            "id, sessionID and messageID are assigned when the part is persisted"
        );
    }

    #[test]
    fn file_diffs_are_typed_validated_and_durable() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let changed = workspace.path().join("demo.rs");
        let created = workspace.path().join("new.rs");
        let changed_path = zuno_paths::wire_path(&changed);
        let created_path = zuno_paths::wire_path(&created);
        let diff = FileDiff::new(&changed, Some("old\n".to_owned()), "new\n".to_owned())
            .expect("absolute changed file is valid");
        let output = ToolOutput::text("edit", "ok").with_file_diff(diff.clone());

        assert_eq!(output.file_diffs(), vec![diff.clone()]);
        assert_eq!(
            output.metadata[METADATA_FILE_DIFFS_KEY],
            serde_json::json!([{
                "path": changed_path,
                "oldText": "old\n",
                "newText": "new\n",
            }])
        );

        let restored = ToolOutput::text("edit", "ok").with_metadata(
            METADATA_FILE_DIFFS_KEY,
            serde_json::json!([
                {"path": "relative.rs", "oldText": "old", "newText": "new"},
                {"path": created_path, "newText": "created"},
            ]),
        );
        assert_eq!(restored.file_diffs().len(), 1);
        assert_eq!(restored.file_diffs()[0].path(), created_path);
    }
    #[test]
    fn tool_output_round_trips() {
        let output = ToolOutput::text("read", "contents")
            .with_metadata("lines", 3)
            .with_attachment(Attachment::new("text/plain", "file:///a.txt"));

        let json = serde_json::to_string(&output).expect("serializable");
        let back: ToolOutput = serde_json::from_str(&json).expect("deserializable");

        assert_eq!(back, output);
    }

    #[test]
    fn measure_never_alters_the_text_it_measures() {
        let output = ToolOutput::text("shell", "a\nb\nc\nd");
        let before = output.output.clone();

        let measurement = output.measure(limits(1, 1));

        assert!(measurement.is_oversized());
        assert_eq!(output.output, before, "detection must not truncate");
    }
}
