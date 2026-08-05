//! The XDG directory table, ported from `packages/core/src/global.ts:10-43`.
//!
//! # What is being reproduced
//!
//! ```text
//! const app    = "opencode"
//! const data   = path.join(xdgData!,   app)
//! const cache  = path.join(xdgCache!,  app)
//! const config = path.join(xdgConfig!, app)
//! const state  = path.join(xdgState!,  app)
//! const tmp    = path.join(os.tmpdir(), app)
//!
//! paths = {
//!   home:   process.env.OPENCODE_TEST_HOME ?? os.homedir(),
//!   data,
//!   bin:    path.join(cache, "bin"),
//!   log:    path.join(data,  "log"),
//!   repos:  path.join(data,  "repos"),
//!   cache, config, state, tmp,
//! }
//! ```
//!
//! Those nine keys, in that order, are exactly what `opencode debug paths`
//! prints, which is what [`Layout::debug_paths_dump`] reproduces byte for byte.
//!
//! # Divergence: no eager `mkdir`
//!
//! `global.ts:35-43` creates seven of these directories at **module import**,
//! before any command has decided it needs them. Nothing here does: every
//! getter is a pure computation and creation happens only in
//! [`Layout::ensure`](crate::Layout::ensure). This divergence is intentional and
//! is recorded in `.omo/notepads/opencode-rust/decisions.md` — a differential
//! test cannot see it, because both binaries report the same *paths*.
//!
//! The eager `mkdir` is observable, though: `TMPDIR=/ opencode debug paths`
//! prints no paths at all and exits 1 with
//! `EACCES: permission denied, mkdir '/opencode'`, because import-time creation
//! runs before the command does.

use std::path::{Path, PathBuf};

use crate::env::{
    Env, HOME, OPENCODE_CONFIG_DIR, OPENCODE_DB, OPENCODE_DISABLE_CHANNEL_DB,
    OPENCODE_DISABLE_PROJECT_CONFIG, OPENCODE_MODELS_URL, OPENCODE_TEST_HOME, TEMP, TMP, TMPDIR,
    XDG_CACHE_HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME,
};
use crate::node_path;

/// The application directory name appended to every XDG base — `global.ts:10`.
pub const APP: &str = "opencode";

/// Node's `os.tmpdir()` POSIX fallback when none of the temp variables is set.
pub const DEFAULT_TMPDIR: &str = "/tmp";

/// `$XDG_DATA_HOME`'s fallback, relative to home — `xdg-basedir@5.1.0`.
const DEFAULT_DATA_SEGMENTS: [&str; 2] = [".local", "share"];
/// `$XDG_CONFIG_HOME`'s fallback, relative to home.
const DEFAULT_CONFIG_SEGMENTS: [&str; 1] = [".config"];
/// `$XDG_STATE_HOME`'s fallback, relative to home.
const DEFAULT_STATE_SEGMENTS: [&str; 2] = [".local", "state"];
/// `$XDG_CACHE_HOME`'s fallback, relative to home.
const DEFAULT_CACHE_SEGMENTS: [&str; 1] = [".cache"];

/// The nine keys `opencode debug paths` prints, in the order it prints them.
///
/// The order is the insertion order of the `paths` object literal in
/// `global.ts:17-29`, which is what `Object.entries` walks.
pub const DEBUG_PATHS_KEYS: [&str; 9] = [
    "home", "data", "bin", "log", "repos", "cache", "config", "state", "tmp",
];

/// Width `debug paths` pads each key to before printing the value —
/// `console.log(key.padEnd(10), value)`.
const DEBUG_PATHS_KEY_WIDTH: usize = 10;

/// Every path the layout defines, plus the environment inputs that shaped it.
///
/// Resolved once from an [`Env`] and then immutable, mirroring the oracle's
/// compute-at-import timing while staying a plain value that tests can build by
/// hand. Flag inputs live here too so that later methods — `db_path`,
/// `config_directories`, `models_cache` — are pure functions of `&self` rather
/// than each reaching back into the process environment at a different moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    home: PathBuf,
    data: PathBuf,
    bin: PathBuf,
    log: PathBuf,
    repos: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    state: PathBuf,
    tmp: PathBuf,
    config_dir_override: Option<String>,
    disable_project_config: bool,
    db_override: Option<String>,
    disable_channel_db: bool,
    models_source: String,
}

impl Layout {
    /// Resolve the layout from the current process environment.
    ///
    /// The home fallback is [`std::env::home_dir`], which on Unix consults
    /// `HOME` and then `getpwuid` — the same ladder as Node's `os.homedir()`.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self::resolve(&Env::from_process())
    }

    /// Resolve the layout from an explicit environment.
    ///
    /// Falls back to [`std::env::home_dir`] when `HOME` is absent from `env`.
    /// Use [`Layout::resolve_with`] for a resolution that reads nothing outside
    /// its arguments.
    #[must_use]
    pub fn resolve(env: &Env) -> Self {
        Self::resolve_with(env, std::env::home_dir().as_deref())
    }

    /// Fully pure resolution: `home_fallback` stands in for `getpwuid`.
    ///
    /// `home_fallback` is consulted only when `HOME` is unset or empty, which is
    /// the condition `uv_os_homedir` uses before falling through to the password
    /// database.
    #[must_use]
    pub fn resolve_with(env: &Env, home_fallback: Option<&Path>) -> Self {
        let system_home = env
            .truthy_value(HOME)
            .map(str::to_owned)
            .or_else(|| home_fallback.map(|path| path.to_string_lossy().into_owned()))
            .unwrap_or_default();

        let data_base = xdg_base(env, XDG_DATA_HOME, &system_home, &DEFAULT_DATA_SEGMENTS);
        let cache_base = xdg_base(env, XDG_CACHE_HOME, &system_home, &DEFAULT_CACHE_SEGMENTS);
        let config_base = xdg_base(env, XDG_CONFIG_HOME, &system_home, &DEFAULT_CONFIG_SEGMENTS);
        let state_base = xdg_base(env, XDG_STATE_HOME, &system_home, &DEFAULT_STATE_SEGMENTS);

        let data = node_path::join(&data_base, APP);
        let cache = node_path::join(&cache_base, APP);
        let config = node_path::join(&config_base, APP);
        let state = node_path::join(&state_base, APP);
        let tmp = node_path::join(&tmpdir(env), APP);

        // `Path.home` uses `??`, so an empty OPENCODE_TEST_HOME wins over the
        // system home. See the module docs on env.rs for the measured proof.
        let home = env
            .value(OPENCODE_TEST_HOME)
            .map_or(system_home, str::to_owned);

        Self {
            bin: PathBuf::from(node_path::join(&cache, "bin")),
            log: PathBuf::from(node_path::join(&data, "log")),
            repos: PathBuf::from(node_path::join(&data, "repos")),
            home: PathBuf::from(home),
            data: PathBuf::from(data),
            cache: PathBuf::from(cache),
            config: PathBuf::from(config),
            state: PathBuf::from(state),
            tmp: PathBuf::from(tmp),
            config_dir_override: env.value(OPENCODE_CONFIG_DIR).map(str::to_owned),
            disable_project_config: env.flag(OPENCODE_DISABLE_PROJECT_CONFIG),
            db_override: env.truthy_value(OPENCODE_DB).map(str::to_owned),
            disable_channel_db: env.exact_flag(OPENCODE_DISABLE_CHANNEL_DB),
            models_source: env
                .truthy_value(OPENCODE_MODELS_URL)
                .unwrap_or(crate::files::DEFAULT_MODELS_SOURCE)
                .to_owned(),
        }
    }

    /// `Global.Path.home`.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `Global.Path.data` — `$XDG_DATA_HOME/opencode`.
    #[must_use]
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// `Global.Path.bin` — `cache()/bin`.
    #[must_use]
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// `Global.Path.log` — `data()/log`.
    #[must_use]
    pub fn log(&self) -> &Path {
        &self.log
    }

    /// `Global.Path.repos` — `data()/repos`.
    #[must_use]
    pub fn repos(&self) -> &Path {
        &self.repos
    }

    /// `Global.Path.cache` — `$XDG_CACHE_HOME/opencode`.
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// `Global.Path.config` — `$XDG_CONFIG_HOME/opencode`.
    ///
    /// This is the raw XDG directory, which is what `debug paths` prints and
    /// what `global.ts:37` creates. `OPENCODE_CONFIG_DIR` does **not** change
    /// it; see [`Layout::effective_config`].
    #[must_use]
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// `Global.Path.state` — `$XDG_STATE_HOME/opencode`.
    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// `Global.Path.tmp` — `<os.tmpdir()>/opencode`.
    #[must_use]
    pub fn temp(&self) -> &Path {
        &self.tmp
    }

    /// The configuration directory the *service* sees:
    /// `Flag.OPENCODE_CONFIG_DIR ?? Path.config` (`global.ts:64`).
    ///
    /// Nullish, not truthy — an `OPENCODE_CONFIG_DIR=""` overrides `config()`
    /// with an empty path here, while `config_directories` drops it. Both
    /// behaviours are the oracle's, in their respective places.
    #[must_use]
    pub fn effective_config(&self) -> &Path {
        match self.config_dir_override.as_deref() {
            Some(override_dir) => Path::new(override_dir),
            None => &self.config,
        }
    }

    /// The raw `OPENCODE_CONFIG_DIR` value, if the variable was present.
    #[must_use]
    pub fn config_dir_override(&self) -> Option<&str> {
        self.config_dir_override.as_deref()
    }

    /// Whether `OPENCODE_DISABLE_PROJECT_CONFIG` was truthy.
    #[must_use]
    pub fn project_config_disabled(&self) -> bool {
        self.disable_project_config
    }

    /// The non-empty `OPENCODE_DB` value, if any.
    #[must_use]
    pub fn db_override(&self) -> Option<&str> {
        self.db_override.as_deref()
    }

    /// Whether `OPENCODE_DISABLE_CHANNEL_DB` was exactly `"1"` or `"true"`.
    #[must_use]
    pub fn channel_db_disabled(&self) -> bool {
        self.disable_channel_db
    }

    /// The resolved model catalog source: `OPENCODE_MODELS_URL` or the default.
    #[must_use]
    pub fn models_source(&self) -> &str {
        &self.models_source
    }

    /// The nine `debug paths` keys paired with their values, in print order.
    #[must_use]
    pub fn entries(&self) -> [(&'static str, &Path); 9] {
        [
            ("home", self.home()),
            ("data", self.data()),
            ("bin", self.bin()),
            ("log", self.log()),
            ("repos", self.repos()),
            ("cache", self.cache()),
            ("config", self.config()),
            ("state", self.state()),
            ("tmp", self.temp()),
        ]
    }

    /// The exact stdout of `opencode debug paths`.
    ///
    /// `console.log(key.padEnd(10), value)` emits the padded key, a single
    /// space, the value, and a newline — so a key at or over ten characters is
    /// still followed by exactly one space. Reproduced with `{:<10}` plus an
    /// explicit space rather than a wider field, because a format width would
    /// swallow that space for a long key.
    #[must_use]
    pub fn debug_paths_dump(&self) -> String {
        let mut out = String::new();
        for (key, value) in self.entries() {
            out.push_str(&format!(
                "{key:<width$} {value}\n",
                width = DEBUG_PATHS_KEY_WIDTH,
                value = value.display()
            ));
        }
        out
    }
}

/// `xdgData || (home ? path.join(home, …defaults) : undefined)`.
///
/// The `||` is why an empty variable falls back, and the absence of any
/// absoluteness check is why a relative one is honoured verbatim — measured:
/// `XDG_DATA_HOME=relx` makes the oracle report `data relx/opencode`. That is
/// the single reason the `dirs` crate is not used here; `dirs` discards a
/// relative XDG value and substitutes the home-relative default, which would
/// send the Rust binary to a different data directory than the real one.
fn xdg_base(env: &Env, key: &str, home: &str, defaults: &[&str]) -> String {
    if let Some(value) = env.truthy_value(key) {
        return value.to_owned();
    }
    if home.is_empty() {
        // The oracle throws a TypeError here (`path.join(undefined, …)`).
        // Recorded as a divergence in issues.md; unreachable on Unix, where
        // getpwuid always resolves for a live process.
        return node_path::join_all(defaults);
    }
    let mut segments = Vec::with_capacity(defaults.len() + 1);
    segments.push(home);
    segments.extend_from_slice(defaults);
    node_path::join_all(segments)
}

/// Port of Node's `os.tmpdir()` on POSIX.
///
/// `TMPDIR || TMP || TEMP || "/tmp"`, then one trailing slash removed unless
/// that would empty the path. The `length > 1` guard is why `TMPDIR=/` yields
/// `/opencode` rather than `opencode`.
fn tmpdir(env: &Env) -> String {
    let raw = env
        .truthy_value(TMPDIR)
        .or_else(|| env.truthy_value(TMP))
        .or_else(|| env.truthy_value(TEMP))
        .unwrap_or(DEFAULT_TMPDIR);
    if raw.len() > 1 && raw.ends_with('/') {
        return raw[..raw.len() - 1].to_owned();
    }
    raw.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(pairs: &[(&str, &str)]) -> Layout {
        let env = Env::from_pairs(pairs.iter().copied());
        Layout::resolve_with(&env, None)
    }

    /// The exact output of `opencode debug paths` on this machine with no XDG
    /// variables set and `HOME=/config`, captured from the 1.18.12 binary.
    #[test]
    fn reproduces_the_measured_default_layout() {
        let resolved = layout(&[(HOME, "/config")]);
        assert_eq!(
            resolved.debug_paths_dump(),
            concat!(
                "home       /config\n",
                "data       /config/.local/share/opencode\n",
                "bin        /config/.cache/opencode/bin\n",
                "log        /config/.local/share/opencode/log\n",
                "repos      /config/.local/share/opencode/repos\n",
                "cache      /config/.cache/opencode\n",
                "config     /config/.config/opencode\n",
                "state      /config/.local/state/opencode\n",
                "tmp        /tmp/opencode\n",
            )
        );
    }

    /// Captured from the binary with all four XDG variables pointed at /tmp/x.
    #[test]
    fn reproduces_the_measured_custom_xdg_layout() {
        let resolved = layout(&[
            (HOME, "/config"),
            (XDG_DATA_HOME, "/tmp/x/data"),
            (XDG_CACHE_HOME, "/tmp/x/cache"),
            (XDG_CONFIG_HOME, "/tmp/x/config"),
            (XDG_STATE_HOME, "/tmp/x/state"),
        ]);
        assert_eq!(
            resolved.debug_paths_dump(),
            concat!(
                "home       /config\n",
                "data       /tmp/x/data/opencode\n",
                "bin        /tmp/x/cache/opencode/bin\n",
                "log        /tmp/x/data/opencode/log\n",
                "repos      /tmp/x/data/opencode/repos\n",
                "cache      /tmp/x/cache/opencode\n",
                "config     /tmp/x/config/opencode\n",
                "state      /tmp/x/state/opencode\n",
                "tmp        /tmp/opencode\n",
            )
        );
    }

    #[test]
    fn empty_xdg_falls_back_but_empty_test_home_does_not() {
        let resolved = layout(&[
            (HOME, "/config"),
            (XDG_DATA_HOME, ""),
            (OPENCODE_TEST_HOME, ""),
        ]);
        assert_eq!(resolved.data(), Path::new("/config/.local/share/opencode"));
        assert_eq!(resolved.home(), Path::new(""));
    }

    #[test]
    fn test_home_overrides_home_but_not_data() {
        let resolved = layout(&[(HOME, "/config"), (OPENCODE_TEST_HOME, "/tmp/fakehome")]);
        assert_eq!(resolved.home(), Path::new("/tmp/fakehome"));
        assert_eq!(resolved.data(), Path::new("/config/.local/share/opencode"));
    }

    #[test]
    fn relative_xdg_is_honoured_verbatim() {
        let resolved = layout(&[(HOME, "/config"), (XDG_DATA_HOME, "relx")]);
        assert_eq!(resolved.data(), Path::new("relx/opencode"));
        assert_eq!(resolved.log(), Path::new("relx/opencode/log"));
        assert_eq!(resolved.repos(), Path::new("relx/opencode/repos"));
    }

    #[test]
    fn tmpdir_follows_the_node_ladder() {
        assert_eq!(layout(&[(TMPDIR, "/a")]).temp(), Path::new("/a/opencode"));
        assert_eq!(layout(&[(TMP, "/b")]).temp(), Path::new("/b/opencode"));
        assert_eq!(layout(&[(TEMP, "/c")]).temp(), Path::new("/c/opencode"));
        assert_eq!(layout(&[]).temp(), Path::new("/tmp/opencode"));
        // TMPDIR wins over TMP wins over TEMP.
        assert_eq!(
            layout(&[(TMPDIR, "/a"), (TMP, "/b"), (TEMP, "/c")]).temp(),
            Path::new("/a/opencode")
        );
        assert_eq!(
            layout(&[(TMP, "/b"), (TEMP, "/c")]).temp(),
            Path::new("/b/opencode")
        );
    }

    #[test]
    fn tmpdir_strips_one_trailing_slash_unless_it_is_root() {
        assert_eq!(
            layout(&[(TMPDIR, "/probe/")]).temp(),
            Path::new("/probe/opencode")
        );
        // The `length > 1` guard: root keeps its slash, so the join yields
        // `/opencode` — measured against the binary, which then dies trying to
        // mkdir it.
        assert_eq!(layout(&[(TMPDIR, "/")]).temp(), Path::new("/opencode"));
    }

    #[test]
    fn config_override_is_nullish_and_does_not_move_config() {
        let resolved = layout(&[(HOME, "/config"), (OPENCODE_CONFIG_DIR, "/tmp/mycfg")]);
        assert_eq!(resolved.config(), Path::new("/config/.config/opencode"));
        assert_eq!(resolved.effective_config(), Path::new("/tmp/mycfg"));
        assert_eq!(resolved.config_dir_override(), Some("/tmp/mycfg"));

        let empty = layout(&[(HOME, "/config"), (OPENCODE_CONFIG_DIR, "")]);
        assert_eq!(empty.effective_config(), Path::new(""));

        let unset = layout(&[(HOME, "/config")]);
        assert_eq!(unset.effective_config(), unset.config());
        assert_eq!(unset.config_dir_override(), None);
    }

    #[test]
    fn home_fallback_is_used_only_when_home_is_absent_or_empty() {
        let env = Env::empty();
        let fallback = Path::new("/from/passwd");
        assert_eq!(
            Layout::resolve_with(&env, Some(fallback)).data(),
            Path::new("/from/passwd/.local/share/opencode")
        );
        let with_empty_home = Env::empty().with(HOME, "");
        assert_eq!(
            Layout::resolve_with(&with_empty_home, Some(fallback)).data(),
            Path::new("/from/passwd/.local/share/opencode")
        );
        let with_home = Env::empty().with(HOME, "/real");
        assert_eq!(
            Layout::resolve_with(&with_home, Some(fallback)).data(),
            Path::new("/real/.local/share/opencode")
        );
    }

    #[test]
    fn no_home_at_all_yields_a_relative_base_instead_of_panicking() {
        let resolved = Layout::resolve_with(&Env::empty(), None);
        assert_eq!(resolved.data(), Path::new(".local/share/opencode"));
        assert_eq!(resolved.home(), Path::new(""));
    }

    #[test]
    fn entries_match_the_documented_key_order() {
        let resolved = layout(&[(HOME, "/config")]);
        let keys: Vec<&str> = resolved.entries().iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, DEBUG_PATHS_KEYS.to_vec());
    }

    #[test]
    fn flag_inputs_are_captured_at_resolve_time() {
        let resolved = layout(&[
            (HOME, "/config"),
            (OPENCODE_DISABLE_PROJECT_CONFIG, "1"),
            (OPENCODE_DISABLE_CHANNEL_DB, "true"),
            (OPENCODE_DB, "custom.db"),
            (OPENCODE_MODELS_URL, "https://example.test"),
        ]);
        assert!(resolved.project_config_disabled());
        assert!(resolved.channel_db_disabled());
        assert_eq!(resolved.db_override(), Some("custom.db"));
        assert_eq!(resolved.models_source(), "https://example.test");

        let bare = layout(&[(HOME, "/config"), (OPENCODE_DB, "")]);
        assert!(!bare.project_config_disabled());
        assert!(!bare.channel_db_disabled());
        assert_eq!(bare.db_override(), None);
        assert_eq!(bare.models_source(), crate::files::DEFAULT_MODELS_SOURCE);
    }

    #[test]
    fn from_process_env_agrees_with_an_explicit_snapshot() {
        let snapshot = Layout::resolve(&Env::from_process());
        assert_eq!(Layout::from_process_env(), snapshot);
    }
}
