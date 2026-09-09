//! A stable, process-shared lock for cooperating writers of one replaceable path.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Holds exclusive write authority for one resolved destination.
///
/// The lock lives beside the destination and is never unlinked: replacing or
/// deleting the lock file would let another writer lock a different inode.
/// Closing this guard releases the operating-system lock, including after a
/// process crash. Acquisition never waits on another writer.
#[derive(Debug)]
pub struct PathWriteGuard {
    destination: PathBuf,
    _lock: File,
}

impl PathWriteGuard {
    /// Resolve aliases and acquire the destination's stable writer lock.
    ///
    /// Returns `WouldBlock` when a cooperating writer already owns the path.
    /// Callers must acquire the guard before checking the expected contents and
    /// retain it until publication and any associated settlement finish.
    pub fn try_acquire(path: &Path) -> io::Result<Self> {
        let resolved = canonical_destination(path)?;
        let name = resolved.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "write target must name a file")
        })?;
        let parent = resolved
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let parent = fs::canonicalize(parent)?;
        let destination = parent.join(name);
        let mut lock_name = name.to_os_string();
        lock_name.push(".zuno-write-lock");
        let lock_path = parent.join(lock_name);
        match fs::symlink_metadata(&lock_path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "writer lock must be a regular file",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        match lock.try_lock() {
            Ok(()) => Ok(Self {
                destination,
                _lock: lock,
            }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another writer owns this destination",
            )),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// The exact destination protected by this guard.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

/// Canonicalize an existing target or its nearest existing parent without writes.
///
/// This gives a durable document the same identity through relative paths and
/// directory aliases even before its optional file projection has been created.
pub fn canonical_destination(path: &Path) -> io::Result<PathBuf> {
    let resolved = super::follow_link_chain(path)?;
    match fs::canonicalize(&resolved) {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let name = resolved.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "write target must name a file")
    })?;
    let mut ancestor = resolved
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&ancestor) {
            Ok(mut root) => {
                for component in missing.into_iter().rev() {
                    root.push(component);
                }
                return Ok(root.join(name));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(&ancestor)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(error);
                }
                let component = ancestor.file_name().ok_or(error)?.to_os_string();
                missing.push(component);
                ancestor = ancestor
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
#[path = "write_guard_tests.rs"]
mod tests;
