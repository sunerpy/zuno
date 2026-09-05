//! Turning JSON into a [`Config`], with the failing key path named.
//!
//! Mirrors `packages/opencode/src/config/parse.ts`: unrecognized top-level keys
//! are rejected before validation (`:40-53`), and a validation failure carries the
//! key path of the offending value (`:59-71`).
//!
//! # How the key path is recovered
//!
//! `serde_json` reports a line and column, not a path, and the crate that adds
//! paths (`serde_path_to_error`) is not among this workspace's pinned
//! dependencies. [`locate_failure`] recovers the path from what is available: it
//! removes one candidate key at a time from a copy of the document and re-runs the
//! deserializer, and the key whose removal makes the error go away is the offending
//! one. A required key cannot be removed without breaking the document for a second
//! reason, so it is instead overwritten with each of [`PROBE_VALUES`]. Recursing into
//! the key that is found produces the full path. This runs only on the failure path,
//! where an extra pass over a config-sized document costs nothing.
//!
//! The one shape it cannot pinpoint is a *required* field whose valid values are a
//! closed set — an enum such as `experimental.policies[].effect`. Neither removal
//! nor any probe value can be shown to repair the document, so the path stops at the
//! enclosing object and the deserializer's own message ("unknown variant `maybe`,
//! expected `allow` or `deny`") supplies the rest.

use crate::schema::provider;
use crate::schema::sandbox::{SandboxMode, SandboxNetworkMode};
use crate::schema::{Config, KNOWN_TOP_LEVEL_KEYS};
use serde_json::Value;
use std::path::Path;
use zuno_error::{ConfigError, ConfigIssue};

/// How deep [`locate_failure`] will descend before giving up.
const MAX_PROBE_DEPTH: usize = 64;

impl Config {
    /// Parse one config layer from JSON text.
    ///
    /// This is strict JSON. Comments and trailing commas belong to the JSONC
    /// reader in the discovery pass, not here.
    ///
    /// Deserialization runs against the **text**, not against an intermediate
    /// [`Value`], and that is load-bearing: `serde_json::Map` is a `BTreeMap` in
    /// this workspace, so a document that has passed through [`Value`] has already
    /// had its keys sorted — which would destroy the author's permission order that
    /// `packages/core/src/v1/config/permission.ts:14-16` says precedence depends on.
    pub fn from_json_str(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let value = serde_json::from_str::<Value>(text).map_err(|source| ConfigError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        reject_unknown_top_level_keys(path, &value)?;
        let config =
            serde_json::from_str::<Self>(text).map_err(|error| invalid(path, &value, &error))?;
        validate_semantics(path, config)
    }

    /// Parse one config layer from an already-decoded JSON document.
    ///
    /// Convenient, but lossy in one respect: a [`Value`] has already sorted its
    /// object keys, so the author's key order is gone. Prefer
    /// [`from_json_str`](Self::from_json_str) whenever the text is still at hand.
    pub fn from_json_value(path: &Path, value: Value) -> Result<Self, ConfigError> {
        reject_unknown_top_level_keys(path, &value)?;
        let config = serde_json::from_value::<Self>(value.clone())
            .map_err(|error| invalid(path, &value, &error))?;
        validate_semantics(path, config)
    }
}

fn validate_semantics(path: &Path, config: Config) -> Result<Config, ConfigError> {
    let mut issues = Vec::new();
    if let Some(sandbox) = &config.sandbox {
        let mode = sandbox.resolved_mode();
        let has_writable_roots = sandbox
            .writable_roots
            .as_ref()
            .is_some_and(|roots| !roots.is_empty());
        let has_protected_paths = sandbox
            .protected_paths
            .as_ref()
            .is_some_and(|paths| !paths.is_empty());

        if mode == SandboxMode::ReadOnly && has_writable_roots {
            issues.push(ConfigIssue::new(
                ["sandbox", "writableRoots"],
                "read-only mode cannot grant writable roots",
            ));
        }
        if mode == SandboxMode::DangerFullAccess {
            if sandbox.network == Some(SandboxNetworkMode::Deny) {
                issues.push(ConfigIssue::new(
                    ["sandbox", "network"],
                    "danger-full-access inherits the host network and cannot enforce network deny",
                ));
            }
            if has_writable_roots {
                issues.push(ConfigIssue::new(
                    ["sandbox", "writableRoots"],
                    "danger-full-access already has host write authority; writableRoots would be misleading",
                ));
            }
            if has_protected_paths {
                issues.push(ConfigIssue::new(
                    ["sandbox", "protectedPaths"],
                    "danger-full-access cannot enforce protectedPaths",
                ));
            }
        }
    }

    if let Some(learning) = &config.learning {
        let resolved = learning.resolved();
        if resolved.enabled && resolved.extractor_model.is_none() {
            issues.push(ConfigIssue::new(
                ["learning", "extractor_model"],
                "a non-empty extractor_model is required when learning.enabled is true",
            ));
        }
        if !resolved.skill_require_review {
            issues.push(ConfigIssue::new(
                ["learning", "skill", "require_review"],
                "Skill candidates always require human review",
            ));
        }
        if resolved.skill_max_learned_rules
            > crate::schema::DEFAULT_LEARNING_SKILL_MAX_LEARNED_RULES
        {
            issues.push(ConfigIssue::new(
                ["learning", "skill", "max_learned_rules"],
                "a Skill may contain at most 15 learned rules",
            ));
        }
    }

    if let Some(providers) = &config.provider {
        for (id, provider) in providers.iter() {
            validate_reasoning_replay(id, provider, &mut issues);
        }
    }

    if issues.is_empty() {
        Ok(config)
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            issues,
        })
    }
}

/// Reject the encrypted-replay routing this layer can *prove* wrong, and nothing else.
///
/// Sealed reasoning only exists on an OpenAI Responses request, so a declaration that
/// sends the request somewhere else is a session that asks for envelopes, never gets
/// them, and says nothing about it. Three shapes are provably wrong, and each is
/// reported against the key the author would have to change:
///
/// 1. A declared transport other than `openai`. `openai-compatible` resolves its own
///    surface from provider-id rules and never sees a declared `surface`, and no other
///    transport has a Responses endpoint at all.
/// 2. A declared surface other than `responses`, at either level. A model's
///    `provider.surface` overrides the provider's, so the model is where the failure
///    would land and the model is what the issue names.
/// 3. A custom endpoint (`options.baseURL` or `options.endpoint`) with no declared
///    surface anywhere: `openai_surface` answers Chat as soon as a provider option
///    carries an endpoint, so the omission is not neutral.
///
/// A provider-level declaration that every configured model overrides governs nothing
/// and is not reported; the model that would actually fail is.
///
/// What is deliberately *not* rejected is silence with no custom endpoint. The catalog
/// `openai` provider infers its transport and keeps the adapter's default surface,
/// which is Responses — the official Responses API is the first endpoint this feature
/// targets, and demanding two redundant declarations there would reject a
/// configuration that works end to end. A provider whose base URL comes from `api`
/// rather than an option also keeps the Responses default, and is left alone too.
fn validate_reasoning_replay(
    id: &str,
    entry: &provider::ProviderConfig,
    issues: &mut Vec<ConfigIssue>,
) {
    let options = entry.options.as_ref();
    let provider_mode = options.and_then(|options| options.reasoning_replay);
    let custom_endpoint = options.is_some_and(|options| {
        options.base_url.as_ref().is_some_and(|url| !url.is_empty())
            || options
                .extra
                .get("endpoint")
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty())
    });

    // A provider-level declaration only decides anything for models that do not
    // override it. When every configured model names its own surface or transport, the
    // provider default governs nothing and cannot be the proven fault.
    let (surface_governs, transport_governs) = match &entry.models {
        None => (true, true),
        Some(models) if models.iter().next().is_none() => (true, true),
        Some(models) => (
            models.iter().any(|(_, model)| {
                model
                    .provider
                    .as_ref()
                    .and_then(|routing| routing.surface)
                    .is_none()
            }),
            models.iter().any(|(_, model)| {
                model
                    .provider
                    .as_ref()
                    .and_then(|routing| routing.transport)
                    .is_none()
            }),
        ),
    };

    check_reasoning_replay_routing(
        provider_mode,
        Routing {
            transport: entry.transport,
            surface: entry.surface,
            custom_endpoint,
            surface_governs,
            transport_governs,
        },
        &["provider", id, "options", "reasoningReplay"],
        issues,
    );
    if options.is_some_and(|options| options.reasoning_replay_max_age.is_some())
        && provider_mode != Some(provider::ReasoningReplay::Encrypted)
    {
        issues.push(ConfigIssue::new(
            ["provider", id, "options", "reasoningReplayMaxAge"],
            "reasoningReplayMaxAge requires reasoningReplay: \"encrypted\"",
        ));
    }

    let Some(models) = &entry.models else {
        return;
    };
    for (model_id, model) in models.iter() {
        // A model's own options overlay the provider's, so its mode is its own value
        // when it names one and the provider's otherwise.
        let declared = model
            .options
            .as_ref()
            .and_then(|options| options.get("reasoningReplay"));
        let mode = match declared {
            None => provider_mode,
            Some(value) => match value.as_str() {
                Some("encrypted") => Some(provider::ReasoningReplay::Encrypted),
                Some("off") => Some(provider::ReasoningReplay::Off),
                _ => {
                    issues.push(ConfigIssue::new(
                        [
                            "provider",
                            id,
                            "models",
                            model_id,
                            "options",
                            "reasoningReplay",
                        ],
                        "reasoningReplay must be \"off\" or \"encrypted\"",
                    ));
                    continue;
                }
            },
        };
        let routing = model.provider.as_ref();
        check_reasoning_replay_routing(
            mode,
            Routing {
                transport: routing
                    .and_then(|routing| routing.transport)
                    .or(entry.transport),
                surface: routing
                    .and_then(|routing| routing.surface)
                    .or(entry.surface),
                custom_endpoint,
                surface_governs: true,
                transport_governs: true,
            },
            &["provider", id, "models", model_id],
            issues,
        );
    }
}

/// Where one model's requests would actually go.
struct Routing {
    transport: Option<provider::ProviderTransport>,
    surface: Option<provider::ProviderSurface>,
    custom_endpoint: bool,
    /// Whether this `surface` decides anything, or is overridden by every model.
    surface_governs: bool,
    /// Whether this `transport` decides anything, or is overridden by every model.
    transport_governs: bool,
}

/// Push one issue per declaration that provably sends sealed reasoning nowhere.
fn check_reasoning_replay_routing(
    mode: Option<provider::ReasoningReplay>,
    routing: Routing,
    path: &[&str],
    issues: &mut Vec<ConfigIssue>,
) {
    if mode != Some(provider::ReasoningReplay::Encrypted) {
        return;
    }
    let transport = routing.transport.filter(|_| routing.transport_governs);
    let forces_responses = matches!(
        transport,
        Some(
            provider::ProviderTransport::BedrockMantle
                | provider::ProviderTransport::BedrockRuntime
        )
    );
    match transport {
        Some(
            provider::ProviderTransport::Openai
            | provider::ProviderTransport::BedrockMantle
            | provider::ProviderTransport::BedrockRuntime,
        )
        | None => {}
        Some(other) => issues.push(ConfigIssue::new(
            path.iter().copied(),
            format!(
                "encrypted reasoning replay requires an OpenAI Responses wire protocol; \
                 transport \"{other}\" cannot carry it, set transport \"openai\", \
                 \"bedrock-mantle\", or \"bedrock-runtime\""
            ),
        )),
    }
    if !routing.surface_governs {
        return;
    }
    match routing.surface {
        Some(provider::ProviderSurface::Responses) => {}
        Some(_) => issues.push(ConfigIssue::new(
            path.iter().copied(),
            "encrypted reasoning replay requires surface \"responses\"",
        )),
        None if routing.custom_endpoint && !forces_responses => issues.push(ConfigIssue::new(
            path.iter().copied(),
            "an OpenAI provider with a custom endpoint and no declared surface resolves \
             to Chat Completions; set surface \"responses\" for encrypted reasoning replay",
        )),
        None => {}
    }
}

/// Report one issue per unknown top-level key so a fixer can act on each.
fn reject_unknown_top_level_keys(path: &Path, value: &Value) -> Result<(), ConfigError> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let issues: Vec<ConfigIssue> = object
        .keys()
        .filter(|key| !KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()))
        .map(|key| ConfigIssue::new([key.as_str()], "unrecognized key"))
        .collect();
    if issues.is_empty() {
        return Ok(());
    }
    Err(ConfigError::Invalid {
        path: path.to_path_buf(),
        issues,
    })
}

fn invalid(path: &Path, value: &Value, error: &serde_json::Error) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_path_buf(),
        issues: vec![ConfigIssue::new(locate_failure(value), error.to_string())],
    }
}

/// One hop of a key path: an object key, or an array index.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

impl Step {
    fn render(&self) -> String {
        match self {
            Self::Key(key) => key.clone(),
            Self::Index(index) => index.to_string(),
        }
    }
}

/// The key path of the value that makes `root` fail to deserialize.
///
/// Returns the deepest path it can prove. An empty path means the failure is the
/// document itself — it is not an object, or no single child accounts for it.
fn locate_failure(root: &Value) -> Vec<String> {
    let mut path: Vec<Step> = Vec::new();
    while path.len() < MAX_PROBE_DEPTH {
        let Some(node) = value_at(root, &path) else {
            break;
        };
        let Some(culprit) = children(node)
            .into_iter()
            .find(|step| is_culprit(root, &path, step))
        else {
            break;
        };
        path.push(culprit);
    }
    path.iter().map(Step::render).collect()
}

/// Values substituted for a child to test whether that child alone is at fault.
///
/// Removal alone cannot implicate a *required* field, because removing it leaves
/// the document failing for a new reason. Substituting a value of every JSON shape
/// covers that case: if any of these makes the whole document valid, the child was
/// the only problem. A false positive is impossible — the document has to pass.
const PROBE_VALUES: &[fn() -> Value] = &[
    || Value::from(0),
    || Value::from(""),
    || Value::from(false),
    || Value::Object(serde_json::Map::new()),
    || Value::Array(Vec::new()),
];

/// Whether `step`, under the node at `path`, is what makes `root` fail.
fn is_culprit(root: &Value, path: &[Step], step: &Step) -> bool {
    let mut probe = root.clone();
    if remove_at(&mut probe, path, step) && parses(&probe) {
        return true;
    }
    PROBE_VALUES.iter().any(|make| {
        let mut probe = root.clone();
        replace_at(&mut probe, path, step, make()) && parses(&probe)
    })
}

fn parses(value: &Value) -> bool {
    serde_json::from_value::<Config>(value.clone()).is_ok()
}

/// The children of `node` that are worth probing.
fn children(node: &Value) -> Vec<Step> {
    match node {
        Value::Object(object) => object.keys().cloned().map(Step::Key).collect(),
        Value::Array(items) => (0..items.len()).map(Step::Index).collect(),
        _ => Vec::new(),
    }
}

fn value_at<'a>(root: &'a Value, path: &[Step]) -> Option<&'a Value> {
    let mut node = root;
    for step in path {
        node = match (step, node) {
            (Step::Key(key), Value::Object(object)) => object.get(key)?,
            (Step::Index(index), Value::Array(items)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(node)
}

/// Remove `step` from the node at `path`. Returns whether anything was removed.
fn remove_at(root: &mut Value, path: &[Step], step: &Step) -> bool {
    match (step, node_at_mut(root, path)) {
        (Step::Key(key), Some(Value::Object(object))) => object.remove(key).is_some(),
        (Step::Index(index), Some(Value::Array(items))) if *index < items.len() => {
            items.remove(*index);
            true
        }
        _ => false,
    }
}

/// Overwrite `step` under the node at `path`. Returns whether anything was written.
fn replace_at(root: &mut Value, path: &[Step], step: &Step, value: Value) -> bool {
    match (step, node_at_mut(root, path)) {
        (Step::Key(key), Some(Value::Object(object))) => {
            object.insert(key.clone(), value).is_some()
        }
        (Step::Index(index), Some(Value::Array(items))) => match items.get_mut(*index) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        },
        _ => false,
    }
}

fn node_at_mut<'a>(root: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    let mut node = root;
    for hop in path {
        node = match (hop, node) {
            (Step::Key(key), Value::Object(object)) => object.get_mut(key)?,
            (Step::Index(index), Value::Array(items)) => items.get_mut(*index)?,
            _ => return None,
        };
    }
    Some(node)
}
