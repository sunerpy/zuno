//! The file primitives both credential stores share: a write that is never
//! world-readable, and a read that says so when the file already is.
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
//! It *then* also calls `set_permissions`, because `mode()` applies only when the
//! file is created and the oracle demonstrably repairs an existing permissive
//! file — observed against the 1.18.12 binary, which turned a `0644` `auth.json`
//! into `0600` on `opencode auth logout`.
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

/// What a read of a credential file produced.
///
/// The permission finding travels with the value instead of only reaching a log
/// sink, so a test — and a caller that wants to refuse to proceed — can act on
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read<T> {
    /// The decoded contents, or `T::default()` when the file does not exist.
    pub value: T,
    /// Set when the file was group- or world-accessible. Always `None` on
    /// platforms without Unix permission bits.
    pub permissions: Option<PermissionWarning>,
}

impl<T> Read<T> {
    /// Whether a permission problem was found.
    #[must_use]
    pub fn is_permissive(&self) -> bool {
        self.permissions.is_some()
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
/// in `.omo/notepads/opencode-rust/decisions.md`.
#[cfg(not(unix))]
fn permission_warning(_path: &Path, _file: &File) -> Result<Option<PermissionWarning>, AuthError> {
    Ok(None)
}

/// Apply [`CREDENTIAL_FILE_MODE`] to a file that already exists.
///
/// [`write_json`] creates with the mode already set, so this only matters when
/// the path was there beforehand at a laxer mode — the case the oracle repairs.
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

/// No-op on platforms without Unix permission bits.
#[cfg(not(unix))]
fn enforce_mode(_path: &Path, _file: &File) -> Result<(), AuthError> {
    Ok(())
}

/// Open a credential file for writing, creating it at
/// [`CREDENTIAL_FILE_MODE`] so its contents are never briefly world-readable.
#[cfg(unix)]
fn open_for_write(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(CREDENTIAL_FILE_MODE)
        .open(path)
}

/// Open a credential file for writing. Without Unix modes the file inherits the
/// directory ACL.
#[cfg(not(unix))]
fn open_for_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

/// Read and decode a credential file.
///
/// - Absent file → `T::default()` and no warning. The oracle treats a missing
///   `auth.json` as an empty record (`auth/index.ts:65`), and so does this.
/// - Present but group- or world-accessible → decoded normally, with a
///   [`PermissionWarning`] returned and a `tracing::warn!` emitted that names the
///   path.
/// - Unreadable → [`AuthError::Read`].
/// - Present but not valid JSON → [`AuthError::Malformed`].
///
/// The last case is a deliberate divergence. The oracle pipes every read failure
/// into `orElseSucceed(() => ({}))`, so a truncated `auth.json` reads as empty
/// and the next `set` writes that emptiness back — silently destroying every
/// credential in the file. Surfacing it instead gives the caller the chance not
/// to. See the task-24 entry in `.omo/notepads/opencode-rust/decisions.md`.
pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<Read<T>, AuthError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == ErrorKind::NotFound => {
            return Ok(Read {
                value: T::default(),
                permissions: None,
            });
        }
        Err(source) => {
            return Err(AuthError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };

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
        .map_err(|source| AuthError::Read {
            path: path.to_path_buf(),
            source,
        })?;

    // An empty file is an empty store, not malformed JSON. It is what a crash
    // between `create` and `write` leaves behind.
    if text.trim().is_empty() {
        return Ok(Read {
            value: T::default(),
            permissions,
        });
    }

    let value = serde_json::from_str(&text).map_err(|source| AuthError::Malformed {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Read { value, permissions })
}

/// Serialize `value` and write it to `path` at [`CREDENTIAL_FILE_MODE`].
///
/// The encoding is `JSON.stringify(value, null, 2)`: two-space indent and **no**
/// trailing newline. Both were confirmed byte-for-byte against a file the 1.18.12
/// binary wrote.
///
/// Missing parent directories are created. The oracle relies on its startup
/// having already created the data directory; a store used on its own cannot.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AuthError> {
    let body = serde_json::to_vec_pretty(value).map_err(|source| AuthError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| AuthError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }

    let mut file = open_for_write(path).map_err(|source| AuthError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    // Repairs a pre-existing file, whose mode `open` leaves untouched.
    enforce_mode(path, &file)?;
    file.write_all(&body).map_err(|source| AuthError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    file.flush().map_err(|source| AuthError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    #[test]
    fn an_empty_file_reads_as_the_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.json");
        fs::write(&path, b"   \n").expect("seed");
        let outcome: Read<Map> = read_json(&path).expect("read");
        assert!(outcome.value.is_empty());
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
