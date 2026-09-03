//! Withhold oversized tool output, and hand back the way to read it.
//!
//! The TypeScript oracle defines the configurable byte and line thresholds in
//! `packages/core/src/v1/config/config.ts:136-148` and persists overflow in
//! `packages/core/src/tool-output-store.ts:112-174`. This layer deliberately changes
//! only what the model sees: the complete payload is stored first, then withheld.
//! Returning a prefix would still spend context on an answer that is usually absent;
//! this follows jcode's rationale at
//! `jcode`.
//!
//! # Withheld is an outcome, not a failure
//!
//! Withholding used to be returned as an error, and the annotated result — the one
//! carrying the artifact path this layer had just recorded — was dropped on the way
//! out. The engine kept only the rendered string, so the artifact existed nowhere but
//! in the prose of an error message, and with it went the exit code, the verification
//! receipt, and the sandbox facts of a command that had in fact succeeded. The refusal
//! also had exactly one exit: re-run with `accept_large_output`. `shell` is
//! [`zuno_tool::ToolReplayPolicy::Never`], so that instructed the model to repeat a
//! side effect that must never be repeated, and to spend on the repeat the context the
//! limit exists to protect. Recovering by hand — `shell` with `tail` — is what
//! truncated an authoritative test summary and forced a needless re-run of a suite.
//!
//! So a withheld result is now a successful result whose text is a notice: it names the
//! artifact, offers the windowed read first, and describes `accept_large_output` as the
//! deliberate back door it is. Both branches keep the artifact reference on the durable
//! part, so a client, a later turn, or a resumed session can find the output without
//! parsing prose.

use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use zuno_error::ToolError;
use zuno_tool::{OutputLimits, SizeMeasurement, ToolOutput, ToolOutputStore};

pub const ACCEPT_LARGE_OUTPUT: &str = zuno_tool::ACCEPT_LARGE_OUTPUT_KEY;

/// The metadata key carrying the typed facts of a withheld result.
///
/// Beside the human-readable notice rather than instead of it: a client surface, and
/// any later turn, must be able to offer retrieval without re-parsing a sentence.
pub const METADATA_WITHHELD_OUTPUT_KEY: &str = "withheldOutput";

/// The tool that reads a withheld artifact back, and the action that does it.
///
/// Named here because the notice has to tell the model something it can actually call.
/// `bg artifact` is that reader: it is registered, permitted wherever a command can be
/// started, session-scoped to the artifacts this session produced, and windowed.
const RETRIEVAL_TOOL: &str = crate::bg::WIRE_ID;
const RETRIEVAL_ACTION: &str = "artifact";

/// A transparent, documented estimate rather than a fabricated tokenizer count.
///
/// Tool output can contain arbitrary text and no model tokenizer is available at
/// this layer. Four UTF-8 bytes per token is intentionally described as an estimate
/// in every notice, so the caller knows both the price and how it was derived.
pub const ESTIMATED_UTF8_BYTES_PER_TOKEN: usize = 4;

#[derive(Debug, Clone)]
pub struct OutputPolicy {
    store: ToolOutputStore,
    limits: OutputLimits,
}

impl OutputPolicy {
    #[must_use]
    pub fn new(store: ToolOutputStore, limits: OutputLimits) -> Self {
        Self { store, limits }
    }

    /// Applies the limits to output that was produced as text.
    ///
    /// # Errors
    ///
    /// Whatever [`ToolOutputStore::persist`] returns when the artifact cannot be
    /// written. Being over the limit is not an error: see [`Self::apply_bytes`].
    pub fn apply(
        &self,
        tool: &str,
        session_id: &str,
        output: ToolOutput,
        accept_large_output: bool,
    ) -> Result<ToolOutput, ToolError> {
        self.decide(tool, session_id, output, None, accept_large_output)
    }

    /// Applies the limits, persisting `raw` rather than the text in `output`.
    ///
    /// For a tool whose output was decoded lossily for display. The artifact is the copy
    /// that outlives the call, so it has to hold what the command actually wrote: a
    /// store fed the decoded string kept `U+FFFD` where the bytes were, and the
    /// retrieval path could then only ever return the damage.
    ///
    /// # Errors
    ///
    /// Whatever [`ToolOutputStore::persist_bytes`] returns when the artifact cannot be
    /// written. Output over the limit is withheld, not refused: the result comes back
    /// `Ok` with a notice as its text and the artifact recorded on it.
    pub fn apply_bytes(
        &self,
        tool: &str,
        session_id: &str,
        output: ToolOutput,
        raw: &[u8],
        accept_large_output: bool,
    ) -> Result<ToolOutput, ToolError> {
        self.decide(tool, session_id, output, Some(raw), accept_large_output)
    }

    fn decide(
        &self,
        tool: &str,
        session_id: &str,
        mut output: ToolOutput,
        raw: Option<&[u8]>,
        accept_large_output: bool,
    ) -> Result<ToolOutput, ToolError> {
        let measurement = output.measure(self.limits);
        if !measurement.is_oversized() {
            return Ok(output);
        }

        let stored = match raw {
            Some(raw) => self.store.persist_bytes(tool, session_id, raw)?,
            None => self.store.persist(tool, session_id, &output.output)?,
        };
        output.record_output_path(&stored.path);
        output
            .metadata
            .insert("oversized".to_owned(), Value::Bool(true));
        output
            .metadata
            .insert("outputBytes".to_owned(), json!(measurement.bytes));
        output
            .metadata
            .insert("outputLines".to_owned(), json!(measurement.lines));

        if accept_large_output {
            output
                .metadata
                .insert("largeOutputAccepted".to_owned(), Value::Bool(true));
            return Ok(output);
        }

        let withheld = WithheldOutput::new(measurement, &stored.path, stored.bytes, stored.lines);
        output.output = withheld.notice();
        output.metadata.insert(
            METADATA_WITHHELD_OUTPUT_KEY.to_owned(),
            serde_json::to_value(&withheld).unwrap_or(Value::Null),
        );
        Ok(output)
    }

    #[must_use]
    pub fn limits(&self) -> OutputLimits {
        self.limits
    }

    #[must_use]
    pub fn store(&self) -> &ToolOutputStore {
        &self.store
    }
}

/// Everything a reader needs about output that was withheld.
///
/// Serialized onto the durable part under [`METADATA_WITHHELD_OUTPUT_KEY`], and
/// rendered into the model-facing notice by [`WithheldOutput::notice`]. One source for
/// both, so the sentence and the structure cannot disagree about which artifact holds
/// what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WithheldOutput {
    /// Where the complete output was saved, in the spelling a caller passes back.
    pub output_path: String,
    /// The artifact's own size, which is what a windowed read pages through.
    pub artifact_bytes: usize,
    /// Lines in the artifact, counted as [`zuno_tool::output::line_count`] does.
    pub artifact_lines: usize,
    /// The measured size of the text that was withheld, and the limits it crossed.
    pub measured_bytes: usize,
    pub measured_lines: usize,
    pub limit_bytes: usize,
    pub limit_lines: usize,
    /// Estimated context cost of returning it inline, and the divisor used.
    pub estimated_tokens: usize,
    pub estimated_bytes_per_token: usize,
    /// The tool and action that read this artifact back.
    pub retrieval_tool: String,
    pub retrieval_action: String,
}

impl WithheldOutput {
    #[must_use]
    pub fn new(
        measurement: SizeMeasurement,
        output_path: &Path,
        artifact_bytes: usize,
        artifact_lines: usize,
    ) -> Self {
        Self {
            output_path: zuno_paths::wire_path(output_path),
            artifact_bytes,
            artifact_lines,
            measured_bytes: measurement.bytes,
            measured_lines: measurement.lines,
            limit_bytes: measurement.limits.max_bytes,
            limit_lines: measurement.limits.max_lines,
            estimated_tokens: estimate_tokens(measurement.bytes),
            estimated_bytes_per_token: ESTIMATED_UTF8_BYTES_PER_TOKEN,
            retrieval_tool: RETRIEVAL_TOOL.to_owned(),
            retrieval_action: RETRIEVAL_ACTION.to_owned(),
        }
    }

    /// The model-facing text of a withheld result.
    ///
    /// Retrieval comes before the inline back door on purpose. The retrieval call reads
    /// an artifact that already exists; the back door re-runs the call that produced it,
    /// which for a side-effecting tool is exactly what must not be repeated.
    #[must_use]
    pub fn notice(&self) -> String {
        format!(
            "Tool output withheld: {} bytes across {} lines (~{} tokens, estimated at 1 token per \
             {} UTF-8 bytes) exceeds the configured limit of {} bytes or {} lines. Every byte was \
             saved to {} ({} bytes, {} lines) and is readable in windows: call `{}` with \
             `action: \"{}\"`, `outputPath: \"{}\"`, and the `cursor` each window returns. Passing \
             `{ACCEPT_LARGE_OUTPUT}: true` returns output like this inline instead, which spends \
             the context this limit protects and re-runs the call — use it only for work you are \
             willing to run again.",
            self.measured_bytes,
            self.measured_lines,
            self.estimated_tokens,
            self.estimated_bytes_per_token,
            self.limit_bytes,
            self.limit_lines,
            self.output_path,
            self.artifact_bytes,
            self.artifact_lines,
            self.retrieval_tool,
            self.retrieval_action,
            self.output_path,
        )
    }
}

#[must_use]
pub fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(ESTIMATED_UTF8_BYTES_PER_TOKEN)
}
