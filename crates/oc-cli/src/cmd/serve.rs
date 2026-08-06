use std::io::Write as _;
use std::sync::Arc;

use oc_server::api::{self, ApiState};
use oc_server::{
    AuthConfig, CompatV1State, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, ServerBuilder,
    ServerConfig, compat_v1_router, events_router,
};

use crate::command::ServeArgs;

pub(super) fn execute(args: &ServeArgs) -> Result<(), String> {
    if args.mdns {
        return Err("--mdns is not supported by the Rust server runtime yet".to_owned());
    }
    if args.mdns_domain != "opencode.local" {
        return Err("--mdns-domain requires --mdns, which is not supported yet".to_owned());
    }
    if !args.cors.is_empty() {
        return Err("--cors is not supported by the Rust server runtime yet".to_owned());
    }

    let directory = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .to_string_lossy()
        .into_owned();
    let auth = AuthConfig::from_env();
    if !auth.required() {
        println!("Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let pool = Arc::new(oc_db::Pool::open_default().map_err(|error| error.to_string())?);
        let events = EventService::new(Arc::clone(&pool), DEFAULT_EVENT_SUBSCRIBER_CAPACITY);
        let config = ServerConfig::default()
            .with_hostname(&args.hostname)
            .with_port(args.port)
            .with_auth(auth)
            .with_default_directory(&directory);
        let state = ApiState::open_default(&directory).map_err(|error| error.to_string())?;
        let server = ServerBuilder::new(config)
            .with_routes(
                api::router(state)
                    .merge(events_router(events))
                    .merge(compat_v1_router(CompatV1State::new())),
            )
            .bind()
            .await
            .map_err(|error| error.to_string())?;
        println!(
            "opencode server listening on http://{}",
            server.local_addr()
        );
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        server.serve().await.map_err(|error| error.to_string())
    })
}
