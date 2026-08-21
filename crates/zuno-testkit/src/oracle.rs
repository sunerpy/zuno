//! Optional upstream research runner.
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
//! global `ZUNO_VERSION` that only the bundler injects, and falls back to the
//! literal `"local"` otherwise.
//!
//! **The default is the installed binary** because performance baselines need the
//! released process tree and every recorded result must name the executable that
//! produced it.
//!
//! **The cost of that default is a version gap**, and this module refuses to
//! paper over it. When the installed release and the pinned source tree disagree,
//! [`Oracle::version_gap`] says so and every [`Provenance::label`] carries both
//! numbers, so research output cannot silently attribute a measurement to the
//! wrong release. A caller that needs the pinned code exactly asks for
//! [`Oracle::from_source`].
//!
//! # Two pins, and they are not the same number
//!
//! | pin | what it names | where it lives |
//! |---|---|---|
//! | [`PINNED_RELEASE`] | the installed binary used by frozen research artifacts | this module, verified by [`Oracle::discover_pinned`] |
//! | the source baseline | the TypeScript tree from which provider cassettes were recorded | `packages/opencode/package.json` in the located tree |
//!
//! They are currently `1.18.18` and `1.18.13`. Conflating them is what produced
//! the artifact F1 rejected — a report recording the source baseline as though it
//! were the binary that ran. [`Oracle::version_gap`] exists to keep the difference
//! visible rather than to excuse it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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

/// The installed release used by frozen upstream research artifacts.
///
/// # Why this is declared here and verified, rather than discovered
///
/// A differential against "whatever `opencode` is on `PATH`" is a differential
/// against an unknown, so a report that cannot name a version cannot support a
/// measurement. But a *named* version that nothing checks is worse than
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
pub const PINNED_RELEASE: &str = "1.18.18";

/// Override the discovered oracle binary with an explicit path.
pub const ENV_ORACLE_BINARY: &str = "ZUNO_TESTKIT_ORACLE";
/// Point the harness at a specific `opencode` source tree.
pub const ENV_ORACLE_SOURCE: &str = "ZUNO_TESTKIT_ORACLE_SOURCE";
/// Force a flavour: `binary` or `source`.
pub const ENV_ORACLE_FLAVOUR: &str = "ZUNO_TESTKIT_ORACLE_FLAVOUR";

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

// ---------------------------------------------------------------------------
// Resolving an executable a differential can actually run
// ---------------------------------------------------------------------------

/// The result of looking for an installed release a differential can run.
///
/// Absence and disagreement are separate variants because they are separate facts
/// and deserve opposite treatment: a machine without `opencode` cannot verify
/// anything and may skip, while a machine whose `opencode` is a *different release*
/// would produce artifacts naming a build that never ran.
#[derive(Debug)]
pub enum PinnedOracle {
    /// An executable that reported [`PINNED_RELEASE`] under a scripted world.
    Found(PathBuf),
    /// No `opencode` exists in any location consulted. Skippable.
    Absent(String),
    /// An `opencode` exists, but no candidate is [`PINNED_RELEASE`] — or none could
    /// report a version at all. Not skippable: the message names every candidate.
    Disagrees(String),
}

/// An installed `opencode` that reports [`PINNED_RELEASE`] **when run the way a
/// differential runs it**, resolved once per test process.
///
/// # Why this exists next to [`Oracle::discover_pinned`]
///
/// [`Oracle::discover_pinned`] answers "is the oracle *the* oracle?" for callers
/// that drive it through [`Oracle::run`], which always executes in the scripted
/// world's own temporary working directory. Many differentials cannot use that:
/// they build a `std::process::Command` themselves, because they need
/// `--format json`, an `env_clear` with a stripped `PATH`, or a database at a path
/// they chose. Those run the binary from **the test process's working directory**,
/// which is inside this repository.
///
/// That difference is load-bearing, and it is why nine test files used to hard-code
/// `…/mise/installs/opencode/1.18.12/opencode`. On a machine where `opencode` is
/// reached through a package-manager shim, the first `PATH` hit is not the binary at
/// all: it is the package manager, which re-execs. This host's shim is a symlink to
/// `mise`, and `mise` resolves its own trust records under the **real** `HOME`, so
/// under a differential's redirected `HOME` it exits with
/// `Config files … are not trusted` and prints nothing on stdout. The hard-coded
/// paths were a workaround for that, and the workaround is what let a differential
/// silently measure 1.18.12 while every report attributed the result to
/// [`PINNED_RELEASE`].
///
/// So candidates are **discovered** — [`ENV_ORACLE_BINARY`], else every `opencode` on
/// `PATH` in `PATH` order — and each is **behaviourally screened**: run
/// `--version` under a [`ScriptedEnv`] from this process's own working directory, and
/// require the output to be [`PINNED_RELEASE`] via [`check_pin`]. A launcher that
/// cannot survive that is rejected for the reason it failed, and the next candidate
/// is tried. No version and no package manager is named in code; the release is
/// pinned, the route to it is not.
#[must_use]
pub fn pinned_oracle() -> &'static PinnedOracle {
    static RESOLVED: OnceLock<PinnedOracle> = OnceLock::new();
    RESOLVED.get_or_init(resolve_pinned_oracle)
}

/// [`pinned_oracle`], reduced to the path a differential needs, with the skip
/// contract applied.
///
/// * found — `Some(path)`, an executable already proven to report [`PINNED_RELEASE`]
///   from this working directory;
/// * absent — `None`, after printing `SKIPPED {test}: … {untested}` so the output
///   says what was *not* measured rather than looking like a pass;
/// * disagreement — **panics**, because continuing would measure one release and
///   report another.
///
/// `untested` completes the sentence "…; {untested}", e.g.
/// `"the rollback seam was NOT tested"`.
///
/// # Panics
///
/// When an `opencode` exists but no candidate is [`PINNED_RELEASE`].
#[must_use]
pub fn pinned_oracle_or_skip(test: &str, untested: &str) -> Option<&'static Path> {
    match pinned_oracle() {
        PinnedOracle::Found(program) => Some(program.as_path()),
        PinnedOracle::Absent(reason) => {
            eprintln!("SKIPPED {test}: {reason}; {untested}");
            None
        }
        PinnedOracle::Disagrees(reason) => panic!("{reason}"),
    }
}

/// Every path worth screening, in preference order.
///
/// An explicit [`ENV_ORACLE_BINARY`] is the only candidate when it is set: an
/// operator naming a binary that turns out to be the wrong release must be told so,
/// not quietly routed around.
fn pinned_oracle_candidates() -> Vec<PathBuf> {
    if let Ok(explicit) = std::env::var(ENV_ORACLE_BINARY) {
        return vec![PathBuf::from(explicit)];
    }
    let mut candidates = Vec::new();
    if let Ok(found) = which::which_all("opencode") {
        for path in found {
            if !candidates.contains(&path) {
                candidates.push(path);
            }
        }
    }
    candidates
}

/// What `candidate --version` prints when run from **this** working directory under a
/// scripted world.
///
/// The working directory is deliberately not overridden. That is the whole point:
/// [`Oracle::probe_version`] runs in the scripted world's temporary directory, where
/// a package-manager shim can still work, so a probe that did the same would accept a
/// launcher that then fails in every caller.
fn version_from_this_working_directory(candidate: &Path) -> Result<String> {
    ensure_executable("oracle", candidate, || {
        format!("install the real opencode CLI, or point {ENV_ORACLE_BINARY} somewhere it exists")
    })?;
    let env = ScriptedEnv::new()?;
    let mut command = Command::new(candidate);
    command.arg("--version").env_clear();
    for (key, value) in &env.env_vars() {
        command.env(key, value);
    }
    let output = command.output().map_err(|source| TestkitError::Spawn {
        program: candidate.to_path_buf(),
        args: vec!["--version".to_owned()],
        source,
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let version = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned();
    if version.is_empty() {
        return Err(TestkitError::VersionProbeFailed {
            role: "oracle",
            program: candidate.to_path_buf(),
            stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(version)
}

fn resolve_pinned_oracle() -> PinnedOracle {
    screen_candidates(pinned_oracle_candidates())
}

/// Take the first candidate that executes and reports [`PINNED_RELEASE`].
///
/// Separated from discovery so the screen is observable on any host: whether
/// `PATH` happens to put the release before a launcher shim decides nothing about
/// whether the screen works, and a test that relied on that ordering would pass on
/// this machine while proving nothing.
fn screen_candidates(candidates: Vec<PathBuf>) -> PinnedOracle {
    if candidates.is_empty() {
        return PinnedOracle::Absent(format!(
            "no opencode on PATH and no {ENV_ORACLE_BINARY}; install release {PINNED_RELEASE} or \
             set {ENV_ORACLE_BINARY} to it"
        ));
    }
    let mut refusals = Vec::new();
    for candidate in candidates {
        match version_from_this_working_directory(&candidate) {
            Ok(reported) => match check_pin(&reported, &candidate) {
                Ok(()) => return PinnedOracle::Found(candidate),
                Err(mismatch) => refusals.push(mismatch.to_string()),
            },
            Err(unusable) => refusals.push(unusable.to_string()),
        }
    }
    PinnedOracle::Disagrees(format!(
        "no installed opencode reports {PINNED_RELEASE} when run from {}. Every artifact in this \
         workspace attributes its measurements to {PINNED_RELEASE}, so continuing would name a \
         build that did not run. Install {PINNED_RELEASE}, or set {ENV_ORACLE_BINARY} to it. \
         Candidates refused:\n  - {}",
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("<unknown working directory>"))
            .display(),
        refusals.join("\n  - "),
    ))
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
        let missing = PathBuf::from("/nonexistent/zuno-testkit/oracle/opencode");
        let err = Oracle::at_binary(&missing).expect_err("a missing path cannot be an oracle");
        let rendered = err.to_string();
        assert!(
            rendered.contains("/nonexistent/zuno-testkit/oracle/opencode"),
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

    /// Verify the source version whenever the complete recordings checkout is
    /// available. Automatic absence is a visible skip on clean build hosts, while
    /// an invalid explicit source remains fatal through [`crate::recordings_root_or_skip`].
    #[test]
    fn the_pinned_source_tree_is_locatable_and_states_its_version() {
        if crate::recordings_root_or_skip(
            "the_pinned_source_tree_is_locatable_and_states_its_version",
            "the pinned source version was NOT verified",
        )
        .is_none()
        {
            return;
        }
        let tree = locate_source_tree()
            .expect("no opencode source tree found; set ZUNO_TESTKIT_ORACLE_SOURCE to a checkout");
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

    /// The screen that replaced nine hard-coded package-manager paths.
    ///
    /// `--version` is re-run here through a raw [`Command`] with a cleared
    /// environment, a scripted `HOME`, and **this test's own working directory** —
    /// which is inside the repository, the condition under which this host's
    /// package-manager shim exits with a trust error and prints nothing. So the
    /// assertion fails if [`pinned_oracle`] ever hands back a launcher instead of the
    /// release, which is the mutant that matters: dropping the behavioural screen
    /// makes the first `PATH` hit win and this test go red.
    ///
    /// Absence is an ordinary skip, the same contract [`pinned_oracle_or_skip`]
    /// applies. It used to be a failure unless the since-removed
    /// `OC_TESTKIT_ALLOW_MISSING_ORACLE` was set — that spelling is recorded as it
    /// was and no longer names anything this crate reads — because measuring nothing
    /// against a real release was a fact a project
    /// claiming parity had to declare deliberately. Zuno makes no such claim, so a
    /// machine without `opencode` is now the normal case rather than an omission
    /// worth confessing — and demanding the variable only meant this unit test failed
    /// on every machine that had never installed the other program.
    #[test]
    fn the_resolved_pinned_oracle_reports_the_pin_from_this_working_directory() {
        let program = match pinned_oracle() {
            PinnedOracle::Found(program) => program.clone(),
            PinnedOracle::Absent(reason) => {
                eprintln!(
                    "SKIPPED the_resolved_pinned_oracle_reports_the_pin_from_this_working_directory: \
                     {reason}"
                );
                return;
            }
            PinnedOracle::Disagrees(reason) => panic!("{reason}"),
        };

        let env = ScriptedEnv::new().expect("a scripted world");
        let mut command = Command::new(&program);
        command.arg("--version").env_clear();
        for (key, value) in &env.env_vars() {
            command.env(key, value);
        }
        let output = command.output().expect("run the resolved oracle");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.lines().map(str::trim).find(|l| !l.is_empty()),
            Some(PINNED_RELEASE),
            "{} did not report {PINNED_RELEASE} from {}\nstdout:\n{stdout}\nstderr:\n{}",
            program.display(),
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("<unknown>"))
                .display(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// The route is discovered, not written down.
    ///
    /// The resolved executable may well sit in a version-named directory — on this
    /// host the only one that survives the screen does. What must not happen is a
    /// path reaching a caller that nothing on `PATH` or in [`ENV_ORACLE_BINARY`] put
    /// there, because that is a path some source file chose. Reintroducing a
    /// hard-coded fallback inside [`pinned_oracle_candidates`] makes this red.
    #[test]
    fn the_resolved_route_came_from_path_or_an_explicit_override() {
        let PinnedOracle::Found(program) = pinned_oracle() else {
            eprintln!(
                "SKIPPED the_resolved_route_came_from_path_or_an_explicit_override: no pinned \
                 oracle resolved"
            );
            return;
        };
        if let Ok(explicit) = std::env::var(ENV_ORACLE_BINARY) {
            assert_eq!(Path::new(&explicit), program.as_path());
            return;
        }
        let on_path: Vec<PathBuf> = which::which_all("opencode")
            .map(Iterator::collect)
            .unwrap_or_default();
        assert!(
            on_path.contains(program),
            "{} is not one of the {} opencode executables on PATH, so some source file chose it \
             rather than discovery: {on_path:?}",
            program.display(),
            on_path.len(),
        );
    }

    /// Write an executable that prints `stdout` and exits `code`.
    #[cfg(unix)]
    fn stand_in(dir: &Path, name: &str, stdout: &str, code: u8) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{stdout}\nexit {code}\n"))
            .expect("write the stand-in");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in executable");
        path
    }

    /// F4's finding, made executable: the screen must walk past a candidate that
    /// cannot report the pin and reach the one that can.
    ///
    /// Both rejects are **real executables this test writes and the screen really
    /// runs**. The first stands in for a package-manager launcher — it prints its
    /// complaint on stderr and nothing on stdout, which is what this host's shim does
    /// under a differential's redirected `HOME`. The second stands in for the release
    /// the nine hard-coded paths used to select: it reports cleanly, and is refused
    /// anyway because it is not the pin.
    ///
    /// This does not depend on where `PATH` happens to put the real binary, which is
    /// what makes it a test of the screen rather than of this machine's ordering.
    #[cfg(unix)]
    #[test]
    fn the_screen_walks_past_a_launcher_and_a_wrong_release_to_reach_the_pin() {
        let PinnedOracle::Found(real) = pinned_oracle() else {
            eprintln!(
                "SKIPPED the_screen_walks_past_a_launcher_and_a_wrong_release_to_reach_the_pin: no \
                 pinned oracle resolved; the screen was NOT exercised"
            );
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let launcher = stand_in(dir.path(), "launcher", ">&2 echo 'not trusted'", 1);
        let wrong = stand_in(dir.path(), "wrong-release", "echo 1.18.14", 0);

        match screen_candidates(vec![launcher.clone(), wrong.clone(), real.clone()]) {
            PinnedOracle::Found(found) => assert_eq!(
                &found,
                real,
                "the screen accepted {} instead of walking past it to the pinned release",
                found.display()
            ),
            other => panic!("the pinned release was in the list yet was not chosen: {other:?}"),
        }

        let refused = screen_candidates(vec![launcher.clone(), wrong.clone()]);
        let PinnedOracle::Disagrees(reason) = refused else {
            panic!("a list holding no pinned release must not resolve: {refused:?}");
        };
        assert!(
            reason.contains(&launcher.display().to_string()),
            "the refusal must name the launcher it could not use: {reason}"
        );
        assert!(
            reason.contains(&wrong.display().to_string()) && reason.contains("1.18.14"),
            "the refusal must name the wrong release and what it reported: {reason}"
        );
        assert!(reason.contains(PINNED_RELEASE), "{reason}");
    }

    /// The failure QA scenario for the pin: a binary that reports another release is
    /// refused and named, not accepted with a warning.
    ///
    /// Set `ZUNO_TESTKIT_MISMATCH_ORACLE` to a real older install during a re-pin. The
    /// hermetic fallback is a real executable this test writes, so either way
    /// [`Oracle::at_binary`] really runs a process and the refused version string
    /// comes out of [`Oracle::probe_version`]'s stdout rather than being handed to
    /// [`check_pin`] as a literal. That is what makes this a test of the gate and not
    /// of [`TestkitError`]'s `Display`.
    #[cfg(unix)]
    #[test]
    fn a_binary_reporting_another_release_is_named_and_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        const MISMATCH_ORACLE: &str = "ZUNO_TESTKIT_MISMATCH_ORACLE";
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("opencode");
        std::fs::write(&fake, "#!/bin/sh\necho 1.18.14\n").expect("write the stand-in release");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in executable");
        let candidate = std::env::var_os(MISMATCH_ORACLE)
            .map(PathBuf::from)
            .unwrap_or(fake);

        let oracle = Oracle::at_binary(&candidate).expect("a runnable file is an oracle");
        assert_eq!(
            oracle.reported_version(),
            "1.18.14",
            "the mismatch probe must execute a 1.18.14 binary; check {MISMATCH_ORACLE}"
        );

        let err = check_pin(oracle.reported_version(), oracle.program())
            .expect_err("a release other than the pin must be refused");
        let rendered = err.to_string();
        assert!(rendered.contains(PINNED_RELEASE), "{rendered}");
        assert!(rendered.contains("1.18.14"), "{rendered}");
        assert!(rendered.contains("opencode"), "{rendered}");
        assert!(rendered.contains(ENV_ORACLE_BINARY), "{rendered}");
        assert!(
            matches!(err, TestkitError::OraclePinMismatch { .. }),
            "{err:?}"
        );
    }
}
