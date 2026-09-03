//! In-process serialization of store operations.
//!
//! The oracle guards each store with a `Semaphore` keyed by git directory
//! (`packages/opencode/src/snapshot/index.ts:53-64`) because `add`, `write-tree`
//! and `read-tree` all mutate one shared index file. This is the same guard.
//!
//! It is deliberately *only* in-process. Cross-process exclusion is left to Git's
//! own `index.lock`, which is the mechanism the store's on-disk format already
//! implies; adding a second, non-Git lock file on top would invent a protocol the
//! TypeScript binary does not speak.
//!
//! `index.lock` does not queue. When two processes reach the same store's index
//! together the loser does not wait for its turn — git exits immediately with
//! `Unable to create '<store>/index.lock': File exists.`, which this crate surfaces
//! as [`crate::SnapshotError::Git`]. That is a correct fail-closed outcome, not
//! serialization: the caller sees a failed capture rather than a delayed one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// One leaked mutex per git directory. The set of stores a process touches is
/// bounded by the worktrees it opens, so this is a handful of words, and a
/// `&'static Mutex` is what lets the guard be returned to the caller.
type Registry = Mutex<HashMap<PathBuf, &'static Mutex<()>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take the lock for `git_dir`, blocking until it is free.
///
/// Poisoning is recovered from rather than propagated: the guarded data is `()`,
/// so a panic in another thread's critical section leaves nothing inconsistent in
/// *this* map. Git's own locking protects the index.
pub(crate) fn acquire(git_dir: &Path) -> MutexGuard<'static, ()> {
    let mutex = {
        let mut registry = registry().lock().unwrap_or_else(PoisonError::into_inner);
        *registry
            .entry(git_dir.to_path_buf())
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
    };
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_directory_maps_to_the_same_mutex() {
        let first = {
            let mut registry = registry().lock().expect("registry");
            *registry
                .entry(PathBuf::from("/store/a"))
                .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
        };
        let second = {
            let mut registry = registry().lock().expect("registry");
            *registry
                .entry(PathBuf::from("/store/a"))
                .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
        };
        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn different_directories_do_not_block_each_other() {
        let first = acquire(Path::new("/store/lock-b"));
        let second = acquire(Path::new("/store/lock-c"));
        drop((first, second));
    }

    #[test]
    fn a_second_acquire_of_one_directory_waits() {
        let guard = acquire(Path::new("/store/lock-d"));
        let handle = std::thread::spawn(|| {
            let inner = acquire(Path::new("/store/lock-d"));
            drop(inner);
        });
        drop(guard);
        handle
            .join()
            .expect("the waiter proceeds once the lock is released");
    }
}
