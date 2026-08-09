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
//!
//! # Two pins, and they are not the same number
//!
//! | pin | what it names | where it lives |
//! |---|---|---|
//! | [`PINNED_RELEASE`] | the installed binary every differential runs against | this module, verified by [`Oracle::discover_pinned`] |
//! | the source baseline | the TypeScript tree this port was read from, and the version it reports to the npm plugin gate | `packages/opencode/package.json` in the located tree, and `oc_plugin::js::spec::REPORTED_PLUGIN_API_VERSION` |
//!
//! They are currently `1.18.15` and `1.18.13`. Conflating them is what produced
//! the artifact F1 rejected — a report recording the source baseline as though it
//! were the binary that ran. [`Oracle::version_gap`] exists to keep the difference
//! visible rather than to excuse it.

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

/// The installed release every compatibility claim in this workspace is measured
/// against: the newest `opencode` release installed on this machine.
///
/// # Why this is declared here and verified, rather than discovered
///
/// A differential against "whatever `opencode` is on `PATH`" is a differential
/// against an unknown, so a report that cannot name a version cannot support a
/// compatibility claim. But a *named* version that nothing checks is worse than
/// no name at all: until plan todo 130 the harness recorded `1.18.13` while
/// resolving the installed `1.18.12`, and every artifact produced under that
/// recording attributed its measurements to a build that never ran.
///
/// So the version is declared once, here, and [`Oracle::discover_pinned`] refuses
/// any binary that does not self-report it. The binary is still *found* by
/// [`Oracle::discover`] — by [`ENV_ORACLE_BINARY`], else on `PATH` — because
/// hard-coding an installation path pins the harness to one machine's package
/// manager. What is pinned is the release, not the route to it.
///
/// Moving this constant means recapturing every artifact measured against it. See
/// `.omo/evidence/task-130-opencode-rust.txt` for what 1.18.15 was measured to
/// produce.
pub const PINNED_RELEASE: &str = "1.18.15";

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

    /// [`Self::discover`], then refuse the result unless it reports [`PINNED_RELEASE`].
    ///
    /// This is the constructor every gate should use. [`Self::discover`] answers
    /// "is there an oracle?"; this one answers "is it *the* oracle?", which is the
    /// question a recorded version has to be able to survive.
    ///
    /// # Errors
    ///
    /// [`TestkitError::BinaryNotFound`] when no oracle exists at all, so a caller
    /// can still distinguish absence (skippable) from disagreement (not), and
    /// [`TestkitError::OraclePinMismatch`] naming both versions and the program when
    /// the resolved binary is a different release.
    pub fn discover_pinned() -> Result<Self> {
        let oracle = Self::discover()?;
        check_pin(&oracle.reported_version, &oracle.program)?;
        Ok(oracle)
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

/// Accept `reported` only if it is [`PINNED_RELEASE`].
///
/// Split out of [`Oracle::discover_pinned`] as a pure function for the same reason
/// [`requested_flavour`] is one: a test cannot set an environment variable in this
/// workspace to redirect discovery, so the refusal has to be reachable with a
/// version string a test produced from a real process it controls.
///
/// # Errors
///
/// [`TestkitError::OraclePinMismatch`] naming both versions and `program`.
pub fn check_pin(reported: &str, program: &Path) -> Result<()> {
    if reported == PINNED_RELEASE {
        return Ok(());
    }
    Err(TestkitError::OraclePinMismatch {
        pinned: PINNED_RELEASE,
        reported: reported.to_owned(),
        program: program.to_path_buf(),
        binary_env: ENV_ORACLE_BINARY,
    })
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

    /// The oracle tree is a hard requirement of this project: it is the source
    /// baseline this port was read from and ninety later tasks read fixtures out of
    /// it. A machine without it cannot verify anything, so this fails loudly rather
    /// than skipping. Note this is the *source* pin, not [`PINNED_RELEASE`] — see
    /// the module docs' "two pins" table.
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

    /// The mismatch F1 rejected on, made a test.
    ///
    /// The assertion is deliberately **not** between two written-down constants:
    /// that would only prove two hand-typed strings match. The right-hand side is
    /// [`Oracle::reported_version`], which is the trimmed first line of stdout from
    /// actually executing the resolved binary with `--version` in
    /// [`Oracle::probe_version`]. So the only way to pass is for the declared pin to
    /// equal what the binary says about itself.
    ///
    /// Absence is skipped and disagreement is fatal, because those are different
    /// facts: a machine without `opencode` cannot verify anything, while a machine
    /// with the *wrong* `opencode` will produce artifacts that name a build that
    /// never ran.
    #[test]
    fn the_declared_pin_equals_the_version_the_resolved_binary_reports() {
        let oracle = match Oracle::discover() {
            Ok(oracle) => oracle,
            Err(TestkitError::BinaryNotFound { .. }) => {
                eprintln!(
                    "SKIPPED the_declared_pin_equals_the_version_the_resolved_binary_reports: no \
                     opencode on PATH and no {ENV_ORACLE_BINARY}; the pin was NOT verified"
                );
                return;
            }
            Err(other) => {
                panic!("resolving the oracle failed for a reason other than absence: {other}")
            }
        };
        assert_eq!(
            oracle.reported_version(),
            PINNED_RELEASE,
            "PINNED_RELEASE claims {PINNED_RELEASE} but {} reports {}. Every artifact in this \
             workspace attributes its measurements to PINNED_RELEASE, so this disagreement makes \
             all of them name a build that did not run. Install {PINNED_RELEASE}, or move the \
             constant and recapture.",
            oracle.program().display(),
            oracle.reported_version()
        );

        let pinned =
            Oracle::discover_pinned().expect("the agreeing oracle must also pass the gate");
        assert_eq!(pinned.reported_version(), PINNED_RELEASE);
    }

    /// The failure QA scenario for the pin: a binary that reports another release is
    /// refused and named, not accepted with a warning.
    ///
    /// The wrong-release binary is a real executable this test writes and
    /// [`Oracle::at_binary`] really runs, so the refused version string comes out of
    /// [`Oracle::probe_version`]'s stdout rather than being handed to [`check_pin`]
    /// as a literal. That is what makes this a test of the gate and not of
    /// [`TestkitError`]'s `Display`.
    #[cfg(unix)]
    #[test]
    fn a_binary_reporting_another_release_is_named_and_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("opencode");
        std::fs::write(&fake, "#!/bin/sh\necho 1.18.12\n").expect("write the stand-in release");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in executable");

        let oracle = Oracle::at_binary(&fake).expect("a runnable file is an oracle");
        assert_eq!(
            oracle.reported_version(),
            "1.18.12",
            "the probe must read the stand-in's own output"
        );

        let err = check_pin(oracle.reported_version(), oracle.program())
            .expect_err("a release other than the pin must be refused");
        let rendered = err.to_string();
        assert!(rendered.contains(PINNED_RELEASE), "{rendered}");
        assert!(rendered.contains("1.18.12"), "{rendered}");
        assert!(rendered.contains("opencode"), "{rendered}");
        assert!(rendered.contains(ENV_ORACLE_BINARY), "{rendered}");
        assert!(
            matches!(err, TestkitError::OraclePinMismatch { .. }),
            "{err:?}"
        );
    }
}
