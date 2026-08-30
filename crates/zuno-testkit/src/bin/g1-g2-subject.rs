use std::path::{Path, PathBuf};

use serde::Serialize;
use zuno_testkit::perf::{W_REAL_SUBJECT, WorkloadMeasurement, measure_g1_g2_subject};

#[derive(Serialize)]
struct SubjectReport {
    label: String,
    program: PathBuf,
    database: PathBuf,
    session_id: &'static str,
    message_count: u64,
    part_count: u64,
    part_data_bytes: u64,
    workloads: Vec<WorkloadMeasurement>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (label, program, database, output) = arguments()?;
    let program = canonical(&program)?;
    let database = canonical(&database)?;
    let workloads = measure_g1_g2_subject(&program, &database).await?;
    let report = SubjectReport {
        label,
        program,
        database,
        session_id: W_REAL_SUBJECT.session_id,
        message_count: W_REAL_SUBJECT.message_count,
        part_count: W_REAL_SUBJECT.part_count,
        part_data_bytes: W_REAL_SUBJECT.part_data_bytes,
        workloads,
    };
    let bytes = serde_json::to_vec_pretty(&report).map_err(|source| {
        zuno_testkit::TestkitError::BaselineDecode {
            path: output.clone(),
            source,
        }
    })?;
    std::fs::write(output, bytes)?;
    Ok(())
}

fn arguments() -> zuno_testkit::Result<(String, PathBuf, PathBuf, PathBuf)> {
    let mut args = std::env::args().skip(1);
    match (
        args.next().as_deref(),
        args.next(),
        args.next().as_deref(),
        args.next(),
        args.next().as_deref(),
        args.next(),
        args.next().as_deref(),
        args.next(),
        args.next(),
    ) {
        (
            Some("--label"),
            Some(label),
            Some("--program"),
            Some(program),
            Some("--database"),
            Some(database),
            Some("--output"),
            Some(output),
            None,
        ) => Ok((
            label,
            PathBuf::from(program),
            PathBuf::from(database),
            PathBuf::from(output),
        )),
        _ => Err(zuno_testkit::TestkitError::BaselineRunFailed {
            workload: "CLI",
            detail: "expected --label LABEL --program PATH --database PATH --output PATH"
                .to_owned(),
        }),
    }
}

fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize()
}
