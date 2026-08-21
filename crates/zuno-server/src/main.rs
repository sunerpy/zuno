use std::io::{self, Write as _};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use zuno_server::api::{self, ApiState};
use zuno_server::{
    AuthConfig, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, ServerBuilder, ServerConfig,
    events_router,
};

#[derive(Debug, Parser)]
#[command(name = "zuno-server")]
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
            // Before anything that might report. This binary previously started a memory
            // sampler and installed no subscriber, so every alert it raised went to
            // `tracing`'s no-op dispatcher: a process could pass 2 GiB, the sampler would
            // observe it, format the incident, and emit it nowhere. That is the same
            // "machinery that runs and cannot be observed" defect the sampler was added to
            // fix, one layer further out.
            //
            // Bound to a named local for the whole of `run`, and not `_`: the handle owns
            // the appender's worker guard, so dropping it early stops file logging and
            // would silence everything the sampler goes on to find. `init` is idempotent
            // and never panics on an already-installed subscriber, so this is safe even
            // when something else got there first — it simply reports `installed() == false`.
            let logging = zuno_observability::init(zuno_observability::LogConfig::from_env(
                zuno_paths::log(),
            ))
            .map_err(|error| format!("failed to initialize logging: {error}"))?;
            let directory = std::env::current_dir()?.to_string_lossy().into_owned();
            // One pool backs both surfaces: the `/api` handlers and the event
            // stream's durable sequence must see the same database, or a cursor
            // would resume against rows the API never wrote.
            let pool = Arc::new(zuno_db::Pool::open_default()?);
            let events = EventService::new(Arc::clone(&pool), DEFAULT_EVENT_SUBSCRIBER_CAPACITY);
            let config = ServerConfig::default()
                .with_hostname(hostname)
                .with_port(port)
                .with_auth(AuthConfig::from_env())
                .with_default_directory(&directory);
            let state = ApiState::open_default(directory)?.with_events(events.clone());
            let server = ServerBuilder::new(config)
                .with_routes(api::router(state.clone()).merge(events_router(events)))
                .bind()
                .await?;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "http://{}", server.local_addr())?;
            stdout.flush()?;
            drop(stdout);
            // This binary is a second long-lived entry point: `zuno serve` gets its
            // sampler from the CLI's `run_process`, and a process started here would
            // otherwise have none. Wiring only the CLI is how one of two entry points
            // silently keeps the hole.
            let memory = zuno_observability::memory::MemorySampler::spawn(Arc::clone(
                zuno_observability::memory::active_sessions(),
            ));
            let served = server.serve().await;
            // Sampler first, then the log sink it writes to: reversing these would discard
            // whatever the sampler reports on its way out.
            memory.shutdown();
            drop(logging);
            served?;
        }
    }
    Ok(())
}
