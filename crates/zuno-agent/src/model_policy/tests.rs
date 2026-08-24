//! The policy's contract, asserted rather than reviewed.
//!
//! Two tests carry the design. [`an_agent_with_no_configured_model_inherits_the_session_model`]
//! is the inversion adopted from `omo-slim`,
//! and [`no_source_file_in_this_crate_names_a_model`] is what keeps it true: it walks
//! every non-test source file in the crate and fails on a model-id-shaped token, so a
//! future edit that reintroduces omo's `CATEGORY_MODEL_REQUIREMENTS` cannot merge
//! quietly.

use super::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use zuno_llm::effort::ResolutionSource;

/// A model id shaped like a real one, without being one.
///
/// Every test model here is spelled from this vocabulary. Real ids are deliberately
/// absent: [`no_source_file_in_this_crate_names_a_model`] scans this file's siblings,
/// and a fixture that names a shipping model would make the crate's promise depend on
/// which files the scan happens to exclude.
const LARGE: &str = "vendor/large-thinker";
const SMALL: &str = "vendor/small-fast";
const SESSION: &str = "vendor/session-default";
const ABSENT: &str = "vendor/retired-last-year";

fn session_model() -> ModelChoice {
    ModelChoice::new(SESSION)
}

/// Availability restricted to an explicit set, which is how the fallthrough is proven.
struct Only(Vec<&'static str>);

impl ModelAvailability for Only {
    fn is_available(&self, model: &ModelChoice) -> bool {
        self.0.contains(&model.model.as_str())
    }
}

fn house() -> ModelPreset {
    ModelPreset::named("house")
        .with_agent("build", ModelChoice::new(LARGE).with_variant("high"))
        .with_agent("explorer", ModelChoice::new(SMALL).with_variant("low"))
        .with_category("cheap", ModelChoice::new(SMALL))
}

fn thrifty() -> ModelPreset {
    ModelPreset::named("thrifty")
        .with_agent("build", ModelChoice::new(SMALL))
        .with_agent("explorer", ModelChoice::new(SMALL))
        .with_category("deliberate", ModelChoice::new(LARGE))
}

fn library() -> PresetLibrary {
    PresetLibrary::new()
        .with_preset(house())
        .with_preset(thrifty())
}

fn everything() -> Only {
    Only(vec![LARGE, SMALL, SESSION])
}

// ---------------------------------------------------------------------------
// Rung 3: the session model, which is the default for every agent.
// ---------------------------------------------------------------------------

#[test]
fn an_agent_with_no_configured_model_inherits_the_session_model() {
    // Slim's `DEFAULT_MODELS` is every agent mapped to `undefined`, "so agents follow
    // the global/session model" (`src/config/constants.ts:29-41`). With no preset
    // selected and no override, that is the whole policy.
    let policy = ModelPolicy::new().with_session_model(session_model());

    for agent in crate::builtin::LEAN_NAMES {
        let resolved = policy.resolve(agent, &everything());
        assert_eq!(
            resolved.model.as_ref(),
            Some(&session_model()),
            "{agent}: an unconfigured agent must run on the session model"
        );
        assert_eq!(resolved.source, ModelSource::SessionModel);
        assert!(
            resolved.inherits_session_model(),
            "{agent}: and must say so, so a caller can tell a choice from a default"
        );
        assert!(
            resolved.diagnostics.is_empty(),
            "{agent}: inheriting is the designed default, not a problem to report"
        );
    }
}

#[test]
fn every_built_in_agent_defaults_to_unset_including_the_engines_internals() {
    // The roster is the source of names, so a seventh agent is covered with no edit
    // here. Internals are included on purpose: `small_model` routing for them belongs
    // to the engine (`zuno-engine/src/compaction.rs:404`), so this module must not
    // silently invent a different answer for them.
    let policy = ModelPolicy::new();
    let resolved = policy.resolve_roster(true, &everything());

    assert_eq!(
        resolved.len(),
        crate::builtin::LEAN_NAMES.len() + crate::builtin::INTERNAL_NAMES.len()
    );
    for resolution in &resolved {
        assert_eq!(
            resolution.model, None,
            "{}: with no session model there is nothing to inherit",
            resolution.subject
        );
        assert_eq!(resolution.source, ModelSource::SessionModel);
    }

    // The vision gate is the roster's, not this module's.
    assert_eq!(
        policy.resolve_roster(false, &everything()).len(),
        resolved.len() - 1
    );
}

// ---------------------------------------------------------------------------
// Rung 2: a selected preset.
// ---------------------------------------------------------------------------

#[test]
fn a_selected_preset_overrides_the_session_model() {
    let library = library().select("house");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model());

    let orchestrator = policy.resolve("build", &everything());
    assert_eq!(
        orchestrator.model,
        Some(ModelChoice::new(LARGE).with_variant("high"))
    );
    assert_eq!(
        orchestrator.source,
        ModelSource::Preset {
            preset: "house".to_owned()
        }
    );
    assert!(!orchestrator.inherits_session_model());

    // An agent the preset is silent about still inherits. A preset is an answer for
    // the agents it names, not a requirement to name all of them.
    let advisor = policy.resolve("advisor", &everything());
    assert_eq!(advisor.model, Some(session_model()));
    assert_eq!(advisor.source, ModelSource::SessionModel);
    assert!(advisor.diagnostics.is_empty());
}

#[test]
fn switching_presets_changes_every_agent_with_no_code_change() {
    // The happy-path QA scenario. The two presets differ only as data; the resolver
    // is the same object in both halves of this test.
    let house_library = library().select("house");
    let thrifty_library = library().select("thrifty");

    let with_house = ModelPolicy::new()
        .with_library(&house_library)
        .with_session_model(session_model());
    let with_thrifty = ModelPolicy::new()
        .with_library(&thrifty_library)
        .with_session_model(session_model());

    let house_models: Vec<Option<String>> = ["build", "explorer"]
        .iter()
        .map(|agent| {
            with_house
                .resolve(agent, &everything())
                .model
                .map(|choice| choice.model)
        })
        .collect();
    let thrifty_models: Vec<Option<String>> = ["build", "explorer"]
        .iter()
        .map(|agent| {
            with_thrifty
                .resolve(agent, &everything())
                .model
                .map(|choice| choice.model)
        })
        .collect();

    assert_eq!(
        house_models,
        vec![Some(LARGE.to_owned()), Some(SMALL.to_owned())]
    );
    assert_eq!(
        thrifty_models,
        vec![Some(SMALL.to_owned()), Some(SMALL.to_owned())]
    );
    assert_ne!(house_models, thrifty_models);
}

// ---------------------------------------------------------------------------
// Rung 1: a per-agent config override.
// ---------------------------------------------------------------------------

#[test]
fn a_per_agent_override_beats_the_preset() {
    let library = library().select("house");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model())
        .with_agent_override("build", ModelChoice::new(SMALL).with_variant("max"));

    let resolved = policy.resolve("build", &everything());
    assert_eq!(
        resolved.model,
        Some(ModelChoice::new(SMALL).with_variant("max")),
        "the user's own config is the highest rung"
    );
    assert_eq!(resolved.source, ModelSource::AgentOverride);

    // And the preset still answers for the agents the override is silent about.
    assert_eq!(
        policy.resolve("explorer", &everything()).source,
        ModelSource::Preset {
            preset: "house".to_owned()
        }
    );
}

#[test]
fn overrides_come_from_the_agent_config_keys_that_already_exist() {
    // `agents.<name>.model` / `.variant` are todo 12's schema keys, already merged onto
    // built-ins by `zuno_catalog::agent::apply`. Reading the same two keys is what stops
    // a user needing a second mechanism for something they have already configured.
    let agents: OrderedMap<AgentConfig> = serde_json::from_value(json!({
        "worker": { "model": LARGE, "variant": "xhigh" },
        "explorer": { "temperature": 0.2 }
    }))
    .expect("agent config parses");

    let policy = ModelPolicy::new()
        .with_agent_overrides(&agents)
        .with_session_model(session_model());

    assert_eq!(
        policy.resolve("worker", &everything()).model,
        Some(ModelChoice::new(LARGE).with_variant("xhigh"))
    );
    // No `model` key is not an override; unrelated agent settings leave the rung silent.
    let resolved = policy.resolve("explorer", &everything());
    assert_eq!(resolved.source, ModelSource::SessionModel);
    assert_eq!(resolved.model, Some(session_model()));
}

// ---------------------------------------------------------------------------
// Categories: a preset shorthand, and nothing else.
// ---------------------------------------------------------------------------

#[test]
fn a_category_resolves_through_the_active_preset_and_not_a_built_in_table() {
    let house_library = library().select("house");
    let thrifty_library = library().select("thrifty");

    let with_house = ModelPolicy::new()
        .with_library(&house_library)
        .with_session_model(session_model());
    let with_thrifty = ModelPolicy::new()
        .with_library(&thrifty_library)
        .with_session_model(session_model());

    // `cheap` exists in one preset and not the other, and `deliberate` the other way
    // round. A built-in table could not produce that asymmetry; only preset data can.
    let cheap = with_house.resolve_category("cheap", &everything());
    assert_eq!(cheap.model, Some(ModelChoice::new(SMALL)));
    assert_eq!(
        cheap.source,
        ModelSource::Category {
            preset: "house".to_owned(),
            category: "cheap".to_owned()
        }
    );

    let deliberate = with_thrifty.resolve_category("deliberate", &everything());
    assert_eq!(deliberate.model, Some(ModelChoice::new(LARGE)));

    let missing = with_thrifty.resolve_category("cheap", &everything());
    assert_eq!(missing.source, ModelSource::SessionModel);
    assert_eq!(missing.model, Some(session_model()));

    // The category vocabulary is whatever the preset says. There is no floor and no
    // ceiling supplied by this crate.
    assert_eq!(house().categories(), vec!["cheap"]);
    assert_eq!(thrifty().categories(), vec!["deliberate"]);
    assert!(ModelPreset::named("bare").categories().is_empty());
}

#[test]
fn omos_eight_categories_are_not_built_in() {
    // The names omo hardcodes at `dist/index.js:24652`. None of them means anything
    // here unless a preset says so, which is the entire concession this module makes
    // to the category idea.
    let empty = PresetLibrary::new()
        .with_preset(ModelPreset::named("bare"))
        .select("bare");
    let policy = ModelPolicy::new()
        .with_library(&empty)
        .with_session_model(session_model());

    for category in [
        "visual-engineering",
        "ultrabrain",
        "deep",
        "artistry",
        "quick",
        "unspecified-low",
        "unspecified-high",
        "writing",
    ] {
        let resolved = policy.resolve_category(category, &everything());
        assert_eq!(
            resolved.model,
            Some(session_model()),
            "{category} must not resolve to anything this crate chose"
        );
        assert_eq!(resolved.source, ModelSource::SessionModel);
        assert_eq!(resolved.diagnostics.len(), 1, "{category}");
    }
}

// ---------------------------------------------------------------------------
// Fallthrough: a diagnostic, never an error.
// ---------------------------------------------------------------------------

#[test]
fn a_preset_naming_an_unavailable_model_falls_through_with_a_diagnostic() {
    // The plan's failure scenario, verbatim: "a preset naming an unavailable model
    // falls through to the session model with a diagnostic rather than erroring".
    let library = PresetLibrary::new()
        .with_preset(ModelPreset::named("stale").with_agent("worker", ModelChoice::new(ABSENT)))
        .select("stale");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model());

    let resolved = policy.resolve("worker", &Only(vec![SESSION]));

    assert_eq!(
        resolved.model,
        Some(session_model()),
        "the answer is still usable"
    );
    assert_eq!(resolved.source, ModelSource::SessionModel);
    assert_eq!(
        resolved.diagnostics,
        vec![Diagnostic::ModelUnavailable {
            model: ABSENT.to_owned(),
            source: ModelSource::Preset {
                preset: "stale".to_owned()
            }
        }]
    );
    assert_eq!(
        resolved.render_diagnostics(),
        vec![format!(
            "worker: `{ABSENT}` from preset `stale` is not in the resolved catalog; \
             falling through to the session model"
        )]
    );
}

#[test]
fn an_unavailable_override_falls_through_to_the_preset_before_the_session() {
    // Two skips, in ladder order, each reported. A caller reading the diagnostics can
    // see exactly which rung it lost and why.
    let library = library().select("house");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model())
        .with_agent_override("explorer", ModelChoice::new(ABSENT));

    let with_preset = policy.resolve("explorer", &Only(vec![SMALL, SESSION]));
    assert_eq!(
        with_preset.model,
        Some(ModelChoice::new(SMALL).with_variant("low"))
    );
    assert_eq!(
        with_preset.source,
        ModelSource::Preset {
            preset: "house".to_owned()
        }
    );
    assert_eq!(with_preset.diagnostics.len(), 1);

    let nothing_available = policy.resolve("explorer", &Only(vec![SESSION]));
    assert_eq!(nothing_available.model, Some(session_model()));
    assert_eq!(nothing_available.source, ModelSource::SessionModel);
    assert_eq!(
        nothing_available
            .diagnostics
            .iter()
            .map(|diagnostic| match diagnostic {
                Diagnostic::ModelUnavailable { source, .. } => source.clone(),
                other => panic!("unexpected diagnostic: {other:?}"),
            })
            .collect::<Vec<_>>(),
        vec![
            ModelSource::AgentOverride,
            ModelSource::Preset {
                preset: "house".to_owned()
            }
        ]
    );
}

#[test]
fn a_stale_preset_name_is_a_diagnostic_not_a_startup_failure() {
    // Slim hit this and chose the same way: "Missing preset → warning, continue with
    // empty preset" (`src/config/codemap.md:201`).
    let library = library().select("deleted-last-week");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model());

    let resolved = policy.resolve("build", &everything());
    assert_eq!(resolved.model, Some(session_model()));
    assert_eq!(
        resolved.diagnostics,
        vec![Diagnostic::UnknownPreset {
            selected: "deleted-last-week".to_owned(),
            available: vec!["house".to_owned(), "thrifty".to_owned()]
        }]
    );
    assert!(
        resolved.diagnostics[0]
            .to_string()
            .contains("available presets: house, thrifty"),
        "the diagnostic must name the way out"
    );
}

#[test]
fn a_bare_model_id_is_reported_rather_than_guessed_at() {
    // Choosing a provider for an unqualified id is exactly the entitlement guessing
    // `CATEGORY_MODEL_REQUIREMENTS` hardcodes as ten provider ids per rung.
    let library = PresetLibrary::new()
        .with_preset(
            ModelPreset::named("sloppy").with_agent("worker", ModelChoice::new("large-thinker")),
        )
        .select("sloppy");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model());

    let resolved = policy.resolve("worker", &AnyModel);
    assert_eq!(resolved.model, Some(session_model()));
    assert_eq!(
        resolved.diagnostics,
        vec![Diagnostic::ModelNotQualified {
            model: "large-thinker".to_owned(),
            source: ModelSource::Preset {
                preset: "sloppy".to_owned()
            }
        }]
    );
    assert!(
        resolved.diagnostics[0]
            .to_string()
            .contains("provider/model"),
        "the diagnostic must say what the id should have looked like"
    );
}

#[test]
fn the_session_model_is_never_checked_for_availability() {
    // There is nothing below it. Rejecting it would turn a working session into a
    // resolution with no model at all.
    let policy = ModelPolicy::new().with_session_model(session_model());
    let resolved = policy.resolve("worker", &NoModel);
    assert_eq!(resolved.model, Some(session_model()));
    assert!(resolved.diagnostics.is_empty());
}

// ---------------------------------------------------------------------------
// Availability against the real resolved catalog.
// ---------------------------------------------------------------------------

#[test]
fn the_resolved_catalog_answers_availability() {
    use zuno_llm::catalog::{Catalog, CatalogDocument, ResolveInput};

    // Two providers, one model each. `provider.*` blocks make both available, which is
    // the one availability source that needs no credential (`provider.ts:1588-1595`).
    let document: CatalogDocument = serde_json::from_str(
        r#"{
          "vendor": {"name":"Vendor","id":"vendor","env":["VENDOR_API_KEY"],
            "models":{"small-fast":{"id":"small-fast","name":"Small",
              "limit":{"context":1,"output":1}}}}
        }"#,
    )
    .expect("catalog fixture parses");
    let config: zuno_config::schema::Config =
        serde_json::from_str(r#"{"provider":{"vendor":{}}}"#).expect("config parses");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));

    assert!(catalog.is_available(&ModelChoice::new(SMALL)));
    assert!(!catalog.is_available(&ModelChoice::new(ABSENT)));
    assert!(
        !catalog.is_available(&ModelChoice::new("small-fast")),
        "a bare id names no provider, so it cannot be found"
    );

    let library = PresetLibrary::new()
        .with_preset(
            ModelPreset::named("mixed")
                .with_agent("worker", ModelChoice::new(ABSENT))
                .with_agent("explorer", ModelChoice::new(SMALL)),
        )
        .select("mixed");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model());

    assert_eq!(
        policy.resolve("worker", &catalog).source,
        ModelSource::SessionModel
    );
    assert_eq!(
        policy.resolve("explorer", &catalog).source,
        ModelSource::Preset {
            preset: "mixed".to_owned()
        }
    );
}

// ---------------------------------------------------------------------------
// Preset data enters through the canonical Zuno configuration schema.
// ---------------------------------------------------------------------------

#[test]
fn typed_config_builds_the_selected_preset_library() {
    let config: zuno_config::schema::Config = serde_json::from_str(&format!(
        r#"{{
          "preset": "installed",
          "presets": {{
            "installed": {{
              "agents": {{
                "build": {{ "model": "{LARGE}", "reasoning": "xhigh" }},
                "explorer": "{SMALL}"
              }},
              "categories": {{ "cheap": "{SMALL}" }}
            }}
          }}
        }}"#
    ))
    .expect("typed config parses");

    let library = PresetLibrary::from_config(&config);
    assert_eq!(library.names(), vec!["installed"]);
    assert_eq!(library.selected(), Some("installed"));

    let preset = library.active().expect("the selected preset exists");
    assert_eq!(preset.agents(), vec!["build", "explorer"]);
    assert_eq!(
        preset.agent("build"),
        Some(&ModelChoice::new(LARGE).with_variant("xhigh"))
    );
    assert_eq!(
        preset.agent("explorer"),
        Some(&ModelChoice::new(SMALL)),
        "a bare string is a model with no variant"
    );
    assert_eq!(preset.categories(), vec!["cheap"]);
    assert_eq!(preset.category("cheap"), Some(&ModelChoice::new(SMALL)));
}

#[test]
fn an_unselected_typed_preset_leaves_every_agent_on_the_session_model() {
    let config: zuno_config::schema::Config = serde_json::from_str(&format!(
        r#"{{"presets":{{"unused":{{"agents":{{"worker":"{LARGE}"}}}}}}}}"#
    ))
    .expect("typed config parses");
    let library = PresetLibrary::from_config(&config);
    assert_eq!(library.selected(), None);
    assert!(library.active().is_none());
    assert!(
        !library.is_empty(),
        "the preset exists, it is simply not chosen"
    );

    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model());
    let resolved = policy.resolve("worker", &everything());
    assert_eq!(resolved.model, Some(session_model()));
    assert!(resolved.diagnostics.is_empty());
}

// ---------------------------------------------------------------------------
// Effort: canonical levels, and deferral to the model's own variant.
// ---------------------------------------------------------------------------

#[test]
fn a_canonical_variant_resolves_through_todo_31s_levels() {
    for (level, expected) in [
        (ReasoningEffort::Off, json!({"reasoningEffort": "none"})),
        (ReasoningEffort::Low, json!({"reasoningEffort": "low"})),
        (
            ReasoningEffort::Medium,
            json!({"reasoningEffort": "medium"}),
        ),
        (ReasoningEffort::High, json!({"reasoningEffort": "high"})),
        (ReasoningEffort::Xhigh, json!({"reasoningEffort": "xhigh"})),
        (ReasoningEffort::Max, json!({"reasoningEffort": "xhigh"})),
    ] {
        assert_eq!(read_variant(level.as_str()), Variant::Effort(level));

        let (outcome, diagnostic) = resolve_variant(
            Some(level.as_str()),
            ProviderFamily::OpenAi,
            EffortCapabilities::default(),
            &BTreeMap::new(),
        );
        assert_eq!(diagnostic, None);
        let EffortOutcome::Options(resolution) = outcome else {
            panic!("a canonical level must produce options");
        };
        assert_eq!(resolution.effort, level);
        assert_eq!(resolution.source, ResolutionSource::GenericMapping);
        assert_eq!(Value::Object(resolution.options), expected);
    }
}

#[test]
fn no_variant_inherits_the_sessions_own_effort() {
    let (outcome, diagnostic) = resolve_variant(
        None,
        ProviderFamily::Anthropic,
        EffortCapabilities::default(),
        &BTreeMap::new(),
    );
    assert_eq!(outcome, EffortOutcome::Inherit);
    assert_eq!(diagnostic, None);
}

#[test]
fn a_catalog_declared_level_wins_over_the_generic_family_mapping() {
    // This is the deferral omo takes by matching a model name
    // (`dist/index.js:28824`, `{}` "so native variants take over"). The branch here is
    // taken on a catalog fact instead, so a newer model needs no new predicate — and
    // the older shape, a token budget, is still what the generic mapping produces when
    // the catalog declares nothing.
    let declared: BTreeMap<String, JsonMap> = [(
        "max".to_owned(),
        json!({"thinking": {"type": "adaptive"}, "effort": "max"})
            .as_object()
            .cloned()
            .expect("an object"),
    )]
    .into_iter()
    .collect();

    let (adaptive, _) = resolve_variant(
        Some("max"),
        ProviderFamily::Anthropic,
        EffortCapabilities::default(),
        &declared,
    );
    let EffortOutcome::Options(resolution) = adaptive else {
        panic!("a canonical level must produce options");
    };
    assert_eq!(resolution.source, ResolutionSource::DeclaredVariant);
    assert_eq!(
        Value::Object(resolution.options),
        json!({"thinking": {"type": "adaptive"}, "effort": "max"})
    );

    let (generic, _) = resolve_variant(
        Some("max"),
        ProviderFamily::Anthropic,
        EffortCapabilities::default(),
        &BTreeMap::new(),
    );
    let EffortOutcome::Options(fallback) = generic else {
        panic!("a canonical level must produce options");
    };
    assert_eq!(fallback.source, ResolutionSource::GenericMapping);
    assert!(
        fallback.options["thinking"]["budgetTokens"].is_number(),
        "with nothing declared, the budget shape is what remains"
    );

    // And the lift only claims the names that are canonical levels.
    let mixed: BTreeMap<String, JsonMap> = [
        ("high".to_owned(), JsonMap::new()),
        ("turbo".to_owned(), JsonMap::new()),
    ]
    .into_iter()
    .collect();
    let lifted = declared_variants(&mixed);
    assert!(lifted.get(ReasoningEffort::High).is_some());
    assert!(lifted.get(ReasoningEffort::Max).is_none());
}

#[test]
fn a_model_declared_variant_is_handed_back_unchanged_and_nothing_is_synthesised() {
    // Slim's own presets contain one: `variant: 'thinking'`
    // (`src/cli/providers.ts:48`). It is not a reasoning level in any provider family,
    // and inventing a mapping for it here would be a second effort policy.
    let options = json!({"reasoning": {"enabled": true}})
        .as_object()
        .cloned()
        .expect("an object");
    let declared: BTreeMap<String, JsonMap> = [("thinking".to_owned(), options.clone())]
        .into_iter()
        .collect();

    assert_eq!(
        read_variant("thinking"),
        Variant::Named("thinking".to_owned())
    );
    let (outcome, diagnostic) = resolve_variant(
        Some("thinking"),
        ProviderFamily::OpenRouter,
        EffortCapabilities::default(),
        &declared,
    );
    assert_eq!(diagnostic, None);
    assert_eq!(
        outcome,
        EffortOutcome::ModelVariant {
            variant: "thinking".to_owned(),
            options
        }
    );
}

#[test]
fn a_variant_nobody_declares_is_a_diagnostic_and_no_options() {
    let declared: BTreeMap<String, JsonMap> = [("thinking".to_owned(), JsonMap::new())]
        .into_iter()
        .collect();
    let (outcome, diagnostic) = resolve_variant(
        Some("ludicrous"),
        ProviderFamily::Bedrock,
        EffortCapabilities::default(),
        &declared,
    );

    assert_eq!(outcome, EffortOutcome::Inherit);
    let Some(diagnostic) = diagnostic else {
        panic!("an unknown variant must be reported");
    };
    assert_eq!(
        diagnostic,
        Diagnostic::UnknownVariant {
            variant: "ludicrous".to_owned(),
            declared: vec!["thinking".to_owned()]
        }
    );
    let rendered = diagnostic.to_string();
    assert!(
        rendered.contains("off, low, medium, high, xhigh, max"),
        "{rendered}"
    );
    assert!(rendered.contains("which declares: thinking"), "{rendered}");
}

#[test]
fn a_presets_variant_reaches_effort_resolution_unchanged() {
    // The two halves of this module joined up: a preset chooses a model and a variant,
    // and the variant is read by the same function every provider family shares.
    let library = library().select("house");
    let policy = ModelPolicy::new()
        .with_library(&library)
        .with_session_model(session_model());
    let resolved = policy.resolve("build", &everything());
    let choice = resolved.model.expect("the preset named a model");

    let (outcome, diagnostic) = resolve_variant(
        choice.variant.as_deref(),
        ProviderFamily::Google,
        EffortCapabilities::default(),
        &BTreeMap::new(),
    );
    assert_eq!(diagnostic, None);
    let EffortOutcome::Options(resolution) = outcome else {
        panic!("`high` is a canonical level");
    };
    assert_eq!(resolution.effort, ReasoningEffort::High);
    assert_eq!(
        Value::Object(resolution.options),
        json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}})
    );
}

// ---------------------------------------------------------------------------
// Small surfaces worth pinning.
// ---------------------------------------------------------------------------

#[test]
fn a_model_choice_splits_and_renders() {
    let bare = ModelChoice::new(SMALL);
    assert_eq!(bare.provider(), Some("vendor"));
    assert_eq!(bare.model_id(), Some("small-fast"));
    assert_eq!(bare.to_string(), SMALL);
    assert_eq!(
        bare.clone().with_variant("low").to_string(),
        format!("{SMALL} (low)")
    );

    for malformed in ["small-fast", "/small-fast", "vendor/", "/", ""] {
        let choice = ModelChoice::new(malformed);
        assert_eq!(choice.provider(), None, "{malformed}");
        assert_eq!(choice.model_id(), None, "{malformed}");
    }
    // A three-segment id keeps its tail: the provider is the first segment only.
    let nested = ModelChoice::new("vendor/family/small-fast");
    assert_eq!(nested.provider(), Some("vendor"));
    assert_eq!(nested.model_id(), Some("family/small-fast"));
}

#[test]
fn every_model_source_renders_for_a_diagnostic() {
    for source in [
        ModelSource::SessionModel,
        ModelSource::Preset {
            preset: "house".to_owned(),
        },
        ModelSource::Category {
            preset: "house".to_owned(),
            category: "cheap".to_owned(),
        },
        ModelSource::AgentOverride,
    ] {
        assert!(
            !source.to_string().is_empty(),
            "a diagnostic embeds this, so it must render"
        );
    }
}

// ---------------------------------------------------------------------------
// The guard: no model id anywhere in this crate's shipping source.
// ---------------------------------------------------------------------------

/// Files the scan excludes, and the reason it may.
///
/// A `tests.rs` module is declared `#[cfg(test)]` by its parent, so a model id inside
/// one cannot reach the binary — and the scanner's own positive-case list has to spell
/// real ids to prove it works. The exemption is verified, not assumed:
/// [`no_source_file_in_this_crate_names_a_model`] asserts each excluded file's parent
/// really does gate it.
const TEST_MODULE: &str = "tests.rs";

fn source_files(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            source_files(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn no_source_file_in_this_crate_names_a_model() {
    // The load-bearing test of todo 64. `builtin::tests` already scans the roster's
    // rendered *prose*; this scans the crate's *source*, so a model id in a struct
    // field, a `const`, or a doc comment fails too — which is where omo's
    // `AGENTS_MODEL_REQUIREMENTS` tables would land if anyone ported them.
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut files = Vec::new();
    source_files(&root, &mut files);
    files.sort();

    // A floor, because a shared `CARGO_TARGET_DIR` can hand this test binary a
    // manifest path that no longer exists, and a scan of nothing passes vacuously.
    assert!(
        files.len() >= 6,
        "scanned only {} source files under {}; the scan is looking in the wrong \
         place and would pass vacuously",
        files.len(),
        root.display()
    );

    let mut scanned = 0_usize;
    let mut excluded = Vec::new();
    for path in &files {
        let display = path
            .strip_prefix(&root)
            .unwrap_or(path.as_path())
            .display()
            .to_string();
        let source = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("{display}: {error}");
        });

        if path.file_name().is_some_and(|name| name == TEST_MODULE) {
            // `src/x/tests.rs` is declared by `src/x.rs`.
            let parent = path
                .parent()
                .map(|directory| directory.with_extension("rs"))
                .unwrap_or_else(|| path.clone());
            let declaration = std::fs::read_to_string(&parent).unwrap_or_else(|error| {
                panic!(
                    "{display}: cannot read its parent module {}: {error}",
                    parent.display()
                );
            });
            assert!(
                declaration.contains("#[cfg(test)]\nmod tests;"),
                "{display} is excluded from the model-id scan because a test module \
                 cannot reach the binary, but {} does not gate it with #[cfg(test)]",
                parent.display()
            );
            excluded.push(display);
            continue;
        }

        for (number, line) in source.lines().enumerate() {
            for token in line.split_whitespace() {
                assert!(
                    !looks_like_model_id(token),
                    "{display}:{}: `{token}` looks like a model id. Every agent \
                     inherits the session model; preset data is configuration, not \
                     source. See this module's header.",
                    number + 1
                );
            }
        }
        scanned += 1;
    }

    assert!(
        scanned >= 3,
        "only {scanned} of {} files were scanned; excluded: {excluded:?}",
        files.len()
    );
    assert!(
        files.iter().any(|path| path.ends_with("model_policy.rs")),
        "the scan must cover this module itself"
    );
}

#[test]
fn the_scanner_would_catch_a_model_id_in_this_module() {
    // Proves the guard above is not vacuous, without leaving a model id in the source
    // it scans: the needles are assembled the same way the scanner's are.
    for planted in [
        ["cl", "aude-opus-4-5"].concat(),
        ["anthropic/cl", "aude-3-5-haiku"].concat(),
        ["gp", "t-5"].concat(),
        ["ki", "mi-k3"].concat(),
        ["gl", "m-5.2"].concat(),
        ["vendor/model-", "7b"].concat(),
    ] {
        assert!(
            looks_like_model_id(&planted),
            "the scan would not have caught {planted}"
        );
    }

    // And the vocabulary this crate's own source uses is not flagged. The last three
    // are source citations, which is how every reference in this crate is written and
    // the shape the crate-wide scan discovered the predicate had to learn: two path
    // segments and a line number read as `provider/model` with digits until the colon
    // is taken into account.
    for benign in [
        "ModelChoice",
        "provider/model",
        "vendor/large-thinker",
        "preset",
        "xhigh",
        "`{model}`",
        "dist/index.js",
        "src/cli/providers.ts:11-56",
        "dist/index.js:24475",
        "session/session.ts:331-335",
    ] {
        assert!(!looks_like_model_id(benign), "{benign} is not a model id");
    }
}
