//! Visibility-atomic same-directory file replacement and consistent reads.
//!
//! A completed sibling file is published over the destination in one name
//! transition. Unix uses `rename`; Windows must use `ReplaceFileW` for an
//! existing destination because Rust's `rename` path uses `MoveFileEx`, whose
//! replace-existing behavior is not equivalent.
//!
//! This primitive deliberately does not promise crash durability. Callers that
//! need a power-loss boundary must sync the temporary file and directory as a
//! separate policy. A successful concurrent open sees either the complete old
//! bytes or the complete new bytes.
//!
//! Windows has a second boundary: while `ReplaceFileW` holds its kernel handles,
//! a fresh open can transiently fail with `ERROR_FILE_NOT_FOUND` or
//! `ERROR_SHARING_VIOLATION`. [`read`] and [`read_to_string`] absorb only those
//! two platform errors with a bounded backoff when the caller already expects a
//! published file. Their `*_optional` counterparts keep first-load absence
//! cheap, but upgrade to the same retry after a sharing violation proves that a
//! file is present. They do not retry permission, encoding, path, or device
//! failures, and Unix reads remain a single system call.
//!
//! A destination that is a symlink is followed deliberately, to the file the link
//! names, and the replacement is published in that file's own directory. Publishing at
//! the link's own name would rename a regular file over the link and silently destroy
//! it, which turns a user's deliberate redirection into a one-way data loss: the next
//! read would see Zuno's document where the user expected their own location to be
//! authoritative. Following matches the `fs::write` semantics this primitive replaced,
//! so a caller that must not follow a link has to inspect the path itself first.
//!
//! The Windows provider is the safe `winsafe` wrapper. That wrapper accepts
//! Unicode strings rather than arbitrary `OsStr` values, so an unrepresentable
//! Windows path fails closed instead of silently falling back to the
//! gap-producing `MoveFileEx` behavior.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Replace `path` with `contents` without exposing partially written bytes.
///
/// The temporary file is created exclusively beside the destination and its
/// handle is closed before publication. A failed publication removes that
/// temporary file and leaves an existing destination untouched.
///
/// When `path` is a symlink, the link is followed to the file it names and the
/// replacement is published there, so the link survives and keeps pointing where the
/// user aimed it. A dangling link resolves to the location it names, which is then
/// created.
///
/// # Errors
///
/// Returns the underlying filesystem error. A symlink chain longer than
/// [`MAX_LINK_DEPTH`] is rejected with [`io::ErrorKind::InvalidInput`] rather than
/// followed further. On Windows, a path that the safe `ReplaceFileW` wrapper cannot
/// represent is rejected with the same kind.
pub fn replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    let resolved = follow_link_chain(path)?;
    let path = resolved.as_path();
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} does not name a file", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let mut temporary_name = file_name.to_os_string();
    temporary_name.push(format!(".tmp.{}", Uuid::new_v4()));
    let temporary = parent.join(temporary_name);

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        drop(file);
        publish(&temporary, path)
    })();
    if result.is_err() {
        let _ignored = fs::remove_file(&temporary);
    }
    result
}

/// Read one complete version of a path that is expected to be published.
///
/// On Windows, a replacement can briefly reject a fresh open; the two
/// corresponding kernel errors are retried with a bounded positive backoff.
///
/// # Errors
///
/// Returns an I/O error when the path remains missing after the replacement
/// window, or for any non-transient failure.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_published(|| fs::read(path))
}

/// Read one complete UTF-8 version of `path`.
///
/// This has the same expected-presence and retry semantics as [`read`].
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] when the published bytes are not
/// UTF-8, or the underlying I/O error for a non-transient failure.
pub fn read_to_string(path: &Path) -> io::Result<String> {
    let bytes = read(path)?;
    String::from_utf8(bytes).map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))
}

/// Read a path that may never have been published.
///
/// A first `NotFound` returns `Ok(None)` immediately. On Windows, a sharing
/// violation proves that the file exists and upgrades the call to [`read`]'s
/// bounded replacement policy.
///
/// # Errors
///
/// Returns an I/O error for every failure except first-load absence.
pub fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    first_load_optional(|| fs::read(path))
}

/// Read an optional UTF-8 path with [`read_optional`]'s semantics.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] for non-UTF-8 bytes, or the
/// underlying non-transient I/O failure.
pub fn read_to_string_optional(path: &Path) -> io::Result<Option<String>> {
    let Some(bytes) = read_optional(path)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))
}

/// Read metadata for a path that is expected to be published.
///
/// This is useful when a caller needs a pre-content stamp for optimistic
/// concurrency.
///
/// # Errors
///
/// Has the same expected-presence and retry semantics as [`read`].
pub fn metadata(path: &Path) -> io::Result<fs::Metadata> {
    read_published(|| fs::metadata(path))
}

/// Read metadata for a path that may never have been published.
///
/// # Errors
///
/// Has the same first-load absence semantics as [`read_optional`].
pub fn metadata_optional(path: &Path) -> io::Result<Option<fs::Metadata>> {
    first_load_optional(|| fs::metadata(path))
}

#[cfg(not(windows))]
fn read_published<T>(operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    operation()
}

#[cfg(not(windows))]
fn first_load_optional<T>(operation: impl FnOnce() -> io::Result<T>) -> io::Result<Option<T>> {
    match operation() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn read_published<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    const RETRY_BUDGET: Duration = Duration::from_secs(1);
    const MAX_DELAY: Duration = Duration::from_millis(8);

    let started = Instant::now();
    let mut delay = Duration::from_millis(1);
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_replacement_window(&error) => {
                if started.elapsed() >= RETRY_BUDGET {
                    return Err(error);
                }
                thread::sleep(delay);
                delay = delay.saturating_mul(2).min(MAX_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn first_load_optional<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<Option<T>> {
    match operation() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.raw_os_error() == Some(32) => read_published(operation).map(Some),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn is_replacement_window(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2 | 32))
}

/// Longest symlink chain a replacement will follow before it refuses.
///
/// Matches the `SYMLOOP_MAX` most platforms enforce, so a chain this primitive accepts
/// is one the kernel would also have resolved.
pub const MAX_LINK_DEPTH: usize = 40;

/// The file a destination ultimately names, following symlinks deliberately.
///
/// A path that does not exist, or that exists and is not a link, is returned as it
/// stands. A relative link target is joined to the directory holding the link, which is
/// how the kernel resolves it.
fn follow_link_chain(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_DEPTH {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_symlink() => {}
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
        let target = fs::read_link(&current)?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} resolves through more than {MAX_LINK_DEPTH} symlinks, which is a loop rather \
             than a redirection",
            path.display()
        ),
    ))
}

#[cfg(not(windows))]
fn publish(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

/// `destination` has already been resolved through any symlink by [`replace`], so the
/// existence test below distinguishes a real file from an absent one rather than a link
/// from its target.
#[cfg(windows)]
fn publish(temporary: &Path, destination: &Path) -> io::Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(_) => replace_existing(temporary, destination),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(temporary, destination),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn replace_existing(temporary: &Path, destination: &Path) -> io::Result<()> {
    let temporary = unicode_path(temporary)?;
    let destination = unicode_path(destination)?;
    winsafe::ReplaceFile(
        destination,
        temporary,
        None,
        winsafe::co::REPLACEFILE::default(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw() as i32))
}

#[cfg(windows)]
fn unicode_path(path: &Path) -> io::Result<&str> {
    path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} cannot be represented losslessly for Windows atomic replacement",
                path.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn replacement_keeps_one_complete_version_visible() {
        #[cfg(not(windows))]
        const REPLACEMENTS: u64 = 500;
        // ReplaceFileW performs materially more filesystem work than rename.
        // Successful-read overlap, asserted below, is the proof of concurrency;
        // repeating the same transition for ten seconds adds no stronger
        // visibility claim and can span several independent retry windows.
        #[cfg(windows)]
        const REPLACEMENTS: u64 = 128;

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("projection.md");
        let old = vec![b'a'; 32 * 1024];
        let new = vec![b'b'; 32 * 1024];
        replace(&path, &old).expect("initial publication");

        let done = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicU64::new(0));
        let exhausted_replacement_windows = Arc::new(AtomicU64::new(0));
        let reader = {
            let done = Arc::clone(&done);
            let reads = Arc::clone(&reads);
            let exhausted_replacement_windows = Arc::clone(&exhausted_replacement_windows);
            let path = path.clone();
            let old = old.clone();
            let new = new.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut failures = Vec::new();
                while !done.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(30) {
                    match read(&path) {
                        Ok(bytes) if bytes == old || bytes == new => {
                            reads.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(bytes) => failures.push(format!(
                            "observed {} bytes that were neither complete version",
                            bytes.len()
                        )),
                        Err(error) if is_sustained_windows_sharing_contention(&error) => {
                            // One read has a one-second retry budget. A stress
                            // writer can continuously begin new ReplaceFileW
                            // windows for longer than that; exhausting the
                            // bounded availability policy with error 32 is not
                            // torn data. Error 2 remains a failure because it
                            // could hide a genuine missing-file gap.
                            exhausted_replacement_windows.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => failures.push(format!("read failed: {error}")),
                    }
                }
                failures
            })
        };

        for index in 0..REPLACEMENTS {
            let contents = if index % 2 == 0 { &new } else { &old };
            replace(&path, contents).expect("replace");
            std::thread::yield_now();
        }
        done.store(true, Ordering::Release);

        let failures = reader.join().expect("reader thread");
        assert!(
            failures.is_empty(),
            "{} consistent reads observed partial bytes or an unexpected error; \
             first three: {:?}; documented Windows replacement contention \
             exhausted its retry budget {} times",
            failures.len(),
            &failures[..failures.len().min(3)],
            exhausted_replacement_windows.load(Ordering::Relaxed)
        );
        assert!(
            reads.load(Ordering::Relaxed) >= 20,
            "the reader did not overlap enough replacements to prove visibility"
        );
        assert!(
            directory
                .path()
                .read_dir()
                .expect("list directory")
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp.")),
            "successful replacement must consume every temporary file"
        );
    }

    fn is_sustained_windows_sharing_contention(error: &io::Error) -> bool {
        #[cfg(windows)]
        {
            error.raw_os_error() == Some(32)
        }
        #[cfg(not(windows))]
        {
            let _ = error;
            false
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_published_read_retries_only_windows_replacement_contention() {
        let mut attempts = 0;
        let value = read_published(|| {
            attempts += 1;
            match attempts {
                1 => Err(io::Error::from_raw_os_error(2)),
                2 => Err(io::Error::from_raw_os_error(32)),
                _ => Ok("complete"),
            }
        })
        .expect("the documented replacement errors must be retried");
        assert_eq!(value, "complete");
        assert_eq!(attempts, 3);

        let mut attempts = 0;
        let error = read_published(|| {
            attempts += 1;
            Err::<(), _>(io::Error::from_raw_os_error(5))
        })
        .expect_err("access denied is not a replacement window");
        assert_eq!(error.raw_os_error(), Some(5));
        assert_eq!(attempts, 1);
    }

    #[cfg(windows)]
    #[test]
    fn a_published_read_recovers_after_a_real_windows_sharing_window() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("projection.md");
        let contents = b"complete";
        replace(&path, contents).expect("initial publication");

        let exclusive = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("exclusive read handle");
        let mut exclusive = Some(exclusive);
        let mut observed_sharing_violation = false;

        let bytes = read_published(|| match fs::read(&path) {
            Err(error) if error.raw_os_error() == Some(32) => {
                observed_sharing_violation = true;
                drop(exclusive.take());
                Err(error)
            }
            result => result,
        })
        .expect("the read must recover after the exclusive handle closes");

        assert!(
            observed_sharing_violation,
            "the exclusive handle must produce a real Windows sharing violation"
        );
        assert_eq!(bytes, contents);
    }

    #[cfg(windows)]
    #[test]
    fn a_non_unicode_windows_path_fails_closed() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt as _;

        let path = std::path::PathBuf::from(OsString::from_wide(&[0xd800]));
        let error = unicode_path(&path).expect_err("unpaired surrogate is not UTF-8");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[cfg(test)]
mod symlink_tests {
    use super::*;

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[test]
    fn a_replacement_through_a_symlink_keeps_the_link_and_writes_its_target() {
        let root = tempfile::tempdir().expect("root");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let target = elsewhere.path().join("memory.md");
        fs::write(&target, b"old").expect("seed");
        let link = root.path().join("memory.md");
        symlink(&target, &link).expect("the user's deliberate redirection");

        replace(&link, b"new").expect("replace through the link");

        assert!(
            fs::symlink_metadata(&link).expect("metadata").is_symlink(),
            "publishing must not turn the link into a regular file"
        );
        assert_eq!(fs::read(&target).expect("target"), b"new");
        assert_eq!(read(&link).expect("read through the link"), b"new");
        assert_eq!(
            fs::read_dir(root.path()).expect("list").count(),
            1,
            "no temporary file may be left beside the link"
        );
    }

    #[test]
    fn a_relative_symlink_resolves_against_the_directory_holding_it() {
        let root = tempfile::tempdir().expect("root");
        let nested = root.path().join("data");
        fs::create_dir(&nested).expect("nested directory");
        let target = nested.join("real.md");
        fs::write(&target, b"old").expect("seed");
        let link = nested.join("link.md");
        symlink(Path::new("real.md"), &link).expect("a relative link");

        replace(&link, b"new").expect("replace through the relative link");

        assert!(fs::symlink_metadata(&link).expect("metadata").is_symlink());
        assert_eq!(fs::read(&target).expect("target"), b"new");
    }

    #[test]
    #[cfg(unix)]
    fn a_chain_of_symlinks_resolves_to_the_file_at_its_end() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("real.md");
        fs::write(&target, b"old").expect("seed");
        let middle = root.path().join("middle.md");
        symlink(&target, &middle).expect("first link");
        let outer = root.path().join("outer.md");
        symlink(&middle, &outer).expect("second link");

        replace(&outer, b"new").expect("replace through the chain");

        assert!(fs::symlink_metadata(&outer).expect("metadata").is_symlink());
        assert!(
            fs::symlink_metadata(&middle)
                .expect("metadata")
                .is_symlink()
        );
        assert_eq!(fs::read(&target).expect("target"), b"new");
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_loop_is_refused_rather_than_followed_forever() {
        let root = tempfile::tempdir().expect("root");
        let first = root.path().join("first.md");
        let second = root.path().join("second.md");
        symlink(&second, &first).expect("first half of the loop");
        symlink(&first, &second).expect("second half of the loop");

        let error = replace(&first, b"new").expect_err("a loop must be refused");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("loop"),
            "the message must name the problem: {error}"
        );
    }

    #[test]
    fn a_dangling_symlink_publishes_at_the_location_it_names() {
        let root = tempfile::tempdir().expect("root");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let target = elsewhere.path().join("absent.md");
        let link = root.path().join("link.md");
        symlink(&target, &link).expect("a dangling link");

        replace(&link, b"new").expect("replace through the dangling link");

        assert!(fs::symlink_metadata(&link).expect("metadata").is_symlink());
        assert_eq!(fs::read(&target).expect("target"), b"new");
    }
}
