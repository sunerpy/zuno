//! Turn-sized snapshot boundaries and restore reports.
//!
//! This module deliberately does not own an undo stack. Session ordering belongs
//! to the session owner, while this crate owns the filesystem safety boundary: one
//! serializable pair of trees and a checked transition between them.

use crate::error::{Result, SnapshotError};
use crate::store::{CaptureExclusions, Store};

/// The two complete worktree trees that bound one user turn.
///
/// A turn may contain several provider steps and many tool calls. Capturing outside
/// that whole interval makes undo match the user's mental model without coupling the
/// snapshot crate to either messages or tools.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnCheckpoint {
    before: String,
    after: String,
    /// Defaulted so a checkpoint serialized before exclusions were recorded still
    /// decodes; an absent field means "nothing known to be excluded".
    #[serde(default)]
    exclusions: CaptureExclusions,
}

impl TurnCheckpoint {
    /// Pair the tree captured before a turn with the tree captured after it.
    #[must_use]
    pub fn new(before: impl Into<String>, after: impl Into<String>) -> Self {
        Self {
            before: before.into(),
            after: after.into(),
            exclusions: CaptureExclusions::default(),
        }
    }

    /// Attach the paths both boundary captures deliberately left out.
    #[must_use]
    pub fn with_exclusions(mut self, exclusions: CaptureExclusions) -> Self {
        self.exclusions = exclusions;
        self
    }

    /// The paths neither boundary of this turn captured.
    ///
    /// These are the paths `/undo` and `/redo` cannot move, because they are not in
    /// either tree. A client that reports a restore should report these too — see
    /// [`CaptureExclusions`].
    #[must_use]
    pub const fn exclusions(&self) -> &CaptureExclusions {
        &self.exclusions
    }

    /// The tree that existed immediately before the turn.
    #[must_use]
    pub fn before(&self) -> &str {
        &self.before
    }

    /// The tree that existed immediately after the turn.
    #[must_use]
    pub fn after(&self) -> &str {
        &self.after
    }

    pub(crate) fn transition(&self, restore: TurnRestore) -> (&str, &str) {
        match restore {
            TurnRestore::Undo => (&self.after, &self.before),
            TurnRestore::Redo => (&self.before, &self.after),
        }
    }
}

/// An in-progress turn capture returned by [`Store::begin_turn`].
///
/// Dropping this value is cancellation: no incomplete checkpoint is exposed. The
/// caller receives a usable [`TurnCheckpoint`] only after [`TurnCapture::finish`]
/// captures the other boundary.
///
/// # A failed turn still needs its checkpoint
///
/// Whether a turn succeeded is independent of whether it wrote files. A turn that
/// ends in a provider error, a cancellation or a tool failure has usually already
/// edited the worktree, so returning early without finishing the capture is how
/// `/undo` loses the very edits the user most wants back. Route the turn's own
/// outcome through [`TurnCapture::finish_with`] instead of propagating it with `?`
/// first.
#[derive(Clone, Debug)]
#[must_use = "dropping a TurnCapture cancels the checkpoint and loses the turn's undo boundary"]
pub struct TurnCapture {
    store: Store,
    before: String,
    exclusions: CaptureExclusions,
}

impl TurnCapture {
    pub(crate) fn new(store: Store, before: String, exclusions: CaptureExclusions) -> Self {
        Self {
            store,
            before,
            exclusions,
        }
    }

    /// The already-captured pre-turn tree.
    #[must_use]
    pub fn before(&self) -> &str {
        &self.before
    }

    /// The paths the pre-turn capture deliberately left out.
    #[must_use]
    pub const fn exclusions(&self) -> &CaptureExclusions {
        &self.exclusions
    }

    /// Capture the post-turn tree and produce a persistent checkpoint pair.
    pub fn finish(self) -> Result<TurnCheckpoint> {
        let after = self
            .store
            .capture()?
            .ok_or(SnapshotError::SnapshotsDisabled)?;
        let mut exclusions = self.exclusions;
        exclusions.merge(after.exclusions().clone());
        Ok(TurnCheckpoint::new(self.before, after.tree().to_owned()).with_exclusions(exclusions))
    }

    /// Finish the capture for a turn that has ended, whatever its outcome was.
    ///
    /// Returns the checkpoint alongside `outcome`, so the caller cannot propagate a
    /// turn failure with `?` before the post-turn tree has been captured. Both
    /// results still need handling: a checkpoint failure is a snapshot problem to
    /// surface, and `outcome` is the turn's own verdict.
    #[must_use = "both the checkpoint and the turn outcome must be handled"]
    pub fn finish_with<T, E>(
        self,
        outcome: Result<T, E>,
    ) -> (Result<TurnCheckpoint>, Result<T, E>) {
        (self.finish(), outcome)
    }
}

/// Which boundary a checked turn restore should move toward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnRestore {
    /// Move from the post-turn tree to the pre-turn tree.
    Undo,
    /// Move from the pre-turn tree back to the post-turn tree.
    Redo,
}

impl TurnRestore {
    /// The user-facing verb for this direction.
    ///
    /// Owned here so every surface — error text, restore summaries, client status
    /// lines — spells the operation the same way instead of each re-deriving it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }
}

impl std::fmt::Display for TurnRestore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What applying the target tree did to one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileOperation {
    /// The target tree created a path absent from the source tree.
    Created,
    /// The target tree replaced the content or mode of an existing path.
    Modified,
    /// The target tree removed a path present in the source tree.
    Deleted,
}

/// One path changed by a successful turn restore.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoredFile {
    /// Forward-slashed path relative to the worktree root.
    pub path: String,
    /// The operation performed while moving toward the target boundary.
    pub operation: FileOperation,
}

/// Complete caller-facing account of a successful turn restore.
///
/// A restore that reaches this type *succeeded*: the worktree is exactly the target
/// tree. [`TurnRestoreReport::summary`] renders that as one unambiguous line, so a
/// client never has to invent success wording — or, worse, publish a success as an
/// advisory detail a renderer is free to drop.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use = "a successful undo or redo must be reported to the user"]
pub struct TurnRestoreReport {
    restore: TurnRestore,
    from: String,
    to: String,
    files: Vec<RestoredFile>,
    #[serde(default)]
    exclusions: CaptureExclusions,
}

impl TurnRestoreReport {
    pub(crate) fn new(
        restore: TurnRestore,
        from: impl Into<String>,
        to: impl Into<String>,
        files: Vec<RestoredFile>,
        exclusions: CaptureExclusions,
    ) -> Self {
        Self {
            restore,
            from: from.into(),
            to: to.into(),
            files,
            exclusions,
        }
    }

    /// Whether this report describes undo or redo.
    #[must_use]
    pub const fn restore(&self) -> TurnRestore {
        self.restore
    }

    /// The exact tree required before the operation was allowed to start.
    #[must_use]
    pub fn from(&self) -> &str {
        &self.from
    }

    /// The exact tree present after the operation completed.
    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    /// Every path changed, sorted by relative path.
    #[must_use]
    pub fn files(&self) -> &[RestoredFile] {
        &self.files
    }

    /// The paths this checkpoint never captured, and so could not restore.
    #[must_use]
    pub const fn exclusions(&self) -> &CaptureExclusions {
        &self.exclusions
    }

    /// How many paths were created, modified and deleted, in that order.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |wanted: FileOperation| {
            self.files
                .iter()
                .filter(|file| file.operation == wanted)
                .count()
        };
        (
            count(FileOperation::Created),
            count(FileOperation::Modified),
            count(FileOperation::Deleted),
        )
    }

    /// One line stating, unambiguously, that the restore happened and what it did.
    ///
    /// This is the text a client should show on success. It never begins with a
    /// severity word, so a surface that filters on one must special-case the report
    /// rather than silently discard it.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = if self.files.is_empty() {
            format!(
                "{} complete: the worktree already matched tree {}, so no file changed",
                self.restore,
                short(&self.to)
            )
        } else {
            let (created, modified, deleted) = self.counts();
            format!(
                "{} complete: {} file(s) restored to tree {} ({created} created, {modified} modified, {deleted} deleted)",
                self.restore,
                self.files.len(),
                short(&self.to)
            )
        };
        if let Some(note) = self.exclusions.summary() {
            text.push_str("; ");
            text.push_str(&note);
        }
        text
    }
}

impl std::fmt::Display for TurnRestoreReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.summary())
    }
}

/// An abbreviated tree hash for human-facing text. Full hashes stay available on
/// [`TurnRestoreReport::from`] and [`TurnRestoreReport::to`].
fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}
