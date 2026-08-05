//! The seam between this profile and the network.
//!
//! # Why a trait
//!
//! Every test in this crate replays recorded bytes. That is not a convention that
//! a later task could quietly break: [`Provider::stream`] reaches the wire only
//! through [`Transport`], and the tests construct the provider with a transport
//! that reads a cassette. There is no `#[cfg(test)]` branch inside the request
//! path, so the "no live provider call in a test" rule holds structurally rather
//! than by inspection.
//!
//! It also keeps `reqwest` out of the translation logic, which is the part with
//! actual behaviour worth testing.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use futures::Stream;
use oc_error::ProviderError;
use serde_json::Value;

use crate::stream::retry_after;
use crate::wire::ErrorEnvelope;

/// A stream of raw response chunks, exactly as the transport received them.
///
/// Bytes, not text. Decoding is [`oc_llm::sse::SseParser`]'s job, because only it
/// holds the boundary state that makes a code point split across two chunks
/// survive.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, ProviderError>> + Send>>;

/// One outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Fully-resolved absolute URL.
    pub url: String,
    /// Headers to send, ordered so a test can compare them.
    pub headers: BTreeMap<String, String>,
    /// The JSON body.
    pub body: Value,
}

/// How a request reaches a server.
pub trait Transport: fmt::Debug + Send + Sync + 'static {
    /// Send `request` and return its response chunks.
    ///
    /// # Errors
    ///
    /// A typed [`ProviderError`]. A non-2xx status is an error here rather than a
    /// stream item, so a caller cannot accidentally translate an error body as if
    /// it were a chunk.
    fn send(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>>;
}

/// The production transport.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    provider: String,
}

impl ReqwestTransport {
    /// A transport for `provider`, using a fresh client.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self::with_client(provider, reqwest::Client::new())
    }

    /// A transport for `provider` sharing an existing client.
    ///
    /// Sharing matters: a client owns the connection pool, and one per provider
    /// per process is the difference between reusing a TLS session and
    /// renegotiating on every turn.
    #[must_use]
    pub fn with_client(provider: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            client,
            provider: provider.into(),
        }
    }
}

impl Transport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>> {
        let client = self.client.clone();
        let provider = self.provider.clone();
        Box::pin(async move {
            let mut builder = client.post(&request.url).json(&request.body);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let response = builder.send().await.map_err(ProviderError::transient)?;

            let status = response.status();
            if !status.is_success() {
                let header = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(retry_after);
                // Read as bytes and decode strictly: a non-UTF-8 error body loses
                // its detail rather than being rendered with replacement
                // characters that could be mistaken for the vendor's own text.
                let bytes = response.bytes().await.unwrap_or_default();
                let text = std::str::from_utf8(&bytes).ok().map(str::to_owned);
                return Err(classify_response(
                    &provider,
                    status.as_u16(),
                    header,
                    text.as_deref(),
                ));
            }

            let chunks = futures::StreamExt::map(response.bytes_stream(), |result| {
                result
                    .map(|bytes| bytes.to_vec())
                    .map_err(ProviderError::transient)
            });
            Ok(Box::pin(chunks) as ChunkStream)
        })
    }
}

/// Classify a non-2xx response into the typed taxonomy.
///
/// The HTTP status is authoritative for the recovery class. The body refines it
/// only where the status genuinely cannot say which of two classes applies: a
/// `400` carrying `context_length_exceeded` needs compaction rather than failure,
/// and a `400` carrying `content_filter` is a refusal. Both of those are
/// *structured code* reads. The vendor's prose is attached as a source for the
/// human and is never examined.
#[must_use]
pub fn classify_response(
    provider: &str,
    status: u16,
    retry_after_header: Option<std::time::Duration>,
    body: Option<&str>,
) -> ProviderError {
    let wire = body
        .and_then(|text| serde_json::from_str::<ErrorEnvelope>(text).ok())
        .map(ErrorEnvelope::into_error);

    if let Some(error) = &wire {
        match error.code_str() {
            Some("context_length_exceeded") => {
                return ProviderError::ContextLimit {
                    limit_tokens: None,
                    used_tokens: None,
                };
            }
            Some("content_filter") => {
                return ProviderError::Refused {
                    provider: provider.to_owned(),
                    provider_text: error.message.clone(),
                };
            }
            _ => {}
        }
        if error.kind.as_deref() == Some("content_filter") {
            return ProviderError::Refused {
                provider: provider.to_owned(),
                provider_text: error.message.clone(),
            };
        }
    }

    if status == 429 {
        return ProviderError::RateLimited {
            retry_after: retry_after_header,
        };
    }

    let detail = ResponseBody {
        provider: provider.to_owned(),
        status,
        body: body.map(truncate),
    };
    match ProviderError::from_status(provider, status) {
        ProviderError::Auth { provider, .. } => ProviderError::Auth {
            provider,
            source: Some(Box::new(detail)),
        },
        ProviderError::Transient { status, .. } => ProviderError::Transient {
            status,
            source: Some(Box::new(detail)),
        },
        ProviderError::Fatal { status, .. } => ProviderError::Fatal {
            status,
            source: Some(Box::new(detail)),
        },
        // `from_status` returns only those three plus `RateLimited`, which the
        // branch above already handled. Listing the rest keeps this exhaustive so
        // a new variant forces a decision here.
        other @ (ProviderError::RateLimited { .. }
        | ProviderError::ContextLimit { .. }
        | ProviderError::Refused { .. }) => other,
    }
}

/// How much of a vendor error body is worth keeping in a log line.
const BODY_LIMIT: usize = 512;

fn truncate(body: &str) -> String {
    if body.len() <= BODY_LIMIT {
        return body.to_owned();
    }
    // Cut on a character boundary; a byte slice of UTF-8 can split a code point.
    let end = body
        .char_indices()
        .take_while(|(index, _)| *index <= BODY_LIMIT)
        .last()
        .map_or(0, |(index, _)| index);
    format!("{}…", &body[..end])
}

/// The vendor's own error text, kept for display.
#[derive(Debug)]
struct ResponseBody {
    provider: String,
    status: u16,
    body: Option<String>,
}

impl fmt::Display for ResponseBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` returned HTTP {}",
            self.provider, self.status
        )?;
        if let Some(body) = &self.body {
            write!(formatter, ": {body}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ResponseBody {}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_error::Recovery;
    use std::time::Duration;

    #[test]
    fn a_429_carries_the_delay_the_vendor_named() {
        let error = classify_response(
            "groq",
            429,
            Some(Duration::from_secs(12)),
            Some(r#"{"error":{"message":"rate limit"}}"#),
        );
        assert_eq!(
            error.recovery(),
            Recovery::Retry {
                after: Some(Duration::from_secs(12))
            }
        );
    }

    #[test]
    fn a_400_naming_context_length_asks_for_compaction() {
        let error = classify_response(
            "deepseek",
            400,
            None,
            Some(r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#),
        );
        assert_eq!(error.recovery(), Recovery::Compact);
    }

    #[test]
    fn a_401_asks_for_reauthentication_and_keeps_the_body_as_a_source() {
        use std::error::Error as _;
        let error = classify_response("openrouter", 401, None, Some(r#"{"message":"no key"}"#));
        assert_eq!(error.recovery(), Recovery::Reauthenticate);
        let source = error.source().expect("body detail").to_string();
        assert!(source.contains("openrouter"), "{source}");
        assert!(source.contains("401"), "{source}");
    }

    #[test]
    fn a_503_is_retryable_and_a_422_is_not() {
        assert_eq!(
            classify_response("mistral", 503, None, None).recovery(),
            Recovery::Retry { after: None }
        );
        assert_eq!(
            classify_response("mistral", 422, None, None).recovery(),
            Recovery::Fail
        );
    }

    #[test]
    fn a_non_json_body_still_classifies_from_the_status_alone() {
        let error = classify_response("venice", 502, None, Some("<html>bad gateway</html>"));
        assert_eq!(error.recovery(), Recovery::Retry { after: None });
    }

    #[test]
    fn a_long_body_is_truncated_on_a_character_boundary() {
        let body = "。".repeat(400);
        let cut = truncate(&body);
        assert!(cut.len() <= BODY_LIMIT + 4, "{}", cut.len());
        assert!(cut.ends_with('…'));
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }
}
