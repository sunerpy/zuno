//! The two provider ids whose surface is chosen by a rule, and nothing else.
//!
//! Every other id this profile claims has a fixed surface that travels on the
//! [`Spec`]. Azure and GitHub Copilot do not, and their rules are genuinely
//! different from each other, so both live here — in one file, named after what
//! they are — rather than as branches inside the request builder. `request.rs`
//! receives a resolved [`ApiSurface`] and never learns which provider produced it.
//!
//! # What the oracle actually does
//!
//! **Azure** (`packages/opencode/src/provider/provider.ts:154-160`):
//!
//! ```text
//! function selectAzureLanguageModel(sdk, modelID, useChat) {
//!   if (useChat && sdk.chat) return sdk.chat(modelID)
//!   if (sdk.responses)       return sdk.responses(modelID)
//!   if (sdk.messages)        return sdk.messages(modelID)
//!   if (sdk.chat)            return sdk.chat(modelID)
//!   return sdk.languageModel(modelID)
//! }
//! ```
//!
//! called with `useChat = Boolean(options?.["useCompletionUrls"])` from both
//! `azure` (`:265`) and `azure-cognitive-services` (`:285`).
//!
//! **`modelID` is passed in and never read.** Azure's rule is a surface-*
//! availability walk gated by one provider option; it is not a model-id rule, and
//! implementing it as one would invent behaviour the oracle does not have. See
//! [`azure_surface`].
//!
//! **GitHub Copilot** (`:225-239`) *is* a model-id rule:
//!
//! ```text
//! if (sdk.responses === undefined && sdk.chat === undefined) return sdk.languageModel(modelID)
//! if (model && "endpoint" in model.api) {
//!   if (model.api.endpoint === "responses" && sdk.responses) return sdk.responses(modelID)
//!   if (model.api.endpoint === "chat" && sdk.chat)           return sdk.chat(modelID)
//! }
//! const match = /^gpt-(\d+)/.exec(modelID)
//! if (match && Number(match[1]) >= 5 && !modelID.startsWith("gpt-5-mini")) return sdk.responses(modelID)
//! return sdk.chat(modelID)
//! ```
//!
//! See [`copilot_surface`].

use zuno_llm::registry::{ApiSurface, Spec};

use crate::family::{Profile, SurfaceRule};

/// The `provider.*.options` key that gates Azure's walk.
///
/// Spelled exactly as the oracle spells it (`provider.ts:265`), so config
/// *content* authored for opencode keeps working once it is under Zuno's
/// filename.
pub const USE_COMPLETION_URLS_OPTION: &str = "useCompletionUrls";

/// The `provider.*.options` key declaring which surfaces an endpoint exposes.
///
/// An array of `"chat"`, `"responses"`, `"messages"`. A Rust provider cannot
/// introspect a remote endpoint, so this is declared configuration with a
/// documented default per rule.
pub const SURFACES_OPTION: &str = "surfaces";

/// The `provider.*.options` key declaring a model's own endpoint.
///
/// Copilot's rule consults `model.api.endpoint` before its version check
/// (`provider.ts:229-232`). That value belongs to the *model*, so it arrives as a
/// map from model id to `"chat"` or `"responses"`.
pub const MODEL_ENDPOINTS_OPTION: &str = "modelEndpoints";

/// Which surfaces an endpoint exposes.
///
/// The three the Azure walk tests, in the order it tests them. `Default` is not
/// a member because `languageModel` is the fallback rather than a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceSupport {
    /// `/chat/completions` is available.
    pub chat: bool,
    /// `/responses` is available.
    pub responses: bool,
    /// `/messages` is available.
    pub messages: bool,
}

impl SurfaceSupport {
    /// Chat-completions only: what a bare OpenAI-compatible endpoint offers.
    #[must_use]
    pub const fn chat_only() -> Self {
        Self {
            chat: true,
            responses: false,
            messages: false,
        }
    }

    /// Chat and responses: what the Azure and Copilot SDKs both expose.
    ///
    /// Evidenced by the oracle calling `sdk.chat` *and* `sdk.responses` on each
    /// without a guard beyond presence.
    #[must_use]
    pub const fn chat_and_responses() -> Self {
        Self {
            chat: true,
            responses: true,
            messages: false,
        }
    }

    /// Nothing but the SDK default.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            chat: false,
            responses: false,
            messages: false,
        }
    }

    /// The documented default for a surface rule.
    #[must_use]
    pub const fn for_rule(rule: SurfaceRule) -> Self {
        match rule {
            SurfaceRule::Fixed(_) => Self::chat_only(),
            SurfaceRule::Azure | SurfaceRule::Copilot => Self::chat_and_responses(),
        }
    }

    /// Read declared support from a spec, defaulting per rule.
    ///
    /// A malformed or absent [`SURFACES_OPTION`] keeps the default rather than
    /// erroring: the option narrows a capability set, and a user who mistypes it
    /// should get the documented behaviour, not a dead provider.
    #[must_use]
    pub fn from_spec(spec: &Spec, rule: SurfaceRule) -> Self {
        let Some(values) = spec
            .options
            .get(SURFACES_OPTION)
            .and_then(serde_json::Value::as_array)
        else {
            return Self::for_rule(rule);
        };
        let mut support = Self::none();
        for value in values {
            match value.as_str() {
                Some("chat") => support.chat = true,
                Some("responses") => support.responses = true,
                Some("messages") => support.messages = true,
                _ => {}
            }
        }
        support
    }
}

/// Azure's surface choice, ported from `provider.ts:154-160`.
///
/// The `model_id` parameter is deliberately absent. The oracle accepts one and
/// never reads it, so taking one here would imply a dependency this rule does not
/// have — and a test could then "confirm" a per-model behaviour that Azure does
/// not exhibit.
///
/// The `use_completion_urls` gate is the only reason `chat` can win ahead of
/// `responses`; without it the walk always prefers `responses`.
#[must_use]
pub fn azure_surface(support: SurfaceSupport, use_completion_urls: bool) -> ApiSurface {
    if use_completion_urls && support.chat {
        return ApiSurface::Chat;
    }
    if support.responses {
        return ApiSurface::Responses;
    }
    if support.messages {
        return ApiSurface::Messages;
    }
    if support.chat {
        return ApiSurface::Chat;
    }
    ApiSurface::Default
}

/// GitHub Copilot's surface choice, ported from `provider.ts:225-239`.
///
/// `declared` is the model's own `api.endpoint`, which the oracle consults first.
///
/// # One deliberate divergence
///
/// After the declared-endpoint block the oracle calls `sdk.responses(modelID)` or
/// `sdk.chat(modelID)` without re-checking presence. With only `chat` available
/// and a model id of `gpt-5`, that dereferences `undefined` and throws a
/// `TypeError`. Here the unavailable surface falls back to the available one. No
/// configuration the first guard admits — which requires at least one of the two
/// — can observe a different *successful* answer; only the crash is removed.
#[must_use]
pub fn copilot_surface(
    model_id: &str,
    declared: Option<ApiSurface>,
    support: SurfaceSupport,
) -> ApiSurface {
    if !support.responses && !support.chat {
        return ApiSurface::Default;
    }
    match declared {
        Some(ApiSurface::Responses) if support.responses => return ApiSurface::Responses,
        Some(ApiSurface::Chat) if support.chat => return ApiSurface::Chat,
        _ => {}
    }
    if prefers_responses(model_id) && support.responses {
        return ApiSurface::Responses;
    }
    if support.chat {
        ApiSurface::Chat
    } else {
        ApiSurface::Responses
    }
}

/// The `/^gpt-(\d+)/` version check, without a regex dependency.
///
/// This is the profile's **only** model-id rule, and it is a version comparison
/// rather than a list of names — which is why it does not reintroduce the
/// hard-coded model ids that `zuno-llm`'s
/// `policy_sources_contain_no_model_id_literals` test exists to prevent. The one
/// literal, `gpt-5-mini`, is the oracle's own explicit exclusion; a version
/// comparison cannot express it.
fn prefers_responses(model_id: &str) -> bool {
    const MINI_EXCLUSION: &str = "gpt-5-mini";

    let Some(rest) = model_id.strip_prefix("gpt-") else {
        return false;
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return false;
    }
    // A version longer than a `u32` can hold is not a real model id; treating an
    // overflow as "not a match" keeps the function total without a panic path.
    let Ok(major) = digits.parse::<u32>() else {
        return false;
    };
    major >= 5 && !model_id.starts_with(MINI_EXCLUSION)
}

/// The model's own declared endpoint, from the spec's model-endpoint map.
#[must_use]
pub fn declared_endpoint(spec: &Spec, model_id: &str) -> Option<ApiSurface> {
    match spec
        .options
        .get(MODEL_ENDPOINTS_OPTION)?
        .get(model_id)?
        .as_str()?
    {
        "chat" => Some(ApiSurface::Chat),
        "responses" => Some(ApiSurface::Responses),
        "messages" => Some(ApiSurface::Messages),
        _ => None,
    }
}

/// The one entry point: which surface this request uses.
///
/// Precedence, highest first:
///
/// 1. The request's own [`ApiSurface`], when it is not
///    [`Default`](ApiSurface::Default). The trait documents per-request routing as
///    travelling here, and a caller that pinned a surface means it.
/// 2. The spec's surface, when the composition root fixed one.
/// 3. The provider id's rule: a fixed surface, Azure's walk, or Copilot's check.
///
/// A resolved [`ApiSurface::Default`] means chat-completions on the wire; see
/// [`endpoint_path`].
#[must_use]
pub fn resolve_surface(
    profile: Profile,
    spec: &Spec,
    request_surface: ApiSurface,
    model_id: &str,
) -> ApiSurface {
    if request_surface != ApiSurface::Default {
        return request_surface;
    }
    if let SurfaceRule::Fixed(fixed) = profile.surface {
        if fixed != ApiSurface::Default {
            return fixed;
        }
        return spec.surface;
    }
    if spec.surface != ApiSurface::Default {
        return spec.surface;
    }
    let support = SurfaceSupport::from_spec(spec, profile.surface);
    match profile.surface {
        SurfaceRule::Azure => azure_surface(support, use_completion_urls(spec)),
        SurfaceRule::Copilot => {
            copilot_surface(model_id, declared_endpoint(spec, model_id), support)
        }
        SurfaceRule::Fixed(fixed) => fixed,
    }
}

/// Whether the spec set Azure's `useCompletionUrls` gate.
///
/// `Boolean(options?.["useCompletionUrls"])` in the oracle, so a non-boolean
/// truthy value counts. JSON has no `truthy`, so this accepts a real `true` and a
/// non-empty string, which is what a hand-written config produces.
#[must_use]
pub fn use_completion_urls(spec: &Spec) -> bool {
    match spec.options.get(USE_COMPLETION_URLS_OPTION) {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::String(text)) => !text.is_empty() && text != "false",
        _ => false,
    }
}

/// The request path a surface maps to on an OpenAI-compatible endpoint.
///
/// [`ApiSurface::Default`] is chat-completions: the oracle's `languageModel` on
/// `@ai-sdk/openai-compatible` is the chat-completions model.
#[must_use]
pub const fn endpoint_path(surface: ApiSurface) -> &'static str {
    match surface {
        ApiSurface::Default | ApiSurface::Chat => "/chat/completions",
        ApiSurface::Responses => "/responses",
        ApiSurface::Messages => "/messages",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn azure_prefers_responses_unless_completion_urls_are_requested() {
        let both = SurfaceSupport::chat_and_responses();
        assert_eq!(azure_surface(both, false), ApiSurface::Responses);
        assert_eq!(azure_surface(both, true), ApiSurface::Chat);
    }

    #[test]
    fn azure_walks_down_to_messages_then_chat_then_the_sdk_default() {
        let messages_only = SurfaceSupport {
            chat: false,
            responses: false,
            messages: true,
        };
        assert_eq!(azure_surface(messages_only, false), ApiSurface::Messages);
        assert_eq!(azure_surface(messages_only, true), ApiSurface::Messages);

        let chat_only = SurfaceSupport::chat_only();
        assert_eq!(azure_surface(chat_only, false), ApiSurface::Chat);

        assert_eq!(
            azure_surface(SurfaceSupport::none(), false),
            ApiSurface::Default
        );
    }

    #[test]
    fn the_copilot_version_check_matches_the_oracle_regex() {
        for id in ["gpt-5", "gpt-5.5", "gpt-6", "gpt-41"] {
            assert!(prefers_responses(id), "{id} should prefer responses");
        }
        for id in [
            "gpt-4o",
            "gpt-4.1",
            "gpt-5-mini",
            "gpt-5-mini-2025",
            "gpt-oss-20b",
            "claude-sonnet-4.5",
            "o3-mini",
            "",
        ] {
            assert!(!prefers_responses(id), "{id} should stay on chat");
        }
    }

    #[test]
    fn a_declared_model_endpoint_beats_the_version_check() {
        let support = SurfaceSupport::chat_and_responses();
        assert_eq!(
            copilot_surface("gpt-5", Some(ApiSurface::Chat), support),
            ApiSurface::Chat
        );
        assert_eq!(
            copilot_surface("gpt-4o", Some(ApiSurface::Responses), support),
            ApiSurface::Responses
        );
    }

    #[test]
    fn copilot_falls_back_to_the_sdk_default_when_neither_surface_exists() {
        assert_eq!(
            copilot_surface("gpt-5", None, SurfaceSupport::none()),
            ApiSurface::Default
        );
    }

    #[test]
    fn surfaces_declared_in_a_spec_narrow_the_default() {
        let spec = Spec::new("azure").with_option(SURFACES_OPTION, json!(["chat"]));
        assert_eq!(
            SurfaceSupport::from_spec(&spec, SurfaceRule::Azure),
            SurfaceSupport::chat_only()
        );
    }

    #[test]
    fn a_malformed_surfaces_option_keeps_the_documented_default() {
        let spec = Spec::new("azure").with_option(SURFACES_OPTION, json!("chat"));
        assert_eq!(
            SurfaceSupport::from_spec(&spec, SurfaceRule::Azure),
            SurfaceSupport::chat_and_responses()
        );
    }

    #[test]
    fn each_surface_maps_to_one_path() {
        assert_eq!(endpoint_path(ApiSurface::Default), "/chat/completions");
        assert_eq!(endpoint_path(ApiSurface::Chat), "/chat/completions");
        assert_eq!(endpoint_path(ApiSurface::Responses), "/responses");
        assert_eq!(endpoint_path(ApiSurface::Messages), "/messages");
    }
}
