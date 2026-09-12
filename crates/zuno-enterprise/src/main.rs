use clap::{ArgGroup, Parser};
use std::path::PathBuf;
use zuno_engine::interrupt::InterruptSignal;

#[derive(Parser)]
#[command(
    name = "zuno-enterprise",
    version,
    about = "Linux enterprise Agent service"
)]
#[command(group(ArgGroup::new("action").required(true).args(["config","definition_ref"])))]
struct Arguments {
    /// Absolute path to an explicit enterprise service configuration.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Validate an immutable definition and print its ID/version/digest for a parent target.
    #[arg(long)]
    definition_ref: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let arguments = Arguments::parse();
    if let Some(path) = arguments.definition_ref {
        let result = async {
            let definition: zuno_enterprise::config::Definition =
                zuno_enterprise::config::read_json(&path).await?;
            definition.validate()?;
            let output = serde_json::to_string(&definition.reference()).map_err(|error| {
                zuno_enterprise::Error::Application(zuno_application::ApplicationError::storage(
                    error,
                ))
            })?;
            println!("{output}");
            Ok::<_, zuno_enterprise::Error>(())
        }
        .await;
        return match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    let config_path = arguments.config.expect("clap requires exactly one action");
    let shutdown = InterruptSignal::new();
    let stopped = shutdown.clone();
    let signal = tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut termination) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! {
                    _=tokio::signal::ctrl_c()=>{},
                    _=termination.recv()=>{},
                }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        stopped.fire();
    });
    let result = async {
        let config = zuno_enterprise::config::read_json(&config_path).await?;
        zuno_enterprise::service::run(config, shutdown).await
    }
    .await;
    signal.abort();
    let _ = signal.await;
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
