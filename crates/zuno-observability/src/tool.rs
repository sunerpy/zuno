//! The structured tool-lifecycle event, which tool code emits instead of `info!`.
//!
//! # Why a helper rather than a convention
//!
//! A tool call has four observable moments — it was requested, it started, it
//! finished, it failed — and each one is written by different code in a different
//! crate. Left to ad-hoc `info!("running bash")` calls, the four moments arrive as
//! four unrelated sentences that nothing can join, count, or time.
//!
//! Every record this module emits carries the same three fields, so a log reader can
//! group by [`FIELD_CALL_ID`] and get the whole life of one call:
//!
//! ```text
//! event=TOOL_LIFECYCLE phase=pending   tool=bash call_id=toolu_01A
//! event=TOOL_LIFECYCLE phase=running   tool=bash call_id=toolu_01A
//! event=TOOL_LIFECYCLE phase=completed tool=bash call_id=toolu_01A elapsed_ms=412
//! ```
//!
//! # The abandoned phase
//!
//! [`ToolLifecycle`] emits `phase=abandoned` from its `Drop` if it was never
//! completed or failed. A tool call that stops being tracked without an outcome is
//! either a `?` on a path nobody considered or a task that was cancelled, and both
//! are bugs worth a warning. Without that, the failure mode is a call that simply
//! stops appearing in the log — an absence, which is the hardest thing to notice.
//!
//! # Failures carry data, not prose
//!
//! [`ToolLifecycle::failed`] takes a typed [`zuno_error::ToolError`] and records the
//! variant name plus the two recovery booleans as fields. A log consumer deciding
//! whether a failure was the model's fault reads `model_correctable=true`; it never
//! has to match on the rendered message, which is the defect `zuno-error` exists to
//! prevent.

use std::time::Instant;
use zuno_error::ToolError;

/// The value of [`FIELD_EVENT`] on every record this module emits, so one filter
/// selects the whole tool stream and nothing else.
pub const EVENT_TOOL_LIFECYCLE: &str = "TOOL_LIFECYCLE";

/// The field naming the structured event kind.
pub const FIELD_EVENT: &str = "event";

/// The field naming which of the five phases a record reports.
pub const FIELD_PHASE: &str = "phase";

/// Wall-clock milliseconds from [`ToolLifecycle::pending`] to the terminal phase.
pub const FIELD_ELAPSED_MS: &str = "elapsed_ms";

/// The [`zuno_error::ToolError`] variant name, as a stable discriminant.
pub const FIELD_ERROR_KIND: &str = "error_kind";

/// Whether running the identical call again may succeed.
pub const FIELD_RETRYABLE: &str = "retryable";

/// Whether the model can fix the failure by issuing a corrected call.
pub const FIELD_MODEL_CORRECTABLE: &str = "model_correctable";

/// Where a tool call is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    /// The model asked for the call; permission and validation have not run yet.
    Pending,
    /// The call passed its checks and the tool is executing.
    Running,
    /// The tool returned a result.
    Completed,
    /// The tool failed.
    Error,
    /// Tracking ended with no outcome. Emitted only from `Drop`.
    Abandoned,
}

impl ToolPhase {
    /// The lowercase spelling written to [`FIELD_PHASE`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Abandoned => "abandoned",
        }
    }

    /// True once no further phase can follow.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Error | Self::Abandoned)
    }
}

/// The stable discriminant for a [`ToolError`] variant.
///
/// A separate function rather than a `Display` impl because the point is that this
/// string is a *key*, not a message: it is safe to filter and count on, and it
/// changes only when a variant is added or renamed.
#[must_use]
pub fn error_kind(error: &ToolError) -> &'static str {
    match error {
        ToolError::Denied { .. } => "denied",
        ToolError::InvalidArgs { .. } => "invalid_args",
        ToolError::Timeout { .. } => "timeout",
        ToolError::Transient { .. } => "transient",
        ToolError::NotFound { .. } => "not_found",
        ToolError::Failed { .. } => "failed",
    }
}

/// Tracks one tool call and emits a record at each phase.
///
/// ```
/// use zuno_observability::tool::ToolLifecycle;
///
/// let mut call = ToolLifecycle::pending("bash", "toolu_01A");
/// call.running();
/// call.completed();
/// ```
///
/// Dropping one without calling [`Self::completed`] or [`Self::failed`] emits
/// `phase=abandoned` at warn level.
#[derive(Debug)]
pub struct ToolLifecycle {
    tool: String,
    call_id: String,
    started: Instant,
    phase: ToolPhase,
}

impl ToolLifecycle {
    /// Records that the model asked for this call, before permission or validation.
    #[must_use]
    pub fn pending(tool: impl Into<String>, call_id: impl Into<String>) -> Self {
        let this = Self {
            tool: tool.into(),
            call_id: call_id.into(),
            started: Instant::now(),
            phase: ToolPhase::Pending,
        };
        tracing::debug!(
            event = EVENT_TOOL_LIFECYCLE,
            phase = ToolPhase::Pending.as_str(),
            tool = %this.tool,
            call_id = %this.call_id,
        );
        this
    }

    /// Records that the checks passed and the tool is executing.
    pub fn running(&mut self) {
        self.phase = ToolPhase::Running;
        tracing::info!(
            event = EVENT_TOOL_LIFECYCLE,
            phase = ToolPhase::Running.as_str(),
            tool = %self.tool,
            call_id = %self.call_id,
        );
    }

    /// Records a successful outcome and the elapsed time.
    pub fn completed(mut self) {
        self.phase = ToolPhase::Completed;
        tracing::info!(
            event = EVENT_TOOL_LIFECYCLE,
            phase = ToolPhase::Completed.as_str(),
            tool = %self.tool,
            call_id = %self.call_id,
            elapsed_ms = self.elapsed_ms(),
        );
    }

    /// Records a failure, its variant discriminant, and the two recovery booleans a
    /// consumer would otherwise have to re-derive.
    pub fn failed(mut self, error: &ToolError) {
        self.phase = ToolPhase::Error;
        tracing::error!(
            event = EVENT_TOOL_LIFECYCLE,
            phase = ToolPhase::Error.as_str(),
            tool = %self.tool,
            call_id = %self.call_id,
            elapsed_ms = self.elapsed_ms(),
            error_kind = error_kind(error),
            retryable = error.is_retryable(),
            model_correctable = error.is_model_correctable(),
            error = %error,
        );
    }

    /// The tool name.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The provider-assigned call identifier.
    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The phase most recently recorded.
    #[must_use]
    pub fn phase(&self) -> ToolPhase {
        self.phase
    }

    /// Milliseconds since [`Self::pending`], saturating rather than wrapping.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

impl Drop for ToolLifecycle {
    fn drop(&mut self) {
        if self.phase.is_terminal() {
            return;
        }
        tracing::warn!(
            event = EVENT_TOOL_LIFECYCLE,
            phase = ToolPhase::Abandoned.as_str(),
            tool = %self.tool,
            call_id = %self.call_id,
            elapsed_ms = self.elapsed_ms(),
            last_phase = self.phase.as_str(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{FIELD_CALL_ID, FIELD_TOOL};

    /// These strings end up in log files that outlive the process and in whatever
    /// queries later read them, so a rename is a breaking change.
    #[test]
    fn the_phase_spellings_are_pinned() {
        assert_eq!(ToolPhase::Pending.as_str(), "pending");
        assert_eq!(ToolPhase::Running.as_str(), "running");
        assert_eq!(ToolPhase::Completed.as_str(), "completed");
        assert_eq!(ToolPhase::Error.as_str(), "error");
        assert_eq!(ToolPhase::Abandoned.as_str(), "abandoned");
    }

    #[test]
    fn the_event_and_field_names_are_pinned() {
        assert_eq!(EVENT_TOOL_LIFECYCLE, "TOOL_LIFECYCLE");
        assert_eq!(FIELD_EVENT, "event");
        assert_eq!(FIELD_PHASE, "phase");
        assert_eq!(FIELD_ELAPSED_MS, "elapsed_ms");
        assert_eq!(FIELD_ERROR_KIND, "error_kind");
        assert_eq!(FIELD_RETRYABLE, "retryable");
        assert_eq!(FIELD_MODEL_CORRECTABLE, "model_correctable");
        assert_eq!(FIELD_TOOL, "tool");
        assert_eq!(FIELD_CALL_ID, "call_id");
    }

    #[test]
    fn only_the_three_outcomes_are_terminal() {
        assert!(!ToolPhase::Pending.is_terminal());
        assert!(!ToolPhase::Running.is_terminal());
        assert!(ToolPhase::Completed.is_terminal());
        assert!(ToolPhase::Error.is_terminal());
        assert!(ToolPhase::Abandoned.is_terminal());
    }

    /// Every `ToolError` variant needs a discriminant, and the discriminants have to
    /// be distinct or a consumer cannot tell two failure classes apart. The match in
    /// `error_kind` is exhaustive, so a new variant fails to compile rather than
    /// falling into a wildcard.
    #[test]
    fn every_tool_error_variant_has_a_distinct_discriminant() {
        let errors = [
            ToolError::Denied {
                tool: "bash".to_owned(),
            },
            ToolError::InvalidArgs {
                tool: "bash".to_owned(),
                source: Box::new(std::io::Error::other("bad")),
            },
            ToolError::Timeout {
                tool: "bash".to_owned(),
                elapsed: std::time::Duration::from_secs(1),
            },
            ToolError::Transient {
                tool: "web_search".to_owned(),
                retry_after: Some(std::time::Duration::from_secs(1)),
                source: Box::new(std::io::Error::other("HTTP 503")),
            },
            ToolError::NotFound {
                tool: "nope".to_owned(),
            },
            ToolError::Failed {
                tool: "bash".to_owned(),
                source: Box::new(std::io::Error::other("exit 1")),
            },
        ];
        let kinds: Vec<&str> = errors.iter().map(error_kind).collect();
        assert_eq!(
            kinds,
            [
                "denied",
                "invalid_args",
                "timeout",
                "transient",
                "not_found",
                "failed"
            ]
        );

        let mut unique = kinds.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), kinds.len(), "discriminants must be distinct");
    }

    #[test]
    fn the_phase_advances_through_the_lifecycle() {
        let mut call = ToolLifecycle::pending("bash", "toolu_01A");
        assert_eq!(call.phase(), ToolPhase::Pending);
        assert_eq!(call.tool(), "bash");
        assert_eq!(call.call_id(), "toolu_01A");

        call.running();
        assert_eq!(call.phase(), ToolPhase::Running);

        call.completed();
    }

    /// The whole lifecycle must be usable with no subscriber installed, because tool
    /// code cannot know whether the process initialized logging.
    #[test]
    fn a_failure_is_recordable_without_a_subscriber() {
        let call = ToolLifecycle::pending("bash", "toolu_01B");
        call.failed(&ToolError::NotFound {
            tool: "bash".to_owned(),
        });
    }

    #[test]
    fn an_abandoned_call_does_not_panic_on_drop() {
        let call = ToolLifecycle::pending("bash", "toolu_01C");
        drop(call);
    }
}
