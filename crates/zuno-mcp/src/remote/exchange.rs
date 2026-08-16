use std::sync::Arc;
use std::sync::atomic::Ordering;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Response, Url};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::protocol::{ExchangeError, decode_response, lock, route_message};

use super::legacy::legacy_read_loop;
use super::sse::SseDecoder;
use super::transport::status_error;
use super::{PROTOCOL_HEADER, RemoteClient, RemoteError, RemoteTransport, SESSION_HEADER};

impl RemoteClient {
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, RemoteError> {
        let _operation = self.inner.operation.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(self.protocol_error("connection is closed"));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (sender, receiver) = oneshot::channel();
        lock(&self.inner.pending).insert(id, sender);
        let exchange = async {
            match self.inner.transport {
                RemoteTransport::StreamableHttp => self.send_streamable(&message).await?,
                RemoteTransport::Sse => {
                    self.ensure_legacy_reader().await?;
                    self.send_legacy(&message).await?;
                }
            }
            let response = receiver
                .await
                .map_err(|_| self.protocol_error("response channel closed"))?;
            let response = response
                .map_err(|error| self.protocol_error(ExchangeError::from(error).to_string()))?;
            decode_response(method, response)
                .map_err(|error| self.protocol_error(error.to_string()))
        };
        let result = match tokio::time::timeout(self.inner.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(RemoteError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            }),
        };
        lock(&self.inner.pending).remove(&id);
        result
    }

    pub(super) async fn send_initialized(&self) -> Result<(), RemoteError> {
        let message = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        match self.inner.transport {
            RemoteTransport::StreamableHttp => {
                let response = self
                    .request_builder(reqwest::Method::POST, self.inner.base_url.clone())
                    .json(&message)
                    .send()
                    .await
                    .map_err(|source| self.http_error(source))?;
                self.capture_session(&response).await;
                if response.status().is_success() {
                    Ok(())
                } else {
                    Err(self.status_error(&response))
                }
            }
            RemoteTransport::Sse => self.send_legacy(&message).await,
        }
    }

    async fn send_streamable(&self, message: &Value) -> Result<(), RemoteError> {
        let response = self
            .request_builder(reqwest::Method::POST, self.inner.base_url.clone())
            .json(message)
            .send()
            .await
            .map_err(|source| self.http_error(source))?;
        self.capture_session(&response).await;
        if !response.status().is_success() {
            return Err(self.status_error(&response));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            self.consume_streamable_sse(response).await
        } else {
            let message = response
                .json::<Value>()
                .await
                .map_err(|source| self.http_error(source))?;
            self.route(message);
            Ok(())
        }
    }

    async fn consume_streamable_sse(&self, mut response: Response) -> Result<(), RemoteError> {
        let mut decoder = SseDecoder::default();
        loop {
            while let Some(event) = decoder.pop() {
                if event.event.as_deref().is_none_or(|kind| kind == "message") {
                    let message = serde_json::from_str::<Value>(&event.data).map_err(|error| {
                        self.protocol_error(format!("invalid SSE JSON: {error}"))
                    })?;
                    let is_response = message.get("id").is_some();
                    self.route(message);
                    if is_response {
                        return Ok(());
                    }
                }
            }
            match response.chunk().await {
                Ok(Some(bytes)) => decoder
                    .push(&bytes)
                    .map_err(|error| self.protocol_error(error))?,
                Ok(None) => return Err(self.protocol_error("SSE response ended before a reply")),
                Err(source) => return Err(self.http_error(source)),
            }
        }
    }

    async fn send_legacy(&self, message: &Value) -> Result<(), RemoteError> {
        let endpoint = self
            .inner
            .legacy
            .as_ref()
            .expect("legacy transport has an endpoint")
            .endpoint
            .clone();
        let response = self
            .request_builder(reqwest::Method::POST, endpoint)
            .json(message)
            .send()
            .await
            .map_err(|source| self.http_error(source))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(self.status_error(&response))
        }
    }

    async fn ensure_legacy_reader(&self) -> Result<(), RemoteError> {
        let legacy = self
            .inner
            .legacy
            .as_ref()
            .expect("legacy transport has state");
        if lock(&legacy.reader).is_some() {
            return Ok(());
        }
        let Some((response, decoder)) = legacy.source.lock().await.take() else {
            return Err(self.protocol_error("legacy SSE source is unavailable"));
        };
        let inner = Arc::clone(&self.inner);
        let reader = tokio::spawn(async move { legacy_read_loop(inner, response, decoder).await });
        *lock(&legacy.reader) = Some(reader);
        Ok(())
    }

    pub(super) fn request_builder(
        &self,
        method: reqwest::Method,
        url: Url,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .inner
            .http
            .request(method, url)
            .headers(self.inner.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json");
        if let Some(bearer) = &self.inner.bearer {
            request = request.header(AUTHORIZATION, format!("Bearer {}", bearer.expose()));
        }
        if let Ok(session) = self.inner.session_id.try_lock()
            && let Some(session) = session.as_ref()
        {
            request = request.header(SESSION_HEADER, session.clone());
        }
        if let Some(initialization) = self.inner.initialization.get() {
            request = request.header(PROTOCOL_HEADER, &initialization.protocol_version);
        }
        request
    }

    async fn capture_session(&self, response: &Response) {
        if let Some(value) = response.headers().get(SESSION_HEADER) {
            *self.inner.session_id.lock().await = Some(value.clone());
        }
    }

    fn route(&self, message: Value) {
        route_message(
            &self.inner.server,
            &self.inner.pending,
            &self.inner.notifications,
            &self.inner.refresh,
            message,
        );
    }

    pub(super) fn protocol_error(&self, message: impl Into<String>) -> RemoteError {
        RemoteError::Protocol {
            server: self.inner.server.clone(),
            transport: self.inner.transport,
            message: message.into(),
        }
    }

    fn http_error(&self, source: reqwest::Error) -> RemoteError {
        if source.is_timeout() {
            RemoteError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            }
        } else {
            RemoteError::Http {
                server: self.inner.server.clone(),
                transport: self.inner.transport,
                source,
            }
        }
    }

    fn status_error(&self, response: &Response) -> RemoteError {
        status_error(
            &self.inner.server,
            self.inner.transport,
            response.status(),
            response.headers(),
        )
    }
}
