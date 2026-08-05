//! The subject: this project's Rust binary, run under the same scripted world.
//!
//! Discovery is deliberately explicit. `cargo test -p oc-testkit` does not build
//! `oc-cli`'s binary, so a naive harness would either silently skip the
//! differential or report a confusing spawn failure. [`Subject::discover`]
//! therefore fails with the exact `cargo` command that would fix it, and
//! [`Subject::discover_or_build`] runs that command for the caller.

use std::path::{Path, PathBuf};

use crate::env::ScriptedEnv;
use crate::error::{Result, TestkitError};
use crate::oracle::ensure_executable;
use crate::run::{Provenance, RunOutcome, run_process};

/// The binary `oc-cli` produces, and the name a drop-in replacement is invoked by.
pub const SUBJECT_BIN: &str = "opencode-rust";
/// The cargo package that builds it.
pub const SUBJECT_PACKAGE: &str = "oc-cli";
/// Override the discovered subject binary with an explicit path.
pub const ENV_SUBJECT_BINARY: &str = "OC_TESTKIT_SUBJECT";

/// This project's binary, ready to run under a scripted environment.
#[derive(Debug)]
pub struct Subject {
    program: PathBuf,
    reported_version: Option<String>,
    env: ScriptedEnv,
}

impl Subject {
    /// Locate the already-built subject binary.
    ///
    /// # Errors
    ///
    /// [`TestkitError::BinaryNotFound`] naming the expected path and the `cargo
    /// build` command that produces it.
    pub fn discover() -> Result<Self> {
        let mut searched = Vec::new();
        if let Ok(explicit) = std::env::var(ENV_SUBJECT_BINARY) {
            return Self::at(PathBuf::from(explicit));
        }
        for candidate in candidate_paths() {
            if candidate.is_file() {
                return Self::at(candidate);
            }
            searched.push(candidate);
        }
        let expected = searched
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from(SUBJECT_BIN));
        Err(TestkitError::BinaryNotFound {
            role: "subject",
            expected,
            searched,
            remedy: format!("run `cargo build -p {SUBJECT_PACKAGE} --bin {SUBJECT_BIN}`"),
        })
    }

    /// Locate the subject binary, building it first if it is missing.
    ///
    /// # Errors
    ///
    /// [`TestkitError::SubjectBuildFailed`] when the build does not succeed, or
    /// [`TestkitError::BinaryNotFound`] if the build reports success but produces
    /// nothing at any expected path.
    pub fn discover_or_build() -> Result<Self> {
        match Self::discover() {
            Ok(subject) => Ok(subject),
            Err(TestkitError::BinaryNotFound { .. }) => {
                build_subject()?;
                Self::discover()
            }
            Err(other) => Err(other),
        }
    }

    /// Use the subject binary at an exact path.
    ///
    /// # Errors
    ///
    /// [`TestkitError::BinaryNotFound`] naming `path` when it does not exist.
    pub fn at(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        ensure_executable("subject", &path, || {
            format!("run `cargo build -p {SUBJECT_PACKAGE} --bin {SUBJECT_BIN}`")
        })?;
        Ok(Self {
            program: path,
            reported_version: None,
            env: ScriptedEnv::new()?,
        })
    }

    /// Replace the scripted environment, e.g. to share an oracle's one.
    #[must_use]
    pub fn with_env(mut self, env: ScriptedEnv) -> Self {
        self.env = env;
        self
    }

    /// Record the version this subject reports, so it reaches every diff label.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Spawn`] when `--version` cannot be run.
    pub fn probe_version(&mut self) -> Result<&str> {
        let outcome = self.run(["--version"])?;
        self.reported_version = Some(
            outcome
                .stdout
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .to_owned(),
        );
        Ok(self.reported_version.as_deref().unwrap_or_default())
    }

    /// The scripted environment this subject runs under.
    #[must_use]
    pub fn env(&self) -> &ScriptedEnv {
        &self.env
    }

    /// The binary that will be executed.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The provenance stamped onto every outcome this subject produces.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        Provenance::Subject {
            program: self.program.clone(),
            reported_version: self.reported_version.clone(),
        }
    }

    /// Run the subject with `args`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Spawn`] when the process cannot be started or waited on.
    pub fn run<I, S>(&self, args: I) -> Result<RunOutcome>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        run_process(
            self.provenance(),
            &self.program,
            &args,
            self.env.working_dir(),
            &self.env.env_vars(),
        )
    }
}

/// Every place the subject binary could be, in preference order.
///
/// `CARGO_TARGET_DIR` is honoured first because this workspace's worktrees share
/// one target directory; without that the harness would look in a per-worktree
/// `target/` that is never populated.
fn candidate_paths() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        roots.push(PathBuf::from(target));
    }
    if let Some(workspace) = workspace_root() {
        roots.push(workspace.join("target"));
    }
    let profiles = ["debug", "release"];
    roots
        .into_iter()
        .flat_map(|root| profiles.iter().map(move |p| root.join(p).join(SUBJECT_BIN)))
        .collect()
}

/// The workspace root, found by walking up from this crate to the `Cargo.toml`
/// that declares `[workspace]`.
#[must_use]
pub fn workspace_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|dir| {
            std::fs::read_to_string(dir.join("Cargo.toml"))
                .is_ok_and(|text| text.contains("[workspace]"))
        })
        .map(Path::to_path_buf)
}

fn build_subject() -> Result<()> {
    let command = format!("cargo build -p {SUBJECT_PACKAGE} --bin {SUBJECT_BIN}");
    let output =
        std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
            .args(["build", "-p", SUBJECT_PACKAGE, "--bin", SUBJECT_BIN])
            .current_dir(
                workspace_root().unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
            )
            .output()
            .map_err(|source| TestkitError::Spawn {
                program: PathBuf::from("cargo"),
                args: vec!["build".to_owned()],
                source,
            })?;
    if output.status.success() {
        return Ok(());
    }
    Err(TestkitError::SubjectBuildFailed {
        command,
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_subject_binary_names_the_build_command() {
        let missing = PathBuf::from("/nonexistent/oc-testkit/subject/opencode-rust");
        let err = Subject::at(&missing).expect_err("a missing path cannot be a subject");
        let rendered = err.to_string();
        assert!(
            rendered.contains("/nonexistent/oc-testkit/subject/opencode-rust"),
            "{rendered}"
        );
        assert!(
            rendered.contains("cargo build -p oc-cli --bin opencode-rust"),
            "{rendered}"
        );
    }

    #[test]
    fn the_workspace_root_is_the_one_declaring_the_workspace() {
        let root = workspace_root().expect("this crate lives in a workspace");
        assert!(
            root.join("crates/oc-testkit/Cargo.toml").is_file(),
            "{}",
            root.display()
        );
    }

    #[test]
    fn candidate_paths_prefer_the_shared_target_dir() {
        let candidates = candidate_paths();
        assert!(!candidates.is_empty());
        assert!(
            candidates.iter().all(|c| c.ends_with(SUBJECT_BIN)),
            "{candidates:?}"
        );
        if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
            assert!(
                candidates[0].starts_with(&target),
                "CARGO_TARGET_DIR must win: {candidates:?}"
            );
        }
    }
}
