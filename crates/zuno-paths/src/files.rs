//! The individual files and subdirectories the layout hosts.
//!
//! Each item below is a direct port, with the oracle line it comes from:
//!
//! | item | oracle |
//! | --- | --- |
//! | `snapshot/<projectID>/<sha1(worktree)>` | `packages/opencode/src/snapshot/index.ts:71` |
//! | `tool-output` | `packages/core/src/tool-output-store.ts:17`, `:118` |
//! | `auth.json` | `packages/opencode/src/auth/index.ts:10` |
//! | `mcp-auth.json` | `packages/opencode/src/mcp/auth.ts:37` |
//! | `models.json` / `models-<sha1(source)>.json` | `packages/core/src/models-dev.ts:161-164` |
//! | `zuno.db` / `zuno-<channel>.db` | Zuno session database |

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::Layout;
use crate::node_path;
use crate::sha1;

/// The per-project snapshot store directory under `data()`.
pub const SNAPSHOT_DIRECTORY: &str = "snapshot";

/// The tool-output spill directory under `data()` —
/// `ToolOutputStore.MANAGED_DIRECTORY`.
pub const TOOL_OUTPUT_DIRECTORY: &str = "tool-output";

/// The provider credential file under `data()`.
pub const AUTH_FILE: &str = "auth.json";

/// The MCP OAuth credential file under `data()`.
pub const MCP_AUTH_FILE: &str = "mcp-auth.json";

/// The model catalog source that gets the unsuffixed cache file name.
pub const DEFAULT_MODELS_SOURCE: &str = "https://models.opencode.ai";

/// The cache file name used for [`DEFAULT_MODELS_SOURCE`].
pub const DEFAULT_MODELS_FILE: &str = "models.json";

/// The database file name used on release channels.
pub const DEFAULT_DB_FILE: &str = "zuno.db";

/// The filename used by Zuno before its independent-project rename completed.
pub const LEGACY_DB_FILE: &str = "opencode.db";

/// The channels that get [`DEFAULT_DB_FILE`] rather than a suffixed name.
pub const UNSUFFIXED_DB_CHANNELS: [&str; 3] = ["latest", "beta", "prod"];

/// The channel a build without a `ZUNO_CHANNEL` define reports.
pub const LOCAL_CHANNEL: &str = "local";

/// The literal `ZUNO_DB` value that selects an in-memory database.
pub const MEMORY_SENTINEL: &str = ":memory:";

/// Where the session database lives.
///
/// A plain [`PathBuf`] cannot express the oracle's third case: `OPENCODE_DB`
/// may be the literal `":memory:"`, which SQLite treats as a sentinel rather
/// than a filename. Modelling that as a variant means a consumer cannot
/// accidentally `create_dir_all` its parent — which is precisely the mistake the
/// string form invites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DbLocation {
    /// `ZUNO_DB=:memory:` — a transient, per-process database.
    Memory,
    /// A file on disk.
    File(PathBuf),
}

impl DbLocation {
    /// The exact string the oracle's `Database.path()` would return.
    ///
    /// Used by the differential harness, and by any consumer handing the value
    /// to a SQLite driver that takes a filename.
    #[must_use]
    pub fn as_oracle_string(&self) -> Cow<'_, str> {
        match self {
            Self::Memory => Cow::Borrowed(MEMORY_SENTINEL),
            Self::File(path) => path.to_string_lossy(),
        }
    }

    /// The on-disk path, or `None` for [`DbLocation::Memory`].
    #[must_use]
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::Memory => None,
            Self::File(path) => Some(path),
        }
    }

    /// Whether this is the in-memory sentinel.
    #[must_use]
    pub fn is_memory(&self) -> bool {
        matches!(self, Self::Memory)
    }
}

/// The channel this binary was built for.
///
/// Uses the build-time `ZUNO_CHANNEL` value when present, otherwise `"local"`.
///
/// A build that does **not** set it reports `"local"` and consequently uses
/// `zuno-local.db`. This means a `cargo run` build reads a different database
/// than the installed binary. See
/// `.omo/notepads/opencode-rust/issues.md`.
#[must_use]
pub fn installation_channel() -> &'static str {
    match option_env!("ZUNO_CHANNEL") {
        Some(channel) => channel,
        None => LOCAL_CHANNEL,
    }
}

/// Port of `InstallationChannel.replace(/[^a-zA-Z0-9._-]/g, "-")`.
///
/// A channel is a Git branch name for preview builds, so it can contain `/`
/// and other characters that must not leak into a filename.
#[must_use]
pub fn sanitize_channel(channel: &str) -> String {
    channel
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

impl Layout {
    /// `data()/snapshot` — the parent of every per-project snapshot store.
    #[must_use]
    pub fn snapshot_root(&self) -> PathBuf {
        PathBuf::from(node_path::join(
            &self.data().to_string_lossy(),
            SNAPSHOT_DIRECTORY,
        ))
    }

    /// `data()/snapshot/<project_id>/<sha1(worktree)>`.
    ///
    /// The hash is `Hash.fast(ctx.worktree)` — SHA-1 hex over the worktree path
    /// string, **not** over a canonicalized or trailing-slash-normalized form.
    /// Two spellings of the same directory therefore produce two stores in the
    /// oracle, and reproducing that is required for an existing store to be
    /// found. Use [`Layout::worktree_hash`] to compute the component alone.
    #[must_use]
    pub fn snapshot_store(&self, project_id: &str, worktree: &Path) -> PathBuf {
        let root = self.snapshot_root();
        PathBuf::from(node_path::join_all([
            root.to_string_lossy().as_ref(),
            project_id,
            &Self::worktree_hash(worktree),
        ]))
    }

    /// `Hash.fast(worktree)` — the leaf component of a snapshot store path.
    #[must_use]
    pub fn worktree_hash(worktree: &Path) -> String {
        sha1::hex(worktree.to_string_lossy().as_bytes())
    }

    /// `data()/tool-output`.
    #[must_use]
    pub fn tool_output(&self) -> PathBuf {
        PathBuf::from(node_path::join(
            &self.data().to_string_lossy(),
            TOOL_OUTPUT_DIRECTORY,
        ))
    }

    /// `data()/auth.json`.
    #[must_use]
    pub fn auth_file(&self) -> PathBuf {
        PathBuf::from(node_path::join(&self.data().to_string_lossy(), AUTH_FILE))
    }

    /// `data()/mcp-auth.json`.
    #[must_use]
    pub fn mcp_auth_file(&self) -> PathBuf {
        PathBuf::from(node_path::join(
            &self.data().to_string_lossy(),
            MCP_AUTH_FILE,
        ))
    }

    /// The model catalog cache for [`Layout::models_source`].
    #[must_use]
    pub fn models_cache(&self) -> PathBuf {
        self.models_cache_for_source(self.models_source())
    }

    /// The model catalog cache for an explicit source.
    ///
    /// `cache()/models.json` for the default source, and
    /// `cache()/models-<sha1(source)>.json` for anything else — so pointing
    /// `OPENCODE_MODELS_URL` at a mirror cannot poison the default cache.
    #[must_use]
    pub fn models_cache_for_source(&self, source: &str) -> PathBuf {
        let file = if source == DEFAULT_MODELS_SOURCE {
            DEFAULT_MODELS_FILE.to_owned()
        } else {
            format!("models-{}.json", sha1::hex(source.as_bytes()))
        };
        PathBuf::from(node_path::join(&self.cache().to_string_lossy(), &file))
    }

    /// Where the session database lives, for the channel this binary was built
    /// for.
    #[must_use]
    pub fn db_path(&self) -> DbLocation {
        self.db_path_for_channel(installation_channel())
    }

    /// Where the session database lives for an explicit channel.
    ///
    /// The resolution order is:
    ///
    /// 1. `ZUNO_DB` set and equal to `":memory:"` → in memory.
    /// 2. `ZUNO_DB` set and absolute → that path, verbatim.
    /// 3. `ZUNO_DB` set and relative → **joined onto `data()`**, not onto
    ///    the working directory. Verified against the 1.18.12 binary: with
    ///    `XDG_DATA_HOME=/tmp/dbprobe/xdg` and `ZUNO_DB=relprobe.db`, the
    ///    file appears at `/tmp/dbprobe/xdg/zuno/relprobe.db` while the
    ///    working directory stayed empty.
    /// 4. Release channel, or `ZUNO_DISABLE_CHANNEL_DB` exactly `1`/`true`
    ///    → `data()/zuno.db`.
    /// 5. Otherwise → `data()/zuno-<sanitized channel>.db`.
    #[must_use]
    pub fn db_path_for_channel(&self, channel: &str) -> DbLocation {
        let data = self.data().to_string_lossy();
        if let Some(override_value) = self.db_override() {
            if override_value == MEMORY_SENTINEL {
                return DbLocation::Memory;
            }
            if node_path::is_absolute(override_value) {
                return DbLocation::File(PathBuf::from(override_value));
            }
            return DbLocation::File(PathBuf::from(node_path::join(&data, override_value)));
        }
        if UNSUFFIXED_DB_CHANNELS.contains(&channel) || self.channel_db_disabled() {
            return DbLocation::File(PathBuf::from(node_path::join(&data, DEFAULT_DB_FILE)));
        }
        let file = format!("zuno-{}.db", sanitize_channel(channel));
        DbLocation::File(PathBuf::from(node_path::join(&data, &file)))
    }
}

/// The pre-rename Zuno database filename corresponding to `path`.
///
/// Arbitrary `ZUNO_DB` names return `None`; only the two default filename forms
/// participate in the hard-cut diagnostic.
#[must_use]
pub fn legacy_db_path(path: &Path) -> Option<PathBuf> {
    let file = path.file_name()?.to_str()?;
    let legacy = if file == DEFAULT_DB_FILE {
        LEGACY_DB_FILE.to_owned()
    } else {
        format!("opencode-{}", file.strip_prefix("zuno-")?)
    };
    Some(path.with_file_name(legacy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{Env, HOME, ZUNO_DB, ZUNO_DISABLE_CHANNEL_DB, ZUNO_MODELS_URL};

    fn layout(pairs: &[(&str, &str)]) -> Layout {
        Layout::resolve_with(&Env::from_pairs(pairs.iter().copied()), None)
    }

    fn base() -> Layout {
        layout(&[(HOME, "/config")])
    }

    #[test]
    fn artifact_paths_hang_off_the_data_directory() {
        let resolved = base();
        let data = Path::new("/config/.local/share/zuno");
        assert_eq!(resolved.snapshot_root(), data.join("snapshot"));
        assert_eq!(resolved.tool_output(), data.join("tool-output"));
        assert_eq!(resolved.auth_file(), data.join("auth.json"));
        assert_eq!(resolved.mcp_auth_file(), data.join("mcp-auth.json"));
    }

    /// The hash is SHA-1 over the worktree string. Every expected digest here
    /// came from coreutils `sha1sum`, independently of this crate's SHA-1.
    #[test]
    fn snapshot_store_uses_sha1_of_the_worktree_string() {
        let resolved = base();
        let worktree = Path::new("/config/workspace/ProdDir/AI/opencode-rust");
        let hash = Layout::worktree_hash(worktree);
        assert_eq!(hash, "0714ccfc127950dd77bb82077e308e9400a11189");
        assert_eq!(
            resolved.snapshot_store("global", worktree),
            Path::new(
                "/config/.local/share/zuno/snapshot/global/0714ccfc127950dd77bb82077e308e9400a11189"
            )
        );
        assert_eq!(
            resolved.snapshot_store("83630750896a66f949c084b8d0e97c1f692b3608", worktree),
            resolved
                .snapshot_root()
                .join("83630750896a66f949c084b8d0e97c1f692b3608")
                .join(&hash)
        );
    }

    /// Two spellings of one directory must hash differently, because the oracle
    /// hashes the raw string and a consumer relying on canonicalization would
    /// silently fail to find an existing store.
    #[test]
    fn worktree_hash_is_not_normalized() {
        assert_eq!(
            Layout::worktree_hash(Path::new("/repo")),
            "83630750896a66f949c084b8d0e97c1f692b3608"
        );
        assert_eq!(
            Layout::worktree_hash(Path::new("/repo/")),
            "9feece9c0dfe9efe2cb209e4c589790fd731e71a"
        );
    }

    #[test]
    fn models_cache_suffixes_only_non_default_sources() {
        let resolved = base();
        let cache = Path::new("/config/.cache/zuno");
        assert_eq!(resolved.models_cache(), cache.join("models.json"));
        assert_eq!(
            resolved.models_cache_for_source(DEFAULT_MODELS_SOURCE),
            cache.join("models.json")
        );

        let mirror = "https://models.example.test";
        let expected = format!("models-{}.json", crate::sha1::hex(mirror.as_bytes()));
        assert_eq!(
            resolved.models_cache_for_source(mirror),
            cache.join(&expected)
        );

        let overridden = layout(&[(HOME, "/config"), (ZUNO_MODELS_URL, mirror)]);
        assert_eq!(overridden.models_cache(), cache.join(&expected));
    }

    #[test]
    fn db_override_memory_sentinel() {
        let resolved = layout(&[(HOME, "/config"), (ZUNO_DB, MEMORY_SENTINEL)]);
        let location = resolved.db_path_for_channel("latest");
        assert_eq!(location, DbLocation::Memory);
        assert!(location.is_memory());
        assert_eq!(location.as_path(), None);
        assert_eq!(location.as_oracle_string(), ":memory:");
    }

    #[test]
    fn db_override_absolute_is_used_verbatim() {
        let resolved = layout(&[(HOME, "/config"), (ZUNO_DB, "/var/lib/oc/custom.db")]);
        assert_eq!(
            resolved.db_path_for_channel("latest"),
            DbLocation::File(PathBuf::from("/var/lib/oc/custom.db"))
        );
    }

    /// The trap: a relative `OPENCODE_DB` resolves under `data()`, never under
    /// the working directory.
    #[test]
    fn db_override_relative_resolves_under_data_not_cwd() {
        let resolved = layout(&[(HOME, "/config"), (ZUNO_DB, "relprobe.db")]);
        let location = resolved.db_path_for_channel("latest");
        assert_eq!(
            location,
            DbLocation::File(PathBuf::from("/config/.local/share/zuno/relprobe.db"))
        );
        let path = location.as_path().expect("file location");
        assert!(
            path.starts_with(resolved.data()),
            "{} not under data()",
            path.display()
        );
        assert_ne!(path, Path::new("relprobe.db"));

        // Nested and dot-segmented relative values normalize the Node way.
        let nested = layout(&[(HOME, "/config"), (ZUNO_DB, "sub/../db/oc.db")]);
        assert_eq!(
            nested.db_path_for_channel("latest"),
            DbLocation::File(PathBuf::from("/config/.local/share/zuno/db/oc.db"))
        );
    }

    #[test]
    fn release_channels_use_the_unsuffixed_name() {
        let resolved = base();
        let data = Path::new("/config/.local/share/zuno");
        for channel in UNSUFFIXED_DB_CHANNELS {
            assert_eq!(
                resolved.db_path_for_channel(channel),
                DbLocation::File(data.join("zuno.db")),
                "channel {channel}"
            );
        }
    }

    #[test]
    fn other_channels_are_suffixed_and_sanitized() {
        let resolved = base();
        let data = Path::new("/config/.local/share/zuno");
        assert_eq!(
            resolved.db_path_for_channel("local"),
            DbLocation::File(data.join("zuno-local.db"))
        );
        assert_eq!(
            resolved.db_path_for_channel("feature/new-thing"),
            DbLocation::File(data.join("zuno-feature-new-thing.db"))
        );
        assert_eq!(
            resolved.db_path_for_channel("dev@2.0 rc"),
            DbLocation::File(data.join("zuno-dev-2.0-rc.db"))
        );
    }

    #[test]
    fn disable_channel_db_forces_the_unsuffixed_name_case_sensitively() {
        for value in ["1", "true"] {
            let resolved = layout(&[(HOME, "/config"), (ZUNO_DISABLE_CHANNEL_DB, value)]);
            assert_eq!(
                resolved.db_path_for_channel("mybranch"),
                DbLocation::File(PathBuf::from("/config/.local/share/zuno/zuno.db")),
                "value {value}"
            );
        }
        // The flag compares the raw string, so `TRUE` does not qualify.
        let uppercase = layout(&[(HOME, "/config"), (ZUNO_DISABLE_CHANNEL_DB, "TRUE")]);
        assert_eq!(
            uppercase.db_path_for_channel("mybranch"),
            DbLocation::File(PathBuf::from("/config/.local/share/zuno/zuno-mybranch.db"))
        );
    }

    #[test]
    fn db_override_wins_over_the_channel_rules() {
        let resolved = layout(&[
            (HOME, "/config"),
            (ZUNO_DB, MEMORY_SENTINEL),
            (ZUNO_DISABLE_CHANNEL_DB, "1"),
        ]);
        assert_eq!(resolved.db_path_for_channel("mybranch"), DbLocation::Memory);
    }

    #[test]
    fn legacy_database_path_maps_only_zuno_default_filename_forms() {
        assert_eq!(
            legacy_db_path(Path::new("/data/zuno.db")),
            Some(PathBuf::from("/data/opencode.db"))
        );
        assert_eq!(
            legacy_db_path(Path::new("/data/zuno-local.db")),
            Some(PathBuf::from("/data/opencode-local.db"))
        );
        assert_eq!(legacy_db_path(Path::new("/data/custom.db")), None);
    }

    #[test]
    fn db_path_uses_the_build_channel() {
        let resolved = base();
        assert_eq!(
            resolved.db_path(),
            resolved.db_path_for_channel(installation_channel())
        );
    }

    #[test]
    fn sanitize_channel_keeps_the_allowed_class() {
        assert_eq!(sanitize_channel("Latest.1_2-3"), "Latest.1_2-3");
        assert_eq!(sanitize_channel("a/b\\c d:e"), "a-b-c-d-e");
        assert_eq!(sanitize_channel(""), "");
    }
}
