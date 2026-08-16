//! A scripted, closed environment for running either side of a differential.
//!
//! Both the oracle and the subject must see the *same* world, and that world must
//! not be the developer's. Every process the harness launches starts from a
//! cleared environment: a fresh `HOME`, fresh `XDG_*` directories, a fresh
//! `TMPDIR`, and an explicit database choice, all inside one temporary tree that
//! is deleted when the fixture drops.
//!
//! Two variables are set unconditionally, and they are part of how the crate's
//! "never make a live call" invariant is enforced rather than merely asserted:
//! `OPENCODE_DISABLE_AUTOUPDATE` and `OPENCODE_DISABLE_MODELS_FETCH`. A run that
//! reached the network for a models catalogue or a new release would be a live
//! call, so the scripted environment forbids both at the process boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zuno_paths::env::ZUNO_ENV_NAME_MAP;

use tempfile::TempDir;

use crate::error::{Result, TestkitError};
use crate::normalize::Normalizer;

/// Where the run's SQLite database should live.
///
/// Mirrors the three shapes `OPENCODE_DB` accepts in
/// `packages/core/src/database/database.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbChoice {
    /// `OPENCODE_DB=:memory:` — nothing touches the disk.
    Memory,
    /// A path under the scripted data directory, set as an absolute path.
    TempFile,
    /// `OPENCODE_DB=<relative>` — exercises data-relative resolution.
    DataRelative(String),
    /// An absolute path chosen by the caller.
    Absolute(PathBuf),
    /// Leave `OPENCODE_DB` unset and let the default location apply.
    Default,
}

/// Host variables that are allowed through the cleared environment.
///
/// `PATH` is unavoidable: the oracle is a Node or Bun program and must be able to
/// find its own runtime. Nothing else is passed by default, so a variable that
/// influences a run is either in this list, set by [`ScriptedEnv`], or set
/// explicitly by the caller — never inherited by accident.
const DEFAULT_PASSTHROUGH: &[&str] = &["PATH"];

/// A temporary, closed environment shared by both sides of a differential.
#[derive(Debug)]
pub struct ScriptedEnv {
    root: TempDir,
    home: PathBuf,
    data: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    state: PathBuf,
    tmp: PathBuf,
    project: PathBuf,
    cwd: PathBuf,
    db: DbChoice,
    passthrough: Vec<String>,
    extra: BTreeMap<String, String>,
}

impl ScriptedEnv {
    /// Create the temporary tree and its directories.
    ///
    /// # Errors
    ///
    /// If the temporary directory or any of its children cannot be created.
    pub fn new() -> Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("zuno-testkit-")
            .tempdir()
            .map_err(|e| TestkitError::io("create scripted temp root", "<tempdir>", e))?;
        let base = root.path().to_path_buf();
        let this = Self {
            home: base.join("home"),
            data: base.join("xdg-data"),
            config: base.join("xdg-config"),
            cache: base.join("xdg-cache"),
            state: base.join("xdg-state"),
            tmp: base.join("tmp"),
            project: base.join("project"),
            cwd: base.join("project"),
            db: DbChoice::Memory,
            passthrough: DEFAULT_PASSTHROUGH
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            extra: BTreeMap::new(),
            root,
        };
        for dir in [
            &this.home,
            &this.data,
            &this.config,
            &this.cache,
            &this.state,
            &this.tmp,
            &this.project,
        ] {
            std::fs::create_dir_all(dir)
                .map_err(|e| TestkitError::io("create scripted dir", dir.clone(), e))?;
        }
        Ok(this)
    }

    /// The temporary root that holds every scripted directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// The scripted `HOME`.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The scripted `XDG_DATA_HOME`.
    #[must_use]
    pub fn xdg_data(&self) -> &Path {
        &self.data
    }

    /// The scripted `XDG_CONFIG_HOME`.
    #[must_use]
    pub fn xdg_config(&self) -> &Path {
        &self.config
    }

    /// The scripted `XDG_CACHE_HOME`.
    #[must_use]
    pub fn xdg_cache(&self) -> &Path {
        &self.cache
    }

    /// The scripted `XDG_STATE_HOME`.
    #[must_use]
    pub fn xdg_state(&self) -> &Path {
        &self.state
    }

    /// The scripted `TMPDIR`.
    #[must_use]
    pub fn tmpdir(&self) -> &Path {
        &self.tmp
    }

    /// The scripted project root, used as the default working directory.
    #[must_use]
    pub fn project(&self) -> &Path {
        &self.project
    }

    /// The working directory a child process is started in.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        &self.cwd
    }

    /// Choose where the database lives.
    #[must_use]
    pub fn with_db(mut self, db: DbChoice) -> Self {
        self.db = db;
        self
    }

    /// Start child processes in `dir`, which must be inside the scripted tree.
    ///
    /// # Errors
    ///
    /// If `dir` does not exist and cannot be created.
    pub fn with_working_dir(mut self, dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .map_err(|e| TestkitError::io("create scripted working dir", dir.clone(), e))?;
        self.cwd = dir;
        Ok(self)
    }

    /// Set an additional environment variable for the run.
    #[must_use]
    pub fn set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }

    /// Allow one more host variable through the cleared environment.
    ///
    /// Every use of this widens what a run can observe, so prefer [`Self::set`]
    /// with an explicit value when the value matters.
    #[must_use]
    pub fn passthrough(mut self, key: impl Into<String>) -> Self {
        self.passthrough.push(key.into());
        self
    }

    /// The complete environment a child process receives.
    #[must_use]
    pub fn env_vars(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for key in &self.passthrough {
            if let Ok(value) = std::env::var(key) {
                env.insert(key.clone(), value);
            }
        }
        env.insert("HOME".to_owned(), display(&self.home));
        env.insert("XDG_DATA_HOME".to_owned(), display(&self.data));
        env.insert("XDG_CONFIG_HOME".to_owned(), display(&self.config));
        env.insert("XDG_CACHE_HOME".to_owned(), display(&self.cache));
        env.insert("XDG_STATE_HOME".to_owned(), display(&self.state));
        env.insert("TMPDIR".to_owned(), display(&self.tmp));
        env.insert("OPENCODE_TEST_HOME".to_owned(), display(&self.home));
        env.insert("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned());
        env.insert("OPENCODE_DISABLE_MODELS_FETCH".to_owned(), "1".to_owned());
        match &self.db {
            DbChoice::Memory => {
                env.insert("OPENCODE_DB".to_owned(), ":memory:".to_owned());
            }
            DbChoice::TempFile => {
                env.insert(
                    "OPENCODE_DB".to_owned(),
                    display(&self.data.join("scripted.db")),
                );
            }
            DbChoice::DataRelative(rel) => {
                env.insert("OPENCODE_DB".to_owned(), rel.clone());
            }
            DbChoice::Absolute(path) => {
                env.insert("OPENCODE_DB".to_owned(), display(path));
            }
            DbChoice::Default => {}
        }
        env.extend(self.extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        for (legacy, zuno) in ZUNO_ENV_NAME_MAP {
            if let Some(value) = env.get(legacy).cloned() {
                env.entry(zuno.to_owned()).or_insert(value);
            }
        }
        env
    }

    /// A [`Normalizer`] that masks this run's own temporary paths as literals.
    ///
    /// The masks are the exact strings this fixture created, in longest-first
    /// order, so a subject that wrote to a *different* temporary path is still
    /// reported as divergent.
    #[must_use]
    pub fn normalizer(&self) -> Normalizer {
        self.decorate(Normalizer::default())
    }

    /// Add this run's path masks to an existing normalizer.
    #[must_use]
    pub fn decorate(&self, normalizer: Normalizer) -> Normalizer {
        normalizer
            .mask_literal("scripted-home", display(&self.home), "<HOME>")
            .mask_literal("scripted-data", display(&self.data), "<DATA>")
            .mask_literal("scripted-config", display(&self.config), "<CONFIG>")
            .mask_literal("scripted-cache", display(&self.cache), "<CACHE>")
            .mask_literal("scripted-state", display(&self.state), "<STATE>")
            .mask_literal("scripted-tmp", display(&self.tmp), "<TMP>")
            .mask_literal("scripted-project", display(&self.project), "<PROJECT>")
            .mask_literal("scripted-root", display(self.root.path()), "<ROOT>")
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_closed_and_rooted_in_the_temp_tree() {
        let env = ScriptedEnv::new().expect("scripted env");
        let vars = env.env_vars();
        let root = env.root().to_string_lossy().into_owned();
        for key in [
            "HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "TMPDIR",
            "OPENCODE_TEST_HOME",
            "ZUNO_TEST_HOME",
        ] {
            let value = vars.get(key).unwrap_or_else(|| panic!("{key} must be set"));
            assert!(
                value.starts_with(&root),
                "{key}={value} escaped the temp tree"
            );
        }
        // Nothing from the host except the documented passthrough.
        let unexpected: Vec<&String> = vars
            .keys()
            .filter(|k| {
                !k.starts_with("XDG_")
                    && !k.starts_with("OPENCODE_")
                    && !k.starts_with("ZUNO_")
                    && !matches!(k.as_str(), "HOME" | "TMPDIR" | "PATH")
            })
            .collect();
        assert!(
            unexpected.is_empty(),
            "unexpected inherited vars: {unexpected:?}"
        );
    }

    /// Part of the no-live-call invariant, enforced at the process boundary.
    #[test]
    fn network_reaching_features_are_disabled_for_every_run() {
        let vars = ScriptedEnv::new().expect("scripted env").env_vars();
        assert_eq!(
            vars.get("OPENCODE_DISABLE_AUTOUPDATE").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            vars.get("OPENCODE_DISABLE_MODELS_FETCH")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            vars.get("ZUNO_DISABLE_AUTOUPDATE").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            vars.get("ZUNO_DISABLE_MODELS_FETCH").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn every_db_choice_produces_the_documented_value() {
        let base = ScriptedEnv::new().expect("scripted env");
        assert_eq!(
            base.env_vars().get("OPENCODE_DB").map(String::as_str),
            Some(":memory:")
        );
        assert_eq!(
            base.env_vars().get("ZUNO_DB").map(String::as_str),
            Some(":memory:")
        );

        let rel = ScriptedEnv::new()
            .expect("scripted env")
            .with_db(DbChoice::DataRelative("sub/dir.db".to_owned()));
        assert_eq!(
            rel.env_vars().get("OPENCODE_DB").map(String::as_str),
            Some("sub/dir.db")
        );

        let none = ScriptedEnv::new()
            .expect("scripted env")
            .with_db(DbChoice::Default);
        assert!(!none.env_vars().contains_key("OPENCODE_DB"));

        let file = ScriptedEnv::new()
            .expect("scripted env")
            .with_db(DbChoice::TempFile);
        let value = file
            .env_vars()
            .get("OPENCODE_DB")
            .cloned()
            .expect("db path");
        assert!(value.starts_with(&file.xdg_data().to_string_lossy().into_owned()));
    }

    #[test]
    fn caller_supplied_variables_win() {
        let env = ScriptedEnv::new()
            .expect("scripted env")
            .set("OPENCODE_DB", "/explicit/path.db")
            .set("OPENCODE_CONFIG_CONTENT", "{}");
        let vars = env.env_vars();
        assert_eq!(
            vars.get("OPENCODE_DB").map(String::as_str),
            Some("/explicit/path.db")
        );
        assert_eq!(
            vars.get("ZUNO_DB").map(String::as_str),
            Some("/explicit/path.db")
        );
        assert_eq!(
            vars.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
            Some("{}")
        );
    }

    #[test]
    fn the_normalizer_masks_this_runs_paths_as_literals() {
        let env = ScriptedEnv::new().expect("scripted env");
        let n = env.normalizer();
        let text = format!(
            "config at {}/opencode/opencode.json",
            env.xdg_config().display()
        );
        assert_eq!(
            n.apply(&text).0,
            "config at <CONFIG>/opencode/opencode.json"
        );
        // A sibling temp path this run did not create is still compared.
        assert_eq!(
            n.apply("/tmp/zuno-testkit-somewhere-else/x").0,
            "/tmp/zuno-testkit-somewhere-else/x"
        );
    }

    #[test]
    fn the_temp_tree_is_removed_on_drop() {
        let path = {
            let env = ScriptedEnv::new().expect("scripted env");
            env.root().to_path_buf()
        };
        assert!(!path.exists(), "the scripted tree outlived its fixture");
    }
}
