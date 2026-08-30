//! The one place in this crate that writes to the filesystem.
//!
//! `packages/core/src/global.ts:35-43` creates seven directories at module
//! import:
//!
//! ```text
//! await Promise.all([
//!   fs.mkdir(Path.data,   { recursive: true }),
//!   fs.mkdir(Path.config, { recursive: true }),
//!   fs.mkdir(Path.state,  { recursive: true }),
//!   fs.mkdir(Path.tmp,    { recursive: true }),
//!   fs.mkdir(Path.log,    { recursive: true }),
//!   fs.mkdir(Path.bin,    { recursive: true }),
//!   fs.mkdir(Path.repos,  { recursive: true }),
//! ])
//! ```
//!
//! The set is reproduced here, the timing is not: nothing is created until a
//! caller asks. Two consequences worth knowing before relying on either
//! behaviour.
//!
//! **`cache()` is not in the list.** It exists only because `bin()` is created
//! beneath it, and `snapshot/`, `tool-output/` and the database are likewise
//! absent — their owners create them on demand. Adding `cache()` explicitly
//! would be inventing behaviour, so the list stays at seven.
//!
//! **The oracle's timing is observable.** `TMPDIR=/ opencode debug paths` prints
//! nothing and exits 1 with `EACCES: permission denied, mkdir '/opencode'`,
//! because import-time creation runs before the command that only wanted to
//! *print* paths. Pure getters cannot fail that way.

use std::path::{Path, PathBuf};

use crate::Layout;

/// A filesystem failure while creating the layout.
///
/// Local to this crate rather than added to `zuno-error`: that crate is already
/// committed and owned by another todo, and a path failure has exactly one
/// shape. Named `CreateDirectory` with the path attached so a caller can report
/// *which* directory it could not create — an `io::Error` alone cannot.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    /// A directory could not be created.
    #[error("failed to create directory {}", path.display())]
    CreateDirectory {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
}

impl Layout {
    /// The seven directories [`Layout::ensure`] creates, in the oracle's order.
    #[must_use]
    pub fn ensured_directories(&self) -> [&Path; 7] {
        [
            self.data(),
            self.config(),
            self.state(),
            self.temp(),
            self.log(),
            self.bin(),
            self.repos(),
        ]
    }

    /// Create every directory in [`Layout::ensured_directories`].
    ///
    /// Idempotent — `create_dir_all` succeeds on an existing directory — so this
    /// is safe to call at every startup, which is how the oracle behaves.
    ///
    /// # Errors
    ///
    /// [`PathsError::CreateDirectory`] naming the first directory that could not
    /// be created. Creation stops there rather than continuing; a layout that is
    /// half-present is not a state any consumer should have to reason about.
    pub fn ensure(&self) -> Result<(), PathsError> {
        for directory in self.ensured_directories() {
            std::fs::create_dir_all(directory).map_err(|source| PathsError::CreateDirectory {
                path: directory.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{
        Env, HOME, TMPDIR, XDG_CACHE_HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME,
    };

    fn isolated(root: &Path) -> Layout {
        let base = |name: &str| root.join(name).to_string_lossy().into_owned();
        let env = Env::empty()
            .with(HOME, base("home"))
            .with(XDG_DATA_HOME, base("data"))
            .with(XDG_CACHE_HOME, base("cache"))
            .with(XDG_CONFIG_HOME, base("config"))
            .with(XDG_STATE_HOME, base("state"))
            .with(TMPDIR, base("tmp"));
        Layout::resolve_with(&env, None)
    }

    /// The property the whole crate rests on: asking for a path never touches
    /// the filesystem.
    #[test]
    fn getters_create_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = isolated(root.path());

        let probed: Vec<PathBuf> = layout
            .entries()
            .iter()
            .map(|(_, path)| path.to_path_buf())
            .chain([
                layout.snapshot_root(),
                layout.snapshot_store("global", Path::new("/repo")),
                layout.tool_output(),
                layout.auth_file(),
                layout.mcp_auth_file(),
                layout.models_cache(),
                layout.effective_config().to_path_buf(),
            ])
            .chain(layout.db_path().as_path().map(Path::to_path_buf))
            .collect();

        for path in &probed {
            assert!(!path.exists(), "{} was created by a getter", path.display());
        }
        assert_eq!(
            std::fs::read_dir(root.path()).expect("read root").count(),
            0,
            "a getter created something under the temp root"
        );
    }

    #[test]
    fn ensure_creates_exactly_the_seven_oracle_directories() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = isolated(root.path());
        layout.ensure().expect("ensure");

        for directory in layout.ensured_directories() {
            assert!(directory.is_dir(), "{} missing", directory.display());
        }
        // `cache()` exists only as `bin()`'s parent, and nothing else is made.
        assert!(layout.cache().is_dir());
        assert!(!layout.snapshot_root().exists());
        assert!(!layout.tool_output().exists());
        assert!(!layout.auth_file().exists());
    }

    #[test]
    fn ensure_is_idempotent() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = isolated(root.path());
        layout.ensure().expect("first ensure");
        layout.ensure().expect("second ensure");
        assert!(layout.data().is_dir());
    }

    #[test]
    fn ensure_reports_which_directory_failed() {
        let root = tempfile::tempdir().expect("tempdir");
        // A regular file where `data()` must be a directory makes create_dir_all
        // fail with NotADirectory / AlreadyExists.
        let data_base = root.path().join("data");
        std::fs::create_dir_all(&data_base).expect("create data base");
        std::fs::write(data_base.join("zuno"), "not a directory").expect("write blocker");

        let layout = isolated(root.path());
        let error = layout.ensure().expect_err("ensure must fail");
        let PathsError::CreateDirectory { path, source } = &error;
        assert_eq!(path, layout.data());
        assert!(
            error
                .to_string()
                .contains(&layout.data().display().to_string()),
            "{error}"
        );
        assert!(std::error::Error::source(&error).is_some());
        assert!(!source.to_string().is_empty());
    }
}
