//! Which provider ids this profile claims, and which family carries the rest.
//!
//! # Why a table and not a fallthrough
//!
//! Both reference implementations treat "OpenAI-compatible" as the default: an
//! unrecognized provider id gets the chat-completions request builder and fails
//! at the first response, with an error that describes JSON rather than
//! configuration. That is the specific failure this module exists to prevent.
//! Bedrock frames its stream as a binary EventStream, Google speaks
//! `contents`/`parts`, Anthropic types every SSE event — none of them produces a
//! `choices[].delta`, so routing any of them here yields a deserialization error
//! that names nothing the user can act on.
//!
//! So classification is a closed table. An id this profile does not claim is
//! refused, and the refusal names the crate that does carry it.
//!
//! # The escape hatch, and why it is explicit
//!
//! Users legitimately point this profile at endpoints nobody has enumerated — a
//! local llama.cpp server, a corporate gateway. The oracle admits those the same
//! way: an arbitrary provider reaches `createOpenAICompatible` only because its
//! catalog entry *declares* `npm: "@ai-sdk/openai-compatible"`
//! (`packages/opencode/src/provider/provider.ts:108`, and the declaration is read
//! at `:1485`, `:1198`). This module mirrors that: an unclaimed id is accepted
//! when — and only when — the spec carries that declaration, via
//! [`Spec::options`](zuno_llm::registry::Spec::options) key [`NPM_OPTION`]. Silence
//! is never taken as consent.

use std::fmt;

use zuno_llm::registry::{ApiSurface, Spec};

/// The `provider.*.options` key that declares an unlisted id compatible.
///
/// Named after the oracle's own catalog field so a user copying a models.dev
/// entry does not have to translate it.
pub const NPM_OPTION: &str = "npm";

/// The value of [`NPM_OPTION`] this profile accepts as an opt-in.
pub const OPENAI_COMPATIBLE_NPM: &str = "@ai-sdk/openai-compatible";

/// A provider family, as the workspace splits provider crates.
///
/// Present so a refusal can name a destination rather than merely decline. Each
/// non-compatible variant knows its crate and its todo, because the person
/// reading the message is deciding where to configure the provider instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// Genuinely OpenAI-compatible on the wire: this crate.
    Compatible,
    /// Anthropic Messages, with typed SSE events and its own content blocks.
    Anthropic,
    /// AWS Bedrock: SigV4-signed requests and binary EventStream framing.
    Bedrock,
    /// Google Generative AI and Vertex, including the Vertex-Anthropic path.
    Google,
    /// OpenAI's own SDK surfaces, which the oracle pins to `responses`.
    OpenAi,
}

impl Family {
    /// Every family, for exhaustive tests and diagnostics.
    pub const ALL: [Self; 5] = [
        Self::Compatible,
        Self::Anthropic,
        Self::Bedrock,
        Self::Google,
        Self::OpenAi,
    ];

    /// The workspace crate that implements this family.
    #[must_use]
    pub const fn crate_name(self) -> &'static str {
        match self {
            Self::Compatible => "zuno-provider-compatible",
            Self::Anthropic => "zuno-provider-anthropic",
            Self::Bedrock => "zuno-provider-bedrock",
            Self::Google => "zuno-provider-google",
            Self::OpenAi => "zuno-provider-openai",
        }
    }

    /// Why this family cannot be served by an OpenAI-compatible request builder.
    ///
    /// Rendered into the refusal so the message explains the decision instead of
    /// asserting it.
    #[must_use]
    pub const fn wire_difference(self) -> &'static str {
        match self {
            Self::Compatible => "it is OpenAI-compatible on the wire",
            Self::Anthropic => {
                "it speaks Anthropic Messages, whose SSE events are typed and \
                 whose content is blocks rather than `choices[].delta`"
            }
            Self::Bedrock => {
                "it speaks Bedrock's invoke-with-response-stream API over \
                 SigV4-signed requests, framed as binary EventStream with \
                 preludes, headers and CRCs rather than SSE"
            }
            Self::Google => {
                "it speaks Gemini's `contents`/`parts` request shape and returns \
                 `candidates` rather than `choices[].delta`"
            }
            Self::OpenAi => {
                "it is pinned to OpenAI's Responses surface, whose typed \
                 `response.*` SSE events are not chat-completions chunks"
            }
        }
    }
}

impl fmt::Display for Family {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Compatible => "openai-compatible",
            Self::Anthropic => "anthropic",
            Self::Bedrock => "bedrock",
            Self::Google => "google",
            Self::OpenAi => "openai",
        };
        formatter.write_str(name)
    }
}

/// How a provider id chooses its API surface.
///
/// Two of the ids this profile claims do not have a fixed surface, and the rule
/// each uses is genuinely different. Making that a variant keeps the choice out
/// of the request builder: `request.rs` reads a resolved [`ApiSurface`] and never
/// asks which provider produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRule {
    /// One surface for every model, fixed at construction.
    ///
    /// Almost every compatible vendor. The oracle's `xai` and `meta` loaders pin
    /// `responses` this way (`provider.ts:212-224`); everything else takes the
    /// SDK default, which is chat-completions.
    Fixed(ApiSurface),
    /// Azure's availability walk, gated by the `useCompletionUrls` option.
    ///
    /// See [`crate::surface::azure_surface`]. Notably **not** model-id keyed.
    Azure,
    /// GitHub Copilot's per-model-id endpoint choice.
    ///
    /// See [`crate::surface::copilot_surface`].
    Copilot,
}

/// One row of the claim table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// The provider id as the catalog names it.
    pub provider: &'static str,
    /// How this id picks its surface.
    pub surface: SurfaceRule,
    /// Whether this vendor is a router that reports its chosen upstream.
    ///
    /// OpenRouter and the Vercel AI Gateway put the resolved upstream in a
    /// top-level `provider` field on each chunk; forwarding it as
    /// [`UpstreamProvider`](zuno_llm::registry::StreamEvent::UpstreamProvider)
    /// lets a session show which model actually answered.
    pub routes_upstreams: bool,
}

/// Every provider id this profile claims, with its surface rule.
///
/// The list is the plan's, cross-checked against the oracle's bundled-SDK map
/// (`packages/opencode/src/provider/provider.ts:107-134`) and its custom loaders
/// (`:212-239`). Sorted so a diagnostic that prints it is stable.
///
/// `azure-cognitive-services` is here because the oracle gives it the *same*
/// selector as `azure` (`:265` and `:285` both call `selectAzureLanguageModel`),
/// differing only in how the base URL is assembled — which is spec data, not a
/// branch in the request builder.
pub const CLAIMED: &[Profile] = &[
    row("alibaba"),
    Profile {
        provider: "azure",
        surface: SurfaceRule::Azure,
        routes_upstreams: false,
    },
    Profile {
        provider: "azure-cognitive-services",
        surface: SurfaceRule::Azure,
        routes_upstreams: false,
    },
    row("cerebras"),
    row("cloudflare-ai-gateway"),
    row("cloudflare-workers-ai"),
    row("cohere"),
    row("deepinfra"),
    row("deepseek"),
    Profile {
        provider: "github-copilot",
        surface: SurfaceRule::Copilot,
        routes_upstreams: false,
    },
    row("gitlab"),
    row("groq"),
    // The oracle pins `meta` to `responses` through its own custom loader
    // (`provider.ts:218-223`) while still using an OpenAI-shaped body.
    Profile {
        provider: "meta",
        surface: SurfaceRule::Fixed(ApiSurface::Responses),
        routes_upstreams: false,
    },
    row("mistral"),
    // The generic declared-compatible entry: a user's own endpoint.
    row("openai-compatible"),
    Profile {
        provider: "openrouter",
        surface: SurfaceRule::Fixed(ApiSurface::Chat),
        routes_upstreams: true,
    },
    row("perplexity"),
    row("togetherai"),
    row("venice"),
    Profile {
        provider: "vercel",
        surface: SurfaceRule::Fixed(ApiSurface::Chat),
        routes_upstreams: true,
    },
    // `provider.ts:212-217` — xAI's custom loader returns `sdk.responses(id)`.
    Profile {
        provider: "xai",
        surface: SurfaceRule::Fixed(ApiSurface::Responses),
        routes_upstreams: false,
    },
];

/// A claimed id with no rule beyond "use the SDK default surface".
const fn row(provider: &'static str) -> Profile {
    Profile {
        provider,
        surface: SurfaceRule::Fixed(ApiSurface::Default),
        routes_upstreams: false,
    }
}

/// Provider ids another crate carries, and which one.
///
/// Kept separate from [`CLAIMED`] so a refusal is a lookup rather than a guess,
/// and so adding a family to the workspace is one row here.
const ELSEWHERE: &[(&str, Family)] = &[
    ("amazon-bedrock", Family::Bedrock),
    ("anthropic", Family::Anthropic),
    ("google", Family::Google),
    ("google-vertex", Family::Google),
    ("google-vertex-anthropic", Family::Google),
    ("openai", Family::OpenAi),
];

/// The profile for a claimed id, or `None`.
#[must_use]
pub fn claimed(provider: &str) -> Option<Profile> {
    CLAIMED.iter().copied().find(|row| row.provider == provider)
}

/// The family that carries `provider`, when this profile does not.
#[must_use]
pub fn elsewhere(provider: &str) -> Option<Family> {
    ELSEWHERE
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, family)| *family)
}

/// Resolve a spec to a profile, or explain the refusal.
///
/// # Errors
///
/// Returns [`UnsupportedProvider`] when the id belongs to another family, or when
/// it is unknown and the spec carries no [`NPM_OPTION`] declaration. Both cases
/// render as "unsupported"; only the first can name a destination crate, and the
/// error keeps that distinction as data rather than as prose.
pub fn resolve(spec: &Spec) -> Result<Profile, UnsupportedProvider> {
    if let Some(profile) = claimed(&spec.provider) {
        return Ok(profile);
    }
    if let Some(family) = elsewhere(&spec.provider) {
        return Err(UnsupportedProvider {
            provider: spec.provider.clone(),
            carried_by: Some(family),
        });
    }
    if declares_compatible(spec) {
        return Ok(row_owned(&spec.provider));
    }
    Err(UnsupportedProvider {
        provider: spec.provider.clone(),
        carried_by: None,
    })
}

/// Whether the spec declares this id OpenAI-compatible.
fn declares_compatible(spec: &Spec) -> bool {
    spec.options
        .get(NPM_OPTION)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|npm| npm == OPENAI_COMPATIBLE_NPM)
}

/// A profile for a declared-compatible id, which is not in the static table.
///
/// The returned row borrows nothing from the spec: an opted-in id gets exactly
/// the default treatment, and the surface still travels on the spec.
fn row_owned(_provider: &str) -> Profile {
    Profile {
        provider: OPENAI_COMPATIBLE_NPM,
        surface: SurfaceRule::Fixed(ApiSurface::Default),
        routes_upstreams: false,
    }
}

/// This profile will not serve a provider id.
///
/// Rendered rather than matched by callers, but the *fields* are what a caller
/// acts on: `carried_by` says whether there is somewhere else to go. It is
/// carried inside [`zuno_error::ProviderError::Fatal`] as a source, so the whole
/// text reaches a `{:#}` render without any layer re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedProvider {
    /// The id that was configured against this profile.
    pub provider: String,
    /// The family that does carry it, when one exists.
    pub carried_by: Option<Family>,
}

impl fmt::Display for UnsupportedProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` is unsupported by the openai-compatible profile: ",
            self.provider
        )?;
        match self.carried_by {
            Some(family) => write!(
                formatter,
                "{}, so it is carried by the `{}` crate ({} family) — configure it there \
                 instead of through this profile",
                family.wire_difference(),
                family.crate_name(),
                family,
            ),
            None => write!(
                formatter,
                "no implemented provider family claims this id. If it really is an \
                 OpenAI-compatible endpoint, declare it with \
                 `provider.{}.options.{} = \"{}\"`; otherwise it belongs to one of {}",
                self.provider, NPM_OPTION, OPENAI_COMPATIBLE_NPM, FamilyList,
            ),
        }
    }
}

impl std::error::Error for UnsupportedProvider {}

/// Renders the crates a user could be looking for, without allocating a Vec.
struct FamilyList;

impl fmt::Display for FamilyList {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, family) in Family::ALL.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "`{}`", family.crate_name())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_claim_table_is_sorted_and_has_no_duplicates() {
        let ids: Vec<&str> = CLAIMED.iter().map(|row| row.provider).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "CLAIMED must stay sorted for stable reports");
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "a provider id is listed twice");
    }

    #[test]
    fn no_id_is_both_claimed_and_delegated() {
        for (id, family) in ELSEWHERE {
            assert!(
                claimed(id).is_none(),
                "`{id}` is claimed here and also delegated to {family}"
            );
        }
    }

    #[test]
    fn every_family_except_this_one_is_reachable_from_a_refusal() {
        for family in Family::ALL {
            if family == Family::Compatible {
                continue;
            }
            assert!(
                ELSEWHERE.iter().any(|(_, listed)| *listed == family),
                "no provider id routes to {family}, so its crate can never be named"
            );
        }
    }

    #[test]
    fn an_unknown_id_is_refused_without_naming_a_crate_it_cannot_name() {
        let error = resolve(&Spec::new("some-local-server")).expect_err("not claimed");
        assert_eq!(error.carried_by, None);
        let rendered = error.to_string();
        assert!(rendered.contains("unsupported"), "{rendered}");
        assert!(rendered.contains(OPENAI_COMPATIBLE_NPM), "{rendered}");
    }

    #[test]
    fn an_unknown_id_that_declares_itself_compatible_is_accepted() {
        let spec =
            Spec::new("some-local-server").with_option(NPM_OPTION, json!(OPENAI_COMPATIBLE_NPM));
        let profile = resolve(&spec).expect("declared compatible");
        assert_eq!(profile.surface, SurfaceRule::Fixed(ApiSurface::Default));
    }

    #[test]
    fn a_declaration_naming_a_different_sdk_is_not_consent() {
        let spec =
            Spec::new("some-local-server").with_option(NPM_OPTION, json!("@ai-sdk/anthropic"));
        assert!(resolve(&spec).is_err());
    }
}
