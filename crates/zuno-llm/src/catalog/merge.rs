//! Merging the user's `provider.*` config over the models.dev catalog.
//!
//! models.dev is an input catalog, not Zuno's runtime provider registry. Package
//! names from that source are translated to native [`ProviderTransport`] values
//! here and do not travel into user configuration or provider construction.
//!
//! # The fallback ladders, verbatim
//!
//! For a model declared in config under key `K` against catalog model `E`:
//!
//! | field | ladder | oracle |
//! |---|---|---|
//! | wire id | `config.id` → `E.api.id` → `K` | `:1438` |
//! | transport | model config → provider config → existing resolved model → imported catalog → `openai-compatible` |
//! | surface | model config → provider config → existing resolved model |
//! | url | `config.provider.api` → `provider.api` → `E.api.url` → catalog provider `api` → `""` | `:1455` |
//! | name | `config.name` → (`K` when `config.id` renames) → `E.name` → `K` | `:1445-1449` |
//! | toolcall | `config.tool_call` → `E` → **`true`** | `:1464` |
//! | everything else boolean | `config.x` → `E` → `false` | `:1461-1481` |
//!
//! Two of those are easy to get wrong in the same direction and both matter:
//!
//! - **`tool_call` defaults to `true`**, alone among the capability booleans. A
//!   `false` default would make every config-declared model refuse to call tools.
//! - **The `name` rung in parentheses is real.** When config gives an `id` that
//!   differs from the map key and no `name`, the *key* becomes the display name
//!   (`:1447`) — so `{"my-alias": {"id": "upstream-model"}}` shows as `my-alias`,
//!   not as the upstream model's name.
//!
//! # The deepseek interleaving special case
//!
//! A model with **no** catalog entry, on the native `openai-compatible` transport,
//! whose wire id contains `deepseek`, defaults to `{ field: "reasoning_content" }`.
//! It is a genuine upstream quirk — DeepSeek-compatible endpoints put reasoning
//! there — and it is gated on there being no existing entry, so it cannot override
//! a catalog that says otherwise. Ported as-is, including the gate.
//!
//! # Where this stops, deliberately
//!
//! `variants` are **merged**, not **derived**. The oracle derives a default variant
//! set with `ProviderTransform.variants(model)` (`:1508-1511`, `:1640-1642`), which
//! is reasoning-effort logic and belongs to `effort.rs` — todo 31, concurrent with
//! this one. This module merges config-declared variants over whatever it is given
//! and drops the ones marked `disabled` (`:1512-1516`), which is the config
//! concern; [`MergeOutcome::variant_derivation_pending`] names the providers where
//! a derived set would have applied, so todo 31 has a seam rather than a rewrite.

use std::collections::{BTreeMap, BTreeSet};

use zuno_config::schema::provider::{
    Modality as ConfigModality, ModelConfig, ModelStatus as ConfigModelStatus, ProviderConfig,
    ProviderSurface as ConfigProviderSurface, ProviderTransport,
};

use crate::catalog::models_dev::{
    CatalogCost, CatalogModel, CatalogProvider, CatalogStatus, Interleaved, Modality,
};
use crate::catalog::resolved::{
    CacheCost, JsonMap, ModalityFlags, ModelApi, ModelCapabilities, ModelCost, ModelEndpoint,
    ModelLimit, ResolvedModel, ResolvedProvider,
};

/// Native transport a model falls back to when no source names one.
pub const DEFAULT_TRANSPORT: ProviderTransport = ProviderTransport::OpenaiCompatible;

/// models.dev provider/package pairs whose wire protocol is implemented by the
/// native OpenAI-compatible provider.
const CATALOG_COMPATIBLE_TRANSPORTS: &[(&str, &str)] = &[
    ("alibaba", "@ai-sdk/alibaba"),
    ("azure", "@ai-sdk/azure"),
    ("cerebras", "@ai-sdk/cerebras"),
    ("cohere", "@ai-sdk/cohere"),
    ("deepinfra", "@ai-sdk/deepinfra"),
    ("github-copilot", "@ai-sdk/github-copilot"),
    ("gitlab", "gitlab-ai-provider"),
    ("groq", "@ai-sdk/groq"),
    ("mistral", "@ai-sdk/mistral"),
    ("perplexity", "@ai-sdk/perplexity"),
    ("togetherai", "@ai-sdk/togetherai"),
    ("venice", "venice-ai-sdk-provider"),
    ("vercel", "@ai-sdk/vercel"),
    ("xai", "@ai-sdk/xai"),
];

/// Translate external models.dev package metadata into a native transport.
fn catalog_transport(provider_id: &str, package: Option<&str>) -> Option<ProviderTransport> {
    match package {
        None => Some(DEFAULT_TRANSPORT),
        Some("@ai-sdk/anthropic") => Some(ProviderTransport::Anthropic),
        Some("@ai-sdk/amazon-bedrock") => Some(ProviderTransport::Bedrock),
        Some("@ai-sdk/amazon-bedrock/mantle") => Some(ProviderTransport::BedrockMantle),
        Some("@ai-sdk/google") => Some(ProviderTransport::Google),
        Some("@ai-sdk/google-vertex") => Some(ProviderTransport::GoogleVertex),
        Some("@ai-sdk/google-vertex/anthropic") => Some(ProviderTransport::GoogleVertexAnthropic),
        Some("@ai-sdk/openai") => Some(ProviderTransport::Openai),
        Some("@ai-sdk/openai-compatible") => Some(ProviderTransport::OpenaiCompatible),
        Some("@openrouter/ai-sdk-provider") => Some(ProviderTransport::Openrouter),
        Some(package)
            if CATALOG_COMPATIBLE_TRANSPORTS
                .iter()
                .any(|&(identity, expected)| identity == provider_id && expected == package) =>
        {
            Some(ProviderTransport::OpenaiCompatible)
        }
        Some(_) => None,
    }
}

/// The reasoning field DeepSeek-compatible endpoints use — `:1486`.
const DEEPSEEK_REASONING_FIELD: &str = "reasoning_content";

/// Model ids that are aliases the bundled providers cannot handle — `:1622-1631`.
///
/// Dropped for the three providers whose SDK surface selection would mis-route
/// them, and kept for everyone else, because a custom provider may well support
/// the alias. The list is short and hard-coded upstream; hard-coding *aliases* is
/// not hard-coding a model list.
const CHAT_ALIAS_EXCLUSIONS: [(&str, &str); 4] = [
    ("openai", "gpt-5-chat-latest"),
    ("github-copilot", "gpt-5-chat-latest"),
    ("openrouter", "gpt-5-chat-latest"),
    ("openrouter", "openai/gpt-5-chat"),
];

/// What a merge produced, plus the seams it deliberately left open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Providers whose models would receive a derived variant set from
    /// `ProviderTransform.variants`, which lives in todo 31's `effort.rs`.
    pub variant_derivation_pending: BTreeSet<String>,
}

/// Build a resolved provider from a catalog entry alone, with no config.
///
/// The port of `fromModelsDevProvider` (`provider.ts:1265-1290`), including the
/// `experimental.modes` expansion: each mode becomes its own model id
/// `<model.id>-<mode>` with the mode's cost merged over the base and the mode's
/// headers replacing the base's (`:1269-1280`).
#[must_use]
pub fn from_catalog(provider_id: &str, provider: &CatalogProvider) -> ResolvedProvider {
    let mut models = BTreeMap::new();
    for (key, model) in &provider.models {
        let base = model_from_catalog(provider_id, provider, model);
        if let Some(experimental) = &model.experimental {
            for (mode, overrides) in &experimental.modes {
                let id = format!("{}-{mode}", model.id);
                let mut derived = base.clone();
                derived.id = id.clone();
                derived.name = format!("{} {}", model.name, capitalize(mode));
                if let Some(cost) = &overrides.cost {
                    derived.cost = merge_cost(&base.cost, cost);
                }
                if let Some(mode_provider) = &overrides.provider {
                    if let Some(body) = &mode_provider.body {
                        derived.options = mode_options(&base, body);
                    }
                    if let Some(headers) = &mode_provider.headers {
                        derived.headers = headers.clone();
                    }
                }
                models.insert(id, derived);
            }
        }
        models.insert(key.clone(), base);
    }
    ResolvedProvider {
        id: provider_id.to_owned(),
        name: provider.name.clone(),
        env: provider.env.clone(),
        options: JsonMap::new(),
        availability: crate::catalog::availability::Availability::none(),
        models,
    }
}

/// `provider.ts:1292-1301` — a mode's snake_case body keys become camelCase
/// SDK options, with one rewrite.
///
/// The rewrite: on `@ai-sdk/openai`, a `reasoning: {mode}` object collapses to a
/// flat `reasoningMode` (`:1297-1300`). Everywhere else the object passes through.
fn mode_options(base: &ResolvedModel, body: &JsonMap) -> JsonMap {
    let mut options: JsonMap = body
        .iter()
        .map(|(key, value)| (snake_to_camel(key), value.clone()))
        .collect();
    if base.api.transport != Some(ProviderTransport::Openai) {
        return options;
    }
    let mode = body
        .get("reasoning")
        .and_then(serde_json::Value::as_object)
        .and_then(|reasoning| reasoning.get("mode"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if let Some(mode) = mode {
        options.remove("reasoning");
        options.insert("reasoningMode".to_owned(), serde_json::Value::String(mode));
    }
    options
}

/// One catalog model, resolved with no config involved.
fn model_from_catalog(
    provider_id: &str,
    provider: &CatalogProvider,
    model: &CatalogModel,
) -> ResolvedModel {
    let package = model
        .provider
        .as_ref()
        .and_then(|api| api.npm.clone())
        .or_else(|| provider.npm.clone());
    let url = model
        .provider
        .as_ref()
        .and_then(|api| api.api.clone())
        .or_else(|| provider.api.clone())
        .unwrap_or_default();
    ResolvedModel {
        id: model.id.clone(),
        provider_id: provider_id.to_owned(),
        name: model.name.clone(),
        family: model.family.clone().unwrap_or_default(),
        release_date: model.release_date.clone(),
        status: model.status.unwrap_or(CatalogStatus::Active),
        api: ModelApi {
            id: model.id.clone(),
            transport: catalog_transport(provider_id, package.as_deref()),
            url,
            endpoint: None,
        },
        capabilities: ModelCapabilities {
            temperature: model.temperature,
            reasoning: model.reasoning,
            attachment: model.attachment,
            toolcall: model.tool_call,
            input: modality_flags(model.modalities.as_ref().map(|m| m.input.as_slice())),
            output: modality_flags(model.modalities.as_ref().map(|m| m.output.as_slice())),
            interleaved: model.interleaved.clone().unwrap_or_default(),
        },
        cost: model
            .cost
            .as_ref()
            .map(cost_from_catalog)
            .unwrap_or_default(),
        limit: ModelLimit {
            context: model.limit.context,
            input: model.limit.input,
            output: model.limit.output,
        },
        options: JsonMap::new(),
        headers: BTreeMap::new(),
        variants: BTreeMap::new(),
    }
}

/// Flatten a modality array into flags, defaulting text on — `:1465-1481`.
fn modality_flags(declared: Option<&[Modality]>) -> ModalityFlags {
    let Some(declared) = declared else {
        return ModalityFlags::default();
    };
    if declared.is_empty() {
        return ModalityFlags::default();
    }
    ModalityFlags {
        text: declared.contains(&Modality::Text),
        audio: declared.contains(&Modality::Audio),
        image: declared.contains(&Modality::Image),
        video: declared.contains(&Modality::Video),
        pdf: declared.contains(&Modality::Pdf),
    }
}

/// Flatten catalog pricing into the oracle's shape — `:1489-1496`.
fn cost_from_catalog(cost: &CatalogCost) -> ModelCost {
    ModelCost {
        input: cost.input,
        output: cost.output,
        cache: CacheCost {
            read: cost.cache_read.unwrap_or_default(),
            write: cost.cache_write.unwrap_or_default(),
        },
    }
}

/// Merge a mode's pricing over a base — `:1276`.
fn merge_cost(base: &ModelCost, overrides: &CatalogCost) -> ModelCost {
    ModelCost {
        input: overrides.input,
        output: overrides.output,
        cache: CacheCost {
            read: overrides.cache_read.unwrap_or(base.cache.read),
            write: overrides.cache_write.unwrap_or(base.cache.write),
        },
    }
}

/// Apply a user's `provider.<id>` block over a catalog-derived provider.
///
/// `existing` is `None` for a provider the catalog has never heard of, which is a
/// supported case: `{"provider":{"my-gateway":{...}}}` with no catalog entry
/// produces a working provider, verified against 1.18.12.
///
/// The catalog entry is passed separately from `existing` because the npm and url
/// ladders consult *both* the resolved model and the raw catalog provider, at
/// different rungs (`:1443`, `:1455`).
#[must_use]
pub fn apply_config(
    provider_id: &str,
    config: &ProviderConfig,
    existing: Option<&ResolvedProvider>,
    catalog: Option<&CatalogProvider>,
    outcome: &mut MergeOutcome,
) -> ResolvedProvider {
    let mut models = existing
        .map(|provider| provider.models.clone())
        .unwrap_or_default();

    if let Some(surface) = config.surface {
        let endpoint = config_surface(surface);
        for model in models.values_mut() {
            model.api.endpoint = Some(endpoint);
        }
    }

    if let Some(declared) = &config.models {
        for (model_key, model_config) in declared.iter() {
            // `:1437` looks the existing model up by the *wire* id when config
            // renames, so `{"alias":{"id":"real"}}` inherits `real`'s metadata.
            let lookup = model_config.id.as_deref().unwrap_or(model_key);
            let existing_model = models.get(lookup).cloned();
            let merged = merge_model(
                provider_id,
                model_key,
                model_config,
                existing_model.as_ref(),
                config,
                catalog,
            );
            if existing_model.is_none() {
                // A brand-new model has no derived variant set yet; todo 31 owns
                // producing one.
                outcome
                    .variant_derivation_pending
                    .insert(provider_id.to_owned());
            }
            models.insert((*model_key).to_owned(), merged);
        }
    }

    let mut options = existing
        .map(|provider| provider.options.clone())
        .unwrap_or_default();
    if let Some(config_options) = &config.options {
        merge_json_into(&mut options, &provider_options_json(config_options));
    }

    ResolvedProvider {
        id: provider_id.to_owned(),
        name: config
            .name
            .clone()
            .or_else(|| existing.map(|provider| provider.name.clone()))
            .unwrap_or_else(|| provider_id.to_owned()),
        env: config
            .env
            .clone()
            .or_else(|| existing.map(|provider| provider.env.clone()))
            .unwrap_or_default(),
        options,
        availability: existing
            .map(|provider| provider.availability.clone())
            .unwrap_or_default(),
        models,
    }
}

/// Serialize `ProviderOptions` back to the JSON bag the SDKs receive.
///
/// Round-tripping through `serde_json` rather than hand-copying fields is
/// deliberate: `ProviderOptions` carries a `#[serde(flatten)] extra` bag for
/// options this workspace has not typed, and hand-copying would drop it.
fn provider_options_json(options: &zuno_config::schema::provider::ProviderOptions) -> JsonMap {
    serde_json::to_value(options)
        .ok()
        .and_then(|value| match value {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// Deep-merge `patch` into `target`, the way the oracle's `mergeDeep` does.
///
/// Objects merge recursively; every other value replaces. Arrays replace rather
/// than concatenate, which is what a user overriding a list expects.
fn merge_json_into(target: &mut JsonMap, patch: &JsonMap) {
    for (key, value) in patch {
        match (target.get_mut(key), value) {
            (Some(serde_json::Value::Object(existing)), serde_json::Value::Object(incoming)) => {
                merge_json_into(existing, incoming);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// One model, config merged over catalog — `provider.ts:1436-1517`.
#[expect(
    clippy::too_many_lines,
    reason = "one fallback ladder per field, matching provider.ts:1436-1517 rung \
              for rung; splitting it would hide which rungs belong together"
)]
fn merge_model(
    provider_id: &str,
    model_key: &str,
    config: &ModelConfig,
    existing: Option<&ResolvedModel>,
    provider_config: &ProviderConfig,
    catalog: Option<&CatalogProvider>,
) -> ResolvedModel {
    let api_id = config
        .id
        .clone()
        .or_else(|| existing.map(|model| model.api.id.clone()))
        .unwrap_or_else(|| model_key.to_owned());

    let api_transport = config
        .provider
        .as_ref()
        .and_then(|api| api.transport)
        .or(provider_config.transport)
        .or_else(|| existing.and_then(|model| model.api.transport))
        .or_else(|| {
            catalog_transport(
                provider_id,
                catalog.and_then(|provider| provider.npm.as_deref()),
            )
        });

    let api_url = config
        .provider
        .as_ref()
        .and_then(|api| api.api.clone())
        .or_else(|| provider_config.api.clone())
        .or_else(|| existing.map(|model| model.api.url.clone()))
        .or_else(|| catalog.and_then(|provider| provider.api.clone()))
        .unwrap_or_default();
    let api_endpoint = config
        .provider
        .as_ref()
        .and_then(|api| api.surface)
        .or(provider_config.surface)
        .map(config_surface)
        .or_else(|| existing.and_then(|model| model.api.endpoint));

    // `:1445-1449`. The middle rung is the surprising one: an `id` that renames
    // makes the map key the display name.
    let name = config
        .name
        .clone()
        .unwrap_or_else(|| match config.id.as_deref() {
            Some(id) if id != model_key => model_key.to_owned(),
            _ => existing
                .map(|model| model.name.clone())
                .unwrap_or_else(|| model_key.to_owned()),
        });

    let existing_caps = existing.map(|model| &model.capabilities);
    let capabilities = ModelCapabilities {
        temperature: config
            .temperature
            .or_else(|| existing_caps.map(|caps| caps.temperature))
            .unwrap_or(false),
        reasoning: config
            .reasoning
            .or_else(|| existing_caps.map(|caps| caps.reasoning))
            .unwrap_or(false),
        attachment: config
            .attachment
            .or_else(|| existing_caps.map(|caps| caps.attachment))
            .unwrap_or(false),
        // `:1464` — the one capability whose default is true.
        toolcall: config
            .tool_call
            .or_else(|| existing_caps.map(|caps| caps.toolcall))
            .unwrap_or(true),
        input: config_modality_flags(
            config.modalities.as_ref().and_then(|m| m.input.as_deref()),
            existing_caps.map(|caps| caps.input),
        ),
        output: config_modality_flags(
            config.modalities.as_ref().and_then(|m| m.output.as_deref()),
            existing_caps.map(|caps| caps.output),
        ),
        interleaved: config
            .interleaved
            .as_ref()
            .map(config_interleaved)
            .or_else(|| existing_caps.map(|caps| caps.interleaved.clone()))
            .unwrap_or_else(|| {
                // `:1485-1487` — only when the catalog has never seen this model.
                if existing.is_none()
                    && api_transport == Some(DEFAULT_TRANSPORT)
                    && api_id.contains("deepseek")
                {
                    Interleaved::Field {
                        field: DEEPSEEK_REASONING_FIELD.to_owned(),
                    }
                } else {
                    Interleaved::Flag(false)
                }
            }),
    };

    let existing_cost = existing.map(|model| model.cost);
    let cost = ModelCost {
        input: config
            .cost
            .as_ref()
            .map(|cost| cost.input)
            .or_else(|| existing_cost.map(|cost| cost.input))
            .unwrap_or_default(),
        output: config
            .cost
            .as_ref()
            .map(|cost| cost.output)
            .or_else(|| existing_cost.map(|cost| cost.output))
            .unwrap_or_default(),
        cache: CacheCost {
            read: config
                .cost
                .as_ref()
                .and_then(|cost| cost.cache_read)
                .or_else(|| existing_cost.map(|cost| cost.cache.read))
                .unwrap_or_default(),
            write: config
                .cost
                .as_ref()
                .and_then(|cost| cost.cache_write)
                .or_else(|| existing_cost.map(|cost| cost.cache.write))
                .unwrap_or_default(),
        },
    };

    let existing_limit = existing.map(|model| model.limit);
    let limit = ModelLimit {
        context: config
            .limit
            .as_ref()
            .map(|limit| limit.context)
            .or_else(|| existing_limit.map(|limit| limit.context))
            .unwrap_or_default(),
        input: config
            .limit
            .as_ref()
            .and_then(|limit| limit.input)
            .or_else(|| existing_limit.and_then(|limit| limit.input)),
        output: config
            .limit
            .as_ref()
            .map(|limit| limit.output)
            .or_else(|| existing_limit.map(|limit| limit.output))
            .unwrap_or_default(),
    };

    let mut options = existing
        .map(|model| model.options.clone())
        .unwrap_or_default();
    if let Some(config_options) = &config.options {
        merge_json_into(&mut options, config_options);
    }

    let mut headers = existing
        .map(|model| model.headers.clone())
        .unwrap_or_default();
    if let Some(config_headers) = &config.headers {
        headers.extend(
            config_headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }

    let mut variants = existing
        .map(|model| model.variants.clone())
        .unwrap_or_default();
    merge_variants(&mut variants, config);

    ResolvedModel {
        id: model_key.to_owned(),
        provider_id: provider_id.to_owned(),
        name,
        family: config
            .family
            .clone()
            .or_else(|| existing.map(|model| model.family.clone()))
            .unwrap_or_default(),
        release_date: config
            .release_date
            .clone()
            .or_else(|| existing.map(|model| model.release_date.clone()))
            .unwrap_or_default(),
        status: config
            .status
            .map(config_status)
            .or_else(|| existing.map(|model| model.status))
            .unwrap_or(CatalogStatus::Active),
        api: ModelApi {
            id: api_id,
            transport: api_transport,
            url: api_url,
            endpoint: api_endpoint,
        },
        capabilities,
        cost,
        limit,
        options,
        headers,
        variants,
    }
}

const fn config_surface(surface: ConfigProviderSurface) -> ModelEndpoint {
    match surface {
        ConfigProviderSurface::Chat => ModelEndpoint::Chat,
        ConfigProviderSurface::Responses => ModelEndpoint::Responses,
        ConfigProviderSurface::Messages => ModelEndpoint::Messages,
    }
}

/// Merge config variants over an existing set, dropping the disabled ones.
///
/// `:1512-1516` and `:1644-1651`. `disabled: true` removes the variant entirely
/// rather than storing the flag, and the flag itself never reaches the SDK.
fn merge_variants(variants: &mut BTreeMap<String, JsonMap>, config: &ModelConfig) {
    let Some(declared) = &config.variants else {
        return;
    };
    for (name, variant) in declared.iter() {
        if variant.disabled == Some(true) {
            variants.remove(name);
            continue;
        }
        let entry = variants.entry(name.to_owned()).or_default();
        merge_json_into(entry, &variant.extra);
        entry.remove("disabled");
    }
}

/// Config modalities, flattened, falling back to the catalog's flags.
///
/// `:1466-1481` reads each modality independently: a config declaring only
/// `["image"]` for input turns image on **and text off**, because each flag falls
/// back to the existing value only when `modalities` is absent entirely.
fn config_modality_flags(
    declared: Option<&[ConfigModality]>,
    existing: Option<ModalityFlags>,
) -> ModalityFlags {
    let Some(declared) = declared else {
        return existing.unwrap_or_default();
    };
    let has = |wanted: ConfigModality| declared.contains(&wanted);
    ModalityFlags {
        text: has(ConfigModality::Text),
        audio: has(ConfigModality::Audio),
        image: has(ConfigModality::Image),
        video: has(ConfigModality::Video),
        pdf: has(ConfigModality::Pdf),
    }
}

/// Config's interleaved union, mapped onto the catalog's.
fn config_interleaved(interleaved: &zuno_config::schema::provider::Interleaved) -> Interleaved {
    use zuno_config::schema::provider::Interleaved as ConfigInterleaved;
    match interleaved {
        ConfigInterleaved::Enabled(flag) => Interleaved::Flag(*flag),
        ConfigInterleaved::Field(field) => Interleaved::Name(field.clone()),
        ConfigInterleaved::Wrapped(wrapped) => Interleaved::Field {
            field: wrapped.field.clone(),
        },
    }
}

/// Config's status enum, mapped onto the catalog's.
const fn config_status(status: ConfigModelStatus) -> CatalogStatus {
    match status {
        ConfigModelStatus::Alpha => CatalogStatus::Alpha,
        ConfigModelStatus::Beta => CatalogStatus::Beta,
        ConfigModelStatus::Deprecated => CatalogStatus::Deprecated,
        ConfigModelStatus::Active => CatalogStatus::Active,
    }
}

/// Remove every model the user cannot select — `provider.ts:1620-1652`.
///
/// In the oracle's order, because the order is observable when several rules would
/// fire: chat-alias exclusions, then `alpha` unless experimental models are on,
/// then `deprecated` always, then the blacklist, then the whitelist.
pub fn filter_models(
    provider: &mut ResolvedProvider,
    config: Option<&ProviderConfig>,
    experimental_models: bool,
) {
    let provider_id = provider.id.clone();
    provider.models.retain(|model_id, model| {
        if CHAT_ALIAS_EXCLUSIONS
            .iter()
            .any(|(excluded_provider, excluded_model)| {
                *excluded_provider == provider_id && *excluded_model == model_id
            })
        {
            return false;
        }
        if model.status == CatalogStatus::Alpha && !experimental_models {
            return false;
        }
        if model.status == CatalogStatus::Deprecated {
            return false;
        }
        if let Some(config) = config {
            if let Some(blacklist) = &config.blacklist
                && blacklist.iter().any(|entry| entry == model_id)
            {
                return false;
            }
            if let Some(whitelist) = &config.whitelist
                && !whitelist.iter().any(|entry| entry == model_id)
            {
                return false;
            }
        }
        true
    });
}

/// `mode[0].toUpperCase() + mode.slice(1)` — `:1275`.
fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// `key.replace(/_([a-z])/g, (_, c) => c.toUpperCase())` — `:1295`.
///
/// Only a lowercase letter after the underscore is promoted, matching the regex:
/// `max_2` stays `max_2`, `max_tokens` becomes `maxTokens`.
fn snake_to_camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut chars = key.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '_'
            && let Some(next) = chars.peek().copied()
            && next.is_ascii_lowercase()
        {
            chars.next();
            out.push(next.to_ascii_uppercase());
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_config::schema::ordered::OrderedMap;

    fn catalog_provider() -> CatalogProvider {
        serde_json::from_str(
            r#"{
              "name": "DeepSeek", "id": "deepseek", "env": ["DEEPSEEK_API_KEY"],
              "npm": "@ai-sdk/openai-compatible", "api": "https://api.deepseek.com",
              "models": {
                "deepseek-chat": {
                  "id": "deepseek-chat", "name": "DeepSeek Chat", "family": "deepseek",
                  "release_date": "2025-12-01", "attachment": true, "reasoning": false,
                  "temperature": true, "tool_call": true,
                  "cost": {"input": 0.14, "output": 0.28, "cache_read": 0.0028},
                  "limit": {"context": 131072, "output": 8192}
                }
              }
            }"#,
        )
        .expect("fixture parses")
    }

    fn model_config(json: &str) -> ModelConfig {
        serde_json::from_str(json).expect("model config parses")
    }

    fn provider_config(json: &str) -> ProviderConfig {
        serde_json::from_str(json).expect("provider config parses")
    }

    #[test]
    fn tool_call_defaults_to_true_for_a_brand_new_model() {
        // `:1464`. Every other capability defaults false; this one does not, and
        // getting it wrong makes every config-declared model refuse tools.
        let mut outcome = MergeOutcome::default();
        let config = provider_config(r#"{"models":{"brand-new":{}}}"#);
        let resolved = apply_config("acme", &config, None, None, &mut outcome);
        let model = &resolved.models["brand-new"];
        assert!(model.capabilities.toolcall, "tool_call must default true");
        assert!(!model.capabilities.reasoning);
        assert!(!model.capabilities.attachment);
        assert!(!model.capabilities.temperature);
    }

    #[test]
    fn a_renaming_id_makes_the_key_the_display_name() {
        // `:1447`. With `id` renaming and no `name`, the map key is the name.
        let mut outcome = MergeOutcome::default();
        let config = provider_config(r#"{"models":{"my-alias":{"id":"upstream-model"}}}"#);
        let resolved = apply_config("acme", &config, None, None, &mut outcome);
        let model = &resolved.models["my-alias"];
        assert_eq!(model.name, "my-alias");
        assert_eq!(model.api.id, "upstream-model");
        assert_eq!(model.id, "my-alias", "the selectable id is the key");
    }

    #[test]
    fn the_transport_ladder_prefers_the_model_then_the_provider_then_the_catalog() {
        let catalog = catalog_provider();
        let existing = from_catalog("deepseek", &catalog);
        let mut outcome = MergeOutcome::default();

        // Model-level wins.
        let config = provider_config(
            r#"{"transport":"bedrock",
                "models":{"m":{"provider":{"transport":"anthropic"}}}}"#,
        );
        let resolved = apply_config(
            "deepseek",
            &config,
            Some(&existing),
            Some(&catalog),
            &mut outcome,
        );
        assert_eq!(
            resolved.models["m"].api.transport,
            Some(ProviderTransport::Anthropic)
        );

        // Provider-level next.
        let config = provider_config(r#"{"transport":"bedrock","models":{"m":{}}}"#);
        let resolved = apply_config(
            "deepseek",
            &config,
            Some(&existing),
            Some(&catalog),
            &mut outcome,
        );
        assert_eq!(
            resolved.models["m"].api.transport,
            Some(ProviderTransport::Bedrock)
        );

        // Then the catalog provider's.
        let config = provider_config(r#"{"models":{"m":{}}}"#);
        let resolved = apply_config(
            "deepseek",
            &config,
            Some(&existing),
            Some(&catalog),
            &mut outcome,
        );
        assert_eq!(
            resolved.models["m"].api.transport,
            Some(ProviderTransport::OpenaiCompatible)
        );

        // And with nothing anywhere, the documented default.
        let resolved = apply_config("acme", &config, None, None, &mut outcome);
        assert_eq!(resolved.models["m"].api.transport, Some(DEFAULT_TRANSPORT));
    }

    #[test]
    fn the_surface_ladder_prefers_the_model_then_the_provider_then_the_catalog() {
        let catalog = catalog_provider();
        let mut existing = from_catalog("deepseek", &catalog);
        existing
            .models
            .get_mut("deepseek-chat")
            .expect("catalog fixture model")
            .api
            .endpoint = Some(ModelEndpoint::Messages);

        let config = provider_config(
            r#"{
                "surface":"responses",
                "models":{
                    "deepseek-chat":{"provider":{"surface":"chat"}},
                    "provider-default":{}
                }
            }"#,
        );
        let resolved = apply_config(
            "deepseek",
            &config,
            Some(&existing),
            Some(&catalog),
            &mut MergeOutcome::default(),
        );
        assert_eq!(
            resolved.models["deepseek-chat"].api.endpoint,
            Some(ModelEndpoint::Chat),
            "a model-level surface must override the provider default"
        );
        assert_eq!(
            resolved.models["provider-default"].api.endpoint,
            Some(ModelEndpoint::Responses),
            "a provider surface must apply to newly configured models"
        );

        let provider_only = provider_config(r#"{"surface":"responses"}"#);
        let resolved = apply_config(
            "deepseek",
            &provider_only,
            Some(&existing),
            Some(&catalog),
            &mut MergeOutcome::default(),
        );
        assert_eq!(
            resolved.models["deepseek-chat"].api.endpoint,
            Some(ModelEndpoint::Responses),
            "a provider surface must also apply to catalog models not repeated in config"
        );

        let inherited = provider_config(r#"{"models":{"deepseek-chat":{}}}"#);
        let resolved = apply_config(
            "deepseek",
            &inherited,
            Some(&existing),
            Some(&catalog),
            &mut MergeOutcome::default(),
        );
        assert_eq!(
            resolved.models["deepseek-chat"].api.endpoint,
            Some(ModelEndpoint::Messages),
            "an existing catalog surface remains the fallback"
        );
    }

    #[test]
    fn a_config_model_inherits_catalog_metadata_it_does_not_override() {
        let catalog = catalog_provider();
        let existing = from_catalog("deepseek", &catalog);
        let mut outcome = MergeOutcome::default();
        let config = provider_config(
            r#"{"models":{"deepseek-chat":{"limit":{"context":123456,"output":999}}}}"#,
        );
        let resolved = apply_config(
            "deepseek",
            &config,
            Some(&existing),
            Some(&catalog),
            &mut outcome,
        );
        let model = &resolved.models["deepseek-chat"];
        assert_eq!(model.limit.context, 123_456.0, "config wins");
        assert_eq!(model.limit.output, 999.0, "config wins");
        assert_eq!(model.family, "deepseek", "inherited");
        assert_eq!(model.release_date, "2025-12-01", "inherited");
        assert_eq!(model.cost.input, 0.14, "inherited");
        assert_eq!(model.cost.cache.read, 0.0028, "inherited");
        assert_eq!(model.cost.cache.write, 0.0, "absent upstream becomes zero");
        assert!(model.capabilities.attachment, "inherited");
        assert_eq!(model.api.url, "https://api.deepseek.com", "inherited");
    }

    #[test]
    fn the_deepseek_interleaving_default_applies_only_to_unknown_models() {
        let mut outcome = MergeOutcome::default();
        // Unknown model, compatible transport, deepseek in the wire id: the quirk fires.
        let config = provider_config(r#"{"models":{"deepseek-r1":{}}}"#);
        let resolved = apply_config("gw", &config, None, None, &mut outcome);
        assert_eq!(
            resolved.models["deepseek-r1"]
                .capabilities
                .interleaved
                .field(),
            Some("reasoning_content")
        );

        // Same id, but the catalog knows the model: the catalog wins.
        let catalog: CatalogProvider = serde_json::from_str(
            r#"{"name":"GW","id":"gw","env":[],"npm":"@ai-sdk/openai-compatible",
                "models":{"deepseek-r1":{"id":"deepseek-r1","name":"R1",
                  "limit":{"context":1,"output":1}}}}"#,
        )
        .expect("fixture parses");
        let existing = from_catalog("gw", &catalog);
        let resolved = apply_config("gw", &config, Some(&existing), Some(&catalog), &mut outcome);
        assert_eq!(
            resolved.models["deepseek-r1"].capabilities.interleaved,
            Interleaved::Flag(false),
            "a known model keeps the catalog's answer"
        );

        // And a different transport does not get the quirk.
        let config =
            provider_config(r#"{"models":{"deepseek-r1":{"provider":{"transport":"anthropic"}}}}"#);
        let resolved = apply_config("gw2", &config, None, None, &mut outcome);
        assert_eq!(
            resolved.models["deepseek-r1"].capabilities.interleaved,
            Interleaved::Flag(false)
        );
    }

    #[test]
    fn experimental_modes_become_their_own_model_ids() {
        // `:1269-1280`, verified against 1.18.12: a `fast` mode on
        // `anthropic/claude-opus-4-6` produced `anthropic/claude-opus-4-6-fast`.
        let catalog: CatalogProvider = serde_json::from_str(
            r#"{"name":"AnyAPI","id":"anyapi","env":["ANYAPI_API_KEY"],
                "npm":"@ai-sdk/openai-compatible","api":"https://api.anyapi.ai/v1",
                "models":{"m":{"id":"m","name":"Model","limit":{"context":1,"output":1},
                  "cost":{"input":1.0,"output":2.0},
                  "experimental":{"modes":{"fast":{
                     "cost":{"input":5.0,"output":6.0},
                     "provider":{"body":{"max_tokens":10},"headers":{"X-Fast":"1"}}}}}}}}"#,
        )
        .expect("fixture parses");
        let resolved = from_catalog("anyapi", &catalog);
        assert!(resolved.models.contains_key("m"));
        let fast = &resolved.models["m-fast"];
        assert_eq!(fast.name, "Model Fast", "mode name is capitalized");
        assert_eq!(fast.cost.input, 5.0, "mode cost replaces base");
        assert_eq!(fast.headers["X-Fast"], "1");
        assert_eq!(
            fast.options["maxTokens"],
            serde_json::json!(10),
            "snake_case body keys become camelCase options"
        );
        assert_eq!(resolved.models["m"].cost.input, 1.0, "base is untouched");
    }

    #[test]
    fn deprecated_is_always_filtered_and_alpha_only_without_the_flag() {
        let mut provider = ResolvedProvider {
            id: "p".to_owned(),
            name: "P".to_owned(),
            env: Vec::new(),
            options: JsonMap::new(),
            availability: crate::catalog::availability::Availability::none(),
            models: BTreeMap::new(),
        };
        for (id, status) in [
            ("active", CatalogStatus::Active),
            ("beta", CatalogStatus::Beta),
            ("alpha", CatalogStatus::Alpha),
            ("deprecated", CatalogStatus::Deprecated),
        ] {
            let mut model = ResolvedModel {
                id: id.to_owned(),
                provider_id: "p".to_owned(),
                name: id.to_owned(),
                family: String::new(),
                release_date: String::new(),
                status,
                api: ModelApi::default(),
                capabilities: ModelCapabilities::default(),
                cost: ModelCost::default(),
                limit: ModelLimit::default(),
                options: JsonMap::new(),
                headers: BTreeMap::new(),
                variants: BTreeMap::new(),
            };
            model.id = id.to_owned();
            provider.models.insert(id.to_owned(), model);
        }

        let mut without = provider.clone();
        filter_models(&mut without, None, false);
        let mut ids: Vec<&str> = without.models.keys().map(String::as_str).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["active", "beta"], "alpha and deprecated go");

        let mut with = provider;
        filter_models(&mut with, None, true);
        let mut ids: Vec<&str> = with.models.keys().map(String::as_str).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["active", "alpha", "beta"],
            "deprecated still goes"
        );
    }

    #[test]
    fn a_disabled_variant_is_removed_rather_than_flagged() {
        let mut variants = BTreeMap::new();
        variants.insert("low".to_owned(), JsonMap::new());
        variants.insert("high".to_owned(), JsonMap::new());
        let config = model_config(
            r#"{"variants":{"low":{"disabled":true},"high":{"reasoningEffort":"high"}}}"#,
        );
        merge_variants(&mut variants, &config);
        assert!(
            !variants.contains_key("low"),
            "disabled variants are removed"
        );
        assert_eq!(
            variants["high"]["reasoningEffort"],
            serde_json::json!("high")
        );
        assert!(
            !variants["high"].contains_key("disabled"),
            "the flag never reaches the SDK"
        );
    }

    #[test]
    fn declaring_modalities_turns_the_undeclared_ones_off() {
        // `:1466-1481` reads each flag independently, so declaring only "image"
        // for input turns text off. Surprising, but it is what the oracle does.
        let catalog = catalog_provider();
        let existing = from_catalog("deepseek", &catalog);
        let mut outcome = MergeOutcome::default();
        let config =
            provider_config(r#"{"models":{"deepseek-chat":{"modalities":{"input":["image"]}}}}"#);
        let resolved = apply_config(
            "deepseek",
            &config,
            Some(&existing),
            Some(&catalog),
            &mut outcome,
        );
        let input = resolved.models["deepseek-chat"].capabilities.input;
        assert!(input.image);
        assert!(!input.text, "an undeclared modality is off, not inherited");
        let output = resolved.models["deepseek-chat"].capabilities.output;
        assert!(output.text, "an absent block does inherit");
    }

    #[test]
    fn provider_options_survive_the_round_trip_including_untyped_keys() {
        let mut outcome = MergeOutcome::default();
        let config = provider_config(
            r#"{"options":{"apiKey":"sk-x","baseURL":"https://x/v1","customThing":{"a":1}}}"#,
        );
        let resolved = apply_config("acme", &config, None, None, &mut outcome);
        assert_eq!(resolved.options["apiKey"], serde_json::json!("sk-x"));
        assert_eq!(
            resolved.options["baseURL"],
            serde_json::json!("https://x/v1")
        );
        assert_eq!(
            resolved.options["customThing"],
            serde_json::json!({"a": 1}),
            "the flattened extra bag must not be dropped"
        );
    }

    #[test]
    fn a_new_model_records_a_pending_variant_derivation() {
        // The seam for todo 31: this crate does not derive reasoning variants.
        let mut outcome = MergeOutcome::default();
        let config = provider_config(r#"{"models":{"brand-new":{}}}"#);
        let _ = apply_config("acme", &config, None, None, &mut outcome);
        assert!(outcome.variant_derivation_pending.contains("acme"));
    }

    #[test]
    fn snake_to_camel_only_promotes_lowercase_letters() {
        assert_eq!(snake_to_camel("max_tokens"), "maxTokens");
        assert_eq!(snake_to_camel("max_2"), "max_2");
        assert_eq!(snake_to_camel("a_b_c"), "aBC");
        assert_eq!(snake_to_camel("plain"), "plain");
        assert_eq!(snake_to_camel("trailing_"), "trailing_");
    }

    #[test]
    fn an_empty_ordered_model_map_leaves_the_catalog_alone() {
        let catalog = catalog_provider();
        let existing = from_catalog("deepseek", &catalog);
        let mut outcome = MergeOutcome::default();
        let config = ProviderConfig {
            models: Some(OrderedMap::default()),
            ..ProviderConfig::default()
        };
        let resolved = apply_config(
            "deepseek",
            &config,
            Some(&existing),
            Some(&catalog),
            &mut outcome,
        );
        assert_eq!(resolved.models.len(), 1);
        assert!(resolved.models.contains_key("deepseek-chat"));
    }
}
