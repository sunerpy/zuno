//! Learning requests retain partial usage, including discarded provider attempts.
use serde::{Deserialize, Serialize};
use zuno_llm::event::{PromptAccounting, StreamEvent};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: u64,
    /// False means observed lower bounds, never a free request.
    pub accounted: bool,
    pub provider_attempts: u32,
}

impl LearningUsage {
    fn add(self, next: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(next.input_tokens),
            output_tokens: self.output_tokens.saturating_add(next.output_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(next.cache_read_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_add(next.cache_write_input_tokens),
            reasoning_tokens: self
                .reasoning_tokens
                .zip(next.reasoning_tokens)
                .map(|(a, b)| a.saturating_add(b)),
            total_tokens: self.total_tokens.saturating_add(next.total_tokens),
            accounted: self.accounted && next.accounted,
            provider_attempts: self
                .provider_attempts
                .saturating_add(next.provider_attempts),
        }
    }
}

#[derive(Default)]
struct Reading {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
    accounting: Option<PromptAccounting>,
    ended: bool,
    inconsistent: bool,
}
impl Reading {
    fn snapshot(&self) -> LearningUsage {
        let read = self.cache_read.unwrap_or_default();
        let write = self.cache_write.unwrap_or_default();
        let input = self.input.unwrap_or_default();
        let input = self
            .accounting
            .map_or(input, |a| a.uncached_input(input, read, write));
        let output = self.output.unwrap_or_default();
        LearningUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: read,
            cache_write_input_tokens: write,
            reasoning_tokens: self.reasoning,
            total_tokens: input
                .saturating_add(output)
                .saturating_add(read)
                .saturating_add(write),
            accounted: self.input.is_some()
                && self.output.is_some()
                && self.accounting.is_some()
                && self.ended
                && !self.inconsistent
                && self.reasoning.is_none_or(|value| value <= output),
            provider_attempts: 1,
        }
    }
}

#[derive(Default)]
pub(crate) struct UsageTracker {
    retired: Option<LearningUsage>,
    current: Reading,
}
impl UsageTracker {
    pub(crate) fn observe(&mut self, event: &StreamEvent) -> crate::Result<()> {
        match event {
            StreamEvent::TokenUsage {
                input_tokens,
                output_tokens,
                reasoning_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
                accounting,
            } => {
                if self
                    .current
                    .accounting
                    .is_some_and(|previous| previous != *accounting)
                {
                    self.current.inconsistent = true;
                    return Err(crate::model::invalid(
                        "learning usage changed accounting within one provider attempt",
                    ));
                }
                self.current.accounting = Some(*accounting);
                for (slot, value) in [
                    (&mut self.current.input, input_tokens),
                    (&mut self.current.output, output_tokens),
                    (&mut self.current.reasoning, reasoning_tokens),
                    (&mut self.current.cache_read, cache_read_input_tokens),
                    (&mut self.current.cache_write, cache_write_input_tokens),
                ] {
                    if value.is_some() {
                        *slot = *value;
                    }
                }
            }
            StreamEvent::RetryRollback { .. } => {
                let current = self.current.snapshot();
                self.retired = Some(match self.retired.take() {
                    Some(prior) => prior.add(current),
                    None => current,
                });
                self.current = Reading::default();
            }
            StreamEvent::MessageEnd { .. } => self.current.ended = true,
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> LearningUsage {
        let current = self.current.snapshot();
        match self.retired.clone() {
            Some(prior) => prior.add(current),
            None => current,
        }
    }
}
