use clap::Parser;
use std::path::PathBuf;
use zuno_engine::interrupt::InterruptSignal;

#[derive(Parser)]
#[command(
    name = "zuno-enterprise",
    version,
    about = "Linux enterprise Agent service"
)]
struct Arguments {
    /// Absolute path to an explicit enterprise service configuration.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let arguments = Arguments::parse();
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
        let config = zuno_enterprise::config::read_json(&arguments.config).await?;
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
