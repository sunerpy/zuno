//! CLI entry point for the TypeScript-only performance baseline.

use std::path::PathBuf;

use oc_testkit::perf::{BaselineRunOptions, measure_typescript_baseline, verify_typescript_oracle};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> oc_testkit::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("check-oracle") => {
            println!("{}", verify_typescript_oracle()?);
            Ok(())
        }
        Some("measure") => {
            let output = output_argument(&mut args)?;
            let options = BaselineRunOptions::todo_93(output);
            let report = measure_typescript_baseline(&options).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|source| {
                    oc_testkit::TestkitError::BaselineDecode {
                        path: options.output,
                        source,
                    }
                })?
            );
            Ok(())
        }
        Some(command) => Err(oc_testkit::TestkitError::BaselineRunFailed {
            workload: "CLI",
            detail: format!(
                "unknown command {command:?}; use check-oracle or measure --output PATH"
            ),
        }),
        None => Err(oc_testkit::TestkitError::BaselineRunFailed {
            workload: "CLI",
            detail: "missing command; use check-oracle or measure --output PATH".to_owned(),
        }),
    }
}

fn output_argument(args: &mut impl Iterator<Item = String>) -> oc_testkit::Result<PathBuf> {
    match (args.next().as_deref(), args.next()) {
        (Some("--output"), Some(path)) if args.next().is_none() => Ok(PathBuf::from(path)),
        _ => Err(oc_testkit::TestkitError::BaselineRunFailed {
            workload: "CLI",
            detail: "measure requires exactly --output PATH".to_owned(),
        }),
    }
}
