//! Running one side of a differential and carrying its provenance forward.
//!
//! A [`RunOutcome`] is deliberately more than stdout. It records *what was run*
//! — which binary, which oracle flavour, which self-reported version — because a
//! differential failure whose report says only "line 3 differs" invites the wrong
//! diagnosis. When the installed oracle is a patch behind the pinned source tree,
//! that fact must be printed at the site of the failure, not remembered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Result, TestkitError};

/// Which side of the differential produced an outcome, and in what flavour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The real `opencode`, as an installed release binary.
    OracleInstalledBinary {
        /// The binary that ran.
        program: PathBuf,
        /// What `--version` printed.
        reported_version: String,
        /// `version` from the pinned source tree's `packages/opencode/package.json`.
        pinned_source_version: Option<String>,
        /// The pinned source tree's `HEAD` commit, abbreviated.
        pinned_source_commit: Option<String>,
    },
    /// The real `opencode`, executed from the pinned source tree via Bun.
    OracleFromSource {
        /// The source tree that was run.
        tree: PathBuf,
        /// What `--version` printed. Builds from source report `local`, because
        /// the version is a compile-time `define`
        /// (`packages/core/src/installation/version.ts`).
        reported_version: String,
        /// `version` from that tree's `packages/opencode/package.json`.
        pinned_source_version: Option<String>,
        /// That tree's `HEAD` commit, abbreviated.
        pinned_source_commit: Option<String>,
    },
    /// This project's Rust binary.
    Subject {
        /// The binary that ran.
        program: PathBuf,
        /// How the harness obtained the binary.
        source: SubjectSource,
        /// What `--version` printed, when it was probed.
        reported_version: Option<String>,
    },
}

/// How the subject binary entered the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectSource {
    /// A caller deliberately supplied an environment override.
    ExplicitEnvironment {
        /// The environment variable naming the binary.
        variable: &'static str,
    },
    /// A caller deliberately supplied a path to [`crate::Subject::at`].
    ExplicitPath,
    /// [`crate::Subject::discover`] found an existing, caller-managed artifact.
    WorkspaceArtifact,
    /// Cargo checked the current workspace sources before the binary was used.
    CargoBuild {
        /// The Cargo package that owns the binary.
        package: &'static str,
        /// The binary target Cargo built.
        binary: &'static str,
    },
}

impl SubjectSource {
    fn label(&self) -> String {
        match self {
            Self::ExplicitEnvironment { variable } => {
                format!("explicit environment {variable}")
            }
            Self::ExplicitPath => "explicit path".to_owned(),
            Self::WorkspaceArtifact => "pre-existing workspace artifact".to_owned(),
            Self::CargoBuild { package, binary } => {
                format!("cargo build -p {package} --bin {binary}")
            }
        }
    }
}

impl Provenance {
    /// A one-line description suitable as a diff label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::OracleInstalledBinary {
                program,
                reported_version,
                pinned_source_version,
                pinned_source_commit,
            } => format!(
                "oracle(installed-binary {}, reports {reported_version}, pinned source {} @ {})",
                program.display(),
                pinned_source_version.as_deref().unwrap_or("unknown"),
                pinned_source_commit.as_deref().unwrap_or("unknown"),
            ),
            Self::OracleFromSource {
                tree,
                reported_version,
                pinned_source_version,
                pinned_source_commit,
            } => format!(
                "oracle(from-source {}, reports {reported_version}, pinned source {} @ {})",
                tree.display(),
                pinned_source_version.as_deref().unwrap_or("unknown"),
                pinned_source_commit.as_deref().unwrap_or("unknown"),
            ),
            Self::Subject {
                program,
                source,
                reported_version,
            } => format!(
                "subject({} -> {}, reports {})",
                source.label(),
                program.display(),
                reported_version.as_deref().unwrap_or("unprobed"),
            ),
        }
    }

    /// The version this side reported for itself, if it was probed.
    #[must_use]
    pub fn reported_version(&self) -> Option<&str> {
        match self {
            Self::OracleInstalledBinary {
                reported_version, ..
            }
            | Self::OracleFromSource {
                reported_version, ..
            } => Some(reported_version),
            Self::Subject {
                reported_version, ..
            } => reported_version.as_deref(),
        }
    }

    /// The version pinned by the oracle source tree, if this is an oracle.
    #[must_use]
    pub fn pinned_source_version(&self) -> Option<&str> {
        match self {
            Self::OracleInstalledBinary {
                pinned_source_version,
                ..
            }
            | Self::OracleFromSource {
                pinned_source_version,
                ..
            } => pinned_source_version.as_deref(),
            Self::Subject { .. } => None,
        }
    }
}

/// The distance between what the oracle *reports* and what the pinned source tree
/// says, surfaced as data so it is never silently assumed away.
///
/// The installed release on a machine is routinely a patch behind the pinned
/// tree. That is a fact about the environment, not a defect, and it belongs in
/// every differential report so a failure is attributed correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionGap {
    /// What the oracle printed for `--version`.
    pub reported: String,
    /// What the pinned source tree declares.
    pub pinned: Option<String>,
}

impl VersionGap {
    /// True when the running oracle is exactly the pinned version.
    #[must_use]
    pub fn is_aligned(&self) -> bool {
        self.pinned.as_deref() == Some(self.reported.as_str())
    }

    /// True when the oracle cannot report a comparable version at all.
    ///
    /// A from-source oracle always lands here: its version is injected by the
    /// bundler, so an unbundled run self-reports `local`.
    #[must_use]
    pub fn is_unversioned(&self) -> bool {
        self.reported == "local"
    }

    /// A sentence naming the gap, for a report header.
    #[must_use]
    pub fn describe(&self) -> String {
        let pinned = self.pinned.as_deref().unwrap_or("unknown");
        if self.is_unversioned() {
            format!(
                "oracle self-reports {reported:?} (version is a build-time define, so an \
                 unbundled run cannot state one); pinned source is {pinned}",
                reported = self.reported
            )
        } else if self.is_aligned() {
            format!("oracle {} matches the pinned source", self.reported)
        } else {
            format!(
                "oracle reports {} but the pinned source is {pinned}; a differential failure here \
                 may be a version gap rather than a compatibility defect",
                self.reported
            )
        }
    }
}

/// One completed process run, with everything needed to interpret it.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// Which side ran, and in what flavour.
    pub provenance: Provenance,
    /// The program that was executed.
    pub program: PathBuf,
    /// Its full argument vector, including any interpreter prefix.
    pub args: Vec<String>,
    /// The working directory it ran in.
    pub working_dir: PathBuf,
    /// Its exit code, or `None` if it was killed by a signal.
    pub exit_code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// The exact environment it received.
    pub env: BTreeMap<String, String>,
}

impl RunOutcome {
    /// True when the process exited zero.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// The diff label for this side, which names the flavour and version.
    #[must_use]
    pub fn label(&self) -> String {
        self.provenance.label()
    }

    /// stdout with the trailing newline removed.
    #[must_use]
    pub fn stdout_trimmed(&self) -> &str {
        self.stdout.trim_end_matches(['\n', '\r'])
    }

    /// A full transcript, including provenance, for pasting into evidence.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "$ {} {}", self.program.display(), self.args.join(" "));
        let _ = writeln!(out, "  provenance: {}", self.label());
        let _ = writeln!(out, "  cwd: {}", self.working_dir.display());
        let _ = writeln!(out, "  exit: {:?}", self.exit_code);
        let _ = writeln!(out, "  stdout: {:?}", self.stdout);
        let _ = writeln!(out, "  stderr: {:?}", self.stderr);
        out
    }
}

/// Execute `program` with `args` under `env`, capturing both streams.
pub(crate) fn run_process(
    provenance: Provenance,
    program: &Path,
    args: &[String],
    working_dir: &Path,
    env: &BTreeMap<String, String>,
) -> Result<RunOutcome> {
    let mut command = Command::new(program);
    command.args(args).current_dir(working_dir).env_clear();
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().map_err(|source| TestkitError::Spawn {
        program: program.to_path_buf(),
        args: args.to_vec(),
        source,
    })?;
    Ok(RunOutcome {
        provenance,
        program: program.to_path_buf(),
        args: args.to_vec(),
        working_dir: working_dir.to_path_buf(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        env: env.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(reported: &str, pinned: Option<&str>) -> Provenance {
        Provenance::OracleInstalledBinary {
            program: PathBuf::from("/usr/bin/opencode"),
            reported_version: reported.to_owned(),
            pinned_source_version: pinned.map(str::to_owned),
            pinned_source_commit: Some("aefaf140c1".to_owned()),
        }
    }

    #[test]
    fn a_label_names_the_flavour_and_both_versions() {
        let label = installed("1.18.12", Some("1.18.13")).label();
        assert!(label.contains("installed-binary"), "{label}");
        assert!(label.contains("reports 1.18.12"), "{label}");
        assert!(label.contains("pinned source 1.18.13"), "{label}");
        assert!(label.contains("aefaf140c1"), "{label}");
    }

    #[test]
    fn a_version_gap_is_named_not_hidden() {
        let gap = VersionGap {
            reported: "1.18.12".to_owned(),
            pinned: Some("1.18.13".to_owned()),
        };
        assert!(!gap.is_aligned());
        assert!(!gap.is_unversioned());
        let described = gap.describe();
        assert!(described.contains("1.18.12"), "{described}");
        assert!(described.contains("1.18.13"), "{described}");
        assert!(described.contains("version gap"), "{described}");
    }

    #[test]
    fn an_aligned_oracle_says_so() {
        let gap = VersionGap {
            reported: "1.18.13".to_owned(),
            pinned: Some("1.18.13".to_owned()),
        };
        assert!(gap.is_aligned());
        assert!(gap.describe().contains("matches the pinned source"));
    }

    #[test]
    fn a_from_source_oracle_is_reported_as_unversioned() {
        let gap = VersionGap {
            reported: "local".to_owned(),
            pinned: Some("1.18.13".to_owned()),
        };
        assert!(gap.is_unversioned());
        assert!(!gap.is_aligned());
        assert!(gap.describe().contains("build-time define"));
    }

    #[test]
    fn running_a_process_captures_streams_and_the_exact_environment() {
        let env: BTreeMap<String, String> = [("ZUNO_TESTKIT_PROBE".to_owned(), "yes".to_owned())]
            .into_iter()
            .collect();
        let outcome = run_process(
            Provenance::Subject {
                program: PathBuf::from("/usr/bin/env"),
                source: SubjectSource::ExplicitPath,
                reported_version: None,
            },
            Path::new("/usr/bin/env"),
            &[],
            Path::new("/"),
            &env,
        )
        .expect("env(1) should run");
        assert!(outcome.is_success());
        assert!(
            outcome.stdout.contains("ZUNO_TESTKIT_PROBE=yes"),
            "{}",
            outcome.render()
        );
        // env_clear means nothing else leaks in.
        assert!(
            !outcome.stdout.contains("XDG_DATA_HOME="),
            "{}",
            outcome.render()
        );
        assert_eq!(outcome.env, env);
    }

    #[test]
    fn a_missing_program_is_a_typed_spawn_failure() {
        let err = run_process(
            Provenance::Subject {
                program: PathBuf::from("/nonexistent/zuno-testkit-probe"),
                source: SubjectSource::ExplicitPath,
                reported_version: None,
            },
            Path::new("/nonexistent/zuno-testkit-probe"),
            &["--version".to_owned()],
            Path::new("/"),
            &BTreeMap::new(),
        )
        .expect_err("a missing program cannot run");
        assert!(matches!(err, TestkitError::Spawn { .. }), "{err:?}");
        assert!(
            err.to_string().contains("/nonexistent/zuno-testkit-probe"),
            "{err}"
        );
    }
}
