//! Per-project Git object stores: track, patch, diff, restore, and hourly GC.
//!
//! A snapshot is how the agent undoes its own edits. Every worktree gets one Git
//! object store, kept outside the user's repository so snapshots never appear in
//! their history, their index, or their reflog:
//!
//! ```text
//! $XDG_DATA_HOME/zuno/snapshot/<projectID>/<sha1(worktree path)>/
//! ```
//!
//! Ports `packages/opencode/src/snapshot/index.ts`.
//!
//! # One store, many sessions
//!
//! The store is keyed by `(project id, worktree)` — **not** by session. Ten
//! sessions in one checkout share one object database and address their snapshots
//! by tree hash, which is what makes the store cheap and what makes deleting one
//! dangerous. [`refcount`] answers "who still needs this store"; the reference
//! count, not the session lifetime, decides when a store may go.
//!
//! # Example
//!
//! ```no_run
//! use zuno_snapshot::{Location, Store};
//!
//! let location = Location::discover(std::path::Path::new("."));
//! let store = Store::open(location);
//!
//! let before = store.track()?.expect("snapshots are enabled");
//! // … the agent edits files …
//! let changed = store.patch(&before)?;
//! println!("{}", store.diff(&before)?);
//! store.restore(&before)?;
//! # Ok::<(), zuno_snapshot::SnapshotError>(())
//! ```

mod error;
mod gc;
mod git;
mod lock;
mod refcount;
mod store;
mod turn;

pub use crate::error::{Result, SnapshotError};
pub use crate::gc::{Collect, GcHandle, GcSchedule, spawn as spawn_gc};
pub use crate::refcount::{
    SessionRef, StoreKey, StoreReferences, discover_stores, is_worktree_hash, reference_counts,
    unreferenced_stores,
};
pub use crate::store::{
    Capture, CaptureExclusions, GC_ARGS, GcOutcome, LARGE_FILE_LIMIT, Location, PRUNE, Patch,
    Store, UNCERTAIN_RESTORE_FILE, UncertainRestore,
};
pub use crate::turn::{
    FileOperation, RestoredFile, TurnCapture, TurnCheckpoint, TurnRestore, TurnRestoreReport,
};
