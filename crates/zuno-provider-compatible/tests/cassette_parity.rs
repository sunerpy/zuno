//! Cassette parity: this profile against real recorded traffic from six vendors.
//!
//! # What "parity" means here, and what it does not
//!
//! Each test drives the **production path** — [`CompatibleProvider::stream`], which
//! reaches bytes only through [`Transport`], and decodes them only through
//! [`zuno_llm::sse::SseParser`] — over the bytes a real vendor really sent. The
//! recorded response is fed in **small slices** so a frame boundary lands mid-chunk
//! on every cassette, which is the failure mode a whole-body parse hides.
//!
//! The assertion is the resulting [`StreamEvent`] sequence, compared exactly. It is
//! not a snapshot: an expected sequence is written out in the test, so a change in
//! translation shows up as a diff of events rather than as a blessed file.
//!
//! **Request bodies are deliberately not compared byte-for-byte.** The recorded
//! requests were produced by the Vercel AI SDK and carry fields this profile does
//! not originate (`stream_options.include_usage`, its own `max_tokens` default).
//! Claiming request parity would be claiming something untrue. What *is* asserted
//! is the recorded **URL**, wherever the recorder did not redact it — that is a
//! real, checkable claim about where this profile sends a request.
//!
//! # Which vendors, and why not more
//!
//! The pinned corpus holds seven OpenAI-compatible endpoints. Six have a provider
//! id this profile claims and are replayed here: DeepSeek, Groq, OpenRouter,
//! TogetherAI, Cloudflare Workers AI, and the Cloudflare AI Gateway. The seventh
//! is `api.openai.com` itself, whose id this profile **refuses** — it belongs to
//! `zuno-provider-openai` — so replaying it under a claimed id would be mislabelling
//! it. It appears in one clearly-named test as canonical-shape evidence and is not
//! counted toward the four.

use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt;
use zuno_error::ProviderError;
use zuno_llm::event::{Message, Role};
use zuno_llm::registry::{
    ApiSurface, CompletionRequest, FinishReason, Provider, Spec, StreamEvent,
};
use zuno_provider_compatible::{ChunkStream, CompatibleProvider, HttpRequest, Transport};
use zuno_testkit::cassette::CassettePlayer;

fn recordings_available(test: &str) -> bool {
    zuno_testkit::recordings_root_or_skip(
        test,
        "compatible-provider cassette replay was NOT tested",
    )
    .is_some()
}

/// A transport that answers from a cassette instead of a socket.
///
/// It captures the request it was handed so a test can assert on the URL, and it
/// hands the recorded body back in `slice` byte pieces. The recorder buffered the
/// whole stream, so the original network boundaries are gone; re-slicing restores
/// the property that matters — that a frame separator and a multi-byte code point
/// can land across two chunks.
#[derive(Debug)]
struct CassetteTransport {
    body: Vec<u8>,
    slice: usize,
    captured: std::sync::Mutex<Option<HttpRequest>>,
}

impl CassetteTransport {
    fn load(name: &str, slice: usize) -> (Arc<Self>, String) {
        let mut player = CassettePlayer::from_oracle(name)
            .unwrap_or_else(|error| panic!("cassette {name}: {error}"));
        let interaction = player
            .next_unchecked()
            .unwrap_or_else(|error| panic!("cassette {name}: {error}"));
        let url = interaction.request.url.clone();
        let body = interaction
            .response
            .decoded_body(name, 1)
            .unwrap_or_else(|error| panic!("cassette {name}: {error}"));
        assert!(
            interaction.response.is_sse(),
            "cassette {name} is not an SSE recording; this profile only replays SSE"
        );
        (
            Arc::new(Self {
                body,
                slice,
                captured: std::sync::Mutex::new(None),
            }),
            url,
        )
    }

    fn captured_url(&self) -> String {
        self.captured
            .lock()
            .expect("no test panics while holding this lock")
            .as_ref()
            .expect("a request was sent")
            .url
            .clone()
    }
}

impl Transport for CassetteTransport {
    fn send(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>> {
        *self
            .captured
            .lock()
            .expect("no test panics while holding this lock") = Some(request);
        let pieces: Vec<Vec<u8>> = self.body.chunks(self.slice).map(<[u8]>::to_vec).collect();
        Box::pin(async move {
            Ok(Box::pin(futures::stream::iter(pieces.into_iter().map(Ok))) as ChunkStream)
        })
    }
}

/// Replay one cassette through the production path and return the events.
async fn replay(
    provider_id: &'static str,
    base_url: &str,
    model_id: &str,
    cassette: &str,
    slice: usize,
) -> (Vec<StreamEvent>, String, String) {
    let (transport, recorded_url) = CassetteTransport::load(cassette, slice);
    let provider = CompatibleProvider::new(
        Spec::new(provider_id).with_base_url(base_url),
        Arc::clone(&transport) as Arc<dyn Transport>,
        Some("test-token".to_owned()),
    )
    .unwrap_or_else(|error| panic!("{provider_id} must be a claimed provider id: {error:?}"));

    let request = CompletionRequest::new(
        model_id,
        vec![Message::new(Role::User, "replayed from a cassette")],
    );
    let events: Vec<StreamEvent> = provider
        .stream(request)
        .map(|item| item.expect("a recorded 200 response translates without error"))
        .collect()
        .await;

    (events, recorded_url, transport.captured_url())
}

fn text(value: &str) -> StreamEvent {
    StreamEvent::TextDelta(value.to_owned())
}

fn stop(reason: FinishReason) -> StreamEvent {
    StreamEvent::MessageEnd {
        stop_reason: Some(reason),
    }
}

fn tool_start(id: &str, name: &str) -> StreamEvent {
    StreamEvent::ToolUseStart {
        id: id.to_owned(),
        name: name.to_owned(),
    }
}

fn tool_input(value: &str) -> StreamEvent {
    StreamEvent::ToolInputDelta(value.to_owned())
}

/// Vendor 1 of 6 — DeepSeek, `api.deepseek.com`.
#[tokio::test]
async fn deepseek_text_replays_to_the_recorded_event_sequence() {
    if !recordings_available("deepseek_text_replays_to_the_recorded_event_sequence") {
        return;
    }
    let (events, recorded_url, sent_url) = replay(
        "deepseek",
        "https://api.deepseek.com/v1",
        "deepseek-v4-flash",
        "openai-compatible-chat/deepseek-streams-text",
        7,
    )
    .await;

    assert_eq!(
        events,
        vec![
            text("Hello"),
            text("!"),
            stop(FinishReason::Stop),
            StreamEvent::TokenUsage {
                input_tokens: Some(14),
                output_tokens: Some(2),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ]
    );
    assert_eq!(
        sent_url, recorded_url,
        "this profile targets the recorded URL"
    );
}

/// Vendor 2 of 6 — Groq, `api.groq.com`. Includes a tool call.
#[tokio::test]
async fn groq_tool_call_replays_to_the_recorded_event_sequence() {
    if !recordings_available("groq_tool_call_replays_to_the_recorded_event_sequence") {
        return;
    }
    let (events, recorded_url, sent_url) = replay(
        "groq",
        "https://api.groq.com/openai/v1",
        "llama-3.3-70b-versatile",
        "openai-compatible-chat/groq-streams-tool-call",
        7,
    )
    .await;

    let usage = StreamEvent::TokenUsage {
        input_tokens: Some(249),
        output_tokens: Some(10),
        cache_read_input_tokens: None,
        cache_write_input_tokens: None,
    };
    assert_eq!(
        events,
        vec![
            tool_start("mcf2d8nn1", "get_weather"),
            tool_input(r#"{"city":"Paris"}"#),
            StreamEvent::ToolUseEnd,
            stop(FinishReason::ToolCalls),
            usage.clone(),
            // Groq repeats its usage on a final `choices: []` chunk. A second
            // accounting event is what the wire says; suppressing it here would
            // hide a real duplicate from the consumer that has to reconcile it.
            usage,
        ]
    );
    assert_eq!(sent_url, recorded_url);
}

/// Vendor 3 of 6 — OpenRouter, `openrouter.ai`. Reports its upstream, and opens
/// with an SSE **comment** frame (`: OPENROUTER PROCESSING`) that must not become
/// an event.
#[tokio::test]
async fn openrouter_text_reports_its_upstream_and_ignores_the_comment_frame() {
    if !recordings_available("openrouter_text_reports_its_upstream_and_ignores_the_comment_frame") {
        return;
    }
    let (events, recorded_url, sent_url) = replay(
        "openrouter",
        "https://openrouter.ai/api/v1",
        "openai/gpt-4o-mini",
        "openai-compatible-chat/openrouter-streams-text",
        7,
    )
    .await;

    assert_eq!(
        events,
        vec![
            StreamEvent::UpstreamProvider {
                provider: "Azure".to_owned()
            },
            text("Hello"),
            text("!"),
            stop(FinishReason::Stop),
            StreamEvent::TokenUsage {
                input_tokens: Some(21),
                output_tokens: Some(3),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ]
    );
    assert_eq!(sent_url, recorded_url);
}

/// Vendor 4 of 6 — TogetherAI, `api.together.xyz`. Splits a tool call's name and
/// arguments across two chunks, with no id on the second.
#[tokio::test]
async fn togetherai_tool_call_survives_a_split_tool_fragment() {
    if !recordings_available("togetherai_tool_call_survives_a_split_tool_fragment") {
        return;
    }
    let (events, recorded_url, sent_url) = replay(
        "togetherai",
        "https://api.together.xyz/v1",
        "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "openai-compatible-chat/togetherai-streams-tool-call",
        7,
    )
    .await;

    assert_eq!(
        events,
        vec![
            tool_start("call_yu1mxtmex7x48nximi9c8jpo", "get_weather"),
            tool_input(r#"{"city":"Paris"}"#),
            StreamEvent::ToolUseEnd,
            stop(FinishReason::ToolCalls),
            StreamEvent::TokenUsage {
                input_tokens: Some(194),
                output_tokens: Some(19),
                cache_read_input_tokens: None,
                cache_write_input_tokens: None,
            },
        ]
    );
    assert_eq!(sent_url, recorded_url);
}

/// Vendor 5 of 6 — Cloudflare Workers AI, `api.cloudflare.com`.
///
/// This is the cassette that carries the **non-standard `reasoning_content`**
/// field: fourteen `delta.reasoning_content` fragments, the first of which is the
/// empty string. Its URL is redacted by the recorder (`{account}`), so only the
/// events are asserted.
#[tokio::test]
async fn cloudflare_workers_ai_reasoning_content_becomes_reasoning_events() {
    if !recordings_available("cloudflare_workers_ai_reasoning_content_becomes_reasoning_events") {
        return;
    }
    let (events, recorded_url, _sent) = replay(
        "cloudflare-workers-ai",
        "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1",
        "@cf/openai/gpt-oss-20b",
        "cloudflare-workers-ai/cloudflare-workers-ai-gpt-oss-20b-tools-tool-call",
        7,
    )
    .await;

    assert!(
        recorded_url.contains("{account}"),
        "the recorder redacts this account id, so URL parity is not assertable here"
    );
    assert_reasoning_then_tool_call(&events);
}

/// Vendor 6 of 6 — the Cloudflare AI Gateway, `gateway.ai.cloudflare.com`.
///
/// A distinct endpoint from Workers AI with its own `/compat/chat/completions`
/// path, emitting the same `reasoning_content` shape.
#[tokio::test]
async fn cloudflare_ai_gateway_reasoning_content_becomes_reasoning_events() {
    if !recordings_available("cloudflare_ai_gateway_reasoning_content_becomes_reasoning_events") {
        return;
    }
    let (events, _recorded, _sent) = replay(
        "cloudflare-ai-gateway",
        "https://gateway.ai.cloudflare.com/v1/acct/gw/compat",
        "workers-ai/@cf/openai/gpt-oss-20b",
        "cloudflare-ai-gateway/cloudflare-ai-gateway-workers-ai-gpt-oss-20b-tools-tool-call",
        7,
    )
    .await;

    assert_reasoning_then_tool_call(&events);
}

/// The shape both Cloudflare cassettes produce.
fn assert_reasoning_then_tool_call(events: &[StreamEvent]) {
    assert_eq!(
        events.first(),
        Some(&StreamEvent::ReasoningStart),
        "the first fragment is an empty `reasoning_content`, which must still open \
         the block: {events:?}"
    );

    let reasoning: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ReasoningDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasoning, "We need to call the function get_weather with city \"Paris\".",
        "the whole non-standard reasoning stream must survive re-slicing"
    );

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ReasoningEnd))
            .count(),
        1,
        "reasoning must be closed exactly once: {events:?}"
    );

    let arguments: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolInputDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(arguments, r#"{"city":"Paris"}"#);

    assert!(
        events.contains(&tool_start("chatcmpl-tool-call-0", "get_weather"))
            || events
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolUseStart { name, .. } if name == "get_weather")),
        "the tool call must be named: {events:?}"
    );

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::MessageEnd { .. }))
            .count(),
        1,
        "two chunks carry `finish_reason: tool_calls`; only one message may end: {events:?}"
    );
}

/// Canonical-shape evidence only. `api.openai.com`'s provider id is **refused** by
/// this profile, so this cassette is not one of the six vendors claimed above.
#[tokio::test]
async fn the_canonical_openai_chat_shape_parses_under_a_declared_compatible_id() {
    if !recordings_available(
        "the_canonical_openai_chat_shape_parses_under_a_declared_compatible_id",
    ) {
        return;
    }
    let (transport, recorded_url) = CassetteTransport::load("openai-chat/streams-text", 7);
    // Configured as a user's own declared-compatible endpoint, which is the only
    // honest way to route this shape through this profile.
    let spec = Spec::new("my-openai-proxy")
        .with_base_url("https://api.openai.com/v1")
        .with_option(
            zuno_provider_compatible::family::NPM_OPTION,
            serde_json::json!(zuno_provider_compatible::family::OPENAI_COMPATIBLE_NPM),
        );
    let provider = CompatibleProvider::new(
        spec,
        Arc::clone(&transport) as Arc<dyn Transport>,
        Some("test-token".to_owned()),
    )
    .expect("a declared-compatible id is accepted");

    let events: Vec<StreamEvent> = provider
        .stream(CompletionRequest::new(
            "gpt-4o-mini",
            vec![Message::new(Role::User, "hi")],
        ))
        .map(|item| item.expect("translates"))
        .collect()
        .await;

    assert_eq!(
        events,
        vec![
            text("Hello"),
            text("!"),
            stop(FinishReason::Stop),
            StreamEvent::TokenUsage {
                input_tokens: Some(22),
                output_tokens: Some(2),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ]
    );
    assert_eq!(transport.captured_url(), recorded_url);
}

/// The same cassette, sliced one byte at a time.
///
/// `zuno-llm`'s parser already proves byte-split safety with a 4220-offset sweep;
/// this asserts that this profile's use of it inherits the property, rather than
/// re-testing the parser.
#[tokio::test]
async fn a_one_byte_at_a_time_stream_produces_the_identical_sequence() {
    if !recordings_available("a_one_byte_at_a_time_stream_produces_the_identical_sequence") {
        return;
    }
    let (whole, _, _) = replay(
        "deepseek",
        "https://api.deepseek.com/v1",
        "deepseek-v4-flash",
        "openai-compatible-chat/deepseek-streams-text",
        usize::MAX,
    )
    .await;
    let (single, _, _) = replay(
        "deepseek",
        "https://api.deepseek.com/v1",
        "deepseek-v4-flash",
        "openai-compatible-chat/deepseek-streams-text",
        1,
    )
    .await;
    assert_eq!(whole, single);
}

/// Every claimed provider id constructs, and nothing else is silently admitted.
#[test]
fn every_claimed_provider_id_constructs_with_only_a_base_url() {
    #[derive(Debug)]
    struct Unused;
    impl Transport for Unused {
        fn send(
            &self,
            _request: HttpRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>> {
            unreachable!("no request is sent")
        }
    }

    for profile in zuno_provider_compatible::family::CLAIMED {
        let provider = CompatibleProvider::new(
            Spec::new(profile.provider).with_base_url("https://endpoint.test/v1"),
            Arc::new(Unused),
            None,
        )
        .unwrap_or_else(|error| panic!("{} must construct: {error:?}", profile.provider));
        assert_eq!(provider.id(), profile.provider);
        assert!(
            provider
                .endpoint("some-model", ApiSurface::Default)
                .starts_with("https://endpoint.test/v1/")
        );
    }
}
