//! CLI entry point for the TypeScript-only performance baseline.

use std::path::PathBuf;

use zuno_testkit::perf::{
    BaselineRunOptions, measure_typescript_baseline, verify_typescript_oracle,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> zuno_testkit::Result<()> {
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
                    zuno_testkit::TestkitError::BaselineDecode {
                        path: options.output,
                        source,
                    }
                })?
            );
            Ok(())
        }
        Some(command) => Err(zuno_testkit::TestkitError::BaselineRunFailed {
            workload: "CLI",
            detail: format!(
                "unknown command {command:?}; use check-oracle or measure --output PATH"
            ),
        }),
        None => Err(zuno_testkit::TestkitError::BaselineRunFailed {
            workload: "CLI",
            detail: "missing command; use check-oracle or measure --output PATH".to_owned(),
        }),
    }
}

fn output_argument(args: &mut impl Iterator<Item = String>) -> zuno_testkit::Result<PathBuf> {
    match (args.next().as_deref(), args.next()) {
        (Some("--output"), Some(path)) if args.next().is_none() => Ok(PathBuf::from(path)),
        _ => Err(zuno_testkit::TestkitError::BaselineRunFailed {
            workload: "CLI",
            detail: "measure requires exactly --output PATH".to_owned(),
        }),
    }
}
