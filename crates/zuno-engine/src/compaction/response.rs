use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use zuno_config::schema::CompactionConfig;
use zuno_error::Recovery;
use zuno_llm::catalog::resolved::ModelCost;
use zuno_llm::event::{FinishReason, PromptAccounting, StreamEvent};
use zuno_llm::registry::{CompletionRequest, Provider};

use super::CompactionStopReason;
use crate::interrupt::InterruptSignal;

pub(super) const DEFAULT_TIMEOUT_SECONDS: u32 = 180;
pub(super) const DEFAULT_MAX_SUMMARY_BYTES: u32 = 65_536;

#[derive(Debug)]
pub(super) struct SummaryFailure {
    pub reason: CompactionStopReason,
    pub message: String,
    pub recovery: Recovery,
}

#[derive(Debug, Default)]
pub(super) struct SummaryResponse {
    pub text: String,
    pub usage: Option<SummaryUsage>,
    pub failure: Option<SummaryFailure>,
}

#[derive(Debug)]
pub(super) struct SummaryUsage {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    accounting: PromptAccounting,
}

impl SummaryUsage {
    pub fn tokens(&self) -> Value {
        json!({
            "input": self.input.unwrap_or(0),
            "output": self.visible_output(),
            "reasoning": self.reasoning.unwrap_or(0),
            "cache": {
                "read": self.cache_read.unwrap_or(0),
                "write": self.cache_write.unwrap_or(0),
            },
            "accounting": self.accounting.as_str(),
        })
    }

    pub fn cost(&self, model: &ModelCost) -> f64 {
        let input = self.input.unwrap_or(0);
        let read = self.cache_read.unwrap_or(0);
        let write = self.cache_write.unwrap_or(0);
        model.charge(
            self.accounting.uncached_input(input, read, write),
            self.visible_output(),
            self.reasoning.unwrap_or(0),
            read,
            write,
        )
    }

    fn visible_output(&self) -> u64 {
        self.output
            .unwrap_or(0)
            .saturating_sub(self.reasoning.unwrap_or(0))
    }
}

impl SummaryResponse {
    fn stop(
        &mut self,
        reason: CompactionStopReason,
        message: impl Into<String>,
        recovery: Recovery,
    ) {
        self.failure = Some(SummaryFailure {
            reason,
            message: message.into(),
            recovery,
        });
    }
}

/// Collect one attempt without accepting partial, replayed, or unbounded output.
pub(super) async fn receive(
    provider: &dyn Provider,
    request: CompletionRequest,
    config: &CompactionConfig,
    interrupt: Option<&InterruptSignal>,
) -> SummaryResponse {
    if interrupt.is_some_and(InterruptSignal::is_set) {
        let mut response = SummaryResponse::default();
        response.stop(
            CompactionStopReason::Interrupted,
            "context compaction was interrupted",
            Recovery::Fail,
        );
        return response;
    }
    let timeout_seconds = config
        .timeout_seconds
        .map_or(DEFAULT_TIMEOUT_SECONDS, |value| value.get());
    let maximum_bytes = config
        .max_summary_bytes
        .map_or(DEFAULT_MAX_SUMMARY_BYTES, |value| value.get()) as usize;
    let timeout = tokio::time::sleep(Duration::from_secs(u64::from(timeout_seconds)));
    tokio::pin!(timeout);
    let interrupted = async {
        match interrupt {
            Some(signal) => signal.notified().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(interrupted);
    let mut stream = provider.stream(request);
    let mut response = SummaryResponse::default();
    let mut completed = false;
    loop {
        let event = tokio::select! {
            biased;
            () = &mut interrupted => {
                response.stop(
                    CompactionStopReason::Interrupted,
                    "context compaction was interrupted",
                    Recovery::Fail,
                );
                break;
            }
            () = &mut timeout => {
                response.stop(
                    CompactionStopReason::Provider,
                    format!("compaction summary exceeded its {timeout_seconds}s deadline"),
                    Recovery::Retry { after: None },
                );
                break;
            }
            event = stream.next() => event,
        };
        let Some(event) = event else {
            if !completed {
                response.stop(
                    CompactionStopReason::Provider,
                    "compaction stream ended before a terminal message",
                    Recovery::Retry { after: None },
                );
            }
            break;
        };
        match event {
            Ok(StreamEvent::TextDelta(text)) => {
                if completed {
                    response.stop(
                        CompactionStopReason::Provider,
                        "compaction emitted text after its terminal message",
                        Recovery::Fail,
                    );
                } else if response.text.len().saturating_add(text.len()) > maximum_bytes {
                    response.stop(
                        CompactionStopReason::OutputLimit,
                        format!("compaction summary exceeded {maximum_bytes} UTF-8 bytes"),
                        Recovery::Fail,
                    );
                } else {
                    response.text.push_str(&text);
                }
            }
            Ok(StreamEvent::RetryRollback { .. }) => {
                response.text.clear();
                response.usage = None;
                completed = false;
            }
            Ok(StreamEvent::MessageEnd { stop_reason }) => match stop_reason {
                None | Some(FinishReason::Stop | FinishReason::Unknown) => completed = true,
                Some(reason) => response.stop(
                    CompactionStopReason::Provider,
                    format!("compaction did not complete normally: {reason}"),
                    Recovery::Fail,
                ),
            },
            Ok(StreamEvent::TokenUsage {
                input_tokens,
                output_tokens,
                reasoning_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
                accounting,
            }) => {
                let usage = response.usage.get_or_insert(SummaryUsage {
                    input: None,
                    output: None,
                    reasoning: None,
                    cache_read: None,
                    cache_write: None,
                    accounting,
                });
                usage.input = input_tokens.or(usage.input);
                usage.output = output_tokens.or(usage.output);
                usage.reasoning = reasoning_tokens.or(usage.reasoning);
                usage.cache_read = cache_read_input_tokens.or(usage.cache_read);
                usage.cache_write = cache_write_input_tokens.or(usage.cache_write);
                usage.accounting = accounting;
            }
            Ok(StreamEvent::Error {
                message,
                retry_after,
            }) => response.stop(
                CompactionStopReason::Provider,
                message,
                Recovery::Retry { after: retry_after },
            ),
            Err(error) => response.stop(
                CompactionStopReason::Provider,
                error.to_string(),
                match error.recovery() {
                    Recovery::Compact => Recovery::Fail,
                    recovery => recovery,
                },
            ),
            Ok(
                StreamEvent::ToolUseStart { .. }
                | StreamEvent::ToolInputDelta { .. }
                | StreamEvent::ToolUseEnd { .. }
                | StreamEvent::ToolUseSignature { .. }
                | StreamEvent::ToolResult { .. }
                | StreamEvent::GeneratedImage { .. }
                | StreamEvent::NativeToolCall { .. },
            ) => response.stop(
                CompactionStopReason::Provider,
                "the tool-free compactor returned a tool operation",
                Recovery::Fail,
            ),
            Ok(
                StreamEvent::ReasoningStart
                | StreamEvent::ReasoningDelta(_)
                | StreamEvent::ReasoningSignatureDelta(_)
                | StreamEvent::ProviderReasoningItem { .. }
                | StreamEvent::ReasoningEnd
                | StreamEvent::ReasoningDone { .. }
                | StreamEvent::ConnectionType { .. }
                | StreamEvent::ConnectionPhase { .. }
                | StreamEvent::StatusDetail { .. }
                | StreamEvent::SessionId(_)
                | StreamEvent::Compaction { .. }
                | StreamEvent::UpstreamProvider { .. },
            ) => {}
        }
        if response.failure.is_some() {
            break;
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_llm::registry::{Capabilities, ProviderStream};

    #[derive(Debug)]
    struct Events {
        events: Vec<StreamEvent>,
        stay_open: bool,
    }

    impl Provider for Events {
        fn id(&self) -> &str {
            "compaction-response-test"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::text_only()
        }

        fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
            let events = futures::stream::iter(self.events.clone().into_iter().map(Ok));
            if self.stay_open {
                Box::pin(events.chain(futures::stream::pending()))
            } else {
                Box::pin(events)
            }
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest::new("summary", Vec::new())
    }

    #[tokio::test(start_paused = true)]
    async fn an_unfinished_summary_has_a_deadline() {
        let provider = Events {
            events: vec![StreamEvent::TextDelta("partial".to_owned())],
            stay_open: true,
        };
        let config = CompactionConfig {
            timeout_seconds: std::num::NonZeroU32::new(1),
            ..CompactionConfig::default()
        };
        let result = receive(&provider, request(), &config, None).await;
        let failure = result.failure.expect("an open stream must time out");
        assert!(failure.recovery.is_retry());
        assert!(failure.message.contains("1s deadline"));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_waiting_summary() {
        let provider = Events {
            events: Vec::new(),
            stay_open: true,
        };
        let signal = InterruptSignal::new();
        let config = CompactionConfig::default();
        let (response, ()) = tokio::join!(
            receive(&provider, request(), &config, Some(&signal)),
            async {
                tokio::task::yield_now().await;
                signal.fire();
            },
        );
        assert_eq!(
            response.failure.unwrap().reason,
            CompactionStopReason::Interrupted
        );
    }

    #[tokio::test]
    async fn the_summary_ceiling_counts_utf8_bytes() {
        let provider = Events {
            events: vec![
                StreamEvent::TextDelta("调研".to_owned()),
                StreamEvent::TextDelta("中".to_owned()),
                StreamEvent::MessageEnd { stop_reason: None },
            ],
            stay_open: false,
        };
        let config = CompactionConfig {
            max_summary_bytes: std::num::NonZeroU32::new(6),
            ..CompactionConfig::default()
        };
        let response = receive(&provider, request(), &config, None).await;
        assert_eq!(response.text, "调研");
        assert_eq!(
            response.failure.unwrap().reason,
            CompactionStopReason::OutputLimit
        );
    }

    #[tokio::test]
    async fn a_summary_cannot_smuggle_a_tool_call_into_a_normal_stop() {
        let provider = Events {
            events: vec![
                StreamEvent::TextDelta("summary".to_owned()),
                StreamEvent::ToolUseStart {
                    id: "call".to_owned(),
                    name: "shell".to_owned(),
                },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ],
            stay_open: false,
        };
        let response = receive(&provider, request(), &CompactionConfig::default(), None).await;
        assert_eq!(
            response.failure.unwrap().reason,
            CompactionStopReason::Provider
        );
    }

    #[tokio::test]
    async fn split_usage_frames_preserve_input_and_do_not_double_count_reasoning() {
        let provider = Events {
            events: vec![
                StreamEvent::TokenUsage {
                    input_tokens: Some(20),
                    output_tokens: None,
                    reasoning_tokens: None,
                    cache_read_input_tokens: Some(5),
                    cache_write_input_tokens: None,
                    accounting: PromptAccounting::CacheInsideInput,
                },
                StreamEvent::TextDelta("summary".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
                StreamEvent::TokenUsage {
                    input_tokens: None,
                    output_tokens: Some(12),
                    reasoning_tokens: Some(3),
                    cache_read_input_tokens: None,
                    cache_write_input_tokens: None,
                    accounting: PromptAccounting::CacheInsideInput,
                },
            ],
            stay_open: false,
        };
        let response = receive(&provider, request(), &CompactionConfig::default(), None).await;
        assert!(response.failure.is_none());
        assert_eq!(
            response.usage.unwrap().tokens(),
            json!({
                "input": 20, "output": 9, "reasoning": 3,
                "cache": { "read": 5, "write": 0 },
                "accounting": "cache-inside-input",
            })
        );
    }
}
