//! Filesystem layout resolution: project root, data dir, cache dir, and
//! per-worktree state paths.
//!
//! # What this crate promises
//!
//! Zuno owns its configuration and data directories. Production path resolution
//! does not read, write, probe, or migrate OpenCode directories.
//!
//! ```text
//! $XDG_DATA_HOME/zuno              data()
//!   ├── auth.json                  auth_file()
//!   ├── mcp-auth.json              mcp_auth_file()
//!   ├── zuno.db                    db_path()          (release channels)
//!   ├── zuno-<channel>.db          db_path()          (preview / local)
//!   ├── log/                       log()
//!   ├── repos/                     repos()
//!   ├── snapshot/                  snapshot_root()
//!   │   └── <projectID>/<sha1(worktree)>/   snapshot_store()
//!   └── tool-output/               tool_output()
//! $XDG_CACHE_HOME/zuno             cache()
//!   ├── bin/                       bin()
//!   └── models.json                models_cache()
//! $XDG_CONFIG_HOME/zuno            config()
//! $XDG_STATE_HOME/zuno             state()
//! <os.tmpdir()>/zuno               temp()
//! ```
//!
//! # Two rules, and why they are not negotiable
//!
//! **Path getters are pure.** No getter creates a directory, and no getter
//! touches the filesystem at all. The oracle does the opposite — `global.ts`
//! `mkdir`s seven directories at import — so creation lives in exactly one
//! place, [`Layout::ensure`]. This is a deliberate divergence a differential
//! test cannot detect, and it is recorded in
//! `.omo/notepads/opencode-rust/decisions.md`.
//!
//! **Joining is Node's, not Rust's.** `path.join` normalizes; `PathBuf::push`
//! concatenates. With `XDG_DATA_HOME=/tmp/x/../y` the oracle reports
//! `/tmp/y/zuno` and a `PathBuf::join` would report `/tmp/x/../y/zuno`
//! — a different directory as far as a string-keyed database row is concerned.
//! See [`node_path`].
//!
//! # Getting a layout
//!
//! The free functions ([`data`], [`cache`], …) read the process environment once
//! into a cached [`Layout`], which mirrors the oracle's compute-at-import
//! timing and is what production code should use:
//!
//! ```
//! let db = zuno_paths::db_path();
//! assert!(db.as_oracle_string().ends_with(".db") || db.is_memory());
//! ```
//!
//! Tests build a [`Layout`] from an explicit [`Env`] instead, because
//! `std::env::set_var` is `unsafe` and this workspace forbids `unsafe_code`:
//!
//! ```
//! use zuno_paths::{Env, Layout, env::{HOME, XDG_DATA_HOME}};
//! use std::path::Path;
//!
//! let env = Env::empty().with(HOME, "/home/u").with(XDG_DATA_HOME, "/srv/data");
//! let layout = Layout::resolve_with(&env, None);
//! assert_eq!(layout.data(), Path::new("/srv/data/zuno"));
//! assert_eq!(layout.log(), Path::new("/srv/data/zuno/log"));
//! ```

pub mod config_chain;
pub mod ensure;
pub mod env;
pub mod files;
pub mod layout;
pub mod node_path;
pub mod project;
pub mod sha1;
pub mod walk;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use crate::config_chain::{CONFIG_FILE_STEM, PROJECT_CONFIG_DIRECTORY, PROJECT_DIRECTORY};
pub use crate::ensure::PathsError;
pub use crate::env::Env;
pub use crate::files::{
    AUTH_FILE, DEFAULT_DB_FILE, DEFAULT_MODELS_FILE, DEFAULT_MODELS_SOURCE, DbLocation,
    MCP_AUTH_FILE, MEMORY_SENTINEL, SNAPSHOT_DIRECTORY, TOOL_OUTPUT_DIRECTORY,
    installation_channel,
};
pub use crate::layout::{APP, DEBUG_PATHS_KEYS, Layout};
pub use crate::project::{GLOBAL_PROJECT_ID, Repository, ResolvedProject, Vcs};

/// The process-wide layout, resolved from the environment on first use.
///
/// Resolved once and cached, which is the same timing as the oracle's
/// module-level constants. Because mutating the environment is `unsafe` and
/// forbidden here, the cache cannot go stale within a process.
pub fn global() -> &'static Layout {
    static GLOBAL: OnceLock<Layout> = OnceLock::new();
    GLOBAL.get_or_init(Layout::from_process_env)
}

/// `Global.Path.home`.
pub fn home() -> &'static Path {
    global().home()
}

/// `Global.Path.data` — `$XDG_DATA_HOME/zuno`.
pub fn data() -> &'static Path {
    global().data()
}

/// `Global.Path.cache` — `$XDG_CACHE_HOME/zuno`.
pub fn cache() -> &'static Path {
    global().cache()
}

/// `Global.Path.config` — `$XDG_CONFIG_HOME/zuno`, before any
/// `ZUNO_CONFIG_DIR` override.
pub fn config() -> &'static Path {
    global().config()
}

/// `Global.Path.state` — `$XDG_STATE_HOME/zuno`.
pub fn state() -> &'static Path {
    global().state()
}

/// `Global.Path.tmp` — `<os.tmpdir()>/zuno`.
pub fn temp() -> &'static Path {
    global().temp()
}

/// `Global.Path.log` — `data()/log`.
pub fn log() -> &'static Path {
    global().log()
}

/// `Global.Path.bin` — `cache()/bin`.
pub fn bin() -> &'static Path {
    global().bin()
}

/// `Global.Path.repos` — `data()/repos`.
pub fn repos() -> &'static Path {
    global().repos()
}

/// The configuration directory the service uses, honouring
/// `ZUNO_CONFIG_DIR`.
pub fn effective_config() -> &'static Path {
    global().effective_config()
}

/// `data()/snapshot`.
#[must_use]
pub fn snapshot_root() -> PathBuf {
    global().snapshot_root()
}

/// `data()/snapshot/<project_id>/<sha1(worktree)>`.
#[must_use]
pub fn snapshot_store(project_id: &str, worktree: &Path) -> PathBuf {
    global().snapshot_store(project_id, worktree)
}

/// `data()/tool-output`.
#[must_use]
pub fn tool_output() -> PathBuf {
    global().tool_output()
}

/// `data()/auth.json`.
#[must_use]
pub fn auth_file() -> PathBuf {
    global().auth_file()
}

/// `data()/mcp-auth.json`.
#[must_use]
pub fn mcp_auth_file() -> PathBuf {
    global().mcp_auth_file()
}

/// The model catalog cache for the configured source.
#[must_use]
pub fn models_cache() -> PathBuf {
    global().models_cache()
}

/// Where the session database lives, honouring `ZUNO_DB`.
#[must_use]
pub fn db_path() -> DbLocation {
    global().db_path()
}

/// The configuration directory chain for `directory`, bounded by `worktree`.
#[must_use]
pub fn config_directories(directory: &Path, worktree: Option<&Path>) -> Vec<PathBuf> {
    global().config_directories(directory, worktree)
}

/// The configuration file chain for `directory`, outermost first.
#[must_use]
pub fn config_files(name: &str, directory: &Path, worktree: Option<&Path>) -> Vec<PathBuf> {
    Layout::config_files(name, directory, worktree)
}

/// Create the seven directories the oracle creates at import.
///
/// # Errors
///
/// [`PathsError::CreateDirectory`] naming the first directory that failed.
pub fn ensure() -> Result<(), PathsError> {
    global().ensure()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every free function must agree with the cached layout it delegates to;
    /// a copy-paste slip between two getters is otherwise invisible.
    #[test]
    fn free_functions_delegate_to_the_cached_layout() {
        let layout = global();
        assert_eq!(home(), layout.home());
        assert_eq!(data(), layout.data());
        assert_eq!(cache(), layout.cache());
        assert_eq!(config(), layout.config());
        assert_eq!(state(), layout.state());
        assert_eq!(temp(), layout.temp());
        assert_eq!(log(), layout.log());
        assert_eq!(bin(), layout.bin());
        assert_eq!(repos(), layout.repos());
        assert_eq!(effective_config(), layout.effective_config());
        assert_eq!(snapshot_root(), layout.snapshot_root());
        assert_eq!(tool_output(), layout.tool_output());
        assert_eq!(auth_file(), layout.auth_file());
        assert_eq!(mcp_auth_file(), layout.mcp_auth_file());
        assert_eq!(models_cache(), layout.models_cache());
        assert_eq!(db_path(), layout.db_path());
        assert_eq!(
            snapshot_store("global", Path::new("/repo")),
            layout.snapshot_store("global", Path::new("/repo"))
        );
    }

    #[test]
    fn global_is_cached() {
        assert!(std::ptr::eq(global(), global()));
    }

    /// The derived paths must actually sit under the directories they are
    /// documented to sit under. This catches a getter pointed at the wrong base
    /// — the failure mode that would send sessions to a directory the TypeScript
    /// binary never reads.
    #[test]
    fn derived_paths_sit_under_their_documented_base() {
        let layout = global();
        for path in [
            layout.log().to_path_buf(),
            layout.repos().to_path_buf(),
            layout.snapshot_root(),
            layout.tool_output(),
            layout.auth_file(),
            layout.mcp_auth_file(),
        ] {
            assert!(
                path.starts_with(layout.data()),
                "{} not under data()",
                path.display()
            );
        }
        assert!(layout.bin().starts_with(layout.cache()));
        assert!(layout.models_cache().starts_with(layout.cache()));
    }

    #[test]
    fn re_exports_are_reachable() {
        assert_eq!(APP, "zuno");
        assert_eq!(PROJECT_CONFIG_DIRECTORY, ".zuno");
        assert_eq!(GLOBAL_PROJECT_ID, "global");
        assert_eq!(AUTH_FILE, "auth.json");
        assert_eq!(MCP_AUTH_FILE, "mcp-auth.json");
        assert_eq!(SNAPSHOT_DIRECTORY, "snapshot");
        assert_eq!(TOOL_OUTPUT_DIRECTORY, "tool-output");
        assert_eq!(DEFAULT_DB_FILE, "zuno.db");
        assert_eq!(DEFAULT_MODELS_FILE, "models.json");
        assert_eq!(DEFAULT_MODELS_SOURCE, "https://models.dev");
        assert_eq!(MEMORY_SENTINEL, ":memory:");
        assert_eq!(DEBUG_PATHS_KEYS.len(), 9);
        assert!(!installation_channel().is_empty());
    }

    #[test]
    fn config_chain_free_functions_delegate() {
        let layout = global();
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(
            config_directories(&cwd, Some(&cwd)),
            layout.config_directories(&cwd, Some(&cwd))
        );
        assert_eq!(
            config_files(CONFIG_FILE_STEM, &cwd, Some(&cwd)),
            Layout::config_files(CONFIG_FILE_STEM, &cwd, Some(&cwd))
        );
    }
}
