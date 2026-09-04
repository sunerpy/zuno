//! The file primitives both credential stores share: a write that is never
//! world-readable and never half-published, and a read that says so when the file
//! already is one of those things.
//!
//! # The mode is part of the create, not a follow-up
//!
//! The oracle writes the file and then chmods it — `fs-util.ts:110-113`:
//!
//! ```text
//! const content = JSON.stringify(data, null, 2)
//! yield* fs.writeFileString(path, content)
//! if (mode) yield* fs.chmod(path, mode)
//! ```
//!
//! Between those two lines the file exists at the process umask, typically
//! `0644`, with the user's refresh tokens already in it. The window is short and
//! it is still a window: any process on the box that opens the path in it keeps a
//! readable descriptor for as long as it likes, and `chmod` afterwards does not
//! revoke an open descriptor.
//!
//! [`write_json`] closes it by passing the mode to `open(2)` through
//! [`OpenOptionsExt::mode`], so the file is `0600` from the instant it exists.
//! It *then* also calls `set_permissions` on the same handle, because `open`'s
//! mode argument is masked by the process umask and the bits have to be exactly
//! `0600` whatever that umask was. A destination that was already permissive is
//! replaced by that private file rather than edited in place, which achieves the
//! repair the oracle performs — observed against the 1.18.12 binary, which turned
//! a `0644` `auth.json` into `0600` on `opencode auth logout`.
//!
//! # The store is published, never truncated
//!
//! [`write_json`] writes a whole document to a sibling of the destination and only
//! then gives it the destination's name. Truncating the destination first would put
//! every credential the user owns inside the window between the truncate and the
//! write: a process killed there, or a machine that loses power, leaves a zero-byte
//! `auth.json` behind.
//!
//! The name transition is `zuno-atomic-file`'s, not this module's, wherever that
//! crate can be used as it stands. Rust's `fs::rename` publishes over an existing
//! Windows destination through `MoveFileEx`, whose replacement is **not** gap-free:
//! a concurrent reader's `open` can transiently fail with `ERROR_FILE_NOT_FOUND`
//! for a file that is present and complete. A reader that decodes that as "no
//! credentials" hands the next write an empty store to ratify, which is the
//! truncate window again wearing absence instead of zero bytes.
//! `zuno_atomic_file::replace` publishes through `ReplaceFileW`, so [`publish`]
//! delegates to it on every platform that is not Unix.
//!
//! Unix keeps a local publication for one reason, the mode: `replace` opens its
//! temporary file with default options, so the sibling would hold the user's
//! refresh tokens at the process umask — typically `0644` — for as long as the
//! write takes, which is a *wider* version of the window the section above closes.
//! Until that crate grows a mode-aware entry point the Unix branch creates the
//! sibling with the mode already applied. Windows has no Unix mode bits, and the
//! file it publishes inherits the containing directory's ACL exactly as the
//! previous `create_new` did, so nothing is given up by delegating there.
//!
//! Two consequences of publishing a *new* file, on every platform:
//!
//! - The write needs permission to create a file in the containing directory, not
//!   only to write the credential file. A hardened layout that made the data
//!   directory read-only and relied on an already-existing `auth.json` staying
//!   writable can no longer refresh a token. On Unix that refusal is
//!   [`AuthError::Directory`], which names the directory as well as the file, so an
//!   operator is not sent to `chmod` the credential file. Off Unix the delegated
//!   publication reports one `io::Error` for the whole operation and it surfaces as
//!   [`AuthError::Write`].
//! - The published file is a new inode, so the previous file's ownership, POSIX
//!   ACLs, SELinux label and extended attributes do not survive it, and a hard link
//!   somebody made to the credential file stops tracking it — that link keeps the
//!   old contents at the old inode, so a backup made with `ln` silently stops
//!   following the store. A symlink *at* the credential path does survive and still
//!   points where the user aimed it — see [`resolve_destination`] — but a group ACL
//!   somebody set on the file the link names is replaced by
//!   [`CREDENTIAL_FILE_MODE`].
//!
//! # The power-loss boundary is Unix's, and best effort elsewhere
//!
//! On Unix [`publish`] syncs the sibling before the rename and the containing
//! directory after it, so a login that reported success survives a power loss. Off
//! Unix the publication is `zuno_atomic_file::replace`, whose own module docs say it
//! deliberately does not promise crash durability and that a caller needing that
//! boundary must sync the temporary file and the directory itself — and that
//! temporary file is private to that crate, so this module cannot reach it without
//! reimplementing the publication that produced the Windows gap. What it does
//! instead is flush the *published* file afterwards, [`flush_published`], which
//! narrows the exposure to the interval between the name transition and that flush
//! rather than leaving the whole document unflushed behind a published name. A power
//! loss inside that interval can still publish a name over data that never reached
//! the device — the zero-byte or torn `auth.json` the section above exists to
//! prevent. Closing it needs a durable publication in `zuno-atomic-file`; until that
//! exists this is a stated non-Unix limitation rather than a guarantee, and it is
//! owed native Windows evidence.
//!
//! # A store does not delete what it cannot read
//!
//! A read-modify-write decodes the file into typed entries, and anything that did not
//! decode would vanish when that typed map is written back. [`rewrite`] carries those
//! values through the write untouched, so one login cannot delete a credential whose
//! shape a newer Zuno wrote, or an authentication variant this build does not model.
//!
//! Whole entries are not the only thing a newer Zuno adds. Both credential entry
//! types are all-`Option` with `#[serde(default)]` and no `deny_unknown_fields`, so
//! an added *field* decodes successfully into nothing and the typed map written back
//! would drop it — including from entries the write never touched.
//! [`unmodelled_fields`] keeps those keys per entry and [`rewrite`] puts them back
//! around the typed ones. A key the type models is never kept that way, so clearing
//! a field really clears it, and changing a credential's `type` does not leave the
//! previous shape's secrets on disk.
//!
//! # Damage is reported, and an empty file is not a lockout
//!
//! A zero-byte credential file is damage: nothing here writes one. It is not damage
//! any caller may be stopped by, on either kind of read.
//!
//! It reaches every surface. `zuno auth list`, `zuno models` and a run whose model
//! credential comes from the environment read it, and `zuno auth login`,
//! `zuno auth logout` and every token refresh read it *and write it back*. The
//! zero-byte file is what the shipped 0.6.6 truncate window left behind, so the
//! population that has one is exactly the population that needs to log in again —
//! and a read-modify-write that refuses it denies the login that repairs it while
//! leaving the file as broken as it found it.
//!
//! Refusing also buys nothing. A file holding no bytes, or nothing but whitespace,
//! holds no credential to preserve; there is no entry a write could destroy and no
//! value a recovery tool could lift out of it. So both reads treat it as a store
//! nobody has written: `T::default()`, with a [`StoreDamage`] report travelling with
//! the value and one `tracing::error!` per file per process that names the file and
//! says what happened. The next write republishes a real store over it.
//!
//! What is still refused is a file with *content* this build cannot parse:
//! [`AuthError::Malformed`] on both reads, because those bytes may be a recoverable
//! store and a write would be the thing that ends it. So is a file that is there and
//! could not be opened, [`AuthError::Unresolved`], which is what a publication in
//! flight looks like — neither attempts a write.
//!
//! Confirming that a file is *absent* is the one part of this that a platform can
//! refuse to answer. See [`presence_of`]: on Windows `ERROR_FILE_NOT_FOUND` is both
//! "no store was ever written" and "`ReplaceFileW` is publishing right now", so
//! absence there is a bounded conclusion rather than a fact, bounded by
//! [`ABSENCE_CONFIRMATION`].
//!
//! # A permissive file warns; it does not refuse
//!
//! Verified against the 1.18.12 binary: with `auth.json` at `0644`,
//! `opencode auth list` read it and printed all three credentials, and left the
//! mode alone. There is no permission check anywhere in `auth/index.ts` or
//! `mcp/auth.ts`.
//!
//! So refusing would be a parity break in the worst possible direction — a user
//! whose file is `0644` because it was restored from a backup or written by an
//! older tool would be locked out of every model they have configured, by the
//! crate whose job is to let them in. [`read_json`] reads the file and reports a
//! [`PermissionWarning`] instead, both as returned data and as a `tracing` event
//! that names the path.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::AuthError;

/// The mode every credential file is written with: owner read/write only.
pub const CREDENTIAL_FILE_MODE: u32 = 0o600;

/// The permission bits that must be clear for a credential file to be private —
/// every group and other bit.
pub const FORBIDDEN_BITS: u32 = 0o077;

/// A credential file on disk is readable by more than its owner.
///
/// Carries the path and the offending mode as data so a caller decides what to
/// do from fields rather than by matching on a rendered string.
#[derive(Clone, PartialEq, Eq)]
pub struct PermissionWarning {
    /// The file whose mode is too permissive.
    pub path: PathBuf,
    /// The permission bits found, masked to `0o777`.
    pub mode: u32,
}

impl PermissionWarning {
    /// The group and other bits that should not have been set.
    #[must_use]
    pub fn offending_bits(&self) -> u32 {
        self.mode & FORBIDDEN_BITS
    }
}

/// Renders `mode` in octal. A derived `Debug` prints `0o644` as `420`, which is
/// the one number an operator reading a dump cannot act on.
impl std::fmt::Debug for PermissionWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PermissionWarning")
            .field("path", &self.path)
            .field("mode", &format_args!("{:04o}", self.mode))
            .finish()
    }
}

impl std::fmt::Display for PermissionWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "credential file {} is mode {:04o}; it should be {:04o} so only you can read it",
            self.path.display(),
            self.mode,
            CREDENTIAL_FILE_MODE
        )
    }
}

/// Damage in a credential file that every read survives.
///
/// Travels with the read as data rather than only reaching a log sink, so a client
/// surface can tell the user their store was emptied instead of that they never had
/// credentials, and so a test can assert on it. It is deliberately *not* convertible
/// into an [`AuthError`]: a file holding no bytes holds nothing a write could
/// destroy, and turning the report into a failure is what denied `zuno auth login`
/// to the users whose file the 0.6.6 truncate window emptied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreDamage {
    /// The file exists and holds no bytes, or nothing but whitespace.
    Empty {
        /// The file that was found empty.
        path: PathBuf,
    },
}

impl StoreDamage {
    /// The file this report concerns.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Empty { path } => path,
        }
    }

    /// The kind of damage, as a stable label. Half of the key the once-per-file
    /// report latch uses, so a second variant cannot silently spend the first one's
    /// report.
    #[must_use]
    fn kind(&self) -> &'static str {
        match self {
            Self::Empty { .. } => "empty",
        }
    }
}

/// One wording, whether the finding is read from the returned data or from the log.
///
/// It says what happened, what was lost, and that logging in again is enough —
/// because it is: the next write publishes a whole store over the empty file.
impl std::fmt::Display for StoreDamage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty { path } => write!(
                formatter,
                "credential file {} holds no store; an interrupted write or a truncation from \
                 outside emptied it. Nothing readable is left in it, so restore a backup if the \
                 credentials it held matter — otherwise log in again and the next write replaces \
                 it",
                path.display()
            ),
        }
    }
}

/// What a read of a credential file produced.
///
/// The permission finding and the damage report travel with the value instead of
/// only reaching a log sink, so a test — and a caller that wants to refuse to
/// proceed — can act on them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read<T> {
    /// The decoded contents, or `T::default()` when the file does not exist or
    /// held no store.
    pub value: T,
    /// Set when the file was group- or world-accessible. Always `None` on
    /// platforms without Unix permission bits.
    pub permissions: Option<PermissionWarning>,
    /// Set when the file exists and held no store to decode. `value` is
    /// `T::default()` then, on both reads: an empty file holds no entry a write could
    /// destroy, so the write proceeds and replaces it rather than refusing the login
    /// that repairs it.
    pub damage: Option<StoreDamage>,
}

impl<T> Read<T> {
    /// Whether a permission problem was found.
    #[must_use]
    pub fn is_permissive(&self) -> bool {
        self.permissions.is_some()
    }

    /// Whether the file was found damaged.
    #[must_use]
    pub fn is_damaged(&self) -> bool {
        self.damage.is_some()
    }
}

/// The permission finding for an already-opened file, or `None` when its mode is
/// private enough.
///
/// Reads the mode from the open handle rather than the path, so a file swapped
/// between the check and the read cannot be reported under the wrong mode.
#[cfg(unix)]
fn permission_warning(path: &Path, file: &File) -> Result<Option<PermissionWarning>, AuthError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = file.metadata().map_err(|source| AuthError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & FORBIDDEN_BITS == 0 {
        return Ok(None);
    }
    Ok(Some(PermissionWarning {
        path: path.to_path_buf(),
        mode,
    }))
}

/// Windows has no Unix permission bits, so there is nothing to check.
///
/// Access there is governed by NTFS ACLs, which this crate does not set; the file
/// inherits the parent directory's ACL. See the crate docs and the task-24 entry
/// in the project's engineering notes.
#[cfg(not(unix))]
fn permission_warning(_path: &Path, _file: &File) -> Result<Option<PermissionWarning>, AuthError> {
    Ok(None)
}

/// Apply [`CREDENTIAL_FILE_MODE`] to a file that already exists.
///
/// [`write_json`] creates with the mode already set, so this only asserts the bits
/// the umask could have cleared from what `open` was given. Unix-only, like the
/// publication that calls it.
#[cfg(unix)]
fn enforce_mode(path: &Path, file: &File) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = file
        .metadata()
        .map_err(|source| AuthError::Permissions {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    if permissions.mode() & 0o777 == CREDENTIAL_FILE_MODE {
        return Ok(());
    }
    permissions.set_mode(CREDENTIAL_FILE_MODE);
    file.set_permissions(permissions)
        .map_err(|source| AuthError::Permissions {
            path: path.to_path_buf(),
            source,
        })
}

/// Create a credential file that must not exist yet, at
/// [`CREDENTIAL_FILE_MODE`] so its contents are never briefly world-readable.
///
/// `create_new` is what makes the sibling [`publish`] writes into safe: a name no
/// other process can already own cannot be a symlink somebody planted in the data
/// directory ahead of the write.
///
/// This is the whole reason the Unix branch does not delegate to
/// `zuno_atomic_file::replace`, which opens its temporary file with default
/// options.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(CREDENTIAL_FILE_MODE)
        .open(path)
}

/// Longest symlink chain a write follows before it refuses, matching the
/// `SYMLOOP_MAX` most platforms enforce — and matching
/// [`zuno_atomic_file::MAX_LINK_DEPTH`], which resolves the same chain for the
/// publication this module delegates off Unix.
///
/// The value is taken from that crate so the two publications cannot disagree about
/// which chain is a loop, but it is a bound on where a *credential* write may land,
/// so it is not inherited without a ceiling: [`REVIEWED_LINK_DEPTH`] refuses to build a
/// depth greater than the one reviewed here. Raising the shared constant is then a
/// decision this module has to take again rather than one it follows silently.
#[cfg(unix)]
const MAX_LINK_DEPTH: usize = zuno_atomic_file::MAX_LINK_DEPTH;

/// The chain depth this module reviewed for a credential write.
///
/// Deliberately a literal. Deriving it from [`zuno_atomic_file::MAX_LINK_DEPTH`] too
/// would make the gate below true by construction and stop it catching the widening it
/// exists to catch.
const REVIEWED_LINK_DEPTH: usize = 40;

/// Refuses to build if the shared bound ever rises above the depth reviewed here.
///
/// Lowering the shared constant is safe to inherit; raising it widens where a secret may
/// be redirected to, so it has to be a decision taken in this module as well. Failing at
/// compile time rather than in a test keeps a widened bound from reaching a release
/// through a green `-p zuno-atomic-file` run.
///
/// Checked on **every** target, deliberately not only where this module resolves the
/// chain itself. Off Unix the whole publication is `zuno_atomic_file::replace`'s and it
/// follows that same constant through its own `follow_link_chain`, so a widened bound
/// would otherwise be inherited silently on the one platform that does not review it —
/// and Windows is where publication is delegated wholesale.
const _: () = assert!(
    zuno_atomic_file::MAX_LINK_DEPTH <= REVIEWED_LINK_DEPTH,
    "a credential write would follow a deeper symlink chain than this module reviewed: \
     raising zuno_atomic_file::MAX_LINK_DEPTH also has to be decided in zuno-auth"
);

/// The directory a path lives in, reading the empty parent of a bare file name as
/// the current directory.
#[cfg(unix)]
fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// The file a credential path ultimately names, following symlinks deliberately.
///
/// A user who points `auth.json` at an encrypted volume or a shared secrets
/// directory means the tokens to live there. Publishing under the link's own name
/// would replace the link with a regular file and strand the location they chose,
/// so the chain is followed and the replacement published in the target's own
/// directory — which is also where writing through the opened link used to land.
/// The link survives; the target file's ownership, ACLs and extended attributes do
/// not, because publication creates a new inode.
///
/// Only the Unix branch resolves the chain itself, because only it places the
/// sibling. Off Unix `zuno_atomic_file::replace` follows the same chain to the same
/// depth with the same refusal, so the path is handed over unresolved.
#[cfg(unix)]
fn resolve_destination(path: &Path) -> Result<PathBuf, AuthError> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_DEPTH {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_symlink() => {}
            Ok(_) => return Ok(current),
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(current),
            Err(source) => {
                return Err(AuthError::Write {
                    path: current,
                    source,
                });
            }
        }
        let target = fs::read_link(&current).map_err(|source| AuthError::Write {
            path: current.clone(),
            source,
        })?;
        current = if target.is_absolute() {
            target
        } else {
            parent_of(&current).join(target)
        };
    }
    Err(AuthError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{} resolves through more than {MAX_LINK_DEPTH} symlinks, which is a loop rather \
                 than a redirection",
                path.display()
            ),
        ),
    })
}

/// Off Unix the publication resolves the chain itself, so the path is passed
/// through untouched.
#[cfg(not(unix))]
fn resolve_destination(path: &Path) -> Result<PathBuf, AuthError> {
    Ok(path.to_path_buf())
}

/// Give `path` the contents `body` in a single name transition, at
/// [`CREDENTIAL_FILE_MODE`].
///
/// The bytes are written to a sibling, restricted before the first one lands,
/// flushed to the device, and only then renamed over the destination. Every failure
/// leaves the previous credential file exactly as it was, and removes the sibling —
/// an unpublished sibling is a spare copy of the user's credentials.
///
/// The containing directory is flushed after the rename so the new name itself
/// survives a power loss rather than only the bytes behind it. A filesystem that
/// refuses to flush a directory is logged and not failed: the document is published
/// by that point, and reporting a failure would invite the caller to write it a
/// second time.
#[cfg(unix)]
fn publish(path: &Path, body: &[u8]) -> Result<(), AuthError> {
    let failed = |source: std::io::Error| AuthError::Write {
        path: path.to_path_buf(),
        source,
    };

    let parent = parent_of(path);
    fs::create_dir_all(parent).map_err(failed)?;
    let file_name = path.file_name().ok_or_else(|| {
        failed(std::io::Error::new(
            ErrorKind::InvalidInput,
            "credential path names no file",
        ))
    })?;
    let mut sibling_name = file_name.to_os_string();
    sibling_name.push(format!(".tmp.{}", uuid::Uuid::new_v4()));
    let sibling = parent.join(sibling_name);

    let published = (|| {
        let mut file = create_private(&sibling).map_err(|source| {
            // Only a refusal to create is the directory's doing. A full disk or a bad
            // name is the write's, and saying "the directory refused" would send the
            // operator to `chmod` something that is not the problem.
            if source.kind() == ErrorKind::PermissionDenied {
                AuthError::Directory {
                    path: path.to_path_buf(),
                    directory: parent.to_path_buf(),
                    source,
                }
            } else {
                failed(source)
            }
        })?;
        // `open`'s mode argument is only ever masked by the umask, so the bits are
        // asserted on the handle before a secret reaches the file. The error names
        // the destination, which is the path an operator can act on.
        enforce_mode(path, &file)?;
        file.write_all(body).map_err(failed)?;
        // A rename that outran its data blocks would publish the empty file this
        // whole dance exists to prevent.
        file.sync_all().map_err(failed)?;
        drop(file);
        fs::rename(&sibling, path).map_err(failed)
    })();
    if published.is_err() {
        let _ignored = fs::remove_file(&sibling);
        return published;
    }
    flush_directory(parent);
    published
}

/// Ask the filesystem to make a completed rename durable, best effort.
///
/// `sync_all` on the sibling orders its data before the name transition; without
/// this the transition itself can be lost by a power failure, which would roll a
/// login back after it reported success. Some filesystems reject `fsync` on a
/// directory descriptor, and there is nothing useful to do about that at this point
/// — the credentials are already published — so the outcome is recorded rather than
/// returned.
#[cfg(unix)]
fn flush_directory(parent: &Path) {
    match File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => {}
        Err(error) => tracing::debug!(
            directory = %parent.display(),
            %error,
            "the credential file is published but its directory could not be flushed, so a \
             power loss could still roll the write back"
        ),
    }
}

/// Give `path` the contents `body` in a single name transition, through the
/// workspace publication primitive.
///
/// `zuno_atomic_file::replace` owns the platform policy this module must not
/// reinvent: on Windows it publishes with `ReplaceFileW` rather than the
/// gap-producing `MoveFileEx` that `fs::rename` would use, it follows a symlink at
/// `path` to the file the link names, and it removes its temporary file on failure.
/// It does not promise a power-loss boundary, and there is nothing to add here
/// without reimplementing the publication — which is the thing that produced the
/// Windows gap in the first place.
///
/// Compiled on Unix under `cfg(test)` as well, so the branch that runs on the
/// platform this host cannot execute is still type-checked and behaviour-tested
/// here, and so the mode it produces — the one reason Unix does not use it — is
/// asserted rather than asserted-in-prose.
#[cfg(any(not(unix), test))]
fn publish_through_workspace_primitive(path: &Path, body: &[u8]) -> Result<(), AuthError> {
    zuno_atomic_file::replace(path, body).map_err(|source| AuthError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Ask the filesystem to make an already-published document durable, best effort.
///
/// The off-Unix publication primitive writes its temporary file, closes it, and
/// publishes the name without a `sync_all` anywhere — its own module docs say it does
/// not promise crash durability and that a caller needing that boundary must sync the
/// temporary file and the directory itself. That temporary file is private to the
/// primitive, so the only handle this module can still reach is the published one.
/// Flushing it moves the exposure from "the whole document may never have reached the
/// device" to "the interval between the name transition and this flush may not have",
/// which is the difference between a token refresh that can leave a zero-byte
/// `auth.json` and one that can only lose the refresh itself.
///
/// It is not a substitute for the Unix boundary and it is not reported as a failure:
/// the document is published by the time this runs, so returning an error would invite
/// the caller to write it a second time. The write handle is deliberate — Windows needs
/// write access for `FlushFileBuffers`, and a read handle would fail on the platform
/// this exists for.
#[cfg(any(not(unix), test))]
fn flush_published(path: &Path) {
    match OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
    {
        Ok(()) => {}
        Err(error) => tracing::debug!(
            path = %path.display(),
            %error,
            "the credential file is published but could not be flushed to the device, so a power \
             loss could still leave it empty or torn"
        ),
    }
}

/// The whole off-Unix publication: the workspace primitive's name transition, then the
/// only part of the power-loss boundary this module can still add from outside it.
///
/// Compiled and behaviour-tested on Unix under `cfg(test)` for the same reason
/// [`publish_through_workspace_primitive`] is.
#[cfg(any(not(unix), test))]
fn publish_off_unix(path: &Path, body: &[u8]) -> Result<(), AuthError> {
    publish_through_workspace_primitive(path, body)?;
    flush_published(path);
    Ok(())
}

#[cfg(not(unix))]
fn publish(path: &Path, body: &[u8]) -> Result<(), AuthError> {
    publish_off_unix(path, body)
}

/// A read failure that names the file an operator can act on.
fn read_failed(path: &Path, source: std::io::Error) -> AuthError {
    AuthError::Read {
        path: path.to_path_buf(),
        source,
    }
}

/// A store nobody has written yet: the result every read gives for an absent file.
fn unwritten<T: Default>() -> Read<T> {
    Read {
        value: T::default(),
        permissions: None,
        damage: None,
    }
}

/// Whether an open failure could be somebody else's publication in flight rather
/// than a fact about the file.
///
/// On Unix `rename` is atomic and `ENOENT` means absent. On Windows a replacement
/// in flight makes a fresh open fail with `ERROR_FILE_NOT_FOUND` (2) — which Rust
/// reports as `NotFound`, indistinguishable from a store that was never written —
/// or with `ERROR_SHARING_VIOLATION` (32). `zuno-atomic-file` documents both and its
/// own reads absorb them.
///
/// The test is on the *kind* rather than on raw error 2, deliberately: Windows
/// answers a missing parent directory with `ERROR_PATH_NOT_FOUND` (3), which is also
/// `NotFound`, and a first login on a machine with no data directory must resolve to
/// absence rather than to a failure.
fn may_be_a_publication_window(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    let transient = error.kind() == ErrorKind::NotFound || error.raw_os_error() == Some(32);
    #[cfg(not(windows))]
    let transient = error.kind() == ErrorKind::NotFound;
    transient
}

/// How long an absence that the platform cannot distinguish from a publication in
/// flight is re-probed before it is concluded anyway.
///
/// Unix does not spend it: `rename` is atomic, so `ENOENT` is a fact and the first probe
/// settles. Windows answers a probe issued while `ReplaceFileW` holds its handles with
/// `ERROR_FILE_NOT_FOUND` — the same answer it gives for a file nobody ever wrote — so
/// absence there is a conclusion drawn after the window has had time to close rather
/// than something the platform reports.
///
/// Small on purpose, and audited in both directions.
///
/// Longer is not safer. The ordinary first `zuno auth login` on a machine with no
/// credential file gets the *unsettled* answer for a file that will never exist, so the
/// whole budget is spent — in `thread::sleep`, inside a synchronous function that async
/// callers reach. `zuno_atomic_file::metadata` bounds its expected-presence policy at one
/// second, which is why this module decides absence itself instead of calling it.
///
/// Shorter is not safer either. A window that outlasts the budget resolves to "absent",
/// and the write that follows publishes a store holding only its own entry. A
/// `ReplaceFileW` handle pair is held for the length of a metadata operation, so this is
/// orders of magnitude more than the window it has to outlast and orders of magnitude
/// less than the wait it replaces.
pub const ABSENCE_CONFIRMATION: std::time::Duration = std::time::Duration::from_millis(6);

/// Whether a credential file is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presence {
    /// The file exists.
    Present,
    /// The file does not exist — as far as the platform is willing to say, within
    /// [`ABSENCE_CONFIRMATION`].
    Absent,
}

/// What one existence probe concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    /// The platform answered about the file itself.
    Settled(Presence),
    /// The platform gave the answer it also gives inside a publication, so it has not
    /// answered about the file.
    ///
    /// Only Windows produces this: `rename` is atomic on Unix, so `ENOENT` there is a
    /// fact about the file and every probe settles on the first answer. The variant is
    /// still compiled everywhere because [`confirm_presence`] is the same code on every
    /// platform and is exercised with an injected probe on this one.
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "constructed only by the Windows arm of classify_probe, and by the \
                      injected-probe tests that exercise the budget policy on a host that \
                      cannot produce the ambiguous answer"
        )
    )]
    Unsettled,
}

/// Read one probe result as a conclusion about the file, or as no conclusion at all.
///
/// A sharing violation is a conclusion: something holds the file open, so it exists.
/// `ERROR_FILE_NOT_FOUND` (raw 2) is the ambiguous one and the only retried answer;
/// `ERROR_PATH_NOT_FOUND` (raw 3, also `NotFound`) is a missing data directory, which is
/// a fact about a first run and must settle immediately.
fn classify_probe(outcome: std::io::Result<()>) -> std::io::Result<Probe> {
    match outcome {
        Ok(()) => Ok(Probe::Settled(Presence::Present)),
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(32) => Ok(Probe::Settled(Presence::Present)),
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(2) => Ok(Probe::Unsettled),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Probe::Settled(Presence::Absent)),
        Err(error) => Err(error),
    }
}

/// Probe until the answer is about the file, or until `budget` is spent.
///
/// Spending the budget resolves to [`Presence::Absent`]: the platform cannot say more,
/// and failing instead would deny the first login on every machine that has no
/// credential file. The probe is injected so the budget policy is exercised on a host
/// that cannot produce the ambiguous answer.
fn confirm_presence(
    budget: std::time::Duration,
    mut probe: impl FnMut() -> std::io::Result<Probe>,
) -> std::io::Result<Presence> {
    let started = std::time::Instant::now();
    let mut delay = std::time::Duration::from_millis(1);
    loop {
        if let Probe::Settled(presence) = probe()? {
            return Ok(presence);
        }
        if started.elapsed() >= budget {
            return Ok(Presence::Absent);
        }
        std::thread::sleep(delay);
        delay = delay
            .saturating_mul(2)
            .min(std::time::Duration::from_millis(2));
    }
}

/// Whether the credential file at `path` exists, on the terms
/// [`ABSENCE_CONFIRMATION`] describes.
///
/// One `stat` on Unix. Deliberately not `zuno_atomic_file::metadata`: that entry point
/// implements an *expected-presence* policy whose one-second budget is spent in full for
/// a file that is legitimately absent, which is the ordinary first-login case here.
fn presence_of(path: &Path) -> Result<Presence, AuthError> {
    confirm_presence(ABSENCE_CONFIRMATION, || {
        classify_probe(fs::metadata(path).map(|_| ()))
    })
    .map_err(|source| read_failed(path, source))
}

/// Open a credential file for a read whose result will be written back, treating
/// absence as a conclusion rather than as the first answer that looked like one.
///
/// `Ok(None)` means the file is not there. That conclusion matters because the caller
/// is about to publish the map it decodes: an open that failed inside another
/// process's publication would otherwise become "no credentials", and the write would
/// replace a populated store with one holding a single entry.
///
/// `presence` is [`presence_of`]. When it says the file is there, the open is attempted
/// once more; when that also fails inside the window the outcome is
/// [`AuthError::Unresolved`] — an explicit "not resolvable" rather than a permissive
/// default.
///
/// Taking both operations as closures is what makes the decision testable on a
/// platform that cannot produce a `MoveFileEx` gap; it follows
/// `zuno_atomic_file`'s own `read_published` test.
fn open_for_update<H>(
    path: &Path,
    mut open: impl FnMut() -> std::io::Result<H>,
    mut presence: impl FnMut() -> Result<Presence, AuthError>,
) -> Result<Option<H>, AuthError> {
    match open() {
        Ok(handle) => return Ok(Some(handle)),
        Err(source) if may_be_a_publication_window(&source) => {}
        Err(source) => return Err(read_failed(path, source)),
    }
    // A store nobody has written yet rather than one that vanished mid-publication —
    // within what the platform can be asked, see [`ABSENCE_CONFIRMATION`].
    if presence()? == Presence::Absent {
        return Ok(None);
    }
    match open() {
        Ok(handle) => Ok(Some(handle)),
        Err(source) if may_be_a_publication_window(&source) => Err(AuthError::Unresolved {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(read_failed(path, source)),
    }
}

/// The damage each credential file has already been reported for, once per process.
///
/// The key is the pair (kind of damage, file), which is as fine as the finding gets:
/// a zero-byte `auth.json` cannot spend the one report that belongs to
/// `mcp-auth.json`, and a second [`StoreDamage`] variant added later cannot spend the
/// report that belongs to an emptied store. It is *not* keyed per read, deliberately —
/// the server's catalogue route reads this file once per HTTP request and the turn path
/// once per turn, so an event per read is log pressure rather than a signal, and the
/// finding does not change between two reads of the same broken file.
static REPORTED_DAMAGE: std::sync::Mutex<
    Option<std::collections::BTreeSet<(&'static str, PathBuf)>>,
> = std::sync::Mutex::new(None);

/// Report damage the first time this process sees it in this file.
///
/// Error level, because the store the user had is gone and no code path can bring it
/// back; and once, because repeating it per read would bury it. The returned
/// [`StoreDamage`] is the durable half of this — a client surface that wants to show it
/// on every read reads the field rather than the log, which is what makes the report
/// visible to a user whose log sink is off.
fn report_once(damage: &StoreDamage) {
    let key = (damage.kind(), damage.path().to_path_buf());
    let mut reported = match REPORTED_DAMAGE.lock() {
        Ok(guard) => guard,
        // A panic in another thread's report says nothing about the set's contents.
        Err(poisoned) => poisoned.into_inner(),
    };
    if !reported
        .get_or_insert_with(std::collections::BTreeSet::new)
        .insert(key)
    {
        return;
    }
    drop(reported);
    tracing::error!(path = %damage.path().display(), "{damage}");
}

/// Decode an open credential file, reporting damage rather than deciding what it
/// means. The caller decides: see [`read_json`] and [`read_json_for_update`].
fn decode<T: DeserializeOwned + Default>(
    path: &Path,
    mut file: File,
) -> Result<Read<T>, AuthError> {
    let permissions = permission_warning(path, &file)?;
    if let Some(warning) = &permissions {
        tracing::warn!(
            path = %warning.path.display(),
            mode = format_args!("{:04o}", warning.mode),
            expected = format_args!("{CREDENTIAL_FILE_MODE:04o}"),
            "{warning}"
        );
    }

    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|source| read_failed(path, source))?;

    // Nothing in this crate writes an empty credential file: `write_json` publishes
    // a whole document under a name that never held a partial one. So an empty file
    // is damage — an interrupted write by an older Zuno, or a truncation from
    // outside — and the operator has to be told even when the caller carries on.
    if text.trim().is_empty() {
        let damage = StoreDamage::Empty {
            path: path.to_path_buf(),
        };
        report_once(&damage);
        return Ok(Read {
            value: T::default(),
            permissions,
            damage: Some(damage),
        });
    }

    let value = serde_json::from_str(&text).map_err(|source| AuthError::Malformed {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Read {
        value,
        permissions,
        damage: None,
    })
}

/// Read and decode a credential file for a caller that will display or consult the
/// result, not write it back.
///
/// - Absent file → `T::default()` and no report. The oracle treats a missing
///   `auth.json` as an empty record (`auth/index.ts:65`), and so does this.
/// - Present but group- or world-accessible → decoded normally, with a
///   [`PermissionWarning`] returned and a `tracing::warn!` emitted that names the
///   path.
/// - Present and empty → `T::default()` with [`StoreDamage::Empty`] returned and one
///   `tracing::error!` per file per process that names the file and says what happened.
///   Refusing would deny `zuno auth list`, `zuno models` and a run whose credential
///   comes from the environment, which is a worse outcome than the loss: it removes the
///   commands that repair it. [`read_json_for_update`] agrees, for the same reason
///   applied to `zuno auth login`.
/// - Unreadable → [`AuthError::Read`].
/// - Present but not valid JSON → [`AuthError::Malformed`].
///
/// The last case is a deliberate divergence. The oracle pipes every read failure
/// into `orElseSucceed(() => ({}))`, so a truncated `auth.json` reads as empty and
/// the next `set` writes that emptiness back — silently destroying every credential
/// in the file. See the task-24 entry in the project's engineering notes.
pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<Read<T>, AuthError> {
    match File::open(path) {
        Ok(file) => decode(path, file),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(unwritten()),
        Err(source) => Err(read_failed(path, source)),
    }
}

/// Read and decode a credential file for a caller that will write the result back.
///
/// Differs from [`read_json`] in exactly one place, and it is not damage: **absence has
/// to be a conclusion**. An open that fails inside another process's publication reports
/// the same `NotFound` as a store that was never written, and on Windows `ReplaceFileW`
/// really does produce that answer for a file that is present and complete. Believing it
/// is how a write comes to publish a store holding only its own entry, so the file's
/// existence is re-probed, and a file that is there and still will not open gives
/// [`AuthError::Unresolved`] with nothing written.
///
/// Damage is reported and carried, not refused. An empty file holds no entry a write
/// could destroy, and the write that follows is `zuno auth login` or a token refresh —
/// the operations that repair it. A file with *content* this build cannot parse is
/// different and still fails, as [`AuthError::Malformed`], on this read and on
/// [`read_json`] alike: those bytes may be a recoverable store.
pub fn read_json_for_update<T: DeserializeOwned + Default>(
    path: &Path,
) -> Result<Read<T>, AuthError> {
    let opened = open_for_update(path, || File::open(path), || presence_of(path))?;
    let Some(file) = opened else {
        return Ok(unwritten());
    };
    decode(path, file)
}

/// The keys one credential type can write at one level of an entry, and the modelled
/// keys below them.
///
/// A read-modify-write cannot tell an *added* field from a *cleared* one by comparing the
/// document with the typed value: both are "on disk, absent from the typed value". The
/// difference is declared here. A key this list names belongs to this build — it writes
/// it or it clears it, and the file follows. A key it does not name belongs to whoever
/// wrote the file, and [`unmodelled_fields`] keeps it.
///
/// Deliberately a literal rather than something derived from a serialization: deriving it
/// would make every skipped `Option` look unmodelled, and the next write would resurrect
/// the field the user just cleared. The cost is that adding a field to a credential type
/// means adding its on-disk spelling here, which each type's own drift test enforces.
pub struct Modelled {
    /// Every key a value of this type can serialize at this level, in its on-disk
    /// spelling.
    pub keys: &'static [&'static str],
    /// The modelled keys that hold an object with modelled keys of its own.
    pub within: &'static [(&'static str, &'static Modelled)],
}

/// The parts of one entry on disk that this build does not model.
///
/// Both credential entry types are all-`Option` with `#[serde(default)]` and no
/// `deny_unknown_fields`, so a field a newer Zuno added decodes successfully into
/// nothing: `skipped`/`preserved` never see it, and the typed map written back would
/// drop it — including from an entry the write never touched. This is what a write puts
/// back around the typed value instead.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Unmodelled {
    /// Keys at this level that no modelled field owns, as the document held them.
    added: serde_json::Map<String, serde_json::Value>,
    /// The same, one level down, under a modelled key that holds an object.
    within: std::collections::BTreeMap<String, Unmodelled>,
}

impl Unmodelled {
    /// Whether the entry held nothing this build does not model.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.within.values().all(Self::is_empty)
    }

    /// Put the unmodelled keys back into a freshly serialized entry.
    ///
    /// A key the typed value produced wins, so a field this build owns is always written
    /// from the typed value and never from the document it read — which is what makes
    /// clearing a field, or changing a credential's `type`, actually take effect.
    ///
    /// A nested carry is only merged into an object that is still there. If the typed
    /// value cleared `tokens`, the unknown keys that were inside `tokens` go with it,
    /// rather than reappearing as an object holding nothing but keys this build cannot
    /// interpret.
    fn apply(&self, target: &mut serde_json::Value) {
        let Some(object) = target.as_object_mut() else {
            return;
        };
        for (key, held) in &self.added {
            object.entry(key.clone()).or_insert_with(|| held.clone());
        }
        for (key, inner) in &self.within {
            if let Some(nested) = object.get_mut(key).filter(|value| value.is_object()) {
                inner.apply(nested);
            }
        }
    }
}

/// The parts of `raw` that `shape` does not model, kept per entry so the next write puts
/// them back.
///
/// A `raw` that is not an object has nothing to keep: a value that shape cannot describe
/// at all did not decode either, so it travels as [`Rewritten::Verbatim`] instead.
#[must_use]
pub fn unmodelled_fields(raw: &serde_json::Value, shape: &Modelled) -> Unmodelled {
    let mut found = Unmodelled::default();
    let Some(object) = raw.as_object() else {
        return found;
    };
    for (key, value) in object {
        if !shape.keys.contains(&key.as_str()) {
            found.added.insert(key.clone(), value.clone());
        }
    }
    for (key, inner) in shape.within {
        if let Some(nested) = object.get(*key).filter(|value| value.is_object()) {
            let below = unmodelled_fields(nested, inner);
            if !below.is_empty() {
                found.within.insert((*key).to_owned(), below);
            }
        }
    }
    found
}

/// Reconcile the carried keys with what a write actually changed, before the document is
/// rewritten.
///
/// Two rules, and both of them are about not putting something back that no longer has a
/// place to be:
///
/// - An entry the write removed carries nothing. Otherwise a `logout` would republish an
///   object holding nothing but keys this build cannot interpret, and the entry would be
///   immortal.
/// - An entry whose *modelled sub-object* the write rewrote loses the keys that lived
///   inside that sub-object. An unknown key inside `tokens` is a claim about the tokens
///   that were there — a proof-of-possession key, a device binding, a nonce — and
///   re-attaching it to a freshly rotated token would hand another build's authority
///   claim to a value it never saw. The keys *beside* the modelled ones are carried
///   either way: they belong to whoever wrote the file and this write did not touch
///   them.
///
/// Comparison is on the serialized form, because that is what reaches the file; a typed
/// change that does not change the document is not a rewrite.
pub fn settle_unmodelled<T: Serialize>(
    before: &std::collections::BTreeMap<String, T>,
    after: &std::collections::BTreeMap<String, T>,
    unmodelled: &mut std::collections::BTreeMap<String, Unmodelled>,
) {
    unmodelled.retain(|key, _| after.contains_key(key));
    for (key, carried) in unmodelled.iter_mut() {
        if carried.within.is_empty() {
            continue;
        }
        let previously = before
            .get(key)
            .and_then(|value| serde_json::to_value(value).ok());
        let now = after
            .get(key)
            .and_then(|value| serde_json::to_value(value).ok());
        carried.within.retain(|child, _| {
            let was = previously.as_ref().and_then(|value| value.get(child));
            was.is_some() && was == now.as_ref().and_then(|value| value.get(child))
        });
    }
}

/// One entry of a credential store on its way back to disk.
///
/// A read-modify-write decodes what it understands and would drop the rest. Carrying
/// the undecoded value through the write puts it back exactly as it was read, so a
/// credential written by a newer Zuno — or an authentication shape this build does not
/// model — survives a login for a different provider instead of being deleted by the
/// store that could not read it.
#[derive(Debug)]
pub enum Rewritten<'a, T> {
    /// A value this build decoded, re-serialized from its typed form, with the keys the
    /// document held that this build does not model put back around it.
    Decoded {
        /// The typed value. Authoritative for every key its type models.
        value: &'a T,
        /// What the document held that the type does not model, or `None` for an entry
        /// with nothing to carry — which is every entry a Zuno of this vintage wrote.
        unmodelled: Option<&'a Unmodelled>,
    },
    /// A value this build did not recognize, written back as the value it read.
    ///
    /// The JSON value is preserved exactly; its spelling is not promised. The encoder
    /// re-serializes it like every other entry, so indentation, and — because this
    /// workspace does not enable `serde_json/preserve_order` — the key order inside the
    /// object, are the encoder's. Anything that depended on the original bytes, such
    /// as a signature computed over them, would not survive; nothing in Zuno does.
    Verbatim(&'a serde_json::Value),
}

/// Serialized by hand rather than as an untagged enum, because merging the unmodelled
/// keys back in means going through a [`serde_json::Value`].
///
/// An entry with nothing to carry skips that round trip entirely and is serialized
/// straight from its typed form, which is what keeps the byte-for-byte encoding — field
/// order included — identical to what every previous release wrote.
impl<T: Serialize> Serialize for Rewritten<'_, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Verbatim(value) => value.serialize(serializer),
            Self::Decoded { value, unmodelled } => {
                let Some(unmodelled) = unmodelled.filter(|held| !held.is_empty()) else {
                    return value.serialize(serializer);
                };
                let mut merged = serde_json::to_value(value).map_err(serde::ser::Error::custom)?;
                unmodelled.apply(&mut merged);
                merged.serialize(serializer)
            }
        }
    }
}

/// The document a read-modify-write publishes: everything it decoded with everything the
/// document held around it, plus every value it could not decode at all, keyed and
/// ordered exactly as a store of `T` alone would be.
///
/// A key present in `decoded` wins over `verbatim`. Those two maps are disjoint by
/// construction — a value either decoded or did not — so the precedence only decides a
/// case a caller would have had to invent. `unmodelled` is keyed by the same keys as
/// `decoded`; a key it does not mention carries nothing.
pub fn rewrite<'a, T>(
    decoded: &'a std::collections::BTreeMap<String, T>,
    unmodelled: &'a std::collections::BTreeMap<String, Unmodelled>,
    verbatim: &'a std::collections::BTreeMap<String, serde_json::Value>,
) -> std::collections::BTreeMap<&'a str, Rewritten<'a, T>> {
    let mut document: std::collections::BTreeMap<&'a str, Rewritten<'a, T>> = verbatim
        .iter()
        .map(|(key, value)| (key.as_str(), Rewritten::Verbatim(value)))
        .collect();
    document.extend(decoded.iter().map(|(key, value)| {
        (
            key.as_str(),
            Rewritten::Decoded {
                value,
                unmodelled: unmodelled.get(key),
            },
        )
    }));
    document
}

/// Serialize `value` and publish it at `path` at [`CREDENTIAL_FILE_MODE`].
///
/// The encoding is `JSON.stringify(value, null, 2)`: two-space indent and **no**
/// trailing newline. Both were confirmed byte-for-byte against a file the 1.18.12
/// binary wrote.
///
/// The document reaches its name in one transition, so an interrupted write leaves
/// the credentials that were there already. A `path` that is a symlink is followed
/// to the file it names, and the replacement published there. Off Unix the
/// transition is `zuno_atomic_file::replace`'s; see the module docs for why Unix is
/// the exception rather than the rule.
///
/// Publishing needs permission to create a file in the containing directory, and
/// replaces the destination's inode, so its ownership, ACLs and extended attributes
/// do not carry over.
///
/// Missing parent directories are created. The oracle relies on its startup
/// having already created the data directory; a store used on its own cannot.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AuthError> {
    let body = serde_json::to_vec_pretty(value).map_err(|source| AuthError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;

    publish(&resolve_destination(path)?, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use zuno_error::Recovery;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    type Map = BTreeMap<String, String>;

    fn mode_of(path: &Path) -> u32 {
        #[cfg(unix)]
        {
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            CREDENTIAL_FILE_MODE
        }
    }

    fn sample() -> Map {
        BTreeMap::from([("alpha".to_owned(), "one".to_owned())])
    }

    #[test]
    fn a_fresh_write_lands_at_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("creds.json");
        write_json(&path, &sample()).expect("write");
        assert_eq!(mode_of(&path), CREDENTIAL_FILE_MODE);
    }

    #[test]
    fn a_write_over_a_permissive_file_repairs_the_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("creds.json");
        fs::write(&path, b"{}").expect("seed");
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");

        write_json(&path, &sample()).expect("write");
        assert_eq!(mode_of(&path), CREDENTIAL_FILE_MODE);
    }

    /// `JSON.stringify(data, null, 2)`: two spaces, no trailing newline.
    #[test]
    fn the_encoding_matches_the_oracle_byte_for_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("creds.json");
        write_json(&path, &sample()).expect("write");
        let bytes = fs::read(&path).expect("read back");
        assert_eq!(
            String::from_utf8(bytes).expect("utf8"),
            "{\n  \"alpha\": \"one\"\n}"
        );
    }

    #[test]
    fn an_absent_file_reads_as_the_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome: Read<Map> = read_json(&dir.path().join("missing.json")).expect("read");
        assert!(outcome.value.is_empty());
        assert_eq!(outcome.permissions, None);
        assert!(!outcome.is_permissive());
    }

    /// A file with nothing in it is what the shipped 0.6.6 truncate window left
    /// behind, and it is exactly the population that has to be able to log in again.
    /// So the read that will be written back admits it too: `T::default()` with the
    /// damage reported, and the write that follows publishes a whole store over it.
    ///
    /// The seeds are the two the reviewer measured: a zero-byte file and one holding
    /// only whitespace.
    #[test]
    fn an_empty_file_is_admitted_by_the_read_that_will_be_written_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (label, seed) in [("zero-byte", b"".as_slice()), ("whitespace", b" \t\r\n ")] {
            let path = dir.path().join(format!("{label}.json"));
            fs::write(&path, seed).expect("seed");

            let outcome =
                read_json_for_update::<Map>(&path).expect("an empty store must not deny the write");
            assert!(outcome.value.is_empty(), "{label}");
            assert_eq!(
                outcome.damage,
                Some(StoreDamage::Empty { path: path.clone() }),
                "{label}"
            );

            // The write half: the login that repairs it actually lands, and the file
            // that comes back is a real store rather than the empty one.
            write_json(&path, &sample()).expect("the repair write must be allowed");
            let repaired = read_json_for_update::<Map>(&path).expect("read back");
            assert_eq!(repaired.value, sample(), "{label}");
            assert_eq!(repaired.damage, None, "{label}");
        }
    }

    /// The remedy is only reachable through the report, so the report has to carry it.
    /// [`AuthError`] deliberately has no variant for this — a test that a failure
    /// wording exists would have pinned the lockout in place.
    #[test]
    fn no_auth_error_variant_reports_an_empty_store_and_the_damage_carries_the_remedy() {
        let path = PathBuf::from("/tmp/auth.json");
        let damage = StoreDamage::Empty { path: path.clone() };
        let rendered = damage.to_string();
        for expected in [
            "holds no store",
            "an interrupted write or a truncation from outside emptied it",
            "log in again and the next write replaces it",
        ] {
            assert!(
                rendered.contains(expected),
                "{rendered} is missing {expected:?}"
            );
        }
        assert!(
            rendered.contains(&path.display().to_string()),
            "{rendered} must name the file"
        );
    }

    /// The other half of that split. `zuno auth list`, `zuno models` and a run whose
    /// model credential comes from the environment all reach the file through this
    /// read, and the zero-byte file is exactly what the shipped truncate bug left
    /// behind — so refusing here would turn an upgrade into a lockout from the
    /// commands that repair it. The damage is reported, not swallowed.
    #[test]
    fn an_empty_file_is_still_readable_and_reports_the_damage_it_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (label, seed) in [("zero-byte", b"".as_slice()), ("whitespace", b"   \n")] {
            let path = dir.path().join(format!("{label}.json"));
            fs::write(&path, seed).expect("seed");

            let outcome = read_json::<Map>(&path).expect("a read must not be denied");

            assert!(outcome.value.is_empty(), "{label}");
            assert!(outcome.is_damaged(), "{label}");
            let damage = outcome
                .damage
                .expect("the report must travel with the read");
            assert_eq!(damage, StoreDamage::Empty { path: path.clone() });
            assert_eq!(damage.path(), path.as_path(), "{label}");
        }
    }

    /// An absent file is not damage: it is a store nobody has written yet, and it is
    /// the state of every machine before the first login. Both reads have to agree,
    /// and neither may report damage.
    #[test]
    fn an_absent_file_is_a_first_login_on_both_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.json");

        for label in ["display", "update"] {
            let outcome: Read<Map> = if label == "display" {
                read_json(&path)
            } else {
                read_json_for_update(&path)
            }
            .expect("absence is not a failure");
            assert!(outcome.value.is_empty(), "{label}");
            assert_eq!(outcome.damage, None, "{label}");
            assert_eq!(outcome.permissions, None, "{label}");
        }

        // The parent directory not existing yet is the same conclusion. On Windows it
        // is a different raw error (ERROR_PATH_NOT_FOUND rather than
        // ERROR_FILE_NOT_FOUND), which is why the predicate keys on the error kind.
        let unborn = dir.path().join("no-such-dir").join("auth.json");
        let outcome: Read<Map> = read_json_for_update(&unborn).expect("a first login");
        assert!(outcome.value.is_empty());
        assert_eq!(outcome.damage, None);
    }

    /// The Windows publication window, which is the exact sequence a concurrent
    /// `zuno auth login` produces for a `zuno serve` refreshing a token: while
    /// `ReplaceFileW` holds its handles a fresh open of a file that is present and
    /// complete fails with `ERROR_FILE_NOT_FOUND` (raw 2, `NotFound` on every
    /// platform). Reading that as "no credentials" is what lets the write that
    /// follows publish a store holding only the refreshed provider, destroying the
    /// others — finding server-07's mechanism with absence substituting for zero
    /// bytes.
    ///
    /// The decision is exercised through the production function with the open and
    /// the presence check injected, because no Linux syscall produces a `MoveFileEx`
    /// gap. `zuno_atomic_file` tests its own `read_published` policy the same way.
    #[test]
    fn an_open_inside_a_publication_window_is_never_concluded_to_be_an_absent_store() {
        let path = Path::new("/tmp/auth.json");
        let window = || std::io::Error::from_raw_os_error(2);
        assert_eq!(
            window().kind(),
            ErrorKind::NotFound,
            "raw 2 is ENOENT and ERROR_FILE_NOT_FOUND; the whole problem is that it \
             is indistinguishable from a store that was never written"
        );

        // The file is there, so the store is re-read rather than defaulted.
        let mut opens = 0;
        let mut presence_checks = 0;
        let opened = open_for_update(
            path,
            || {
                opens += 1;
                if opens == 1 {
                    Err(window())
                } else {
                    Ok("the complete store")
                }
            },
            || {
                presence_checks += 1;
                Ok(Presence::Present)
            },
        )
        .expect("a file that is present is not a failure");
        assert_eq!(opened, Some("the complete store"));
        assert_eq!(opens, 2, "a confirmed-present file must be opened again");
        assert_eq!(presence_checks, 1);

        // Still inside a window on the second attempt: unresolved, never empty.
        let error = open_for_update(path, || Err::<&str, _>(window()), || Ok(Presence::Present))
            .expect_err("a file that is there and will not open is not an empty store");
        match &error {
            AuthError::Unresolved { path: named } => assert_eq!(named, path),
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(error.recovery(), Recovery::Fail);

        // Confirmed absence still resolves to a first login, with no failure and
        // without consulting the file twice.
        let mut opens = 0;
        let absent = open_for_update(
            path,
            || {
                opens += 1;
                Err::<&str, _>(window())
            },
            || Ok(Presence::Absent),
        )
        .expect("an absent store is not damage");
        assert_eq!(absent, None);
        assert_eq!(opens, 1);

        // An open that succeeds must not pay for a presence check at all.
        let mut presence_checks = 0;
        let opened = open_for_update(
            path,
            || Ok("the complete store"),
            || {
                presence_checks += 1;
                Ok(Presence::Present)
            },
        )
        .expect("a readable file");
        assert_eq!(opened, Some("the complete store"));
        assert_eq!(presence_checks, 0);

        // A failure that is not a publication window is reported as itself, so a
        // permission problem is never mistaken for absence.
        let error = open_for_update(
            path,
            || Err::<&str, _>(std::io::Error::from(ErrorKind::PermissionDenied)),
            || Ok(Presence::Present),
        )
        .expect_err("access denied is not absence");
        match &error {
            AuthError::Read {
                path: named,
                source,
            } => {
                assert_eq!(named, path);
                assert_eq!(source.kind(), ErrorKind::PermissionDenied);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // A presence check that fails for its own reasons is reported rather than
        // read as absence.
        let error = open_for_update(
            path,
            || Err::<&str, _>(window()),
            || {
                Err(read_failed(
                    path,
                    std::io::Error::from(ErrorKind::PermissionDenied),
                ))
            },
        )
        .expect_err("an unanswerable presence question is not an answer");
        assert!(matches!(error, AuthError::Read { .. }), "{error:?}");

        // Windows adds ERROR_SHARING_VIOLATION to the same window, and answers a
        // missing parent directory with ERROR_PATH_NOT_FOUND.
        #[cfg(windows)]
        {
            let opened = open_for_update(
                path,
                || {
                    opens += 1;
                    if opens == 2 {
                        Err(std::io::Error::from_raw_os_error(32))
                    } else {
                        Ok("the complete store")
                    }
                },
                || Ok(Presence::Present),
            )
            .expect("a sharing violation over a present file is a window");
            assert_eq!(opened, Some("the complete store"));

            let absent = open_for_update(
                path,
                || Err::<&str, _>(std::io::Error::from_raw_os_error(3)),
                || Ok(Presence::Absent),
            )
            .expect("a missing data directory is a first login");
            assert_eq!(absent, None);
        }
    }

    /// The damage report is not the whole obligation: an operator who never inspects
    /// the returned value still has to be told, because the read now succeeds. This
    /// captures the `tracing` output and asserts the event names the file and the
    /// remedy at error level.
    #[test]
    fn the_damage_report_reaches_the_log_naming_the_file_and_the_remedy() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("lock").extend_from_slice(buffer);
                Ok(buffer.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        fs::write(&path, b"").expect("what an interrupted write left");

        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let outcome: Read<Map> = tracing::subscriber::with_default(subscriber, || {
            // Another test in this binary may already have hit this callsite with no
            // subscriber in scope, which caches the interest as "never" and makes the
            // capture below silently empty. Observed once as a parallel-run flake.
            tracing::callsite::rebuild_interest_cache();
            read_json(&path).expect("read")
        });
        assert!(outcome.is_damaged());

        let logged = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
        assert!(logged.contains("ERROR"), "{logged}");
        assert!(logged.contains(&path.display().to_string()), "{logged}");
        assert!(logged.contains("log in again"), "{logged}");
    }

    /// The window this write exists to close. A reader that catches the
    /// destination between a truncate and its write sees an empty or half-written
    /// store; a killed process leaves exactly that state on disk for good. Every
    /// read here must land on one complete document or the other.
    #[cfg(unix)]
    #[test]
    fn a_concurrent_reader_never_observes_a_half_published_store() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::time::{Duration, Instant};

        fn store_of(filler: char) -> Map {
            (0..64)
                .map(|index| {
                    (
                        format!("provider-{index:03}"),
                        std::iter::repeat_n(filler, 256).collect(),
                    )
                })
                .collect()
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let old = store_of('o');
        let new = store_of('n');
        write_json(&path, &old).expect("seed");

        let done = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicU64::new(0));
        let reader = {
            let done = Arc::clone(&done);
            let reads = Arc::clone(&reads);
            let path = path.clone();
            let old = old.clone();
            let new = new.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut failures = Vec::new();
                while !done.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(30) {
                    match read_json::<Map>(&path) {
                        Ok(outcome) if outcome.is_damaged() => {
                            failures.push(format!("read reported damage: {:?}", outcome.damage))
                        }
                        Ok(outcome) if outcome.value == old || outcome.value == new => {
                            reads.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(outcome) => failures
                            .push(format!("read a store of {} entries", outcome.value.len())),
                        Err(error) => failures.push(error.to_string()),
                    }
                }
                failures
            })
        };

        for index in 0..200 {
            let contents = if index % 2 == 0 { &new } else { &old };
            write_json(&path, contents).expect("write");
            std::thread::yield_now();
        }
        done.store(true, Ordering::Release);

        let failures = reader.join().expect("reader thread");
        assert!(
            failures.is_empty(),
            "{} reads did not see a complete store; first three: {:?}",
            failures.len(),
            &failures[..failures.len().min(3)]
        );
        assert!(
            reads.load(Ordering::Relaxed) >= 20,
            "the reader did not overlap enough writes to prove anything"
        );
        assert!(
            fs::read_dir(dir.path())
                .expect("list")
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp.")),
            "a successful write must consume its sibling"
        );
    }

    /// The reviewer's hardened layout: data directory `0555`, `auth.json` `0600`. An
    /// in-place truncate could refresh a token there; publication cannot, because it
    /// has to create a sibling. The refusal must name the directory that refused, and
    /// it must leave the credential that is already there alone.
    #[cfg(unix)]
    #[test]
    fn a_directory_that_refuses_a_new_file_is_named_and_the_old_credentials_survive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        write_json(&path, &sample()).expect("seed while the directory is still writable");

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555))
            .expect("harden the directory");

        // A privileged test process ignores directory permissions, so probe rather
        // than assume; the assertion below would otherwise pass for the wrong reason.
        let enforced = File::create(dir.path().join("probe")).is_err();

        let outcome = write_json(&path, &Map::from([("beta".to_owned(), "two".to_owned())]));

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("restore");

        if !enforced {
            assert!(
                outcome.is_ok(),
                "this process is not subject to the directory mode, so there is nothing to \
                 assert about the refusal"
            );
            return;
        }

        match outcome.expect_err("a 0555 directory cannot accept a sibling") {
            AuthError::Directory {
                path: named,
                directory,
                source,
            } => {
                assert_eq!(named, path, "the failure must name the credential file");
                assert_eq!(
                    directory,
                    dir.path(),
                    "and the directory that actually refused"
                );
                assert_eq!(source.kind(), ErrorKind::PermissionDenied);
            }
            other => panic!("a refusal to create must not look like a failed write: {other}"),
        }

        assert_eq!(
            read_json::<Map>(&path).expect("read").value,
            sample(),
            "a write that never started must leave the credentials that were there"
        );
        assert_eq!(
            fs::read_dir(dir.path()).expect("list").count(),
            1,
            "and must not leave a sibling behind"
        );
    }

    /// Publishing under the link's own name would replace a user's deliberate
    /// redirection with a regular file and leave the location they chose holding
    /// stale tokens.
    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_keeps_the_link_and_lands_on_its_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let target = elsewhere.path().join("vault.json");
        let link = dir.path().join("auth.json");
        std::os::unix::fs::symlink(&target, &link).expect("the user's redirection");

        write_json(&link, &sample()).expect("write through the link");

        assert!(
            fs::symlink_metadata(&link).expect("metadata").is_symlink(),
            "the link must survive the write"
        );
        assert_eq!(read_json::<Map>(&target).expect("read").value, sample());
        assert_eq!(mode_of(&target), CREDENTIAL_FILE_MODE);
        assert_eq!(
            fs::read_dir(dir.path()).expect("list").count(),
            1,
            "no sibling may be left beside the link"
        );
    }

    /// Two publication implementations exist for one reason — `replace` cannot
    /// create its temporary file private — and the Unix branch is the copy. Pin the
    /// properties that must not drift apart, because a divergence is how the Windows
    /// gap got in: the same resolved destination through a chain of links, the same
    /// surviving links, the same absence of a leftover temporary file, and the same
    /// refusal depth.
    #[cfg(unix)]
    #[test]
    fn the_unix_publication_and_the_workspace_primitive_agree_on_destination_and_cleanup() {
        assert_eq!(
            MAX_LINK_DEPTH,
            zuno_atomic_file::MAX_LINK_DEPTH,
            "the two publications must refuse the same chain"
        );

        for label in ["zuno-auth", "zuno-atomic-file"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let elsewhere = tempfile::tempdir().expect("elsewhere");
            let target = elsewhere.path().join("vault.json");
            let middle = dir.path().join("middle.json");
            let link = dir.path().join("auth.json");
            std::os::unix::fs::symlink(&target, &middle).expect("first link");
            std::os::unix::fs::symlink(&middle, &link).expect("second link");

            if label == "zuno-auth" {
                write_json(&link, &sample()).expect(label);
            } else {
                // The exact function `publish` calls off Unix, driven here so the
                // branch this host cannot execute is still covered.
                publish_through_workspace_primitive(&link, b"{\n  \"alpha\": \"one\"\n}")
                    .expect(label);
            }

            assert!(
                fs::symlink_metadata(&link).expect(label).is_symlink(),
                "{label}: the outer link must survive"
            );
            assert!(
                fs::symlink_metadata(&middle).expect(label).is_symlink(),
                "{label}: the inner link must survive"
            );
            assert_eq!(
                read_json::<Map>(&target).expect(label).value,
                sample(),
                "{label}: the chain must resolve to the same file"
            );
            assert_eq!(
                fs::read_dir(dir.path()).expect(label).count(),
                2,
                "{label}: no temporary may be left beside the links"
            );
            assert_eq!(
                fs::read_dir(elsewhere.path()).expect(label).count(),
                1,
                "{label}: no temporary may be left beside the target"
            );
        }
    }

    /// Why the split exists, asserted instead of argued. The workspace primitive
    /// opens its temporary file with default options, so on Unix the published
    /// credential file lands at whatever the umask allows — the same file the
    /// module docs refuse to create — while the local publication lands at `0600`
    /// whatever the umask is. Probed against a default-options file in the same
    /// directory so the assertion does not depend on this machine's umask.
    ///
    /// If `zuno-atomic-file` ever grows a mode-aware entry point, this test is the
    /// one that says the Unix branch may then be deleted.
    #[cfg(unix)]
    #[test]
    fn the_workspace_primitive_cannot_publish_a_private_file_on_unix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let probe = dir.path().join("probe");
        drop(File::create(&probe).expect("a file created with default options"));
        let umask_mode = mode_of(&probe);

        let delegated = dir.path().join("delegated.json");
        publish_through_workspace_primitive(&delegated, b"{}").expect("delegate");
        assert_eq!(
            mode_of(&delegated),
            umask_mode,
            "the primitive publishes at the umask, which is the window this module closes"
        );

        let local = dir.path().join("local.json");
        write_json(&local, &sample()).expect("write");
        assert_eq!(mode_of(&local), CREDENTIAL_FILE_MODE);

        if umask_mode == CREDENTIAL_FILE_MODE {
            // A umask of 0077 hides the difference; the assertion above still holds
            // and the divergence is only invisible on this machine.
            return;
        }
        assert_ne!(
            mode_of(&delegated),
            CREDENTIAL_FILE_MODE,
            "delegating on Unix would expose the user's refresh tokens"
        );
    }

    /// Absence has to be a conclusion, and on Windows the platform answers the
    /// ambiguous `ERROR_FILE_NOT_FOUND` for a file that will never exist as readily as
    /// for one being replaced. The budget spent before that resolves to absence is
    /// [`ABSENCE_CONFIRMATION`], and the ordinary first `zuno auth login` pays it in
    /// `thread::sleep` inside a synchronous function async callers reach —
    /// `zuno-mcp`'s `oauth::token::store_tokens` is one. So the bound is pinned in wall
    /// clock rather than only as a constant.
    #[test]
    fn an_absence_the_platform_will_not_confirm_settles_in_milliseconds_not_seconds() {
        let mut probes = 0;
        let started = std::time::Instant::now();
        let presence = confirm_presence(ABSENCE_CONFIRMATION, || {
            probes += 1;
            Ok(Probe::Unsettled)
        })
        .expect("a probe that cannot answer is not a failure");
        let spent = started.elapsed();

        assert_eq!(
            presence,
            Presence::Absent,
            "a first login on a machine with no credential file must not be denied"
        );
        assert!(
            probes >= 2,
            "the window must be given a retry, got {probes}"
        );
        assert!(
            spent < std::time::Duration::from_millis(250),
            "an absent credential file cost {spent:?}; the expected-presence policy in \
             zuno_atomic_file::metadata would have spent a full second here, on a tokio worker"
        );
        assert!(
            ABSENCE_CONFIRMATION <= std::time::Duration::from_millis(50),
            "this budget is spent in full on the ordinary first-login path, so it is a latency \
             bound and not only a correctness one"
        );
    }

    /// The other direction of the same bound: an answer about the file costs one probe
    /// and no sleep at all, so the budget is only ever paid by the ambiguous case.
    #[test]
    fn a_platform_that_answers_about_the_file_pays_nothing() {
        for (label, answer, expected) in [
            (
                "present",
                Probe::Settled(Presence::Present),
                Presence::Present,
            ),
            ("absent", Probe::Settled(Presence::Absent), Presence::Absent),
        ] {
            let mut probes = 0;
            let started = std::time::Instant::now();
            let presence = confirm_presence(std::time::Duration::from_secs(30), || {
                probes += 1;
                Ok(answer)
            })
            .expect(label);
            assert_eq!(presence, expected, "{label}");
            assert_eq!(probes, 1, "{label}");
            assert!(
                started.elapsed() < std::time::Duration::from_millis(250),
                "{label}"
            );
        }
    }

    /// A probe that fails for its own reasons is a failure, not an absence: a
    /// `PermissionDenied` on the containing directory must never resolve to "first
    /// login" and let the write publish a store holding one entry.
    #[test]
    fn a_probe_that_fails_for_its_own_reasons_is_not_an_absence() {
        let error = confirm_presence(ABSENCE_CONFIRMATION, || {
            Err(std::io::Error::from(ErrorKind::PermissionDenied))
        })
        .expect_err("a refused probe is not an answer");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }

    /// Which `zuno-atomic-file` entry point the presence probe is wired to cannot be
    /// observed on this host: off Windows both `metadata` and `metadata_optional`
    /// reduce to one syscall. The difference is a full second of `thread::sleep` on
    /// Windows for a file that legitimately does not exist. So the wiring is pinned as
    /// text here — the only mechanism that fails on this host — and in wall clock by
    /// the `cfg(windows)` test below, which runs in the `windows-test` job.
    #[test]
    fn the_presence_probe_is_a_plain_stat_and_not_an_expected_presence_entry_point() {
        let source = include_str!("store.rs");
        let body = source
            .split_once("fn presence_of(path: &Path)")
            .expect("presence_of must still exist")
            .1
            .split_once("\n}\n")
            .expect("a function body")
            .0;
        assert!(
            body.contains("fs::metadata(path)"),
            "the probe must be one plain stat: {body}"
        );
        assert!(
            !body.contains("zuno_atomic_file::"),
            "zuno_atomic_file::metadata implements an expected-presence policy that spends a \
             full second concluding that an absent credential file is absent, in a synchronous \
             function async callers reach: {body}"
        );
    }

    /// The ordinary first `zuno auth login` on Windows: no credential file, and the
    /// platform answers with the same raw error a `ReplaceFileW` in flight produces.
    /// This is the case that used to cost a second of `thread::sleep`.
    #[cfg(windows)]
    #[test]
    fn a_first_login_does_not_wait_a_second_for_a_file_that_never_existed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        let started = std::time::Instant::now();
        let outcome: Read<Map> = read_json_for_update(&path).expect("a first login");
        let spent = started.elapsed();

        assert!(outcome.value.is_empty());
        assert_eq!(outcome.damage, None);
        assert!(
            spent < std::time::Duration::from_millis(500),
            "every first login and every first MCP token store would pay {spent:?}"
        );
    }

    /// The off-Unix publication is the workspace primitive's name transition plus the
    /// only part of the power-loss boundary this module can still add from outside it.
    /// Driven on Unix under `cfg(test)` so the branch this host cannot execute is
    /// behaviour-tested: the document arrives, a flush failure never turns an
    /// already-published file into a reported failure, and no temporary is left behind.
    #[test]
    fn the_off_unix_publication_flushes_what_it_published_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        publish_off_unix(&path, b"{\n  \"alpha\": \"one\"\n}").expect("publish");
        assert_eq!(read_json::<Map>(&path).expect("read").value, sample());
        assert_eq!(
            fs::read_dir(dir.path()).expect("read_dir").count(),
            1,
            "an unpublished sibling is a spare copy of the user's credentials"
        );

        publish_off_unix(&path, b"{\n  \"beta\": \"two\"\n}").expect("republish");
        assert_eq!(
            read_json::<Map>(&path).expect("read").value,
            BTreeMap::from([("beta".to_owned(), "two".to_owned())])
        );
        assert_eq!(fs::read_dir(dir.path()).expect("read_dir").count(), 1);

        // Best effort, and best effort means it does not fail: the document is
        // published by the time the flush runs, so an error here would invite the
        // caller to write it a second time.
        flush_published(&dir.path().join("never-published.json"));
    }

    /// A newer Zuno adds a *field*, not only a whole entry. The shape declares which
    /// keys belong to this build; everything else is carried, at this level and inside
    /// a modelled object, and a key the type owns is never carried — otherwise
    /// clearing a field would resurrect it on the next write.
    #[test]
    fn unmodelled_fields_keeps_what_the_shape_does_not_name_and_nothing_else() {
        static INNER: Modelled = Modelled {
            keys: &["accessToken"],
            within: &[],
        };
        static SHAPE: Modelled = Modelled {
            keys: &["tokens", "serverUrl"],
            within: &[("tokens", &INNER)],
        };

        let raw: serde_json::Value = serde_json::from_str(
            r#"{
                "tokens": { "accessToken": "a", "tokenType": "DPoP" },
                "serverUrl": "https://example.test",
                "boundDevice": true,
                "dpopKey": { "kty": "EC" }
            }"#,
        )
        .expect("seed");
        let carried = unmodelled_fields(&raw, &SHAPE);
        assert!(!carried.is_empty());

        // Applied to a freshly serialized entry, the carried keys come back and the
        // typed ones win.
        let mut typed: serde_json::Value = serde_json::from_str(
            r#"{ "tokens": { "accessToken": "rotated" }, "serverUrl": "https://example.test" }"#,
        )
        .expect("typed");
        carried.apply(&mut typed);
        assert_eq!(typed["boundDevice"], serde_json::json!(true));
        assert_eq!(typed["dpopKey"]["kty"], serde_json::json!("EC"));
        assert_eq!(
            typed["tokens"]["tokenType"],
            serde_json::json!("DPoP"),
            "a carry inside a modelled object has to come back too"
        );
        assert_eq!(
            typed["tokens"]["accessToken"],
            serde_json::json!("rotated"),
            "the typed value owns every key its shape names"
        );

        // A modelled key the document held and the typed value cleared stays cleared,
        // and the carried keys that lived inside it go with it.
        let mut cleared: serde_json::Value =
            serde_json::from_str(r#"{ "serverUrl": "https://example.test" }"#).expect("cleared");
        carried.apply(&mut cleared);
        assert!(
            cleared.get("tokens").is_none(),
            "clearing a field must actually clear it: {cleared}"
        );
        assert_eq!(cleared["boundDevice"], serde_json::json!(true));

        // An entry that is not an object carries nothing: it did not decode either, so
        // it travels as `Rewritten::Verbatim` instead.
        assert!(unmodelled_fields(&serde_json::json!([1, 2]), &SHAPE).is_empty());
        assert!(unmodelled_fields(&serde_json::json!(null), &SHAPE).is_empty());

        // An entry a Zuno of this vintage wrote carries nothing at all, which is what
        // keeps its encoding byte-for-byte identical.
        let ours: serde_json::Value =
            serde_json::from_str(r#"{ "tokens": { "accessToken": "a" } }"#).expect("ours");
        assert!(unmodelled_fields(&ours, &SHAPE).is_empty());
    }

    /// `rewrite` merges three maps, and the precedence is what makes a decoded entry
    /// authoritative over the bytes it was read from while an undecodable entry
    /// survives untouched.
    #[test]
    fn rewrite_carries_verbatim_entries_and_unmodelled_fields_together() {
        static SHAPE: Modelled = Modelled {
            keys: &["alpha"],
            within: &[],
        };
        let decoded = BTreeMap::from([("known".to_owned(), sample())]);
        let raw: serde_json::Value =
            serde_json::from_str(r#"{ "alpha": "one", "future": 7 }"#).expect("raw");
        let unmodelled = BTreeMap::from([("known".to_owned(), unmodelled_fields(&raw, &SHAPE))]);
        let verbatim = BTreeMap::from([(
            "newer-zuno".to_owned(),
            serde_json::json!({ "type": "passkey", "counter": 7 }),
        )]);

        let document = rewrite(&decoded, &unmodelled, &verbatim);
        let rendered = serde_json::to_string(&document).expect("encode");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("decode");

        assert_eq!(parsed["known"]["alpha"], serde_json::json!("one"));
        assert_eq!(
            parsed["known"]["future"],
            serde_json::json!(7),
            "a field this build does not model must survive the write: {rendered}"
        );
        assert_eq!(parsed["newer-zuno"]["type"], serde_json::json!("passkey"));
        assert_eq!(parsed["newer-zuno"]["counter"], serde_json::json!(7));
    }

    /// The report latch is keyed by (kind of damage, file), so a frequent read of one
    /// broken file cannot spend the report that belongs to another file — or to a
    /// second kind of damage added later.
    #[test]
    fn the_damage_report_latch_is_keyed_per_file_and_per_kind() {
        let first = StoreDamage::Empty {
            path: PathBuf::from("/tmp/one-auth.json"),
        };
        let second = StoreDamage::Empty {
            path: PathBuf::from("/tmp/two-auth.json"),
        };
        assert_ne!(
            (first.kind(), first.path().to_path_buf()),
            (second.kind(), second.path().to_path_buf()),
            "two files must not share one report"
        );
        // Reporting is idempotent per key, and reporting one file must not consume the
        // other's report. Observed through the latch rather than the log sink, which
        // the capture test above owns.
        report_once(&first);
        report_once(&first);
        report_once(&second);
        let reported = REPORTED_DAMAGE.lock().expect("latch");
        let held = reported.as_ref().expect("something was reported");
        assert!(held.contains(&("empty", PathBuf::from("/tmp/one-auth.json"))));
        assert!(held.contains(&("empty", PathBuf::from("/tmp/two-auth.json"))));
    }

    #[test]
    fn malformed_json_is_a_typed_error_naming_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.json");
        fs::write(&path, b"{ not json").expect("seed");
        let error = read_json::<Map>(&path).expect_err("must fail");
        match &error {
            AuthError::Malformed { path: named, .. } => assert_eq!(named, &path),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_file_warns_and_still_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("creds.json");
        write_json(&path, &sample()).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let outcome: Read<Map> = read_json(&path).expect("read must still succeed");
        assert_eq!(outcome.value, sample(), "contents must survive the warning");
        let warning = outcome.permissions.expect("warning expected");
        assert_eq!(warning.path, path);
        assert_eq!(warning.mode, 0o644);
        assert_eq!(warning.offending_bits(), 0o044);
        assert!(warning.to_string().contains(&path.display().to_string()));

        // The mode has to be actionable in a debug dump, not decimal 420.
        let debugged = format!("{warning:?}");
        assert!(debugged.contains("0644"), "{debugged}");
        assert!(!debugged.contains("420"), "{debugged}");
    }

    #[cfg(unix)]
    #[test]
    fn only_group_and_other_bits_trigger_the_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("creds.json");
        write_json(&path, &sample()).expect("write");

        for (mode, expected) in [
            (0o600, false),
            (0o400, false),
            (0o700, false),
            (0o640, true),
            (0o604, true),
            (0o644, true),
            (0o660, true),
            (0o666, true),
            (0o601, true),
        ] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
            let outcome: Read<Map> = read_json(&path).expect("read");
            assert_eq!(
                outcome.is_permissive(),
                expected,
                "mode {mode:04o} should{} warn",
                if expected { "" } else { " not" }
            );
        }
    }

    /// The returned [`PermissionWarning`] is not the whole obligation: an
    /// operator who never inspects the value still has to be told. This captures
    /// the `tracing` output and asserts the event names the file and both modes.
    #[cfg(unix)]
    #[test]
    fn the_permission_warning_reaches_the_log_naming_the_file() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("lock").extend_from_slice(buffer);
                Ok(buffer.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        write_json(&path, &sample()).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let outcome: Read<Map> =
            tracing::subscriber::with_default(subscriber, || read_json(&path).expect("read"));
        assert!(outcome.is_permissive());

        let logged = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
        assert!(logged.contains("WARN"), "{logged}");
        assert!(logged.contains(&path.display().to_string()), "{logged}");
        assert!(logged.contains("0644"), "{logged}");
        assert!(logged.contains("0600"), "{logged}");
    }

    /// The credential must not ride along into the log with the warning.
    #[cfg(unix)]
    #[test]
    fn the_permission_warning_carries_no_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        write_json(
            &path,
            &BTreeMap::from([("openai".to_owned(), "sk-canary-value".to_owned())]),
        )
        .expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let outcome: Read<Map> = read_json(&path).expect("read");
        let warning = outcome.permissions.expect("warning");
        let rendered = format!("{warning} / {warning:?}");
        assert!(!rendered.contains("sk-canary-value"), "{rendered}");
    }

    #[test]
    fn round_trip_preserves_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("creds.json");
        let value = BTreeMap::from([
            ("alpha".to_owned(), "one".to_owned()),
            ("beta".to_owned(), "two".to_owned()),
        ]);
        write_json(&path, &value).expect("write");
        let outcome: Read<Map> = read_json(&path).expect("read");
        assert_eq!(outcome.value, value);
    }
}
