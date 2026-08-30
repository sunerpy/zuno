//! The two QA scenarios for this profile, runnable and self-checking.
//!
//! ```text
//! cargo run -p zuno-provider-compatible --example qa_scenarios
//! ```
//!
//! Both are covered by assertions in `tests/rules.rs`; this exists so the
//! behaviour can be *read* rather than inferred from a passing test — in
//! particular so the refusal message can be seen exactly as a user sees it.

use std::pin::Pin;
use std::sync::Arc;

use zuno_error::ProviderError;
use zuno_llm::event::{Message, RequestContentBlock, Role};
use zuno_llm::registry::{CompletionRequest, Declined, Spec};
use zuno_provider_compatible::{ChunkStream, CompatibleProvider, HttpRequest, Transport};

#[derive(Debug)]
struct NeverSends;

impl Transport for NeverSends {
    fn send(
        &self,
        _request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>> {
        unreachable!("this example builds requests; it never sends one")
    }
}

fn main() {
    happy_reasoning_content_echo();
    println!();
    failure_amazon_bedrock_is_refused();
}

/// Happy path: a provider emitting `reasoning_content` has it echoed back only
/// when the model requires it.
fn happy_reasoning_content_echo() {
    let provider = CompatibleProvider::new(
        Spec::new("deepseek").with_base_url("https://api.deepseek.com/v1"),
        Arc::new(NeverSends),
        Some("token".to_owned()),
    )
    .expect("deepseek is a claimed provider id");

    let history = vec![
        Message::new(Role::User, "will it rain in Paris?"),
        Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::SignedThinking {
                    thinking: "check the forecast for Paris".to_owned(),
                    signature: String::new(),
                },
                RequestContentBlock::Text {
                    text: "No.".to_owned(),
                },
            ],
        ),
        Message::new(Role::User, "and tomorrow?"),
    ];

    println!("== HAPPY: reasoning_content is echoed only when the model requires it ==");
    for model in ["deepseek-v4-flash", "deepseek-chat"] {
        let body = provider.body_for(&CompletionRequest::new(model, history.clone()));
        let assistant = &body["messages"][1];
        println!(
            "\n-- model `{model}` --\n\
             reasoning_protocol : {}\n\
             assistant message  : {}\n\
             body.thinking      : {}",
            provider
                .quirks_for(model, zuno_llm::registry::ApiSurface::Default)
                .reasoning_protocol,
            serde_json::to_string(assistant).expect("serializable"),
            body.get("thinking")
                .map_or_else(|| "<absent>".to_owned(), ToString::to_string),
        );
    }

    let required = provider.body_for(&CompletionRequest::new(
        "deepseek-v4-flash",
        history.clone(),
    ));
    let not_required = provider.body_for(&CompletionRequest::new("deepseek-chat", history));
    assert!(required["messages"][1].get("reasoning_content").is_some());
    assert!(
        not_required["messages"][1]
            .get("reasoning_content")
            .is_none()
    );
    println!("\nOK: echoed for deepseek-v4-flash, omitted for deepseek-chat.");
}

/// Failure path: `amazon-bedrock` configured against this profile is refused with
/// a message naming the crate that carries it.
fn failure_amazon_bedrock_is_refused() {
    println!("== FAILURE: amazon-bedrock configured against the compatible profile ==\n");

    let declined = CompatibleProvider::new(
        Spec::new("amazon-bedrock")
            .with_base_url("https://bedrock-runtime.us-east-1.amazonaws.com"),
        Arc::new(NeverSends),
        Some("token".to_owned()),
    )
    .expect_err("bedrock is not OpenAI-compatible on the wire");

    let Declined::Failed(error) = declined else {
        panic!("a misrouted provider is a failure, not an availability state");
    };

    println!("ProviderError variant : {error:?}");
    println!("recovery              : {:?}", error.recovery());
    println!("rendered              :");
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut depth = 0;
    while let Some(current) = source {
        println!("  {:indent$}{current}", "", indent = depth * 2);
        source = current.source();
        depth += 1;
    }

    assert_eq!(error.recovery(), zuno_error::Recovery::Fail);
    println!("\nOK: refused before any request was built, and never retried.");
}
