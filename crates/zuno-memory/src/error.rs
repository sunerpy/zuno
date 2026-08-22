//! What a refused memory write reports, and why the shape differs from success.
//!
//! # The asymmetry is the design
//!
//! A **successful** write returns [`crate::Usage`] and nothing else — no entry
//! list. A **failed** write carries the current entries. That asymmetry is the
//! single most expensive lesson in the reference, recorded verbatim at
//! `memory_tool.py:711-723`:
//!
//! > We do NOT echo the full entries list here — dumping it invites the model to
//! > "find more to fix" and re-issue the same operations (observed thrash: the
//! > correct batch on call 1, then 5 redundant repeats). Entries are only shown on
//! > the error/over-budget paths, where the model genuinely needs them to decide
//! > what to consolidate.
//!
//! So the rule runs in both directions and both directions matter. Success is
//! terminal: it says the write landed and gives the model nothing to act on.
//! Failure is actionable: it hands over exactly the material needed to consolidate
//! and retry. Adding the entry list to the success path looks like helpfulness and
//! costs five redundant tool calls per write.

use crate::scope::Scope;
use std::path::PathBuf;

/// Width of the one-line entry previews carried in error messages.
///
/// `memory_tool.py:698`. Long enough to identify an entry, short enough that a
/// full store's worth of previews stays readable in a tool result.
const PREVIEW_WIDTH: usize = 80;

/// Truncate an entry to one identifiable line.
fn preview(entry: &str) -> String {
    let flattened = entry.replace('\n', " ");
    let mut out: String = flattened.chars().take(PREVIEW_WIDTH).collect();
    if flattened.chars().count() > PREVIEW_WIDTH {
        out.push_str("...");
    }
    out
}

fn numbered(entries: &[String]) -> String {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| format!("\n  {}. {}", index + 1, preview(entry)))
        .collect()
}

/// Everything that can refuse a memory operation.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// A batch with no operations. Distinguished from a no-op batch so a caller
    /// bug is not mistaken for a satisfied request.
    #[error("no operations were supplied")]
    EmptyBatch,

    /// An operation could not be built from its fields — an `add` with no content,
    /// a `remove` with no locator, an unknown action.
    ///
    /// Kept as an error rather than made unrepresentable by the type system,
    /// because todo 100 parses these from model-supplied JSON and needs the
    /// message rather than a serde rejection.
    #[error("operation {index} ({action}): {reason}")]
    MalformedOperation {
        /// One-based position, matching the reference's `Operation {i + 1}`.
        index: usize,
        /// The action as the caller named it.
        action: String,
        /// What was missing or wrong.
        reason: String,
    },

    /// Content matched a prompt-injection or exfiltration pattern.
    ///
    /// Raised before anything touches disk: one poisoned operation rejects the
    /// whole batch (`memory_tool.py:577-586`).
    #[error("operation {index}: {threat}")]
    Blocked {
        /// One-based position of the offending operation.
        index: usize,
        /// Which pattern or codepoint tripped.
        threat: crate::threat::Threat,
    },

    /// A `replace` or `remove` locator matched no entry.
    ///
    /// Carries the entries so the caller can pick a real locator instead of
    /// guessing again.
    #[error(
        "operation {index}: no entry contains {needle:?}. No operations were applied \
         (the batch is all-or-nothing). Current entries ({}):{}",
        entries.len(),
        numbered(entries)
    )]
    NoMatch {
        /// One-based position of the offending operation.
        index: usize,
        /// The substring that matched nothing.
        needle: String,
        /// Every entry in the store as it stands, unmodified.
        entries: Vec<String>,
    },

    /// A locator matched two or more *distinct* entries.
    ///
    /// Ambiguity is refused rather than resolved by position, because "the first
    /// match" is not something the caller can predict from the substring it sent —
    /// it would silently edit the wrong note. Identical duplicate entries are not
    /// ambiguous: they are the same text, so acting on the first is well defined.
    #[error(
        "operation {index}: {needle:?} matched {} distinct entries — be more specific. \
         No operations were applied (the batch is all-or-nothing). Matches:{}",
        matches.len(),
        numbered(matches)
    )]
    Ambiguous {
        /// One-based position of the offending operation.
        index: usize,
        /// The substring that matched too much.
        needle: String,
        /// The distinct entries it matched.
        matches: Vec<String>,
        /// Every entry in the store as it stands, unmodified.
        entries: Vec<String>,
    },

    /// The batch's **final** result would exceed the scope's cap.
    ///
    /// Nothing is evicted to make room. A store at its cap is a signal that the
    /// model's notes need consolidating, and consolidating is a judgement about
    /// which of two overlapping facts to keep — not something a byte count can
    /// decide. So the write fails, the entries come back, and the next call can
    /// merge deliberately in one batch.
    #[error(
        "{scope} memory would be at {projected}/{limit} chars after this batch — over the cap. \
         Nothing was written. Remove or shorten entries in the same batch, then retry. \
         Current entries ({}):{}",
        entries.len(),
        numbered(entries)
    )]
    CapExceeded {
        /// Which store refused.
        scope: Scope,
        /// Where the batch's final result would have landed.
        projected: usize,
        /// The cap it would have crossed.
        limit: usize,
        /// Every entry in the store as it stands, unmodified.
        entries: Vec<String>,
    },

    /// The file changed under us between load and write.
    ///
    /// Refused rather than overwritten, with the on-disk text preserved at
    /// `backup`. The alternative is silent data loss: rewriting the whole file
    /// from a stale view discards whatever the other writer added.
    #[error(
        "refusing to write {}: {reason}. A snapshot of the on-disk file was saved to {}. \
         Resolve the drift first — reconcile the snapshot into the store one entry at a \
         time — then retry.",
        path.display(),
        backup.display()
    )]
    ExternalDrift {
        /// The store that was not written.
        path: PathBuf,
        /// Which drift signal fired.
        reason: DriftReason,
        /// Where the pre-existing bytes were preserved.
        backup: PathBuf,
    },

    /// The resident file no longer matches an audited apply or undo snapshot.
    #[error(
        "refusing to replace {}: resident memory changed after the audited snapshot was prepared",
        path.display()
    )]
    StateMismatch {
        /// The store that changed.
        path: PathBuf,
        /// Snapshot the operation was prepared against.
        expected: Vec<String>,
        /// Entries now present on disk.
        actual: Vec<String>,
    },

    /// The file exists but could not be read as UTF-8 text.
    ///
    /// Treated as abort, never as "empty store". A read-modify-write that took a
    /// failed read for an empty file would rewrite the store down to whatever the
    /// current batch adds, wiping every prior note — the reference is emphatic
    /// about this at `memory_tool.py:750-770`.
    #[error(
        "refusing to write {}: the file exists but could not be read, so its contents cannot \
         be preserved. Nothing was written.",
        path.display()
    )]
    Unreadable {
        /// The store that was not written.
        path: PathBuf,
    },

    /// A filesystem operation failed.
    #[error("{operation} {}: {source}", path.display())]
    Io {
        /// What was being attempted.
        operation: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

impl MemoryError {
    /// The store's entries at the point of refusal, for the variants that carry
    /// them.
    ///
    /// Todo 100 serializes these into the tool result. Kept as an accessor as well
    /// as in [`std::fmt::Display`] so a caller can render them its own way without
    /// parsing the message.
    #[must_use]
    pub fn current_entries(&self) -> Option<&[String]> {
        match self {
            Self::NoMatch { entries, .. }
            | Self::Ambiguous { entries, .. }
            | Self::CapExceeded { entries, .. } => Some(entries),
            _ => None,
        }
    }

    /// Whether the refusal is one the model can act on by consolidating.
    ///
    /// Todo 100's circuit breaker counts these: a cap or locator failure is worth
    /// one more attempt, a blocked pattern or a drifted file is not.
    #[must_use]
    pub const fn is_consolidation_failure(&self) -> bool {
        matches!(
            self,
            Self::NoMatch { .. } | Self::Ambiguous { .. } | Self::CapExceeded { .. }
        )
    }

    /// Whether a proposal can be corrected by changing its model-supplied fields.
    #[must_use]
    pub const fn is_proposal_correctable(&self) -> bool {
        matches!(
            self,
            Self::EmptyBatch
                | Self::MalformedOperation { .. }
                | Self::Blocked { .. }
                | Self::NoMatch { .. }
                | Self::Ambiguous { .. }
                | Self::CapExceeded { .. }
        )
    }

    /// Whether the destination rename may have landed before this error arose.
    #[must_use]
    pub fn may_have_written(&self) -> bool {
        matches!(
            self,
            Self::Io {
                operation: "stat after write",
                ..
            }
        )
    }
}

/// Which signal identified the file as externally modified.
///
/// Three signals, not one. The plan specifies mtime-plus-length; the reference
/// uses two structural checks instead (`memory_tool.py:807-856`). They catch
/// different writers and neither subsumes the other, so all three run — see
/// [`crate::store::MemoryStore::apply_batch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftReason {
    /// Modification time or byte length differs from what the load observed.
    ///
    /// The only signal that catches a hand edit which *keeps* the §-delimited
    /// shape — the other two would see a well-formed file and let the write
    /// through, silently discarding the edit.
    Stamp,
    /// The file does not survive a parse-then-serialize round trip.
    ///
    /// Something wrote content the parser cannot faithfully reproduce, so
    /// rewriting the file from parsed entries would lose bytes.
    RoundTrip,
    /// A single parsed entry is larger than the whole store's cap.
    ///
    /// Impossible for a tool-written entry, since the cap is checked against the
    /// entire store. An external writer appended free-form text into what the
    /// parser now reads as one entry.
    EntryOverflow,
}

impl std::fmt::Display for DriftReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stamp => {
                "the file's timestamp and length changed after this store was loaded, so an \
                 external writer edited it"
            }
            Self::RoundTrip => {
                "the file on disk holds content that would not survive a parse-and-rewrite, \
                 so rewriting it would discard bytes"
            }
            Self::EntryOverflow => {
                "one entry on disk is larger than the whole store's cap, which only happens \
                 when an external writer appended into it"
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_flattens_and_truncates() {
        assert_eq!(preview("one\ntwo"), "one two");
        let long = "x".repeat(PREVIEW_WIDTH + 10);
        let shown = preview(&long);
        assert_eq!(shown.chars().count(), PREVIEW_WIDTH + 3);
        assert!(shown.ends_with("..."));
    }

    #[test]
    fn cap_exceeded_message_lists_the_entries() {
        let err = MemoryError::CapExceeded {
            scope: Scope::Global,
            projected: 2_400,
            limit: 2_200,
            entries: vec![
                "prefers tabs".to_string(),
                "hates trailing whitespace".to_string(),
            ],
        };
        let text = err.to_string();
        assert!(text.contains("2400/2200"), "{text}");
        assert!(text.contains("1. prefers tabs"), "{text}");
        assert!(text.contains("2. hates trailing whitespace"), "{text}");
        assert_eq!(err.current_entries().map(<[String]>::len), Some(2));
        assert!(err.is_consolidation_failure());
    }

    #[test]
    fn drift_is_not_a_consolidation_failure() {
        let err = MemoryError::ExternalDrift {
            path: PathBuf::from("/tmp/MEMORY.md"),
            reason: DriftReason::Stamp,
            backup: PathBuf::from("/tmp/MEMORY.md.bak.1"),
        };
        assert!(!err.is_consolidation_failure());
        assert!(err.current_entries().is_none());
        assert!(err.to_string().contains("MEMORY.md.bak.1"));
    }
}
