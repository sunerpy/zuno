use super::*;
use std::sync::Mutex;
use zuno_agent::model_policy::ModelPreset;
use zuno_tool::{AllowAll, DenyAll, NeverInterrupted, PermissionAsker, erase};

const PARENT: &str = "ses_parent";
const MODEL_A: &str = "acme/reasoner";
const MODEL_B: &str = "acme/other-reasoner";
const MODEL_MUTE: &str = "acme/no-reasoning";

fn contract(objective: &str) -> DelegationContract {
    DelegationContract {
        objective: objective.to_owned(),
        deliverable: "Return the requested result.".to_owned(),
        instructions: objective.to_owned(),
        success_evidence: "Cite the concrete evidence used.".to_owned(),
        scope: None,
        constraints: None,
        dependencies: Vec::new(),
    }
}

fn params(objective: &str) -> TaskParams {
    TaskParams {
        contract: contract(objective),
        agent: "explorer".to_owned(),
        background: None,
        report_delivery: None,
        task_id: None,
    }
}

fn to_explorer() -> TaskParams {
    params("look around")
}

#[test]
fn logical_task_identity_depends_only_on_the_agent_and_contract() {
    let agreement = contract("inspect the runtime");
    let first = delegation_logical_key("explorer", &agreement);
    let second = delegation_logical_key("explorer", &agreement);
    let other_agent = delegation_logical_key("oracle", &agreement);
    let other_contract = delegation_logical_key("explorer", &contract("inspect the database"));

    assert_eq!(first, second);
    assert_ne!(first, other_agent);
    assert_ne!(first, other_contract);
}

fn route(model: Option<&str>, effort: Option<&str>) -> DelegationModelRequest {
    DelegationModelRequest {
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
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

fn selectable_policy(models: &[&str]) -> SubagentModelPolicy {
    SubagentModelPolicy::new(true, models.iter().map(|model| (*model).to_owned()))
        .expect("selectable test policy")
}

fn selectable_facts() -> Arc<FixedFacts> {
    let mut options = Map::new();
    options.insert(
        "reasoningEffort".to_owned(),
        Value::String("high".to_owned()),
    );
    let mut variants = BTreeMap::new();
    variants.insert("high".to_owned(), options);
    Arc::new(
        FixedFacts::new()
            .with(
                MODEL_A,
                ModelFacts {
                    family: ProviderFamily::OpenAi,
                    reasoning: true,
                    effort: EffortCapabilities::default(),
                    variants,
                },
            )
            .with_reasoning(MODEL_B, ProviderFamily::OpenAi),
    )
}

fn selectable_tool(host: Arc<RecordingHost>) -> SelectableTaskTool {
    TaskTool::new(host, selectable_facts())
        .with_session_model(ModelChoice::new(MODEL_B))
        .with_subagent_model_policy(selectable_policy(&[MODEL_A]))
        .selectable()
}

fn selectable_params(model: Option<&str>, effort: Option<&str>) -> SelectableTaskParams {
    SelectableTaskParams {
        contract: contract("look around"),
        agent: "explorer".to_owned(),
        background: None,
        report_delivery: None,
        task_id: None,
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
    }
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
    async fn ask(
        &self,
        _origin: zuno_tool::PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
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
    async fn ask(
        &self,
        _origin: zuno_tool::PermissionOrigin<'_>,
        _tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
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

// -- rejection 1: a coordinator as target -----------------------------------

#[tokio::test]
async fn the_coordinator_is_refused_and_the_message_lists_the_valid_targets() {
    let error = tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                agent: COORDINATOR.to_owned(),
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
    assert!(text.contains("Set `agent` to one of"), "{text}");
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
async fn an_unknown_agent_is_told_which_agents_exist() {
    let error = tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                agent: "cheap".to_owned(),
                ..params("do a thing")
            },
            allowed(),
        )
        .await
        .expect_err("an agent outside the roster cannot be targeted");

    let text = message(&error);
    assert!(text.contains("Unknown Agent `cheap`"), "{text}");
    for target in valid_targets(false) {
        assert!(text.contains(&target), "must list {target}: {text}");
    }
}

// -- rejection 2: depth ------------------------------------------------------

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

// -- rejection 3: permission ------------------------------------------------

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
async fn the_permission_pattern_is_the_agent() {
    let asker = Arc::new(RecordingAllower::default());
    tool(Arc::new(RecordingHost::new()))
        .run(
            TaskParams {
                contract: contract("read the docs"),
                agent: "librarian".to_owned(),
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
    assert_eq!(ask.metadata["agent"], "librarian");
    assert_eq!(ask.metadata["objective"], "read the docs");
}

#[test]
fn the_advertised_schema_is_a_typed_delegation_contract() {
    let definition = erase(tool(Arc::new(RecordingHost::new()))).definition();
    let properties = &definition.parameters["properties"];

    for expected in [
        "objective",
        "deliverable",
        "instructions",
        "success_evidence",
        "scope",
        "constraints",
        "dependencies",
        "agent",
        "background",
        "reportDelivery",
        "task_id",
    ] {
        assert!(
            properties.get(expected).is_some(),
            "{expected} must be advertised"
        );
    }
    for removed in ["description", "prompt", "load_skills"] {
        assert!(
            properties.get(removed).is_none(),
            "legacy field `{removed}` must not be advertised"
        );
    }
    assert_eq!(
        definition.parameters["required"],
        serde_json::json!([
            "objective",
            "deliverable",
            "instructions",
            "success_evidence",
            "agent"
        ])
    );
    assert_eq!(
        properties["scope"]["properties"]["include"]["items"]["type"],
        "string"
    );
    assert_eq!(
        properties["scope"]["properties"]["exclude"]["items"]["type"],
        "string"
    );
    assert_eq!(
        properties["constraints"]["properties"]["must"]["items"]["type"],
        "string"
    );
    assert_eq!(
        properties["constraints"]["properties"]["must_not"]["items"]["type"],
        "string"
    );
    assert!(
        properties.get("model").is_none() && properties.get("effort").is_none(),
        "the default-disabled policy must preserve the existing task schema"
    );
}

#[test]
fn an_enabled_policy_advertises_optional_model_and_effort_fields() {
    let definition = erase(selectable_tool(Arc::new(RecordingHost::new()))).definition();
    let properties = &definition.parameters["properties"];

    assert_eq!(
        properties["model"]["type"],
        serde_json::json!(["string", "null"])
    );
    assert_eq!(
        properties["effort"]["type"],
        serde_json::json!(["string", "null"])
    );
    let required = definition.parameters["required"]
        .as_array()
        .expect("required fields");
    assert!(!required.contains(&Value::String("model".to_owned())));
    assert!(!required.contains(&Value::String("effort".to_owned())));
}

#[test]
fn subagent_model_policy_is_canonical_and_rejects_ambiguous_authority() {
    let first = selectable_policy(&[MODEL_B, MODEL_A]);
    let second = selectable_policy(&[MODEL_A, MODEL_B]);
    assert_eq!(first, second);
    let mut expected = vec![MODEL_A.to_owned(), MODEL_B.to_owned()];
    expected.sort();
    assert_eq!(first.allowed_models(), expected);

    assert_eq!(
        SubagentModelPolicy::new(true, Vec::<String>::new()),
        Err(SubagentModelPolicyError::EmptyEnabledAllowlist)
    );
    assert_eq!(
        SubagentModelPolicy::new(true, [MODEL_A.to_owned(), MODEL_A.to_owned()]),
        Err(SubagentModelPolicyError::Duplicate(MODEL_A.to_owned()))
    );
    for invalid in ["", "model", "/model", "provider/", "provider/model/extra"] {
        assert_eq!(
            SubagentModelPolicy::new(true, [invalid.to_owned()]),
            Err(SubagentModelPolicyError::InvalidModel),
            "{invalid}"
        );
    }
}

#[test]
fn legacy_task_arguments_are_refused_instead_of_migrated() {
    for removed in [
        "description",
        "prompt",
        "load_skills",
        "subagent_type",
        "category",
        "model",
        "effort",
    ] {
        let mut value = serde_json::json!({
            "objective": "Current objective",
            "deliverable": "Current deliverable",
            "instructions": "Current instructions",
            "success_evidence": "Current evidence",
            "agent": "explorer",
        });
        value
            .as_object_mut()
            .expect("fixture object")
            .insert(removed.to_owned(), Value::String("legacy".to_owned()));
        let rendered = format!("{:?}", serde_json::from_value::<TaskParams>(value));
        assert!(rendered.contains(removed), "{rendered}");
    }
}

#[tokio::test]
async fn empty_required_contract_fields_are_rejected_before_dispatch() {
    for field in [
        "objective",
        "deliverable",
        "instructions",
        "success_evidence",
    ] {
        let host = Arc::new(RecordingHost::new());
        let mut value = to_explorer();
        match field {
            "objective" => value.contract.objective = "  ".to_owned(),
            "deliverable" => value.contract.deliverable = "  ".to_owned(),
            "instructions" => value.contract.instructions = "  ".to_owned(),
            "success_evidence" => value.contract.success_evidence = "  ".to_owned(),
            _ => unreachable!(),
        }
        let error = tool(Arc::clone(&host))
            .run(value, allowed())
            .await
            .expect_err("blank required contract fields must not dispatch");
        assert!(message(&error).contains(&format!("`{field}` must not be empty")));
        assert!(host.dispatched().is_empty());
    }
}

#[tokio::test]
async fn empty_optional_contract_items_are_rejected_before_dispatch() {
    let host = Arc::new(RecordingHost::new());
    let mut value = to_explorer();
    value.contract.scope = Some(DelegationScope {
        include: vec!["crates/zuno-tools/**".to_owned(), " ".to_owned()],
        exclude: Vec::new(),
    });
    let error = tool(Arc::clone(&host))
        .run(value, allowed())
        .await
        .expect_err("blank scope items must not dispatch");
    assert!(
        message(&error).contains("`scope.include` item 2 must not be empty"),
        "{}",
        message(&error)
    );
    assert!(host.dispatched().is_empty());
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
async fn a_composition_can_replace_the_native_roster_with_custom_agents() {
    let host = Arc::new(RecordingHost::new());
    let targets = DelegationTargets::new(vec!["release-reviewer".to_owned()])
        .expect("custom target is valid");
    tool(Arc::clone(&host))
        .with_targets(targets)
        .run(
            TaskParams {
                agent: "release-reviewer".to_owned(),
                ..params("review the release")
            },
            allowed(),
        )
        .await
        .expect("the resolved custom target is reachable");

    assert_eq!(host.dispatched()[0].agent, "release-reviewer");
    let error = tool(Arc::new(RecordingHost::new()))
        .with_targets(
            DelegationTargets::new(vec!["release-reviewer".to_owned()])
                .expect("custom target is valid"),
        )
        .run(to_explorer(), allowed())
        .await
        .expect_err("an omitted native target must not remain reachable");
    assert!(message(&error).contains("Unknown Agent `explorer`"));
}

#[test]
fn composition_targets_reject_duplicates_and_the_coordinator() {
    assert_eq!(
        DelegationTargets::new(vec!["reviewer".to_owned(), "reviewer".to_owned()])
            .expect_err("duplicates are ambiguous"),
        DelegationTargetError::Duplicate("reviewer".to_owned())
    );
    assert_eq!(
        DelegationTargets::new(vec![COORDINATOR.to_owned()])
            .expect_err("the coordinator cannot recurse"),
        DelegationTargetError::Coordinator
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
                agent: looker.clone(),
                ..params("describe the screenshot")
            },
            allowed(),
        )
        .await
        .expect_err("the gated target is absent without a vision model");
    assert!(message(&refused).contains(&format!("Unknown Agent `{looker}`")));

    tool(Arc::new(RecordingHost::new()))
        .with_vision_available(true)
        .run(
            TaskParams {
                agent: looker,
                ..params("describe the screenshot")
            },
            allowed(),
        )
        .await
        .expect("the gated target is present with one");
}

// -- durable model-selection authority -------------------------------------

#[tokio::test]
async fn enabled_model_selection_requires_an_allowlisted_available_declared_pair() {
    let cases = [
        (
            selectable_params(None, Some("high")),
            "`effort` requires an explicit allowlisted `model`",
        ),
        (
            selectable_params(Some(MODEL_B), None),
            "is not authorized for this session",
        ),
        (
            selectable_params(Some(MODEL_A), Some("missing")),
            "is not a variant declared by",
        ),
    ];

    for (params, expected) in cases {
        let host = Arc::new(RecordingHost::new());
        let error = selectable_tool(Arc::clone(&host))
            .run(params, allowed())
            .await
            .expect_err("invalid explicit authority must be refused");
        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert!(message(&error).contains(expected), "{}", message(&error));
        assert!(
            host.dispatched().is_empty(),
            "invalid model authority must not create a child"
        );
    }

    let host = Arc::new(RecordingHost::new());
    let unavailable = TaskTool::new(host.clone(), selectable_facts())
        .with_session_model(ModelChoice::new(MODEL_B))
        .with_subagent_model_policy(selectable_policy(&["acme/retired"]))
        .selectable()
        .run(selectable_params(Some("acme/retired"), None), allowed())
        .await
        .expect_err("an allowlisted but unresolved model must be refused");
    assert!(message(&unavailable).contains("is not present in the resolved model catalog"));
    assert!(host.dispatched().is_empty());
}

#[tokio::test]
async fn enabled_model_selection_dispatches_the_exact_frozen_choice() {
    let host = Arc::new(RecordingHost::new());
    let mut params = selectable_params(Some(MODEL_A), Some("high"));
    params.task_id = Some("ses_earlier".to_owned());
    selectable_tool(Arc::clone(&host))
        .run(params, allowed())
        .await
        .expect("the exact allowlisted pair dispatches");

    let request = &host.dispatched()[0];
    assert_eq!(request.resume_session_id.as_deref(), Some("ses_earlier"));
    assert_eq!(request.model, Some(ModelChoice::new(MODEL_A)));
    assert_eq!(request.effort, Some(ReasoningEffort::High));
    assert_eq!(request.provider_options["reasoningEffort"], "high");
    assert_eq!(request.requested_model.as_deref(), Some(MODEL_A));
    assert_eq!(request.requested_effort.as_deref(), Some("high"));
    assert_eq!(request.subagent_model_policy, selectable_policy(&[MODEL_A]));
}

#[tokio::test]
async fn an_enabled_policy_without_an_explicit_model_keeps_existing_routing() {
    let host = Arc::new(RecordingHost::new());
    selectable_tool(Arc::clone(&host))
        .run(selectable_params(None, None), allowed())
        .await
        .expect("omitting model keeps the existing delegation ladder");

    let request = &host.dispatched()[0];
    assert_eq!(request.model, Some(ModelChoice::new(MODEL_B)));
    assert_eq!(request.requested_model, None);
    assert_eq!(request.requested_effort, None);
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

    let plan = subject.plan("explorer", None, &route(Some(MODEL_A), None));

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
        subject
            .plan("explorer", None, &DelegationModelRequest::default())
            .model,
        Some(ModelChoice::new(MODEL_B)),
        "the preset rung must still answer"
    );
    assert_eq!(
        subject
            .plan("worker", None, &DelegationModelRequest::default())
            .model,
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

    let plan = subject.plan(
        GENERIC_EXECUTOR,
        Some("cheap"),
        &DelegationModelRequest::default(),
    );

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

    let plan = subject.plan(
        GENERIC_EXECUTOR,
        Some("nope"),
        &DelegationModelRequest::default(),
    );

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
        &route(Some(MODEL_A), Some("low")),
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
        &route(Some("acme/retired"), None),
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
    let plan =
        tool(Arc::new(RecordingHost::new())).plan("explorer", None, &route(Some("reasoner"), None));

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
        &route(Some(MODEL_MUTE), Some("high")),
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
        &route(Some(MODEL_A), Some("ponder")),
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
        .plan("explorer", None, &route(None, Some("ponder")));

    assert_eq!(plan.provider_options, options);
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
}

#[test]
fn an_effort_with_no_resolvable_model_is_reported() {
    let plan = TaskTool::new(Arc::new(RecordingHost::new()), Arc::new(NoProviders)).plan(
        "explorer",
        None,
        &route(None, Some("low")),
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
    let host = Arc::new(
        RecordingHost::new().with_report_metadata(serde_json::json!({
            "schemaVersion": 1,
            "jobId": null,
            "sessionId": format!("ses_child_of_{PARENT}"),
            "status": "completed",
            "finalText": "done",
            "changedPaths": ["src/lib.rs"],
            "verificationRecords": [],
            "uncertainSideEffects": []
        })),
    );
    let output = tool(Arc::clone(&host))
        .run(to_explorer(), allowed())
        .await
        .expect("a foreground delegation runs");

    let child = format!("ses_child_of_{PARENT}");
    assert!(output.output.contains(&format!("<task id=\"{child}\"")));
    assert!(output.output.contains("state=\"completed\""));
    assert!(!output.output.contains("background="));
    assert_eq!(output.metadata["subagent"]["sessionId"], child);
    assert_eq!(output.metadata["subagent"]["agent"], "explorer");
    assert_eq!(output.metadata["subagent"]["objective"], "look around");
    assert_eq!(
        output.metadata["subagent"]["contract"]["deliverable"],
        "Return the requested result."
    );
    assert_eq!(
        output.metadata["subagent"]["contract"]["instructions"],
        "look around"
    );
    assert_eq!(
        output.metadata["subagent"]["contract"]["success_evidence"],
        "Cite the concrete evidence used."
    );
    assert_eq!(output.metadata["subagent"]["state"], "completed");
    assert_eq!(output.metadata["subagent"]["background"], false);
    assert_eq!(output.metadata["subagent"]["reportDelivery"], "foreground");
    assert_eq!(output.metadata["subagent"]["report"]["finalText"], "done");
    assert_eq!(
        output.metadata["subagent"]["report"]["changedPaths"],
        serde_json::json!(["src/lib.rs"])
    );
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
    assert!(output.output.contains(&format!("<task id=\"{child}\"")));
    assert!(output.output.contains("job=\"job_"));
    assert!(output.output.contains("reportDelivery=\"nextStep\""));
    assert!(output.output.contains("state=\"running\""));
    assert_eq!(output.metadata["subagent"]["sessionId"], child);
    assert!(
        output.metadata["subagent"]["jobId"]
            .as_str()
            .is_some_and(|job| job.starts_with("job_"))
    );
    assert_eq!(output.metadata["subagent"]["state"], "running");
    assert_eq!(output.metadata["subagent"]["background"], true);
    assert_eq!(output.metadata["subagent"]["reportDelivery"], "nextStep");
    assert!(host.dispatched()[0].background);
    assert_eq!(
        host.dispatched()[0].report_delivery,
        ReportDelivery::NextStep
    );
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
async fn quiet_report_delivery_reaches_the_background_host() {
    let host = Arc::new(RecordingHost::new());
    let output = tool(Arc::clone(&host))
        .run(
            TaskParams {
                background: Some(true),
                report_delivery: Some(ReportDelivery::Quiet),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("quiet background delegation runs");

    assert!(output.output.contains("reportDelivery=\"quiet\""));
    assert_eq!(host.dispatched()[0].report_delivery, ReportDelivery::Quiet);
}

#[tokio::test]
async fn report_delivery_without_background_is_refused_before_permission_or_dispatch() {
    let host = Arc::new(RecordingHost::new());
    let error = tool(Arc::clone(&host))
        .run(
            TaskParams {
                report_delivery: Some(ReportDelivery::Quiet),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect_err("foreground report delivery has no meaning");

    assert!(message(&error).contains("requires `background: true`"));
    assert!(host.dispatched().is_empty());
}

#[tokio::test]
async fn repeated_background_dispatches_receive_fresh_job_ids() {
    let host = Arc::new(RecordingHost::new());
    let first = tool(Arc::clone(&host))
        .run(
            TaskParams {
                background: Some(true),
                task_id: Some("ses_earlier".to_owned()),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("first background dispatch");
    let second = tool(host)
        .run(
            TaskParams {
                background: Some(true),
                task_id: Some("ses_earlier".to_owned()),
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("second background dispatch");

    let job = |output: &str| {
        output
            .split("job=\"")
            .nth(1)
            .and_then(|tail| tail.split('"').next())
            .expect("rendered job id")
            .to_owned()
    };
    assert_ne!(job(&first.output), job(&second.output));
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
        .with_agent_override("explorer", ModelChoice::new(MODEL_B).with_variant("high"))
        .run(
            TaskParams {
                contract: DelegationContract {
                    objective: "survey the crate".to_owned(),
                    ..contract("look around")
                },
                ..to_explorer()
            },
            allowed(),
        )
        .await
        .expect("an allowed delegation runs");

    let dispatched = host.dispatched();
    let request = &dispatched[0];
    assert_eq!(request.agent, "explorer");
    assert_eq!(
        request.model,
        Some(ModelChoice::new(MODEL_B).with_variant("high"))
    );
    assert_eq!(request.effort, Some(ReasoningEffort::High));
    assert_eq!(request.provider_options["reasoningEffort"], "high");
    assert_eq!(
        request.logical_key,
        delegation_logical_key(
            "explorer",
            &DelegationContract {
                objective: "survey the crate".to_owned(),
                ..contract("look around")
            }
        )
    );
    assert!(request.prompt.contains("Instructions:\nlook around"));
    assert!(
        request
            .prompt
            .contains("Deliverable:\nReturn the requested result.")
    );
    assert_eq!(request.description.as_deref(), Some("survey the crate"));
}

#[tokio::test]
async fn a_resolution_note_reaches_the_caller_in_the_rendered_output() {
    let output = tool(Arc::new(RecordingHost::new()))
        .with_agent_override("explorer", ModelChoice::new(MODEL_MUTE).with_variant("max"))
        .run(to_explorer(), allowed())
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

        async fn dispatch(
            &self,
            _request: ChildTurnRequest,
            _interrupt: Arc<dyn zuno_tool::InterruptHandle>,
        ) -> Result<ChildTurn, ChildTurnError> {
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
