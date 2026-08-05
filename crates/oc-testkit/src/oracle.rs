//! The oracle: the real `opencode`, run under a scripted environment.
//!
//! # Two flavours, and why the default is the installed binary
//!
//! The oracle exists in two forms, and they do not report the same version:
//!
//! | flavour | what runs | `--version` says |
//! |---|---|---|
//! | [`OracleFlavour::InstalledBinary`] | the released, bundled binary on `PATH` | its real release, e.g. `1.18.12` |
//! | [`OracleFlavour::FromSource`] | `bun run --conditions=browser ./src/index.ts` in the pinned tree | `local` |
//!
//! The from-source flavour cannot state a version because the version is a
//! build-time `define`: `packages/core/src/installation/version.ts` reads a
//! global `OPENCODE_VERSION` that only the bundler injects, and falls back to the
//! literal `"local"` otherwise.
//!
//! **The default is the installed binary**, for three reasons: it is the artifact
//! users actually run, so a difference against it is a difference users would
//! see; it self-reports a real version, so a report can name what it compared
//! against; and it is roughly twice as fast per invocation, which matters when
//! ninety later tasks each run it.
//!
//! **The cost of that default is a version gap**, and this module refuses to
//! paper over it. When the installed release and the pinned source tree disagree,
//! [`Oracle::version_gap`] says so and every [`Provenance::label`] carries both
//! numbers, so a differential failure can never be silently attributed to a
//! compatibility defect when it was a patch-level difference. A caller that needs
//! the pinned code exactly asks for [`Oracle::from_source`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::env::ScriptedEnv;
use crate::error::{Result, TestkitError};
use crate::run::{Provenance, RunOutcome, VersionGap, run_process};

/// Which form of the real `opencode` an [`Oracle`] drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleFlavour {
    /// A released, bundled binary.
    InstalledBinary,
    /// The pinned TypeScript source tree, executed with Bun.
    FromSource,
}

/// Override the discovered oracle binary with an explicit path.
pub const ENV_ORACLE_BINARY: &str = "OC_TESTKIT_ORACLE";
/// Point the harness at a specific `opencode` source tree.
pub const ENV_ORACLE_SOURCE: &str = "OC_TESTKIT_ORACLE_SOURCE";
/// Force a flavour: `binary` or `source`.
pub const ENV_ORACLE_FLAVOUR: &str = "OC_TESTKIT_ORACLE_FLAVOUR";

/// The real `opencode`, ready to run under a scripted environment.
#[derive(Debug)]
pub struct Oracle {
    flavour: OracleFlavour,
    program: PathBuf,
    args_prefix: Vec<String>,
    tree: Option<PathBuf>,
    pinned_version: Option<String>,
    pinned_commit: Option<String>,
    reported_version: String,
    env: ScriptedEnv,
}

impl Oracle {
    /// Locate an oracle, honouring the environment overrides.
    ///
    /// Order: [`ENV_ORACLE_BINARY`], then [`ENV_ORACLE_FLAVOUR`], then the
    /// installed binary on `PATH`, then the pinned source tree.
    ///
    /// # Errors
    ///
    /// [`TestkitError::BinaryNotFound`] naming every location consulted, when no
    /// oracle can be found.
    pub fn discover() -> Result<Self> {
        if let Ok(explicit) = std::env::var(ENV_ORACLE_BINARY) {
            return Self::at_binary(PathBuf::from(explicit));
        }
        match requested_flavour(std::env::var(ENV_ORACLE_FLAVOUR).ok().as_deref())? {
            Some(OracleFlavour::FromSource) => Self::from_source(require_tree()?),
            Some(OracleFlavour::InstalledBinary) => Self::installed_binary(),
            None => Self::auto(),
        }
    }

    fn auto() -> Result<Self> {
        let mut searched = Vec::new();
        match which::which("opencode") {
            Ok(found) => return Self::at_binary(found),
            Err(_) => searched.push(PathBuf::from("opencode (via PATH)")),
        }
        match locate_source_tree() {
            Some(tree) => Self::from_source(tree),
            None => {
                searched.push(PathBuf::from(
                    "<ancestor>/opencode/packages/opencode/package.json",
                ));
                Err(TestkitError::BinaryNotFound {
                    role: "oracle",
                    expected: PathBuf::from("opencode (via PATH)"),
                    searched,
                    remedy: format!(
                        "install the real opencode CLI, set {ENV_ORACLE_BINARY} to its path, or set \
                         {ENV_ORACLE_SOURCE} to a checkout of the opencode source tree"
                    ),
                })
            }
        }
    }

    /// Use the installed release binary found on `PATH`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::BinaryNotFound`] when `opencode` is not on `PATH`.
    pub fn installed_binary() -> Result<Self> {
        match which::which("opencode") {
            Ok(found) => Self::at_binary(found),
            Err(_) => Err(TestkitError::BinaryNotFound {
                role: "oracle",
                expected: PathBuf::from("opencode (via PATH)"),
                searched: vec![PathBuf::from(std::env::var("PATH").unwrap_or_default())],
                remedy: format!(
                    "install the real opencode CLI, or set {ENV_ORACLE_BINARY} to its path"
                ),
            }),
        }
    }

    /// Use the `opencode` binary at an exact path.
    ///
    /// # Errors
    ///
    /// [`TestkitError::BinaryNotFound`] naming `path` when it does not exist, or
    /// [`TestkitError::BinaryNotExecutable`] when it is not runnable.
    pub fn at_binary(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        ensure_executable("oracle", &path, || {
            format!(
                "build or install the real opencode CLI at that path, or point \
                 {ENV_ORACLE_BINARY} somewhere it exists"
            )
        })?;
        let tree = locate_source_tree();
        let mut oracle = Self {
            flavour: OracleFlavour::InstalledBinary,
            program: path,
            args_prefix: Vec::new(),
            pinned_version: tree.as_deref().and_then(read_pinned_version),
            pinned_commit: tree.as_deref().and_then(read_head_commit),
            tree,
            reported_version: String::new(),
            env: ScriptedEnv::new()?,
        };
        oracle.reported_version = oracle.probe_version()?;
        Ok(oracle)
    }

    /// Run the pinned TypeScript source tree with Bun.
    ///
    /// # Errors
    ///
    /// [`TestkitError::BinaryNotFound`] naming the entry point or `bun` when
    /// either is missing.
    pub fn from_source(tree: impl Into<PathBuf>) -> Result<Self> {
        let tree = tree.into();
        let entry = tree.join("packages/opencode/src/index.ts");
        if !entry.is_file() {
            return Err(TestkitError::BinaryNotFound {
                role: "oracle",
                expected: entry,
                searched: vec![tree.clone()],
                remedy: format!(
                    "set {ENV_ORACLE_SOURCE} to a checkout of the opencode source tree that \
                     contains packages/opencode/src/index.ts"
                ),
            });
        }
        let bun = which::which("bun").map_err(|_| TestkitError::BinaryNotFound {
            role: "oracle",
            expected: PathBuf::from("bun (via PATH)"),
            searched: vec![PathBuf::from(std::env::var("PATH").unwrap_or_default())],
            remedy: "install bun; the from-source oracle is a TypeScript entry point".to_owned(),
        })?;
        let mut oracle = Self {
            flavour: OracleFlavour::FromSource,
            program: bun,
            args_prefix: vec![
                "run".to_owned(),
                "--conditions=browser".to_owned(),
                entry.to_string_lossy().into_owned(),
            ],
            pinned_version: read_pinned_version(&tree),
            pinned_commit: read_head_commit(&tree),
            tree: Some(tree),
            reported_version: String::new(),
            env: ScriptedEnv::new()?,
        };
        oracle.reported_version = oracle.probe_version()?;
        Ok(oracle)
    }

    /// Replace the scripted environment, e.g. with one a
    /// [`ConfigFixture`](crate::ConfigFixture) built.
    #[must_use]
    pub fn with_env(mut self, env: ScriptedEnv) -> Self {
        self.env = env;
        self
    }

    /// The scripted environment this oracle runs under.
    #[must_use]
    pub fn env(&self) -> &ScriptedEnv {
        &self.env
    }

    /// Which flavour is in use.
    #[must_use]
    pub fn flavour(&self) -> &OracleFlavour {
        &self.flavour
    }

    /// The program that will be executed.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The pinned source tree, when one was located.
    #[must_use]
    pub fn source_tree(&self) -> Option<&Path> {
        self.tree.as_deref()
    }

    /// The provenance stamped onto every outcome this oracle produces.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        match self.flavour {
            OracleFlavour::InstalledBinary => Provenance::OracleInstalledBinary {
                program: self.program.clone(),
                reported_version: self.reported_version.clone(),
                pinned_source_version: self.pinned_version.clone(),
                pinned_source_commit: self.pinned_commit.clone(),
            },
            OracleFlavour::FromSource => Provenance::OracleFromSource {
                tree: self.tree.clone().unwrap_or_else(|| self.program.clone()),
                reported_version: self.reported_version.clone(),
                pinned_source_version: self.pinned_version.clone(),
                pinned_source_commit: self.pinned_commit.clone(),
            },
        }
    }

    /// The version this oracle reported for itself.
    #[must_use]
    pub fn reported_version(&self) -> &str {
        &self.reported_version
    }

    /// The distance between the running oracle and the pinned source tree.
    #[must_use]
    pub fn version_gap(&self) -> VersionGap {
        VersionGap {
            reported: self.reported_version.clone(),
            pinned: self.pinned_version.clone(),
        }
    }

    /// The recordings root inside the located source tree.
    #[must_use]
    pub fn recordings_root(&self) -> Option<PathBuf> {
        self.tree
            .as_ref()
            .map(|t| t.join("packages/llm/test/fixtures/recordings"))
            .filter(|p| p.is_dir())
    }

    /// Run the oracle with `args`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::Spawn`] when the process cannot be started or waited on.
    pub fn run<I, S>(&self, args: I) -> Result<RunOutcome>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let full = self.full_args(args);
        run_process(
            self.provenance(),
            &self.program,
            &full,
            self.env.working_dir(),
            &self.env.env_vars(),
        )
    }

    fn full_args<I, S>(&self, args: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut full = self.args_prefix.clone();
        full.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        full
    }

    fn probe_version(&self) -> Result<String> {
        let args = self.full_args(["--version"]);
        let probe_env: BTreeMap<String, String> = self.env.env_vars();
        let outcome = run_process(
            self.provenance(),
            &self.program,
            &args,
            self.env.working_dir(),
            &probe_env,
        )?;
        let version = outcome
            .stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned();
        if version.is_empty() {
            return Err(TestkitError::VersionProbeFailed {
                role: "oracle",
                program: self.program.clone(),
                stdout: outcome.stdout,
                stderr: outcome.stderr,
            });
        }
        Ok(version)
    }
}

/// Interpret [`ENV_ORACLE_FLAVOUR`], rejecting a value this harness does not
/// implement rather than silently picking a default.
///
/// Split out as a pure function because a test cannot set an environment variable
/// in this workspace: Rust 2024 makes `std::env::set_var` `unsafe`, and the
/// workspace forbids `unsafe_code`.
///
/// # Errors
///
/// [`TestkitError::UnknownOracleFlavour`] for anything other than `binary`,
/// `source`, or absent.
pub fn requested_flavour(value: Option<&str>) -> Result<Option<OracleFlavour>> {
    match value {
        None => Ok(None),
        Some("source") => Ok(Some(OracleFlavour::FromSource)),
        Some("binary") => Ok(Some(OracleFlavour::InstalledBinary)),
        Some(other) => Err(TestkitError::UnknownOracleFlavour {
            requested: other.to_owned(),
            accepted: &["binary", "source"],
        }),
    }
}

pub(crate) fn ensure_executable(
    role: &'static str,
    path: &Path,
    remedy: impl Fn() -> String,
) -> Result<()> {
    if !path.exists() {
        return Err(TestkitError::BinaryNotFound {
            role,
            expected: path.to_path_buf(),
            searched: Vec::new(),
            remedy: remedy(),
        });
    }
    if path.is_dir() {
        return Err(TestkitError::BinaryNotExecutable {
            role,
            path: path.to_path_buf(),
            detail: "is a directory".to_owned(),
        });
    }
    Ok(())
}

fn require_tree() -> Result<PathBuf> {
    locate_source_tree().ok_or_else(|| TestkitError::BinaryNotFound {
        role: "oracle",
        expected: PathBuf::from("<ancestor>/opencode/packages/opencode/src/index.ts"),
        searched: vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))],
        remedy: format!("set {ENV_ORACLE_SOURCE} to a checkout of the opencode source tree"),
    })
}

/// Find the pinned `opencode` source tree.
///
/// Honours [`ENV_ORACLE_SOURCE`], otherwise walks up from this crate looking for
/// a sibling `opencode` checkout. The walk is what makes the harness work
/// unchanged from the main checkout and from a `git worktree` beside it.
#[must_use]
pub fn locate_source_tree() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(ENV_ORACLE_SOURCE) {
        let path = PathBuf::from(explicit);
        return path
            .join("packages/opencode/package.json")
            .is_file()
            .then_some(path);
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.ancestors()
        .map(|a| a.join("opencode"))
        .find(|c| c.join("packages/opencode/package.json").is_file())
}

fn read_pinned_version(tree: &Path) -> Option<String> {
    let text = std::fs::read_to_string(tree.join("packages/opencode/package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("version")?.as_str().map(str::to_owned)
}

fn read_head_commit(tree: &Path) -> Option<String> {
    let git = tree.join(".git");
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let sha = match head.strip_prefix("ref: ") {
        Some(reference) => std::fs::read_to_string(git.join(reference))
            .ok()?
            .trim()
            .to_owned(),
        None => head.to_owned(),
    };
    (sha.len() >= 10).then(|| sha[..10].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure QA scenario: an absent oracle must name the path it wanted.
    #[test]
    fn a_missing_oracle_binary_names_the_expected_path() {
        let missing = PathBuf::from("/nonexistent/oc-testkit/oracle/opencode");
        let err = Oracle::at_binary(&missing).expect_err("a missing path cannot be an oracle");
        let rendered = err.to_string();
        assert!(
            rendered.contains("/nonexistent/oc-testkit/oracle/opencode"),
            "the error must name the expected path, got: {rendered}"
        );
        assert!(
            rendered.contains("remedy:"),
            "the error must be actionable: {rendered}"
        );
        assert!(
            matches!(err, TestkitError::BinaryNotFound { role: "oracle", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_directory_is_not_an_oracle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = Oracle::at_binary(dir.path()).expect_err("a directory cannot be an oracle");
        assert!(
            matches!(err, TestkitError::BinaryNotExecutable { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_source_tree_without_the_entry_point_is_rejected_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = Oracle::from_source(dir.path()).expect_err("an empty tree has no entry point");
        assert!(
            err.to_string().contains("packages/opencode/src/index.ts"),
            "{err}"
        );
    }

    #[test]
    fn the_flavour_override_accepts_two_values_and_refuses_the_rest() {
        assert_eq!(requested_flavour(None).expect("absent is valid"), None);
        assert_eq!(
            requested_flavour(Some("binary")).expect("binary is valid"),
            Some(OracleFlavour::InstalledBinary)
        );
        assert_eq!(
            requested_flavour(Some("source")).expect("source is valid"),
            Some(OracleFlavour::FromSource)
        );
        for typo in ["Binary", "from-source", "installed", ""] {
            let err = requested_flavour(Some(typo))
                .expect_err("an unimplemented flavour must not fall back silently");
            let rendered = err.to_string();
            assert!(rendered.contains("binary, source"), "{rendered}");
            assert!(
                matches!(err, TestkitError::UnknownOracleFlavour { .. }),
                "{err:?}"
            );
        }
    }

    /// The oracle tree is a hard requirement of this project: the plan pins it at
    /// 1.18.13 and ninety later tasks read fixtures out of it. A machine without
    /// it cannot verify anything, so this fails loudly rather than skipping.
    #[test]
    fn the_pinned_source_tree_is_locatable_and_states_its_version() {
        let tree = locate_source_tree()
            .expect("no opencode source tree found; set OC_TESTKIT_ORACLE_SOURCE to a checkout");
        let version = read_pinned_version(&tree).expect("the tree must declare a version");
        assert!(
            version.starts_with("1."),
            "unexpected pinned version {version} in {}",
            tree.display()
        );
        assert!(
            tree.join("packages/llm/test/fixtures/recordings").is_dir(),
            "the tree must carry the recorded provider traffic"
        );
    }
}
