use std::io::{self, Write as _};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use oc_server::{
    AuthConfig, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, ServerBuilder, ServerConfig,
    events_router,
};

#[derive(Debug, Parser)]
#[command(name = "oc-server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        hostname: String,
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Cli { command } = Cli::parse();
    match command {
        Command::Serve { hostname, port } => {
            let events = EventService::new(
                Arc::new(oc_db::Pool::open_default()?),
                DEFAULT_EVENT_SUBSCRIBER_CAPACITY,
            );
            let config = ServerConfig::default()
                .with_hostname(hostname)
                .with_port(port)
                .with_auth(AuthConfig::from_env());
            let server = ServerBuilder::new(config)
                .with_routes(events_router(events))
                .bind()
                .await?;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "http://{}", server.local_addr())?;
            stdout.flush()?;
            drop(stdout);
            server.serve().await?;
        }
    }
    Ok(())
}
