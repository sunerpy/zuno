//! Provider-sealed reasoning: one reader, one include key, one vocabulary.
//!
//! # What an endpoint that seals reasoning asks of a client
//!
//! An OpenAI Responses endpoint that supports sealed reasoning returns a
//! `reasoning` item carrying an opaque `encrypted_content` envelope, but only when
//! the request asked for it with `include: ["reasoning.encrypted_content"]`. Without
//! that entry the item arrives with a summary and no envelope, and there is nothing
//! a later request can replay: the reasoning is history, not context. That single
//! missing request field is the whole difference between a session that replays
//! reasoning across tool calls and one that never does.
//!
//! Replaying it back is equally exact. The envelope is sealed against the model that
//! minted it, so the item has to travel verbatim, in the position it was streamed,
//! and only to that same model. Endpoints reject a replay that reorders items, that
//! substitutes the plaintext summary for the envelope, or that presents an envelope
//! minted by a different model — and they reject it for the whole request, not just
//! for the offending item.
//!
//! # Why this is one option and not a provider identity
//!
//! Sealing is an endpoint capability. The official Responses API has it, a loopback
//! gateway in front of another vendor's models can have it, and an otherwise
//! identical OpenAI-compatible endpoint can lack it. A provider-id allowlist would
//! therefore be wrong in both directions. So the capability is declared in
//! configuration —
//! [`ProviderOptions::reasoning_replay`](zuno_config::schema::provider::ProviderOptions::reasoning_replay)
//! — and travels to the adapters inside [`Spec::options`](crate::registry::Spec::options).
//! [`ReasoningReplay::Off`] is the default, so an endpoint that never opts in is
//! never asked for an envelope and never sent one: no `include` entry, no sealed
//! `reasoning` item. That is narrower than "the same bytes as before", because the
//! Responses input of *every* provider is now ordered the way the model streamed it —
//! text, reasoning and tool calls interleaved rather than text last. Opting out of
//! this capability does not opt out of that.
//!
//! # Why the reader lives in the spine
//!
//! Two Responses adapters (`zuno-provider-openai`, `zuno-provider-compatible`) and
//! the engine all need the same answer, and the engine cannot depend on either
//! adapter. Each writing its own `options.get("reasoningReplay")` is the arrangement
//! that let the reasoning-effort defect survive: writer and readers agreed only by
//! coincidence. Here the config field, this reader, and
//! [`ENCRYPTED_REASONING_INCLUDE`] are the only spellings, so a drift is a compile
//! error rather than a capability that silently stops arriving.
//!
//! # Strict reading, and the one shape that is not an error
//!
//! A misspelled mode or a non-numeric age is [`InvalidReasoningReplayOption`], and
//! the adapter that reads it declines construction. Guessing [`ReasoningReplay::Off`]
//! would hand the user a working session that quietly never replays — the exact
//! defect this module exists to end. An explicit JSON `null` is the one accepted
//! stand-in for absent, because a config bag written by hand spells "unset" that way
//! (`examples/config/zuno-multi-provider.json` does it for `maxTokens`).
//!
//! Note that the encrypted/max-age *pairing* is a configuration rule, enforced once
//! where the whole document is visible
//! (`crates/zuno-config/src/schema/parse.rs`). A per-model options bag that turns
//! replay off under a provider that set a max age still constructs here.

use crate::event::RequestContentBlock;
use crate::registry::Spec;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::time::Duration;

pub use zuno_config::schema::provider::ReasoningReplay;

/// Option key selecting sealed-reasoning replay (`reasoningReplay`).
pub const REASONING_REPLAY_OPTION: &str = "reasoningReplay";
/// Option key bounding how old a replayed envelope may be (`reasoningReplayMaxAge`).
pub const REASONING_REPLAY_MAX_AGE_OPTION: &str = "reasoningReplayMaxAge";
/// The `include` entry that makes a Responses endpoint seal its reasoning.
pub const ENCRYPTED_REASONING_INCLUDE: &str = "reasoning.encrypted_content";

/// The request field [`ENCRYPTED_REASONING_INCLUDE`] belongs to.
const INCLUDE_FIELD: &str = "include";

/// A configuration value this module refuses to interpret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidReasoningReplayOption {
    /// `reasoningReplay` is not one of the modes Zuno implements.
    #[error(
        "provider option `{option}` must be \"off\" or \"encrypted\", got `{value}`; \
         a sealed-reasoning endpoint is declared, never guessed"
    )]
    Mode {
        /// The option key that was read.
        option: &'static str,
        /// The rejected value, as written.
        value: Value,
    },
    /// `reasoningReplayMaxAge` is not a positive whole number of milliseconds.
    #[error(
        "provider option `{option}` must be a positive whole number of milliseconds, got `{value}`"
    )]
    MaxAge {
        /// The option key that was read.
        option: &'static str,
        /// The rejected value, as written.
        value: Value,
    },
}

/// What one endpoint asks of Zuno about sealed reasoning.
///
/// `Copy` so an adapter's `Quirks` and the engine's per-step decision can both hold
/// it without a clone, and `Eq` so a test can assert the resolved policy directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReasoningReplayPolicy {
    /// Whether sealed reasoning is requested and replayed at all.
    pub mode: ReasoningReplay,
    /// Oldest envelope still replayed, when the endpoint expires them.
    ///
    /// `None` means Zuno does not age envelopes out on its own. An envelope that
    /// the endpoint has already expired is then refused on the wire instead of
    /// being dropped locally, which is the correct trade only when the endpoint
    /// does not expire them.
    pub max_age: Option<Duration>,
}

impl ReasoningReplayPolicy {
    /// The policy declared by one provider-option bag.
    ///
    /// # Errors
    ///
    /// [`InvalidReasoningReplayOption`] when either key is present with a shape this
    /// module cannot interpret. The caller declines construction rather than
    /// falling back to [`ReasoningReplay::Off`].
    pub fn from_options(
        options: &BTreeMap<String, Value>,
    ) -> Result<Self, InvalidReasoningReplayOption> {
        let mode = match options.get(REASONING_REPLAY_OPTION) {
            None | Some(Value::Null) => ReasoningReplay::default(),
            Some(value) => {
                serde_json::from_value::<ReasoningReplay>(value.clone()).map_err(|_| {
                    InvalidReasoningReplayOption::Mode {
                        option: REASONING_REPLAY_OPTION,
                        value: value.clone(),
                    }
                })?
            }
        };
        let max_age = match options.get(REASONING_REPLAY_MAX_AGE_OPTION) {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|millis| *millis > 0)
                    .map(Duration::from_millis)
                    .ok_or_else(|| InvalidReasoningReplayOption::MaxAge {
                        option: REASONING_REPLAY_MAX_AGE_OPTION,
                        value: value.clone(),
                    })?,
            ),
        };
        Ok(Self { mode, max_age })
    }

    /// The policy declared by one registry spec.
    ///
    /// # Errors
    ///
    /// As [`from_options`](Self::from_options).
    pub fn from_spec(spec: &Spec) -> Result<Self, InvalidReasoningReplayOption> {
        Self::from_options(&spec.options)
    }

    /// Whether requests must ask for, and replay, sealed reasoning.
    #[must_use]
    pub const fn requests_encrypted(self) -> bool {
        matches!(self.mode, ReasoningReplay::Encrypted)
    }

    /// The stable spelling used in durable events and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self.mode {
            ReasoningReplay::Off => "off",
            ReasoningReplay::Encrypted => "encrypted",
        }
    }

    /// Add [`ENCRYPTED_REASONING_INCLUDE`] to a Responses request body.
    ///
    /// A no-op unless [`requests_encrypted`](Self::requests_encrypted). Merges into
    /// an existing `include` array rather than replacing it, so a hand-written
    /// `include` keeps its other entries and cannot acquire a duplicate. A present
    /// `include` of any other JSON shape is overwritten: it could not have been sent
    /// as-is, and the entry that makes the endpoint seal its reasoning is the one
    /// part of the body a passthrough option may not remove.
    ///
    /// Chat Completions has no `include` field. Callers refuse the combination
    /// before reaching a body, so this is only ever called for Responses.
    pub fn insert_include(self, body: &mut Map<String, Value>) {
        if !self.requests_encrypted() {
            return;
        }
        let entry = Value::String(ENCRYPTED_REASONING_INCLUDE.to_owned());
        match body.get_mut(INCLUDE_FIELD) {
            Some(Value::Array(include)) => {
                if !include.iter().any(|item| item == &entry) {
                    include.push(entry);
                }
            }
            _ => {
                body.insert(INCLUDE_FIELD.to_owned(), Value::Array(vec![entry]));
            }
        }
    }
}

/// Whether output a sealed reasoning item can explain follows it in the same message.
///
/// `rest` is the blocks after the sealed item, in stream order.
///
/// A Responses endpoint validates the pairing positionally: the item has to sit
/// immediately before the output it produced. Sent without that output, OpenAI answers
/// `400 Item 'rs_...' of type 'reasoning' was provided without its required following
/// item` and kiro-provider answers `400 invalid_reasoning_replay`, because there is
/// nothing to fingerprint. Durable history reaches that shape honestly — a step
/// interrupted after the reasoning item, one that spent its whole output allowance on
/// reasoning, or one that failed mid-stream — and the item would then be replayed on
/// every later request to the same model, so each of those turns would fail for a
/// reason the user cannot see. The item stays in history and leaves the request.
///
/// # Why this lives beside the policy rather than in each adapter
///
/// Two Responses adapters decide with it and the engine counts with it. While each
/// adapter owned a private copy, the engine's durable
/// `replayedReasoningCapsules` counted envelopes an adapter then dropped, so the one
/// field an operator is told to trust over-reported replay for the rest of a session.
/// One function means the count and the wire cannot disagree.
#[must_use]
pub fn sealed_item_has_following_output(rest: &[RequestContentBlock]) -> bool {
    rest.iter().any(|block| match block {
        RequestContentBlock::Text { text } => !text.is_empty(),
        RequestContentBlock::ResourceLink { .. } | RequestContentBlock::ToolUse { .. } => true,
        RequestContentBlock::ProviderEncryptedReasoning { .. }
        | RequestContentBlock::SignedThinking { .. }
        | RequestContentBlock::ToolResult { .. }
        | RequestContentBlock::Image { .. }
        | RequestContentBlock::ImageAttachment { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options(value: Value) -> BTreeMap<String, Value> {
        value
            .as_object()
            .expect("object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    #[test]
    fn an_endpoint_that_says_nothing_is_off_and_asks_for_no_envelope() {
        let policy = ReasoningReplayPolicy::from_options(&options(json!({ "maxTokens": 1024 })))
            .expect("silence is a valid declaration");
        assert_eq!(policy, ReasoningReplayPolicy::default());
        assert!(!policy.requests_encrypted());
        assert_eq!(policy.as_str(), "off");

        let mut body = Map::new();
        policy.insert_include(&mut body);
        assert!(
            body.is_empty(),
            "an off policy must not add an `include` field: {body:?}"
        );
    }

    #[test]
    fn an_explicit_null_reads_as_unset_rather_than_as_a_bad_shape() {
        let policy = ReasoningReplayPolicy::from_options(&options(json!({
            "reasoningReplay": null,
            "reasoningReplayMaxAge": null
        })))
        .expect("a hand-written bag spells unset as null");
        assert_eq!(policy, ReasoningReplayPolicy::default());
    }

    #[test]
    fn an_encrypted_endpoint_asks_for_the_envelope_and_keeps_a_declared_age() {
        let policy = ReasoningReplayPolicy::from_options(&options(json!({
            "reasoningReplay": "encrypted",
            "reasoningReplayMaxAge": 86_400_000
        })))
        .expect("the declared capability parses");
        assert!(policy.requests_encrypted());
        assert_eq!(policy.as_str(), "encrypted");
        assert_eq!(policy.max_age, Some(Duration::from_secs(86_400)));

        let mut body = Map::new();
        policy.insert_include(&mut body);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn a_misspelled_mode_is_refused_instead_of_read_as_off() {
        let error = ReasoningReplayPolicy::from_options(&options(json!({
            "reasoningReplay": "encrypted_content"
        })))
        .expect_err("a near-miss must not degrade to a session that never replays");
        assert!(
            matches!(error, InvalidReasoningReplayOption::Mode { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("encrypted"), "{error}");

        for bad_age in [json!(0), json!(-1), json!("24h"), json!(1.5)] {
            let error = ReasoningReplayPolicy::from_options(&options(json!({
                "reasoningReplay": "encrypted",
                "reasoningReplayMaxAge": bad_age
            })))
            .expect_err("a max age Zuno cannot compare against is a configuration error");
            assert!(
                matches!(error, InvalidReasoningReplayOption::MaxAge { .. }),
                "{bad_age} produced {error:?}"
            );
        }
    }

    #[test]
    fn a_model_level_off_under_a_provider_max_age_still_constructs() {
        let policy = ReasoningReplayPolicy::from_options(&options(json!({
            "reasoningReplay": "off",
            "reasoningReplayMaxAge": 86_400_000
        })))
        .expect("the pairing rule belongs to config validation, not to this reader");
        assert!(!policy.requests_encrypted());
        assert_eq!(policy.max_age, Some(Duration::from_secs(86_400)));
    }

    #[test]
    fn the_include_entry_merges_and_never_duplicates() {
        let policy = ReasoningReplayPolicy {
            mode: ReasoningReplay::Encrypted,
            max_age: None,
        };

        let mut body = Map::new();
        body.insert("include".to_owned(), json!(["file_search_call.results"]));
        policy.insert_include(&mut body);
        assert_eq!(
            body["include"],
            json!(["file_search_call.results", "reasoning.encrypted_content"]),
            "an author's other include entries survive"
        );
        policy.insert_include(&mut body);
        assert_eq!(
            body["include"],
            json!(["file_search_call.results", "reasoning.encrypted_content"]),
            "a second pass must not duplicate the entry"
        );

        let mut malformed = Map::new();
        malformed.insert("include".to_owned(), json!("reasoning"));
        policy.insert_include(&mut malformed);
        assert_eq!(
            malformed["include"],
            json!(["reasoning.encrypted_content"]),
            "an include of the wrong shape is replaced by the one entry that must be sent"
        );
    }

    #[test]
    fn a_spec_declares_the_policy_the_adapters_read() {
        let mut spec = Spec::new("kiro-local");
        spec.options.insert(
            REASONING_REPLAY_OPTION.to_owned(),
            Value::String("encrypted".to_owned()),
        );
        let policy = ReasoningReplayPolicy::from_spec(&spec).expect("spec options parse");
        assert!(policy.requests_encrypted());
    }
}
