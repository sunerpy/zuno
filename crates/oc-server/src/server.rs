//! Listener safety and final router assembly.
//!
//! Network names are resolved once, validated, and the selected `SocketAddr` is
//! passed directly to `TcpListener::bind`. That avoids validating one DNS answer
//! and binding a later answer. With no password, *every* resolved address must be
//! loopback; mixed or non-loopback answers are refused. Resolution failure is an
//! error rather than an assumption that a hostname is local.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use oc_db::session_prune::SessionPruneProgress;
use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::{TurnEvent, TurnEventSender};
use oc_engine::status::SessionRunRegistry;
use tokio::net::{TcpListener, lookup_host};

use crate::auth::WWW_AUTHENTICATE_VALUE;
use crate::discovery::{self, LocalServerRegistration};
use crate::{AuthConfig, EventFanout, RequestBroker};

pub type SessionMutationFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionModelSelection {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPromptExecution {
    pub session_id: String,
    pub directory: PathBuf,
    pub message_id: String,
    pub prompt: String,
    pub agent: Option<String>,
    pub model: Option<SessionModelSelection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCompactExecution {
    pub session_id: String,
    pub directory: PathBuf,
    pub agent: Option<String>,
    pub model: Option<SessionModelSelection>,
}

pub trait SessionMutationExecutor: Send + Sync + std::fmt::Debug {
    fn prompt(
        &self,
        request: SessionPromptExecution,
        interrupt: InterruptSignal,
        events: TurnEventSender,
    ) -> SessionMutationFuture;

    fn compact(
        &self,
        request: SessionCompactExecution,
        interrupt: InterruptSignal,
    ) -> SessionMutationFuture;
}

/// Bind, middleware, and fan-out settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    hostname: String,
    port: u16,
    auth: AuthConfig,
    default_directory: String,
    event_capacity: usize,
}

impl ServerConfig {
    /// Overrides `127.0.0.1`. Unauthenticated non-loopback values are refused.
    #[must_use]
    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Overrides ephemeral port `0`.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Supplies the already-resolved authentication settings.
    #[must_use]
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// Supplies the request fallback used when neither SDK directory form exists.
    #[must_use]
    pub fn with_default_directory(mut self, directory: impl Into<String>) -> Self {
        self.default_directory = directory.into();
        self
    }

    /// Overrides the per-connection queue ceiling, primarily for pressure tests.
    #[must_use]
    pub fn with_event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = capacity.max(1);
        self
    }

    /// Hostname spelling supplied by configuration or `--hostname`.
    #[must_use]
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Requested port; zero asks the kernel for an ephemeral port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        let default_directory = std::env::current_dir().map_or_else(
            |_| ".".to_owned(),
            |directory| directory.to_string_lossy().into_owned(),
        );
        Self {
            hostname: "127.0.0.1".to_owned(),
            port: 0,
            auth: AuthConfig::default(),
            default_directory,
            event_capacity: crate::DEFAULT_EVENT_SUBSCRIBER_CAPACITY,
        }
    }
}

/// Shared process-local services later route groups extend.
#[derive(Clone, Debug)]
pub struct ServerServices {
    /// One-live-turn registry used by session and control routes.
    pub runs: SessionRunRegistry,
    /// Bounded per-connection destination for engine transitions.
    pub events: EventFanout<TurnEvent>,
    /// Bounded destination for session-maintenance progress.
    pub maintenance_events: EventFanout<SessionPruneProgress>,
    /// Pending permission and question requests raised by HTTP-driven turns.
    pub requests: RequestBroker,
    pub mutations: Option<Arc<dyn SessionMutationExecutor>>,
}

impl ServerServices {
    /// Creates an empty run registry and event fan-out.
    #[must_use]
    pub fn new(event_capacity: usize) -> Self {
        Self {
            runs: SessionRunRegistry::new(),
            events: EventFanout::with_capacity(event_capacity),
            maintenance_events: EventFanout::with_capacity(event_capacity),
            requests: RequestBroker::default(),
            mutations: None,
        }
    }

    #[must_use]
    pub fn with_mutations(mut self, mutations: Arc<dyn SessionMutationExecutor>) -> Self {
        self.mutations = Some(mutations);
        self
    }

    #[must_use]
    pub fn with_requests(mut self, requests: RequestBroker) -> Self {
        self.requests = requests;
        self
    }
}

/// Final assembly point for core and route-owned routers.
pub struct ServerBuilder {
    config: ServerConfig,
    services: ServerServices,
    routes: Router,
}

impl ServerBuilder {
    /// Creates the core with a health route and no feature routes.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        let services = ServerServices::new(config.event_capacity);
        Self {
            config,
            services,
            routes: Router::new(),
        }
    }

    /// Reuses caller-owned engine services.
    #[must_use]
    pub fn with_services(mut self, services: ServerServices) -> Self {
        self.services = services;
        self
    }

    /// Merges routes before the mandatory middleware layers are applied.
    ///
    /// Todos 52-62 extend the surface through this seam. Accepting only a complete
    /// router prevents a later caller from adding a route *after* authentication.
    #[must_use]
    pub fn with_routes(mut self, routes: Router) -> Self {
        self.routes = self.routes.merge(routes);
        self
    }

    /// Builds an in-process router with the same middleware as a bound server.
    pub fn router(self) -> Router {
        let auth = self.config.auth.clone();
        let default_directory = self.config.default_directory.clone();
        Router::new()
            .route("/health", get(health))
            .merge(self.routes)
            .fallback(StatusCode::NOT_FOUND)
            .layer(Extension(self.services))
            .layer(middleware::from_fn_with_state(
                default_directory,
                attach_directory,
            ))
            // Last layer is outermost. Reject before directory selection or any
            // future route handler can perform work.
            .layer(middleware::from_fn_with_state(auth, require_auth))
    }

    /// Resolves, security-checks, and binds the configured listener.
    pub async fn bind(self) -> Result<BoundServer, ServerError> {
        let address = resolve_address(
            &self.config.hostname,
            self.config.port,
            self.config.auth.required(),
        )
        .await?;
        let listener = TcpListener::bind(address)
            .await
            .map_err(|source| ServerError::Bind { address, source })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ServerError::LocalAddress { source })?;
        let registration =
            discovery::register(local_addr).map_err(|source| ServerError::Discovery { source })?;
        let router = self.router();
        Ok(BoundServer {
            listener,
            router,
            local_addr,
            _registration: registration,
        })
    }
}

impl std::fmt::Debug for ServerBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerBuilder")
            .field("config", &self.config)
            .field("services", &self.services)
            .finish_non_exhaustive()
    }
}

/// A listener that passed the non-loopback security gate.
pub struct BoundServer {
    listener: TcpListener,
    router: Router,
    local_addr: SocketAddr,
    _registration: Option<LocalServerRegistration>,
}

impl BoundServer {
    /// The kernel-selected address, including the real port when `0` was requested.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves until the process or task is cancelled.
    pub async fn serve(self) -> Result<(), ServerError> {
        axum::serve(self.listener, self.router)
            .await
            .map_err(|source| ServerError::Serve { source })
    }
}

impl std::fmt::Debug for BoundServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundServer")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

/// Startup failures. No variant contains credentials.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// DNS or host parsing failed before the listener was created.
    #[error("could not resolve --hostname `{hostname}` on port {port}: {source}")]
    Resolve {
        hostname: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },
    /// Resolution returned no address to bind.
    #[error("--hostname `{hostname}` resolved to no addresses on port {port}")]
    NoAddress { hostname: String, port: u16 },
    /// The hard gate against silently exposed unauthenticated listeners.
    #[error(
        "refusing --hostname `{hostname}`: a non-loopback listener would expose the unauthenticated server to the network; set OPENCODE_SERVER_PASSWORD to a non-empty value before using this --hostname"
    )]
    UnsecuredNonLoopback { hostname: String },
    /// The selected, already-validated address could not be bound.
    #[error("could not bind HTTP server to {address}: {source}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    /// The socket bound but its actual ephemeral address could not be read.
    #[error("could not read the bound HTTP listener address: {source}")]
    LocalAddress {
        #[source]
        source: std::io::Error,
    },
    /// The listener bound but could not publish its loopback discovery record.
    #[error("could not register the local HTTP server for maintenance discovery: {source}")]
    Discovery {
        #[source]
        source: std::io::Error,
    },
    /// The serving task terminated unexpectedly.
    #[error("HTTP server stopped: {source}")]
    Serve {
        #[source]
        source: std::io::Error,
    },
}

async fn resolve_address(
    hostname: &str,
    port: u16,
    authenticated: bool,
) -> Result<SocketAddr, ServerError> {
    let lookup_name = hostname
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
        .unwrap_or(hostname);
    let resolved = lookup_host((lookup_name, port))
        .await
        .map_err(|source| ServerError::Resolve {
            hostname: hostname.to_owned(),
            port,
            source,
        })?
        .collect::<BTreeSet<_>>();
    if resolved.is_empty() {
        return Err(ServerError::NoAddress {
            hostname: hostname.to_owned(),
            port,
        });
    }
    if !authenticated && resolved.iter().any(|address| !address.ip().is_loopback()) {
        return Err(ServerError::UnsecuredNonLoopback {
            hostname: hostname.to_owned(),
        });
    }
    resolved
        .into_iter()
        .next()
        .ok_or_else(|| ServerError::NoAddress {
            hostname: hostname.to_owned(),
            port,
        })
}

async fn health() -> &'static str {
    "ok\n"
}

async fn require_auth(State(auth): State<AuthConfig>, request: Request, next: Next) -> Response {
    if auth.authorizes(request.headers()) {
        return next.run(request).await;
    }
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(WWW_AUTHENTICATE_VALUE),
    );
    response
}

async fn attach_directory(
    State(default_directory): State<String>,
    mut request: Request,
    next: Next,
) -> Response {
    let directory = crate::directory::resolve(request.uri(), request.headers(), &default_directory);
    request.extensions_mut().insert(directory);
    next.run(request).await
}
