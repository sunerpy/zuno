use super::*;
use oc_agent::model_policy::ModelPreset;
use oc_tool::{AllowAll, DenyAll, NeverInterrupted, PermissionAsker, erase};
use std::sync::Mutex;

const PARENT: &str = "ses_parent";
const MODEL_A: &str = "acme/reasoner";
const MODEL_B: &str = "acme/other-reasoner";
const MODEL_MUTE: &str = "acme/no-reasoning";

fn params(prompt: &str) -> TaskParams {
    TaskParams {
        prompt: prompt.to_owned(),
        ..TaskParams::default()
    }
}

fn to_explorer() -> TaskParams {
    TaskParams {
        subagent_type: Some("explorer".to_owned()),
        ..params("look around")
    }
}

fn facts() -> Arc<FixedFacts> {
    Arc::new(
        FixedFacts::new()
            .with_reasoning(MODEL_A, ProviderFamily::OpenAi)
            .with_reasoning(MODEL_B, ProviderFamily::OpenAi)
            .without_reasoning(MODEL_MUTE, ProviderFamily::OpenAi),
    )
}

fn tool(host: Arc<RecordingHost>) -> TaskTool {
    TaskTool::new(host, facts()).with_session_model(ModelChoice::new(MODEL_A))
}

fn context(permission: Arc<dyn PermissionAsker>) -> ToolContext {
    ToolContext::new(
        PARENT,
        "msg_1",
        "call_1",
        COORDINATOR,
        permission,
        Arc::new(NeverInterrupted),
    )
}

fn allowed() -> ToolContext {
    context(Arc::new(AllowAll))
}

/// Records the ask so the guidance travelling on a refusal is assertable — see
/// [`denial_guidance`] for why the text cannot ride on [`ToolError::Denied`].
#[derive(Default)]
struct RecordingDenier(Mutex<Vec<PermissionAsk>>);

#[async_trait]
impl PermissionAsker for RecordingDenier {
    async fn ask(&self, tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ask);
        Err(ToolError::Denied {
            tool: tool.to_owned(),
        })
    }
}

#[derive(Default)]
struct RecordingAllower(Mutex<Vec<PermissionAsk>>);

#[async_trait]
impl PermissionAsker for RecordingAllower {
    async fn ask(&self, _tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ask);
        Ok(())
    }
}

fn message(error: &ToolError) -> String {
    match error {
        ToolError::InvalidArgs { source, .. } | ToolError::Failed { source, .. } => {
            source.to_string()
        }
        other => other.to_string(),
    }
}

// -- rejection 1: neither target ---------------------------------------------

#[tokio::test]
async fn neither_target_is_refused_with_both_ways_to_fix_it() {
    let error = tool(Arc::new(RecordingHost::new()))
        .run(params("do a thing"), allowed())
        .await
        .expect_err("a delegation with no target cannot run");

    let text = message(&error);
    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    assert!(
        text.contains("Must provide either `category` or `subagent_type`"),
        "{text}"
    );
    assert!(text.contains("subagent_type=\"worker\""), "{text}");
    assert!(text.contains("category=\"<preset shorthand>\""), "{text}");
    for target in valid_targets(false) {
        assert!(text.contains(&target), "the fix must list {target}: {text}");
    }
}

// -- rejection 2: both targets ----------------------------------------------

#[tokio::test]
async fn both_targets_are_refused_as_mutually_exclusive() {
    let error = tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                subagent_type: Some("explorer".to_owned()),
                category: Some("cheap".to_owned()),
                ..params("do a thing")
            },
            allowed(),
        )
        .await
        .expect_err("naming both a category and an agent cannot run");

    let text = message(&error);
    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    assert!(text.contains("mutually exclusive"), "{text}");
    assert!(text.contains("Provide only one"), "{text}");
    assert!(text.contains("keep `subagent_type=\"explorer\"`"), "{text}");
    assert!(text.contains("keep `category=\"cheap\"`"), "{text}");
}

// -- rejection 3: a coordinator as target -----------------------------------

#[tokio::test]
async fn the_coordinator_is_refused_and_the_message_lists_the_valid_targets() {
    let error = tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                subagent_type: Some(COORDINATOR.to_owned()),
                ..params("coordinate yourself")
            },
            allowed(),
        )
        .await
        .expect_err("a coordinator cannot be a delegation target");

    let text = message(&error);
    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    assert!(
        text.contains("`orchestrator` coordinates delegations"),
        "{text}"
    );
    assert!(text.contains("Set `subagent_type` to one of"), "{text}");
    let targets = valid_targets(false);
    assert!(!targets.is_empty(), "the roster must offer some target");
    for target in &targets {
        assert!(text.contains(target), "must list {target}: {text}");
    }
    assert!(
        !targets.iter().any(|name| name == COORDINATOR),
        "the roster itself must exclude the coordinator, not just this message"
    );
}

#[tokio::test]
async fn an_unknown_agent_is_told_which_agents_exist_and_about_category() {
    let error = tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                subagent_type: Some("cheap".to_owned()),
                ..params("do a thing")
            },
            allowed(),
        )
        .await
        .expect_err("an agent outside the roster cannot be targeted");

    let text = message(&error);
    assert!(text.contains("Unknown agent `cheap`"), "{text}");
    assert!(text.contains("category=\"cheap\""), "{text}");
    for target in valid_targets(false) {
        assert!(text.contains(&target), "must list {target}: {text}");
    }
}

// -- rejection 4: depth ------------------------------------------------------

#[tokio::test]
async fn depth_exceeded_names_the_config_key_and_is_not_model_correctable() {
    let error = tool(Arc::new(RecordingHost::new().at_depth(1)))
        .run(to_explorer(), allowed())
        .await
        .expect_err("a child session may not delegate at the default bound");

    let text = message(&error);
    assert!(text.contains("Subagent depth limit reached"), "{text}");
    assert!(text.contains("`subagent_depth` is 1"), "{text}");
    assert!(text.contains("raise `subagent_depth` in config"), "{text}");
    assert!(
        !error.is_model_correctable(),
        "reissuing the identical call cannot fix a depth limit"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn composition_depth_bounds_delegation_even_when_the_session_is_at_the_root() {
    let mut ctx = allowed();
    ctx.depth = 1;

    let error = tool(Arc::new(RecordingHost::new()))
        .run(to_explorer(), ctx)
        .await
        .expect_err("a task composed inside another tool call is already one hop deep");

    assert!(message(&error).contains("Subagent depth limit reached"));
}

#[tokio::test]
async fn a_raised_bound_permits_exactly_one_more_hop() {
    let host = Arc::new(RecordingHost::new().at_depth(1));
    let output = tool(Arc::clone(&host))
        .with_limits(DelegationLimits { subagent_depth: 2 })
        .run(to_explorer(), allowed())
        .await
        .expect("depth 1 is inside a bound of 2");

    assert!(output.output.contains("<task_result>"));
    assert_eq!(host.dispatched().len(), 1);
}

// -- rejection 5: permission ------------------------------------------------

#[tokio::test]
async fn a_permission_refusal_stays_denied_and_carries_guidance_naming_the_grant() {
    let asker = Arc::new(RecordingDenier::default());
    let host = Arc::new(RecordingHost::new());
    let error = tool(Arc::clone(&host))
        .run(
            to_explorer(),
            context(Arc::clone(&asker) as Arc<dyn PermissionAsker>),
        )
        .await
        .expect_err("a refused delegation cannot run");

    assert!(matches!(error, ToolError::Denied { .. }));
    assert_eq!(error.tool(), WIRE_ID);
    assert!(
        !error.is_model_correctable(),
        "a denial needs a grant, not a corrected call"
    );
    assert!(
        host.dispatched().is_empty(),
        "no child session may exist after a refusal"
    );

    let asked = asker
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ask = asked.first().expect("the gate must have been consulted");
    let guidance = ask.metadata[GUIDANCE_KEY]
        .as_str()
        .expect("guidance is a string");
    assert!(
        guidance.contains("is not permitted for `explorer`"),
        "{guidance}"
    );
    assert!(
        guidance.contains("Grant `task` for pattern `explorer`"),
        "{guidance}"
    );
    assert!(guidance.contains("librarian"), "{guidance}");
}

#[tokio::test]
async fn the_permission_pattern_is_the_subagent_type() {
    let asker = Arc::new(RecordingAllower::default());
    tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                subagent_type: Some("librarian".to_owned()),
                description: Some("read the docs".to_owned()),
                ..params("find the spec")
            },
            context(Arc::clone(&asker) as Arc<dyn PermissionAsker>),
        )
        .await
        .expect("an allowed delegation runs");

    let asked = asker
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ask = asked.first().expect("the gate must have been consulted");
    assert_eq!(ask.permission, PERMISSION_KEY);
    assert_eq!(ask.patterns, vec!["librarian".to_owned()]);
    assert_eq!(ask.always, vec!["*".to_owned()]);
    assert_eq!(ask.metadata["subagent_type"], "librarian");
    assert_eq!(ask.metadata["description"], "read the docs");
}

#[tokio::test]
async fn a_category_call_gates_on_the_agent_it_actually_runs() {
    let asker = Arc::new(RecordingAllower::default());
    tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                category: Some("cheap".to_owned()),
                ..params("mechanical edit")
            },
            context(Arc::clone(&asker) as Arc<dyn PermissionAsker>),
        )
        .await
        .expect("a category delegation runs");

    let asked = asker
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        asked[0].patterns,
        vec![GENERIC_EXECUTOR.to_owned()],
        "a rule cannot match a pattern no agent is named by"
    );
}

// -- the dropped argument ---------------------------------------------------

#[tokio::test]
async fn load_skills_is_refused_and_points_at_per_agent_permissions() {
    let host = Arc::new(RecordingHost::new());
    let error = tool(Arc::clone(&host))
        .run(
            TaskParams {
                load_skills: Some(Value::Array(Vec::new())),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect_err("load_skills is not a parameter of this tool");

    let text = message(&error);
    assert!(text.contains("`load_skills` is not a parameter"), "{text}");
    assert!(text.contains("permission-gated per agent"), "{text}");
    assert!(text.contains("choose the `subagent_type`"), "{text}");
    assert!(
        host.dispatched().is_empty(),
        "silently ignoring it would let the caller believe a skill was loaded"
    );
}

#[test]
fn the_advertised_schema_never_mentions_load_skills() {
    let definition = erase(tool(Arc::new(RecordingHost::new()))).definition();
    let properties = &definition.parameters["properties"];

    assert!(properties.get("load_skills").is_none());
    assert!(!definition.description.contains("load_skills"));
    for expected in [
        "description",
        "prompt",
        "subagent_type",
        "category",
        "model",
        "effort",
        "background",
        "task_id",
    ] {
        assert!(
            properties.get(expected).is_some(),
            "{expected} must be advertised"
        );
    }
    assert_eq!(properties["prompt"]["type"], "string");
}

#[test]
fn an_unknown_argument_is_still_refused() {
    let rendered = format!(
        "{:?}",
        serde_json::from_value::<TaskParams>(serde_json::json!({
            "prompt": "x",
            "run_in_background": true,
        }))
    );

    assert!(rendered.contains("run_in_background"), "{rendered}");
}

// -- targets come from the roster ------------------------------------------

#[test]
fn valid_targets_are_the_rosters_and_exclude_the_coordinator() {
    let without_vision = valid_targets(false);
    let with_vision = valid_targets(true);

    assert!(!without_vision.contains(&COORDINATOR.to_owned()));
    assert!(without_vision.contains(&GENERIC_EXECUTOR.to_owned()));
    assert!(
        with_vision.len() > without_vision.len(),
        "a vision-capable catalog adds a target: {with_vision:?}"
    );
    assert_eq!(
        without_vision,
        delegable(false)
            .into_iter()
            .map(|agent| agent.name.to_owned())
            .collect::<Vec<_>>(),
        "the list must be the roster's, not a copy of it"
    );
}

#[tokio::test]
async fn a_vision_gated_target_is_unreachable_until_the_catalog_offers_one() {
    let looker = valid_targets(true)
        .into_iter()
        .find(|name| !valid_targets(false).contains(name))
        .expect("one target is capability-gated");

    let refused = tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                subagent_type: Some(looker.clone()),
                ..params("describe the screenshot")
            },
            allowed(),
        )
        .await
        .expect_err("the gated target is absent without a vision model");
    assert!(message(&refused).contains(&format!("Unknown agent `{looker}`")));

    tool(Arc::new(RecordingHost::new()))
        .with_vision_available(true)
        .run(
            TaskParams {
                subagent_type: Some(looker),
                ..params("describe the screenshot")
            },
            allowed(),
        )
        .await
        .expect("the gated target is present with one");
}

// -- the precedence ladder -------------------------------------------------

#[test]
fn the_call_argument_outranks_the_config_override_the_preset_and_the_session() {
    let presets = PresetLibrary::new()
        .with_preset(ModelPreset::named("p").with_agent("explorer", ModelChoice::new(MODEL_B)))
        .select("p");
    let subject = TaskTool::new(Arc::new(RecordingHost::new()), facts())
        .with_presets(presets)
        .with_session_model(ModelChoice::new(MODEL_A))
        .with_agent_override("explorer", ModelChoice::new(MODEL_B));

    let plan = subject.plan(
        "explorer",
        None,
        &TaskParams {
            model: Some(MODEL_A.to_owned()),
            ..to_explorer()
        },
    );

    assert_eq!(plan.model, Some(ModelChoice::new(MODEL_A)));
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
}

#[test]
fn without_a_call_argument_the_lower_rungs_still_decide() {
    let presets = PresetLibrary::new()
        .with_preset(ModelPreset::named("p").with_agent("explorer", ModelChoice::new(MODEL_B)))
        .select("p");
    let subject = TaskTool::new(Arc::new(RecordingHost::new()), facts())
        .with_presets(presets)
        .with_session_model(ModelChoice::new(MODEL_A));

    assert_eq!(
        subject.plan("explorer", None, &to_explorer()).model,
        Some(ModelChoice::new(MODEL_B)),
        "the preset rung must still answer"
    );
    assert_eq!(
        subject.plan("worker", None, &to_explorer()).model,
        Some(ModelChoice::new(MODEL_A)),
        "an agent the preset is silent about falls to the session model"
    );
}

#[test]
fn a_category_resolves_through_the_active_preset_and_runs_the_generic_executor() {
    let presets = PresetLibrary::new()
        .with_preset(ModelPreset::named("p").with_category("cheap", ModelChoice::new(MODEL_B)))
        .select("p");
    let subject = TaskTool::new(Arc::new(RecordingHost::new()), facts())
        .with_presets(presets)
        .with_session_model(ModelChoice::new(MODEL_A));

    let plan = subject.plan(GENERIC_EXECUTOR, Some("cheap"), &params("mechanical"));

    assert_eq!(plan.agent, GENERIC_EXECUTOR);
    assert_eq!(plan.model, Some(ModelChoice::new(MODEL_B)));
    assert_eq!(plan.category.as_deref(), Some("cheap"));
}

#[test]
fn an_unknown_category_is_a_note_and_the_session_model_not_a_failure() {
    let presets = PresetLibrary::new()
        .with_preset(ModelPreset::named("p"))
        .select("p");
    let subject = TaskTool::new(Arc::new(RecordingHost::new()), facts())
        .with_presets(presets)
        .with_session_model(ModelChoice::new(MODEL_A));

    let plan = subject.plan(GENERIC_EXECUTOR, Some("nope"), &params("mechanical"));

    assert_eq!(plan.model, Some(ModelChoice::new(MODEL_A)));
    assert!(
        plan.notes
            .iter()
            .any(|note| note.contains("no category `nope`")),
        "{:?}",
        plan.notes
    );
}

// -- what a provider cannot honour ----------------------------------------

#[test]
fn an_explicit_effort_becomes_the_childs_outbound_provider_options() {
    let plan = tool(Arc::new(RecordingHost::new())).plan(
        "explorer",
        None,
        &TaskParams {
            model: Some(MODEL_A.to_owned()),
            effort: Some("low".to_owned()),
            ..to_explorer()
        },
    );

    assert_eq!(plan.effort, Some(ReasoningEffort::Low));
    assert_eq!(plan.provider_options["reasoningEffort"], "low");
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
}

#[test]
fn an_unavailable_explicit_model_falls_through_and_says_so() {
    let plan = tool(Arc::new(RecordingHost::new())).plan(
        "explorer",
        None,
        &TaskParams {
            model: Some("acme/retired".to_owned()),
            ..to_explorer()
        },
    );

    assert_eq!(plan.model, Some(ModelChoice::new(MODEL_A)));
    assert!(
        plan.notes
            .iter()
            .any(|note| note.contains("`acme/retired`") && note.contains("resolved catalog")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn an_unqualified_explicit_model_falls_through_and_says_so() {
    let plan = tool(Arc::new(RecordingHost::new())).plan(
        "explorer",
        None,
        &TaskParams {
            model: Some("reasoner".to_owned()),
            ..to_explorer()
        },
    );

    assert_eq!(plan.model, Some(ModelChoice::new(MODEL_A)));
    assert!(
        plan.notes
            .iter()
            .any(|note| note.contains("`provider/model` form")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn an_effort_a_non_reasoning_model_cannot_honour_is_reported_not_silently_dropped() {
    let plan = tool(Arc::new(RecordingHost::new())).plan(
        "explorer",
        None,
        &TaskParams {
            model: Some(MODEL_MUTE.to_owned()),
            effort: Some("high".to_owned()),
            ..to_explorer()
        },
    );

    assert_eq!(plan.effort, None);
    assert!(plan.provider_options.is_empty());
    assert!(
        plan.notes
            .iter()
            .any(|note| note.contains("produces no reasoning") && note.contains("drop `effort`")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn an_effort_name_the_model_does_not_declare_is_reported() {
    let plan = tool(Arc::new(RecordingHost::new())).plan(
        "explorer",
        None,
        &TaskParams {
            model: Some(MODEL_A.to_owned()),
            effort: Some("ponder".to_owned()),
            ..to_explorer()
        },
    );

    assert_eq!(plan.effort, None);
    assert!(
        plan.notes
            .iter()
            .any(|note| note.contains("`ponder`") && note.contains("canonical reasoning level")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn a_model_declared_variant_is_passed_through_verbatim() {
    let mut variants = BTreeMap::new();
    let mut options = Map::new();
    options.insert("thinking".to_owned(), Value::Bool(true));
    variants.insert("ponder".to_owned(), options.clone());
    let facts = Arc::new(FixedFacts::new().with(
        MODEL_A,
        ModelFacts {
            family: ProviderFamily::OpenAi,
            reasoning: true,
            effort: EffortCapabilities::default(),
            variants,
        },
    ));

    let plan = TaskTool::new(Arc::new(RecordingHost::new()), facts)
        .with_session_model(ModelChoice::new(MODEL_A))
        .plan(
            "explorer",
            None,
            &TaskParams {
                effort: Some("ponder".to_owned()),
                ..to_explorer()
            },
        );

    assert_eq!(plan.provider_options, options);
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
}

#[test]
fn an_effort_with_no_resolvable_model_is_reported() {
    let plan = TaskTool::new(Arc::new(RecordingHost::new()), Arc::new(NoProviders)).plan(
        "explorer",
        None,
        &TaskParams {
            effort: Some("low".to_owned()),
            ..to_explorer()
        },
    );

    assert_eq!(plan.model, None);
    assert!(
        plan.notes
            .iter()
            .any(|note| note.contains("no model resolved for this delegation")),
        "{:?}",
        plan.notes
    );
}

// -- the two ids ----------------------------------------------------------

#[tokio::test]
async fn a_foreground_delegation_returns_the_child_session_id_and_no_job_id() {
    let host = Arc::new(RecordingHost::new());
    let output = tool(Arc::clone(&host))
        .run(to_explorer(), allowed())
        .await
        .expect("a foreground delegation runs");

    let child = format!("ses_child_of_{PARENT}");
    assert!(output.output.contains(&format!("<task id=\"{child}\"")));
    assert!(output.output.contains("state=\"completed\""));
    assert!(!output.output.contains("background="));
    assert_eq!(host.dispatched()[0].parent_session_id, PARENT);
    assert!(!host.dispatched()[0].background);
}

#[tokio::test]
async fn a_background_dispatch_reports_a_job_id_distinct_from_the_session_id() {
    let host = Arc::new(RecordingHost::new());
    let output = tool(Arc::clone(&host))
        .run(
            TaskParams {
                background: Some(true),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("a background delegation runs");

    let child = format!("ses_child_of_{PARENT}");
    let job = background_id(&child);
    assert_ne!(job, child);
    assert!(output.output.contains(&format!("<task id=\"{child}\"")));
    assert!(output.output.contains(&format!("background=\"{job}\"")));
    assert!(output.output.contains("state=\"running\""));
    assert!(host.dispatched()[0].background);
}

#[tokio::test]
async fn a_host_that_reuses_the_session_id_as_its_job_id_is_refused() {
    let error = tool(Arc::new(RecordingHost::new().conflating_ids()))
        .run(
            TaskParams {
                background: Some(true),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect_err("two indistinguishable ids are not two ids");

    assert!(matches!(error, ToolError::Failed { .. }));
    assert!(message(&error).contains("must be distinguishable"));
}

#[tokio::test]
async fn a_task_id_resumes_that_session_instead_of_creating_one() {
    let host = Arc::new(RecordingHost::new());
    let output = tool(Arc::clone(&host))
        .run(
            TaskParams {
                task_id: Some("ses_earlier".to_owned()),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("a resumed delegation runs");

    assert_eq!(
        host.dispatched()[0].resume_session_id.as_deref(),
        Some("ses_earlier")
    );
    assert!(output.output.contains("<task id=\"ses_earlier\""));
}

// -- what the host receives ----------------------------------------------

#[tokio::test]
async fn the_dispatch_carries_the_resolved_model_effort_and_options() {
    let host = Arc::new(RecordingHost::new());
    tool(Arc::clone(&host))
        .run(
            TaskParams {
                model: Some(MODEL_B.to_owned()),
                effort: Some("high".to_owned()),
                description: Some("survey the crate".to_owned()),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("an allowed delegation runs");

    let dispatched = host.dispatched();
    let request = &dispatched[0];
    assert_eq!(request.agent, "explorer");
    assert_eq!(request.model, Some(ModelChoice::new(MODEL_B)));
    assert_eq!(request.effort, Some(ReasoningEffort::High));
    assert_eq!(request.provider_options["reasoningEffort"], "high");
    assert_eq!(request.prompt, "look around");
    assert_eq!(request.description.as_deref(), Some("survey the crate"));
}

#[tokio::test]
async fn a_resolution_note_reaches_the_caller_in_the_rendered_output() {
    let output = tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                model: Some(MODEL_MUTE.to_owned()),
                effort: Some("max".to_owned()),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("an unhonourable effort does not fail the delegation");

    assert!(output.output.contains("<note>"), "{}", output.output);
    assert!(
        output.output.contains("produces no reasoning"),
        "{}",
        output.output
    );
}

#[tokio::test]
async fn a_host_failure_is_reported_as_a_tool_failure() {
    struct Broken;

    #[async_trait]
    impl ChildTurnHost for Broken {
        async fn delegation_depth(&self, _session_id: &str) -> Result<u32, ChildTurnError> {
            Ok(0)
        }

        async fn dispatch(&self, _request: ChildTurnRequest) -> Result<ChildTurn, ChildTurnError> {
            Err(ChildTurnError::UnknownSession("ses_gone".to_owned()))
        }
    }

    let error = TaskTool::new(Arc::new(Broken), facts())
        .run(to_explorer(), allowed())
        .await
        .expect_err("a host failure fails the call");

    assert!(matches!(error, ToolError::Failed { .. }));
    assert!(message(&error).contains("drop `task_id`"));
}

// -- identity ------------------------------------------------------------

#[test]
fn the_tool_is_named_for_the_registry_slot_it_fills() {
    let subject = tool(Arc::new(RecordingHost::new()));

    assert_eq!(subject.id(), WIRE_ID);
    assert_eq!(WIRE_ID, crate::registry::BuiltinSlot::Task.wire_id());
    assert_eq!(PERMISSION_KEY, WIRE_ID);
    assert!(!subject.description().is_empty());
}

#[tokio::test]
async fn deny_all_and_allow_all_disagree_only_about_whether_a_child_appears() {
    let denied = Arc::new(RecordingHost::new());
    let allowed_host = Arc::new(RecordingHost::new());

    assert!(
        tool(Arc::clone(&denied))
            .run(to_explorer(), context(Arc::new(DenyAll)))
            .await
            .is_err()
    );
    assert!(
        tool(Arc::clone(&allowed_host))
            .run(to_explorer(), allowed())
            .await
            .is_ok()
    );
    assert!(denied.dispatched().is_empty());
    assert_eq!(allowed_host.dispatched().len(), 1);
}
