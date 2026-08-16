//! Every way the harness itself can fail, as data.
//!
//! This crate does not use `anyhow`, even though the workspace guard in
//! `zuno-error` exempts it. A harness whose failures are opaque strings is a
//! harness that reports "something went wrong" at exactly the moment a
//! ninety-task verification chain needs to know *what*. Every variant below
//! carries the paths, names, and counts a caller (or a human reading a CI log)
//! needs in order to act, and there is deliberately no catch-all.
//!
//! The domain types in [`zuno_error`](https://docs.rs) are the right home for
//! failures that a *product* code path can produce. None of them describes
//! "the oracle binary this test suite compares against is not installed", so
//! that lives here instead of being forced into a shape it does not fit.

use std::path::PathBuf;

/// A failure originating inside the test harness.
#[derive(Debug, thiserror::Error)]
pub enum TestkitError {
    /// A binary the harness must execute was not found.
    ///
    /// Carries every location that was searched so the message can name the
    /// path a reader should create, and a concrete remedy.
    #[error(
        "{role} binary not found.\n  expected at: {expected}\n  also searched: {}\n  remedy: {remedy}",
        if searched.is_empty() { "(nothing else)".to_owned() } else { searched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ") }
    )]
    BinaryNotFound {
        /// Which side of the differential is missing: `"oracle"` or `"subject"`.
        role: &'static str,
        /// The path the harness expected to find, named verbatim in the message.
        expected: PathBuf,
        /// Every other location consulted, in the order consulted.
        searched: Vec<PathBuf>,
        /// The command or action that would make `expected` exist.
        remedy: String,
    },

    /// A path was found but is not usable as an executable.
    #[error("{role} binary at {path} is not executable ({detail})")]
    BinaryNotExecutable {
        /// Which side of the differential.
        role: &'static str,
        /// The offending path.
        path: PathBuf,
        /// What specifically was wrong (not a directory, no permission, ...).
        detail: String,
    },

    /// Spawning or waiting on a child process failed.
    #[error("failed to run {program} {}: {source}", args.join(" "))]
    Spawn {
        /// The program that could not be run.
        program: PathBuf,
        /// The arguments it would have received.
        args: Vec<String>,
        /// The underlying OS failure.
        #[source]
        source: std::io::Error,
    },

    /// A filesystem operation the harness needs failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What the harness was doing, e.g. `"create scripted config dir"`.
        action: String,
        /// The path involved.
        path: PathBuf,
        /// The underlying OS failure.
        #[source]
        source: std::io::Error,
    },

    /// A cassette name is not addressable inside the recordings root.
    ///
    /// Mirrors the oracle's own guard in `packages/http-recorder/src/cassette.ts`,
    /// which rejects empty names, absolute paths, and any `..` segment.
    #[error("invalid cassette name {name:?}: {reason}")]
    InvalidCassetteName {
        /// The rejected name.
        name: String,
        /// Which rule it broke.
        reason: &'static str,
    },

    /// A cassette file could not be decoded.
    #[error("cassette {path} is not a version-1 recording: {source}")]
    CassetteDecode {
        /// The file that failed to decode.
        path: PathBuf,
        /// The `serde_json` failure, retained so line and column survive.
        #[source]
        source: serde_json::Error,
    },

    /// A cassette declared a format version this crate does not implement.
    #[error("cassette {path} declares version {found}, this harness implements version {expected}")]
    CassetteVersion {
        /// The file with the unexpected version.
        path: PathBuf,
        /// The version the file declares.
        found: u32,
        /// The version this crate implements.
        expected: u32,
    },

    /// Replay ran past the end of the recorded interactions.
    ///
    /// The oracle's replay is a strict cursor, not a search: request *n* may only
    /// be served by interaction *n*. Running out means the code under test made
    /// more calls than were recorded, which is a real behavioural difference.
    #[error(
        "cassette {cassette} exhausted: request {requested} of {recorded} recorded HTTP interactions"
    )]
    CassetteExhausted {
        /// The cassette name.
        cassette: String,
        /// The 1-based index of the request that had nothing left to match.
        requested: usize,
        /// How many HTTP interactions the cassette holds.
        recorded: usize,
    },

    /// The next recorded interaction does not match the incoming request.
    #[error(
        "cassette {cassette} interaction {index} does not match the request\n  recorded: {recorded}\n  incoming: {incoming}"
    )]
    CassetteMismatch {
        /// The cassette name.
        cassette: String,
        /// The 1-based cursor position that failed to match.
        index: usize,
        /// Canonical form of the recorded request.
        recorded: String,
        /// Canonical form of the incoming request.
        incoming: String,
    },

    /// Recorded interactions were left unconsumed when replay finished.
    ///
    /// The mirror image of [`Self::CassetteExhausted`]: the code under test made
    /// *fewer* calls than the oracle did.
    #[error("cassette {cassette} finished with {unused} of {recorded} interactions unused")]
    CassetteUnused {
        /// The cassette name.
        cassette: String,
        /// How many interactions were never consumed.
        unused: usize,
        /// How many HTTP interactions the cassette holds.
        recorded: usize,
    },

    /// A recorded body claimed `base64` encoding but did not decode.
    #[error("cassette {cassette} interaction {index} has an undecodable base64 body: {detail}")]
    CassetteBodyEncoding {
        /// The cassette name.
        cassette: String,
        /// The 1-based interaction index.
        index: usize,
        /// The decoder's complaint.
        detail: String,
    },

    /// The recordings root could not be located.
    #[error("no cassette recordings root found.\n  searched: {}\n  remedy: {remedy}", searched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))]
    RecordingsRootNotFound {
        /// Every candidate consulted.
        searched: Vec<PathBuf>,
        /// How to point the harness at a real recordings tree.
        remedy: String,
    },

    /// Binding the mock provider's loopback listener failed.
    #[error("mock provider could not bind {addr}: {source}")]
    MockBind {
        /// The address that was requested.
        addr: String,
        /// The underlying OS failure.
        #[source]
        source: std::io::Error,
    },

    /// A scenario name was registered twice on one mock provider.
    #[error("mock provider already has a scenario named {name:?}")]
    DuplicateScenario {
        /// The colliding name.
        name: String,
    },

    /// A `--version` probe produced output the harness cannot read as a version.
    #[error(
        "{role} version probe produced no usable version.\n  program: {program}\n  stdout: {stdout:?}\n  stderr: {stderr:?}"
    )]
    VersionProbeFailed {
        /// Which side was probed.
        role: &'static str,
        /// The program that was run.
        program: PathBuf,
        /// Its captured stdout.
        stdout: String,
        /// Its captured stderr.
        stderr: String,
    },

    /// The resolved oracle is not the release this port is pinned to compare against.
    ///
    /// Hard rather than advisory: a pin only means something if a recorded version
    /// cannot drift away from the binary that produced the measurement. Recording
    /// `1.18.13` while resolving `1.18.12` is the inconsistency F1 rejected on.
    #[error(
        "the resolved oracle is not the pinned release.\n  pinned:   {pinned}\n  \
         reported: {reported}\n  program:  {program}\n  remedy: install {pinned} and put it \
         first on PATH, point {binary_env} at it, or move \
         `zuno_testkit::oracle::PINNED_RELEASE` to {reported} and recapture every artifact \
         measured against it"
    )]
    OraclePinMismatch {
        /// The release [`PINNED_RELEASE`](crate::oracle::PINNED_RELEASE) declares.
        pinned: &'static str,
        /// What the resolved binary said when asked for its own version.
        reported: String,
        /// The binary that was resolved and probed.
        program: PathBuf,
        /// The variable that overrides discovery, restated so the error is actionable.
        binary_env: &'static str,
    },

    /// `OC_TESTKIT_ORACLE_FLAVOUR` named something the harness does not implement.
    #[error("unknown oracle flavour {requested:?}; accepted values are {}", accepted.join(", "))]
    UnknownOracleFlavour {
        /// What the environment asked for.
        requested: String,
        /// The flavours this harness implements.
        accepted: &'static [&'static str],
    },

    /// Building the subject binary on demand failed.
    #[error("failed to build the subject binary with `{command}` (exit {status:?})\n{stderr}")]
    SubjectBuildFailed {
        /// The cargo invocation that was attempted.
        command: String,
        /// Its exit code, if it produced one.
        status: Option<i32>,
        /// Its captured stderr.
        stderr: String,
    },

    /// The committed performance baseline is not valid JSON of the expected schema.
    #[error("TypeScript baseline {path} is invalid: {source}")]
    BaselineDecode {
        /// Artifact that could not be decoded.
        path: PathBuf,
        /// JSON syntax or shape failure.
        #[source]
        source: serde_json::Error,
    },

    /// A decoded baseline violates a frozen measurement invariant.
    #[error("TypeScript baseline invariant failed: {detail}")]
    BaselineInvariant {
        /// Exact missing or contradictory fact.
        detail: String,
    },

    /// The methodology document lost its hash-delimited formula section.
    #[error("docs/perf-methodology.md must contain one PERF_FORMULAS_START/END section")]
    MethodologyFormulaSection,

    /// Linux process-tree metadata could not be read.
    #[error("could not read process-tree data for pid {pid} at {path}: {source}")]
    ProcessTreeRead {
        /// Process being enumerated.
        pid: u32,
        /// Procfs file or directory.
        path: PathBuf,
        /// Kernel filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// Linux procfs exposed a malformed PID or RSS number.
    #[error("invalid process-tree number {value:?} for pid {pid} at {path}: {source}")]
    ProcessTreeParse {
        /// Process being enumerated.
        pid: u32,
        /// Procfs file containing the value.
        path: PathBuf,
        /// Text that was not numeric.
        value: String,
        /// Integer parse failure.
        #[source]
        source: std::num::ParseIntError,
    },

    /// Linux procfs omitted a field required for RSS measurement.
    #[error("invalid process-tree data for pid {pid} at {path}: {detail}")]
    ProcessTreeFormat {
        /// Process being measured.
        pid: u32,
        /// Procfs file with the missing field.
        path: PathBuf,
        /// Specific format defect.
        detail: String,
    },

    /// A process disappeared during an otherwise valid process-tree walk.
    #[error("pid {pid} exited while its RSS was sampled")]
    ProcessVanished {
        /// PID that exited.
        pid: u32,
    },

    /// The user's real database cannot be copied for W-real.
    #[error("W-real database is unavailable at {path}: {detail}")]
    RealDatabaseUnavailable {
        /// Path resolved by `zuno-paths` using the installed release channel.
        path: PathBuf,
        /// Actionable reason the baseline cannot proceed.
        detail: String,
    },

    /// The resolved database is not the snapshot the W-real subject was pinned to.
    ///
    /// Raised before anything is copied or measured, so a mutated or substituted
    /// database costs seconds rather than a full paired measurement pass.
    #[error("W-real database at {path} is not the pinned snapshot: {detail}")]
    WRealDatabaseMismatch {
        /// Database that was resolved and rejected.
        path: PathBuf,
        /// Expected-versus-found identity plus the recapture procedure.
        detail: String,
    },

    /// The pinned W-real session does not exist in the resolved database.
    ///
    /// A distinct variant from [`Self::WRealSubjectDrifted`] because the remedies
    /// differ: an absent session means the wrong database, a drifted one means the
    /// right database was written to.
    #[error("W-real pinned session {session_id} is absent from {path}: {detail}")]
    WRealSubjectMissing {
        /// Session the pin names.
        session_id: String,
        /// Database searched for it.
        path: PathBuf,
        /// What was found instead plus the recapture procedure.
        detail: String,
    },

    /// The pinned W-real session exists but no longer holds the pinned content.
    #[error("W-real pinned session {session_id} in {path} drifted: {detail}")]
    WRealSubjectDrifted {
        /// Session the pin names.
        session_id: String,
        /// Database it was read from.
        path: PathBuf,
        /// Expected-versus-found counts plus the recapture procedure.
        detail: String,
    },

    /// A required local helper command is unavailable.
    #[error("required command {command:?} was not found; {remedy}")]
    HelperCommandNotFound {
        /// Executable expected on PATH.
        command: &'static str,
        /// Concrete installation or build action.
        remedy: &'static str,
    },

    /// A local helper command failed while preparing or running a workload.
    #[error("{program} {} failed with exit {status:?}: {stderr}", args.join(" "))]
    HelperCommandFailed {
        /// Program that ran.
        program: PathBuf,
        /// Complete argument vector.
        args: Vec<String>,
        /// Exit code, or `None` for signal termination.
        status: Option<i32>,
        /// Captured diagnostic output.
        stderr: String,
    },

    /// `docs/divergences.toml` is not valid TOML of the expected shape.
    #[error("divergence allow-list {path} could not be decoded: {detail}")]
    DivergenceDecode {
        /// The file that failed to decode.
        path: PathBuf,
        /// The `toml` parser's complaint, retained so line and column survive.
        detail: String,
    },

    /// A decoded allow-list entry would weaken the guarantee the file exists for.
    ///
    /// Separate from [`Self::DivergenceDecode`] because a file that parses but
    /// carries an entry with an empty `reason` is the failure this whole mechanism
    /// is aimed at: the allow-list turning into a place to hide differences.
    #[error("divergence allow-list {path} is not usable: {detail}")]
    DivergenceShape {
        /// The offending file.
        path: PathBuf,
        /// Which rule was broken, naming the entry.
        detail: String,
    },

    /// The compatibility report could not be written.
    #[error("could not write the compatibility report to {path}: {source}")]
    ReportWrite {
        /// Intended artifact path.
        path: PathBuf,
        /// The underlying OS failure.
        #[source]
        source: std::io::Error,
    },

    /// A long-running oracle workload failed to start or make expected progress.
    #[error("TypeScript {workload} workload failed: {detail}")]
    BaselineRunFailed {
        /// Stable workload label.
        workload: &'static str,
        /// Missing process, provider request, or other exact failure.
        detail: String,
    },
}

/// The harness result type.
pub type Result<T> = std::result::Result<T, TestkitError>;

impl TestkitError {
    /// Wrap an [`std::io::Error`] with the action and path that produced it.
    pub(crate) fn io(
        action: impl Into<String>,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            action: action.into(),
            path: path.into(),
            source,
        }
    }
}
