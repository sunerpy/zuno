//! Handle-anchored path resolution for every file tool that touches the filesystem.
//!
//! # The defect this exists to close
//!
//! The file tools used to resolve a path, ask the user for permission, and then hand
//! the resolved *string* to `std::fs`. Between the authorization and the write, any
//! process that can write inside the workspace could replace an ancestor directory
//! with a symlink pointing anywhere. `std::fs::write` and `std::fs::create_dir_all`
//! both follow symlinks, so the bytes landed outside the workspace while the only
//! permission the user ever saw was a plain workspace `write`.
//!
//! Two repairs were considered and rejected before this one:
//!
//! * `O_NOFOLLOW` on the leaf alone does nothing, because the swapped object is an
//!   intermediate directory, not the final component.
//! * Re-canonicalizing after authorization is still a check followed by a separate
//!   use, and the window simply moves.
//!
//! # What this module guarantees
//!
//! Resolution descends from the *authorization boundary* — the workspace root, or the
//! external directory the user explicitly granted — one segment at a time, keeping an
//! open handle to each directory and refusing every symlink it meets. Leaf operations
//! are then performed **through the retained handle**, not by name from the
//! filesystem root, so no later change to any ancestor name can redirect them.
//!
//! The property this buys is exact: *the operation reaches the directory object the
//! tool resolved and the user authorized, or it fails.* Substituting a symlink for
//! any ancestor cannot redirect it. An attacker who instead renames the authorized
//! directory itself still only reaches the object the user approved, and an attacker
//! who deletes it gets a failure, because a deleted directory accepts no new entries.
//!
//! # Per-platform mechanism
//!
//! * **Linux** anchors through `/proc/self/fd/{fd}/{segment}` and opens each segment
//!   with `O_NOFOLLOW | O_DIRECTORY`. The kernel resolves the magic link to the
//!   directory the descriptor pins, so this is race-free without any FFI. When procfs
//!   is not mounted the portable backend below is used instead.
//! * **macOS** opens `root/relative` with `O_NOFOLLOW_ANY`, which makes the kernel
//!   reject a symlink in *any* component of the path in one atomic resolution.
//! * **Windows** walks segment by segment with `FILE_FLAG_OPEN_REPARSE_POINT` and
//!   refuses any component carrying `FILE_ATTRIBUTE_REPARSE_POINT`, which covers both
//!   symlinks and directory junctions, then re-verifies the anchor's
//!   `(volume_serial_number, file_index)` identity immediately before publishing.
//! * **Any other target** uses the portable backend: the same segment walk with a
//!   `symlink_metadata` refusal and the same pre-publish identity re-verification.
//!
//! Only Linux closes the window completely, because only Linux offers a safe-std way
//! to name a path relative to an open descriptor. See `docs`-facing notes in the
//! change that introduced this module for what the other platforms still require.
//!
//! # Why no `openat2`, `renameat` or `GetFinalPathNameByHandleW`
//!
//! The workspace forbids first-party `unsafe` (`[workspace.lints.rust] unsafe_code =
//! "forbid"` in the root `Cargo.toml`), so those syscalls are reachable only through a
//! wrapper crate. None of the three has one this workspace already builds, so each
//! would add a crate to the graph to narrow a window the segment walk already narrows.
//!
//! Windows file identity is the one exception, because safe `std` cannot express it on
//! stable at all: `MetadataExt::{volume_serial_number, file_index}` sit behind the
//! unstable `windows_by_handle` feature. `winapi-util` returns the same two fields from
//! `GetFileInformationByHandle` through a safe wrapper, and the workspace already
//! compiles that crate for Windows under `ignore`, `walkdir`, and `termcolor`, so the
//! edge costs no new dependency. Everything else here is plain `std`.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};

/// Upper bound on the number of path segments below the authorization boundary.
///
/// A legitimate workspace path is nowhere near this deep. The bound keeps a
/// pathological argument from turning into an unbounded number of directory opens.
const MAX_SEGMENTS: usize = 128;

/// Outcome of a publication that replaces a file's contents.
#[derive(Debug)]
pub(crate) enum PublishFailure {
    /// The destination was not touched; it still holds exactly its previous content.
    ///
    /// Nothing landed, so the caller may report a plain failure.
    NotApplied(io::Error),
    /// The replacement may or may not have taken effect.
    ///
    /// The rename that makes new content visible either failed after partially
    /// completing or lost its result. The caller must report this as an uncertain
    /// outcome so the destination is inspected rather than written again.
    Uncertain(io::Error),
}

impl PublishFailure {
    /// The underlying I/O error, for the caller's typed `ToolError`.
    pub(crate) fn into_error(self) -> io::Error {
        match self {
            Self::NotApplied(error) | Self::Uncertain(error) => error,
        }
    }

    /// Whether the destination's content is now unknown.
    pub(crate) fn is_uncertain(&self) -> bool {
        matches!(self, Self::Uncertain(_))
    }

    /// The kind of the underlying I/O error, for callers that classify before reporting.
    pub(crate) fn error_kind(&self) -> io::ErrorKind {
        match self {
            Self::NotApplied(error) | Self::Uncertain(error) => error.kind(),
        }
    }
}

/// Stable identity of a directory, used to detect a substituted ancestor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume: u64,
    #[cfg(windows)]
    index: u64,
    #[cfg(not(any(unix, windows)))]
    unavailable: (),
}

/// Read the identity through the open handle rather than by name.
///
/// The handle is the point: asking the filesystem about the path again would defeat
/// the anchor, because the name is the part an attacker controls.
#[cfg(unix)]
fn identity_of(handle: &File) -> io::Result<Option<DirIdentity>> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = handle.metadata()?;
    Ok(Some(DirIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

/// `std::os::windows::fs::MetadataExt::{volume_serial_number, file_index}` are still
/// unstable (`windows_by_handle`, rust-lang/rust#63010), so the identity is read from
/// `GetFileInformationByHandle` through `winapi-util`'s safe wrapper. That keeps the
/// FFI outside first-party code, so the workspace `unsafe_code = "forbid"` guarantee
/// still holds, and it reads the same two fields the unstable API would return.
#[cfg(windows)]
fn identity_of(handle: &File) -> io::Result<Option<DirIdentity>> {
    let information = winapi_util::file::information(handle)?;
    Ok(Some(DirIdentity {
        volume: information.volume_serial_number(),
        index: information.file_index(),
    }))
}

#[cfg(not(any(unix, windows)))]
fn identity_of(_handle: &File) -> io::Result<Option<DirIdentity>> {
    Ok(None)
}

/// A directory pinned by an open handle, together with the boundary it descends from.
#[derive(Debug)]
pub(crate) struct AnchoredDir {
    handle: File,
    root: PathBuf,
    relative: PathBuf,
    identity: Option<DirIdentity>,
}

impl AnchoredDir {
    /// Descend `relative` beneath `root`, refusing to follow any symlink on the way.
    ///
    /// `root` must already be canonical: it is the boundary the user authorized, and
    /// it is trusted. `create` makes missing intermediate directories, which is what
    /// the `write` and `apply_patch` tools need; each freshly created directory is
    /// then opened under the same no-symlink rule, so a directory swapped in between
    /// the create and the open is still refused.
    pub(crate) fn descend(root: &Path, relative: &Path, create: bool) -> io::Result<Self> {
        let segments = segments_of(relative)?;
        let mut current = Self {
            handle: open_root(root)?,
            root: root.to_owned(),
            relative: PathBuf::new(),
            identity: None,
        };
        current.identity = identity_of(&current.handle)?;
        for segment in segments {
            current = current.child(&segment, create)?;
        }
        Ok(current)
    }

    /// Open one directory segment beneath this anchor.
    fn child(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        let mut relative = self.relative.clone();
        relative.push(name);
        let handle = match open_child_dir(self, name) {
            Ok(handle) => handle,
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(self.leaf_path(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                open_child_dir(self, name).map_err(|error| refused_segment(self, name, error))?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(error),
            Err(error) => return Err(refused_segment(self, name, error)),
        };
        let identity = identity_of(&handle)?;
        Ok(Self {
            handle,
            root: self.root.clone(),
            relative,
            identity,
        })
    }

    /// The path a leaf operation must use so that it is anchored at this directory.
    ///
    /// On Linux this is a procfs path that the kernel resolves through the pinned
    /// descriptor. Elsewhere it is the real path, because safe `std` offers no way to
    /// name a file relative to an open handle.
    fn leaf_path(&self, name: &OsStr) -> PathBuf {
        anchored_leaf(self, name)
    }

    /// The real path of this anchor, for diagnostics and permission metadata.
    pub(crate) fn path(&self) -> PathBuf {
        if self.relative.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(&self.relative)
        }
    }

    /// Confirm the anchor still is the object it was when it was opened.
    ///
    /// On Linux the anchor is authoritative by construction, so this additionally
    /// asserts that the pinned directory is *still located* beneath the authorization
    /// boundary: an attacker who renames the authorized directory out of the
    /// workspace must not silently keep the grant.
    pub(crate) fn revalidate(&self) -> io::Result<()> {
        revalidate_anchor(self)
    }

    /// Read this directory's entries through the anchor.
    ///
    /// The directory flag comes from the entry's own file type, so a symlink to a
    /// directory is reported as the link it is rather than as a directory.
    pub(crate) fn read_dir(&self) -> io::Result<Vec<(OsString, bool)>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(self.leaf_path(OsStr::new(".")))? {
            let entry = entry?;
            let is_directory = entry
                .file_type()
                .map(|kind| kind.is_dir())
                .or_else(|_| entry.metadata().map(|metadata| metadata.is_dir()))
                .unwrap_or(false);
            entries.push((entry.file_name(), is_directory));
        }
        Ok(entries)
    }
}

/// A file named relative to a pinned directory.
#[derive(Debug)]
pub(crate) struct AnchoredFile {
    dir: AnchoredDir,
    name: OsString,
}

impl AnchoredFile {
    /// Descend to `target`'s parent beneath `root` and bind `target`'s file name.
    ///
    /// `target` must be an absolute path beneath `root`.
    pub(crate) fn open(root: &Path, target: &Path, create_parents: bool) -> io::Result<Self> {
        let relative = target.strip_prefix(root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not inside the authorized directory {}",
                    target.display(),
                    root.display()
                ),
            )
        })?;
        let name = relative.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no file name", target.display()),
            )
        })?;
        let parent = relative.parent().unwrap_or(Path::new(""));
        let dir = AnchoredDir::descend(root, parent, create_parents)?;
        Ok(Self {
            dir,
            name: name.to_owned(),
        })
    }

    /// The real path this file resolves to, for diagnostics and formatter calls.
    pub(crate) fn path(&self) -> PathBuf {
        self.dir.path().join(&self.name)
    }

    /// Metadata for the leaf itself, never for a symlink's destination.
    pub(crate) fn symlink_metadata(&self) -> io::Result<Metadata> {
        fs::symlink_metadata(self.leaf())
    }

    /// Read the file, refusing a leaf that has become a symlink.
    ///
    /// The tools resolve a symlinked leaf to its destination *before* authorization,
    /// so a leaf that is a symlink here appeared after the user approved a specific
    /// destination. Refusing it is the point.
    pub(crate) fn read(&self) -> io::Result<Vec<u8>> {
        self.reject_symlink_leaf()?;
        let mut file = open_leaf_read(&self.leaf())?;
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut file, &mut bytes)?;
        Ok(bytes)
    }

    /// Create the file and write `bytes`, failing if it already exists.
    ///
    /// Creation and the existence test are a single operation, which is what
    /// `apply_patch`'s add case relies on. A failure part-way through the write
    /// removes the partial file so no torn content survives; if that cleanup also
    /// fails the outcome is uncertain.
    pub(crate) fn create_new(&self, bytes: &[u8]) -> Result<(), PublishFailure> {
        let path = self.leaf();
        let mut file = open_leaf_create_new(&path).map_err(PublishFailure::NotApplied)?;
        match file.write_all(bytes).and_then(|()| file.flush()) {
            Ok(()) => Ok(()),
            Err(error) => {
                drop(file);
                match fs::remove_file(&path) {
                    Ok(()) => Err(PublishFailure::NotApplied(error)),
                    Err(_) => Err(PublishFailure::Uncertain(error)),
                }
            }
        }
    }

    /// Replace the file's contents so a concurrent reader never observes a torn write.
    ///
    /// The bytes are written to a sibling temporary file that is then renamed over the
    /// destination. Every step is anchored, and the anchor's identity is re-verified
    /// immediately before the rename on the platforms where the anchor cannot name a
    /// path by descriptor.
    pub(crate) fn publish(&self, bytes: &[u8]) -> Result<(), PublishFailure> {
        self.reject_symlink_leaf()
            .map_err(PublishFailure::NotApplied)?;
        let temporary_name = temporary_name(&self.name);
        let temporary = self.dir.leaf_path(&temporary_name);
        let destination = self.leaf();

        let mut file = open_leaf_create_new(&temporary).map_err(PublishFailure::NotApplied)?;
        if let Err(error) = file.write_all(bytes).and_then(|()| file.flush()) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(PublishFailure::NotApplied(error));
        }
        drop(file);

        if let Err(error) = self.dir.revalidate() {
            let _ = fs::remove_file(&temporary);
            return Err(PublishFailure::NotApplied(error));
        }

        match rename_anchored(&temporary, &destination) {
            Ok(()) => Ok(()),
            Err(error) => {
                // The temporary file is still the only copy of the new bytes, and the
                // destination still holds its previous content, unless the rename got
                // far enough to unlink it. Removing the temporary succeeds only when
                // it is still where it was put, which is the case in which nothing was
                // published.
                match fs::remove_file(&temporary) {
                    Ok(()) => Err(PublishFailure::NotApplied(error)),
                    Err(_) => Err(PublishFailure::Uncertain(error)),
                }
            }
        }
    }

    /// Remove the leaf, unlinking the name rather than a symlink's destination.
    pub(crate) fn remove(&self) -> io::Result<()> {
        fs::remove_file(self.leaf())
    }

    /// Refuse a leaf that is a symlink, so a publication never destroys a link.
    ///
    /// The tools resolve a symlinked leaf to its destination *before* they ask for
    /// permission, so `resolve` owns the decision to follow a link exactly once. A
    /// link observed here therefore appeared after the user approved a specific
    /// destination, and neither following it nor overwriting it is what the user
    /// agreed to.
    fn reject_symlink_leaf(&self) -> io::Result<()> {
        match self.symlink_metadata() {
            Ok(metadata) if metadata.is_symlink() => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is a symlink; a file tool resolves a link before asking for \
                     permission and will not write through one it did not authorize",
                    self.path().display()
                ),
            )),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn leaf(&self) -> PathBuf {
        self.dir.leaf_path(&self.name)
    }
}

/// Split a relative path into plain segments, rejecting anything else.
///
/// `..`, a root, and a drive prefix are all rejected: the caller resolved and
/// normalized the path already, so their presence here means the path never was
/// beneath the boundary.
fn segments_of(relative: &Path) -> io::Result<Vec<OsString>> {
    let mut segments = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => segments.push(part.to_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is not a plain relative path beneath the authorized directory",
                        relative.display()
                    ),
                ));
            }
        }
    }
    if segments.len() > MAX_SEGMENTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} nests deeper than the {MAX_SEGMENTS} directories a file tool will open",
                relative.display()
            ),
        ));
    }
    Ok(segments)
}

/// Name for the sibling temporary file a publication renames into place.
fn temporary_name(name: &OsStr) -> OsString {
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".zuno-{}.tmp", uuid::Uuid::new_v4().simple()));
    temporary
}

// --- Linux ------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod platform {
    use super::AnchoredDir;
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    /// `O_DIRECTORY` on every Linux architecture Zuno supports (asm-generic).
    const O_DIRECTORY: i32 = 0o200_000;
    /// `O_NOFOLLOW` on every Linux architecture Zuno supports (asm-generic).
    const O_NOFOLLOW: i32 = 0o400_000;

    /// Whether `/proc/self/fd` can be used to name a path relative to a descriptor.
    ///
    /// Probed once, by actually resolving a directory through the magic link rather
    /// than by testing for the mount point, because a container can present
    /// `/proc/self/fd` while restricting what resolves through it.
    fn procfs_anchoring() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let Ok(root) = File::open("/") else {
                return false;
            };
            let probe = PathBuf::from(format!("/proc/self/fd/{}", root.as_raw_fd())).join(".");
            std::fs::metadata(probe).is_ok_and(|meta| meta.is_dir())
        })
    }

    fn descriptor_path(dir: &AnchoredDir) -> Option<PathBuf> {
        procfs_anchoring()
            .then(|| PathBuf::from(format!("/proc/self/fd/{}", dir.handle.as_raw_fd())))
    }

    pub(super) fn open_root(root: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(root)
    }

    pub(super) fn open_child_dir(parent: &AnchoredDir, name: &OsStr) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(anchored_leaf(parent, name))
    }

    pub(super) fn anchored_leaf(dir: &AnchoredDir, name: &OsStr) -> PathBuf {
        match descriptor_path(dir) {
            Some(base) => base.join(name),
            None => dir.path().join(name),
        }
    }

    pub(super) fn open_leaf_read(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(path)
    }

    pub(super) fn open_leaf_create_new(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(O_NOFOLLOW)
            .open(path)
    }

    pub(super) fn rename_anchored(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    /// Confirm the pinned directory is still located beneath its boundary.
    ///
    /// The descriptor guarantees the *object* cannot be substituted, so the only
    /// remaining question is whether that object is still where the user authorized
    /// it. `readlink` on the magic link answers that from the kernel's own view.
    pub(super) fn revalidate_anchor(dir: &AnchoredDir) -> io::Result<()> {
        let Some(base) = descriptor_path(dir) else {
            return super::portable_revalidate(dir);
        };
        let real = std::fs::read_link(base)?;
        if real.starts_with(&dir.root) {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the authorized directory {} moved to {} during the operation",
                dir.path().display(),
                real.display()
            ),
        ))
    }
}

// --- macOS ------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use super::AnchoredDir;
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};

    /// `O_DIRECTORY` on Darwin.
    const O_DIRECTORY: i32 = 0x0010_0000;
    /// `O_NOFOLLOW` on Darwin.
    const O_NOFOLLOW: i32 = 0x0000_0100;
    /// `O_NOFOLLOW_ANY` on Darwin: reject a symlink in *any* component of the path.
    const O_NOFOLLOW_ANY: i32 = 0x2000_0000;

    pub(super) fn open_root(root: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW_ANY)
            .open(root)
    }

    pub(super) fn open_child_dir(parent: &AnchoredDir, name: &OsStr) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW_ANY)
            .open(anchored_leaf(parent, name))
    }

    /// Darwin has no procfs, so a leaf is named by its real path.
    ///
    /// `O_NOFOLLOW_ANY` on every open below still makes the kernel re-reject a
    /// symlinked ancestor atomically, so a leaf can never be *created* outside.
    pub(super) fn anchored_leaf(dir: &AnchoredDir, name: &OsStr) -> PathBuf {
        dir.path().join(name)
    }

    pub(super) fn open_leaf_read(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NOFOLLOW_ANY)
            .open(path)
    }

    pub(super) fn open_leaf_create_new(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(O_NOFOLLOW | O_NOFOLLOW_ANY)
            .open(path)
    }

    /// `rename` cannot carry `O_NOFOLLOW_ANY`, so this is the one unprotected step.
    ///
    /// The anchor is re-verified immediately before the call, which narrows the window
    /// to the rename itself. Closing it needs `renameat` against the pinned
    /// descriptor, which needs a wrapper crate this crate may not depend on.
    pub(super) fn rename_anchored(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    pub(super) fn revalidate_anchor(dir: &AnchoredDir) -> io::Result<()> {
        let reopened = open_root(&dir.path())?;
        let identity = super::identity_of(&reopened)?;
        if identity.is_some() && identity == dir.identity {
            return Ok(());
        }
        Err(super::substituted(dir))
    }
}

// --- Windows ----------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use super::AnchoredDir;
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::{Path, PathBuf};

    /// Required to obtain a handle to a directory rather than a file.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// Open the reparse point itself instead of following it.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    /// Set on symlinks, directory junctions, and mount points.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    /// `ERROR_SHARING_VIOLATION`: the destination is open in another process.
    const ERROR_SHARING_VIOLATION: i32 = 32;
    /// `ERROR_ACCESS_DENIED`, which a replace can also raise transiently.
    const ERROR_ACCESS_DENIED: i32 = 5;
    /// Attempts a publication makes before reporting a locked destination.
    const RENAME_ATTEMPTS: u32 = 5;

    fn open_directory(path: &Path) -> io::Result<File> {
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = handle.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is a symlink, junction, or mount point, which a file tool will not \
                     follow to reach an authorized directory",
                    path.display()
                ),
            ));
        }
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", path.display()),
            ));
        }
        Ok(handle)
    }

    pub(super) fn open_root(root: &Path) -> io::Result<File> {
        open_directory(root)
    }

    pub(super) fn open_child_dir(parent: &AnchoredDir, name: &OsStr) -> io::Result<File> {
        open_directory(&anchored_leaf(parent, name))
    }

    /// Windows has no way to name a path relative to a handle in safe `std`.
    pub(super) fn anchored_leaf(dir: &AnchoredDir, name: &OsStr) -> PathBuf {
        dir.path().join(name)
    }

    fn reject_reparse_point(path: &Path, handle: &File) -> io::Result<()> {
        if handle.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is a symlink or junction, which a file tool will not follow",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn open_leaf_read(path: &Path) -> io::Result<File> {
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        reject_reparse_point(path, &handle)?;
        Ok(handle)
    }

    pub(super) fn open_leaf_create_new(path: &Path) -> io::Result<File> {
        let handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        reject_reparse_point(path, &handle)?;
        Ok(handle)
    }

    /// Replace the destination, retrying the transient sharing failures Windows raises.
    ///
    /// `std::fs::rename` maps to `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`, which
    /// keeps content replacement atomic but fails outright when another process holds
    /// the destination open without `FILE_SHARE_DELETE`. That is a real condition to
    /// report rather than something to paper over with a truncating write, so it is
    /// retried briefly and then surfaced.
    pub(super) fn rename_anchored(from: &Path, to: &Path) -> io::Result<()> {
        let mut attempt = 0;
        loop {
            match std::fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let transient = matches!(
                        error.raw_os_error(),
                        Some(ERROR_SHARING_VIOLATION) | Some(ERROR_ACCESS_DENIED)
                    );
                    attempt += 1;
                    if !transient || attempt >= RENAME_ATTEMPTS {
                        return Err(error);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10 * u64::from(attempt)));
                }
            }
        }
    }

    pub(super) fn revalidate_anchor(dir: &AnchoredDir) -> io::Result<()> {
        super::portable_revalidate(dir)
    }
}

// --- Any other target -------------------------------------------------------

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::AnchoredDir;
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::{Path, PathBuf};

    fn reject_symlink(path: &Path) -> io::Result<()> {
        if std::fs::symlink_metadata(path)?.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is a symlink, which a file tool will not follow",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn open_root(root: &Path) -> io::Result<File> {
        reject_symlink(root)?;
        File::open(root)
    }

    pub(super) fn open_child_dir(parent: &AnchoredDir, name: &OsStr) -> io::Result<File> {
        let path = anchored_leaf(parent, name);
        reject_symlink(&path)?;
        File::open(path)
    }

    pub(super) fn anchored_leaf(dir: &AnchoredDir, name: &OsStr) -> PathBuf {
        dir.path().join(name)
    }

    pub(super) fn open_leaf_read(path: &Path) -> io::Result<File> {
        reject_symlink(path)?;
        File::open(path)
    }

    pub(super) fn open_leaf_create_new(path: &Path) -> io::Result<File> {
        OpenOptions::new().write(true).create_new(true).open(path)
    }

    pub(super) fn rename_anchored(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    pub(super) fn revalidate_anchor(dir: &AnchoredDir) -> io::Result<()> {
        super::portable_revalidate(dir)
    }
}

use platform::{
    anchored_leaf, open_child_dir, open_leaf_create_new, open_leaf_read, open_root,
    rename_anchored, revalidate_anchor,
};

/// The refusal for a path segment that is not a plain directory inside the boundary.
///
/// `ErrorKind::PermissionDenied` is the strongest kind stable `std` offers here:
/// `FilesystemLoop` is still unstable, so the kernel's `ELOOP` cannot be named. The
/// message carries the detail, and the tools turn this into `ToolError::InvalidArgs`
/// because the path the model asked for is not the path the user authorized.
fn refused_segment(parent: &AnchoredDir, name: &OsStr, source: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} is not a plain directory inside the authorized tree {} — a symlink, junction, \
             or mount point here is refused rather than followed ({source})",
            parent.path().join(name).display(),
            parent.root.display()
        ),
    )
}

/// The error a substituted ancestor produces.
fn substituted(dir: &AnchoredDir) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "the authorized directory {} was replaced during the operation",
            dir.path().display()
        ),
    )
}

/// Re-walk the boundary and require the anchor to still be the same object.
///
/// Used wherever the platform cannot name a path relative to an open handle. It
/// detects a substitution rather than preventing one, which is why the Linux backend
/// does not need it.
fn portable_revalidate(dir: &AnchoredDir) -> io::Result<()> {
    let reopened = AnchoredDir::descend(&dir.root, &dir.relative, false)?;
    if reopened.identity.is_some() && reopened.identity == dir.identity {
        return Ok(());
    }
    Err(substituted(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[test]
    fn a_publication_replaces_content_without_a_torn_intermediate_state() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("notes.txt");
        fs::write(&path, b"old").expect("seed");
        let file = AnchoredFile::open(root.path(), &path, false).expect("anchor");

        file.publish(b"new content").expect("publish");

        assert_eq!(fs::read(&path).expect("read"), b"new content");
        let leftovers: Vec<_> = fs::read_dir(root.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "the temporary file must not survive");
    }

    #[test]
    fn a_symlinked_ancestor_is_refused_rather_than_followed() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), &root.path().join("docs")).expect("plant the link");

        let error = AnchoredFile::open(root.path(), &root.path().join("docs/escape.txt"), true)
            .expect_err("a symlinked ancestor must be refused");

        assert!(
            !outside.path().join("escape.txt").exists(),
            "nothing may be created through the link"
        );
        assert_eq!(
            error.kind(),
            io::ErrorKind::PermissionDenied,
            "unexpected refusal: {error:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_leaf_that_became_a_symlink_is_refused_rather_than_written_through() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let destination = outside.path().join("secret.txt");
        fs::write(&destination, b"original").expect("seed");
        let link = root.path().join("secret.txt");
        std::os::unix::fs::symlink(&destination, &link).expect("plant the link");

        let file = AnchoredFile::open(root.path(), &link, false).expect("anchor");
        let error = file.read().expect_err("a symlinked leaf must be refused");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&destination).expect("read"), b"original");
    }

    #[test]
    #[cfg(unix)]
    fn a_publication_over_a_symlinked_leaf_is_refused_and_spares_both_link_and_target() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let destination = outside.path().join("secret.txt");
        fs::write(&destination, b"original").expect("seed");
        let link = root.path().join("secret.txt");
        std::os::unix::fs::symlink(&destination, &link).expect("plant the link");

        let file = AnchoredFile::open(root.path(), &link, false).expect("anchor");
        let failure = file
            .publish(b"replacement")
            .expect_err("a link planted after authorization must be refused");

        assert!(!failure.is_uncertain(), "nothing was published");
        assert_eq!(failure.into_error().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            fs::read(&destination).expect("read"),
            b"original",
            "the link's destination must not be redirected into"
        );
        assert!(
            fs::symlink_metadata(&link).expect("metadata").is_symlink(),
            "the link itself must survive"
        );
    }

    #[test]
    fn missing_intermediate_directories_are_created_only_when_asked() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("a/b/c.txt");

        let error = AnchoredFile::open(root.path(), &target, false)
            .expect_err("a missing parent must not be created implicitly");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        let file = AnchoredFile::open(root.path(), &target, true).expect("anchor");
        file.publish(b"deep").expect("publish");
        assert_eq!(fs::read(&target).expect("read"), b"deep");
    }

    #[test]
    fn a_path_outside_the_boundary_is_refused_before_any_open() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");

        let error = AnchoredFile::open(root.path(), &outside.path().join("x.txt"), true)
            .expect_err("a path outside the boundary must be refused");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_parent_traversal_segment_is_refused() {
        let root = tempfile::tempdir().expect("root");
        let error = segments_of(Path::new("a/../../b")).expect_err("`..` must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(AnchoredDir::descend(root.path(), Path::new(""), false).is_ok());
    }

    #[test]
    fn create_new_reports_an_existing_file_without_replacing_it() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("once.txt");
        fs::write(&path, b"first").expect("seed");
        let file = AnchoredFile::open(root.path(), &path, false).expect("anchor");

        let failure = file.create_new(b"second").expect_err("must not replace");

        assert!(!failure.is_uncertain());
        assert_eq!(failure.into_error().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).expect("read"), b"first");
    }

    #[test]
    #[cfg(unix)]
    fn removing_a_leaf_unlinks_the_name_and_not_a_link_destination() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let destination = outside.path().join("keep.txt");
        fs::write(&destination, b"keep").expect("seed");
        let link = root.path().join("link.txt");
        std::os::unix::fs::symlink(&destination, &link).expect("plant the link");

        let file = AnchoredFile::open(root.path(), &link, false).expect("anchor");
        file.remove().expect("remove");

        assert!(!link.exists());
        assert!(destination.exists(), "the link's destination must survive");
    }
}
