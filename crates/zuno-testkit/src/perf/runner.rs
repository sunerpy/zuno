//! TypeScript-only baseline orchestration and paired-run ordering.

use std::path::PathBuf;
use std::time::Duration;

use crate::Oracle;
use crate::error::{Result, TestkitError};
use crate::oracle::ENV_ORACLE_BINARY;

use super::baseline::{BaselineReport, RunMeasurement, WorkloadMeasurement, WorkloadName};
use super::database::RealDatabaseSnapshot;
use super::fixtures::{machine_facts, write_report};
use super::methodology::PERF_METHODOLOGY_REVISION;
use super::workload::measure_one;

pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// The 90-second mark, which is a warm-up discard only for `W-soak`.
///
/// It also sizes every sampling window and gates when a restored session's first
/// turn may be typed. See [`warm_up_discard`] for what revision 2 changed.
pub(super) const WARM_UP: Duration = Duration::from_secs(90);
pub(super) const SETTLE: Duration = Duration::from_secs(60);
/// Wall clock a restored session gets to finish its turn *after* hydration.
///
/// W-real cannot type its turn until hydration settles at the 90-second mark, so
/// the window has to outlast that gate plus the turn itself. Measured against the
/// 1.18.12 binary on the 105 MB session used here: 13s from keystroke to the
/// first provider request, and the tree was still growing at 1.1 GB 55s later,
/// because every request re-serialises the whole session. 300s leaves room for
/// both round trips without capping a slower machine.
const RESUMED_TURN_ALLOWANCE: Duration = Duration::from_secs(300);
const FULL_SOAK_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
const FULL_SOAK_TURNS: usize = 500;
const SOAK_SMOKE_TURNS: usize = 20;

/// Runtime settings that cannot weaken the frozen G1/G2 methodology.
#[derive(Debug, Clone)]
pub struct BaselineRunOptions {
    /// Destination for the TypeScript-only JSON artifact.
    pub output: PathBuf,
    /// Run the permitted 20-turn smoke instead of the deferred full W-soak.
    pub soak_smoke_only: bool,
}

impl BaselineRunOptions {
    /// Options used for the committed baseline artifact in Todo 93.
    #[must_use]
    pub fn todo_93(output: impl Into<PathBuf>) -> Self {
        Self {
            output: output.into(),
            soak_smoke_only: true,
        }
    }
}

/// Alternating order for a future same-machine TS/Rust paired comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairedSide {
    /// Real TypeScript `opencode` oracle.
    TypeScript,
    /// Future Rust subject; Todo 93 never executes this side.
    Rust,
}

/// Produce a drift-cancelling AB/BA paired schedule.
#[must_use]
pub fn interleaved_pair_order(repetitions: usize) -> Vec<PairedSide> {
    let mut order = Vec::with_capacity(repetitions.saturating_mul(2));
    for repetition in 0..repetitions {
        if repetition % 2 == 0 {
            order.extend([PairedSide::TypeScript, PairedSide::Rust]);
        } else {
            order.extend([PairedSide::Rust, PairedSide::TypeScript]);
        }
    }
    order
}

const fn uses_real_database(workload: WorkloadName) -> bool {
    matches!(workload, WorkloadName::WReal)
}

/// Leading samples a workload discards as warm-up before its peak is taken.
///
/// Revision 2 discards only for `W-soak`, whose startup transient is noise
/// against hours of steady-state growth. `W-idle` and `W-real` are bounded
/// cold-start workloads whose peak **is** the startup-plus-turn spike, so
/// revision 1's blanket 90-second discard threw away the value they exist to
/// measure: it cut 45 of W-idle's 75 samples and under-reported its median peak
/// as 729 MB against the 932 MB its retained samples actually hold.
pub(super) const fn warm_up_discard(workload: WorkloadName) -> Duration {
    match workload {
        WorkloadName::WSoak => WARM_UP,
        WorkloadName::WIdle | WorkloadName::WReal => Duration::ZERO,
    }
}

const fn window(workload: WorkloadName) -> Duration {
    match workload {
        WorkloadName::WIdle | WorkloadName::WSoak => WARM_UP.saturating_add(SETTLE),
        WorkloadName::WReal => WARM_UP
            .saturating_add(RESUMED_TURN_ALLOWANCE)
            .saturating_add(SETTLE),
    }
}

/// Locate the **released** TypeScript binary this baseline measures.
///
/// `OC_TESTKIT_ORACLE` is honoured because the not-found error's own remedy tells
/// the operator to set it, and because a `PATH` entry can resolve to a launcher
/// shim rather than the binary. The from-source flavour is deliberately not
/// honoured: running the TypeScript entry point under Bun would measure a
/// different process tree than the release users run, so a baseline taken that
/// way would not describe the software the gates are about.
fn released_oracle() -> Result<Oracle> {
    match std::env::var(ENV_ORACLE_BINARY) {
        Ok(explicit) => Oracle::at_binary(explicit),
        Err(_) => Oracle::installed_binary(),
    }
}

/// Verify that the released TypeScript oracle is available and versioned.
///
/// # Errors
/// Returns the oracle's actionable binary-discovery error.
pub fn verify_typescript_oracle() -> Result<String> {
    released_oracle().map(|oracle| {
        format!(
            "{} ({})",
            oracle.program().display(),
            oracle.reported_version()
        )
    })
}

/// Measure W-idle, W-real, and the selected W-soak mode against TypeScript only.
///
/// # Errors
/// Returns a typed binary, database, helper-command, workload, or artifact failure.
pub async fn measure_typescript_baseline(options: &BaselineRunOptions) -> Result<BaselineReport> {
    let oracle = released_oracle()?;
    let database = RealDatabaseSnapshot::capture()?;
    let machine = machine_facts(&oracle)?;
    let mut idle_runs = Vec::with_capacity(5);
    let mut real_runs = Vec::with_capacity(5);

    for repetition in 1..=5 {
        let order = if repetition % 2 == 1 {
            [WorkloadName::WIdle, WorkloadName::WReal]
        } else {
            [WorkloadName::WReal, WorkloadName::WIdle]
        };
        for workload in order {
            let run = measure_one(
                oracle.program(),
                workload,
                repetition,
                1,
                window(workload),
                uses_real_database(workload).then_some(&database),
            )
            .await?;
            match workload {
                WorkloadName::WIdle => idle_runs.push(run),
                WorkloadName::WReal => real_runs.push(run),
                WorkloadName::WSoak => unreachable!("the paired loop contains G1/G2 only"),
            }
        }
    }

    let (soak_turns, soak_duration, smoke_only) = if options.soak_smoke_only {
        (SOAK_SMOKE_TURNS, window(WorkloadName::WSoak), true)
    } else {
        (FULL_SOAK_TURNS, FULL_SOAK_DURATION, false)
    };
    let soak = measure_one(
        oracle.program(),
        WorkloadName::WSoak,
        1,
        soak_turns,
        soak_duration,
        None,
    )
    .await;
    let (soak_runs, soak_deferred) = soak_outcome(soak, smoke_only)?;

    let report = BaselineReport {
        schema_version: 1,
        methodology_revision: PERF_METHODOLOGY_REVISION,
        subject: "typescript-only".to_owned(),
        machine,
        workloads: vec![
            WorkloadMeasurement {
                name: WorkloadName::WIdle,
                smoke_only: false,
                turns: 1,
                wall_clock_seconds: window(WorkloadName::WIdle).as_secs(),
                session_id: None,
                message_count: None,
                part_count: None,
                part_data_bytes: None,
                median_peak_rss_kib: Some(median_peak(&idle_runs)?),
                runs: idle_runs,
                deferred_reason: None,
            },
            WorkloadMeasurement {
                name: WorkloadName::WReal,
                smoke_only: false,
                turns: 1,
                wall_clock_seconds: window(WorkloadName::WReal).as_secs(),
                session_id: Some(database.session.id.clone()),
                message_count: Some(database.session.message_count),
                part_count: Some(database.session.part_count),
                part_data_bytes: Some(database.session.part_data_bytes),
                median_peak_rss_kib: Some(median_peak(&real_runs)?),
                runs: real_runs,
                deferred_reason: None,
            },
            WorkloadMeasurement {
                name: WorkloadName::WSoak,
                smoke_only,
                turns: soak_turns,
                wall_clock_seconds: soak_duration.as_secs(),
                session_id: None,
                message_count: None,
                part_count: None,
                part_data_bytes: None,
                median_peak_rss_kib: None,
                runs: soak_runs,
                deferred_reason: soak_deferred,
            },
        ],
    };
    write_report(&options.output, &report)?;
    Ok(report)
}

/// Accept a failed W-soak **smoke** as an explicitly deferred workload.
///
/// The smoke cannot satisfy G3 even when it succeeds, so losing it costs no gate
/// evidence and must not discard the G1/G2 runs already measured. A failed *full*
/// soak is the G3 input itself and is always propagated.
fn soak_outcome(
    outcome: Result<RunMeasurement>,
    smoke_only: bool,
) -> Result<(Vec<RunMeasurement>, Option<String>)> {
    match outcome {
        Ok(run) => Ok((vec![run], None)),
        Err(error) if smoke_only => Ok((Vec::new(), Some(soak_deferral(&error)))),
        Err(error) => Err(error),
    }
}

fn soak_deferral(error: &TestkitError) -> String {
    format!(
        "not measured: the 20-turn W-soak smoke is not a G3 input and was not \
         pursued; the full W-soak of {FULL_SOAK_TURNS} turns over \
         {} hours remains owed by the G3 gate. Smoke attempt reported: {error}",
        FULL_SOAK_DURATION.as_secs() / 3600
    )
}

fn median_peak(runs: &[RunMeasurement]) -> Result<u64> {
    if runs.len() != 5 {
        return Err(TestkitError::BaselineInvariant {
            detail: format!("median requires exactly five runs, got {}", runs.len()),
        });
    }
    let mut peaks: Vec<u64> = runs.iter().map(|run| run.peak_rss_kib).collect();
    peaks.sort_unstable();
    Ok(peaks[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_order_alternates_ab_then_ba() {
        // Given: two repetitions of a future paired comparison.
        // When: the deterministic schedule is produced.
        let order = interleaved_pair_order(2);
        // Then: neither side always receives the earlier machine state.
        assert_eq!(
            order,
            vec![
                PairedSide::TypeScript,
                PairedSide::Rust,
                PairedSide::Rust,
                PairedSide::TypeScript,
            ]
        );
    }

    #[test]
    fn a_restored_sessions_window_outlasts_its_post_hydration_turn() {
        // Given: the 90-second hydration gate a restored session's turn waits for.
        // When: each workload's sampling window is derived.
        let idle = window(WorkloadName::WIdle);
        let real = window(WorkloadName::WReal);

        // Then: W-real keeps sampling long after the gate its turn starts at, so the
        // window cannot end before the turn it exists to measure.
        assert_eq!(idle, WARM_UP + SETTLE);
        assert!(real > WARM_UP + SETTLE);
        assert!(real - WARM_UP >= RESUMED_TURN_ALLOWANCE);
    }

    #[test]
    fn only_the_soak_discards_warm_up_samples() {
        // Given: every frozen workload.
        // When: each is asked how much of its trace it discards as warm-up.
        let discards = [
            WorkloadName::WIdle,
            WorkloadName::WReal,
            WorkloadName::WSoak,
        ]
        .map(warm_up_discard);

        // Then: only the hours-long soak drops its startup transient. Discarding it
        // for a bounded cold-start workload would throw away the spike being
        // measured, which is what revision 1 did to W-idle's 148-second trace.
        assert_eq!(discards, [Duration::ZERO, Duration::ZERO, WARM_UP]);
    }

    #[test]
    fn a_failed_soak_smoke_is_deferred_while_a_failed_full_soak_fails_the_report() {
        // Given: the same workload failure reported by a smoke and by a full soak.
        let failure = || {
            Err(TestkitError::BaselineRunFailed {
                workload: "W-soak",
                detail: "only 3 of 20 cassette-backed turns completed".to_owned(),
            })
        };

        // When: each outcome is classified.
        let smoke = soak_outcome(failure(), true).expect("a smoke failure is deferred");
        let full = soak_outcome(failure(), false);

        // Then: the smoke records a reason that names both what was attempted and
        // what the G3 gate still owes, without a measurement; the full soak, which
        // G3 consumes, propagates instead.
        assert!(smoke.0.is_empty());
        let reason = smoke.1.expect("deferral reason");
        assert!(reason.contains("not measured"), "{reason}");
        assert!(reason.contains("500 turns over 2 hours"), "{reason}");
        assert!(
            reason.contains("only 3 of 20 cassette-backed turns completed"),
            "{reason}"
        );
        assert!(full.is_err());
    }

    #[test]
    fn only_real_workload_requires_database() {
        // Given: every frozen workload variant.
        let workloads = [
            WorkloadName::WIdle,
            WorkloadName::WReal,
            WorkloadName::WSoak,
        ];

        // When: database routing is selected for each workload.
        let routed = workloads.map(uses_real_database);

        // Then: only W-real receives the user-history snapshot.
        assert_eq!(routed, [false, true, false]);
    }
}
