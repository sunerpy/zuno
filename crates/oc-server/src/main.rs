use std::io::{self, Write as _};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use oc_server::api::{self, ApiState};
use oc_server::{
    AuthConfig, CompatV1State, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, ServerBuilder,
    ServerConfig, compat_v1_router, events_router,
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
            let directory = std::env::current_dir()?.to_string_lossy().into_owned();
            // One pool backs both surfaces: the `/api` handlers and the event
            // stream's durable sequence must see the same database, or a cursor
            // would resume against rows the API never wrote.
            let pool = Arc::new(oc_db::Pool::open_default()?);
            let events = EventService::new(Arc::clone(&pool), DEFAULT_EVENT_SUBSCRIBER_CAPACITY);
            let config = ServerConfig::default()
                .with_hostname(hostname)
                .with_port(port)
                .with_auth(AuthConfig::from_env())
                .with_default_directory(&directory);
            let state = ApiState::open_default(directory)?;
            let server = ServerBuilder::new(config)
                .with_routes(
                    api::router(state)
                        .merge(events_router(events))
                        .merge(compat_v1_router(CompatV1State::new())),
                )
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
