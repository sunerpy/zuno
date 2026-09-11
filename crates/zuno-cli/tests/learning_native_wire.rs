//! Cross-provider learning wire coverage belongs at the CLI composition boundary.
//!
//! Keeping these tests here leaves the core dependency closure free of native
//! provider implementations, including development dependencies. Provider replies
//! are scripted; the native builders below never open a transport.

use futures::stream;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use zuno_config::ResolvedLearningConfig;
use zuno_db::{Pool, migration};
use zuno_learning::{ExtractionRequest, LearningExtractor, LearningModel, LearningModelClient};
use zuno_llm::{
    event::{FinishReason, StreamEvent},
    registry::{
        ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, Spec, generation,
    },
};
use zuno_paths::DbLocation;

fn pool() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    {
        let mut db = pool.get().expect("connection");
        migration::apply(&mut db).expect("schema");
        db.execute_batch(r#"
          INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) VALUES('p','/work',1,1,'[]');
          INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
          VALUES('s','p','s','/work','test','1',1,1),('s2','p','s2','/work','test two','1',1,1);
          INSERT INTO message(id,session_id,time_created,time_updated,data)
          VALUES('m','s',2,2,'{"role":"assistant","finish":"stop","time":{"completed":2}}');
        "#).expect("fixture");
    }
    pool
}

#[derive(Debug)]
struct ScriptedProvider {
    replies: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "test"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .replies
            .lock()
            .expect("replies")
            .pop_front()
            .expect("unexpected model request");
        Box::pin(stream::iter(events.into_iter().map(Ok)))
    }
}

fn answer(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ]
}

fn client(
    replies: Vec<Vec<StreamEvent>>,
) -> (LearningModelClient, Arc<Mutex<Vec<CompletionRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        LearningModelClient {
            provider: Arc::new(ScriptedProvider {
                replies: Mutex::new(replies.into()),
                requests: requests.clone(),
            }),
            model: LearningModel {
                provider_id: "test".to_owned(),
                model_id: "model".to_owned(),
                wire_id: "model".to_owned(),
                surface: ApiSurface::Chat,
                parameters: Default::default(),
                headers: Default::default(),
                sampling_params: true,
            },
            events: zuno_db::event_log::SessionEventLog::new(pool()),
            limits: ResolvedLearningConfig::default(),
        },
        requests,
    )
}

fn request() -> ExtractionRequest {
    ExtractionRequest {
        project_id: "p".to_owned(),
        session_id: "s".to_owned(),
        source_message_id: "m".to_owned(),
        transcript: "legacy transcript must not reach the model".repeat(10_000),
        sources: Vec::new(),
        sources_truncated: false,
        had_tool_calls: true,
        had_artifacts: false,
        explicit_feedback: false,
        user_corrected: false,
        recovered_from_error: false,
    }
}

fn native_learning_request_body(
    request: &CompletionRequest,
    provider_output_limit: u64,
) -> serde_json::Value {
    let spec = Spec::new("test").with_option(generation::MAX_TOKENS, json!(provider_output_limit));
    match request.surface {
        ApiSurface::Chat | ApiSurface::Responses => {
            let config =
                zuno_provider_openai::OpenAiConfig::try_from_spec(spec).expect("native config");
            zuno_provider_openai::build_request_body(request, &config)
                .expect("native OpenAI request body")
        }
        ApiSurface::Messages => {
            let config = zuno_provider_anthropic::AnthropicConfig::from_spec(spec);
            let mut body = zuno_provider_anthropic::build_request_body(request, &config)
                .expect("native Anthropic request body");
            // Anthropic start_stream applies request-local options after its builder.
            request.apply_parameters(&mut body, ApiSurface::Messages);
            body
        }
        ApiSurface::Default => panic!("fixture must select an actual native surface"),
    }
}

async fn assert_learning_output_limit_reaches_native_wire(surface: ApiSurface, wire_key: &str) {
    for (source_parameters, expected_limit) in [
        (json!({"maxTokens": 0}), 512),
        (
            json!({
                "maxTokens": 0, "max_tokens": 4096,
                "max_output_tokens": 128, "max_completion_tokens": 0
            }),
            128,
        ),
    ] {
        let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
        client.model.surface = surface;
        client.model.parameters = serde_json::from_value(source_parameters).expect("model options");
        client.limits.execution_max_output_tokens = 512;
        client.extract(request()).await.expect("bounded extraction");
        let requests = requests.lock().expect("captured request");
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        let body = native_learning_request_body(sent, 8192);
        assert!(
            body.get(generation::MAX_TOKENS).is_none(),
            "SDK maxTokens must not leak into the native {surface:?} body: {:?}",
            body.get(generation::MAX_TOKENS)
        );
        assert_eq!(
            native_learning_request_body(sent, 0),
            body,
            "an unspecified provider cap must still produce the same bounded native request"
        );
        assert_eq!(
            body[wire_key], expected_limit,
            "native wire must enforce the execution cap"
        );
        assert_eq!(
            body["model"], sent.model_id,
            "never switch the selected model"
        );
        assert!(body.get("tools").is_none());
        assert!(
            body.get("metadata").is_none(),
            "learning must stay detached"
        );
        for other in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
            if other != wire_key {
                assert!(body.get(other).is_none(), "unexpected wire alias {other}");
            }
        }
        let mut foreground = sent.clone();
        foreground.parameters.remove(generation::MAX_TOKENS);
        assert_eq!(
            body,
            native_learning_request_body(&foreground, expected_limit),
            "the normalized learning limit must use the foreground native configuration path"
        );
        assert_eq!(
            client.events.read_after("s", None).expect("audit events")[0].properties["request"]["parameters"]
                [generation::MAX_TOKENS],
            expected_limit,
            "the durable request records the bounded semantic cap before native lowering"
        );
    }
}

#[tokio::test]
async fn learning_output_limit_reaches_native_responses_wire() {
    assert_learning_output_limit_reaches_native_wire(ApiSurface::Responses, "max_output_tokens")
        .await;
}

#[tokio::test]
async fn learning_output_limit_reaches_native_chat_wire() {
    assert_learning_output_limit_reaches_native_wire(ApiSurface::Chat, "max_tokens").await;
}

#[tokio::test]
async fn learning_output_limit_reaches_native_anthropic_wire() {
    assert_learning_output_limit_reaches_native_wire(ApiSurface::Messages, "max_tokens").await;
}
