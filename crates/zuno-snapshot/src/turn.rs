//! Turn-sized snapshot boundaries and restore reports.
//!
//! This module deliberately does not own an undo stack. Session ordering belongs
//! to the session owner, while this crate owns the filesystem safety boundary: one
//! serializable pair of trees and a checked transition between them.

use crate::error::{Result, SnapshotError};
use crate::store::Store;

/// The two complete worktree trees that bound one user turn.
///
/// A turn may contain several provider steps and many tool calls. Capturing outside
/// that whole interval makes undo match the user's mental model without coupling the
/// snapshot crate to either messages or tools.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnCheckpoint {
    before: String,
    after: String,
}

impl TurnCheckpoint {
    /// Pair the tree captured before a turn with the tree captured after it.
    #[must_use]
    pub fn new(before: impl Into<String>, after: impl Into<String>) -> Self {
        Self {
            before: before.into(),
            after: after.into(),
        }
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
#[derive(Clone, Debug)]
pub struct TurnCapture {
    store: Store,
    before: String,
}

impl TurnCapture {
    pub(crate) fn new(store: Store, before: String) -> Self {
        Self { store, before }
    }

    /// The already-captured pre-turn tree.
    #[must_use]
    pub fn before(&self) -> &str {
        &self.before
    }

    /// Capture the post-turn tree and produce a persistent checkpoint pair.
    pub fn finish(self) -> Result<TurnCheckpoint> {
        let after = self
            .store
            .track()?
            .ok_or(SnapshotError::SnapshotsDisabled)?;
        Ok(TurnCheckpoint::new(self.before, after))
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnRestoreReport {
    restore: TurnRestore,
    from: String,
    to: String,
    files: Vec<RestoredFile>,
}

impl TurnRestoreReport {
    pub(crate) fn new(
        restore: TurnRestore,
        from: impl Into<String>,
        to: impl Into<String>,
        files: Vec<RestoredFile>,
    ) -> Self {
        Self {
            restore,
            from: from.into(),
            to: to.into(),
            files,
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
}
