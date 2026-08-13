//! The compatibility suite's machine-readable report.
//!
//! # Why the report is part of the gate rather than a log
//!
//! A green suite answers "did anything I checked disagree?". It does not answer
//! "what did you check?" — and for a drop-in-replacement claim that second
//! question is the load-bearing one. A suite that silently stopped comparing a
//! surface, or that never compared it at all, is green in exactly the same way as
//! one that compared everything.
//!
//! So the suite emits this artifact and each surface carries a [`Verdict`]. A
//! reader (or plan todos F1-F4) can then see the difference between *compared and
//! equal*, *compared with a declared exception*, and *never compared*, instead of
//! inferring all three from the absence of a failure.
//!
//! # Why normalizations are enumerated
//!
//! Every differential comparison scrubs volatile values — ids, timestamps,
//! absolute paths, ports. Each scrub is a small licence to differ, and a large
//! enough pile of them makes any two programs agree. Listing them with a reason
//! puts the pile in the artifact, where it can be argued with. Plan todo 86's
//! "must not normalize away a real difference" is only checkable if the
//! normalizations are visible.
//!
//! # Why gaps are here and not in `docs/divergences.toml`
//!
//! An unimplemented surface is not a decision. Recording it as a declared
//! divergence would dress an omission up as a design choice; recording it as a
//! [`KnownGap`] keeps it legible as work outstanding. See
//! [`crate::divergence`].

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Result, TestkitError};

/// Where the report is written when `OC_COMPAT_REPORT` is unset.
pub const DEFAULT_RELATIVE_PATH: &str = "target/compat/compat-report.json";

/// Environment variable overriding the report's destination.
pub const PATH_ENV: &str = "OC_COMPAT_REPORT";

/// The artifact's format version, so a consumer can refuse a shape it predates.
pub const SCHEMA_VERSION: u32 = 1;

/// How thoroughly one surface was compared against the oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Compared against the oracle and equal after the listed normalizations.
    Compared,
    /// Compared, and the difference found is a declared divergence or a recorded
    /// gap. The entry's `detail` names which.
    PartiallyCompared,
    /// Not compared. `detail` must say why, because an unexplained omission here
    /// is indistinguishable from an oversight.
    NotCompared,
    /// A comparison exists but could not run in this environment — typically the
    /// oracle binary or a language server is absent. Never silent: the suite
    /// prints the skip as well as recording it.
    Skipped,
}

/// What the comparison was made against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleKind {
    /// The real `opencode` binary, executed.
    LiveBinary,
    /// A committed capture of the real binary's output.
    CommittedFixture,
    /// The upstream TypeScript source tree, read.
    SourceTree,
    /// A real third-party counterpart, executed (a language server, an MCP server).
    LiveCounterpart,
    /// No oracle. Only valid with [`Verdict::NotCompared`].
    None,
}

/// One compared surface.
#[derive(Debug, Clone, Serialize)]
pub struct ComparedSurface {
    /// Stable identifier, so a consumer can diff two reports.
    pub id: String,
    /// Human-readable name of the surface.
    pub name: String,
    /// How thoroughly it was compared.
    pub verdict: Verdict,
    /// What it was compared against.
    pub oracle: OracleKind,
    /// The test that performs the comparison, as `path::test_name`, so a reader
    /// can run it directly rather than trusting this line.
    pub evidence: String,
    /// What was actually established, or why nothing was.
    pub detail: String,
    /// Counts the comparison measured. Present so a later report can be diffed
    /// against this one numerically.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measured: Vec<(String, u64)>,
}

/// A volatile value a comparison scrubs before diffing.
#[derive(Debug, Clone, Serialize)]
pub struct Normalization {
    /// Which surface applies it.
    pub surface: String,
    /// What is scrubbed.
    pub value: String,
    /// Why scrubbing it does not hide a real difference.
    pub reason: String,
}

/// A behavioural difference from upstream, bound to the allow-list entry for it.
///
/// # Why this type replaced `NominatedDivergence`
///
/// Until plan todo 119 this was a *nomination*: a difference recorded here
/// precisely because it was NOT in `docs/divergences.toml`, with the suite
/// asserting it stayed out so the plan's declared count kept holding. That made a
/// second reporting structure for the same kind of fact, and — because the
/// assertion was that the id is *absent* from the allow-list — no gate could ever
/// fail when a real behavioural difference went undeclared. A reader consulting the
/// declared allow-list learned nothing about six of them.
///
/// Now every record names, in [`BehaviouralDifference::declared_as`], the entry in
/// the allow-list that must cover it. The gate resolves that id against the loaded
/// file and fails when it is missing, so the allow-list is the single place and this
/// list is only the index into it. Where two differences turned out to be one
/// decision, they share a `declared_as` rather than being declared twice.
#[derive(Debug, Clone, Serialize)]
pub struct BehaviouralDifference {
    /// Stable id of the difference, kept from the nomination it replaces.
    pub id: String,
    /// The surface affected.
    pub surface: String,
    /// The id in `docs/divergences.toml` that must declare this difference.
    ///
    /// Resolved against the loaded allow-list by the gate. A difference whose entry
    /// is absent, renamed or deleted fails there.
    pub declared_as: String,
    /// The upstream file and lines that make this a difference rather than a guess.
    pub upstream_evidence: String,
    /// The test that proves the divergent behaviour is live, not merely written down.
    pub asserted_by: String,
}

/// A surface where this implementation is behind upstream, with no decision behind it.
#[derive(Debug, Clone, Serialize)]
pub struct KnownGap {
    /// Stable identifier.
    pub id: String,
    /// The surface affected.
    pub surface: String,
    /// Exactly what is missing, and where it is (or is not) served instead.
    pub detail: String,
}

/// The oracle's availability in the environment that produced this report.
#[derive(Debug, Clone, Serialize)]
pub struct OracleAvailability {
    /// Whether the real binary was found.
    pub available: bool,
    /// Where it was found, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The version it reported, when it was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The release this report claims its comparisons were measured against.
    ///
    /// Must equal [`Self::version`] whenever the oracle was available — the suite
    /// asserts it. The two disagreeing (`1.18.13` recorded, `1.18.12` executed) is
    /// what the first final-verification wave rejected: a reader could not tell which
    /// upstream build the compatibility claim rested on. Set from
    /// [`PINNED_RELEASE`](crate::oracle::PINNED_RELEASE).
    ///
    /// The name is kept for schema compatibility with reports already committed
    /// under [`SCHEMA_VERSION`].
    pub pinned_source_version: String,
}

/// The allow-list's state, as the suite observed it.
#[derive(Debug, Clone, Serialize)]
pub struct DivergenceSummary {
    /// Entries the file declares.
    pub declared_count: usize,
    /// Entries [`crate::divergence::DECLARED_COUNT`] expects.
    pub expected_count: usize,
    /// Every declared id, sorted.
    pub ids: Vec<String>,
}

/// The whole artifact.
#[derive(Debug, Clone, Serialize)]
pub struct CompatReport {
    /// Format version.
    pub schema_version: u32,
    /// The command that produces this artifact, restated so a reader can rerun it.
    pub generated_by: &'static str,
    /// Oracle availability at generation time.
    pub oracle: OracleAvailability,
    /// The allow-list's observed state.
    pub divergences: DivergenceSummary,
    /// Every surface, in declaration order.
    pub surfaces: Vec<ComparedSurface>,
    /// Every normalization applied anywhere in the suite.
    pub normalizations: Vec<Normalization>,
    /// Every behavioural difference, each naming the allow-list entry declaring it.
    pub behavioural_differences: Vec<BehaviouralDifference>,
    /// Every surface where this port is behind upstream.
    pub known_gaps: Vec<KnownGap>,
}

impl CompatReport {
    /// Surfaces carrying the given verdict.
    #[must_use]
    pub fn with_verdict(&self, verdict: Verdict) -> Vec<&ComparedSurface> {
        self.surfaces
            .iter()
            .filter(|surface| surface.verdict == verdict)
            .collect()
    }

    /// The report's destination: `OC_COMPAT_REPORT`, else under the workspace root.
    ///
    /// # Errors
    ///
    /// Fails when neither is resolvable, which means the harness cannot say where
    /// it would have written its evidence — worth failing over rather than
    /// discarding the artifact.
    pub fn destination() -> Result<PathBuf> {
        if let Ok(explicit) = std::env::var(PATH_ENV)
            && !explicit.trim().is_empty()
        {
            return Ok(PathBuf::from(explicit));
        }
        let root = crate::subject::workspace_root().ok_or_else(|| TestkitError::Io {
            action: "locate the workspace root for the compatibility report".to_owned(),
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no ancestor Cargo.toml declares [workspace]",
            ),
        })?;
        Ok(root.join(DEFAULT_RELATIVE_PATH))
    }

    /// Writes the report as pretty-printed JSON, creating parent directories.
    ///
    /// Pretty rather than compact because the primary consumer is a human asking
    /// "what was proven?", and a diff between two runs should be readable.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created or the file cannot be
    /// written.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| TestkitError::ReportWrite {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let mut json =
            serde_json::to_string_pretty(self).map_err(|source| TestkitError::ReportWrite {
                path: path.to_path_buf(),
                source: std::io::Error::other(source),
            })?;
        json.push('\n');
        std::fs::write(path, json).map_err(|source| TestkitError::ReportWrite {
            path: path.to_path_buf(),
            source,
        })
    }

    /// A one-screen summary for a test's stdout.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str("compatibility report\n");
        for surface in &self.surfaces {
            out.push_str(&format!(
                "  [{:>18}] {} — {}\n",
                serde_json::to_value(surface.verdict)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                surface.id,
                surface.detail
            ));
        }
        out.push_str(&format!(
            "  {} normalization(s), {} known gap(s), {} declared divergence(s), \
             {} behavioural difference(s) each resolved to a declared entry\n",
            self.normalizations.len(),
            self.known_gaps.len(),
            self.divergences.declared_count,
            self.behavioural_differences.len()
        ));
        out
    }
}

// ---------------------------------------------------------------------------
// The known gaps, owned here so one source renders every artifact
// ---------------------------------------------------------------------------

/// The gap recording that a turn on this port persists no step-boundary parts.
///
/// Named so the witness in `crates/oc-testkit/tests/session_interop.rs` can look
/// the gap up in [`known_gaps`] by id rather than by matching prose. Deleting the
/// entry then fails that test instead of quietly un-recording the gap.
pub const TURN_PART_GAP_ID: &str = "assistant-turn-step-parts";

/// The assistant part types release 1.18.15 persists for one plain single-step turn.
///
/// Ordered as upstream writes them: `start-step` inserts `step-start`
/// unconditionally — `snapshot.track()` may return nothing and the `updatePart` on
/// the next line still runs (`packages/opencode/src/session/processor.ts:424-432`)
/// — the text deltas accumulate into one `text` part (`:486-530`), and
/// `finish-step` appends `step-finish` carrying `reason`, `cost` and `tokens`
/// (`:435-455`).
pub const UPSTREAM_TURN_PART_TYPES: &[&str] = &["step-start", "text", "step-finish"];

/// The assistant part types this port persists for the same turn.
///
/// Measured on the `run` path at release 1.18.15 in
/// `.omo/evidence/task-136-opencode-rust.txt:191-215`, inside a git repository and
/// outside one, so this is not the "`step-start` only carries a snapshot" case.
pub const PORTED_TURN_PART_TYPES: &[&str] = &["text"];

/// The part types [`UPSTREAM_TURN_PART_TYPES`] has and [`PORTED_TURN_PART_TYPES`]
/// does not.
///
/// Computed rather than typed a third time, so the gap's prose, the generated
/// documentation table and the witness assertion cannot disagree about which types
/// are missing.
#[must_use]
pub fn missing_turn_part_types() -> Vec<&'static str> {
    UPSTREAM_TURN_PART_TYPES
        .iter()
        .filter(|kind| !PORTED_TURN_PART_TYPES.contains(*kind))
        .copied()
        .collect()
}

/// The gap [`TURN_PART_GAP_ID`] names, on its own.
///
/// Exposed separately from [`known_gaps`] so the witness can reach it without
/// supplying API counts it has no business knowing. [`known_gaps`] returns this
/// same value, and a unit test below asserts it does, so the entry cannot be
/// dropped from the shipped list while the witness keeps passing.
#[must_use]
pub fn turn_part_gap() -> KnownGap {
    KnownGap {
        id: TURN_PART_GAP_ID.to_owned(),
        surface: "the `part` rows one assistant turn persists — the step-boundary parts".to_owned(),
        detail: format!(
            "For one plain single-step turn the release persists [{upstream}] and this port \
             persists [{ported}], so [{missing}] is never written. Measured on the `run` path at \
             1.18.15 in .omo/evidence/task-136-opencode-rust.txt:191-215, inside a git repository \
             and outside one; the user's production database holds 280,859 step-start rows, so the \
             release's shape is the normal one rather than an artefact. This is a GAP and not a \
             declared divergence because nothing chose it: `oc-db` already models both types as \
             first-class wire tags (crates/oc-db/src/message.rs:139-142,181-182) and \
             `oc-engine::stream::StreamProjector` already writes upstream's exact shape including \
             the snapshot hashes (crates/oc-engine/src/stream.rs:211-265,869-977), but no \
             production caller reaches it — the live turn path accumulates and then checkpoints \
             only text, reasoning and tool parts (crates/oc-engine/src/loop.rs:1547-1588). An \
             unwired implementation is work outstanding, so declaring it in docs/divergences.toml \
             would dress an omission up as a decision. What a consumer loses: upstream reads \
             `step-finish.cost`/`tokens` to aggregate session usage \
             (packages/core/src/session/projector.ts:36-42,90-108) and takes the first \
             `step-start.snapshot` and last `step-finish.snapshot` as the bounds of a turn's diff \
             (packages/opencode/src/session/summary.ts:82-99), which `revert` then refreshes \
             (packages/opencode/src/session/revert.ts:70-77). Interoperability is unaffected and \
             was measured to be: every assertion in crates/oc-testkit/tests/session_interop.rs \
             holds across this difference in both directions. Witnessed by \
             crates/oc-testkit/tests/session_interop.rs::{witness}.",
            upstream = UPSTREAM_TURN_PART_TYPES.join(", "),
            ported = PORTED_TURN_PART_TYPES.join(", "),
            missing = missing_turn_part_types().join(", "),
            witness = TURN_PART_WITNESS,
        ),
    }
}

/// The test name [`turn_part_gap`]'s detail points a reader at.
///
/// A constant so the gap text and the `#[tokio::test]` cannot drift apart silently;
/// the witness asserts its own name against it.
pub const TURN_PART_WITNESS: &str =
    "the_recorded_turn_part_gap_matches_what_a_turn_actually_persists";

/// The gap recording the still-unbacked part of the measured pre-`/api` surface.
pub const V1_SURFACE_GAP_ID: &str = "v1-surface-unbacked";

/// The test name [`v1_surface_gap`]'s detail points a reader at.
pub const V1_SURFACE_WITNESS: &str = "compat_v1_declared_backing_matches_what_the_router_answers";

/// How much of the pre-`/api` surface is really backed.
///
/// Counted by `oc_server::v1_coverage` from the live route table and passed in,
/// for the same reason [`known_gaps`] takes its API counts as parameters: this
/// crate has `oc-server` as a *dev*-dependency only, so restating the numbers here
/// would put a second copy of them one edit away from disagreeing with the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V1SurfaceCoverage {
    /// Routes the plugin capture measured, and the surface therefore registers.
    pub measured: usize,
    /// Routes answered by real local work.
    pub served: usize,
    /// Routes registered as structured `501` seams.
    pub unbacked: usize,
    /// Unbacked routes whose `501` names a served `/api` alternative.
    pub redirected: usize,
}

impl V1SurfaceCoverage {
    /// Builds the summary, deriving `unbacked` so it cannot contradict the rest.
    #[must_use]
    pub const fn new(measured: usize, served: usize, redirected: usize) -> Self {
        Self {
            measured,
            served,
            unbacked: measured - served,
            redirected,
        }
    }
}

/// The gap [`V1_SURFACE_GAP_ID`] names, on its own.
///
/// # Why this is recorded as a gap
///
/// The surface is presented as "plugin compatibility routes" measured from real
/// plugin callsites, but some calls still get `501`. Nothing chose that:
/// `docs/v1-surface-capture.md` describes those seams as awaiting backends. So it
/// is a gap, and
/// `docs/divergences.toml:11-14` is explicit that a merely unimplemented surface
/// "is a gap, not a divergence … and must never be laundered into an entry here".
///
/// Until the tenth review wave the only status this surface published was a `501`
/// hint reading "its backend lands in todos 57-62", which had been true and then
/// silently stopped being true when those todos closed — a plugin author was sent
/// to finished work. The hint is now built from each route's recorded `/api`
/// alternative and this entry carries the coverage, so both are derived rather than
/// asserted in prose.
#[must_use]
pub fn v1_surface_gap(coverage: V1SurfaceCoverage) -> KnownGap {
    KnownGap {
        id: V1_SURFACE_GAP_ID.to_owned(),
        surface: format!(
            "{} of the {} measured pre-/api (v1) routes the installed plugins actually call",
            coverage.unbacked, coverage.measured,
        ),
        detail: format!(
            "The pre-/api surface exists because the published SDK sends unprefixed paths, so \
             every resident plugin talks to it. It registers {measured} routes, each with a \
             recorded plugin callsite, and {served} do real local work. Ten adapters reuse the \
             corresponding /api implementations for app.agents, provider.list, session.list, \
             session.create, session.get, session.abort, session.summarize, session.messages, \
             session.prompt and session.promptAsync. Three local authentication backends persist \
             auth.set credentials and invoke the installed provider OAuth authorize/callback \
             closures. POST /tui/show-toast remains a recording sink \
             rather than a display — no server entry point attaches a forwarder \
             (crates/oc-server/src/main.rs and crates/oc-cli/src/cmd/serve.rs both build a bare \
             CompatV1State::new). {unbacked} of the {measured} answer `501 not_implemented`. \
             {redirected} of those {unbacked} name a served /api alternative; the other {stranded} \
             have no served /api spelling at all — app.log, config.get, session.status, \
             session.update, session.children and session.todo — so a plugin that needs one has no \
             working call today. The installed auth plugins' measured authentication routes are \
             served; the remaining gaps are non-authentication operations. This is a GAP and not a \
             declared divergence \
             because nothing chose it, and docs/divergences.toml:11-14 \
             forbids recording an unimplemented surface as a decision. Witnessed by \
             crates/oc-server/tests/compat_v1.rs::{witness}, which drives every route and fails \
             if a declared status disagrees with what the router answers.",
            measured = coverage.measured,
            served = coverage.served,
            unbacked = coverage.unbacked,
            redirected = coverage.redirected,
            stranded = coverage.unbacked - coverage.redirected,
            witness = V1_SURFACE_WITNESS,
        ),
    }
}

/// The gap recording that the v1 `/agent` body's shape is unclassified.
pub const V1_AGENT_GAP_ID: &str = "v1-agent-projection-unverified";

/// The test name [`v1_agent_projection_gap`]'s detail points a reader at.
pub const V1_AGENT_WITNESS: &str =
    "compat_v1_agent_projection_drift_is_recorded_and_drops_no_required_key";

/// The gap [`V1_AGENT_GAP_ID`] names, on its own.
///
/// # Why this is a gap and not the `slug` defect next door
///
/// The twelfth review wave reported this beside the pre-`/api` `Session` projection
/// dropping `slug`. That one was a defect and is fixed: `slug` was `required` in the
/// oracle *and* in the schema this build publishes at `/doc`, so the build was
/// rejecting its own response, and the value was already one layer down.
///
/// None of that holds here. Every oracle-**required** `Agent` key is served, this
/// build publishes no `Agent` schema for the body to contradict, and the only
/// committed capture is 1.18.12 while the port targets a later release — so the
/// remaining difference is in optional keys measured against a stale contract.
/// Choosing a shape from that would be inventing the missing capture, so the
/// difference is recorded here unclassified rather than declared in
/// `docs/divergences.toml`, which at lines 11-14 forbids recording something nobody
/// decided as a decision.
#[must_use]
pub fn v1_agent_projection_gap() -> KnownGap {
    KnownGap {
        id: V1_AGENT_GAP_ID.to_owned(),
        surface: "the `Agent` body shape `GET /agent` serves the pre-/api (v1) SDK".to_owned(),
        detail: format!(
            "The projection serves three keys the oracle `Agent` schema does not declare — \
             builtIn, maxSteps and tools, against a schema with additionalProperties:false — and \
             omits six it declares as optional: hidden, native, steps, temperature, topP and \
             variant. maxSteps against the oracle's steps reads as a rename. What is NOT missing \
             is any required key: all four of name, mode, permission and options are served, so \
             no v1 caller reads a promised field and gets nothing. That is the line between this \
             and the `Session` slug omission the same review wave found, which was a defect \
             because the dropped key was required by the oracle AND by the OpenAPI this build \
             publishes at /doc, making the build contradict itself. Here the build publishes no \
             `Agent` schema at all, and the only committed oracle capture is 1.18.12 while this \
             port targets a later release, so whether the difference is upstream drift already \
             corrected at the targeted version or a real omission cannot be settled without a \
             capture at that version. Picking an answer would be inventing the evidence, so the \
             difference is recorded unclassified: a gap, not a decision, which \
             docs/divergences.toml:11-14 requires. Witnessed by \
             crates/oc-server/tests/compat_v1.rs::{witness}, which measures the served key set \
             against the oracle schema and fails if a required key is ever dropped or if this \
             build starts publishing an `Agent` schema of its own — either event ends the reason \
             recorded here.",
            witness = V1_AGENT_WITNESS,
        ),
    }
}

/// Every surface where this port is behind upstream with no decision behind it.
///
/// # Why this list moved out of the compatibility suite
///
/// It used to be a private `fn known_gaps()` inside
/// `crates/oc-testkit/tests/compat_suite.rs`, reachable only by the test that
/// writes `target/compat/compat-report.json` — a file nothing commits. Meanwhile
/// `docs/divergences.md` told readers, twice, that a merely unimplemented surface
/// "is reported as `known_gaps` by the compatibility report **and listed in the
/// compatibility matrix**". No such listing existed, so for a reader consulting the
/// committed documentation the gap section was empty and the promise was prose
/// nothing derived — the same defect the first and fourth final-verification waves
/// rejected twice over stale counts.
///
/// Living here, one list renders both the report and the matrix's generated
/// `known-gaps` block, so a gap cannot be recorded in the artifact a reader never
/// sees.
///
/// # Parameters
///
/// `api_gap_count` and `upstream_api_operations` come from the live gate rather
/// than from constants restated here: the suite passes its frozen-by-name gap set
/// and upstream operation count, and `crates/oc-cli/tests/docs.rs` passes what it
/// probed off the running server and the committed oracle capture. A gap closing
/// therefore changes this text without anyone editing it.
#[must_use]
pub fn known_gaps(
    api_gap_count: usize,
    upstream_api_operations: usize,
    v1: V1SurfaceCoverage,
) -> Vec<KnownGap> {
    vec![
        KnownGap {
            id: "api-backends-unavailable".to_owned(),
            surface: format!(
                "{api_gap_count} of the {upstream_api_operations} upstream /api operations"
            ),
            detail: format!(
                "Every upstream operation is invoked against both processes and its status, \
                 normalized body, and observable session/PTY state delta are captured. {} \
                 operations have local backends. The remaining {api_gap_count} return an \
                 operation-specific 503 backend_unavailable response and are never counted as \
                 parity. The matrix rejects any 501 before applying a differential exemption. \
                 This remains a compatibility gap, not a declared behavioral difference.",
                upstream_api_operations - api_gap_count,
            ),
        },
        KnownGap {
            id: "permission-evaluation-semantics".to_owned(),
            surface: "permission resolution (`findLast` wildcard matching)".to_owned(),
            detail: "The merged permission CONFIG is compared against the real binary; the \
                     evaluation order that turns it into an allow/ask/deny decision is verified \
                     against the upstream source by unit tests, not differentially, because the \
                     binary exposes no command that prints a resolved decision."
                .to_owned(),
        },
        KnownGap {
            id: "channel-dependent-database-filename".to_owned(),
            surface: "$XDG_DATA_HOME/opencode/opencode-<channel>.db".to_owned(),
            detail: "A source build of either implementation resolves opencode-local.db while an \
                     installed release resolves opencode.db, so a `cargo build` does not see the \
                     user's sessions. This port mirrors the oracle's rule \
                     (packages/core/src/database/database.ts:45-55) exactly, so it is FAITHFUL \
                     BEHAVIOUR and not a divergence — recorded here because it presents as a \
                     parity bug the first time anyone tries it. Plan todo 92 owns documenting it."
                .to_owned(),
        },
        turn_part_gap(),
        v1_surface_gap(v1),
        v1_agent_projection_gap(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_gap_carries_an_id_a_surface_and_a_detail() {
        let gaps = known_gaps(10, 58, V1SurfaceCoverage::new(20, 1, 10));
        assert!(
            !gaps.is_empty(),
            "an empty gap list makes the report vacuous"
        );
        for gap in &gaps {
            assert!(
                !gap.id.trim().is_empty(),
                "a gap without an id cannot be looked up"
            );
            assert!(
                !gap.surface.trim().is_empty(),
                "gap {} names no surface",
                gap.id
            );
            assert!(
                !gap.detail.trim().is_empty(),
                "gap {} says nothing about what is missing, which is how a gap becomes invisible",
                gap.id
            );
        }
        let mut ids: Vec<&str> = gaps.iter().map(|gap| gap.id.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(count, ids.len(), "two gaps share an id: {ids:?}");
    }

    #[test]
    fn the_v1_agent_gap_is_shipped_and_names_its_witness() {
        let gap = known_gaps(10, 58, V1SurfaceCoverage::new(20, 1, 10))
            .into_iter()
            .find(|gap| gap.id == V1_AGENT_GAP_ID)
            .expect(
                "the /agent projection gap is reachable through v1_agent_projection_gap() but is \
                 NOT in the list the compatibility report and the documentation render, so a \
                 reader would never learn the difference was recorded",
            );
        assert_eq!(gap.detail, v1_agent_projection_gap().detail);
        assert!(
            gap.detail.contains(V1_AGENT_WITNESS),
            "the gap does not name the test that measures it, so the recording could rot while \
             the measurement passed"
        );
    }

    #[test]
    fn the_turn_part_gap_names_exactly_the_types_the_port_does_not_write() {
        assert_eq!(
            missing_turn_part_types(),
            vec!["step-start", "step-finish"],
            "the missing set is derived from the two type lists; changing either without \
             re-measuring the port's real behaviour is what the session_interop witness rejects"
        );
        let gap = known_gaps(10, 58, V1SurfaceCoverage::new(20, 1, 10))
            .into_iter()
            .find(|gap| gap.id == TURN_PART_GAP_ID)
            .expect(
                "the turn-part gap is reachable through turn_part_gap() but is NOT in the list \
                 the compatibility report and the documentation actually render, so a reader \
                 would never see it while its witness kept passing",
            );
        assert_eq!(
            gap.detail,
            turn_part_gap().detail,
            "known_gaps ships a different text than turn_part_gap, so the witness and the \
             artifact describe two different gaps"
        );
        for kind in missing_turn_part_types() {
            assert!(
                gap.detail.contains(kind),
                "the gap's detail does not mention the missing {kind} part"
            );
        }
    }

    fn surface(id: &str, verdict: Verdict) -> ComparedSurface {
        ComparedSurface {
            id: id.to_owned(),
            name: id.to_owned(),
            verdict,
            oracle: OracleKind::LiveBinary,
            evidence: "tests/x.rs::y".to_owned(),
            detail: "d".to_owned(),
            measured: Vec::new(),
        }
    }

    fn report() -> CompatReport {
        CompatReport {
            schema_version: SCHEMA_VERSION,
            generated_by: "cargo test --test compat_suite",
            oracle: OracleAvailability {
                available: false,
                path: None,
                version: None,
                pinned_source_version: crate::oracle::PINNED_RELEASE.to_owned(),
            },
            divergences: DivergenceSummary {
                declared_count: 7,
                expected_count: 7,
                ids: vec!["a".to_owned()],
            },
            surfaces: vec![
                surface("one", Verdict::Compared),
                surface("two", Verdict::NotCompared),
            ],
            normalizations: Vec::new(),
            behavioural_differences: Vec::new(),
            known_gaps: Vec::new(),
        }
    }

    #[test]
    fn the_report_round_trips_to_a_file_and_names_every_surface() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("report.json");
        report().write(&path).expect("write");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("\"schema_version\": 1"), "{text}");
        assert!(text.contains("\"id\": \"one\""), "{text}");
        assert!(text.contains("\"verdict\": \"not_compared\""), "{text}");
        assert!(
            text.ends_with('\n'),
            "the artifact must be newline-terminated"
        );
    }

    #[test]
    fn verdict_filtering_separates_compared_from_uncompared() {
        let report = report();
        assert_eq!(report.with_verdict(Verdict::Compared).len(), 1);
        assert_eq!(report.with_verdict(Verdict::NotCompared).len(), 1);
        assert_eq!(report.with_verdict(Verdict::Skipped).len(), 0);
    }

    #[test]
    fn the_default_destination_resolves_under_the_workspace_root() {
        let resolved = CompatReport::destination().expect("a destination must resolve");
        assert!(
            resolved.ends_with("compat-report.json"),
            "unexpected default: {}",
            resolved.display()
        );
    }
}
