//! `provider`, `model` and `integration` — the resolved catalogue on the wire.
//!
//! # Two layers that must not be confused
//!
//! `zuno-llm`'s [`Catalog`] already decides *which* providers and models a user can
//! select, and that decision is measured against the released binary by
//! `zuno-llm/tests/catalog_differential.rs`. This module does not repeat it. It
//! answers a different question: given that decision, what does upstream's V2 HTTP
//! surface *look like*?
//!
//! So availability comes from [`Catalog::resolve`] and the wire shape comes from
//! the models.dev document projected the way `packages/core/src/plugin/models-dev.ts`
//! projects it. Re-deriving availability here would fork the one behaviour the
//! port has already proven.
//!
//! # The projection is `models-dev.ts`, line for line
//!
//! - `applyModel` (`:74-113`) decides a model's `api`, `capabilities`, `cost`,
//!   `status`, `limit` and `time.released`.
//! - `projectModel` (`packages/core/src/catalog.ts:78-99`) then folds the
//!   provider's `api` and `request` into each model. The native-with-no-overrides
//!   case rewrites the model's `api` to the *provider's*, keeping only the model's
//!   `id` — that is why a `deepseek-chat` answer reports
//!   `package: "@ai-sdk/openai-compatible"` even though the model entry in
//!   models.dev names no package at all.
//! - `available` (`catalog.ts:71-76`) is the filter, and a provider with no
//!   available models still appears in `/api/provider` while contributing nothing
//!   to `/api/model`.
//!
//! # Integrations are derived, not stored
//!
//! `/api/integration` is the union of every models.dev provider that declares
//! `env` keys (`models-dev.ts:123-137`) and the two OAuth-capable integrations
//! upstream's provider plugins register. Sorting is `name.localeCompare`, which is
//! **case-insensitive first** — `openai` sorts before `OpenCode`, which a plain
//! byte comparison gets backwards. [`locale_compare`] exists for that one reason.
//!
//! A connection is reported for an integration whose environment variable is set,
//! which is what makes a provider available without a stored credential.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path as PathParam, State};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use zuno_llm::catalog::models_dev::{
    CatalogCost, CatalogDocument, CatalogModel, CatalogProvider, CatalogStatus,
};

use super::catalog::{LocationEnvelope, OptionalEnvelope, RequestBody};
use super::error::ApiError;
use super::state::ApiState;

/// The OAuth integrations upstream registers outside models.dev.
///
/// `openai`'s two ChatGPT methods and `opencode`'s device flow are declared by
/// provider plugins, not by the catalogue, so a models.dev-only derivation would
/// omit both and the differential would disagree by two entries. The labels are
/// the observed 1.18.12 strings.
const PLUGIN_INTEGRATIONS: &[StaticIntegration] = &[
    StaticIntegration {
        id: "openai",
        name: "openai",
        methods: &[
            StaticMethod::OAuth {
                id: "chatgpt-browser",
                label: "ChatGPT Pro/Plus (browser)",
            },
            StaticMethod::OAuth {
                id: "chatgpt-headless",
                label: "ChatGPT Pro/Plus (headless)",
            },
        ],
    },
    StaticIntegration {
        id: "opencode",
        name: "OpenCode",
        methods: &[
            StaticMethod::OAuth {
                id: "device",
                label: "OpenCode Console account",
            },
            StaticMethod::Key {
                label: Some("API key (service account)"),
            },
        ],
    },
];

/// A plugin-registered integration.
struct StaticIntegration {
    /// The integration id.
    id: &'static str,
    /// Its display name.
    name: &'static str,
    /// The connection methods it offers.
    methods: &'static [StaticMethod],
}

/// One plugin-registered connection method.
enum StaticMethod {
    /// An OAuth flow.
    OAuth {
        /// The method id.
        id: &'static str,
        /// The button label.
        label: &'static str,
    },
    /// A pasted API key.
    Key {
        /// The optional label.
        label: Option<&'static str>,
    },
}

/// Upstream's `Provider.Info` (`packages/schema/src/provider.ts:52-60`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderInfo {
    /// The provider id.
    pub id: String,
    /// The integration that authenticates it, when it is not the provider itself.
    #[serde(rename = "integrationID", skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
    /// Display name.
    pub name: String,
    /// Whether the provider is switched off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// How to talk to it.
    pub api: ProviderApi,
    /// Header and body overlay applied to every request.
    pub request: RequestBody,
}

/// A provider's `api` block. `type` decides which of the other fields appear.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderApi {
    /// `aisdk` or `native`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The AI SDK package, on the `aisdk` arm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra settings; required and empty on the `native` arm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
}

/// Upstream's `Model.Info` (`packages/schema/src/model.ts:59-86`), in field order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelInfo {
    /// The model id.
    pub id: String,
    /// The provider it belongs to.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Model family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Display name.
    pub name: String,
    /// How to talk to it.
    pub api: ModelApi,
    /// What it can do.
    pub capabilities: Capabilities,
    /// Header and body overlay.
    pub request: RequestBody,
    /// Alternate wire configurations.
    pub variants: Vec<Variant>,
    /// Release timestamp.
    pub time: ReleaseTime,
    /// Price bands.
    pub cost: Vec<Cost>,
    /// Lifecycle status.
    pub status: &'static str,
    /// Whether it is selectable.
    pub enabled: bool,
    /// Token limits.
    pub limit: Limit,
}

/// A model's `api` block. `id` leads, which is upstream's schema order rather than
/// the object-spread order the TypeScript happens to build.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelApi {
    /// The upstream model identifier.
    pub id: String,
    /// `aisdk` or `native`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The AI SDK package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
}

/// `Model.Capabilities` (`model.ts:31-35`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Whether the model can call tools.
    pub tools: bool,
    /// Accepted media types.
    pub input: Vec<String>,
    /// Produced media types.
    pub output: Vec<String>,
}

/// One entry of `variants`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Variant {
    /// The variant id.
    pub id: String,
    /// Header overlay.
    pub headers: Map<String, Value>,
    /// Body overlay.
    pub body: Map<String, Value>,
}

/// `time.released`, in epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseTime {
    /// Release timestamp, `0` when the catalogue's date does not parse
    /// (`models-dev.ts:9-12`).
    pub released: i64,
}

/// One price band (`model.ts:37-49`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cost {
    /// The context tier this band applies above, when it is not the base band.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<CostTier>,
    /// Input price per million tokens.
    pub input: Number,
    /// Output price per million tokens.
    pub output: Number,
    /// Cache prices.
    pub cache: CostCache,
}

/// A cost band's `{type, size}` bound.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostTier {
    /// Always `context` today.
    #[serde(rename = "type")]
    pub kind: String,
    /// The context size the band applies above.
    pub size: Number,
}

/// The `cache` half of a price band.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostCache {
    /// Cache-read price.
    pub read: Number,
    /// Cache-write price.
    pub write: Number,
}

/// `limit` (`model.ts:81-85`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Limit {
    /// Context window.
    pub context: i64,
    /// Input cap, when the catalogue states one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<i64>,
    /// Output cap.
    pub output: i64,
}

/// The generated legacy SDK's `GET /provider` payload.
///
/// This is deliberately projected from [`CatalogDocument`] rather than from
/// [`ModelInfo`]. `ModelInfo.time.released` is already epoch milliseconds for
/// the V2 wire contract; converting that value back into the legacy
/// `release_date` string loses the canonical catalogue spelling and caused the
/// old adapter to return strings such as `"1764547200000"`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LegacyProviderList {
    pub all: Vec<LegacyProviderInfo>,
    pub default: BTreeMap<String, String>,
    pub connected: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LegacyProviderInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    pub name: String,
    pub env: Vec<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    pub models: BTreeMap<String, LegacyModelInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LegacyModelInfo {
    pub id: String,
    pub name: String,
    pub release_date: String,
    pub attachment: bool,
    pub reasoning: bool,
    pub temperature: bool,
    pub tool_call: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<LegacyCost>,
    pub limit: LegacyLimit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<LegacyModalities>,
    pub status: &'static str,
    pub options: Map<String, Value>,
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<LegacyModelProvider>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LegacyCost {
    pub input: Number,
    pub output: Number,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<LegacyCostBand>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LegacyCostBand {
    pub input: Number,
    pub output: Number,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<Number>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LegacyLimit {
    pub context: Number,
    pub output: Number,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LegacyModalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LegacyModelProvider {
    pub npm: String,
}

/// Upstream's `Integration.Info` (`packages/schema/src/integration.ts:95-100`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationInfo {
    /// The integration id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The ways it can be connected.
    pub methods: Vec<Method>,
    /// The connections that currently exist.
    pub connections: Vec<Connection>,
}

/// One connection method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Method {
    /// An OAuth flow.
    OAuth {
        /// The method id.
        id: String,
        /// Always `oauth`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The button label.
        label: String,
    },
    /// A pasted API key.
    Key {
        /// Always `key`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The optional label.
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Environment variables that authenticate it.
    Env {
        /// Always `env`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The variable names.
        names: Vec<String>,
    },
}

/// One existing connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Connection {
    /// Always `env` here; stored-credential connections are not reported, see the
    /// note on [`integrations`].
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The variable that supplies it.
    pub name: String,
}

/// `GET /api/provider` — every provider the user can currently select.
///
/// # Errors
/// Returns [`ApiError::ConfigInvalid`] when the config tree does not parse and
/// [`ApiError::CatalogUnavailable`] when the models.dev document cannot be read.
pub async fn providers(
    State(state): State<ApiState>,
) -> Result<LocationEnvelope<Vec<ProviderInfo>>, ApiError> {
    let view = CatalogView::open(&state)?;
    Ok(state.envelope(view.providers()))
}

/// `GET /api/provider/{providerID}` — one provider.
///
/// Upstream answers **404** with a `ProviderNotFoundError` body for an unknown id
/// (`packages/server/src/handlers/provider.ts:18-30`), which is the one place in
/// this group where absence is an error rather than an empty value.
///
/// # Errors
/// Returns [`ApiError::ProviderNotFound`] for an unknown provider, plus the two
/// catalogue errors [`providers`] can raise.
pub async fn provider(
    State(state): State<ApiState>,
    PathParam(provider_id): PathParam<String>,
) -> Result<LocationEnvelope<ProviderInfo>, ApiError> {
    let view = CatalogView::open(&state)?;
    view.providers()
        .into_iter()
        .find(|candidate| candidate.id == provider_id)
        .map(|found| state.envelope(found))
        .ok_or(ApiError::ProviderNotFound(provider_id))
}

/// `GET /api/model` — every selectable model, newest release first.
///
/// # Errors
/// Same as [`providers`].
pub async fn models(
    State(state): State<ApiState>,
) -> Result<LocationEnvelope<Vec<ModelInfo>>, ApiError> {
    let view = CatalogView::open(&state)?;
    Ok(state.envelope(view.models()))
}

/// Projects the canonical catalogue onto the generated legacy SDK boundary.
///
/// Availability still comes from the same [`CatalogView`] as `/api/provider`
/// and `/api/model`; only the wire projection differs.
pub(crate) fn legacy_provider_list(state: &ApiState) -> Result<LegacyProviderList, ApiError> {
    let view = CatalogView::open(state)?;
    Ok(view.legacy_provider_list())
}

/// `GET /api/integration` — every integration and its live connections.
///
/// # Stored credentials are deliberately not read
///
/// Upstream also reports a connection for a credential in the user's auth file.
/// This handler reports only environment-derived connections: an unauthenticated
/// loopback endpoint that enumerates which providers the user has credentials for
/// is a disclosure this port is not willing to add silently, and the availability
/// decision that actually matters already consults the auth store through
/// [`Catalog::resolve`]. The gap is recorded rather than hidden.
///
/// # Errors
/// Same as [`providers`].
pub async fn integrations(
    State(state): State<ApiState>,
) -> Result<LocationEnvelope<Vec<IntegrationInfo>>, ApiError> {
    let view = CatalogView::open(&state)?;
    Ok(state.envelope(view.integrations()))
}

/// `GET /api/integration/{integrationID}` — one integration, or nothing.
///
/// Answers **200 with no `data` key** for an unknown id, which is upstream's
/// `Schema.UndefinedOr` success rather than a 404
/// (`protocol/src/groups/integration.ts:25-30`).
///
/// # Errors
/// Same as [`providers`].
pub async fn integration(
    State(state): State<ApiState>,
    PathParam(integration_id): PathParam<String>,
) -> Result<OptionalEnvelope<IntegrationInfo>, ApiError> {
    let view = CatalogView::open(&state)?;
    let found = view
        .integrations()
        .into_iter()
        .find(|candidate| candidate.id == integration_id);
    Ok(state.optional_envelope(found))
}

/// The models.dev document plus the availability decision for one request.
struct CatalogView {
    /// The loaded catalogue document.
    document: Arc<CatalogDocument>,
    /// Provider ids the resolver considers available.
    available: Vec<String>,
    /// Model ids per available provider, as the resolver left them.
    selectable: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// The environment this request was answered from.
    env: zuno_paths::Env,
}

impl CatalogView {
    /// Loads the catalogue and resolves availability.
    ///
    /// # Errors
    /// Returns [`ApiError::ConfigInvalid`] when the config tree does not parse and
    /// [`ApiError::CatalogUnavailable`] when the catalogue cannot be read from
    /// disk. Neither case is answered with an empty catalogue: a user whose
    /// config is broken must not be told they have no models.
    fn open(state: &ApiState) -> Result<Self, ApiError> {
        let resolved = super::catalog::Resolution::open(state)?;
        let document = state.models_document()?;
        let layout = zuno_paths::Layout::resolve(state.env());
        let credentials = zuno_auth::AuthStore::resolve(&layout, state.env())
            .all()
            .map(|stored| stored.entries)
            .unwrap_or_default();
        let input = zuno_llm::catalog::ResolveInput::new()
            .with_config(&resolved.config)
            .with_credentials(credentials)
            .with_env(
                state
                    .env()
                    .iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            )
            .with_experimental_models(state.env().flag(ENABLE_EXPERIMENTAL_MODELS));
        let catalog = zuno_llm::catalog::Catalog::resolve(&document, &input);
        let available = catalog
            .provider_ids()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let selectable = available
            .iter()
            .filter_map(|id| {
                catalog
                    .provider(id)
                    .map(|provider| (id.clone(), provider.models.keys().cloned().collect()))
            })
            .collect();
        Ok(Self {
            document,
            available,
            selectable,
            env: state.env().clone(),
        })
    }

    /// Every available provider, projected onto the wire shape.
    fn providers(&self) -> Vec<ProviderInfo> {
        let mut data = self
            .available
            .iter()
            .filter_map(|id| {
                self.document
                    .get(id)
                    .map(|provider| provider_info(id, provider))
            })
            .collect::<Vec<_>>();
        data.sort_by(|left, right| left.id.cmp(&right.id));
        data
    }

    /// Every selectable model, newest release first.
    ///
    /// Ties keep catalogue order, matching `Array.sortWith`'s stability
    /// (`catalog.ts:200-208`); an unstable sort here would reorder same-day
    /// releases on every request.
    fn models(&self) -> Vec<ModelInfo> {
        let mut data = Vec::new();
        for (provider_id, models) in &self.selectable {
            let Some(provider) = self.document.get(provider_id) else {
                continue;
            };
            for (model_id, model) in &provider.models {
                if !models.contains(model_id) {
                    continue;
                }
                data.push(model_info(provider_id, provider, model_id, model));
            }
        }
        data.sort_by_key(|entry| std::cmp::Reverse(entry.time.released));
        data
    }

    fn legacy_provider_list(&self) -> LegacyProviderList {
        let connected = self.available.clone();
        let all = self
            .available
            .iter()
            .filter_map(|provider_id| {
                let provider = self.document.get(provider_id)?;
                let selectable = self.selectable.get(provider_id)?;
                let models = provider
                    .models
                    .iter()
                    .filter(|(model_id, _)| selectable.contains(*model_id))
                    .map(|(model_id, model)| (model_id.clone(), legacy_model_info(model_id, model)))
                    .collect();
                Some(LegacyProviderInfo {
                    api: provider.api.clone(),
                    name: provider.name.clone(),
                    env: provider.env.clone(),
                    id: provider_id.clone(),
                    npm: provider.npm.clone(),
                    models,
                })
            })
            .collect();
        LegacyProviderList {
            all,
            default: BTreeMap::new(),
            connected,
        }
    }

    /// Every integration, name-ordered.
    fn integrations(&self) -> Vec<IntegrationInfo> {
        let mut data = Vec::new();
        for (id, provider) in self.document.as_ref() {
            if provider.env.is_empty() {
                continue;
            }
            data.push(IntegrationInfo {
                id: id.clone(),
                name: provider.name.clone(),
                methods: vec![
                    Method::Key {
                        kind: "key",
                        label: None,
                    },
                    Method::Env {
                        kind: "env",
                        names: provider.env.clone(),
                    },
                ],
                connections: provider
                    .env
                    .iter()
                    .filter(|name| {
                        self.env
                            .value(name)
                            .is_some_and(|value| !value.trim().is_empty())
                    })
                    .map(|name| Connection {
                        kind: "env",
                        name: name.clone(),
                    })
                    .collect(),
            });
        }
        for entry in PLUGIN_INTEGRATIONS {
            if data.iter().any(|existing| existing.id == entry.id) {
                continue;
            }
            data.push(IntegrationInfo {
                id: entry.id.to_owned(),
                name: entry.name.to_owned(),
                methods: entry
                    .methods
                    .iter()
                    .map(|method| match method {
                        StaticMethod::OAuth { id, label } => Method::OAuth {
                            id: (*id).to_owned(),
                            kind: "oauth",
                            label: (*label).to_owned(),
                        },
                        StaticMethod::Key { label } => Method::Key {
                            kind: "key",
                            label: label.map(ToOwned::to_owned),
                        },
                    })
                    .collect(),
                connections: Vec::new(),
            });
        }
        data.sort_by(|left, right| locale_compare(&left.name, &right.name));
        data
    }
}

/// `OPENCODE_ENABLE_EXPERIMENTAL_MODELS`, the flag that admits `alpha` models.
const ENABLE_EXPERIMENTAL_MODELS: &str = "OPENCODE_ENABLE_EXPERIMENTAL_MODELS";

/// `name.localeCompare` for ASCII names: case-insensitive first, case as the
/// tie-break.
///
/// This is load-bearing, not pedantry: `openai` and `OpenCode` differ only in
/// case at the fourth character, and a byte comparison puts `OpenCode` first
/// while the oracle puts `openai` first.
fn locale_compare(left: &str, right: &str) -> Ordering {
    let folded = left.to_lowercase().cmp(&right.to_lowercase());
    if folded == Ordering::Equal {
        left.cmp(right)
    } else {
        folded
    }
}

/// Projects one catalogue provider onto `Provider.Info`.
fn provider_info(id: &str, provider: &CatalogProvider) -> ProviderInfo {
    ProviderInfo {
        id: id.to_owned(),
        integration_id: None,
        name: provider.name.clone(),
        disabled: None,
        api: provider_api(provider),
        request: RequestBody::empty(),
    }
}

/// The provider's `api` block (`models-dev.ts:143-155`).
fn provider_api(provider: &CatalogProvider) -> ProviderApi {
    match &provider.npm {
        Some(package) => ProviderApi {
            kind: "aisdk",
            package: Some(package.clone()),
            url: provider.api.clone(),
            settings: None,
        },
        None => ProviderApi {
            kind: "native",
            package: None,
            url: provider.api.clone(),
            settings: Some(Map::new()),
        },
    }
}

/// Projects one catalogue model onto `Model.Info`, then folds the provider in.
fn model_info(
    provider_id: &str,
    provider: &CatalogProvider,
    model_id: &str,
    model: &CatalogModel,
) -> ModelInfo {
    let provider_api = provider_api(provider);
    ModelInfo {
        id: model_id.to_owned(),
        provider_id: provider_id.to_owned(),
        family: model.family.clone(),
        name: model.name.clone(),
        api: model_api(&model.id, model, &provider_api),
        capabilities: Capabilities {
            tools: model.tool_call,
            input: model
                .modalities
                .as_ref()
                .map(|modalities| modality_names(&modalities.input))
                .unwrap_or_default(),
            output: model
                .modalities
                .as_ref()
                .map(|modalities| modality_names(&modalities.output))
                .unwrap_or_default(),
        },
        request: RequestBody::empty(),
        variants: Vec::new(),
        time: ReleaseTime {
            released: released(&model.release_date),
        },
        cost: costs(model.cost.as_ref()),
        status: match model.status.unwrap_or(CatalogStatus::Active) {
            CatalogStatus::Alpha => "alpha",
            CatalogStatus::Beta => "beta",
            CatalogStatus::Deprecated => "deprecated",
            CatalogStatus::Active => "active",
        },
        enabled: true,
        limit: Limit {
            context: whole(model.limit.context),
            input: model.limit.input.map(whole),
            output: whole(model.limit.output),
        },
    }
}

fn legacy_model_info(model_id: &str, model: &CatalogModel) -> LegacyModelInfo {
    LegacyModelInfo {
        id: model_id.to_owned(),
        name: model.name.clone(),
        release_date: model.release_date.clone(),
        attachment: model.attachment,
        reasoning: model.reasoning,
        temperature: model.temperature,
        tool_call: model.tool_call,
        cost: model.cost.as_ref().map(legacy_cost),
        limit: LegacyLimit {
            context: number(model.limit.context),
            output: number(model.limit.output),
        },
        modalities: model
            .modalities
            .as_ref()
            .map(|modalities| LegacyModalities {
                input: modality_names(&modalities.input),
                output: modality_names(&modalities.output),
            }),
        status: status_name(model.status),
        options: Map::new(),
        headers: BTreeMap::new(),
        provider: model
            .provider
            .as_ref()
            .and_then(|provider| provider.npm.as_ref())
            .map(|npm| LegacyModelProvider { npm: npm.clone() }),
    }
}

fn legacy_cost(cost: &CatalogCost) -> LegacyCost {
    LegacyCost {
        input: number(cost.input),
        output: number(cost.output),
        cache_read: Some(number(cost.cache_read.unwrap_or(0.0))),
        cache_write: Some(number(cost.cache_write.unwrap_or(0.0))),
        context_over_200k: cost.context_over_200k.as_ref().map(|band| LegacyCostBand {
            input: number(band.input),
            output: number(band.output),
            cache_read: Some(number(band.cache_read.unwrap_or(0.0))),
            cache_write: Some(number(band.cache_write.unwrap_or(0.0))),
        }),
    }
}

fn status_name(status: Option<CatalogStatus>) -> &'static str {
    match status.unwrap_or(CatalogStatus::Active) {
        CatalogStatus::Alpha => "alpha",
        CatalogStatus::Beta => "beta",
        CatalogStatus::Deprecated => "deprecated",
        CatalogStatus::Active => "active",
    }
}

/// The model's `api` block after `applyModel` and `projectModel` have both run.
///
/// The native-with-no-overrides branch is `catalog.ts:80-81`: the provider's `api`
/// replaces the model's wholesale, keeping only the model's `id`.
fn model_api(model_id: &str, model: &CatalogModel, provider: &ProviderApi) -> ModelApi {
    let own = match model.provider.as_ref().and_then(|entry| entry.npm.clone()) {
        Some(package) => ModelApi {
            id: model_id.to_owned(),
            kind: "aisdk",
            package: Some(package),
            url: model.provider.as_ref().and_then(|entry| entry.api.clone()),
            settings: None,
        },
        None => ModelApi {
            id: model_id.to_owned(),
            kind: "native",
            package: None,
            url: model.provider.as_ref().and_then(|entry| entry.api.clone()),
            settings: Some(Map::new()),
        },
    };
    if own.kind == "native" && own.url.is_none() && own.settings.as_ref().is_some_and(Map::is_empty)
    {
        return ModelApi {
            id: own.id,
            kind: provider.kind,
            package: provider.package.clone(),
            url: provider.url.clone(),
            settings: provider.settings.clone(),
        };
    }
    if own.kind == "aisdk" && provider.kind == "aisdk" && own.url.is_none() {
        return ModelApi {
            url: provider.url.clone(),
            ..own
        };
    }
    own
}

/// Maps the port's modality enum onto the strings upstream emits.
fn modality_names(values: &[zuno_llm::catalog::models_dev::Modality]) -> Vec<String> {
    values
        .iter()
        .map(|value| match serde_json::to_value(value) {
            Ok(Value::String(text)) => text,
            _ => String::new(),
        })
        .collect()
}

/// `Date.parse` for the `YYYY-MM-DD` dates models.dev uses, in epoch
/// milliseconds, and `0` for anything that does not parse (`models-dev.ts:9-12`).
fn released(date: &str) -> i64 {
    let mut parts = date.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return 0;
    };
    let (Ok(year), Ok(month), Ok(day)) = (
        year.parse::<i64>(),
        month.parse::<i64>(),
        day.parse::<i64>(),
    ) else {
        return 0;
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0;
    }
    days_from_civil(year, month, day) * 86_400_000
}

/// Days since the Unix epoch for a civil date, Howard Hinnant's `days_from_civil`.
///
/// Written out rather than pulled in as a dependency: the task forbids adding one,
/// and this is the only date arithmetic the API surface needs.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Every price band for a model: the base, then declared tiers, then the
/// `context_over_200k` surcharge (`models-dev.ts:13-49`).
fn costs(cost: Option<&CatalogCost>) -> Vec<Cost> {
    let base = Cost {
        tier: None,
        input: number(cost.map_or(0.0, |entry| entry.input)),
        output: number(cost.map_or(0.0, |entry| entry.output)),
        cache: CostCache {
            read: number(cost.and_then(|entry| entry.cache_read).unwrap_or(0.0)),
            write: number(cost.and_then(|entry| entry.cache_write).unwrap_or(0.0)),
        },
    };
    let mut bands = vec![base];
    if let Some(entry) = cost {
        for tier in entry.tiers.iter().flatten() {
            bands.push(Cost {
                tier: tier.tier.as_ref().map(|bound| CostTier {
                    kind: bound.kind.clone(),
                    size: number(bound.size),
                }),
                input: number(tier.input),
                output: number(tier.output),
                cache: CostCache {
                    read: number(tier.cache_read.unwrap_or(0.0)),
                    write: number(tier.cache_write.unwrap_or(0.0)),
                },
            });
        }
        if let Some(band) = &entry.context_over_200k {
            bands.push(Cost {
                tier: Some(CostTier {
                    kind: "context".to_owned(),
                    size: Number::from(200_000),
                }),
                input: number(band.input),
                output: number(band.output),
                cache: CostCache {
                    read: number(band.cache_read.unwrap_or(0.0)),
                    write: number(band.cache_write.unwrap_or(0.0)),
                },
            });
        }
    }
    bands
}

/// A JSON number that prints as an integer when the value is whole.
///
/// `serde_json` renders `0.0_f64` as `0.0`; JavaScript renders it as `0`. Without
/// this, every zero price in the response would differ from the oracle's by two
/// characters and the differential would fail on formatting rather than on data.
fn number(value: f64) -> Number {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the guard above keeps the value inside i64 and integral"
        )]
        return Number::from(value as i64);
    }
    Number::from_f64(value).unwrap_or_else(|| Number::from(0))
}

/// A catalogue limit as the integer upstream's `Schema.Int` requires.
fn whole(value: f64) -> i64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "catalogue limits are token counts well inside i64"
    )]
    {
        value as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_dates_become_epoch_milliseconds() {
        assert_eq!(released("2025-12-01"), 1_764_547_200_000);
        assert_eq!(released("1970-01-01"), 0);
        assert_eq!(released("not-a-date"), 0);
        assert_eq!(released("2025-13-01"), 0);
    }

    #[test]
    fn whole_prices_print_as_integers() {
        assert_eq!(serde_json::to_string(&number(0.0)).unwrap(), "0");
        assert_eq!(serde_json::to_string(&number(7.0)).unwrap(), "7");
        assert_eq!(serde_json::to_string(&number(0.14)).unwrap(), "0.14");
    }

    #[test]
    fn locale_ordering_puts_lowercase_openai_before_opencode() {
        let mut names = vec!["OpenCode", "openai", "Zhipu AI", "AnyAPI"];
        names.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(names, vec!["AnyAPI", "openai", "OpenCode", "Zhipu AI"]);
    }
}
