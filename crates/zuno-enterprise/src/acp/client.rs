//! Public API client: bearer refresh pins actor identity and never follows
//! redirects or mechanically retries writes.
use crate::{Error, config, invalid};
use futures::StreamExt as _;
use serde::{Serialize, de::DeserializeOwned};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use url::Url;
use zuno_application::api::ActorView;
use zuno_auth::Secret;

#[derive(Clone)]
pub(super) struct Api {
    http: reqwest::Client,
    base: Url,
    token_file: PathBuf,
    actor: ActorView,
    approved_token: Arc<Mutex<String>>,
}
#[derive(Debug, thiserror::Error)]
pub(super) enum ApiError {
    #[error("enterprise access was denied or expired")]
    Forbidden,
    #[error("enterprise resource was not found")]
    NotFound,
    #[error("enterprise state changed")]
    Conflict,
    #[error("enterprise admission quota is exhausted")]
    Capacity,
    #[error("enterprise API response is invalid")]
    Invalid,
    #[error("enterprise API request did not return a confirmed response")]
    Unavailable,
    #[error("enterprise input admission is unconfirmed; inspect its request receipt")]
    Unconfirmed,
}
impl Api {
    pub(super) async fn new(options: &config::StateClientConfig) -> Result<Self, Error> {
        let base = Url::parse(&options.endpoint)
            .map_err(|_| invalid("invalid ACP public API endpoint"))?;
        if base.scheme() != "https"
            || base.host_str().is_none()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.username().is_empty()
            || base.password().is_some()
            || !base.path().ends_with("/api/v1/")
            || base.path().contains("/internal/")
        {
            return Err(invalid(
                "ACP bridge requires an HTTPS public /api/v1/ endpoint",
            ));
        }
        let mut builder = zuno_network::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10));
        if let Some(path) = &options.root_certificate {
            let bytes = config::read_file(path, 65536).await?;
            let certificate = reqwest::Certificate::from_pem(&bytes)
                .map_err(|_| invalid("invalid ACP API certificate"))?;
            builder = builder.add_root_certificate(certificate);
        }
        let http = builder
            .build()
            .map_err(|_| invalid("invalid ACP API transport"))?;
        let token = config::secret(&options.access_token_file).await?;
        let actor: ActorView = decode(
            http.get(base.join("identity").expect("static route"))
                .bearer_auth(token.expose())
                .send()
                .await
                .map_err(|_| invalid("ACP API identity unavailable"))?,
        )
        .await
        .map_err(|_| invalid("ACP API identity is not authorized"))?;
        let fingerprint = zuno_orchestration::sha256_text(token.expose());
        Ok(Self {
            http,
            base,
            token_file: options.access_token_file.clone(),
            actor,
            approved_token: Arc::new(Mutex::new(fingerprint)),
        })
    }
    pub(super) fn actor(&self) -> &ActorView {
        &self.actor
    }
    async fn token(&self) -> Result<Secret, ApiError> {
        let token = config::secret(&self.token_file)
            .await
            .map_err(|_| ApiError::Forbidden)?;
        let fingerprint = zuno_orchestration::sha256_text(token.expose());
        let mut approved = self.approved_token.lock().await;
        if *approved != fingerprint {
            let actor: ActorView = decode(
                self.http
                    .get(self.base.join("identity").map_err(|_| ApiError::Invalid)?)
                    .bearer_auth(token.expose())
                    .send()
                    .await
                    .map_err(|_| ApiError::Unavailable)?,
            )
            .await?;
            if actor != self.actor {
                return Err(ApiError::Forbidden);
            }
            *approved = fingerprint;
        }
        Ok(token)
    }
    pub(super) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        self.send(path, None).await
    }
    pub(super) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        input: &impl Serialize,
    ) -> Result<T, ApiError> {
        let bytes = serde_json::to_vec(input).map_err(|_| ApiError::Invalid)?;
        if bytes.len() > zuno_application::MAX_INPUT_BYTES + 16384 {
            return Err(ApiError::Invalid);
        }
        self.send(path, Some(bytes)).await
    }
    async fn send<T: DeserializeOwned>(
        &self,
        path: &str,
        input: Option<Vec<u8>>,
    ) -> Result<T, ApiError> {
        if path.starts_with('/') || path.contains("://") || path.contains("..") {
            return Err(ApiError::Invalid);
        }
        let url = self.base.join(path).map_err(|_| ApiError::Invalid)?;
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(ApiError::Invalid);
        }
        let token = self.token().await?;
        let request = match input {
            Some(bytes) => self
                .http
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes),
            None => self.http.get(url),
        }
        .bearer_auth(token.expose());
        decode(request.send().await.map_err(|_| ApiError::Unavailable)?).await
    }
}
async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, ApiError> {
    match response.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err(ApiError::Forbidden),
        404 => return Err(ApiError::NotFound),
        409 => return Err(ApiError::Conflict),
        429 => return Err(ApiError::Capacity),
        _ => return Err(ApiError::Unavailable),
    }
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return Err(ApiError::Invalid);
    }
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim() == "application/json")
        })
    {
        return Err(ApiError::Invalid);
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::Unavailable)?;
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(ApiError::Invalid);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::Invalid)
}
