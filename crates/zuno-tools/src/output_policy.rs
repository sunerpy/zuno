//! Refuse oversized tool output until the caller explicitly accepts its context cost.
//!
//! The TypeScript oracle defines the configurable byte and line thresholds in
//! `packages/core/src/v1/config/config.ts:136-148` and persists overflow in
//! `packages/core/src/tool-output-store.ts:112-174`. This layer deliberately changes
//! only what the model sees: the complete payload is stored first, then withheld.
//! Returning a prefix would still spend context on an answer that is usually absent;
//! this follows jcode's rationale at
//! `jcode`.

use serde_json::{Value, json};
use std::fmt;
use std::path::PathBuf;
use zuno_error::ToolError;
use zuno_tool::{OutputLimits, SizeMeasurement, ToolOutput, ToolOutputStore};

pub const ACCEPT_LARGE_OUTPUT: &str = zuno_tool::ACCEPT_LARGE_OUTPUT_KEY;

/// A transparent, documented estimate rather than a fabricated tokenizer count.
///
/// Tool output can contain arbitrary text and no model tokenizer is available at
/// this layer. Four UTF-8 bytes per token is intentionally described as an estimate
/// in every refusal, so the caller knows both the price and how it was derived.
pub const ESTIMATED_UTF8_BYTES_PER_TOKEN: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum OutputPolicyError {
    #[error(transparent)]
    Persistence(#[from] ToolError),
    #[error(transparent)]
    Oversized(#[from] OversizedOutputRefusal),
}

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

    pub fn apply(
        &self,
        tool: &str,
        session_id: &str,
        mut output: ToolOutput,
        accept_large_output: bool,
    ) -> Result<ToolOutput, OutputPolicyError> {
        let measurement = output.measure(self.limits);
        if !measurement.is_oversized() {
            return Ok(output);
        }

        let stored = self.store.persist(tool, session_id, &output.output)?;
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

        Err(OversizedOutputRefusal::new(measurement, stored.path).into())
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OversizedOutputRefusal {
    pub measurement: SizeMeasurement,
    pub estimated_tokens: usize,
    pub output_path: PathBuf,
}

impl OversizedOutputRefusal {
    #[must_use]
    pub fn new(measurement: SizeMeasurement, output_path: PathBuf) -> Self {
        Self {
            measurement,
            estimated_tokens: estimate_tokens(measurement.bytes),
            output_path,
        }
    }
}

impl fmt::Display for OversizedOutputRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Tool output withheld: {} bytes across {} lines (~{} tokens, estimated at 1 token per {} UTF-8 bytes) exceeds the configured limit of {} bytes or {} lines. Full output saved to {}. Re-run with `{ACCEPT_LARGE_OUTPUT}: true` to return it in full.",
            self.measurement.bytes,
            self.measurement.lines,
            self.estimated_tokens,
            ESTIMATED_UTF8_BYTES_PER_TOKEN,
            self.measurement.limits.max_bytes,
            self.measurement.limits.max_lines,
            self.output_path.display(),
        )
    }
}

impl std::error::Error for OversizedOutputRefusal {}

#[must_use]
pub fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(ESTIMATED_UTF8_BYTES_PER_TOKEN)
}
