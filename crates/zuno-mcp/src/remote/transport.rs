use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use zuno_auth::Secret;
use zuno_config::schema::mcp::McpRemote;

use crate::stdio::{DEFAULT_REQUEST_TIMEOUT, PROTOCOL_VERSION};

use super::legacy::open_legacy;
use super::{
    LegacyState, NOTIFICATION_CAPACITY, RemoteClient, RemoteError, RemoteInner, RemoteTransport,
};

pub(super) async fn connect_transport(
    server: &str,
    config: &McpRemote,
    transport: RemoteTransport,
    bearer: Option<Secret>,
) -> Result<RemoteClient, RemoteError> {
    let base_url = reqwest::Url::parse(&config.url).map_err(|error| RemoteError::Config {
        server: server.to_owned(),
        message: error.to_string(),
    })?;
    let timeout = config.timeout.map_or(DEFAULT_REQUEST_TIMEOUT, |value| {
        Duration::from_millis(u64::from(value.get()))
    });
    let headers = configured_headers(server, config)?;
    let http = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|source| RemoteError::Http {
            server: server.to_owned(),
            transport,
            source,
        })?;
    let legacy = if transport == RemoteTransport::Sse {
        let (endpoint, response, decoder) =
            open_legacy(server, &base_url, &http, &headers, bearer.as_ref(), timeout).await?;
        Some(LegacyState {
            endpoint,
            source: tokio::sync::Mutex::new(Some((response, decoder))),
            reader: Mutex::new(None),
        })
    } else {
        None
    };
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);
    let (refresh, _refresh_receiver) = mpsc::channel(1);
    let client = RemoteClient {
        inner: Arc::new(RemoteInner {
            server: server.to_owned(),
            base_url,
            timeout,
            transport,
            http,
            headers,
            bearer,
            next_id: AtomicU64::new(1),
            pending,
            notifications,
            refresh,
            initialization: OnceLock::new(),
            session_id: tokio::sync::Mutex::new(None),
            legacy,
            operation: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
        }),
    };
    let initialization = client
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": crate::CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await?;
    let initialization = serde_json::from_value(initialization)
        .map_err(|error| client.protocol_error(format!("invalid initialize result: {error}")))?;
    let _already_initialized = client.inner.initialization.set(initialization);
    client.send_initialized().await?;
    Ok(client)
}

fn configured_headers(server: &str, config: &McpRemote) -> Result<HeaderMap, RemoteError> {
    let mut headers = HeaderMap::new();
    for (name, value) in config.headers.iter().flatten() {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|error| RemoteError::Config {
                server: server.to_owned(),
                message: format!("invalid header name {name:?}: {error}"),
            })?;
        let value = HeaderValue::from_str(value).map_err(|error| RemoteError::Config {
            server: server.to_owned(),
            message: format!("invalid value for header {name}: {error}"),
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

pub(super) fn status_error(
    server: &str,
    transport: RemoteTransport,
    status: StatusCode,
    headers: &HeaderMap,
) -> RemoteError {
    let challenge = headers
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    RemoteError::Status {
        server: server.to_owned(),
        transport,
        status,
        challenge,
    }
}
