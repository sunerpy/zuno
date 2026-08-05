//! TypeScript-only baseline orchestration and paired-run ordering.

use std::path::PathBuf;
use std::time::Duration;

use crate::Oracle;
use crate::error::{Result, TestkitError};

use super::baseline::{BaselineReport, RunMeasurement, WorkloadMeasurement, WorkloadName};
use super::database::RealDatabaseSnapshot;
use super::fixtures::{machine_facts, write_report};
use super::methodology::PERF_METHODOLOGY_REVISION;
use super::workload::measure_one;

pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
pub(super) const WARM_UP: Duration = Duration::from_secs(90);
pub(super) const SETTLE: Duration = Duration::from_secs(60);
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

/// Verify that the released TypeScript oracle is available and versioned.
///
/// # Errors
/// Returns the oracle's actionable binary-discovery error.
pub fn verify_typescript_oracle() -> Result<String> {
    Oracle::installed_binary().map(|oracle| {
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
    let oracle = Oracle::installed_binary()?;
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
                WARM_UP + SETTLE,
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
        (SOAK_SMOKE_TURNS, WARM_UP + SETTLE, true)
    } else {
        (FULL_SOAK_TURNS, FULL_SOAK_DURATION, false)
    };
    let soak_run = measure_one(
        oracle.program(),
        WorkloadName::WSoak,
        1,
        soak_turns,
        soak_duration,
        None,
    )
    .await?;

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
                wall_clock_seconds: (WARM_UP + SETTLE).as_secs(),
                session_id: None,
                message_count: None,
                part_count: None,
                part_data_bytes: None,
                median_peak_rss_kib: Some(median_peak(&idle_runs)?),
                runs: idle_runs,
            },
            WorkloadMeasurement {
                name: WorkloadName::WReal,
                smoke_only: false,
                turns: 1,
                wall_clock_seconds: (WARM_UP + SETTLE).as_secs(),
                session_id: Some(database.session.id.clone()),
                message_count: Some(database.session.message_count),
                part_count: Some(database.session.part_count),
                part_data_bytes: Some(database.session.part_data_bytes),
                median_peak_rss_kib: Some(median_peak(&real_runs)?),
                runs: real_runs,
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
                runs: vec![soak_run],
            },
        ],
    };
    write_report(&options.output, &report)?;
    Ok(report)
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
