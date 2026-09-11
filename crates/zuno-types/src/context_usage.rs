//! Canonical context occupancy and replaceable provider-usage snapshots.
//!
//! Context occupancy is the last provider-confirmed request plus content not yet
//! included in that request. Session consumption is a separate, cumulative meter.
//! A request's local estimate never replaces an existing confirmed baseline.
//! The tracker is pure and serializable, including its retry checkpoint.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Independent request owners. A tracker accepts exactly one source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextUsageSource {
    #[default]
    Main,
    Child,
    Learning,
    Compaction,
    Auxiliary,
}

impl ContextUsageSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Child => "child",
            Self::Learning => "learning",
            Self::Compaction => "compaction",
            Self::Auxiliary => "auxiliary",
        }
    }
}

/// Whether `used_tokens` describes a measurement, a local estimate, or no value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextUsageFreshness {
    #[default]
    Unknown,
    Estimated,
    Confirmed,
}

/// Provider accounting carried without a dependency on a provider crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ContextTokenAccounting {
    #[default]
    Unknown,
    CacheInsideInput,
    CacheBesideInput,
}

/// Durable identity of one provider attempt.
///
/// `request_sequence` is monotonic within a session/source, preferably the durable
/// request event's sequence. It must not be a step number that resets each turn.
/// Attempts of the same logical request keep the same ID and sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextRequestIdentity {
    pub request_id: String,
    pub request_sequence: u64,
    pub attempt: u32,
    pub context_epoch: u64,
    pub provider_id: String,
    pub model_id: String,
    pub source: ContextUsageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub time_started: i64,
    /// Estimate of normalized system/developer context and tool schemas for this
    /// exact request. It supports accounting for added fixed context between
    /// confirmations without replacing the provider baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_prefix: Option<ContextHistoryPrefix>,
}

/// A bounded fingerprint of the non-system history actually sent to a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextHistoryPrefix {
    pub message_count: u64,
    pub sha256: String,
}

impl ContextRequestIdentity {
    fn is_valid(&self) -> bool {
        !self.request_id.is_empty()
            && !self.provider_id.is_empty()
            && !self.model_id.is_empty()
            && self.attempt > 0
            && self.history_prefix.as_ref().is_none_or(|prefix| {
                prefix.sha256.len() == 64
                    && prefix.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    }

    fn same_logical_request(&self, other: &Self) -> bool {
        self.request_id == other.request_id
            && self.request_sequence == other.request_sequence
            && self.context_epoch == other.context_epoch
            && self.provider_id == other.provider_id
            && self.model_id == other.model_id
            && self.source == other.source
            && self.turn_id == other.turn_id
            && self.time_started == other.time_started
            && self.request_context_tokens == other.request_context_tokens
            && self.history_prefix == other.history_prefix
    }
}

/// A provider frame's optional fields. These are snapshots, never token deltas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageCounters {
    pub input_tokens: Option<u64>,
    /// Inclusive of reasoning, just like Zuno's normalized provider event.
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub accounting: ContextTokenAccounting,
}

impl ContextUsageCounters {
    /// Replace supplied fields and retain omitted fields from this attempt.
    ///
    /// An accounting-mode change within an attempt is invalid and ignored as a
    /// whole; it cannot reinterpret counters already received from the provider.
    pub fn merge_snapshot(&mut self, frame: Self) -> bool {
        if self.accounting != ContextTokenAccounting::Unknown
            && frame.accounting != ContextTokenAccounting::Unknown
            && self.accounting != frame.accounting
        {
            return false;
        }
        let before = *self;
        if frame.input_tokens.is_some() {
            self.input_tokens = frame.input_tokens;
        }
        if frame.output_tokens.is_some() {
            self.output_tokens = frame.output_tokens;
        }
        if frame.reasoning_tokens.is_some() {
            self.reasoning_tokens = frame.reasoning_tokens;
        }
        if frame.cache_read_input_tokens.is_some() {
            self.cache_read_input_tokens = frame.cache_read_input_tokens;
        }
        if frame.cache_write_input_tokens.is_some() {
            self.cache_write_input_tokens = frame.cache_write_input_tokens;
        }
        if frame.accounting != ContextTokenAccounting::Unknown {
            self.accounting = frame.accounting;
        }
        *self != before
    }

    /// Known prompt occupancy. Missing input is not a measured zero.
    #[must_use]
    pub fn prompt_tokens(self) -> Option<u64> {
        let input = self.input_tokens?;
        match self.accounting {
            ContextTokenAccounting::Unknown => None,
            ContextTokenAccounting::CacheInsideInput => Some(input),
            ContextTokenAccounting::CacheBesideInput => Some(
                input
                    .saturating_add(self.cache_read_input_tokens.unwrap_or_default())
                    .saturating_add(self.cache_write_input_tokens.unwrap_or_default()),
            ),
        }
    }

    /// Measured prompt plus the output received so far.
    ///
    /// An input-only frame is a lower bound, marked `estimated` until output is
    /// supplied. Reasoning is already included in output and is never added here.
    #[must_use]
    pub fn context_tokens(self) -> Option<u64> {
        self.prompt_tokens().map(|prompt| {
            prompt.saturating_add(
                self.output_tokens
                    .unwrap_or(self.reasoning_tokens.unwrap_or_default()),
            )
        })
    }

    #[must_use]
    pub fn is_complete(self) -> bool {
        self.prompt_tokens().is_some() && self.output_tokens.is_some()
    }

    /// Convert one merged snapshot to disjoint cumulative-consumption buckets.
    #[must_use]
    pub fn disjoint(self) -> ContextUsageTotals {
        let output = self
            .output_tokens
            .unwrap_or(self.reasoning_tokens.unwrap_or_default());
        let reasoning = self.reasoning_tokens.unwrap_or_default().min(output);
        let mut cache_read = self.cache_read_input_tokens.unwrap_or_default();
        let mut cache_write = self.cache_write_input_tokens.unwrap_or_default();
        let input = self.input_tokens.unwrap_or_else(|| {
            if self.accounting == ContextTokenAccounting::CacheInsideInput {
                cache_read.saturating_add(cache_write)
            } else {
                0
            }
        });
        let (uncached, unclassified) = match self.accounting {
            ContextTokenAccounting::Unknown => {
                let observed = input.max(cache_read.saturating_add(cache_write));
                cache_read = 0;
                cache_write = 0;
                (0, observed)
            }
            ContextTokenAccounting::CacheInsideInput => {
                // A provider's malformed breakdown must not exceed its total.
                cache_read = cache_read.min(input);
                cache_write = cache_write.min(input.saturating_sub(cache_read));
                (
                    input.saturating_sub(cache_read).saturating_sub(cache_write),
                    0,
                )
            }
            ContextTokenAccounting::CacheBesideInput => (input, 0),
        };
        ContextUsageTotals {
            input: uncached,
            output: output.saturating_sub(reasoning),
            reasoning,
            cache_read,
            cache_write,
            unclassified,
        }
    }
}

/// Disjoint observed consumption across attempts; never context occupancy.
///
/// When `cumulative_known` is false these counters are an observed lower bound,
/// not a claim that the provider billed no additional unreported tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageTotals {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub unclassified: u64,
}

impl ContextUsageTotals {
    #[must_use]
    pub const fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.unclassified)
    }

    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            reasoning: self.reasoning.saturating_add(other.reasoning),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
            unclassified: self.unclassified.saturating_add(other.unclassified),
        }
    }
}

/// Provenance and raw normalized counters of the latest usable measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageConfirmation {
    pub request: ContextRequestIdentity,
    pub usage: ContextUsageCounters,
    pub time_confirmed: i64,
}

/// Shared wire snapshot for TUI, HTTP, ACP and durable replay.
///
/// `None` means unknown, including a tail that was not measured. A zero tail is
/// explicitly `Some(0)`. A lower local full-request estimate is retained for
/// diagnostics but cannot erase `last_confirmed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageSnapshot {
    pub session_id: String,
    pub source: ContextUsageSource,
    pub revision: u64,
    pub context_epoch: u64,
    pub request: Option<ContextRequestIdentity>,
    pub last_confirmed: Option<ContextUsageConfirmation>,
    pub request_estimate_tokens: Option<u64>,
    pub estimated_tail_tokens: Option<u64>,
    pub used_tokens: Option<u64>,
    pub context_limit: Option<u64>,
    pub freshness: ContextUsageFreshness,
    pub cumulative_usage: ContextUsageTotals,
    pub cumulative_known: bool,
    pub time_updated: i64,
}

impl ContextUsageSnapshot {
    #[must_use]
    pub fn unknown(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            source: ContextUsageSource::Main,
            revision: 0,
            context_epoch: 0,
            request: None,
            last_confirmed: None,
            request_estimate_tokens: None,
            estimated_tail_tokens: None,
            used_tokens: None,
            context_limit: None,
            freshness: ContextUsageFreshness::Unknown,
            cumulative_usage: ContextUsageTotals::default(),
            cumulative_known: true,
            time_updated: 0,
        }
    }

    fn refresh(&mut self) {
        self.used_tokens = match self.last_confirmed.as_ref() {
            Some(confirmed) => confirmed
                .usage
                .context_tokens()
                .zip(self.estimated_tail_tokens)
                .map(|(baseline, tail)| baseline.saturating_add(tail)),
            None => self.request_estimate_tokens,
        };
        self.freshness = if self.used_tokens.is_none() {
            ContextUsageFreshness::Unknown
        } else if self.estimated_tail_tokens == Some(0)
            && self.last_confirmed.as_ref().is_some_and(|confirmed| {
                Some(&confirmed.request) == self.request.as_ref() && confirmed.usage.is_complete()
            })
        {
            ContextUsageFreshness::Confirmed
        } else {
            ContextUsageFreshness::Estimated
        };
    }

    /// Validate a snapshot read from durable storage or another process.
    pub fn validate(&self) -> Result<(), InvalidContextUsage> {
        if self.session_id.is_empty() || self.context_limit == Some(0) {
            return Err(InvalidContextUsage(
                "invalid session identity or context limit",
            ));
        }
        if let Some(request) = &self.request {
            validate_identity(request, self.context_epoch, self.source)?;
        }
        if let Some(confirmed) = &self.last_confirmed {
            validate_identity(&confirmed.request, self.context_epoch, self.source)?;
            if confirmed.usage.prompt_tokens().is_none() {
                return Err(InvalidContextUsage(
                    "confirmation has no known prompt usage",
                ));
            }
            if self.request.as_ref().is_some_and(|request| {
                request.provider_id != confirmed.request.provider_id
                    || request.model_id != confirmed.request.model_id
                    || request.request_sequence < confirmed.request.request_sequence
            }) {
                return Err(InvalidContextUsage(
                    "confirmation does not belong to this context",
                ));
            }
        }
        let mut derived = self.clone();
        derived.refresh();
        if self.used_tokens != derived.used_tokens || self.freshness != derived.freshness {
            return Err(InvalidContextUsage(
                "context occupancy or freshness is inconsistent",
            ));
        }
        Ok(())
    }
}

/// An invalid persisted/wire state, never a reason to silently reset usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidContextUsage(pub &'static str);

impl std::fmt::Display for InvalidContextUsage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for InvalidContextUsage {}

fn validate_identity(
    request: &ContextRequestIdentity,
    epoch: u64,
    source: ContextUsageSource,
) -> Result<(), InvalidContextUsage> {
    if !request.is_valid() || request.context_epoch != epoch || request.source != source {
        return Err(InvalidContextUsage(
            "request identity, epoch, or source mismatch",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct AttemptCheckpoint {
    before_confirmation: Option<ContextUsageConfirmation>,
    before_cumulative: ContextUsageTotals,
    before_cumulative_known: bool,
    request_tail: Option<u64>,
    usage: ContextUsageCounters,
    committed: bool,
}

/// Pure, resumable state machine for one session/source's context.
///
/// Persist the tracker, not only `snapshot()`: the checkpoint is required to
/// remove a failed attempt's provisional counters after a process restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageTracker {
    snapshot: ContextUsageSnapshot,
    active: Option<AttemptCheckpoint>,
    /// The existing durable history checkpoint is an input to this state machine,
    /// not a counter that clients are allowed to rewrite.
    #[serde(default)]
    observed_history_epoch: Option<i64>,
}

impl ContextUsageTracker {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            snapshot: ContextUsageSnapshot::unknown(session_id),
            active: None,
            observed_history_epoch: None,
        }
    }

    #[must_use]
    pub fn for_source(session_id: impl Into<String>, source: ContextUsageSource) -> Self {
        let mut tracker = Self::new(session_id);
        tracker.snapshot.source = source;
        tracker
    }

    /// Seed a projection from an already validated snapshot.
    ///
    /// This cannot recover the active attempt's rollback checkpoint. Runtime
    /// resume must deserialize the complete tracker saved by the context store.
    pub fn from_snapshot(snapshot: ContextUsageSnapshot) -> Result<Self, InvalidContextUsage> {
        snapshot.validate()?;
        Ok(Self {
            snapshot,
            active: None,
            observed_history_epoch: None,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> &ContextUsageSnapshot {
        &self.snapshot
    }

    /// Begin a main/child/auxiliary request owned by this tracker.
    ///
    /// Local tail tokens must describe only newly included normalized content.
    /// Missing tail measurements remain unknown; the full prompt estimate is
    /// used only when there is no applicable provider-confirmed baseline.
    /// Duplicate, stale and differently owned requests make no change.
    pub fn start_request(
        &mut self,
        request: ContextRequestIdentity,
        request_estimate_tokens: Option<u64>,
        estimated_tail_tokens: Option<u64>,
        context_limit: Option<u64>,
        at_ms: i64,
    ) -> bool {
        if !request.is_valid()
            || request.source != self.snapshot.source
            || request.context_epoch < self.snapshot.context_epoch
        {
            return false;
        }
        if let Some(current) = &self.snapshot.request {
            if request.request_sequence < current.request_sequence {
                return false;
            }
            if request.request_sequence == current.request_sequence {
                if !request.same_logical_request(current) || request.attempt <= current.attempt {
                    return false;
                }
                let current = current.clone();
                return self.rollback_request(&current, request.attempt, at_ms);
            }
        }
        self.discard_uncommitted();
        if request.context_epoch != self.snapshot.context_epoch
            || self
                .snapshot
                .last_confirmed
                .as_ref()
                .is_some_and(|confirmed| {
                    confirmed.request.provider_id != request.provider_id
                        || confirmed.request.model_id != request.model_id
                })
        {
            self.snapshot.last_confirmed = None;
        }
        self.snapshot.context_epoch = request.context_epoch;
        self.active = Some(AttemptCheckpoint {
            before_confirmation: self.snapshot.last_confirmed.clone(),
            before_cumulative: self.snapshot.cumulative_usage,
            before_cumulative_known: self.snapshot.cumulative_known,
            request_tail: estimated_tail_tokens,
            usage: ContextUsageCounters::default(),
            committed: false,
        });
        self.snapshot.request = Some(request);
        self.snapshot.request_estimate_tokens = request_estimate_tokens;
        self.snapshot.estimated_tail_tokens = estimated_tail_tokens;
        self.snapshot.context_limit = context_limit.filter(|limit| *limit > 0);
        self.changed(at_ms);
        true
    }

    /// Replace one attempt's partial provider snapshot field by field.
    pub fn observe_usage(
        &mut self,
        request: &ContextRequestIdentity,
        frame: ContextUsageCounters,
        at_ms: i64,
    ) -> bool {
        if self.snapshot.request.as_ref() != Some(request) {
            return false;
        }
        let Some(active) = self.active.as_mut().filter(|active| !active.committed) else {
            return false;
        };
        if !active.usage.merge_snapshot(frame) {
            return false;
        }
        self.snapshot.cumulative_usage = active.before_cumulative.plus(active.usage.disjoint());
        self.snapshot.cumulative_known =
            active.before_cumulative_known && active.usage.is_complete();
        if active.usage.prompt_tokens().is_some() {
            self.snapshot.last_confirmed = Some(ContextUsageConfirmation {
                request: request.clone(),
                usage: active.usage,
                time_confirmed: at_ms,
            });
            self.snapshot.estimated_tail_tokens = Some(0);
        }
        self.changed(at_ms);
        true
    }

    /// Seal the attempt when its assistant checkpoint is durable.
    pub fn commit_request(&mut self, request: &ContextRequestIdentity, at_ms: i64) -> bool {
        if self.snapshot.request.as_ref() != Some(request) {
            return false;
        }
        let Some(active) = self.active.as_mut().filter(|active| !active.committed) else {
            return false;
        };
        active.committed = true;
        self.snapshot.cumulative_known =
            active.before_cumulative_known && active.usage.is_complete();
        if !active.usage.is_complete() {
            // The assistant may contain unmeasured output. Retain the previous
            // measurement, but do not claim a measured empty tail.
            self.snapshot.estimated_tail_tokens = None;
        }
        self.changed(at_ms);
        true
    }

    /// Restore the context baseline and begin the next attempt exactly once.
    ///
    /// Discarding generated content does not refund observed provider usage.
    /// Preserve this attempt's counters and mark consumption incomplete because
    /// a failed stream cannot prove that its last snapshot was the final bill.
    pub fn rollback_request(
        &mut self,
        request: &ContextRequestIdentity,
        next_attempt: u32,
        at_ms: i64,
    ) -> bool {
        if self.snapshot.request.as_ref() != Some(request) || next_attempt <= request.attempt {
            return false;
        }
        let Some(active) = self.active.as_mut().filter(|active| !active.committed) else {
            return false;
        };
        self.snapshot.last_confirmed = active.before_confirmation.clone();
        active.before_cumulative = active.before_cumulative.plus(active.usage.disjoint());
        active.before_cumulative_known = false;
        self.snapshot.cumulative_usage = active.before_cumulative;
        self.snapshot.cumulative_known = false;
        self.snapshot.estimated_tail_tokens = active.request_tail;
        active.usage = ContextUsageCounters::default();
        if let Some(current) = self.snapshot.request.as_mut() {
            current.attempt = next_attempt;
        }
        self.changed(at_ms);
        true
    }

    /// Discard an uncheckpointed attempt when execution ends without a retry.
    ///
    /// Keep its request identity as the sequence fence, so a late frame cannot
    /// resurrect the discarded usage and an older request cannot be replayed.
    pub fn abandon_request(&mut self, request: &ContextRequestIdentity, at_ms: i64) -> bool {
        if self.snapshot.request.as_ref() != Some(request) {
            return false;
        }
        let Some(active) = self.active.as_ref().filter(|active| !active.committed) else {
            return false;
        };
        let request_tail = active.request_tail;
        self.discard_uncommitted();
        self.snapshot.estimated_tail_tokens = request_tail;
        self.active = None;
        self.changed(at_ms);
        true
    }

    /// Compaction changes the context epoch, not historical consumption.
    ///
    /// The next request can legitimately establish a much smaller context.
    pub fn reset_epoch(&mut self, context_epoch: u64, at_ms: i64) -> bool {
        if context_epoch <= self.snapshot.context_epoch {
            return false;
        }
        self.discard_uncommitted();
        self.snapshot.context_epoch = context_epoch;
        self.snapshot.request = None;
        self.snapshot.last_confirmed = None;
        self.snapshot.request_estimate_tokens = None;
        self.snapshot.estimated_tail_tokens = None;
        self.active = None;
        self.changed(at_ms);
        true
    }

    /// Invalidate measurements when compaction/revert changes the durable history
    /// checkpoint, even if that checkpoint moves backwards or is removed.
    pub fn observe_history_epoch(&mut self, history_epoch: i64, at_ms: i64) -> bool {
        let Ok(epoch) = u64::try_from(history_epoch) else {
            return false;
        };
        let previous = self.observed_history_epoch.replace(history_epoch);
        if previous == Some(history_epoch) {
            return false;
        }
        if previous.is_some() || self.snapshot.context_epoch != epoch {
            self.reset_epoch(
                self.snapshot.context_epoch.saturating_add(1).max(epoch),
                at_ms,
            );
        } else {
            self.changed(at_ms);
        }
        true
    }

    /// Replace the full unaccounted tail estimate, never add a repeated delta.
    pub fn set_estimated_tail(&mut self, tokens: Option<u64>, at_ms: i64) -> bool {
        if self.snapshot.estimated_tail_tokens == tokens {
            return false;
        }
        self.snapshot.estimated_tail_tokens = tokens;
        self.changed(at_ms);
        true
    }

    /// Seed historical totals when adopting a pre-snapshot session.
    ///
    /// Once a request exists its consumption is owned by that request's
    /// checkpoint, so callers cannot replace it with an unrelated session sum.
    pub fn seed_cumulative(&mut self, usage: ContextUsageTotals, known: bool, at_ms: i64) -> bool {
        if self.snapshot.request.is_some()
            || (self.snapshot.cumulative_usage == usage && self.snapshot.cumulative_known == known)
        {
            return false;
        }
        self.snapshot.cumulative_usage = usage;
        self.snapshot.cumulative_known = known;
        self.changed(at_ms);
        true
    }

    pub fn validate(&self) -> Result<(), InvalidContextUsage> {
        self.snapshot.validate()?;
        if self.observed_history_epoch.is_some_and(|epoch| epoch < 0) {
            return Err(InvalidContextUsage("invalid durable history epoch"));
        }
        if let Some(active) = &self.active {
            let request = self
                .snapshot
                .request
                .as_ref()
                .ok_or(InvalidContextUsage("attempt checkpoint has no request"))?;
            if let Some(confirmed) = &active.before_confirmation {
                validate_identity(
                    &confirmed.request,
                    self.snapshot.context_epoch,
                    self.snapshot.source,
                )?;
                if confirmed.usage.prompt_tokens().is_none()
                    || confirmed.request.request_sequence >= request.request_sequence
                    || confirmed.request.provider_id != request.provider_id
                    || confirmed.request.model_id != request.model_id
                {
                    return Err(InvalidContextUsage("invalid pre-attempt confirmation"));
                }
            }
            if self.snapshot.cumulative_usage
                != active.before_cumulative.plus(active.usage.disjoint())
            {
                return Err(InvalidContextUsage(
                    "attempt consumption disagrees with its checkpoint",
                ));
            }
            let confirmation_matches = if active.usage.prompt_tokens().is_some() {
                self.snapshot
                    .last_confirmed
                    .as_ref()
                    .is_some_and(|confirmed| {
                        &confirmed.request == request && confirmed.usage == active.usage
                    })
            } else {
                self.snapshot.last_confirmed == active.before_confirmation
            };
            if !confirmation_matches {
                return Err(InvalidContextUsage(
                    "attempt measurement disagrees with its checkpoint",
                ));
            }
            let expected_known =
                if active.committed || active.usage != ContextUsageCounters::default() {
                    active.before_cumulative_known && active.usage.is_complete()
                } else {
                    active.before_cumulative_known
                };
            if self.snapshot.cumulative_known != expected_known {
                return Err(InvalidContextUsage(
                    "attempt completeness disagrees with its checkpoint",
                ));
            }
        }
        Ok(())
    }

    /// A terminally incomplete response may have consumed more than it reported.
    pub fn mark_cumulative_unknown(&mut self, at_ms: i64) -> bool {
        let mut changed = self.snapshot.cumulative_known;
        if let Some(active) = &mut self.active {
            changed |= active.before_cumulative_known;
            active.before_cumulative_known = false;
        }
        if !changed {
            return false;
        }
        self.snapshot.cumulative_known = false;
        self.changed(at_ms);
        true
    }

    fn discard_uncommitted(&mut self) {
        if let Some(active) = &self.active
            && !active.committed
        {
            self.snapshot.last_confirmed = active.before_confirmation.clone();
            // Only context is rolled back. These counters were observed on a real
            // attempt, and a missing final response can hide additional charges.
            self.snapshot.cumulative_known = false;
        }
    }

    fn changed(&mut self, at_ms: i64) {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        self.snapshot.time_updated = self.snapshot.time_updated.max(at_ms);
        self.snapshot.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(sequence: u64) -> ContextRequestIdentity {
        ContextRequestIdentity {
            request_id: format!("request-{sequence}"),
            request_sequence: sequence,
            attempt: 1,
            context_epoch: 0,
            provider_id: "synthetic-provider".to_owned(),
            model_id: "synthetic-model".to_owned(),
            source: ContextUsageSource::Main,
            turn_id: Some("synthetic-turn".to_owned()),
            time_started: i64::try_from(sequence).unwrap(),
            request_context_tokens: None,
            history_prefix: None,
        }
    }

    fn counters(input: Option<u64>, output: Option<u64>) -> ContextUsageCounters {
        ContextUsageCounters {
            input_tokens: input,
            output_tokens: output,
            accounting: ContextTokenAccounting::CacheInsideInput,
            ..ContextUsageCounters::default()
        }
    }

    fn confirmed() -> ContextUsageTracker {
        let mut tracker = ContextUsageTracker::new("ses_synthetic");
        assert!(tracker.start_request(request(1), Some(70_000), Some(0), Some(200_000), 1));
        assert!(tracker.observe_usage(&request(1), counters(Some(125_350), Some(40)), 2));
        assert!(tracker.commit_request(&request(1), 3));
        tracker
    }

    #[test]
    fn lower_estimate_keeps_measurement_and_counts_the_unaccounted_tail() {
        let mut tracker = confirmed();
        assert!(tracker.start_request(request(2), Some(73_948), Some(3_000), Some(200_000), 4));
        assert_eq!(tracker.snapshot().used_tokens, Some(128_390));
        assert_eq!(tracker.snapshot().request_estimate_tokens, Some(73_948));
        assert_eq!(
            tracker.snapshot().freshness,
            ContextUsageFreshness::Estimated
        );
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 125_390);
        assert!(tracker.observe_usage(&request(2), counters(Some(149_501), Some(9)), 5));
        assert_eq!(tracker.snapshot().used_tokens, Some(149_510));
        assert_eq!(tracker.snapshot().estimated_tail_tokens, Some(0));
        assert_eq!(
            tracker.snapshot().freshness,
            ContextUsageFreshness::Confirmed
        );
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 274_900);
        tracker.validate().unwrap();
    }

    #[test]
    fn partial_frames_replace_fields_and_repeated_frames_are_idempotent() {
        let mut tracker = ContextUsageTracker::new("ses_synthetic");
        tracker.start_request(request(1), Some(10), Some(0), Some(200_000), 1);
        tracker.observe_usage(&request(1), counters(Some(149_501), None), 2);
        assert_eq!(
            tracker.snapshot().freshness,
            ContextUsageFreshness::Estimated
        );
        assert!(tracker.observe_usage(&request(1), counters(None, Some(9)), 3));
        assert_eq!(tracker.snapshot().used_tokens, Some(149_510));
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 149_510);
        let same = tracker.clone();
        assert!(!tracker.observe_usage(&request(1), counters(None, Some(9)), 99));
        assert_eq!(tracker, same);
        assert!(tracker.observe_usage(&request(1), counters(None, Some(12)), 4));
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 149_513);
        tracker.validate().unwrap();
    }

    #[test]
    fn cache_splits_and_reasoning_are_never_charged_twice() {
        for (accounting, expected) in [
            (ContextTokenAccounting::CacheInsideInput, 125),
            (ContextTokenAccounting::CacheBesideInput, 175),
        ] {
            let mut tracker = ContextUsageTracker::new("ses_synthetic");
            tracker.start_request(request(1), None, Some(0), None, 1);
            tracker.observe_usage(
                &request(1),
                ContextUsageCounters {
                    input_tokens: Some(100),
                    cache_read_input_tokens: Some(40),
                    accounting,
                    ..ContextUsageCounters::default()
                },
                2,
            );
            tracker.observe_usage(
                &request(1),
                ContextUsageCounters {
                    output_tokens: Some(25),
                    reasoning_tokens: Some(10),
                    cache_write_input_tokens: Some(10),
                    accounting,
                    ..ContextUsageCounters::default()
                },
                3,
            );
            assert_eq!(tracker.snapshot().used_tokens, Some(expected));
            assert_eq!(tracker.snapshot().cumulative_usage.total(), expected);
            assert_eq!(tracker.snapshot().cumulative_usage.output, 15);
            assert_eq!(tracker.snapshot().cumulative_usage.reasoning, 10);
            tracker.validate().unwrap();
        }
    }

    #[test]
    fn resume_then_rollback_restores_baseline_and_rejects_old_attempts() {
        let mut tracker = confirmed();
        tracker.start_request(request(2), Some(73_948), Some(5_000), Some(200_000), 4);
        tracker.observe_usage(&request(2), counters(Some(149_501), Some(9)), 5);
        let encoded = serde_json::to_vec(&tracker).unwrap();
        let mut resumed: ContextUsageTracker = serde_json::from_slice(&encoded).unwrap();
        resumed.validate().unwrap();
        assert!(resumed.rollback_request(&request(2), 2, 6));
        assert_eq!(resumed.snapshot().used_tokens, Some(130_390));
        assert_eq!(resumed.snapshot().cumulative_usage.total(), 274_900);
        assert!(!resumed.snapshot().cumulative_known);
        let rolled_back = resumed.clone();
        assert!(!resumed.rollback_request(&request(2), 2, 7));
        assert!(!resumed.observe_usage(&request(2), counters(Some(999_999), Some(99)), 8));
        assert!(!resumed.start_request(request(1), Some(1), Some(0), None, 9));
        assert_eq!(resumed, rolled_back);
        let mut retry = request(2);
        retry.attempt = 2;
        assert!(resumed.observe_usage(&retry, counters(Some(130_000), Some(5)), 10));
        assert_eq!(resumed.snapshot().cumulative_usage.total(), 404_905);
        assert!(!resumed.snapshot().cumulative_known);
        resumed.validate().unwrap();
    }

    #[test]
    fn compaction_can_legitimately_reduce_context_without_resetting_consumption() {
        let mut tracker = confirmed();
        assert!(tracker.reset_epoch(10, 4));
        assert_eq!(tracker.snapshot().used_tokens, None);
        assert_eq!(tracker.snapshot().freshness, ContextUsageFreshness::Unknown);
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 125_390);
        let mut compacted = request(2);
        compacted.context_epoch = 10;
        assert!(tracker.start_request(compacted.clone(), Some(1_000), Some(0), Some(200_000), 5));
        assert_eq!(tracker.snapshot().used_tokens, Some(1_000));
        assert!(tracker.observe_usage(&compacted, counters(Some(1_100), Some(4)), 6));
        assert_eq!(tracker.snapshot().used_tokens, Some(1_104));
        assert!(!tracker.observe_usage(&request(1), counters(Some(125_350), Some(40)), 7));
        assert!(!tracker.reset_epoch(9, 8));
        tracker.validate().unwrap();
    }

    #[test]
    fn model_change_invalidates_the_measurement_but_not_the_cumulative_meter() {
        let mut tracker = confirmed();
        let mut changed = request(2);
        changed.model_id = "another-model".to_owned();
        tracker.start_request(changed, Some(12_000), Some(0), Some(32_000), 4);
        assert_eq!(tracker.snapshot().last_confirmed, None);
        assert_eq!(tracker.snapshot().used_tokens, Some(12_000));
        assert_eq!(tracker.snapshot().context_limit, Some(32_000));
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 125_390);
        tracker.validate().unwrap();
    }

    #[test]
    fn unknown_is_distinct_from_confirmed_zero_and_from_unmeasured_tail() {
        let mut tracker = ContextUsageTracker::new("ses_synthetic");
        assert_eq!(tracker.snapshot().used_tokens, None);
        tracker.start_request(request(1), None, None, Some(200_000), 1);
        tracker.observe_usage(&request(1), counters(None, Some(9)), 2);
        assert_eq!(tracker.snapshot().used_tokens, None);
        assert_eq!(tracker.snapshot().freshness, ContextUsageFreshness::Unknown);
        tracker.observe_usage(&request(1), counters(Some(0), Some(0)), 3);
        assert_eq!(tracker.snapshot().used_tokens, Some(0));
        assert_eq!(
            tracker.snapshot().freshness,
            ContextUsageFreshness::Confirmed
        );
        tracker.set_estimated_tail(None, 4);
        assert_eq!(tracker.snapshot().used_tokens, None);
        assert!(tracker.snapshot().last_confirmed.is_some());
    }

    #[test]
    fn child_learning_and_other_sources_cannot_overwrite_the_main_context() {
        let mut main = confirmed();
        let unchanged = main.clone();
        for source in [
            ContextUsageSource::Child,
            ContextUsageSource::Learning,
            ContextUsageSource::Compaction,
            ContextUsageSource::Auxiliary,
        ] {
            let mut other = request(2);
            other.source = source;
            assert!(!main.start_request(other.clone(), Some(5), Some(0), Some(100), 4));
            assert!(!main.observe_usage(&other, counters(Some(5), Some(1)), 5));
            assert_eq!(main, unchanged);
            let mut independent = ContextUsageTracker::for_source("ses_synthetic", source);
            independent.start_request(other.clone(), Some(5), Some(0), Some(100), 4);
            independent.observe_usage(&other, counters(Some(5), Some(1)), 5);
            assert_eq!(independent.snapshot().used_tokens, Some(6));
            independent.validate().unwrap();
        }
    }

    #[test]
    fn large_tail_saturates_without_clamping_occupancy_to_the_context_limit() {
        let mut tracker = confirmed();
        tracker.start_request(request(2), Some(70_000), Some(200_000), Some(200_000), 4);
        assert_eq!(tracker.snapshot().used_tokens, Some(325_390));
        tracker.set_estimated_tail(Some(u64::MAX), 5);
        assert_eq!(tracker.snapshot().used_tokens, Some(u64::MAX));
        tracker.validate().unwrap();
    }

    #[test]
    fn changed_provider_counters_can_revise_usage_downward() {
        let mut tracker = ContextUsageTracker::new("ses_synthetic");
        tracker.start_request(request(1), Some(1_000), Some(0), Some(200_000), 1);
        tracker.observe_usage(&request(1), counters(Some(1_100), Some(40)), 2);
        tracker.observe_usage(&request(1), counters(None, Some(30)), 3);
        assert_eq!(tracker.snapshot().used_tokens, Some(1_130));
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 1_130);
    }

    #[test]
    fn a_committed_attempt_cannot_be_rolled_back_or_counted_again() {
        let mut tracker = confirmed();
        let committed = tracker.clone();
        assert!(!tracker.start_request(request(1), Some(1), Some(0), Some(1), 8));
        assert!(!tracker.commit_request(&request(1), 9));
        assert!(!tracker.rollback_request(&request(1), 2, 10));
        assert!(!tracker.observe_usage(&request(1), counters(Some(1), Some(1)), 11));
        assert_eq!(tracker, committed);
    }

    #[test]
    fn incomplete_checkpoint_preserves_unknown_occupancy_and_consumption() {
        let mut tracker = confirmed();
        tracker.start_request(request(2), Some(90_000), Some(1_000), None, 4);
        tracker.observe_usage(&request(2), counters(None, Some(9)), 5);
        tracker.commit_request(&request(2), 6);
        assert_eq!(tracker.snapshot().used_tokens, None);
        assert!(!tracker.snapshot().cumulative_known);
        assert_eq!(
            tracker
                .snapshot()
                .last_confirmed
                .as_ref()
                .unwrap()
                .request
                .request_id,
            "request-1"
        );
        tracker.validate().unwrap();
    }

    #[test]
    fn wire_snapshot_has_schema_and_rejects_inconsistent_derived_values() {
        let tracker = confirmed();
        let schema = schemars::schema_for!(ContextUsageSnapshot);
        assert!(serde_json::to_value(schema).unwrap()["properties"]["usedTokens"].is_object());
        let encoded = serde_json::to_vec(tracker.snapshot()).unwrap();
        let mut decoded: ContextUsageSnapshot = serde_json::from_slice(&encoded).unwrap();
        decoded.validate().unwrap();
        decoded.used_tokens = Some(73_948);
        assert!(decoded.validate().is_err());
    }

    #[test]
    fn abandoned_attempt_restores_usage_and_keeps_its_sequence_fence() {
        let mut tracker = confirmed();
        tracker.start_request(request(2), Some(73_948), Some(2_000), Some(200_000), 4);
        tracker.observe_usage(&request(2), counters(Some(149_501), Some(9)), 5);
        assert!(tracker.abandon_request(&request(2), 6));
        assert_eq!(tracker.snapshot().used_tokens, Some(127_390));
        assert_eq!(tracker.snapshot().cumulative_usage.total(), 274_900);
        assert!(!tracker.snapshot().cumulative_known);
        assert!(!tracker.abandon_request(&request(2), 7));
        assert!(!tracker.observe_usage(&request(2), counters(Some(999), Some(1)), 8));
        assert!(!tracker.start_request(request(1), Some(1), Some(0), Some(100), 9));
        tracker.validate().unwrap();
    }
}
