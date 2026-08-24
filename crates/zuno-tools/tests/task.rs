//! Cross-crate behaviour of the `task` delegation tool.
//!
//! Two things can only be checked from here. The first is the plan's acceptance
//! criterion — that an explicit `model` and `effort` reach the **child's outbound
//! request** — which is proven by handing the recorded dispatch to
//! [`zuno_llm::effort::EffortResolution::apply_to`], the same function a provider
//! adapter uses to decorate a body. The second is the assertion
//! `zuno-agent/src/builtin.rs:78` defers to this crate: that its `GOVERNED_TOOL_IDS`
//! still name real registry built-ins.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use zuno_agent::builtin::{GOVERNED_TOOL_IDS, delegable};
use zuno_agent::model_policy::ModelChoice;
use zuno_error::ToolError;
use zuno_llm::effort::{
    EffortResolution, ProviderFamily, ReasoningEffort, ResolutionSource, resolve_effort,
};
use zuno_llm::registry::ApiSurface;
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext, erase};
use zuno_tools::FileTools;
use zuno_tools::registry::{BUILTIN_ORDER, BuiltinSlot, RegistryFlags, ToolRegistryBuilder};
use zuno_tools::task::{
    COORDINATOR, ChildTurn, ChildTurnError, ChildTurnHost, ChildTurnRequest, FixedFacts,
    GENERIC_EXECUTOR, RecordingHost, TaskTool, WIRE_ID, valid_targets,
};

const REASONER: &str = "acme/reasoner";

fn facts() -> Arc<FixedFacts> {
    Arc::new(FixedFacts::new().with_reasoning(REASONER, ProviderFamily::OpenAi))
}

fn tool(host: Arc<RecordingHost>) -> TaskTool {
    TaskTool::new(host, facts()).with_session_model(ModelChoice::new(REASONER))
}

fn context() -> ToolContext {
    ToolContext::new(
        "ses_root",
        "msg_1",
        "call_1",
        COORDINATOR,
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn source_message(error: &ToolError) -> String {
    match error {
        ToolError::InvalidArgs { source, .. } | ToolError::Failed { source, .. } => {
            source.to_string()
        }
        other => other.to_string(),
    }
}

/// The body a provider adapter starts from before an effort decorates it.
fn base_body(model: &str) -> Map<String, Value> {
    json!({ "model": model, "messages": [] })
        .as_object()
        .cloned()
        .expect("an object literal")
}

/// QA happy path plus the plan's acceptance criterion, in one assertion chain.
#[tokio::test]
async fn an_explicit_model_and_effort_reach_the_childs_outbound_request() {
    let host = Arc::new(RecordingHost::new());
    tool(Arc::clone(&host))
        .run_erased(
            json!({
                "prompt": "survey the crate",
                "subagent_type": "explorer",
                "model": REASONER,
                "effort": "low",
            }),
            context(),
        )
        .await
        .expect("an allowed delegation runs");

    let dispatched = host.dispatched();
    let request = dispatched.first().expect("one child turn");
    assert_eq!(request.agent, "explorer");
    assert_eq!(request.model, Some(ModelChoice::new(REASONER)));
    assert_eq!(request.effort, Some(ReasoningEffort::Low));

    // The same merge a provider adapter performs, so this is the child's real body.
    // The assertion changed from `reasoningEffort` to the two wire fields that
    // option becomes: the delegated options are SDK provider-option names, and
    // asserting the pre-lowering spelling here made the child body look verified
    // while it was in fact going out under a field no endpoint reads.
    let resolution = EffortResolution {
        effort: request.effort.expect("an effort resolved"),
        source: ResolutionSource::GenericMapping,
        options: request.provider_options.clone(),
    };
    let mut chat = base_body(&request.model.as_ref().expect("a model").model);
    resolution.apply_to(&mut chat, ApiSurface::Chat);
    let mut responses = base_body(&request.model.as_ref().expect("a model").model);
    resolution.apply_to(&mut responses, ApiSurface::Responses);

    assert_eq!(chat["model"], REASONER);
    assert_eq!(
        chat["reasoning_effort"], "low",
        "the effort must reach a chat body under its wire name: {chat:?}"
    );
    assert_eq!(
        responses["reasoning"],
        serde_json::json!({"effort": "low"}),
        "the Responses surface takes the level nested: {responses:?}"
    );
    assert!(
        !chat.contains_key("reasoningEffort") && !responses.contains_key("reasoningEffort"),
        "the SDK option name must not reach either body"
    );
    assert_eq!(
        request.provider_options,
        resolve_effort(
            ProviderFamily::OpenAi,
            ReasoningEffort::Low,
            zuno_llm::effort::EffortCapabilities::default(),
            &zuno_llm::effort::DeclaredVariants::new(),
        )
        .options,
        "the options must be todo 31's, not a second mapping invented here"
    );
}

/// QA failure path: the refusal must list what the caller may name instead.
#[tokio::test]
async fn naming_build_is_rejected_with_a_message_listing_valid_targets() {
    let host = Arc::new(RecordingHost::new());
    let error = tool(Arc::clone(&host))
        .run_erased(
            json!({ "prompt": "coordinate", "subagent_type": COORDINATOR }),
            context(),
        )
        .await
        .expect_err("a coordinator is never a delegation target");

    let text = source_message(&error);
    assert_eq!(error.tool(), WIRE_ID);
    assert!(
        text.contains("cannot be a delegation target"),
        "must say why: {text}"
    );
    let targets = valid_targets(false);
    assert_eq!(
        targets.len(),
        6,
        "the native roster's delegable set: {targets:?}"
    );
    for target in &targets {
        assert!(text.contains(target), "must offer {target}: {text}");
    }
    assert!(
        host.dispatched().is_empty(),
        "no child session may be created by a rejected call"
    );
}

#[tokio::test]
async fn every_delegable_agent_is_reachable_and_the_coordinator_is_not() {
    for agent in delegable(true) {
        let host = Arc::new(RecordingHost::new());
        tool(Arc::clone(&host))
            .with_vision_available(true)
            .run_erased(
                json!({ "prompt": "work", "subagent_type": agent.name }),
                context(),
            )
            .await
            .unwrap_or_else(|error| panic!("{} must be reachable: {error}", agent.name));
        assert_eq!(host.dispatched()[0].agent, agent.name);
    }

    assert!(
        !valid_targets(true).iter().any(|name| name == COORDINATOR),
        "the roster, not this tool, is what excludes the coordinator"
    );
}

/// The assertion `zuno-agent/src/builtin.rs:78` explicitly defers to this crate.
#[test]
fn every_governed_tool_id_is_a_real_production_tool() {
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory)
            .expect("open work-state tool registry fixture"),
    );
    let configured = zuno_tools::work_state_tools(pool);
    let production_ids = BUILTIN_ORDER
        .iter()
        .map(|slot| slot.wire_id())
        .chain(configured.iter().map(|tool| tool.id()))
        .collect::<Vec<_>>();

    assert!(
        BUILTIN_ORDER.len() >= 14,
        "the scan must see the whole fixed-slot builtin table, saw {}",
        BUILTIN_ORDER.len()
    );
    for governed in GOVERNED_TOOL_IDS {
        assert!(
            production_ids.contains(&governed),
            "`{governed}` is named by a permission set but is not a production tool: \
             {production_ids:?}"
        );
    }
    assert!(
        GOVERNED_TOOL_IDS.contains(&WIRE_ID),
        "delegation must be governable"
    );
    for dead in ["write", "apply_patch", "invalid"] {
        assert!(
            !GOVERNED_TOOL_IDS.contains(&dead),
            "`{dead}` cannot be named by a rule and must stay out of the governed set"
        );
    }
}

#[tokio::test]
async fn the_task_tool_registers_in_its_upstream_slot_and_resolves() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let file_tools = FileTools::new(workspace.path()).expect("file tools");
    let mut builder =
        ToolRegistryBuilder::new(workspace.path(), file_tools, RegistryFlags::default());
    builder
        .register_builtin(
            BuiltinSlot::Task,
            erase(tool(Arc::new(RecordingHost::new()))),
        )
        .expect("the wire id must match the slot");
    let registry = builder.build();

    assert!(
        registry.all().iter().any(|entry| entry.id() == WIRE_ID),
        "the delegation tool must be in the assembled registry"
    );
}

#[tokio::test]
async fn a_child_session_cannot_delegate_again_at_the_default_bound() {
    struct InChildSession;

    #[async_trait]
    impl ChildTurnHost for InChildSession {
        async fn delegation_depth(&self, _session_id: &str) -> Result<u32, ChildTurnError> {
            Ok(1)
        }

        async fn dispatch(&self, _request: ChildTurnRequest) -> Result<ChildTurn, ChildTurnError> {
            panic!("a delegation past the bound must never reach the host");
        }
    }

    let error = TaskTool::new(Arc::new(InChildSession), facts())
        .run_erased(
            json!({ "prompt": "delegate deeper", "subagent_type": GENERIC_EXECUTOR }),
            context(),
        )
        .await
        .expect_err("unbounded recursive delegation must be impossible");

    let text = source_message(&error);
    assert!(text.contains("Subagent depth limit reached"), "{text}");
    assert!(text.contains("`subagent_depth`"), "{text}");
    assert!(!error.is_retryable());
    assert!(!error.is_model_correctable());
}

/// A convenience so these tests exercise the same decode path the model does.
trait RunErased {
    async fn run_erased(self, args: Value, ctx: ToolContext) -> Result<String, ToolError>;
}

impl RunErased for TaskTool {
    async fn run_erased(self, args: Value, ctx: ToolContext) -> Result<String, ToolError> {
        erase(self).execute(args, ctx).await.map(|out| out.output)
    }
}
