//! Diagnostics reflect local observations across the provider's redaction boundary.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use zuno_error::{ProviderDiagnosticPhase, ProviderError, Recovery};
use zuno_llm::http::RequestDeadlines;
use zuno_llm::registry::{ApiSurface, CompletionRequest, Provider, Spec};
use zuno_llm::sse::StreamIdleTimeout;
use zuno_provider_compatible::{
    ChunkStream, CompatibleProvider, HttpRequest, HttpTimeouts, Transport,
};

#[derive(Clone, Copy, Debug)]
enum FailureAt {
    HeaderDeadline,
    RequestBudgetBeforeHeaders,
    StreamIdle,
    WireReportedIdle,
    IncompleteStream,
}

#[derive(Debug)]
struct FixtureTransport(FailureAt);

impl Transport for FixtureTransport {
    fn send(
        &self,
        _request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            match self.0 {
                FailureAt::HeaderDeadline | FailureAt::RequestBudgetBeforeHeaders => {
                    #[derive(Debug, thiserror::Error)]
                    #[error("fixture transport failed")]
                    struct WrappedTimeout(#[source] ProviderError);

                    let total = matches!(self.0, FailureAt::RequestBudgetBeforeHeaders)
                        .then_some(Duration::from_millis(10));
                    let error = RequestDeadlines::start(HttpTimeouts::new(
                        total,
                        Some(Duration::from_millis(30)),
                        None,
                    ))
                    .headers("reflected fixture-secret", std::future::pending::<()>())
                    .await
                    .expect_err("the fixture never returns headers");
                    Err(ProviderError::transient(WrappedTimeout(error)))
                }
                FailureAt::StreamIdle => Ok(Box::pin(futures::stream::pending()) as ChunkStream),
                FailureAt::WireReportedIdle => {
                    let event = b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"upstream_stream_idle_timeout\",\"message\":\"reflected fixture-secret\"}}}\n\n";
                    Ok(
                        Box::pin(futures::stream::once(async move { Ok(event.to_vec()) }))
                            as ChunkStream,
                    )
                }
                FailureAt::IncompleteStream => {
                    Ok(Box::pin(futures::stream::empty()) as ChunkStream)
                }
            }
        })
    }
}

fn provider(failure: FailureAt) -> CompatibleProvider {
    CompatibleProvider::new(
        Spec::new("diagnostic-fixture")
            .with_base_url("https://fixture.invalid/v1")
            .with_surface(ApiSurface::Responses)
            .with_option("transport", serde_json::json!("openai-compatible")),
        Arc::new(FixtureTransport(failure)),
        Some("fixture-secret".to_owned()),
    )
    .expect("explicit compatible fixture")
    .with_idle_timeout(StreamIdleTimeout::new(Duration::from_millis(20)))
}

#[tokio::test(start_paused = true)]
async fn observed_phases_survive_the_provider_boundary_without_fabricating_http_facts() {
    let cases = [
        (
            FailureAt::HeaderDeadline,
            ProviderDiagnosticPhase::ResponseHeaders,
        ),
        (
            FailureAt::RequestBudgetBeforeHeaders,
            ProviderDiagnosticPhase::RequestBudget,
        ),
        (FailureAt::StreamIdle, ProviderDiagnosticPhase::StreamIdle),
    ];
    for (failure, phase) in cases {
        let provider = provider(failure);
        let mut stream = provider.stream(CompletionRequest::new("fixture-model", Vec::new()));
        let error = stream.next().await.unwrap().expect_err("fixture fails");
        assert_eq!(error.recovery(), Recovery::Retry { after: None });
        let snapshot = error.diagnostic_snapshot();
        assert_eq!(snapshot.phase(), phase, "{failure:?}");
        assert_eq!(snapshot.fields()["phase"], phase.as_str());
        assert!(snapshot.fields()["status"].is_null());
        assert!(snapshot.fields()["requestID"].is_null());
        assert!(!snapshot.fields().to_string().contains("fixture-secret"));
        assert!(!error.diagnostic().contains("fixture-secret"));
        assert!(
            stream.next().await.is_none(),
            "one failure must terminate this attempt"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn upstream_timeout_codes_and_eof_do_not_prove_a_local_timeout_phase() {
    let cases = [
        (FailureAt::WireReportedIdle, "upstream_stream_idle_timeout"),
        (FailureAt::IncompleteStream, "upstream_stream_incomplete"),
    ];
    for (failure, code) in cases {
        let provider = provider(failure);
        let mut stream = provider.stream(CompletionRequest::new("fixture-model", Vec::new()));
        let error = stream.next().await.unwrap().expect_err("fixture fails");
        assert_eq!(error.structured_code(), Some(code));
        assert!(error.permits_partial_output_retry());
        let fields = error.diagnostic_fields();
        assert_eq!(fields["phase"], "unknown");
        assert!(fields["status"].is_null());
        assert!(fields["requestID"].is_null());
        assert!(!fields.to_string().contains("fixture-secret"));
        assert!(stream.next().await.is_none());
    }
}
