//! The per-provider quirk table: one place, keyed by capability where possible.
//!
//! # Why a table rather than branches
//!
//! Both reference implementations express these quirks as conditionals scattered
//! through the request builder, each testing a model name. The cost is not
//! duplication, it is that no single place answers "what is different about this
//! provider" — so the next quirk is added wherever the author happened to be, and
//! the set drifts. `zuno-llm` already enforces the opposite discipline for effort
//! and cache policy with a test named
//! `policy_sources_contain_no_model_id_literals`, and this module keeps to it:
//!
//! - **Sampling-param stripping keys off a declared capability**, not a model
//!   name. Reasoning models that reject `temperature` and `top_p` are described by
//!   [`Capabilities::sampling_params`] being `false`, which the catalog supplies
//!   per model. The reference implementation instead maintained a growing
//!   `is_reasoning_model()` prefix list
//!   (`claw-code`)
//!   that had to be edited for every new reasoning model.
//! - **The one genuine model-id rule lives in [`MODEL_PROTOCOL_RULES`]**, a single
//!   named table with its citation, extensible from config so a user meeting a new
//!   model does not wait for a release.
//!
//! # The reasoning-content protocol
//!
//! Some OpenAI-compatible vendors put reasoning in a non-standard
//! `delta.reasoning_content` (Cloudflare Workers AI, DeepSeek, GLM — verified in
//! the recorded corpus, see `crate::stream`). Reading it is unconditional and
//! harmless: a provider that never sends it costs nothing.
//!
//! **Echoing it back is not.** A subset of models require prior assistant
//! reasoning replayed as `reasoning_content` on the next turn
//! (`openai_compat.rs:977`, `:1287-1300`), and the same models require a
//! `thinking: {"type": "enabled"}` opt-in. For every other model that echo is
//! pure token cost, and a vendor that never sent the field may reject it. So the
//! echo is gated on [`Quirks::reasoning_protocol`], and nothing else in the crate
//! decides it.

use zuno_llm::registry::{ApiSurface, Capabilities, Spec};

use crate::family::Profile;
use crate::surface::resolve_surface;

/// The `provider.*.options` key forcing the reasoning-content protocol on or off.
///
/// A boolean. Set it when a vendor ships a model the table does not know yet, or
/// to switch the protocol off for a model the table matches too eagerly.
pub const REASONING_CONTENT_OPTION: &str = "reasoningContent";

/// The `provider.*.options` key extending [`MODEL_PROTOCOL_RULES`].
///
/// An array of canonical model-id prefixes, matched exactly as the built-in table
/// is matched.
pub const REASONING_CONTENT_MODELS_OPTION: &str = "reasoningContentModels";

/// A model-id rule, matched against the canonical id.
///
/// One field today, but a struct rather than a bare `&str` so a second protocol
/// requirement lands as a column here instead of as a second parallel list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelRule {
    /// Canonical-id prefix this rule matches.
    pub prefix: &'static str,
    /// The model requires reasoning echoed back as `reasoning_content`, and
    /// requires the `thinking` opt-in that goes with it.
    pub reasoning_protocol: bool,
}

/// Every model-id rule this profile applies. Exactly one row.
///
/// `deepseek-v4` Pro and Flash require prior assistant reasoning replayed as
/// `reasoning_content` and a `thinking: {"type":"enabled"}` opt-in
/// (`claw-code`,
/// `:1226-1227`, `:1287-1300`).
///
/// Nothing else in this crate matches a model id. If a row is added here, that is
/// the review boundary.
pub const MODEL_PROTOCOL_RULES: &[ModelRule] = &[ModelRule {
    prefix: "deepseek-v4",
    reasoning_protocol: true,
}];

/// What is different about this request, resolved once.
///
/// Built before the body is assembled so `request.rs` reads flags rather than
/// re-deriving them, and so a test can assert the decision without building a
/// request at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quirks {
    /// The resolved API surface. See [`crate::surface::resolve_surface`].
    pub surface: ApiSurface,
    /// Capabilities as the catalog declared them for this model.
    pub capabilities: Capabilities,
    /// This model requires the `reasoning_content` echo and `thinking` opt-in.
    pub reasoning_protocol: bool,
    /// This vendor reports the upstream it routed to.
    pub routes_upstreams: bool,
}

impl Quirks {
    /// Resolve every quirk for one request.
    #[must_use]
    pub fn resolve(
        profile: Profile,
        spec: &Spec,
        capabilities: Capabilities,
        model_id: &str,
        request_surface: ApiSurface,
    ) -> Self {
        Self {
            surface: resolve_surface(profile, spec, request_surface, model_id),
            capabilities,
            reasoning_protocol: reasoning_protocol(spec, model_id),
            routes_upstreams: profile.routes_upstreams,
        }
    }

    /// Whether `temperature`, `top_p` and friends may be sent.
    ///
    /// A capability read, never a model-name test. This is the whole point of
    /// [`Capabilities::sampling_params`] existing.
    #[must_use]
    pub const fn accepts_sampling_params(&self) -> bool {
        self.capabilities.sampling_params
    }

    /// Whether tool definitions may be sent.
    #[must_use]
    pub const fn accepts_tools(&self) -> bool {
        self.capabilities.tool_calls
    }

    /// Whether image parts may be sent, rather than dropped.
    #[must_use]
    pub const fn accepts_attachments(&self) -> bool {
        self.capabilities.attachments
    }
}

/// Whether this model requires the reasoning-content protocol.
///
/// Precedence: an explicit spec boolean, then the spec's extension list, then
/// [`MODEL_PROTOCOL_RULES`]. An explicit `false` therefore switches the protocol
/// off even for a model the table matches, which is the escape hatch a user needs
/// when a vendor changes behaviour mid-release.
#[must_use]
pub fn reasoning_protocol(spec: &Spec, model_id: &str) -> bool {
    if let Some(serde_json::Value::Bool(flag)) = spec.options.get(REASONING_CONTENT_OPTION) {
        return *flag;
    }
    let canonical = canonical_model_id(model_id);
    if let Some(values) = spec
        .options
        .get(REASONING_CONTENT_MODELS_OPTION)
        .and_then(serde_json::Value::as_array)
    {
        let matched = values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|prefix| canonical.starts_with(&prefix.to_ascii_lowercase()));
        if matched {
            return true;
        }
    }
    MODEL_PROTOCOL_RULES
        .iter()
        .any(|rule| rule.reasoning_protocol && canonical.starts_with(rule.prefix))
}

/// Strip a routing prefix and case from a model id.
///
/// A router names the same model `deepseek/deepseek-v4-pro`; the vendor names it
/// `deepseek-v4-pro`. Both must match one rule, so the prefix is dropped before
/// comparison — the same normalization the reference implementation performs
/// (`openai_compat.rs:974-976`).
#[must_use]
pub fn canonical_model_id(model_id: &str) -> String {
    let lowered = model_id.to_ascii_lowercase();
    lowered
        .rsplit('/')
        .next()
        .unwrap_or(lowered.as_str())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caps(sampling: bool) -> Capabilities {
        Capabilities {
            reasoning: true,
            tool_calls: true,
            prompt_cache: false,
            attachments: false,
            sampling_params: sampling,
        }
    }

    #[test]
    fn a_routing_prefix_does_not_hide_a_model_rule() {
        let spec = Spec::new("openrouter");
        assert!(reasoning_protocol(&spec, "deepseek-v4-pro"));
        assert!(reasoning_protocol(&spec, "deepseek/deepseek-v4-flash"));
        assert!(reasoning_protocol(&spec, "DeepSeek/DeepSeek-V4-Pro"));
    }

    #[test]
    fn a_model_outside_the_table_does_not_get_the_protocol() {
        let spec = Spec::new("deepseek");
        for model in [
            "deepseek-chat",
            "deepseek-v3",
            "gpt-4o",
            "llama-3.3-70b",
            "@cf/openai/gpt-oss-20b",
        ] {
            assert!(!reasoning_protocol(&spec, model), "{model}");
        }
    }

    #[test]
    fn config_can_add_a_model_the_table_does_not_know() {
        let spec =
            Spec::new("deepseek").with_option(REASONING_CONTENT_MODELS_OPTION, json!(["glm-5"]));
        assert!(reasoning_protocol(&spec, "glm-5-air"));
        assert!(!reasoning_protocol(&spec, "glm-4"));
    }

    #[test]
    fn an_explicit_false_overrides_the_table() {
        let spec = Spec::new("deepseek").with_option(REASONING_CONTENT_OPTION, json!(false));
        assert!(!reasoning_protocol(&spec, "deepseek-v4-pro"));
    }

    #[test]
    fn sampling_support_comes_from_capabilities_not_from_the_model_name() {
        let profile = crate::family::claimed("groq").expect("groq is claimed");
        let spec = Spec::new("groq");

        let permissive = Quirks::resolve(profile, &spec, caps(true), "o3", ApiSurface::Default);
        assert!(permissive.accepts_sampling_params());

        let strict = Quirks::resolve(
            profile,
            &spec,
            caps(false),
            "llama-3.3-70b",
            ApiSurface::Default,
        );
        assert!(!strict.accepts_sampling_params());
    }
}
