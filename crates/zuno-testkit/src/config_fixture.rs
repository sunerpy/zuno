//! Multi-layer config trees, materialized on disk.
//!
//! # What has to be buildable
//!
//! Config discovery in the oracle is a merge over many sources, and the ones that
//! come from the filesystem are:
//!
//! 1. the global directory, `$XDG_CONFIG_HOME/opencode/opencode.{json,jsonc}`;
//! 2. every `.opencode` directory from the working directory up to the worktree
//!    root, nearest last;
//! 3. `$HOME/.opencode`;
//! 4. `$OPENCODE_CONFIG_DIR`, when set;
//! 5. `opencode.{json,jsonc}` files walked up from the working directory, in
//!    reverse so the nearest wins;
//! 6. `$OPENCODE_CONFIG` naming one file, and `$OPENCODE_CONFIG_CONTENT` carrying
//!    one inline.
//!
//! The precise order and semantics are Todo 7-12's subject, not this builder's.
//! What this builder owes them is the ability to *construct* any arrangement —
//! global only, project only, both, a nested `.opencode` chain, an env-var layer,
//! and project config switched off — and to state afterwards exactly which files
//! it wrote, so a differential over twelve trees is describable as data.
//!
//! Ordering note taken from `packages/opencode/src/config/paths.ts`: the ancestor
//! walk stops at the worktree root, so a fixture that wants an ancestor layer to
//! be *visible* must keep it inside the worktree, and one that wants it ignored
//! must place it above the `.git` marker. [`ConfigFixture::mark_worktree_root`]
//! is how a test picks.

use std::path::{Path, PathBuf};

use crate::env::{DbChoice, ScriptedEnv};
use crate::error::{Result, TestkitError};

/// The config file basename the oracle looks for.
pub const CONFIG_BASENAME: &str = "opencode";

/// Which layer a written file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayer {
    /// `$XDG_CONFIG_HOME/opencode/`.
    Global,
    /// `$HOME/.opencode/`.
    HomeDotOpencode,
    /// A `.opencode/` directory inside the project tree.
    ProjectDotOpencode,
    /// A bare config file inside the project tree, in both products' spellings.
    ProjectFile,
    /// The file `$OPENCODE_CONFIG` points at.
    EnvConfigFile,
    /// The directory `$OPENCODE_CONFIG_DIR` points at.
    EnvConfigDir,
    /// Inline JSON in `$OPENCODE_CONFIG_CONTENT`; no file is written.
    EnvConfigContent,
}

/// One layer this fixture placed.
#[derive(Debug, Clone)]
pub struct PlacedLayer {
    /// Which layer it is.
    pub layer: ConfigLayer,
    /// The file written, or `None` for [`ConfigLayer::EnvConfigContent`].
    pub path: Option<PathBuf>,
    /// The exact bytes written.
    pub contents: String,
}

/// A builder for a layered config tree inside a scripted environment.
#[derive(Debug)]
pub struct ConfigFixture {
    env: ScriptedEnv,
    layers: Vec<PlacedLayer>,
}

impl ConfigFixture {
    /// A fresh scripted environment with no config files.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the temporary tree cannot be created.
    pub fn new() -> Result<Self> {
        Ok(Self {
            env: ScriptedEnv::new()?,
            layers: Vec::new(),
        })
    }

    /// Build on an existing scripted environment.
    #[must_use]
    pub fn on(env: ScriptedEnv) -> Self {
        Self {
            env,
            layers: Vec::new(),
        }
    }

    /// Choose where the database lives.
    #[must_use]
    pub fn with_db(mut self, db: DbChoice) -> Self {
        self.env = self.env.with_db(db);
        self
    }

    /// Write `$XDG_CONFIG_HOME/opencode/opencode.json`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn global(self, contents: &str) -> Result<Self> {
        let path = self
            .env
            .xdg_config()
            .join(CONFIG_BASENAME)
            .join("opencode.json");
        let zuno = self.env.xdg_config().join("zuno").join("zuno.json");
        self.place_with_zuno_mirror(ConfigLayer::Global, path, zuno, contents)
    }

    /// Write `$XDG_CONFIG_HOME/opencode/opencode.jsonc`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn global_jsonc(self, contents: &str) -> Result<Self> {
        let path = self
            .env
            .xdg_config()
            .join(CONFIG_BASENAME)
            .join("opencode.jsonc");
        let zuno = self.env.xdg_config().join("zuno").join("zuno.jsonc");
        self.place_with_zuno_mirror(ConfigLayer::Global, path, zuno, contents)
    }

    /// Write `$HOME/.opencode/opencode.json`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn home_dot_opencode(self, contents: &str) -> Result<Self> {
        let path = self.env.home().join(".opencode").join("opencode.json");
        let zuno = self.env.home().join(".zuno").join("zuno.json");
        self.place_with_zuno_mirror(ConfigLayer::HomeDotOpencode, path, zuno, contents)
    }

    /// Write `<project>/<relative>/opencode.json` and Zuno's `zuno.json` beside it.
    ///
    /// Pass `""` for the project root itself.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn project_file(self, relative: &str, contents: &str) -> Result<Self> {
        let path = self.env.project().join(relative).join("opencode.json");
        let zuno = self.env.project().join(relative).join("zuno.json");
        self.place_with_zuno_mirror(ConfigLayer::ProjectFile, path, zuno, contents)
    }

    /// Write `<project>/<relative>/opencode.jsonc` and Zuno's `zuno.jsonc` beside it.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn project_file_jsonc(self, relative: &str, contents: &str) -> Result<Self> {
        let path = self.env.project().join(relative).join("opencode.jsonc");
        let zuno = self.env.project().join(relative).join("zuno.jsonc");
        self.place_with_zuno_mirror(ConfigLayer::ProjectFile, path, zuno, contents)
    }

    /// Write `<project>/<relative>/.opencode/opencode.json`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn project_dot_opencode(self, relative: &str, contents: &str) -> Result<Self> {
        let path = self
            .env
            .project()
            .join(relative)
            .join(".opencode")
            .join("opencode.json");
        let zuno = self
            .env
            .project()
            .join(relative)
            .join(".zuno")
            .join("zuno.json");
        self.place_with_zuno_mirror(ConfigLayer::ProjectDotOpencode, path, zuno, contents)
    }

    /// Write an arbitrary file under `<project>/<relative>`.
    ///
    /// For the surrounding material a config layer needs — an `AGENTS.md`, a
    /// custom agent, a plugin — rather than for config itself.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn project_asset(self, relative: &str, contents: &str) -> Result<Self> {
        let path = self.env.project().join(relative);
        write_file(&path, contents)?;
        Ok(self)
    }

    /// Write a config file outside the tree and point `$OPENCODE_CONFIG` at it.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    pub fn env_config_file(mut self, contents: &str) -> Result<Self> {
        let path = self.env.root().join("env-config").join("opencode.json");
        write_file(&path, contents)?;
        self.env = self
            .env
            .set("OPENCODE_CONFIG", path.to_string_lossy().into_owned());
        self.layers.push(PlacedLayer {
            layer: ConfigLayer::EnvConfigFile,
            path: Some(path),
            contents: contents.to_owned(),
        });
        Ok(self)
    }

    /// Write a config directory outside the tree and point `$OPENCODE_CONFIG_DIR`
    /// at it.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the file cannot be written.
    /// Writes Zuno's filename only, unlike the mirrored layers above.
    ///
    /// One environment variable names this directory for both binaries, and Zuno
    /// rejects a legacy-named file in any directory it probes — so the usual
    /// side-by-side mirror is not available here. A differential over this layer
    /// has to run each binary with the variable pointing at its own directory.
    pub fn env_config_dir(mut self, contents: &str) -> Result<Self> {
        let dir = self.env.root().join("env-config-dir");
        let path = dir.join(format!("{}.json", zuno_paths::CONFIG_FILE_STEM));
        write_file(&path, contents)?;
        self.env = self
            .env
            .set("OPENCODE_CONFIG_DIR", dir.to_string_lossy().into_owned());
        self.layers.push(PlacedLayer {
            layer: ConfigLayer::EnvConfigDir,
            path: Some(path),
            contents: contents.to_owned(),
        });
        Ok(self)
    }

    /// Carry config inline in `$OPENCODE_CONFIG_CONTENT`, writing nothing.
    #[must_use]
    pub fn env_config_content(mut self, contents: &str) -> Self {
        self.env = self.env.set("OPENCODE_CONFIG_CONTENT", contents);
        self.layers.push(PlacedLayer {
            layer: ConfigLayer::EnvConfigContent,
            path: None,
            contents: contents.to_owned(),
        });
        self
    }

    /// Set `OPENCODE_DISABLE_PROJECT_CONFIG=1`, which drops the project `.opencode`
    /// chain from discovery.
    #[must_use]
    pub fn disable_project_config(mut self) -> Self {
        self.env = self.env.set("OPENCODE_DISABLE_PROJECT_CONFIG", "1");
        self
    }

    /// Create an empty `.git` directory at `<project>/<relative>`, which is what
    /// stops the oracle's ancestor walk.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the directory cannot be created.
    pub fn mark_worktree_root(self, relative: &str) -> Result<Self> {
        let dir = self.env.project().join(relative).join(".git");
        std::fs::create_dir_all(&dir)
            .map_err(|e| TestkitError::io("create worktree marker", dir, e))?;
        Ok(self)
    }

    /// Run child processes in `<project>/<relative>`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Io`] when the directory cannot be created.
    pub fn working_dir(mut self, relative: &str) -> Result<Self> {
        let dir = self.env.project().join(relative);
        self.env = self.env.with_working_dir(dir)?;
        Ok(self)
    }

    /// Set one more environment variable for the run.
    #[must_use]
    pub fn env_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env = self.env.set(key, value);
        self
    }

    /// Every layer this fixture placed, in the order it placed them.
    #[must_use]
    pub fn layers(&self) -> &[PlacedLayer] {
        &self.layers
    }

    /// A one-line description of the tree, for naming a case in a matrix.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.layers.is_empty() {
            return "no-config".to_owned();
        }
        self.layers
            .iter()
            .map(|l| match l.layer {
                ConfigLayer::Global => "global",
                ConfigLayer::HomeDotOpencode => "home-dot-opencode",
                ConfigLayer::ProjectDotOpencode => "project-dot-opencode",
                ConfigLayer::ProjectFile => "project-file",
                ConfigLayer::EnvConfigFile => "env-config-file",
                ConfigLayer::EnvConfigDir => "env-config-dir",
                ConfigLayer::EnvConfigContent => "env-config-content",
            })
            .collect::<Vec<_>>()
            .join("+")
    }

    /// The finished scripted environment, ready for an [`Oracle`](crate::Oracle)
    /// or a [`Subject`](crate::Subject).
    #[must_use]
    pub fn into_env(self) -> ScriptedEnv {
        self.env
    }

    /// The scripted environment, borrowed.
    #[must_use]
    pub fn env(&self) -> &ScriptedEnv {
        &self.env
    }

    /// Write one config layer as both products' filenames.
    ///
    /// There is no single-path variant any more: the two products' canonical config
    /// filenames are now disjoint, so every file-backed layer needs both spellings
    /// or one binary reads nothing. `path` is the oracle's, and it is the one
    /// recorded in [`PlacedLayer`] because the differential's assertions are about
    /// what the oracle was given.
    fn place_with_zuno_mirror(
        mut self,
        layer: ConfigLayer,
        oracle_path: PathBuf,
        zuno_path: PathBuf,
        contents: &str,
    ) -> Result<Self> {
        write_file(&oracle_path, contents)?;
        write_file(&zuno_path, contents)?;
        self.layers.push(PlacedLayer {
            layer,
            path: Some(oracle_path),
            contents: contents.to_owned(),
        });
        Ok(self)
    }
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| TestkitError::io("create config dir", parent.to_path_buf(), e))?;
    }
    std::fs::write(path, contents)
        .map_err(|e| TestkitError::io("write config file", path.to_path_buf(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_global_only_tree_writes_one_file_in_the_xdg_config_dir() {
        let fixture = ConfigFixture::new()
            .expect("fixture")
            .global(r#"{"model":"anthropic/claude-opus-4-7"}"#)
            .expect("write global");
        assert_eq!(fixture.layers().len(), 1);
        let placed = &fixture.layers()[0];
        assert_eq!(placed.layer, ConfigLayer::Global);
        let path = placed.path.as_ref().expect("a path");
        assert!(path.is_file(), "{}", path.display());
        assert!(path.starts_with(fixture.env().xdg_config()));
        assert!(
            path.ends_with("opencode/opencode.json"),
            "{}",
            path.display()
        );
        assert_eq!(fixture.describe(), "global");
    }

    #[test]
    fn a_dot_opencode_chain_places_a_file_at_every_level() {
        let fixture = ConfigFixture::new()
            .expect("fixture")
            .mark_worktree_root("")
            .expect("worktree marker")
            .project_dot_opencode("", r#"{"model":"root"}"#)
            .expect("root layer")
            .project_dot_opencode("a", r#"{"model":"a"}"#)
            .expect("a layer")
            .project_dot_opencode("a/b", r#"{"model":"b"}"#)
            .expect("b layer")
            .working_dir("a/b")
            .expect("cwd");

        let paths: Vec<&PathBuf> = fixture
            .layers()
            .iter()
            .filter_map(|l| l.path.as_ref())
            .collect();
        assert_eq!(paths.len(), 3);
        for path in &paths {
            assert!(path.is_file(), "{}", path.display());
            assert!(
                path.ends_with(".opencode/opencode.json"),
                "{}",
                path.display()
            );
        }
        assert_eq!(
            fixture.env().working_dir(),
            fixture.env().project().join("a/b")
        );
        assert!(fixture.env().project().join(".git").is_dir());
        assert_eq!(
            fixture.describe(),
            "project-dot-opencode+project-dot-opencode+project-dot-opencode"
        );
    }

    #[test]
    fn every_layer_kind_is_constructible_and_self_describing() {
        let fixture = ConfigFixture::new()
            .expect("fixture")
            .global(r#"{"a":1}"#)
            .expect("global")
            .home_dot_opencode(r#"{"b":2}"#)
            .expect("home")
            .project_dot_opencode("", r#"{"c":3}"#)
            .expect("project dir")
            .project_file("", r#"{"d":4}"#)
            .expect("project file")
            .project_file_jsonc("nested", "{ /* comment */ }")
            .expect("project jsonc")
            .env_config_file(r#"{"e":5}"#)
            .expect("env file")
            .env_config_dir(r#"{"f":6}"#)
            .expect("env dir")
            .env_config_content(r#"{"g":7}"#);

        assert_eq!(fixture.layers().len(), 8);
        let kinds: Vec<ConfigLayer> = fixture.layers().iter().map(|l| l.layer).collect();
        assert!(kinds.contains(&ConfigLayer::Global));
        assert!(kinds.contains(&ConfigLayer::HomeDotOpencode));
        assert!(kinds.contains(&ConfigLayer::ProjectDotOpencode));
        assert!(kinds.contains(&ConfigLayer::ProjectFile));
        assert!(kinds.contains(&ConfigLayer::EnvConfigFile));
        assert!(kinds.contains(&ConfigLayer::EnvConfigDir));
        assert!(kinds.contains(&ConfigLayer::EnvConfigContent));

        let vars = fixture.env().env_vars();
        assert!(
            vars.get("OPENCODE_CONFIG")
                .is_some_and(|v| v.ends_with("opencode.json"))
        );
        assert!(
            vars.get("OPENCODE_CONFIG_DIR")
                .is_some_and(|v| v.ends_with("env-config-dir"))
        );
        assert_eq!(
            vars.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
            Some(r#"{"g":7}"#)
        );

        // The inline layer is the only one that writes nothing.
        let without_path: Vec<ConfigLayer> = fixture
            .layers()
            .iter()
            .filter(|l| l.path.is_none())
            .map(|l| l.layer)
            .collect();
        assert_eq!(without_path, vec![ConfigLayer::EnvConfigContent]);
        for placed in fixture.layers().iter().filter(|l| l.path.is_some()) {
            let path = placed.path.as_ref().expect("checked");
            assert_eq!(
                std::fs::read_to_string(path).expect("readable"),
                placed.contents,
                "{} was not written verbatim",
                path.display()
            );
        }
    }

    /// Todo 12 needs a matrix of trees, and each case has to be nameable.
    #[test]
    fn a_matrix_of_trees_is_constructible_and_each_case_is_named() {
        let cases: Vec<(String, ConfigFixture)> = vec![
            ("none".to_owned(), ConfigFixture::new().expect("f")),
            (
                "global".to_owned(),
                ConfigFixture::new()
                    .expect("f")
                    .global(r#"{"a":1}"#)
                    .expect("w"),
            ),
            (
                "project".to_owned(),
                ConfigFixture::new()
                    .expect("f")
                    .project_file("", r#"{"a":1}"#)
                    .expect("w"),
            ),
            (
                "both".to_owned(),
                ConfigFixture::new()
                    .expect("f")
                    .global(r#"{"a":1}"#)
                    .expect("w")
                    .project_file("", r#"{"a":2}"#)
                    .expect("w"),
            ),
            (
                "chain".to_owned(),
                ConfigFixture::new()
                    .expect("f")
                    .project_dot_opencode("", r#"{"a":1}"#)
                    .expect("w")
                    .project_dot_opencode("deep", r#"{"a":2}"#)
                    .expect("w")
                    .working_dir("deep")
                    .expect("cwd"),
            ),
            (
                "env-only".to_owned(),
                ConfigFixture::new()
                    .expect("f")
                    .env_config_content(r#"{"a":1}"#),
            ),
            (
                "project-disabled".to_owned(),
                ConfigFixture::new()
                    .expect("f")
                    .project_file("", r#"{"a":1}"#)
                    .expect("w")
                    .disable_project_config(),
            ),
        ];
        assert_eq!(cases.len(), 7);
        for (name, fixture) in &cases {
            assert!(
                !fixture.describe().is_empty(),
                "case {name} has no description"
            );
            let root = fixture.env().root();
            for placed in fixture.layers() {
                if let Some(path) = &placed.path {
                    assert!(path.starts_with(root), "case {name} escaped its temp tree");
                }
            }
        }
        let disabled = &cases[6].1;
        assert_eq!(
            disabled
                .env()
                .env_vars()
                .get("OPENCODE_DISABLE_PROJECT_CONFIG")
                .map(String::as_str),
            Some("1")
        );
    }
}
