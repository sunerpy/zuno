//! What both surfaces must be able to trust about the shared composition root.

use super::*;
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::run_turn;

use crate::cmd::tool_runtime;
use std::path::Path;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use zuno_catalog::agent::{Agent, AgentMode, AgentSource};
use zuno_llm::sse::StreamIdleTimeout;
use zuno_orchestration::sha256_text;
use zuno_paths::Env;

#[derive(Debug)]
struct DirectTestSandbox {
    capabilities: zuno_sandbox::SandboxCapabilities,
}

impl DirectTestSandbox {
    fn new() -> Self {
        Self {
            capabilities: zuno_sandbox::SandboxCapabilities {
                backend: "test_direct".to_owned(),
                executable: Some("/bin/sh".into()),
                read_only: true,
                workspace_write: true,
                danger_full_access: true,
                network_isolation: true,
            },
        }
    }
}

impl zuno_sandbox::SandboxBackend for DirectTestSandbox {
    fn capabilities(&self) -> &zuno_sandbox::SandboxCapabilities {
        &self.capabilities
    }

    fn prepare(
        &self,
        request: zuno_sandbox::PrepareRequest,
    ) -> Result<zuno_sandbox::PreparedCommand, zuno_sandbox::SandboxError> {
        let program = request.program.clone();
        let arguments = request.arguments.clone();
        let writable_roots = if request.policy.mode() == zuno_sandbox::SandboxMode::WorkspaceWrite {
            vec![request.policy.workspace().to_owned()]
        } else {
            Vec::new()
        };
        Ok(zuno_sandbox::PreparedCommand::from_backend(
            request,
            program,
            arguments,
            &self.capabilities,
            writable_roots,
            Vec::new(),
        ))
    }
}

fn test_sandbox() -> Option<Arc<dyn zuno_sandbox::SandboxResolver>> {
    Some(Arc::new(DirectTestSandbox::new()))
}

#[derive(Debug)]
struct UnavailableTestSandbox;

impl zuno_sandbox::SandboxResolver for UnavailableTestSandbox {
    fn resolve(
        self: Arc<Self>,
        policy: zuno_sandbox::SandboxPolicy,
        on_unavailable: zuno_sandbox::SandboxUnavailableAction,
    ) -> Result<zuno_sandbox::SandboxResolution, zuno_sandbox::SandboxError> {
        let _ = self;
        if on_unavailable == zuno_sandbox::SandboxUnavailableAction::RunUnconfined
            && policy.mode() == zuno_sandbox::SandboxMode::WorkspaceWrite
        {
            return zuno_sandbox::SandboxResolution::unavailable_fallback(
                policy,
                zuno_sandbox::SandboxUnavailableCause::BubblewrapNotFound,
            );
        }
        Err(zuno_sandbox::SandboxError::BubblewrapNotFound)
    }
}

fn agent(name: &str) -> Agent {
    Agent {
        name: name.to_owned(),
        description: None,
        mode: AgentMode::All,
        hidden: None,
        model: None,
        variant: None,
        reasoning: None,
        temperature: None,
        top_p: None,
        color: None,
        prompt: None,
        steps: None,
        tools: None,
        delegates: None,
        required_skills: None,
        options: serde_json::Map::new(),
        permission: None,
        source: AgentSource::Native,
    }
}

fn agent_profile(
    entry: Agent,
    directory: &Path,
    config: &zuno_config::schema::Config,
) -> zuno_agent::profile::AgentProfile {
    let env = Env::empty();
    let dynamic = crate::cmd::agent::DynamicRules::resolve(directory, None, &env, config);
    crate::cmd::agent::resolved_profile(entry, config, &dynamic, false)
}

#[test]
fn agent_selection_prefers_the_request_then_config_then_orchestrator() {
    assert_eq!(resolve_agent_name(Some("build"), Some("plan")), "build");
    assert_eq!(resolve_agent_name(None, Some("plan")), "plan");
    assert_eq!(resolve_agent_name(None, None), "orchestrator");
}

#[test]
fn configured_subagents_reach_task_with_their_model_choice() {
    let mut explorer = agent("explorer");
    explorer.mode = AgentMode::Subagent;
    let mut looker = agent("looker");
    looker.mode = AgentMode::Subagent;
    let mut custom = agent("release-reviewer");
    custom.mode = AgentMode::Subagent;
    custom.source = AgentSource::Config;
    custom.model = Some("provider/reviewer".to_owned());
    custom.variant = Some("high".to_owned());
    let mut primary = agent("release-primary");
    primary.mode = AgentMode::Primary;
    primary.source = AgentSource::Config;

    let resolved = delegation_agents(&[explorer, looker, custom, primary], false)
        .expect("the resolved catalog is a valid task roster");

    assert_eq!(
        resolved.targets.as_slice(),
        &["explorer".to_owned(), "release-reviewer".to_owned()]
    );
    assert_eq!(
        resolved.models,
        vec![(
            "release-reviewer".to_owned(),
            ModelChoice::new("provider/reviewer").with_variant("high")
        )]
    );
}

fn traced_resolver(prompt: &str) -> Resolver {
    let mut assembly = PromptAssembly::new();
    assembly
        .push("agent.base", "test:agent", prompt)
        .expect("test prompt section");
    Resolver {
        requested_agent: "build".to_owned(),
        system_prompt: assembly.render(),
        prompt_assembly: Some(assembly),
        runtime_prompt_policy: RuntimePromptPolicy::default(),
        max_steps: None,
        requested_provider: "provider".to_owned(),
        requested_model: "model".to_owned(),
        wire_model: "model".to_owned(),
        spec: Spec::new(COMPATIBLE_PROVIDER),
        reasoning_options: serde_json::Map::new(),
        orchestration_seed: None,
    }
}

#[test]
fn resolver_step_limit_is_opt_in() {
    let unlimited = traced_resolver("AGENT")
        .resolve_agent("build")
        .expect("configured agent");
    assert_eq!(unlimited.max_steps, None);

    let mut resolver = traced_resolver("AGENT");
    resolver.max_steps = NonZeroU32::new(17);
    let limited = resolver.resolve_agent("build").expect("configured agent");
    assert_eq!(limited.max_steps, NonZeroU32::new(17));
}

#[test]
fn host_planning_classifier_persists_multi_stage_work_before_the_provider() {
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    {
        let mut connection = pool.get().expect("schema connection");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-plan-policy', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES \
                   ('ses-plan-policy', 'project-plan-policy', 'plan', '/workspace', 'plan', '1', 1, 1),
                   ('ses-atomic-policy', 'project-plan-policy', 'atomic', '/workspace', 'atomic', '1', 1, 1);",
            )
            .expect("seed planning sessions");
    }

    let decision = ensure_host_plan(
        &pool,
        HostPlanningRequest {
            session_id: "ses-plan-policy",
            agent: "build",
            prompt: "Investigate the bug, implement the fix, and run the focused tests.",
            source: PlanningInputSource::User,
            content: PlanningContentFacts::empty(),
            plan_available: true,
            goal_id: None,
        },
    )
    .expect("classify and persist multi-stage work");
    assert!(matches!(decision, PlanningDecision::Create(_)));
    let plan = zuno_tools::WorkStateStore::new(Arc::clone(&pool))
        .plan("ses-plan-policy")
        .expect("read plan")
        .expect("host-created plan");
    assert_eq!(
        plan.steps
            .iter()
            .map(|step| step.id.as_str())
            .collect::<Vec<_>>(),
        ["investigate", "implement", "verify"]
    );
    assert_eq!(plan.steps[0].status, zuno_tools::PlanStepStatus::InProgress);

    let atomic = ensure_host_plan(
        &pool,
        HostPlanningRequest {
            session_id: "ses-atomic-policy",
            agent: "build",
            prompt: "Commit the current staged changes.",
            source: PlanningInputSource::User,
            content: PlanningContentFacts::empty(),
            plan_available: true,
            goal_id: None,
        },
    )
    .expect("classify atomic work");
    assert!(matches!(atomic, PlanningDecision::Atomic(_)));
    assert!(
        zuno_tools::WorkStateStore::new(pool)
            .plan("ses-atomic-policy")
            .expect("read atomic plan")
            .is_none()
    );
}

#[test]
fn host_planning_classifier_appends_a_new_epoch_after_terminal_work() {
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    {
        let mut connection = pool.get().expect("schema connection");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-plan-epoch', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES \
                   ('ses-plan-epoch', 'project-plan-epoch', 'plan', '/workspace', 'plan', '1', 1, 1);",
            )
            .expect("seed planning session");
    }
    let store = zuno_tools::WorkStateStore::new(Arc::clone(&pool));
    let first = store
        .update_plan(
            "ses-plan-epoch",
            zuno_tools::PlanUpdateParams {
                expected_revision: None,
                goal_id: None,
                title: "Finished work".to_owned(),
                steps: vec![zuno_tools::PlanStep {
                    id: "verify".to_owned(),
                    title: "Verify".to_owned(),
                    status: zuno_tools::PlanStepStatus::Completed,
                }],
            },
        )
        .expect("seed terminal plan");

    let decision = ensure_host_plan(
        &pool,
        HostPlanningRequest {
            session_id: "ses-plan-epoch",
            agent: "build",
            prompt: "Investigate the new bug, implement the fix, and verify it.",
            source: PlanningInputSource::User,
            content: PlanningContentFacts::empty(),
            plan_available: true,
            goal_id: None,
        },
    )
    .expect("append new epoch");

    assert!(matches!(decision, PlanningDecision::Create(_)));
    let updated = store
        .plan("ses-plan-epoch")
        .expect("read plan")
        .expect("updated plan");
    assert_eq!(updated.revision, first.revision + 1);
    assert_eq!(updated.steps[0].id, "verify");
    assert_eq!(
        updated
            .steps
            .iter()
            .skip(1)
            .map(|step| step.id.as_str())
            .collect::<Vec<_>>(),
        ["epoch-2-investigate", "epoch-2-implement", "epoch-2-verify"]
    );
}

#[test]
fn child_report_does_not_seed_a_plan_for_an_atomic_parent_session() {
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    {
        let mut connection = pool.get().expect("schema connection");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-report-plan', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES \
                   ('ses-report-plan', 'project-report-plan', 'report', '/workspace', 'report', '1', 1, 1);",
            )
            .expect("seed report session");
    }

    let decision = ensure_host_plan(
        &pool,
        HostPlanningRequest {
            session_id: "ses-report-plan",
            agent: "orchestrator",
            prompt: "Implemented the fix and verified the tests.",
            source: PlanningInputSource::ChildReport,
            content: PlanningContentFacts::empty(),
            plan_available: true,
            goal_id: None,
        },
    )
    .expect("classify child report");

    assert!(matches!(decision, PlanningDecision::Atomic(_)));
    assert!(
        zuno_tools::WorkStateStore::new(pool)
            .plan("ses-report-plan")
            .expect("read plan")
            .is_none()
    );
}

#[test]
fn production_turn_runs_the_host_planning_classifier_after_input_is_durable() {
    let turn = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/turn.rs"),
    )
    .expect("turn.rs is readable");
    let persisted = turn
        .find("self.persist_user_input(&message, &parts)?")
        .expect("input persistence call");
    let classified = turn
        .find("self.ensure_durable_plan(prompt, options.planning_source, options.content)?;")
        .expect("host planning classifier call");
    let provider = turn
        .find(".drive_input_unaccounted(guard, options.routing.as_ref(), events.clone())")
        .expect("provider turn call");

    assert!(
        persisted < classified && classified < provider,
        "the host must classify only after the user input is durable and before the provider call"
    );
}

#[test]
fn promoted_subagent_report_persists_host_metadata_on_the_user_message() {
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    {
        let mut connection = pool.get().expect("schema connection");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-report', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES \
                   ('ses-report', 'project-report', 'report', '/workspace', 'report', '1', 1, 1);",
            )
            .expect("seed report session");
    }
    let inbox = zuno_db::inbox::SessionInbox::new(Arc::clone(&pool));
    inbox
        .admit(zuno_db::inbox::NewSessionInput::new(
            "input-report",
            "ses-report",
            json!({
                "kind": "subagentReport",
                "jobID": "job-report",
                "childSessionID": "ses-child",
                "status": "completed",
                "text": "background result",
                "metadata": {
                    "schemaVersion": 1,
                    "agent": "explorer",
                    "finalText": "background result"
                }
            }),
            zuno_db::inbox::InputDelivery::Queue,
            2,
        ))
        .expect("admit report");
    inbox
        .promote_id("ses-report", "input-report")
        .expect("promote report")
        .expect("queued report");
    let connection = pool.get().expect("persistence connection");
    let mut message = zuno_db::message::MessageRecord::from_json(json!({
        "id": "input-report",
        "sessionID": "ses-report",
        "role": "user",
        "time": {"created": 3},
        "agent": "orchestrator",
        "model": {"providerID": "fake", "modelID": "fake-model"}
    }))
    .expect("user message");
    let part = zuno_db::message::PartRecord::from_json(
        json!({
            "id": "part-report",
            "sessionID": "ses-report",
            "messageID": "input-report",
            "type": "text",
            "text": "background result"
        }),
        3,
    )
    .expect("report text part");
    let transaction = zuno_db::open::immediate_transaction(&connection).expect("begin transaction");
    attach_promoted_task_report_metadata(&transaction, &mut message)
        .expect("attach task report metadata");
    persist_prepared_user_message(&transaction, &message, &[part]).expect("persist report message");
    consume_promoted_input(&transaction, "ses-report", "input-report").expect("consume report");
    transaction.commit().expect("commit report");

    let stored = zuno_db::message::MessageStore::new(&connection)
        .message("input-report")
        .expect("stored report message");
    assert_eq!(stored.data["taskReport"]["agent"], "explorer");
    assert_eq!(stored.data["taskReport"]["finalText"], "background result");
}

fn test_capability() -> Arc<CapabilitySnapshot> {
    Arc::new(CapabilitySnapshot::new(
        PackIdentity {
            id: zuno_orchestration::PACK_ID.to_owned(),
            version: zuno_orchestration::PACK_VERSION.to_owned(),
            upstream_revision: zuno_orchestration::CAPABILITY_REVIEW_REVISION.to_owned(),
        },
        0,
        sha256_text("test permission policy"),
        CapabilityContents::default(),
    ))
}

fn test_capability_with_council() -> Arc<CapabilitySnapshot> {
    let mut capability = test_capability().as_ref().clone();
    capability.councils = council_descriptors();
    Arc::new(capability)
}

/// The delegation collaborators a test supplies to reach [`tool_runtime::assemble`].
///
/// A recording host and no catalog facts, because none of these assertions drive a
/// child turn. That this compiles at all is the point the production wiring rests on:
/// `Delegation` is a required field, so `turn.rs` cannot assemble a turn's tools
/// without handing over a real `ChildTurnHost`.
fn test_delegation() -> tool_runtime::Delegation {
    tool_runtime::Delegation {
        host: Arc::new(zuno_tools::task::RecordingHost::new()),
        facts: Arc::new(zuno_tools::task::NoProviders),
        targets: zuno_tools::task::DelegationTargets::new(zuno_tools::task::valid_targets(false))
            .expect("native delegation targets are valid"),
        agent_models: Vec::new(),
        session_model: zuno_agent::model_policy::ModelChoice::new("provider/model"),
        presets: zuno_agent::model_policy::PresetLibrary::new(),
        limits: zuno_tools::task::DelegationLimits::default(),
        vision_available: false,
    }
}

fn test_background_executions(directory: &Path) -> Arc<zuno_pty::BackgroundExecutionService> {
    Arc::new(
        zuno_pty::BackgroundExecutionService::open(directory.join(".background"))
            .expect("test background execution service"),
    )
}

#[test]
fn workflow_job_projection_types_councils_and_keeps_child_progress_in_order() {
    fn item(
        id: &str,
        parent_id: Option<&str>,
        subject: &str,
        owner: &str,
        status: zuno_tools::WorkItemStatus,
        tokens: i64,
    ) -> zuno_tools::WorkItem {
        zuno_tools::WorkItem {
            id: id.to_owned(),
            session_id: "ses_parent".to_owned(),
            goal_id: None,
            plan_step_id: None,
            parent_id: parent_id.map(str::to_owned),
            subject: subject.to_owned(),
            description: format!("{subject} description"),
            active_form: Some(format!("Running {subject}")),
            status,
            priority: zuno_tools::WorkItemPriority::Medium,
            dependencies: Vec::new(),
            owner: Some(owner.to_owned()),
            revision: 1,
            tokens_used: tokens,
            usage_known: true,
            time_used_ms: 5_000,
            time_created: 1_000,
            time_updated: 6_000,
        }
    }

    let council = zuno_db::job::JobSubject::workflow("run_council", "council:balanced-review");
    assert_eq!(
        project_job_subject(&council),
        zuno_types::JobSubjectProjection::Council {
            run_id: "run_council".to_owned(),
            preset: "balanced-review".to_owned(),
        }
    );

    let items = vec![
        item(
            "work_run_council:node:0",
            Some("work_run_council"),
            "evidence",
            "explorer",
            zuno_tools::WorkItemStatus::Completed,
            120,
        ),
        item(
            "work_run_council:node:1",
            Some("work_run_council"),
            "judgment",
            "oracle",
            zuno_tools::WorkItemStatus::InProgress,
            80,
        ),
        item(
            "work_somewhere_else:node:0",
            Some("work_somewhere_else"),
            "unrelated",
            "general",
            zuno_tools::WorkItemStatus::Pending,
            0,
        ),
    ];
    let children = project_job_children(&council, &items);
    assert_eq!(children.len(), 2, "{children:#?}");
    assert_eq!(children[0].subject, "evidence");
    assert_eq!(children[0].owner.as_deref(), Some("explorer"));
    assert_eq!(children[0].status, "completed");
    assert_eq!(children[0].span.usage.total(), 120);
    assert_eq!(children[1].subject, "judgment");
    assert_eq!(children[1].owner.as_deref(), Some("oracle"));
    assert_eq!(children[1].status, "in_progress");

    let workflow = zuno_db::job::JobSubject::workflow("run_release", "release-hardening");
    assert_eq!(
        project_job_subject(&workflow),
        zuno_types::JobSubjectProjection::Workflow {
            run_id: "run_release".to_owned(),
            workflow: "release-hardening".to_owned(),
        }
    );
    let workflow_children = project_job_children(
        &workflow,
        &[item(
            "work_run_release:node:0",
            Some("work_run_release"),
            "verify",
            "fixer",
            zuno_tools::WorkItemStatus::Pending,
            0,
        )],
    );
    assert_eq!(workflow_children.len(), 1);
    assert_eq!(workflow_children[0].subject, "verify");
    assert_eq!(workflow_children[0].owner.as_deref(), Some("fixer"));
    assert!(
        project_job_children(
            &zuno_db::job::JobSubject::child_session("ses_child"),
            &items,
        )
        .is_empty()
    );
}

#[derive(Debug, Default)]
struct NoProductAgents;

#[async_trait::async_trait]
impl zuno_tools::product_agent::ProductAgentHost for NoProductAgents {
    async fn dispatch(
        &self,
        _request: zuno_tools::product_agent::ProductAgentRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<zuno_tools::product_agent::ProductAgentTurn, String> {
        Err("test fixture has no product agents".to_owned())
    }
}

#[derive(Debug, Default)]
struct NoJobController;

#[async_trait::async_trait]
impl zuno_tools::job_cancel::JobController for NoJobController {
    async fn cancel(
        &self,
        _parent_session_id: &str,
        _job_id: &str,
    ) -> Result<zuno_tools::job_cancel::CancelOutcome, String> {
        Ok(zuno_tools::job_cancel::CancelOutcome {
            requested: false,
            message: "no live fixture job".to_owned(),
        })
    }
}

fn test_product_agents() -> Arc<dyn zuno_tools::product_agent::ProductAgentHost> {
    Arc::new(NoProductAgents)
}

#[derive(Debug, Default)]
struct NoWorkflows;

#[async_trait::async_trait]
impl zuno_tools::workflow::WorkflowHost for NoWorkflows {
    async fn dispatch(
        &self,
        _request: zuno_tools::workflow::WorkflowRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<zuno_tools::workflow::WorkflowTurn, String> {
        Err("test fixture has no workflows".to_owned())
    }
}

fn test_workflows() -> Arc<dyn zuno_tools::workflow::WorkflowHost> {
    Arc::new(NoWorkflows)
}

#[derive(Debug, Default)]
struct NoCouncils;

#[async_trait::async_trait]
impl zuno_tools::council::CouncilHost for NoCouncils {
    async fn dispatch(
        &self,
        _request: zuno_tools::council::CouncilRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<zuno_tools::council::CouncilTurn, String> {
        Err("test fixture has no Councils".to_owned())
    }
}

fn test_councils() -> Arc<dyn zuno_tools::council::CouncilHost> {
    Arc::new(NoCouncils)
}

fn test_job_controller() -> Arc<dyn zuno_tools::job_cancel::JobController> {
    Arc::new(NoJobController)
}

fn plan(directory: &str, session: SessionChoice) -> TurnPlan {
    let directory = PathBuf::from(directory);
    let auth_store = zuno_auth::AuthStore::new(directory.join(".zuno-test-auth.json"));
    let project = zuno_paths::project::ResolvedProject {
        previous: None,
        id: "project-turn-test".to_owned(),
        directory: directory.clone(),
        vcs: None,
    };
    let agent = agent("build");
    let config = zuno_config::schema::Config::default();
    let profile = agent_profile(agent.clone(), &directory, &config);
    let extension_scope = zuno_extension::Scope::new(&directory);
    TurnPlan {
        profile: zuno_harness::default_profile(),
        resolver: Resolver {
            requested_agent: agent.name.clone(),
            system_prompt: String::new(),
            prompt_assembly: None,
            runtime_prompt_policy: RuntimePromptPolicy::default(),
            max_steps: None,
            requested_provider: "provider".to_owned(),
            requested_model: "model".to_owned(),
            wire_model: "model".to_owned(),
            spec: Spec::new(COMPATIBLE_PROVIDER).with_surface(ApiSurface::Chat),
            reasoning_options: serde_json::Map::new(),
            orchestration_seed: None,
        },
        catalog_models: Vec::new(),
        reasoning_efforts: std::collections::BTreeMap::new(),
        skills: Arc::new(zuno_catalog::skill::Skills::default()),
        required_skills: Vec::new(),
        capability: test_capability(),
        tool_authority: None,
        agents: vec![agent.clone()],
        extensions: zuno_extension::ResolvedExtensions::default(),
        configured_extension_tool_ids: Vec::new(),
        extension_scope,
        extension_revision: 0,
        extension_transaction: None,
        extension_prepared: None,
        instructions: zuno_config::LoadedInstructions::default(),
        delegation_facts: Arc::new(zuno_tools::task::FixedFacts::new()),
        vision_available: false,
        reasoning_supported: false,
        is_delegated: false,
        effort: None,
        effective_variant: None,
        effort_override: None,
        variant_override: None,
        thinking_override: false,
        goal_retry_policy: GoalRetryPolicy::default(),
        directory,
        project,
        env: zuno_paths::Env::empty(),
        config,
        agent: profile,
        provider_id: "provider".to_owned(),
        model_id: "model".to_owned(),
        model_override: None,
        auth_store,
        credential: None,
        session,
        title: None,
        internals: stub_internals(),
        presets: PresetLibrary::new(),
        reflection_model: None,
        window: TokenWindow {
            context: 0,
            max_output: 0,
        },
        notes: Vec::new(),
    }
}

#[test]
fn debug_agent_snapshot_reports_the_effective_runtime_dimensions() {
    let plan = plan("/tmp", SessionChoice::New);
    let snapshot = plan.debug_agent_snapshot();

    assert_eq!(snapshot["schemaVersion"], 2);
    assert_eq!(snapshot["agent"]["name"], "build");
    assert_eq!(snapshot["model"]["effective"], "provider/model");
    assert!(snapshot["model"].get("reasoningSupported").is_some());
    assert!(snapshot["tools"]["policyVisible"].is_array());
    assert!(snapshot["tools"]["unavailable"].is_array());
    assert!(
        snapshot["tools"]["policyVisible"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                snapshot["tools"]["unavailable"]
                    .as_array()
                    .into_iter()
                    .flatten()
            )
            .any(|tool| tool["id"] == zuno_tools::JOB_RECONCILE_WIRE_ID),
        "debug output must account for the registered uncertain-job reconciliation tool"
    );
    assert!(snapshot["mcp"]["servers"].is_array());
    assert!(snapshot["skills"]["required"].is_array());
    assert_eq!(snapshot["skills"]["parentExpandedBodiesInherited"], false);
    assert!(snapshot.get("delegates").is_some());
    assert!(snapshot["sandbox"].get("configuredMode").is_some());
    assert!(
        snapshot["policySources"]
            .as_array()
            .is_some_and(|sources| !sources.is_empty())
    );
}

#[test]
fn debug_agent_reports_mcp_inheritance_as_unresolved_until_tools_are_connected() {
    let mut plan = plan("/tmp", SessionChoice::New);
    let mut definition = plan.agent.definition().clone();
    definition.name = "exact".to_owned();
    definition.tools = Some(vec!["known_mcp_tool".to_owned()]);
    plan.agent = zuno_agent::profile::AgentProfile::resolve(
        definition,
        plan.agent.capabilities().rules().to_vec(),
        false,
    );

    let snapshot = plan.debug_agent_snapshot();

    assert_eq!(snapshot["mcp"]["inheritance"]["state"], "not-connected");
    assert_eq!(
        snapshot["mcp"]["inheritance"]["reason"],
        "MCP tool ids are known only after a live connection; role rules and the Agent allowlist are evaluated per discovered tool, and no parent Attempt authority applies to this root diagnostic"
    );
    assert!(
        snapshot["mcp"].get("inheritsConnectedTools").is_none(),
        "a synthetic probe must not claim that an exact allowlist accepts or rejects unknown MCP ids"
    );
}

#[test]
fn debug_agent_evaluates_live_mcp_tools_and_exact_parent_schema_authority() {
    let mut plan = plan("/tmp", SessionChoice::New);
    let tool = ToolSchemaIdentity {
        name: "known_mcp_tool".to_owned(),
        description_sha256: sha256_text("connected description"),
        schema_sha256: sha256_text("connected schema"),
        ui_intent: "generic".to_owned(),
    };
    let parent = ToolSchemaIdentity {
        description_sha256: sha256_text("parent description"),
        ..tool.clone()
    };
    plan.tool_authority = Some(Arc::from([parent]));
    plan.agent = plan
        .agent
        .clone()
        .with_tool_authority(["known_mcp_tool".to_owned()]);
    let diagnostics = crate::cmd::mcp_runtime::McpRuntimeDiagnostics {
        discovery_status: "ready".to_owned(),
        servers: vec![crate::cmd::mcp_runtime::McpServerDiagnostic {
            name: "codegraph".to_owned(),
            desired_enabled: true,
            state: "connected".to_owned(),
            error: None,
        }],
        connected_servers: vec!["codegraph".to_owned()],
        tools: vec![tool],
        warnings: Vec::new(),
        cleanup_warnings: Vec::new(),
    };

    let snapshot = plan.debug_agent_snapshot_with_mcp(Some(&diagnostics));

    assert_eq!(snapshot["mcp"]["inheritance"]["state"], "evaluated");
    assert_eq!(
        snapshot["mcp"]["inheritance"]["reason"],
        "connected MCP tool ids and exact schemas were evaluated against role rules, the Agent allowlist, and parent Attempt authority"
    );
    assert_eq!(snapshot["mcp"]["discoveryStatus"], "ready");
    assert_eq!(snapshot["mcp"]["connectedServers"][0], "codegraph");
    assert!(
        snapshot["tools"]["unavailable"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|entry| {
                entry["id"] == "known_mcp_tool"
                    && entry["reason"] == "schema differs from the parent attempt tool authority"
            }))
    );
    assert!(snapshot["skills"]["summary"]["metadataBudgetBytes"].is_number());
    assert!(
        snapshot["skills"]["summary"]["previewOmitted"]
            .as_u64()
            .is_some()
    );
}

#[test]
fn debug_agent_live_root_mcp_does_not_claim_parent_attempt_authority() {
    let plan = plan("/tmp", SessionChoice::New);
    let diagnostics = crate::cmd::mcp_runtime::McpRuntimeDiagnostics {
        discovery_status: "ready".to_owned(),
        servers: vec![crate::cmd::mcp_runtime::McpServerDiagnostic {
            name: "codegraph".to_owned(),
            desired_enabled: true,
            state: "connected".to_owned(),
            error: None,
        }],
        connected_servers: vec!["codegraph".to_owned()],
        tools: Vec::new(),
        warnings: Vec::new(),
        cleanup_warnings: Vec::new(),
    };

    let snapshot = plan.debug_agent_snapshot_with_mcp(Some(&diagnostics));

    assert!(snapshot["tools"]["parentAuthority"].is_null());
    assert_eq!(
        snapshot["mcp"]["inheritance"]["reason"],
        "connected MCP tool ids and exact schemas were evaluated against role rules and the Agent allowlist; no parent Attempt authority applies to this root diagnostic"
    );
}

#[test]
fn debug_agent_marks_skill_prompt_metadata_disabled_without_hiding_tool_budgets() {
    let mut plan = plan("/tmp", SessionChoice::New);
    plan.config.skills = Some(zuno_config::schema::SkillsConfig {
        include_instructions: Some(false),
        ..zuno_config::schema::SkillsConfig::default()
    });

    let snapshot = plan.debug_agent_snapshot();

    assert_eq!(snapshot["skills"]["summary"]["metadataEnabled"], false);
    assert!(snapshot["skills"]["summary"]["metadataBudgetBytes"].is_null());
    assert!(snapshot["skills"]["summary"]["metadataBudgetApproxTokens"].is_null());
    assert!(snapshot["skills"]["summary"]["metadataCoverage"].is_null());
    assert!(
        snapshot["skills"]["summary"]["selectedBodyBudgetBytes"].is_number(),
        "includeInstructions only removes automatic prompt metadata; selected bodies remain bounded"
    );
}

fn orchestration_seed(capability: &CapabilitySnapshot) -> Arc<AttemptSeed> {
    Arc::new(AttemptSeed {
        capability: capability.clone(),
        agent: AgentAttemptIdentity {
            name: "build".to_owned(),
            source_id: "test://agent/build".to_owned(),
            definition_sha256: sha256_text("build definition"),
            permission_sha256: sha256_text("build permission"),
            prompt_policy_sha256: sha256_text("build prompt policy"),
        },
        preset: None,
        parent_attempt: None,
        workflow: None,
        workflow_node: None,
    })
}

fn parent_attempt(capability: CapabilitySnapshot) -> AttemptSnapshot {
    AttemptSnapshot {
        schema_version: zuno_orchestration::SNAPSHOT_SCHEMA_VERSION,
        turn_id: "turn-parent".to_owned(),
        step: 1,
        capability,
        owner: zuno_orchestration::OwnerLineage {
            session_id: "ses-parent".to_owned(),
            parent_session_id: None,
            parent_attempt: None,
            workflow: None,
            workflow_node: None,
        },
        agent: orchestration_seed(test_capability().as_ref()).agent.clone(),
        model: zuno_orchestration::ModelAttemptIdentity {
            provider_id: "provider".to_owned(),
            model_id: "model".to_owned(),
            wire_model_id: "model".to_owned(),
            surface: "responses".to_owned(),
            reasoning_sha256: sha256_text("max"),
            preset: None,
        },
        selected_skills: Vec::new(),
        prompt: zuno_orchestration::PromptReceiptIdentity {
            event_id: Some("evt-parent".to_owned()),
            assembly_sha256: sha256_text("assembly"),
            actual_sha256: sha256_text("actual"),
        },
        tools: Vec::new(),
    }
}

#[test]
fn delegated_agent_attempt_identity_hashes_the_full_parent_tool_schema_authority() {
    let directory = tempfile::TempDir::new().expect("temporary workspace");
    let profile = agent_profile(
        agent("deep"),
        directory.path(),
        &zuno_config::schema::Config::default(),
    );
    let first = ToolSchemaIdentity {
        name: "codegraph_query".to_owned(),
        description_sha256: sha256_text("first description"),
        schema_sha256: sha256_text("same schema"),
        ui_intent: "generic".to_owned(),
    };
    let changed = ToolSchemaIdentity {
        description_sha256: sha256_text("changed description"),
        ..first.clone()
    };

    let unrestricted = agent_attempt_identity(&profile, None).expect("unrestricted identity");
    let first_identity = agent_attempt_identity(&profile, Some(std::slice::from_ref(&first)))
        .expect("first authority identity");
    let changed_identity = agent_attempt_identity(&profile, Some(std::slice::from_ref(&changed)))
        .expect("changed authority identity");

    assert_eq!(
        first_identity.definition_sha256,
        changed_identity.definition_sha256
    );
    assert_ne!(
        unrestricted.permission_sha256,
        first_identity.permission_sha256
    );
    assert_ne!(
        first_identity.permission_sha256,
        changed_identity.permission_sha256
    );
}

#[test]
fn delegated_turn_inherits_the_parent_attempt_and_workflow_lineage() {
    let mut delegated = plan("/workspace", SessionChoice::New);
    delegated.resolver.orchestration_seed = Some(orchestration_seed(delegated.capability.as_ref()));
    let parent = parent_attempt(delegated.capability.as_ref().clone());
    let parent_identity = parent.identity().expect("parent attempt identity");

    delegated
        .inherit_orchestration(&parent, Some("release"), Some("implement"))
        .expect("matching capability generation is inherited");

    let inherited = delegated
        .resolver
        .orchestration_seed
        .as_deref()
        .expect("delegated orchestration seed");
    assert_eq!(inherited.capability, parent.capability);
    assert_eq!(inherited.parent_attempt.as_ref(), Some(&parent_identity));
    assert_eq!(inherited.workflow.as_deref(), Some("release"));
    assert_eq!(inherited.workflow_node.as_deref(), Some("implement"));
}

#[test]
fn delegated_turn_rejects_a_drifted_capability_generation() {
    let mut delegated = plan("/workspace", SessionChoice::New);
    delegated.resolver.orchestration_seed = Some(orchestration_seed(delegated.capability.as_ref()));
    let mut drifted = delegated.capability.as_ref().clone();
    drifted.extension_revision = drifted.extension_revision.saturating_add(1);
    let parent = parent_attempt(drifted);

    let error = delegated
        .inherit_orchestration(&parent, None, None)
        .expect_err("drifted capability generation must not execute");

    assert!(error.contains("stale or mismatched"), "{error}");
    assert!(error.contains("refresh the parent turn"), "{error}");
}

#[test]
fn delegated_turn_rejects_broader_sandbox_authority_than_the_parent_attempt() {
    let mut delegated = plan("/workspace", SessionChoice::New);
    delegated.resolver.orchestration_seed = Some(orchestration_seed(delegated.capability.as_ref()));
    let mut parent_capability = delegated.capability.as_ref().clone();
    parent_capability.sandbox.mode = "read-only".to_owned();
    parent_capability.sandbox.network = "deny".to_owned();
    let parent = parent_attempt(parent_capability);

    let error = delegated
        .inherit_orchestration(&parent, None, None)
        .expect_err("a child must not re-resolve broader sandbox authority");

    assert!(error.contains("stale or mismatched"), "{error}");
    assert!(error.contains("refresh the parent turn"), "{error}");
}

#[tokio::test]
async fn prepared_extension_aborts_cleanly_when_session_preparation_fails() {
    let directory = tempfile::tempdir().expect("workspace");
    let environment = crate::environment::StartupEnvironment::resolve(
        &Env::empty(),
        &crate::GlobalOptions::default(),
    );
    let scope = zuno_extension::Scope::new(directory.path());
    let package = serde_json::from_value(serde_json::json!({
        "apiVersion": zuno_extension::API_VERSION,
        "id": "review",
        "description": "review extension",
        "workflows": {
            "review": {
                "description": "review",
                "prompt": "Review the change."
            }
        }
    }))
    .expect("valid extension");
    environment
        .extensions()
        .define(&scope, package)
        .expect("define");
    let transaction = match environment
        .extensions()
        .stage_run(&scope, "review", &[])
        .expect("stage")
    {
        zuno_extension::StageOutcome::Pending(transaction) => transaction,
        zuno_extension::StageOutcome::Unchanged { .. } => panic!("activation must stage"),
    };
    let prepared = environment
        .extensions()
        .begin_transition(&transaction)
        .expect("reserve");
    let mut candidate = plan(
        directory.path().to_str().expect("utf-8 workspace"),
        SessionChoice::Existing("ses_missing".to_owned()),
    );
    candidate.extension_scope = scope.clone();
    candidate.extension_revision = transaction.revision();
    candidate.extension_transaction = Some(transaction);
    candidate
        .use_prepared_extension_transition(prepared)
        .expect("attach reservation");
    let database =
        Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));

    let opened = TurnHost::open_with_dependencies(
        candidate,
        &environment,
        TurnHostDependencies {
            approval: Arc::new(zuno_tool::AllowAll),
            question: None,
            runs: SessionRunRegistry::new(),
            mcp: None,
            database,
            child_observer: None,
        },
    )
    .await;
    assert!(opened.is_err(), "missing session prevents host preparation");

    assert_eq!(
        environment.extensions().dynamic_statuses(&scope)[0].state,
        zuno_extension::DynamicState::Defined,
        "a side-effect-free preparation failure must abort, not poison, the transition"
    );
    assert!(environment.extensions().uncertainty(&scope).is_none());
    assert_eq!(
        environment.extensions().desired_revision(&scope),
        environment.extensions().active_revision(&scope)
    );
}

#[test]
fn goal_retry_policy_resolves_defaults_and_rejects_invalid_ranges() {
    let defaults =
        resolve_goal_retry_policy(&zuno_config::schema::Config::default()).expect("defaults");
    assert_eq!(
        defaults.delay(1, None, 0),
        DEFAULT_GOAL_RETRY_INITIAL_DELAY.saturating_sub(DEFAULT_GOAL_RETRY_INITIAL_DELAY / 5)
    );
    assert_eq!(defaults.poll_interval(), DEFAULT_GOAL_RETRY_POLL_INTERVAL);

    let mut config = zuno_config::schema::Config {
        goal: Some(zuno_config::schema::GoalConfig {
            retry: Some(zuno_config::schema::GoalRetryConfig {
                initial_delay_ms: std::num::NonZeroU64::new(5_000),
                max_delay_ms: std::num::NonZeroU64::new(1_000),
                jitter_percent: Some(0),
                poll_interval_ms: std::num::NonZeroU64::new(100),
            }),
        }),
        ..Default::default()
    };
    let error = resolve_goal_retry_policy(&config).expect_err("max before initial is invalid");
    assert!(error.contains("max delay"), "{error}");

    config.goal.as_mut().expect("goal").retry = Some(zuno_config::schema::GoalRetryConfig {
        initial_delay_ms: std::num::NonZeroU64::new(1_000),
        max_delay_ms: std::num::NonZeroU64::new(5_000),
        jitter_percent: Some(101),
        poll_interval_ms: std::num::NonZeroU64::new(100),
    });
    let error = resolve_goal_retry_policy(&config).expect_err("jitter above 100 is invalid");
    assert!(error.contains("0..=100"), "{error}");
}

#[test]
fn unresolved_tool_failures_choose_safe_or_uncertain_goal_recovery() {
    assert_eq!(goal_tool_failure(&[]), None);

    let safe = ToolFailureRecovery {
        tool: "web_search".to_owned(),
        replay_policy: zuno_tool::ToolReplayPolicy::Safe,
        retry_after: Some(Duration::from_secs(3)),
    };
    assert_eq!(
        goal_tool_failure(std::slice::from_ref(&safe)),
        Some(GoalTerminalFailure::Retry {
            reason: GoalRetryReason::ToolTransient,
            retry_after: Some(Duration::from_secs(3)),
        })
    );

    let uncertain = ToolFailureRecovery {
        tool: "shell".to_owned(),
        replay_policy: zuno_tool::ToolReplayPolicy::Never,
        retry_after: Some(Duration::from_secs(7)),
    };
    assert_eq!(
        goal_tool_failure(&[safe, uncertain]),
        Some(GoalTerminalFailure::Pause(
            zuno_goal::GoalPauseReason::UncertainSideEffect
        ))
    );
}

#[test]
fn prepared_user_message_persistence_preserves_database_busy() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let location = zuno_paths::DbLocation::File(directory.path().join("locked.db"));
    let mut connection = zuno_db::open::open(&location).expect("open primary connection");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    connection
        .busy_timeout(std::time::Duration::ZERO)
        .expect("disable retry delay for the lock assertion");
    let mut blocker = zuno_db::open::open(&location).expect("open blocking connection");
    let _write_lock = blocker
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("hold the database write lock");
    let (message, parts) = prepare_user_message(
        UserMessageInput {
            session_id: "ses_locked",
            agent: "build",
            provider_id: "provider",
            model_id: "model",
            text: "Persist me after the writer releases the lock.",
            message_id: Some("msg_locked"),
            now: 1_780_000_000_000,
        },
        None,
    )
    .expect("prepare user message");

    let error = persist_prepared_user_message(&connection, &message, &parts)
        .expect_err("the active writer must make this write retryable");

    assert!(matches!(error, zuno_error::DbError::Busy { .. }));
}

fn stub_internals() -> Internals {
    let agent = |name: &str| InternalAgent {
        name: name.to_owned(),
        prompt: String::new(),
        model: EngineModel::new(Spec::new(COMPATIBLE_PROVIDER), "model", ApiSurface::Chat),
    };
    Internals {
        title: agent("title"),
        compaction: agent("compaction"),
        summary: agent("summary"),
        council_synth: agent("council-synth"),
    }
}

#[test]
fn resolved_prompt_blocks_become_the_text_and_file_parts_the_engine_projects() {
    let input = UserMessageInput {
        session_id: "ses_reference",
        agent: "build",
        provider_id: "provider",
        model_id: "model",
        text: "inspect @note.txt @diagram.png",
        message_id: None,
        now: 1_780_000_000_000,
    };
    let content = vec![
        RequestContentBlock::Text {
            text: "inspect @note.txt @diagram.png".to_owned(),
        },
        RequestContentBlock::Text {
            text: "--- BEGIN REFERENCED FILE: note.txt ---\nreal body\n--- END REFERENCED FILE: note.txt ---".to_owned(),
        },
        RequestContentBlock::Image {
            filename: Some("diagram.png".to_owned()),
            media_type: "image/png".to_owned(),
            data: "aW1hZ2U=".to_owned(),
        },
    ];

    let parts = request_content_parts(&input, "msg_reference", &content)
        .expect("text and image request blocks are valid user content");

    assert_eq!(
        parts
            .iter()
            .filter(|part| part.kind == zuno_db::message::PartKind::Text)
            .count(),
        2
    );
    let image = parts
        .iter()
        .find(|part| part.kind == zuno_db::message::PartKind::File)
        .expect("the image became a stored file part");
    assert_eq!(image.data["mime"], "image/png");
    assert_eq!(image.data["filename"], "diagram.png");
    assert_eq!(image.data["data"], "aW1hZ2U=");
}

#[test]
fn production_prompt_composition_honours_the_memory_master_switch() {
    let directory = tempfile::TempDir::new().expect("temporary memory paths");
    let paths = zuno_memory::ScopePaths::at(
        directory.path().join("global/MEMORY.md"),
        directory.path().join("project/RULES.md"),
    );
    let mut seeded = zuno_memory::MemoryStore::open(
        zuno_memory::Scope::Project,
        paths.for_scope(zuno_memory::Scope::Project).to_path_buf(),
    )
    .expect("seeded project memory");
    seeded
        .apply_batch(&[zuno_memory::Operation::add(
            "production composition sentinel",
        )])
        .expect("seed memory");
    let base = "SYSTEM\r\n${UNCHANGED}\n终";
    let resolver = || traced_resolver(base);

    let mut disabled = resolver();
    let config = serde_json::from_str(r#"{"memory":false}"#).expect("disabled config");
    configure_resident_memory(&mut disabled, &config, paths.clone()).expect("disabled path");
    assert_eq!(disabled.system_prompt.as_bytes(), base.as_bytes());

    let mut enabled = resolver();
    configure_resident_memory(&mut enabled, &zuno_config::schema::Config::default(), paths)
        .expect("enabled path");
    assert!(
        enabled
            .system_prompt
            .contains("production composition sentinel")
    );
    assert_ne!(enabled.system_prompt.as_bytes(), base.as_bytes());
}

#[tokio::test]
async fn prompt_assembly_records_agent_memory_instructions_and_skills_in_order() {
    let root = tempfile::TempDir::new().expect("temporary prompt root");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    let paths = zuno_memory::ScopePaths::at(
        root.path().join("global/MEMORY.md"),
        repo.join(".zuno/RULES.md"),
    );
    for (scope, marker) in [
        (zuno_memory::Scope::Global, "GLOBAL_MEMORY"),
        (zuno_memory::Scope::Project, "PROJECT_MEMORY"),
    ] {
        let mut store = zuno_memory::MemoryStore::open(scope, paths.for_scope(scope).to_path_buf())
            .expect("open memory store");
        store
            .apply_batch(&[zuno_memory::Operation::add(marker)])
            .expect("seed memory");
    }
    std::fs::write(repo.join("AGENTS.md"), "PROJECT_INSTRUCTIONS").expect("write instructions");
    let env = Env::empty()
        .with(
            zuno_paths::env::HOME,
            root.path().join("home").to_string_lossy().into_owned(),
        )
        .with(
            zuno_paths::env::XDG_CONFIG_HOME,
            root.path()
                .join("home/.config")
                .to_string_lossy()
                .into_owned(),
        );
    let loaded = zuno_config::Instructions::discover(&zuno_config::InstructionOptions::new(
        repo.clone(),
        Some(repo.clone()),
        &env,
        Vec::new(),
    ))
    .load()
    .await;
    let skills =
        zuno_catalog::skill::Skills::from_loaded([zuno_catalog::skill::Skill::embedded_at_path(
            "verify",
            Some("Verify the assembled result.".to_owned()),
            PathBuf::from("/skills/verify/SKILL.md"),
            "body",
        )]);
    let mut resolver = traced_resolver("AGENT");
    let mut notes = Vec::new();
    configure_resident_memory(
        &mut resolver,
        &zuno_config::schema::Config::default(),
        paths,
    )
    .expect("inject memory");
    resolver
        .append_prompt_section(
            "extensions",
            "zuno-extension::active-packages",
            zuno_extension::ResolvedExtensions::default().prompt_section(),
        )
        .expect("inject extension provenance");
    announce_instructions(&mut resolver, &loaded, &mut notes).expect("inject instructions");
    announce_skills(&mut resolver, &skills, 0, None).expect("inject skills");

    let assembly = resolver
        .prompt_assembly
        .as_ref()
        .expect("structured prompt assembly");
    assert_eq!(
        assembly
            .sections()
            .iter()
            .map(|section| section.id())
            .collect::<Vec<_>>(),
        vec![
            "agent.base",
            "instructions.project.0",
            "extensions",
            "skills.policy",
            "skills.index",
            "memory.global",
            "memory.project",
        ]
    );
    assert_eq!(
        assembly.sections()[5].source(),
        root.path().join("global/MEMORY.md").display().to_string()
    );
    assert_eq!(
        assembly.sections()[6].source(),
        repo.join(".zuno/RULES.md").display().to_string()
    );
    assert_eq!(
        assembly.sections()[2].source(),
        "zuno-extension::active-packages"
    );
    assert_eq!(
        assembly.sections()[1].source(),
        repo.join("AGENTS.md").display().to_string()
    );
    assert_eq!(assembly.sections()[3].source(), "zuno skill trigger policy");
    assert_eq!(assembly.sections()[4].source(), "discovered skill index");
    assert_eq!(resolver.system_prompt, assembly.render());
    assert!(notes.is_empty(), "{notes:?}");
}

/// Two models under one provider, with `title` overridden to the smaller one.
///
/// The provider carries an endpoint in `options.baseURL`. It has to: a provider with no
/// endpoint in either place is one no turn could ever run against, and building specs
/// from it was how a spec with no base URL used to pass unnoticed. This is the same
/// lesson as the top-level `api` key the seam tests no longer send — a fixture must not
/// be servable in ways the real input shape is not, and must not be unservable in ways
/// it would not be either.
fn catalog_with_two_models_and_a_title_override() -> (Catalog, zuno_config::schema::Config) {
    let document = serde_json::from_str(
        r#"{"test":{"id":"test","name":"Test","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{
               "big":{"id":"big","name":"Big","limit":{"context":200000,"output":8192}},
               "small":{"id":"small","name":"Small","limit":{"context":100000,"output":4096}}}}}"#,
    )
    .expect("catalog document");
    let config = serde_json::from_str(
        r#"{"provider":{"test":{"options":{"baseURL":"https://gateway.test/v1"}}},
             "agents":{"title":{"model":"test/small"}}}"#,
    )
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    (catalog, config)
}

#[test]
fn model_selection_splits_only_the_provider_prefix() {
    let document = serde_json::from_str(
        r#"{"anyapi":{"id":"anyapi","name":"AnyAPI","env":[],"models":{"openai/gpt":{"id":"openai/gpt","name":"GPT","limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config = serde_json::from_str(r#"{"provider":{"anyapi":{}}}"#).expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let (provider, model, _) = select_model(
        &catalog,
        Some("anyapi/openai/gpt"),
        &CatalogProvenance::Fetched,
    )
    .expect("nested model id");
    assert_eq!(provider, "anyapi");
    assert_eq!(model, "openai/gpt");
}

#[test]
fn every_declared_wire_transport_selects_its_production_registry_key() {
    let cases = [
        (ProviderTransport::Anthropic, "anthropic"),
        (ProviderTransport::Bedrock, "amazon-bedrock"),
        (ProviderTransport::BedrockMantle, "amazon-bedrock/mantle"),
        (ProviderTransport::Google, "google"),
        (ProviderTransport::GoogleVertex, "google-vertex"),
        (
            ProviderTransport::GoogleVertexAnthropic,
            "google-vertex/anthropic",
        ),
        (ProviderTransport::Openai, "openai"),
        (ProviderTransport::OpenaiCompatible, COMPATIBLE_PROVIDER),
        (ProviderTransport::Openrouter, COMPATIBLE_PROVIDER),
    ];

    for (transport, expected) in cases {
        assert_eq!(
            provider_factory_key(Some(transport)),
            Some(expected),
            "resolved native transport `{transport}` selected the wrong production factory"
        );
    }
    assert_eq!(provider_factory_key(None), None);
}

fn named_compatible_cases() -> [(&'static str, ProviderTransport); 15] {
    [
        ("openrouter", ProviderTransport::Openrouter),
        ("xai", ProviderTransport::OpenaiCompatible),
        ("mistral", ProviderTransport::OpenaiCompatible),
        ("groq", ProviderTransport::OpenaiCompatible),
        ("deepinfra", ProviderTransport::OpenaiCompatible),
        ("cerebras", ProviderTransport::OpenaiCompatible),
        ("cohere", ProviderTransport::OpenaiCompatible),
        ("togetherai", ProviderTransport::OpenaiCompatible),
        ("perplexity", ProviderTransport::OpenaiCompatible),
        ("vercel", ProviderTransport::OpenaiCompatible),
        ("alibaba", ProviderTransport::OpenaiCompatible),
        ("gitlab", ProviderTransport::OpenaiCompatible),
        ("venice", ProviderTransport::OpenaiCompatible),
        ("azure", ProviderTransport::OpenaiCompatible),
        ("github-copilot", ProviderTransport::OpenaiCompatible),
    ]
}

fn production_wire_spec_result(
    provider_id: &str,
    transport: ProviderTransport,
    model_id: &str,
    endpoint: &str,
    extra_options: serde_json::Value,
) -> Result<Spec, String> {
    let mut options = extra_options
        .as_object()
        .cloned()
        .expect("provider options are an object");
    options.insert("baseURL".to_owned(), serde_json::json!(endpoint));
    let mut models = serde_json::Map::new();
    models.insert(
        model_id.to_owned(),
        serde_json::json!({
            "id": model_id,
            "name": "Production wire replay",
            "limit": {"context": 100000, "output": 8192}
        }),
    );
    let mut providers = serde_json::Map::new();
    providers.insert(
        provider_id.to_owned(),
        serde_json::json!({
            "id": provider_id,
            "name": "Production wire replay",
            "env": [],
            "transport": transport,
            "options": serde_json::Value::Object(options),
            "models": serde_json::Value::Object(models)
        }),
    );
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": serde_json::Value::Object(providers)
    }))
    .expect("production replay config");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let model = catalog
        .model(provider_id, model_id)
        .expect("production replay model resolves");
    model_spec(&catalog, model, &Env::empty())
}

fn production_wire_spec(
    provider_id: &str,
    transport: ProviderTransport,
    model_id: &str,
    endpoint: &str,
    extra_options: serde_json::Value,
) -> Spec {
    production_wire_spec_result(provider_id, transport, model_id, endpoint, extra_options)
        .expect("production replay spec resolves")
}

fn openai_wire_spec(
    provider_id: &str,
    model_id: &str,
    endpoint: &str,
    custom_base_url: bool,
    advertised_endpoint: Option<&str>,
) -> Spec {
    let provider_location = if custom_base_url {
        serde_json::json!({"options": {"baseURL": endpoint}})
    } else {
        serde_json::json!({"api": endpoint})
    };
    let mut provider = provider_location
        .as_object()
        .cloned()
        .expect("provider location is an object");
    provider.insert("id".to_owned(), serde_json::json!(provider_id));
    provider.insert("name".to_owned(), serde_json::json!("OpenAI wire replay"));
    provider.insert("env".to_owned(), serde_json::json!([]));
    provider.insert("transport".to_owned(), serde_json::json!("openai"));
    provider.insert(
        "models".to_owned(),
        serde_json::json!({
            model_id: {
                "id": model_id,
                "name": "OpenAI wire replay",
                "limit": {"context": 100000, "output": 8192}
            }
        }),
    );
    let providers =
        serde_json::Map::from_iter([(provider_id.to_owned(), serde_json::Value::Object(provider))]);
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": serde_json::Value::Object(providers)
    }))
    .expect("OpenAI replay config");
    let mut catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    if let Some(advertised_endpoint) = advertised_endpoint {
        let mut value = serde_json::to_value(
            catalog
                .model(provider_id, model_id)
                .expect("OpenAI replay model resolves"),
        )
        .expect("OpenAI replay model serializes");
        value["api"]["endpoint"] = serde_json::json!(advertised_endpoint);
        let resolved = serde_json::from_value(value).expect("advertised endpoint resolves");
        assert!(catalog.replace_provider_models(
            provider_id,
            std::collections::BTreeMap::from([(model_id.to_owned(), resolved)]),
        ));
    }
    let model = catalog
        .model(provider_id, model_id)
        .expect("OpenAI replay model resolves");
    model_spec(&catalog, model, &Env::empty()).expect("OpenAI replay spec resolves")
}

fn plugin_resolved_wire_spec(
    catalog_model_id: &str,
    api_model_id: &str,
    advertised_endpoint: &str,
    endpoint: &str,
) -> Spec {
    let document: zuno_llm::catalog::models_dev::CatalogDocument =
        serde_json::from_value(serde_json::json!({
            "github-copilot": {
                "id": "github-copilot",
                "name": "GitHub Copilot",
                "env": [],
                "npm": "@ai-sdk/github-copilot",
                "models": {
                    catalog_model_id: {
                        "id": api_model_id,
                        "name": "Advertised endpoint replay",
                        "limit": {"context": 100000, "output": 8192}
                    }
                }
            }
        }))
        .expect("pinned Copilot catalog metadata");
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "github-copilot": {"options": {"baseURL": endpoint}}
        }
    }))
    .expect("Copilot endpoint override");
    let mut catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));

    // Provider model hooks return the SDK's resolved Model shape. Exercise the
    // same serde boundary as `HandleModelLoader`, including a catalog id that is
    // different from the wire id so the declaration must follow `api.id`.
    let mut plugin_value = serde_json::to_value(
        catalog
            .model("github-copilot", catalog_model_id)
            .expect("base Copilot model resolves"),
    )
    .expect("resolved model serializes");
    plugin_value["api"]["endpoint"] = serde_json::json!(advertised_endpoint);
    let plugin_model: zuno_llm::catalog::ResolvedModel =
        serde_json::from_value(plugin_value).expect("plugin model metadata resolves");
    assert!(
        catalog.replace_provider_models(
            "github-copilot",
            std::collections::BTreeMap::from([(catalog_model_id.to_owned(), plugin_model)]),
        ),
        "the plugin model replaces its pinned catalog provider"
    );

    let model = catalog
        .model("github-copilot", catalog_model_id)
        .expect("plugin-provided Copilot model resolves");
    assert_eq!(model.api.id, api_model_id);
    model_spec(&catalog, model, &Env::empty()).expect("plugin-provided Copilot spec resolves")
}

fn pinned_wire_spec(
    provider_id: &str,
    model_id: &str,
    endpoint: &str,
    expected_transport: ProviderTransport,
) -> Spec {
    let document: zuno_llm::catalog::models_dev::CatalogDocument = serde_json::from_str(
        include_str!("../../../zuno-llm/tests/fixtures/models-dev-pinned.json"),
    )
    .expect("pinned catalog fixture");
    let mut providers = serde_json::Map::new();
    providers.insert(
        provider_id.to_owned(),
        serde_json::json!({"options": {"baseURL": endpoint}}),
    );
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": serde_json::Value::Object(providers)
    }))
    .expect("pinned provider endpoint override");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let model = catalog
        .model(provider_id, model_id)
        .expect("pinned provider model resolves");
    assert_eq!(model.api.transport, Some(expected_transport));
    model_spec(&catalog, model, &Env::empty()).expect("pinned provider spec resolves")
}

struct ReplayCase<'a> {
    provider_id: &'a str,
    registry_key: &'a str,
    model_id: &'a str,
    cassette: &'a str,
    endpoint_suffix: &'a str,
    expected_body_key: &'a str,
    expected_text: &'a str,
}

async fn replay_selected_production_spec<F>(case: ReplayCase<'_>, build_spec: F)
where
    F: FnOnce(&str) -> Spec,
{
    let ReplayCase {
        provider_id,
        registry_key,
        model_id,
        cassette,
        endpoint_suffix,
        expected_body_key,
        expected_text,
    } = case;
    if zuno_testkit::recordings_root_or_skip(
        &format!("replay_selected_production_spec[{provider_id}/{model_id}]"),
        "the selected production provider spec was NOT replayed",
    )
    .is_none()
    {
        return;
    }
    let scenario = zuno_testkit::Scenario::new(provider_id)
        .on_path(endpoint_suffix)
        .from_oracle_cassette(cassette)
        .expect("recorded provider response loads");
    let mock = zuno_testkit::MockProvider::start(vec![scenario])
        .await
        .expect("loopback provider starts");
    assert!(mock.authored_scenarios().is_empty());

    let endpoint = if registry_key == COMPATIBLE_PROVIDER {
        format!("{}/v1", mock.base_url())
    } else {
        mock.base_url().to_owned()
    };
    let spec = build_spec(&endpoint);
    assert_eq!(
        spec.provider, provider_id,
        "production selection collapsed `{provider_id}` into its factory"
    );
    assert_eq!(spec.factory(), registry_key);
    let credential = Credential::Api {
        key: zuno_auth::Secret::new("production-replay-credential"),
        metadata: None,
    };
    let providers = provider_registry(provider_id, Some(credential), None);
    assert!(
        providers.is_registered(registry_key),
        "production registry omitted `{registry_key}`"
    );

    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let replay_plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &replay_plan.project, now).expect("persist project");
    let session =
        resolve_session(&mut connection, &replay_plan, now).expect("create replay session");
    persist_user_message(
        &connection,
        UserMessageInput {
            session_id: &session.id,
            agent: "build",
            provider_id,
            model_id,
            text: "Reply with a short greeting.",
            message_id: None,
            now,
        },
    )
    .expect("persist replay prompt");

    let internal = InternalAgent {
        name: "summary".to_owned(),
        prompt: "Summarize the conversation.".to_owned(),
        model: EngineModel::new(spec.clone(), model_id, spec.surface),
    };
    let internals = Internals {
        title: internal.clone(),
        compaction: internal.clone(),
        summary: internal.clone(),
        council_synth: internal,
    };
    let registry = RegistryProviders(&providers);
    let compaction = zuno_config::schema::CompactionConfig::default();
    let mut state = CompactionState::default();
    let mut context = PreludeContext {
        connection: &mut connection,
        providers: &registry,
        internals: &internals,
        compaction: &compaction,
        window: TokenWindow {
            context: 100_000,
            max_output: 8_192,
        },
        state: &mut state,
        hooks: &zuno_engine::compaction::NoopCompactionHooks,
    };
    let text = zuno_engine::prelude::summarize(&session.id, &mut context)
        .await
        .expect("production provider stream decodes");
    assert_eq!(text, expected_text);

    let captured = mock.captured().await;
    assert_eq!(captured.len(), 1, "one production stream request expected");
    assert!(
        captured[0].path.ends_with(endpoint_suffix),
        "`{registry_key}` dispatched to {}, expected suffix {endpoint_suffix}",
        captured[0].path
    );
    let body = captured[0].json().expect("production request body is JSON");
    let forbidden_body_key = if expected_body_key == "input" {
        "messages"
    } else {
        "input"
    };
    assert!(
        body.get(expected_body_key).is_some(),
        "`{registry_key}` request body omitted `{expected_body_key}`: {body}"
    );
    assert!(
        body.get(forbidden_body_key).is_none(),
        "`{registry_key}` request body retained `{forbidden_body_key}`: {body}"
    );
    assert!(
        captured[0]
            .served_origin
            .as_ref()
            .is_some_and(zuno_testkit::ResponseOrigin::is_recorded),
        "`{registry_key}` did not decode recorded provider bytes"
    );
    mock.shutdown().await;
}

struct RegistrationCase<'a> {
    registry_key: &'a str,
    transport: ProviderTransport,
    model_id: &'a str,
    cassette: &'a str,
    extra_options: serde_json::Value,
    endpoint_suffix: &'a str,
    expected_body_key: &'a str,
    expected_text: &'a str,
}

async fn replay_production_registration(case: RegistrationCase<'_>) {
    let RegistrationCase {
        registry_key,
        transport,
        model_id,
        cassette,
        extra_options,
        endpoint_suffix,
        expected_body_key,
        expected_text,
    } = case;
    let provider_id = if registry_key == COMPATIBLE_PROVIDER {
        "wire-test"
    } else {
        registry_key
    };
    replay_selected_production_spec(
        ReplayCase {
            provider_id,
            registry_key,
            model_id,
            cassette,
            endpoint_suffix,
            expected_body_key,
            expected_text,
        },
        |endpoint| production_wire_spec(provider_id, transport, model_id, endpoint, extra_options),
    )
    .await;
}

#[tokio::test]
async fn production_compatible_registration_dispatches_and_decodes_recorded_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: COMPATIBLE_PROVIDER,
        transport: ProviderTransport::OpenaiCompatible,
        model_id: "deepseek-chat",
        cassette: "openai-compatible-chat/deepseek-streams-text",
        extra_options: serde_json::json!({}),
        endpoint_suffix: "/v1/chat/completions",
        expected_body_key: "messages",
        expected_text: "Hello!",
    })
    .await;
}

#[test]
fn every_todo_94_identity_reaches_its_profile_from_resolved_config() {
    for (provider_id, transport) in named_compatible_cases() {
        let spec = production_wire_spec(
            provider_id,
            transport,
            "selection-probe",
            "https://selection.test/v1",
            serde_json::json!({}),
        );
        assert_eq!(
            spec.provider, provider_id,
            "identity collapsed for {provider_id}"
        );
        assert_eq!(spec.factory(), COMPATIBLE_PROVIDER, "{provider_id}");
        let profile = zuno_provider_compatible::family::resolve(&spec)
            .unwrap_or_else(|error| panic!("{provider_id} was not reachable: {error}"));
        assert_eq!(
            profile.provider, provider_id,
            "wrong profile for {provider_id}"
        );
        assert_eq!(
            profile.routes_upstreams,
            matches!(provider_id, "openrouter" | "vercel"),
            "router behavior did not survive selection for {provider_id}"
        );
    }
}

#[test]
fn an_unknown_transport_is_refused_from_resolved_config() {
    let error = serde_json::from_value::<zuno_config::schema::Config>(serde_json::json!({
        "provider": {
            "unknown-provider": {
                "transport": "not-implemented",
                "models": {"unknown-model": {}}
            }
        }
    }))
    .expect_err("unknown transports must fail at the config boundary");
    assert!(error.to_string().contains("not-implemented"), "{error}");
}

#[tokio::test]
async fn production_openrouter_keeps_router_identity_and_dispatches_recorded_sse() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openrouter",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "openai/gpt-4o-mini",
            cassette: "openai-compatible-chat/openrouter-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            production_wire_spec(
                "openrouter",
                ProviderTransport::Openrouter,
                "openai/gpt-4o-mini",
                endpoint,
                serde_json::json!({}),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_azure_selector_dispatches_to_responses() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "azure",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "deployment-a",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| {
            production_wire_spec(
                "azure",
                ProviderTransport::OpenaiCompatible,
                "deployment-a",
                endpoint,
                serde_json::json!({}),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_copilot_rule_dispatches_by_model_id() {
    for (model_id, cassette, endpoint_suffix, expected_body_key) in [
        (
            "gpt-5",
            "openai-responses/gpt-5-5-streams-text",
            "/v1/responses",
            "input",
        ),
        (
            "gpt-5-mini",
            "openai-compatible-chat/deepseek-streams-text",
            "/v1/chat/completions",
            "messages",
        ),
    ] {
        replay_selected_production_spec(
            ReplayCase {
                provider_id: "github-copilot",
                registry_key: COMPATIBLE_PROVIDER,
                model_id,
                cassette,
                endpoint_suffix,
                expected_body_key,
                expected_text: "Hello!",
            },
            |endpoint| {
                production_wire_spec(
                    "github-copilot",
                    ProviderTransport::OpenaiCompatible,
                    model_id,
                    endpoint,
                    serde_json::json!({}),
                )
            },
        )
        .await;
    }
}

#[tokio::test]
async fn production_copilot_advertised_responses_beats_a_heuristic_hostile_id() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "github-copilot",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "mai-code-1-flash-picker",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| {
            plugin_resolved_wire_spec(
                "mai-code-alias",
                "mai-code-1-flash-picker",
                "responses",
                endpoint,
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_copilot_advertised_chat_beats_a_responses_heuristic_id() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "github-copilot",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "gpt-5",
            cassette: "openai-compatible-chat/deepseek-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| plugin_resolved_wire_spec("gpt-5-alias", "gpt-5", "chat", endpoint),
    )
    .await;
}

#[tokio::test]
async fn pinned_groq_transport_selects_compatible_factory_and_dispatches() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "groq",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "allam-2-7b",
            cassette: "openai-compatible-chat/groq-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            pinned_wire_spec(
                "groq",
                "allam-2-7b",
                endpoint,
                ProviderTransport::OpenaiCompatible,
            )
        },
    )
    .await;
}

#[tokio::test]
async fn pinned_mistral_transport_selects_compatible_factory_and_dispatches() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "mistral",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "codestral-latest",
            cassette: "openai-compatible-chat/togetherai-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            pinned_wire_spec(
                "mistral",
                "codestral-latest",
                endpoint,
                ProviderTransport::OpenaiCompatible,
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_anthropic_registration_dispatches_and_decodes_recorded_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "anthropic",
        transport: ProviderTransport::Anthropic,
        model_id: "claude-haiku-4-5-20251001",
        cassette: "anthropic-messages/streams-text",
        extra_options: serde_json::json!({"maxTokens": 20, "promptCache": false}),
        endpoint_suffix: "/v1/messages",
        expected_body_key: "messages",
        expected_text: "Hello!",
    })
    .await;
}

#[tokio::test]
async fn production_openai_registration_dispatches_and_decodes_recorded_responses_sse() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai",
            registry_key: "openai",
            model_id: "gpt-5.5",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| openai_wire_spec("openai", "gpt-5.5", endpoint, false, None),
    )
    .await;
}

#[tokio::test]
async fn production_custom_openai_base_url_without_advertised_endpoint_dispatches_to_chat() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "private-openai-gateway",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "gateway-model",
            cassette: "openai-compatible-chat/deepseek-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            openai_wire_spec(
                "private-openai-gateway",
                "gateway-model",
                endpoint,
                true,
                None,
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_custom_openai_base_url_honors_advertised_responses() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai-wire",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "gateway-model",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| {
            openai_wire_spec(
                "openai-wire",
                "gateway-model",
                endpoint,
                true,
                Some("responses"),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_catalog_native_openai_without_override_keeps_responses() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai-wire",
            registry_key: "openai",
            model_id: "gpt-native",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| openai_wire_spec("openai-wire", "gpt-native", endpoint, false, None),
    )
    .await;
}

#[tokio::test]
async fn production_catalog_native_openai_honors_advertised_chat() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai-wire",
            registry_key: "openai",
            model_id: "gpt-native-chat",
            cassette: "openai-compatible-chat/deepseek-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            openai_wire_spec(
                "openai-wire",
                "gpt-native-chat",
                endpoint,
                false,
                Some("chat"),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_bedrock_registration_dispatches_and_decodes_recorded_eventstream() {
    replay_production_registration(RegistrationCase {
        registry_key: "amazon-bedrock",
        transport: ProviderTransport::Bedrock,
        model_id: "us.amazon.nova-micro-v1:0",
        cassette: "bedrock-converse/streams-text",
        extra_options: serde_json::json!({
            "region": "us-east-1",
            "accessKeyId": "AKIAREPLAY",
            "secretAccessKey": "replay-secret"
        }),
        endpoint_suffix: "/model/us.amazon.nova-micro-v1%3A0/converse-stream",
        expected_body_key: "messages",
        expected_text: "Hello",
    })
    .await;
}

#[tokio::test]
async fn production_bedrock_mantle_registration_dispatches_and_decodes_recorded_eventstream() {
    replay_production_registration(RegistrationCase {
        registry_key: "amazon-bedrock/mantle",
        transport: ProviderTransport::BedrockMantle,
        model_id: "openai.gpt-oss-120b",
        cassette: "bedrock-converse/streams-text",
        extra_options: serde_json::json!({
            "region": "us-east-1",
            "accessKeyId": "AKIAREPLAY",
            "secretAccessKey": "replay-secret"
        }),
        endpoint_suffix: "/model/openai.gpt-oss-120b/converse-stream",
        expected_body_key: "messages",
        expected_text: "Hello",
    })
    .await;
}

#[tokio::test]
async fn production_google_registration_dispatches_and_decodes_recorded_gemini_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "google",
        transport: ProviderTransport::Google,
        model_id: "gemini-2.5-flash",
        cassette: "gemini/streams-text",
        extra_options: serde_json::json!({}),
        endpoint_suffix: "/models/gemini-2.5-flash:streamGenerateContent",
        expected_body_key: "contents",
        expected_text: "Hello!",
    })
    .await;
}

#[tokio::test]
async fn production_vertex_gemini_registration_dispatches_and_decodes_recorded_gemini_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "google-vertex",
        transport: ProviderTransport::GoogleVertex,
        model_id: "gemini-2.5-flash",
        cassette: "gemini/streams-text",
        extra_options: serde_json::json!({"project": "project-a", "location": "us-central1"}),
        endpoint_suffix: "/models/gemini-2.5-flash:streamGenerateContent",
        expected_body_key: "contents",
        expected_text: "Hello!",
    })
    .await;
}

#[tokio::test]
async fn production_vertex_anthropic_registration_dispatches_and_decodes_recorded_anthropic_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "google-vertex/anthropic",
        transport: ProviderTransport::GoogleVertexAnthropic,
        model_id: "claude-haiku-4-5-20251001",
        cassette: "anthropic-messages/streams-text",
        extra_options: serde_json::json!({
            "project": "project-a",
            "location": "us",
            "maxTokens": 20
        }),
        endpoint_suffix: "/claude-haiku-4-5-20251001:streamRawPredict",
        expected_body_key: "messages",
        expected_text: "Hello!",
    })
    .await;
}

/// The catalog a forbidden fetch leaves behind, as [`CatalogSource::load`] builds it.
fn forbidden_fetch() -> CatalogProvenance {
    CatalogProvenance::FetchForbidden {
        origin: "https://models.dev".to_owned(),
        cache: PathBuf::from("/nowhere/cache/zuno/models.json"),
    }
}

/// A config that specifies a provider and a model end to end, as an air-gapped user
/// pointing at a private gateway writes it.
fn self_specified_config() -> zuno_config::schema::Config {
    serde_json::from_str(
        r#"{"provider":{"private":{"name":"Private","id":"private","env":[],
             "transport":"openai-compatible","api":"https://gateway.internal/v1",
             "models":{"house-model":{"id":"house-model","name":"House Model",
               "tool_call":true,"limit":{"context":100000,"output":10000},
               "cost":{"input":0,"output":0}}},
             "options":{"apiKey":"k","baseURL":"https://gateway.internal/v1"}}}}"#,
    )
    .expect("config")
}

/// Todo 108's happy path, at the seam that refused it: an empty catalog plus a config
/// that leaves nothing to look up must still select the model.
#[test]
fn a_config_specified_model_selects_with_no_catalog_at_all() {
    let config = self_specified_config();
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );

    let (provider, model, resolved) =
        select_model(&catalog, Some("private/house-model"), &forbidden_fetch())
            .expect("a config that fully specifies the model needs no catalog");

    assert_eq!(provider, "private");
    assert_eq!(model, "house-model");
    assert_eq!(resolved.api.url, "https://gateway.internal/v1");
    assert!(
        provider_factory_key(resolved.api.transport).is_some(),
        "the config's transport must survive resolution or the turn is refused later"
    );
}

/// The other half: a model nobody defines still fails immediately, and names the fix.
#[test]
fn a_model_no_config_defines_fails_immediately_and_names_the_fix() {
    let config = self_specified_config();
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );

    let message = select_model(&catalog, Some("private/absent-model"), &forbidden_fetch())
        .expect_err("nothing defines this model");

    for needle in [
        "private/absent-model",
        "provider",
        "ZUNO_DISABLE_MODELS_FETCH",
        "https://models.dev",
        "/nowhere/cache/zuno/models.json",
        "ZUNO_MODELS_PATH",
    ] {
        assert!(
            message.contains(needle),
            "the refusal must name `{needle}`, so it is actionable rather than \
             surfacing later as an empty model list: {message}"
        );
    }
}

/// With nothing requested and nothing selectable, the policy must still be named.
///
/// "No available model" alone reads as "you configured nothing", which is the
/// mis-diagnosis this whole todo was about.
#[test]
fn an_empty_catalog_with_no_request_still_explains_the_policy() {
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new(),
    );
    let message = select_model(&catalog, None, &forbidden_fetch())
        .expect_err("an empty catalog offers no default");
    assert!(message.contains("ZUNO_DISABLE_MODELS_FETCH"), "{message}");
    assert!(
        message.contains("/nowhere/cache/zuno/models.json"),
        "{message}"
    );

    // And a catalog that was genuinely loaded must NOT blame the flag.
    let loaded = select_model(&catalog, None, &CatalogProvenance::Fetched)
        .expect_err("an empty catalog offers no default");
    assert!(
        !loaded.contains("ZUNO_DISABLE_MODELS_FETCH"),
        "a loaded catalog that lists nothing is a configuration problem, not a \
         policy one: {loaded}"
    );
}

#[test]
fn new_session_is_lazy_and_first_user_message_commits_with_it() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");
    let prepared = prepare_turn_host(&connection, &plan, now).expect("prepare session");
    let session_id = prepared.identity.id().to_owned();
    assert!(!prepared.identity.is_materialized());
    let before: i64 = connection
        .query_row("SELECT count(*) FROM session", [], |row| row.get(0))
        .expect("count sessions before input");
    assert_eq!(
        before, 0,
        "opening the welcome screen must not create a row"
    );

    let (message, parts) = prepare_user_message(
        UserMessageInput {
            session_id: &session_id,
            agent: "build",
            provider_id: "provider",
            model_id: "model",
            text: "hello",
            message_id: None,
            now,
        },
        None,
    )
    .expect("prepare prompt");
    let SessionMaterializer::Pending(mut input) = prepared.materializer else {
        panic!("a new session must remain pending");
    };
    input.time = Some(now);
    let transaction = connection
        .transaction()
        .expect("start first-input transaction");
    zuno_db::session::create(&transaction, &input).expect("create session in transaction");
    zuno_db::inbox::admit_and_promote_in(
        &transaction,
        zuno_db::inbox::NewSessionInput::new(
            format!("inp_{}", message.id),
            &session_id,
            json!({
                "message": message.to_json(),
                "parts": parts.iter().map(zuno_db::message::PartRecord::to_json).collect::<Vec<_>>(),
            }),
            zuno_db::inbox::InputDelivery::Queue,
            now,
        ),
    )
    .expect("record durable input");
    persist_prepared_user_message(&transaction, &message, &parts).expect("persist prompt");
    transaction.commit().expect("commit session and prompt");

    let store = zuno_db::message::MessageStore::new(&connection);
    let messages = store
        .messages_for_session(&session_id)
        .expect("load messages");
    assert_eq!(messages.len(), 1);
    let grouped = store
        .parts_by_message(&[messages[0].id.clone()])
        .expect("load message parts");
    let parts = grouped
        .get(&messages[0].id)
        .expect("parts grouped under the message");
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].data["text"], "hello");
    let promoted: i64 = connection
        .query_row(
            "SELECT promoted_seq FROM session_input WHERE session_id = ?1",
            [&session_id],
            |row| row.get(0),
        )
        .expect("the direct prompt is durably admitted and promoted");
    assert!(promoted > 0);
}

/// The two tables that name a model do not name it the same way, and only a test
/// that reads the persisted bytes can tell.
///
/// A session row spelled `modelID` has no `id`, which is what the released
/// TypeScript binary decodes (`session.ts:88-93`); it rejects the whole listing with
/// `Expected string, got undefined` and exit 1. Writing and reading the row through
/// this port alone passes with either spelling, so these assert on the stored JSON's
/// **keys** rather than on a round trip.
#[test]
fn a_persisted_session_names_its_model_id_the_way_upstream_reads_it() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");
    let session = resolve_session(&mut connection, &plan, now).expect("create session");

    let stored: String = connection
        .query_row(
            "SELECT model FROM session WHERE id = ?1",
            [&session.id],
            |row| row.get(0),
        )
        .expect("read the persisted model column");
    let model: serde_json::Value = serde_json::from_str(&stored).expect("the column holds JSON");
    let keys = model.as_object().expect("a model object");

    assert_eq!(
        model["id"], "model",
        "upstream's session decoder reads `row.model.id` (session.ts:88-93); a \
         session_list on the released binary dies without it. Stored: {stored}"
    );
    assert!(
        !keys.contains_key("modelID"),
        "`modelID` is the *message* spelling (message.ts:121-125). A session row \
         carrying it is the defect this test exists for. Stored: {stored}"
    );
    assert_eq!(model["providerID"], "provider");
    assert!(
        !keys.contains_key("variant"),
        "`variant` is optional upstream and this port has none to record, so it must \
         be omitted rather than written as null. Stored: {stored}"
    );
}

/// The mirror of the test above, so a later edit cannot "unify" the two shapes.
///
/// A message's model is `{modelID, providerID}` (`message.ts:121-125`). Renaming this
/// to `id` to match the session row would break the sibling boundary in exactly the
/// same way, and nothing else in the suite would notice.
#[test]
fn a_persisted_message_keeps_the_message_spelling_of_its_model() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");
    let session = resolve_session(&mut connection, &plan, now).expect("create session");
    persist_user_message(
        &connection,
        UserMessageInput {
            session_id: &session.id,
            agent: "build",
            provider_id: "provider",
            model_id: "model",
            text: "hello",
            message_id: None,
            now,
        },
    )
    .expect("persist prompt");

    let store = zuno_db::message::MessageStore::new(&connection);
    let messages = store
        .messages_for_session(&session.id)
        .expect("load messages");
    let model = &messages[0].data["model"];
    let keys = model.as_object().expect("a model object");

    assert_eq!(
        model["modelID"], "model",
        "a message names the model `modelID` (message.ts:121-125): {model}"
    );
    assert_eq!(model["providerID"], "provider");
    assert!(
        !keys.contains_key("id"),
        "the session spelling must not leak into a message: {model}"
    );
}

#[test]
fn an_explicit_session_is_reused_rather_than_created() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let now = 1_780_000_000_000;
    let created = {
        let plan = plan("/workspace", SessionChoice::New);
        ensure_project(&connection, &plan.project, now).expect("persist project");
        resolve_session(&mut connection, &plan, now).expect("create session")
    };

    let reused = resolve_session(
        &mut connection,
        &plan("/workspace", SessionChoice::Existing(created.id.clone())),
        now,
    )
    .expect("reuse the named session");
    assert_eq!(reused.id, created.id);

    let continued = resolve_session(
        &mut connection,
        &plan("/workspace", SessionChoice::Continue),
        now,
    )
    .expect("continue the directory's most recent session");
    assert_eq!(continued.id, created.id);
}

#[test]
fn recent_sessions_stay_with_the_current_directory_and_hide_children_and_archived_rows() {
    fn insert(
        connection: &mut zuno_db::Connection,
        input: zuno_db::session::SessionCreate,
    ) -> zuno_db::session::Session {
        let transaction = connection.transaction().expect("open transaction");
        let session = zuno_db::session::create(&transaction, &input)
            .expect("create fixture session")
            .into_session();
        transaction.commit().expect("commit fixture session");
        session
    }

    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");

    let current = insert(
        &mut connection,
        zuno_db::session::SessionCreate::new(
            "ses_current",
            "current",
            &plan.project.id,
            "/workspace",
            "/workspace",
            "current",
            crate::RUST_PACKAGE_VERSION,
        )
        .at(now),
    );
    let previous = insert(
        &mut connection,
        zuno_db::session::SessionCreate::new(
            "ses_previous",
            "previous",
            &plan.project.id,
            "/workspace",
            "/workspace",
            "previous",
            crate::RUST_PACKAGE_VERSION,
        )
        .at(now - 1),
    );
    let mut child = zuno_db::session::SessionCreate::new(
        "ses_child",
        "child",
        &plan.project.id,
        "/workspace",
        "/workspace",
        "child",
        crate::RUST_PACKAGE_VERSION,
    )
    .at(now + 1);
    child.parent_id = Some(current.id.clone());
    insert(&mut connection, child);
    insert(
        &mut connection,
        zuno_db::session::SessionCreate::new(
            "ses_elsewhere",
            "elsewhere",
            &plan.project.id,
            "/workspace",
            "/elsewhere",
            "elsewhere",
            crate::RUST_PACKAGE_VERSION,
        )
        .at(now + 2),
    );
    let archived = insert(
        &mut connection,
        zuno_db::session::SessionCreate::new(
            "ses_archived",
            "archived",
            &plan.project.id,
            "/workspace",
            "/workspace",
            "archived",
            crate::RUST_PACKAGE_VERSION,
        )
        .at(now + 3),
    );
    connection
        .execute(
            "UPDATE session SET time_archived = ?1 WHERE id = ?2",
            rusqlite::params![now + 4, archived.id],
        )
        .expect("archive fixture session");

    let listed = recent_sessions(&connection, "/workspace", 100).expect("list picker sessions");
    assert_eq!(
        listed
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![current.id.as_str(), previous.id.as_str()]
    );
    assert_eq!(
        switchable_session(&connection, "/workspace", &previous.id)
            .expect("validate same-directory root"),
        Some(previous)
    );
    assert!(
        switchable_session(&connection, "/workspace", "ses_child")
            .expect("reject child")
            .is_none()
    );
    assert!(
        switchable_session(&connection, "/workspace", "ses_elsewhere")
            .expect("reject other directory")
            .is_none()
    );
    assert!(
        switchable_session(&connection, "/workspace", &archived.id)
            .expect("reject archived")
            .is_none()
    );
}

#[test]
fn session_choice_resolves_the_two_flags_into_one_answer() {
    assert_eq!(SessionChoice::resolve(None, false), SessionChoice::New);
    assert_eq!(SessionChoice::resolve(None, true), SessionChoice::Continue);
    assert_eq!(
        SessionChoice::resolve(Some("ses_1"), true),
        SessionChoice::Existing("ses_1".to_owned())
    );
}

#[test]
fn a_turn_option_preset_overrides_the_configuration_default() {
    let config: zuno_config::schema::Config = serde_json::from_str(
        r#"{
          "preset": "fast",
          "presets": {
            "deliberate": {"agents": {"build": "test/big"}},
            "fast": {"agents": {"build": "test/small"}}
          }
        }"#,
    )
    .expect("preset fixture");

    let library = turn_presets(&config, Some("deliberate"));

    assert_eq!(library.selected(), Some("deliberate"));
    assert_eq!(
        library
            .active()
            .and_then(|preset| preset.agent("build"))
            .map(|choice| choice.model.as_str()),
        Some("test/big")
    );
}

#[test]
fn all_internals_resolve_with_the_roster_prompt_and_a_reachable_model() {
    // Given: a catalog with two models and a per-agent override for `title` only.
    let (catalog, config) = catalog_with_two_models_and_a_title_override();
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();

    // When: the internals are resolved.
    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            presets: &PresetLibrary::new(),
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("every internal resolves");

    // Then: the overridden one took its override, the remaining internals inherited
    // the session's model, and every internal carries the roster's prompt.
    assert_eq!(internals.title.model.model_id, "small");
    assert_eq!(internals.compaction.model.model_id, "big");
    assert_eq!(internals.summary.model.model_id, "big");
    assert_eq!(internals.council_synth.model.model_id, "big");
    for internal in [
        &internals.title,
        &internals.compaction,
        &internals.summary,
        &internals.council_synth,
    ] {
        assert!(
            !internal.prompt.trim().is_empty(),
            "`{}` resolved with no prompt, so its request would carry no instructions",
            internal.name
        );
    }
    assert_eq!(
        internals.title.prompt,
        zuno_catalog::agent::builtin::PROMPT_TITLE,
        "the title prompt was rewritten instead of read from the catalog"
    );
}

#[test]
fn internal_models_keep_the_responses_surface_selected_by_the_catalog() {
    let document = serde_json::from_str("{}").expect("empty catalog document");
    let config: zuno_config::schema::Config = serde_json::from_str(
        r#"{
          "small_model": "kiro-local/gpt-5.6-sol",
          "memory": {"reflection": true},
          "provider": {
            "kiro-local": {
              "transport": "openai",
              "surface": "responses",
              "options": {"baseURL": "http://127.0.0.1:8787/v1"},
              "models": {
                "gpt-5.6-sol": {
                  "name": "GPT 5.6 Sol via Kiro",
                  "limit": {"context": 272000, "output": 64000}
                }
              }
            }
          }
        }"#,
    )
    .expect("responses provider config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let model = catalog
        .model("kiro-local", "gpt-5.6-sol")
        .expect("configured responses model");
    let env = Env::empty();
    let mut notes = Vec::new();

    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            presets: &PresetLibrary::new(),
            catalog: &catalog,
            provider_id: "kiro-local",
            model_id: "gpt-5.6-sol",
            session_model: model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("responses internals resolve");

    for internal in [
        &internals.title,
        &internals.compaction,
        &internals.summary,
        &internals.council_synth,
    ] {
        assert_eq!(internal.model.provider.surface, ApiSurface::Responses);
        assert_eq!(
            internal.model.surface,
            ApiSurface::Responses,
            "{} replaced the catalog surface instead of preserving it",
            internal.name
        );
    }

    let reflection = resolve_reflection_model(&config, &catalog, "kiro-local", &env, &mut notes)
        .expect("reflection resolution succeeds")
        .expect("reflection is enabled by the explicit small model");
    assert_eq!(reflection.provider.surface, ApiSurface::Responses);
    assert_eq!(reflection.surface, ApiSurface::Responses);
}

/// Every name the roster declares internal must resolve here.
///
/// The assertion is over [`zuno_agent::builtin::INTERNAL_NAMES`] and not an independent
/// list, so a new internal added to the roster fails this test rather than silently
/// becoming another declared-and-never-invoked entry — which is the exact defect this
/// wiring exists to remove.
#[test]
fn the_resolved_set_is_exactly_what_the_roster_calls_internal() {
    let (catalog, config) = catalog_with_two_models_and_a_title_override();
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();
    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            presets: &PresetLibrary::new(),
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("every internal resolves");

    let resolved: std::collections::BTreeSet<&str> = [
        internals.title.name.as_str(),
        internals.compaction.name.as_str(),
        internals.summary.name.as_str(),
        internals.council_synth.name.as_str(),
    ]
    .into_iter()
    .collect();
    let declared: std::collections::BTreeSet<&str> =
        zuno_agent::builtin::INTERNAL_NAMES.into_iter().collect();
    assert_eq!(
        resolved, declared,
        "the roster declares internals this composition root does not resolve"
    );
}

#[test]
fn an_internal_pointed_at_another_provider_falls_back_and_says_why() {
    // Given: an override naming a model under a provider whose credential this turn
    // does not wire.
    let (catalog, _) = catalog_with_two_models_and_a_title_override();
    let config: zuno_config::schema::Config =
        serde_json::from_str(r#"{"agents":{"summary":{"model":"elsewhere/some-model"}}}"#)
            .expect("config");
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();

    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            presets: &PresetLibrary::new(),
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("a declined override is not a failure");

    assert_eq!(internals.summary.model.model_id, "big");
    assert!(
        notes.iter().any(|note| note.contains("summary")),
        "the downgrade was silent; notes: {notes:?}"
    );
}

/// A catalog holding one provider whose endpoint is placed wherever the caller says.
///
/// `api` is the top-level provider key that feeds `model.api.url`; `endpoint` and
/// `base_url` go into `provider.probe.options`.
fn endpoint_catalog(api: Option<&str>, endpoint: Option<&str>, base_url: Option<&str>) -> Catalog {
    let mut options = serde_json::Map::new();
    if let Some(endpoint) = endpoint {
        options.insert("endpoint".to_owned(), serde_json::json!(endpoint));
    }
    if let Some(base_url) = base_url {
        options.insert("baseURL".to_owned(), serde_json::json!(base_url));
    }
    let mut provider = serde_json::Map::new();
    provider.insert(
        "transport".to_owned(),
        serde_json::json!("openai-compatible"),
    );
    if let Some(api) = api {
        provider.insert("api".to_owned(), serde_json::json!(api));
    }
    provider.insert("options".to_owned(), serde_json::Value::Object(options));
    provider.insert(
        "models".to_owned(),
        serde_json::json!({"probe-model": {"id": "probe-model"}}),
    );
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {"probe": serde_json::Value::Object(provider)}
    }))
    .expect("config");
    Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    )
}

fn probe_spec(
    api: Option<&str>,
    endpoint: Option<&str>,
    base_url: Option<&str>,
) -> Result<Spec, String> {
    probe_spec_in(api, endpoint, base_url, &Env::empty())
}

/// The same probe, resolved against an explicit environment.
///
/// Separate from [`probe_spec`] so the ladder cases keep asserting against an
/// environment that carries nothing: a placeholder-free URL must resolve identically
/// whatever is set, and a fixture that quietly supplied variables would hide a fix that
/// only worked when they happened to be present.
fn probe_spec_in(
    api: Option<&str>,
    endpoint: Option<&str>,
    base_url: Option<&str>,
    env: &Env,
) -> Result<Spec, String> {
    let catalog = endpoint_catalog(api, endpoint, base_url);
    let model = catalog
        .model("probe", "probe-model")
        .expect("the config declares the model");
    model_spec(&catalog, model, env)
}

/// The whole ladder, rung by rung — `provider.ts:1698-1700` plus `:355-358`.
///
/// Each case gives the winning rung a distinct URL, so a reordering changes the
/// asserted value rather than merely changing which of two identical URLs was used.
/// The first case is the defect: `options.baseURL` alone, the shape the upstream docs
/// show, which reached the transport with no endpoint at all.
#[test]
fn the_endpoint_comes_from_options_before_the_catalog() {
    let cases = [
        (
            (None, None, Some("https://from-base-url/v1")),
            "https://from-base-url/v1",
            "`options.baseURL` alone must be the endpoint",
        ),
        (
            (None, Some("https://from-endpoint/v1"), None),
            "https://from-endpoint/v1",
            "`options.endpoint` alone must be the endpoint",
        ),
        (
            (
                None,
                Some("https://from-endpoint/v1"),
                Some("https://from-base-url/v1"),
            ),
            "https://from-endpoint/v1",
            "`options.endpoint` must beat `options.baseURL`",
        ),
        (
            (Some("https://from-api/v1"), None, None),
            "https://from-api/v1",
            "a catalog-supplied `api.url` must still work when no option names one",
        ),
        (
            (
                Some("https://from-api/v1"),
                None,
                Some("https://from-base-url/v1"),
            ),
            "https://from-base-url/v1",
            "`options.baseURL` must beat the catalog's `api.url`",
        ),
        (
            (
                Some("https://from-api/v1"),
                Some("https://from-endpoint/v1"),
                Some("https://from-base-url/v1"),
            ),
            "https://from-endpoint/v1",
            "`options.endpoint` must beat everything",
        ),
    ];

    for ((api, endpoint, base_url), expected, why) in cases {
        let spec = probe_spec(api, endpoint, base_url).expect("an endpoint resolves");
        assert_eq!(
            spec.base_url.as_deref(),
            Some(expected),
            "{why} (api={api:?}, endpoint={endpoint:?}, baseURL={base_url:?})"
        );
    }
}

/// An empty option is not an endpoint — `provider.ts:1699` tests the string for `!== ""`.
///
/// Without the emptiness test, `"baseURL": ""` would win the ladder and produce a spec
/// whose base URL is the empty string, which the transport would happily prepend
/// nothing to and dial a relative path.
#[test]
fn an_empty_endpoint_option_falls_through_to_the_next_rung() {
    let spec = probe_spec(Some("https://from-api/v1"), Some(""), Some(""))
        .expect("the catalog rung still answers");
    assert_eq!(spec.base_url.as_deref(), Some("https://from-api/v1"));
}

/// A URL naming no variable is returned byte for byte — `provider.ts:1712-1715`.
///
/// The expansion pass runs on every turn, so the case that must not change anything is
/// the case that runs almost always. The awkward inputs are here on purpose: `${}` has
/// an empty name and the oracle's `[^}]+` needs at least one character, `${unclosed` has
/// no terminator, and a bare `$` or `{` is not a placeholder at all. Each would be an
/// easy off-by-one in a hand-rolled scan, and each would corrupt a perfectly ordinary
/// URL that happens to contain a brace.
///
/// The environment binds the **empty name** as well as `SET`, and that is not
/// decoration: with only `SET` bound, dropping the scan's `offset > 0` guard left `${}`
/// looking up a name nothing carried, which fell back to the literal and produced a
/// byte-identical answer — the test passed while the guard was gone. A binding for `""`
/// is what makes `${}` staying literal an observable claim rather than a coincidence.
/// Nothing can export an empty-named variable through a POSIX `environ`, so this only
/// ever comes from a constructed [`Env`]; the guard exists because the oracle's `[^}]+`
/// demands at least one character, not because the input is reachable.
#[test]
fn a_url_naming_no_variable_is_unchanged_by_expansion() {
    let env = Env::from_pairs([("SET", "substituted"), ("", "empty-name")]);
    for url in [
        "https://api.example.com/v1",
        "http://127.0.0.1:8080/v1",
        "https://api.example.com/v1?filter=a{b}c",
        "https://api.example.com/${}/v1",
        "https://api.example.com/${unclosed/v1",
        "https://api.example.com/$SET/v1",
        "https://api.example.com/{SET}/v1",
        "",
    ] {
        assert_eq!(
            expand_variables(url, &env),
            url,
            "expansion must not alter a URL that names no variable"
        );
    }
}

/// A set variable substitutes; an unset one keeps its literal `${VAR}`.
///
/// `?? item` (`provider.ts:1714`) is the whole of the second half. Substituting the
/// empty string for an unset name would turn `https://${REGION}.api.example.com/v1`
/// into `https://.api.example.com/v1` — a *different host*, silently — so this is the
/// one case where the wrong answer is worse than no answer.
#[test]
fn an_unset_variable_keeps_its_placeholder_while_a_set_one_substitutes() {
    let env = Env::empty()
        .with("REGION", "eu-west-1")
        .with("SUFFIX", "example.com")
        .with("BLANK", "");
    let cases = [
        (
            "https://${REGION}.api.example.com/v1",
            "https://eu-west-1.api.example.com/v1",
            "a set variable must substitute",
        ),
        (
            "https://${MISSING}.api.example.com/v1",
            "https://${MISSING}.api.example.com/v1",
            "an unset variable must keep its literal placeholder, not collapse the host",
        ),
        (
            "https://${REGION}.api.${SUFFIX}/v1",
            "https://eu-west-1.api.example.com/v1",
            "every placeholder in one URL must be expanded",
        ),
        (
            "https://${REGION}.api.${MISSING}/v1",
            "https://eu-west-1.api.${MISSING}/v1",
            "one unset variable must not stop the others from expanding",
        ),
        (
            "https://api.${BLANK}example.com/v1",
            "https://api.example.com/v1",
            "a variable set to the empty string substitutes empty — `\"\"` is not \
             nullish in the oracle either",
        ),
    ];

    for (url, expected, why) in cases {
        assert_eq!(expand_variables(url, &env), expected, "{why}");
    }
}

/// Expansion applies to whichever rung the ladder chose, all three of them.
///
/// The defect is one step past todo 109: the ladder was already correct, and every rung
/// it could choose was then dialled literally. A fix that only expanded `model.api.url`
/// — the field whose doc comment promised expansion — would leave the two option rungs
/// broken, so each is asserted separately here.
#[test]
fn every_endpoint_rung_is_expanded_after_it_wins() {
    let env = Env::empty().with("HOST", "gateway.internal");
    let cases = [
        (
            (Some("https://${HOST}/v1"), None, None),
            "the catalog's `api.url` must be expanded",
        ),
        (
            (None, Some("https://${HOST}/v1"), None),
            "`options.endpoint` must be expanded",
        ),
        (
            (None, None, Some("https://${HOST}/v1")),
            "`options.baseURL` must be expanded",
        ),
    ];

    for ((api, endpoint, base_url), why) in cases {
        let spec = probe_spec_in(api, endpoint, base_url, &env).expect("an endpoint resolves");
        assert_eq!(
            spec.base_url.as_deref(),
            Some("https://gateway.internal/v1"),
            "{why}"
        );
    }
}

/// A rung is chosen on its unexpanded text, and expanded only afterwards.
///
/// The oracle tests `options["baseURL"] !== ""` at `:1699-1700` and expands at `:1712`, in
/// that order. `BLANK` is set to the empty string, so `"baseURL": "${BLANK}"` is a
/// non-empty rung that wins the ladder and *then* becomes empty — it does not fall
/// through to the catalog's `api.url`. Moving expansion ahead of
/// [`super::provider_endpoint`] would produce `https://from-api/v1` here, which is why
/// this case exists rather than a second happy-path one.
#[test]
fn a_rung_is_chosen_before_expansion_not_after() {
    let env = Env::empty().with("BLANK", "");
    let spec = probe_spec_in(Some("https://from-api/v1"), None, Some("${BLANK}"), &env)
        .expect("a non-empty rung was available before expansion");
    assert_eq!(
        spec.base_url.as_deref(),
        Some(""),
        "`options.baseURL` was non-empty when the ladder read it, so it must win and \
         then expand to nothing; falling through to `api.url` means expansion ran first"
    );
}

/// Neither an endpoint key nor `apiKey` is ever forwarded as an SDK option.
///
/// [`model_spec`] now forwards **both** bags — the provider's, seeded first, and the
/// model's overlaid on top — so the exclusion has to hold on both, and the keys are
/// planted in both here. `Spec::options` is read by allow-listed key today —
/// `capabilities`, `extraBody`, `useCompletionUrls` — so a stray `baseURL` or `apiKey`
/// there is inert, and inert-today is exactly how it would go unnoticed until someone
/// widened that read and a request body grew a field named after a URL, or one carrying
/// key material. Every other option must still come through, or this becomes a filter
/// that eats configuration.
#[test]
fn the_endpoint_keys_do_not_also_travel_in_the_option_bag() {
    let document = serde_json::from_str(
        r#"{"probe":{"id":"probe","name":"Probe","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{"probe-model":{"id":"probe-model","name":"Probe Model",
               "limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config = serde_json::from_str(
        r#"{"provider":{"probe":{"options":{
               "baseURL":"https://from-base-url/v1",
               "endpoint":"https://from-base-url/v1",
               "apiKey":"sk-provider-level",
               "providerKept":true},
             "models":{"probe-model":{"options":{
               "baseURL":"https://model-level/v1",
               "endpoint":"https://model-level-endpoint/v1",
               "apiKey":"sk-model-level",
               "extraBody":{"kept":true}}}}}}}"#,
    )
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let provider = catalog.provider("probe").expect("the provider");
    let model = catalog.model("probe", "probe-model").expect("the model");
    for (label, options) in [("provider", &provider.options), ("model", &model.options)] {
        for key in ["baseURL", "endpoint", "apiKey"] {
            assert!(
                options.contains_key(key),
                "the fixture lost `{key}` from the {label}'s options in the merge, so \
                 the exclusion it is meant to prove is untested"
            );
        }
    }

    let spec = model_spec(&catalog, model, &Env::empty())
        .expect("the provider option supplies the endpoint");

    assert_eq!(
        spec.base_url.as_deref(),
        Some("https://from-base-url/v1"),
        "a model-level endpoint key is not an endpoint source; only the provider's is \
         (`provider.ts:1698-1700` reads `provider.options`)"
    );
    for key in ["endpoint", "baseURL", "apiKey"] {
        assert!(
            !spec.options.contains_key(key),
            "`{key}` reached the SDK option bag: {:?}",
            spec.options
        );
    }
    assert_eq!(
        spec.options.get("extraBody"),
        Some(&serde_json::json!({"kept": true})),
        "every model option that is not resolved elsewhere must still reach the SDK"
    );
    assert_eq!(
        spec.options.get("providerKept"),
        Some(&serde_json::json!(true)),
        "every provider option that is not resolved elsewhere must still reach the SDK"
    );
    assert!(
        !format!("{:?}", spec.options).contains("sk-"),
        "key material reached the option bag: {:?}",
        spec.options
    );
}

#[test]
fn a_configured_image_modality_reaches_the_compatible_provider_capability_map() {
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "vision-gateway": {
                "transport": "openai",
                "surface": "responses",
                "options": {"baseURL": "http://127.0.0.1:8787/v1"},
                "models": {
                    "vision-model": {
                        "attachment": true,
                        "reasoning": true,
                        "tool_call": true,
                        "modalities": {
                            "input": ["text", "image"],
                            "output": ["text"]
                        },
                        "limit": {"context": 1000, "output": 100}
                    }
                }
            }
        }
    }))
    .expect("vision provider config");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let model = catalog
        .model("vision-gateway", "vision-model")
        .expect("configured model resolves");
    assert!(
        model.capabilities.input.image,
        "fixture must be image-capable"
    );

    let spec = model_spec(&catalog, model, &Env::empty()).expect("compatible spec resolves");
    let capabilities = spec
        .options
        .get(zuno_provider_compatible::provider::MODEL_CAPABILITIES_OPTION)
        .and_then(serde_json::Value::as_object)
        .and_then(|models| models.get(&model.api.id))
        .and_then(serde_json::Value::as_object)
        .expect("resolved per-model capability map");
    assert_eq!(
        capabilities.get("attachments"),
        Some(&serde_json::json!(true)),
        "an image-capable model must not be rejected as text-only before transport"
    );
}

/// No endpoint anywhere fails immediately and names the key that supplies one.
///
/// The pre-fix path composed the whole turn and then said `unrecoverable provider
/// failure (status=None)` from the transport, which names nothing actionable.
#[test]
fn a_provider_with_no_endpoint_anywhere_names_the_key_to_set() {
    let message = probe_spec(None, None, None).expect_err("nothing supplies an endpoint");
    for needle in ["provider.probe.options", "baseURL", "endpoint"] {
        assert!(
            message.contains(needle),
            "the refusal must name `{needle}`: {message}"
        );
    }
}

/// The endpoint injected into every fixture built by [`options_spec`].
const PROBE_ENDPOINT: &str = "https://gateway.probe/v1";

/// The spec one provider/model option pair produces, endpoint supplied for free.
///
/// `baseURL` is injected into the provider's options rather than left to the caller so
/// no test here can accidentally assert on a `Spec` that [`model_spec`] refused to
/// build — and so the endpoint-exclusion test operates on a `baseURL` that is genuinely
/// load-bearing rather than a decorative one.
fn options_spec(provider_options: &serde_json::Value, model_options: &serde_json::Value) -> Spec {
    let document = serde_json::from_str(
        r#"{"probe":{"id":"probe","name":"Probe","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{"probe-model":{"id":"probe-model","name":"Probe Model",
               "limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let mut provider = provider_options
        .as_object()
        .cloned()
        .expect("the provider options are an object");
    provider.insert("baseURL".to_owned(), serde_json::json!(PROBE_ENDPOINT));
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {"probe": {
            "options": serde_json::Value::Object(provider),
            "models": {"probe-model": {"options": model_options}}
        }}
    }))
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let model = catalog.model("probe", "probe-model").expect("the model");
    model_spec(&catalog, model, &Env::empty()).expect("the injected endpoint resolves")
}

/// A resolved provider carrying exactly `options`, for the credential precedence table.
fn probe_provider(options: serde_json::Value) -> Catalog {
    let document = serde_json::from_str(
        r#"{"probe":{"id":"probe","name":"Probe","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{"probe-model":{"id":"probe-model","name":"Probe Model",
               "limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {"probe": {"options": options}}
    }))
    .expect("config");
    Catalog::resolve(&document, &ResolveInput::new().with_config(&config))
}

/// `options.apiKey` is primary, stored auth is next, and provider env is the
/// final fallback.
///
/// The whole table in one test, because the defect was a *precedence* and a precedence
/// is only wrong relative to its alternatives: reading only the option breaks
/// `opencode auth login`, and reading only the credential is the bug this todo fixes.
/// Every expectation names a distinct string, so no row can pass by coincidence.
#[test]
fn an_options_api_key_is_primary_and_the_stored_credential_is_the_fallback() {
    let stored = zuno_auth::Credential::Api {
        key: zuno_auth::Secret::new("sk-from-the-store"),
        metadata: None,
    };
    let cases = [
        (
            serde_json::json!({"apiKey": "sk-from-options"}),
            true,
            Some("sk-from-options"),
            "`options.apiKey` must win when both are present",
        ),
        (
            serde_json::json!({"apiKey": "sk-from-options"}),
            false,
            Some("sk-from-options"),
            "`options.apiKey` alone must be the credential",
        ),
        (
            serde_json::json!({}),
            true,
            Some("sk-from-the-store"),
            "the stored credential must still authenticate when no option names a key",
        ),
        (
            serde_json::json!({}),
            false,
            None,
            "neither source means no credential, which a local endpoint is entitled to",
        ),
        (
            serde_json::json!({"apiKey": ""}),
            true,
            Some(""),
            "an explicitly empty `apiKey` is a key, not a fall-through: `:1719` tests \
             for `=== undefined`, and falling back here would present a real vendor \
             key to an endpoint the user never named",
        ),
    ];

    for (options, present, expected, why) in cases {
        let catalog = probe_provider(options.clone());
        let provider = catalog.provider("probe").expect("the provider resolves");
        assert_eq!(
            provider.options.get("apiKey"),
            options.get("apiKey"),
            "the fixture lost `apiKey` in the merge, so this row proves nothing"
        );

        let resolved =
            resolved_credential(Some(provider), present.then_some(&stored), &Env::empty());

        assert_eq!(
            resolved.as_ref().map(credential_value).as_deref(),
            expected,
            "{why} (options={options})"
        );
    }
}

#[test]
fn the_first_declared_provider_environment_key_is_the_final_credential_fallback() {
    let document = serde_json::from_str(
        r#"{"probe":{"id":"probe","name":"Probe","env":["PRIMARY_KEY","SECONDARY_KEY"],
             "npm":"@ai-sdk/openai-compatible",
             "models":{"probe-model":{"id":"probe-model","name":"Probe Model",
               "limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config =
        serde_json::from_str(r#"{"provider":{"probe":{}}}"#).expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let provider = catalog.provider("probe").expect("provider");
    let env = Env::empty()
        .with("PRIMARY_KEY", "sk-from-primary-env")
        .with("SECONDARY_KEY", "sk-from-secondary-env");

    let from_env = resolved_credential(Some(provider), None, &env).expect("environment key");
    assert_eq!(credential_value(&from_env), "sk-from-primary-env");

    let stored = Credential::Api {
        key: zuno_auth::Secret::new("sk-from-store"),
        metadata: None,
    };
    let from_store = resolved_credential(Some(provider), Some(&stored), &env).expect("stored key");
    assert_eq!(credential_value(&from_store), "sk-from-store");
}

/// Why [`provider_api_key`]'s string test can never be reached from a config file.
///
/// `ProviderOptions::api_key` is typed `Option<String>`
/// (`zuno-config/src/schema/provider.rs:54`), so a non-string `apiKey` is refused before
/// any provider is resolved and the `as_str` guard is belt over braces — kept because
/// `ResolvedProvider::options` is a free-form JSON map that a future non-config source
/// could populate, and a number silently becoming `Bearer 7` is not an acceptable
/// outcome. Asserted rather than assumed, so a schema change that loosened the field
/// would show up here instead of at a gateway.
#[test]
fn a_non_string_api_key_never_reaches_the_resolved_provider() {
    let refused = serde_json::from_value::<zuno_config::schema::Config>(serde_json::json!({
        "provider": {"probe": {"options": {"apiKey": 7}}}
    }));

    assert!(
        refused.is_err(),
        "the config schema accepted a non-string `apiKey`, so `provider_api_key`'s \
         fall-through is now reachable and needs a case of its own"
    );
}

/// A provider-level option reaches the surface that reads it — `:1676`.
///
/// `useCompletionUrls` is the case worth pinning: it is a *provider* option in the
/// oracle (`provider.ts:265` passes `options?.["useCompletionUrls"]`), it has a reader
/// here, and before this fix setting it where the docs say to set it did nothing at all.
/// The assertion goes through that reader rather than stopping at
/// `spec.options.contains_key`, because "the bag holds the key" and "the code that
/// consults the bag sees it" are different claims and only the second one matters.
#[test]
fn a_provider_level_option_reaches_the_surface_that_reads_it() {
    let spec = options_spec(
        &serde_json::json!({"useCompletionUrls": true}),
        &serde_json::json!({}),
    );

    assert!(
        zuno_provider_compatible::surface::use_completion_urls(&spec),
        "a provider-level `useCompletionUrls` is still inert; options: {:?}",
        spec.options
    );
}

/// The model wins on collision, and the provider's other leaves survive — `:1497`.
///
/// Three claims in one fixture, each with its own witness: `shared` proves the direction,
/// `providerOnly` proves the merge is deep rather than a replace, and `modelOnly` proves
/// the model's own keys are not lost to the seed.
#[test]
fn a_model_option_wins_over_a_provider_option_of_the_same_name() {
    let spec = options_spec(
        &serde_json::json!({
            "extraBody": {"shared": "from-the-provider", "providerOnly": "kept"},
            "providerScalar": "kept"
        }),
        &serde_json::json!({
            "extraBody": {"shared": "from-the-model", "modelOnly": "kept"},
            "providerScalar": "replaced"
        }),
    );

    assert_eq!(
        spec.options.get("extraBody"),
        Some(&serde_json::json!({
            "shared": "from-the-model",
            "providerOnly": "kept",
            "modelOnly": "kept"
        })),
        "the provider/model overlay is not a deep merge with the model winning"
    );
    assert_eq!(
        spec.options.get("providerScalar"),
        Some(&serde_json::json!("replaced")),
        "a provider-level scalar overrode the model's, so the direction is inverted"
    );
}

/// An internal whose own model has no endpoint is declined, not fatal.
///
/// A per-model `provider.api` can give one model in a provider an endpoint and leave
/// another without one. Propagating that with `?` would lose the whole turn because a
/// title agent could not be reached, so it takes the same downgrade-and-say-why path as
/// a cross-provider or unsupported-transport override.
#[test]
fn an_internal_whose_model_has_no_endpoint_falls_back_and_says_why() {
    // Given: `big` carries its own endpoint, `small` carries none, and `title` is
    // pointed at `small`.
    let document = serde_json::from_str(
        r#"{"test":{"id":"test","name":"Test","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{
               "big":{"id":"big","name":"Big","limit":{"context":200000,"output":8192}},
               "small":{"id":"small","name":"Small","limit":{"context":100000,"output":4096}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config = serde_json::from_str(
        r#"{"provider":{"test":{"models":{
             "big":{"provider":{"api":"https://gateway.test/v1"}},
             "small":{}}}},
             "agents":{"title":{"model":"test/small"}}}"#,
    )
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    assert!(
        catalog
            .model("test", "small")
            .expect("small resolves")
            .api
            .url
            .is_empty(),
        "the fixture must leave `small` without an endpoint or it proves nothing"
    );
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();

    // When: the internals resolve.
    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            presets: &PresetLibrary::new(),
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("an unreachable override is a downgrade, not a failure");

    // Then: title fell back to the session model and said so.
    assert_eq!(internals.title.model.model_id, "big");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("title") && note.contains("baseURL")),
        "the downgrade was silent or did not name the missing key; notes: {notes:?}"
    );
}

#[test]
fn a_catalog_limit_that_is_absent_or_negative_reads_as_no_window() {
    assert_eq!(token_count(200_000.0), 200_000);
    assert_eq!(token_count(0.0), 0);
    assert_eq!(token_count(-1.0), 0);
    assert_eq!(token_count(f64::NAN), 0);
    assert_eq!(token_count(f64::INFINITY), 0);
}

#[test]
fn production_registry_exposes_all_three_goal_tools() {
    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let config = zuno_config::schema::Config::default();
    let selected_agent = agent_profile(agent("build"), directory.path(), &config);
    let runtime = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &config,
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
            background_executions: test_background_executions(directory.path()),
            sandbox: test_sandbox(),
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .expect("production registry assembles");
    let ids = runtime
        .tools
        .iter()
        .map(|tool| tool.id())
        .collect::<Vec<_>>();

    for goal_tool in ["goal_get", "goal_propose", "goal_update"] {
        assert!(
            ids.contains(&goal_tool),
            "production registry is missing `{goal_tool}`; visible tools: {ids:?}"
        );
    }
}

fn interaction_tool_ids(
    policy: zuno_goal::InteractionPolicy,
    attached_human_surface: bool,
) -> Vec<String> {
    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let config = zuno_config::schema::Config::default();
    let selected_agent = agent_profile(agent("build"), directory.path(), &config);
    let question = attached_human_surface.then(|| {
        Arc::new(zuno_tools::question::ScriptedAnswers::default())
            as Arc<dyn zuno_tools::question::QuestionAsker>
    });
    tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &config,
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question,
            interaction_policy: policy,
            background_executions: test_background_executions(directory.path()),
            sandbox: test_sandbox(),
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .expect("production registry assembles")
    .tools
    .iter()
    .map(|tool| tool.id().to_owned())
    .collect()
}

#[test]
fn interaction_tools_follow_plan_goal_and_subagent_boundaries() {
    let plan = interaction_tool_ids(zuno_goal::InteractionPolicy::PlanClarification, true);
    assert!(
        plan.iter()
            .any(|tool| tool == zuno_tools::question::WIRE_ID)
    );
    assert!(
        plan.iter()
            .all(|tool| tool != zuno_goal::REQUEST_GOAL_INPUT_TOOL_ID)
    );

    let goal = interaction_tool_ids(zuno_goal::InteractionPolicy::GoalAutonomous, true);
    assert!(
        goal.iter()
            .all(|tool| tool != zuno_tools::question::WIRE_ID)
    );
    assert!(
        goal.iter()
            .any(|tool| tool == zuno_goal::REQUEST_GOAL_INPUT_TOOL_ID)
    );

    let headless_goal = interaction_tool_ids(zuno_goal::InteractionPolicy::GoalAutonomous, false);
    assert!(
        headless_goal
            .iter()
            .all(|tool| tool != zuno_goal::REQUEST_GOAL_INPUT_TOOL_ID),
        "a Goal must not receive a human-request tool when no client can present it"
    );

    let subagent = interaction_tool_ids(zuno_goal::InteractionPolicy::SubagentReportOnly, true);
    assert!(subagent.iter().all(|tool| {
        tool != zuno_tools::question::WIRE_ID && tool != zuno_goal::REQUEST_GOAL_INPUT_TOOL_ID
    }));
}

#[test]
fn goal_request_projection_requires_the_active_goal() {
    assert!(human_request_belongs_to_goal(
        Some("goal-active"),
        Some("goal-active")
    ));
    assert!(!human_request_belongs_to_goal(
        Some("goal-other"),
        Some("goal-active")
    ));
    assert!(!human_request_belongs_to_goal(None, Some("goal-active")));
    assert!(!human_request_belongs_to_goal(None, None));
}

#[cfg(unix)]
#[tokio::test]
async fn production_registry_wires_configured_shell_into_the_shell_tool() {
    use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let config = zuno_config::schema::Config {
        shell: Some("/bin/sh".to_owned()),
        ..Default::default()
    };
    let selected_agent = agent_profile(agent("build"), directory.path(), &config);
    let runtime = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &config,
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
            background_executions: test_background_executions(directory.path()),
            sandbox: test_sandbox(),
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .expect("production registry assembles");
    let shell = runtime
        .tools
        .iter()
        .find(|tool| tool.id() == "shell")
        .expect("the build profile exposes the shell tool");

    let output = shell
        .invoke(
            serde_json::json!({"command": "printf configured-shell"}),
            ToolContext::new(
                "ses_shell_config",
                "msg_shell_config",
                "call_shell_config",
                "build",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("the configured shell executes");

    assert_eq!(output.title, "printf configured-shell");
    assert_eq!(output.metadata["shell"], "sh");
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_full_access_uses_the_native_backend_and_retains_managed_lifecycle_metadata() {
    use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let outside = tempfile::TempDir::new().expect("external destination");
    let proof = outside.path().join("full-access-proof");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let config = zuno_config::schema::Config::from_json_str(
        Path::new("zuno.json"),
        r#"{"shell":"/bin/sh","sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("full-access config");
    let selected_agent = agent_profile(agent("build"), directory.path(), &config);
    let runtime = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &config,
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
            background_executions: test_background_executions(directory.path()),
            sandbox: None,
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .expect("full access must not probe bubblewrap");
    let shell = runtime
        .tools
        .iter()
        .find(|tool| tool.id() == "shell")
        .expect("build exposes Shell");
    let output = shell
        .invoke(
            serde_json::json!({
                "command": format!("printf native > '{}'", proof.display())
            }),
            ToolContext::new(
                "ses_full_access",
                "msg_full_access",
                "call_full_access",
                "build",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("native command executes through the background service");

    assert_eq!(std::fs::read_to_string(proof).expect("proof"), "native");
    assert_eq!(output.metadata["sandboxMode"], "danger-full-access");
    assert_eq!(output.metadata["sandboxBackend"], "danger_full_access");
    assert_eq!(output.metadata["sandboxNetwork"], "allowed");
    assert_eq!(
        output.metadata["sandboxRequestedMode"],
        "danger-full-access"
    );
    assert_eq!(output.metadata["sandboxResolutionKind"], "explicit_native");
    assert_eq!(output.metadata["sandboxFallback"], false);
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_fallback_is_visible_and_keeps_managed_shell_guards_and_authority() {
    use zuno_config::schema::permission::PermissionAction;
    use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let config = zuno_config::schema::Config::from_json_str(
        Path::new("trusted.json"),
        r#"{
            "shell": "/bin/sh",
            "permission": {
                "mode": "strict",
                "rules": {
                    "shell": {
                        "*": "allow",
                        "printf forbidden": "deny"
                    }
                }
            },
            "sandbox": {
                "mode": "workspace-write",
                "network": "deny",
                "onUnavailable": "run-unconfined"
            }
        }"#,
    )
    .expect("trusted fallback config");
    let selected_agent = agent_profile(agent("build"), directory.path(), &config);
    let background_executions = test_background_executions(directory.path());
    let runtime = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &config,
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
            background_executions: Arc::clone(&background_executions),
            sandbox: Some(Arc::new(UnavailableTestSandbox)),
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .expect("trusted unavailable fallback assembles");
    assert!(
        runtime
            .sandbox_notice
            .as_deref()
            .is_some_and(|notice| notice.contains("without OS isolation"))
    );
    assert_eq!(
        zuno_permission::evaluate("shell", "printf forbidden", &runtime.rules),
        PermissionAction::Deny,
        "native fallback must not widen an explicit Shell deny"
    );
    let shell = runtime
        .tools
        .iter()
        .find(|tool| tool.id() == "shell")
        .expect("build exposes Shell");

    let refused = shell
        .invoke(
            serde_json::json!({"command": "rm -rf /"}),
            ToolContext::new(
                "ses_fallback",
                "msg_fallback_refusal",
                "call_fallback_refusal",
                "build",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect_err("catastrophic commands remain refused");
    let zuno_error::ToolError::Failed { source, .. } = refused else {
        panic!("catastrophic refusal must be a terminal tool failure");
    };
    assert!(source.to_string().contains("blocked"));

    let output = shell
        .invoke(
            serde_json::json!({"command": "printf fallback", "background": true}),
            ToolContext::new(
                "ses_fallback",
                "msg_fallback",
                "call_fallback",
                "build",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("fallback command executes through the managed process service");

    assert_eq!(output.metadata["sandboxMode"], "danger-full-access");
    assert_eq!(output.metadata["sandboxNetwork"], "allowed");
    assert_eq!(output.metadata["sandboxRequestedMode"], "workspace-write");
    assert_eq!(output.metadata["sandboxRequestedNetwork"], "denied");
    assert_eq!(
        output.metadata["sandboxResolutionKind"],
        "unavailable_fallback"
    );
    assert_eq!(output.metadata["sandboxFallback"], true);
    assert_eq!(
        output.metadata["sandboxFallbackReason"]["code"],
        "bubblewrap_not_found"
    );

    let id = zuno_pty::BackgroundExecutionId::parse(
        output.metadata["task_id"]
            .as_str()
            .expect("background task id"),
    )
    .expect("valid background id");
    let settled = background_executions
        .wait(&id, None)
        .await
        .expect("background command settles")
        .info;
    assert_eq!(settled.authority.schema_version, 3);
    assert_eq!(settled.authority.approval_mode, "strict");
    assert_eq!(
        settled.authority.requested_mode(),
        zuno_sandbox::SandboxMode::WorkspaceWrite
    );
    assert_eq!(
        settled.authority.mode,
        zuno_sandbox::SandboxMode::DangerFullAccess
    );
}

#[test]
fn read_only_agent_refuses_unavailable_fallback_even_when_trusted_config_allows_it() {
    use zuno_config::schema::permission::PermissionAction;

    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let config = zuno_config::schema::Config::from_json_str(
        Path::new("trusted.json"),
        r#"{"sandbox":{"onUnavailable":"run-unconfined"}}"#,
    )
    .expect("trusted fallback config");
    let rules = vec![
        zuno_permission::Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        },
        zuno_permission::Rule {
            permission: "apply_patch".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
        zuno_permission::Rule {
            permission: "write".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
        zuno_permission::Rule {
            permission: "edit".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
    ];
    let selected_agent =
        zuno_agent::profile::AgentProfile::resolve(agent("read-only-shell"), rules, false);

    let error = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &config,
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
            background_executions: test_background_executions(directory.path()),
            sandbox: Some(Arc::new(UnavailableTestSandbox)),
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .err()
    .expect("read-only authority must stay fail-closed");

    assert!(error.contains("bubblewrap"));
}

#[test]
fn fallback_preserves_each_configured_permission_mode() {
    for (configured, expected) in [
        ("standard", "standard"),
        ("strict", "strict"),
        ("allow_all", "allow_all"),
    ] {
        let directory = tempfile::TempDir::new().expect("temporary tool workspace");
        let config = zuno_config::schema::Config::from_json_str(
            Path::new("trusted.json"),
            &format!(
                r#"{{
                    "permission": {{"mode": "{configured}"}},
                    "sandbox": {{"onUnavailable": "run-unconfined"}}
                }}"#
            ),
        )
        .expect("permission config");
        let selected_agent = agent_profile(agent("build"), directory.path(), &config);
        let requested =
            tool_runtime::sandbox_policy(directory.path(), &config, &selected_agent, &[])
                .expect("requested policy");
        let resolution = zuno_sandbox::SandboxResolution::unavailable_fallback(
            requested,
            zuno_sandbox::SandboxUnavailableCause::BubblewrapNotFound,
        )
        .expect("fallback resolution");
        let prepared = resolution
            .backend()
            .prepare(zuno_sandbox::PrepareRequest {
                program: "/bin/sh".into(),
                arguments: Vec::new(),
                cwd: directory.path().to_owned(),
                environment: Default::default(),
                policy: resolution.execution_policy().clone(),
            })
            .expect("prepared fallback");

        assert_eq!(prepared.authority().approval_mode, expected);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_read_only_agent_contract_narrows_a_full_access_invocation() {
    use zuno_config::schema::permission::PermissionAction;
    use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let config = zuno_config::schema::Config::from_json_str(
        Path::new("zuno.json"),
        r#"{"shell":"/bin/sh","sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("full-access config");
    let rules = vec![
        zuno_permission::Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        },
        zuno_permission::Rule {
            permission: "apply_patch".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
        zuno_permission::Rule {
            permission: "write".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
        zuno_permission::Rule {
            permission: "edit".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        },
    ];
    let selected_agent =
        zuno_agent::profile::AgentProfile::resolve(agent("read-only-shell"), rules, false);
    let runtime = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &config,
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
            background_executions: test_background_executions(directory.path()),
            sandbox: test_sandbox(),
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .expect("read-only effective policy assembles");
    let shell = runtime
        .tools
        .iter()
        .find(|tool| tool.id() == "shell")
        .expect("the custom read-only profile retains Shell");
    let output = shell
        .invoke(
            serde_json::json!({"command": "printf narrowed"}),
            ToolContext::new(
                "ses_narrowed",
                "msg_narrowed",
                "call_narrowed",
                "read-only-shell",
                Arc::new(AllowAll),
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("read-only command executes");

    assert_eq!(output.metadata["sandboxMode"], "read-only");
    assert_eq!(output.metadata["sandboxNetwork"], "denied");
}

#[test]
fn production_registry_exposes_council_only_to_a_delegating_profile() {
    fn ids_for(agent_name: &str) -> Vec<String> {
        let directory = tempfile::TempDir::new().expect("temporary tool workspace");
        let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
        let config = zuno_config::schema::Config::default();
        let selected_agent = agent_profile(agent(agent_name), directory.path(), &config);
        let runtime = tool_runtime::assemble(
            directory.path(),
            None,
            &Env::empty(),
            &config,
            &selected_agent,
            tool_runtime::ToolSelection {
                provider_id: "provider",
                model_id: "model",
                manifest: Arc::new(zuno_harness::ToolManifest::standard()),
                contributions: Arc::new(zuno_harness::ToolContributions::default()),
                question: None,
                interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
                background_executions: test_background_executions(directory.path()),
                sandbox: test_sandbox(),
                todo_store: Arc::new(
                    zuno_db::Pool::open(&zuno_paths::DbLocation::Memory)
                        .expect("in-memory todo store"),
                ),
                goal_store: Arc::new(
                    GoalStore::open_memory(goal_spill.path().to_owned())
                        .expect("in-memory goal store"),
                ),
                mcp_loader: None,
                skills: Arc::new(zuno_catalog::skill::Skills::default()),
                capability: test_capability_with_council(),
                delegation: test_delegation(),
                product_agents: test_product_agents(),
                workflows: test_workflows(),
                councils: test_councils(),
                job_controller: test_job_controller(),
                memory: None,
                tool_authority: None,
            },
        )
        .expect("production registry assembles");
        runtime
            .tools
            .iter()
            .map(|tool| tool.id().to_owned())
            .collect()
    }

    assert!(ids_for("orchestrator").contains(&zuno_tools::COUNCIL_WIRE_ID.to_owned()));
    assert!(!ids_for("build").contains(&zuno_tools::COUNCIL_WIRE_ID.to_owned()));
}

#[test]
fn council_picker_matches_the_final_agent_capability_snapshot() {
    let directory = tempfile::TempDir::new().expect("temporary turn workspace");
    let path = directory.path().to_string_lossy();

    let mut orchestrator = plan(&path, SessionChoice::New);
    orchestrator.capability = test_capability_with_council();
    orchestrator.agent = agent_profile(
        agent("orchestrator"),
        directory.path(),
        &orchestrator.config,
    );
    assert_eq!(
        orchestrator.council_choices(),
        vec![CouncilChoice {
            name: "balanced-review".to_owned(),
            description: "3 seats · quorum 2 · up to 3 parallel".to_owned(),
        }]
    );

    let mut build = plan(&path, SessionChoice::New);
    build.capability = test_capability_with_council();
    assert!(
        build.council_choices().is_empty(),
        "a non-delegating Agent must not advertise a launcher its dispatcher removes"
    );

    let mut restricted = agent("orchestrator");
    restricted.tools = Some(vec!["read".to_owned()]);
    orchestrator.agent = agent_profile(restricted, directory.path(), &orchestrator.config);
    assert!(
        orchestrator.council_choices().is_empty(),
        "an explicit tool allowlist must also hide the launcher"
    );
}

#[test]
fn production_registry_uses_the_frozen_profile_rules() {
    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let selected_agent = zuno_agent::profile::AgentProfile::resolve(
        agent("build"),
        vec![
            zuno_permission::Rule {
                permission: "*".to_owned(),
                pattern: "*".to_owned(),
                action: zuno_config::schema::permission::PermissionAction::Allow,
            },
            zuno_permission::Rule {
                permission: "read".to_owned(),
                pattern: "*".to_owned(),
                action: zuno_config::schema::permission::PermissionAction::Deny,
            },
        ],
        false,
    );
    let runtime = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &zuno_config::schema::Config::default(),
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::standard()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
            background_executions: test_background_executions(directory.path()),
            sandbox: test_sandbox(),
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            capability: test_capability(),
            delegation: test_delegation(),
            product_agents: test_product_agents(),
            workflows: test_workflows(),
            councils: test_councils(),
            job_controller: test_job_controller(),
            memory: None,
            tool_authority: None,
        },
    )
    .expect("production registry assembles");

    assert!(
        runtime.tools.iter().all(|tool| tool.id() != "read"),
        "tool visibility must come from the profile snapshot, not recomputed config"
    );
    assert_eq!(runtime.rules, selected_agent.capabilities().rules());
}

#[test]
fn goal_dynamic_context_is_rebuilt_from_authoritative_sql_for_each_request() {
    let spill = tempfile::tempdir().expect("temporary goal spill directory");
    let store =
        Arc::new(GoalStore::open_memory(spill.path().to_owned()).expect("in-memory goal store"));
    let continuation = GoalContinuation::new(Arc::clone(&store), SessionRunRegistry::new());
    let first_goal = store
        .create_goal("ses_goal_context", "first objective", None)
        .expect("create goal");
    let first = continuation
        .injection("ses_goal_context")
        .expect("read first injection")
        .map(|entry| dynamic_context_from_goal_entry(&entry))
        .expect("goal context exists");
    assert_eq!(
        first,
        DynamicContext::new(zuno_goal::render_goal_context(&first_goal))
    );

    let second_goal = store
        .update_objective("ses_goal_context", "second objective from SQL")
        .expect("update objective")
        .expect("goal exists");
    let second = continuation
        .injection("ses_goal_context")
        .expect("read second injection")
        .map(|entry| dynamic_context_from_goal_entry(&entry))
        .expect("goal context exists");
    assert_eq!(
        second,
        DynamicContext::new(zuno_goal::render_goal_context(&second_goal))
    );
    assert_ne!(
        first, second,
        "the second request reused stale goal context"
    );
}

#[test]
fn durable_work_context_projects_plan_todos_jobs_reports_and_prior_receipt_from_sql() {
    let pool =
        Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
    let mut connection = pool.open_connection().expect("open connection");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project_work_context', '/workspace', 1, 1, '[]');
             INSERT INTO session (
                 id, project_id, slug, directory, title, version, time_created, time_updated
             ) VALUES (
                 'ses_work_context', 'project_work_context', 'work-context', '/workspace',
                 'Work context', 'zuno', 1, 1
             );
             INSERT INTO work_plan (
                 session_id, id, goal_id, revision, title, steps, time_created, time_updated
             ) VALUES (
                 'ses_work_context', 'plan_durable', 'goal_durable', 2, 'Durable plan',
                 '[{\"id\":\"inspect\",\"title\":\"Inspect state\",\"status\":\"in_progress\"}]',
                 2, 3
             );
             INSERT INTO work_item (
                 id, session_id, goal_id, plan_step_id, parent_id, subject, description,
                 active_form, status, priority, dependencies, owner, revision,
                 tokens_used, usage_known, time_used_ms, time_created, time_updated
             ) VALUES (
                 'todo_durable', 'ses_work_context', 'goal_durable', 'inspect', NULL,
                 'Inspect state', 'Inspect durable state', 'Inspecting durable state',
                 'in_progress', 'high', '[]', 'build', 4, 0, 1, 0, 4, 5
             );",
        )
        .expect("seed session work state");
    let receipt = zuno_db::event_log::SessionEventLog::new(Arc::clone(&pool))
        .append(
            "ses_work_context",
            zuno_db::event_log::NewSessionEvent::new(
                "session.prompt.assembled",
                serde_json::Map::new(),
            )
            .expect("prompt event"),
        )
        .expect("append prompt receipt");

    let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
    jobs.create(zuno_db::job::NewAgentJob::new(
        "job_active",
        "ses_work_context",
        zuno_db::job::JobSubject::child_session("ses_child_active"),
        zuno_db::job::ReportDelivery::NextStep,
        6,
    ))
    .expect("create active job");
    jobs.create(zuno_db::job::NewAgentJob::new(
        "job_report",
        "ses_work_context",
        zuno_db::job::JobSubject::child_session("ses_child_report"),
        zuno_db::job::ReportDelivery::NextStep,
        7,
    ))
    .expect("create reported job");
    jobs.settle(
        "job_report",
        zuno_db::job::JobSettlement::completed(
            json!({"finalText": "report answer"}),
            8,
            Some(zuno_db::inbox::NewSessionInput::new(
                "input_report",
                "ses_work_context",
                json!({
                    "kind": "subagentReport",
                    "jobID": "job_report",
                    "childSessionID": "ses_child_report",
                    "status": "completed",
                    "text": "report answer"
                }),
                zuno_db::inbox::InputDelivery::Queue,
                8,
            )),
        ),
    )
    .expect("settle reported job");
    jobs.create(zuno_db::job::NewAgentJob::new(
        "job_reconciled",
        "ses_work_context",
        zuno_db::job::JobSubject::child_session("ses_child_reconciled"),
        zuno_db::job::ReportDelivery::Quiet,
        9,
    ))
    .expect("create reconciled job");
    jobs.settle(
        "job_reconciled",
        zuno_db::job::JobSettlement::completed(json!({"finalText": "done"}), 10, None),
    )
    .expect("settle reconciled job");

    let context = durable_work_context(&connection, "ses_work_context")
        .expect("project durable work context")
        .expect("durable work exists");
    assert!(context.starts_with("runtime.work_state\n"));
    let snapshot: Value = serde_json::from_str(
        context
            .lines()
            .last()
            .expect("context ends with the typed snapshot"),
    )
    .expect("decode durable work context");

    assert_eq!(snapshot["schemaVersion"], 1);
    assert_eq!(snapshot["plan"]["id"], "plan_durable");
    assert_eq!(snapshot["plan"]["revision"], 2);
    assert_eq!(snapshot["todos"][0]["id"], "todo_durable");
    assert_eq!(snapshot["todos"][0]["status"], "in_progress");
    assert_eq!(snapshot["pendingReports"][0]["id"], "input_report");
    assert_eq!(snapshot["latestPriorPromptReceiptId"], receipt.id.as_str());
    let job_ids = snapshot["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .filter_map(|job| job["id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(job_ids, ["job_active", "job_report"]);
    assert!(
        !context.contains("job_reconciled"),
        "a quiet reconciled terminal must not pollute continuation context"
    );
}

#[test]
fn durable_work_context_has_a_deterministic_total_prompt_budget() {
    let snapshot = DurableWorkContextSnapshot {
        schema_version: DURABLE_WORK_CONTEXT_SCHEMA_VERSION,
        plan: None,
        todos: (0..DURABLE_WORK_CONTEXT_MAX_ENTRIES)
            .map(|index| DurableTodoContext {
                id: format!("todo-{index:02}"),
                goal_id: Some("goal-budget".to_owned()),
                plan_step_id: None,
                subject: format!("todo {index}: {}", "界".repeat(2_000)),
                status: "pending".to_owned(),
                priority: "medium".to_owned(),
                dependencies: Vec::new(),
                owner: Some("build".to_owned()),
                revision: 1,
            })
            .collect(),
        jobs: Vec::new(),
        pending_reports: Vec::new(),
        latest_prior_prompt_receipt_id: Some("evt-budget".to_owned()),
        omitted_todos: 0,
        omitted_jobs: 0,
        omitted_pending_reports: 0,
    };

    let rendered = render_durable_work_context(snapshot).expect("bounded work context");

    assert!(
        rendered.len() <= DURABLE_WORK_CONTEXT_MAX_BYTES,
        "runtime work context used {} bytes",
        rendered.len()
    );
    let decoded: Value = serde_json::from_str(
        rendered
            .lines()
            .last()
            .expect("context ends with a JSON snapshot"),
    )
    .expect("decode bounded snapshot");
    assert!(
        decoded["omittedTodos"].as_u64().unwrap_or_default() > 0,
        "the byte cap must report structural omission"
    );
    assert!(
        decoded["todos"]
            .as_array()
            .is_some_and(|todos| !todos.is_empty()),
        "the bounded snapshot should retain as much current state as fits"
    );
    assert!(
        decoded["todos"][0]["subject"]
            .as_str()
            .is_some_and(|subject| subject.len() <= DURABLE_WORK_CONTEXT_TEXT_MAX_BYTES)
    );
}

#[test]
fn durable_work_context_fail_safes_an_existing_twenty_kib_plan_goal_id() {
    let pool =
        Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
    let mut connection = pool.open_connection().expect("open connection");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project_oversized_goal', '/workspace', 1, 1, '[]');
             INSERT INTO session (
                 id, project_id, slug, directory, title, version, time_created, time_updated
             ) VALUES (
                 'ses_oversized_goal', 'project_oversized_goal', 'oversized-goal', '/workspace',
                 'Oversized goal', 'zuno', 1, 1
             );",
        )
        .expect("seed session");
    let oversized_goal_id = "g".repeat(20 * 1024);
    connection
        .execute(
            "INSERT INTO work_plan (
                 session_id, id, goal_id, revision, title, steps, time_created, time_updated
             ) VALUES (
                 'ses_oversized_goal', 'plan_oversized_goal', ?1, 1, 'Durable plan', '[]', 2, 2
             )",
            [&oversized_goal_id],
        )
        .expect("seed existing invalid durable plan");

    let rendered = durable_work_context(&connection, "ses_oversized_goal")
        .expect("invalid durable identity must not block the next turn")
        .expect("durable work exists");

    assert!(
        rendered.len() <= DURABLE_WORK_CONTEXT_MAX_BYTES,
        "runtime work context used {} bytes",
        rendered.len()
    );
    let decoded: Value = serde_json::from_str(
        rendered
            .lines()
            .last()
            .expect("context ends with a JSON snapshot"),
    )
    .expect("decode bounded snapshot");
    let projected = decoded["plan"]["goalId"]
        .as_str()
        .expect("explicit invalid identity marker");
    assert!(projected.starts_with("zuno.invalid-id/v1;field=plan.goal_id;"));
    assert!(projected.contains("error=too_long"));
    assert!(projected.contains("value=omitted"));
    assert!(projected.contains("bytes=20480"));
    assert!(projected.contains("sha256="));
    assert_ne!(projected, oversized_goal_id);
}

#[test]
fn goal_usage_delta_includes_every_confirmed_assistant_step_and_token_bucket() {
    fn checkpoint(connection: &mut rusqlite::Connection, record: &zuno_db::message::MessageRecord) {
        let transaction = connection.transaction().expect("start checkpoint");
        {
            let store = zuno_db::message::MessageStore::new(&transaction);
            let previous = store
                .find_message(&record.id)
                .expect("read prior checkpoint")
                .map(|message| zuno_db::session::MessageUsage::from_data(&message.data));
            store
                .put_message(record)
                .expect("persist assistant checkpoint");
            zuno_db::session::reconcile_usage(
                &transaction,
                &record.session_id,
                previous,
                zuno_db::session::MessageUsage::from_data(&record.data),
                None,
            )
            .expect("reconcile session usage");
        }
        transaction.commit().expect("commit checkpoint");
    }

    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let fixture_plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &fixture_plan.project, now).expect("persist project");
    let session =
        resolve_session(&mut connection, &fixture_plan, now).expect("create fixture session");
    let assistant = |id: &str, created: i64, values: [i64; 5]| {
        zuno_db::message::MessageRecord::from_json(serde_json::json!({
            "id": id,
            "sessionID": session.id,
            "role": "assistant",
            "time": { "created": created, "completed": created + 1 },
            "parentID": "msg_parent",
            "modelID": "model",
            "providerID": "provider",
            "mode": "build",
            "agent": "build",
            "path": { "cwd": "/workspace", "root": "/workspace" },
            "cost": 0,
            "tokens": {
                "input": values[0],
                "output": values[1],
                "reasoning": values[2],
                "cache": { "read": values[3], "write": values[4] },
                "accounting": "cache-beside-input"
            },
            "finish": "stop"
        }))
        .expect("valid assistant message")
    };
    let baseline = assistant("msg_baseline", now, [1, 1, 1, 1, 1]);
    checkpoint(&mut connection, &baseline);
    let before = goal_usage(&connection, &session.id).expect("read usage before turn");
    let step_one = assistant("msg_step_1", now + 2, [1, 2, 3, 4, 5]);
    checkpoint(&mut connection, &step_one);
    let step_two = assistant("msg_step_2", now + 4, [10, 20, 30, 40, 50]);
    checkpoint(&mut connection, &step_two);
    let after = goal_usage(&connection, &session.id).expect("read usage after turn");

    assert_eq!(after.tokens - before.tokens, 165);
    assert!(goal_turn_accounting_known(before, after));
}

#[test]
fn failed_provider_request_keeps_confirmed_goal_usage_and_marks_the_turn_unknown() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let fixture_plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &fixture_plan.project, now).expect("persist project");
    let session =
        resolve_session(&mut connection, &fixture_plan, now).expect("create fixture session");

    let assistant = zuno_db::message::MessageRecord::from_json(serde_json::json!({
        "id": "msg_confirmed",
        "sessionID": session.id,
        "role": "assistant",
        "time": { "created": now, "completed": now + 1 },
        "parentID": "msg_parent",
        "modelID": "model",
        "providerID": "provider",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0,
        "tokens": {
            "input": 10,
            "output": 5,
            "reasoning": 2,
            "cache": { "read": 3, "write": 4 },
            "accounting": "cache-beside-input"
        },
        "finish": "stop"
    }))
    .expect("valid assistant message");
    let transaction = connection.transaction().expect("start checkpoint");
    {
        let store = zuno_db::message::MessageStore::new(&transaction);
        store
            .put_message(&assistant)
            .expect("persist confirmed checkpoint");
        zuno_db::session::reconcile_usage(
            &transaction,
            &session.id,
            None,
            zuno_db::session::MessageUsage::from_data(&assistant.data),
            None,
        )
        .expect("reconcile confirmed usage");
    }
    transaction.commit().expect("commit checkpoint");

    let before = goal_usage(&connection, &session.id).expect("read confirmed usage");
    zuno_db::session::record_provider_request_started(&connection, &session.id, 1_234, Some(8_192))
        .expect("record attempted request");
    zuno_db::session::record_turn_failure(&connection, &session.id).expect("record failed turn");
    let after = goal_usage(&connection, &session.id).expect("read failed request usage");

    assert_eq!(
        after.tokens, before.tokens,
        "confirmed usage must be monotonic"
    );
    assert_eq!(after.estimated_pending_prompt_tokens, Some(1_234));
    assert_eq!(after.failed_turns, before.failed_turns + 1);
    assert!(!goal_turn_accounting_known(before, after));
}

/// Neither surface may compose a turn or bypass the selected driver.
///
/// The whole point of this module is that `run` and the TUI cannot drift apart in
/// which tools exist, which rules govern them, or how a session is resolved — and
/// the way they would drift is a second composition root or a direct loop call.
#[test]
fn only_this_module_composes_a_turn() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
    let composition = [
        "ToolRegistryDispatcher::new",
        ".service::<dyn AgentDriver>()",
        "self\n            .driver\n            .drive(",
    ];
    let mut scanned = 0_usize;
    for entry in std::fs::read_dir(&directory).expect("the command directory is readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Test modules are excluded because they do not compose production turns —
        // and because this file names both needles in its own assertion message.
        if name.ends_with("_tests.rs") {
            continue;
        }
        scanned += 1;
        let source = std::fs::read_to_string(&path).expect("a readable source file");
        for needle in composition {
            let occurrences = source.matches(needle).count();
            let expected = usize::from(name == "turn.rs");
            assert_eq!(
                occurrences, expected,
                "`{name}` mentions `{needle}` {occurrences} time(s); the turn \
                 composition belongs to `turn.rs` and to nothing else, because a \
                 second call site is how two surfaces come to offer different tools"
            );
        }
        assert_eq!(
            source.matches("run_turn(").count(),
            0,
            "`{name}` bypasses the active AgentDriver"
        );
    }
    assert!(
        scanned >= 17,
        "scanned only {scanned} files under {}; the scan is looking in the wrong \
         place and would pass vacuously",
        directory.display()
    );

    let driver =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../zuno-engine/src/driver.rs");
    let driver = std::fs::read_to_string(&driver).expect("the default driver source is readable");
    assert_eq!(
        driver.matches("run_turn(").count(),
        1,
        "only DefaultAgentDriver may own the built-in loop call"
    );
}

/// One value of every [`TurnError`] variant, so a claim about rendering covers all of
/// them rather than the two a bug report happened to name.
///
/// Each carries a distinctive payload — `session-in-the-message`, `agent-in-the-message`
/// — because the assertions below check that the payload survives, and a shared
/// placeholder would let one variant pass on another's text.
fn every_turn_error() -> Vec<TurnError> {
    vec![
        TurnError::NoUserMessage {
            session_id: "ses_in-the-message".to_owned(),
        },
        TurnError::MissingUserField {
            message_id: "msg_in-the-message".to_owned(),
            field: "agent",
        },
        TurnError::AgentNotFound {
            agent: "agent-in-the-message".to_owned(),
        },
        TurnError::ModelNotFound {
            provider_id: "provider-in-the-message".to_owned(),
            model_id: "model-in-the-message".to_owned(),
        },
        TurnError::StepLimit {
            agent: "agent-in-the-message".to_owned(),
            max_steps: 100,
        },
        TurnError::StreamEndedWithoutMessageEnd { step: 3 },
        TurnError::EmptyAssistantMessage {
            provider_id: "empty-provider-in-the-message".to_owned(),
            step: 4,
        },
        TurnError::DuplicateToolUse {
            step: 5,
            call_id: "duplicate-call-in-the-message".to_owned(),
        },
        TurnError::ToolInputWithoutStart {
            step: 6,
            call_id: "input-call-in-the-message".to_owned(),
        },
        TurnError::ToolUseEndWithoutStart {
            step: 7,
            call_id: "end-call-in-the-message".to_owned(),
        },
        TurnError::ToolSignatureWithoutStart {
            step: 8,
            call_id: "signature-call-in-the-message".to_owned(),
        },
        TurnError::InvalidToolCalls {
            count: 3,
            tool: "write-in-the-message".to_owned(),
        },
        TurnError::MissingHumanRequestId {
            tool: "goal_request_input".to_owned(),
        },
        TurnError::EventConsumerClosed,
        TurnError::Hook(
            "plugin `fixture-plugin` failed hook `chat.params`: fixture failure".to_owned(),
        ),
        TurnError::Database(zuno_error::DbError::Busy { retry_after: None }),
        TurnError::Provider(ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311),
        }),
        TurnError::Provider(ProviderError::RateLimited { retry_after: None }),
        TurnError::Provider(ProviderError::Transient {
            status: None,
            source: Some(Box::new(std::io::Error::other(
                "error sending request for url (http://gateway.invalid/v1)",
            ))),
        }),
        TurnError::Provider(ProviderError::Auth {
            provider: "test".to_owned(),
            source: None,
        }),
        TurnError::Provider(ProviderError::Refused {
            provider: "test".to_owned(),
            provider_text: None,
        }),
        TurnError::Provider(ProviderError::Fatal {
            status: Some(400),
            source: None,
        }),
        TurnError::PromptAssembly(
            zuno_engine::prompt::PromptAssemblyError::ContextLimitExceeded {
                estimated_prompt_tokens: 16_384,
                context_limit: 8_192,
            },
        ),
        TurnError::ProviderRetryDeadlineExceeded {
            attempt: 2,
            elapsed: std::time::Duration::from_secs(180),
        },
        TurnError::Cache(zuno_llm::cache::CacheViolation::StaticPrefixChanged { turn: 2 }),
    ]
}

/// The category every existing assertion reads still leads the message.
///
/// This is the guarantee the chain walk had to keep. Todo 109's and 110's tests, the
/// compaction suite's `unrecoverable provider failure` check and anything a user has
/// scripted all read the front of the line; appending causes must not move it. Asserted
/// for every variant rather than the ones with known assertions, because the next
/// assertion will be written against whichever variant this test did not cover.
#[test]
fn every_variant_keeps_its_category_at_the_front_of_the_message() {
    for error in every_turn_error() {
        let category = error.to_string();
        let rendered = describe_turn_failure(&error, None);

        assert!(
            rendered.starts_with(&category),
            "the category moved: expected `{rendered}` to start with `{category}`"
        );
    }
}

/// A new [`TurnError`] variant must break this, so its author decides what is rendered.
///
/// `every_turn_error` is a hand-written list, and a hand-written list of an enum's
/// variants silently goes stale — which is exactly how a new failure class would arrive
/// rendering as a bare category again. The match is exhaustive with no wildcard arm, so
/// the compiler refuses to build this file until the new variant is named, and the count
/// refuses to pass until it is also constructed.
#[test]
fn the_variant_table_covers_the_whole_enum() {
    let mut named = std::collections::BTreeSet::new();
    for error in every_turn_error() {
        let name = match &error {
            TurnError::NoUserMessage { .. } => "NoUserMessage",
            TurnError::MissingUserField { .. } => "MissingUserField",
            TurnError::AgentNotFound { .. } => "AgentNotFound",
            TurnError::ModelNotFound { .. } => "ModelNotFound",
            TurnError::StepLimit { .. } => "StepLimit",
            TurnError::StreamEndedWithoutMessageEnd { .. } => "StreamEndedWithoutMessageEnd",
            TurnError::EmptyAssistantMessage { .. } => "EmptyAssistantMessage",
            TurnError::DuplicateToolUse { .. } => "DuplicateToolUse",
            TurnError::ToolInputWithoutStart { .. } => "ToolInputWithoutStart",
            TurnError::ToolUseEndWithoutStart { .. } => "ToolUseEndWithoutStart",
            TurnError::ToolSignatureWithoutStart { .. } => "ToolSignatureWithoutStart",
            TurnError::InvalidToolCalls { .. } => "InvalidToolCalls",
            TurnError::MissingHumanRequestId { .. } => "MissingHumanRequestId",
            TurnError::EventConsumerClosed => "EventConsumerClosed",
            TurnError::Hook(_) => "Hook",
            TurnError::Database(_) => "Database",
            TurnError::Provider(_) => "Provider",
            TurnError::PromptAssembly(_) => "PromptAssembly",
            TurnError::ProviderRetryDeadlineExceeded { .. } => "ProviderRetryDeadlineExceeded",
            TurnError::Cache(_) => "Cache",
        };
        named.insert(name);
    }

    assert_eq!(
        named.len(),
        20,
        "the table covers only {named:?}; every variant needs a value or the rendering \
         claims above are vacuous for the ones missing"
    );
}

#[test]
fn a_plugin_hook_failure_names_the_plugin_and_hook_on_the_user_surface() {
    let error = TurnError::Hook(
        "plugin `fixture-plugin` failed hook `chat.params`: fixture failure".to_owned(),
    );

    let rendered = describe_turn_failure(&error, None);

    assert!(rendered.contains("fixture-plugin"), "{rendered}");
    assert!(rendered.contains("chat.params"), "{rendered}");
    assert!(rendered.contains("fixture failure"), "{rendered}");
}

/// A wrapped failure names its cause instead of only its class.
///
/// The measured defect: an unreachable endpoint, a dead port, a TLS refusal and an
/// unexpanded `${VAR}` all rendered as `transient provider failure (status=None)`, with
/// the URL one `source()` call away the whole time.
#[test]
fn a_transport_failure_names_the_url_it_could_not_reach() {
    let error = TurnError::Provider(ProviderError::transient(std::io::Error::other(
        "error sending request for url (http://${GW_HOST}/v1/chat/completions)",
    )));

    let rendered = describe_turn_failure(&error, None);

    assert!(
        rendered.contains("http://${GW_HOST}/v1/chat/completions"),
        "the transport error's URL was dropped: {rendered}"
    );
    assert!(
        rendered.starts_with("transient provider failure (status=None)"),
        "the category must still lead: {rendered}"
    );
}

/// The credential the turn presented never reaches the message, even echoed verbatim.
///
/// Todo 110 guaranteed no key material on the auth path by building its message from
/// the provider id alone. Walking the `#[source]` chain renders whatever the gateway put
/// in its 401 body, and a gateway answering `Incorrect API key provided: sk-…` is a real
/// shape — so the guarantee now needs enforcing rather than following from the message's
/// construction.
#[test]
fn a_rejected_credential_is_scrubbed_from_the_body_that_echoed_it() {
    let secret = "sk-SUPERSECRET-DO-NOT-ECHO";
    let error = TurnError::Provider(ProviderError::Auth {
        provider: "test".to_owned(),
        source: Some(Box::new(std::io::Error::other(format!(
            "provider `test` returned HTTP 401: {{\"error\":{{\"message\":\"Incorrect API \
             key provided: {secret}\"}}}}"
        )))),
    });

    let rendered = describe_turn_failure(&error, Some(secret));

    assert!(
        !rendered.contains(secret),
        "the rendered failure echoed the key it presented: {rendered}"
    );
    assert!(
        rendered.contains(REDACTED),
        "the key was dropped without a trace, so the message reads as if the gateway \
         said nothing: {rendered}"
    );
    for needle in ["provider.test.options.apiKey", "zuno auth login test"] {
        assert!(
            rendered.contains(needle),
            "scrubbing cost the advice `{needle}`: {rendered}"
        );
    }
}

/// An empty credential is a legitimate configuration and must not corrupt the message.
///
/// `provider_api_key` documents why `apiKey: ""` reaches this path: it means "this
/// endpoint takes no key". `str::replace` with an empty pattern inserts its replacement
/// between every character, so an unguarded scrub would turn every failure a keyless
/// local endpoint produces into unreadable noise.
#[test]
fn an_empty_credential_scrubs_nothing() {
    let error = TurnError::Provider(ProviderError::transient(std::io::Error::other(
        "tcp connect error: Connection refused",
    )));

    assert_eq!(
        describe_turn_failure(&error, Some("")),
        describe_turn_failure(&error, None),
        "an empty credential changed the message it appears in"
    );
}

async fn spawn_turn_response_server(
    chunks: Vec<(std::time::Duration, Vec<u8>)>,
    finish: bool,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind turn-response fixture");
    let address = listener.local_addr().expect("turn fixture address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept turn request");
        read_http_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\
                  Connection: close\r\n\
                  \r\n",
            )
            .await
            .expect("write turn response headers");

        for (delay, chunk) in chunks {
            tokio::time::sleep(delay).await;
            socket
                .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                .await
                .expect("write turn chunk size");
            socket
                .write_all(&chunk)
                .await
                .expect("write turn response chunk");
            socket
                .write_all(b"\r\n")
                .await
                .expect("terminate turn response chunk");
        }

        if finish {
            socket
                .write_all(b"0\r\n\r\n")
                .await
                .expect("finish turn response");
        } else {
            std::future::pending::<()>().await;
        }
    });
    (address, server)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 4096];
        let bytes = socket.read(&mut buffer).await.expect("read turn request");
        assert!(bytes > 0, "turn request ended before its headers");
        request.extend_from_slice(&buffer[..bytes]);
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let content_length = std::str::from_utf8(&request[..header_end])
        .expect("turn request headers are UTF-8")
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("numeric content length")
                })
            })
        })
        .unwrap_or_default();
    while request.len() - header_end < content_length {
        let mut buffer = [0_u8; 4096];
        let bytes = socket
            .read(&mut buffer)
            .await
            .expect("read turn request body");
        assert!(bytes > 0, "turn request ended before its body");
        request.extend_from_slice(&buffer[..bytes]);
    }
    assert!(
        request.starts_with(b"POST /v1/chat/completions "),
        "real turn used an unexpected endpoint"
    );
}

fn chat_delta(text: &str) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-turn-idle",
            "object": "chat.completion.chunk",
            "created": 1_780_000_000,
            "model": "model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": text},
                "finish_reason": null
            }]
        })
    )
    .into_bytes()
}

fn chat_end() -> Vec<u8> {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-turn-idle",
            "object": "chat.completion.chunk",
            "created": 1_780_000_000,
            "model": "model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        })
    )
    .into_bytes()
}

async fn collect_turn_events(
    mut receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
) -> Vec<TurnEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

async fn run_compatible_turn(
    chunks: Vec<(std::time::Duration, Vec<u8>)>,
    finish: bool,
    transport_idle: std::time::Duration,
) -> (
    Result<zuno_engine::r#loop::TurnOutcome, TurnError>,
    Vec<TurnEvent>,
    std::time::Duration,
) {
    let (address, server) = spawn_turn_response_server(chunks, finish).await;
    let transport: Arc<dyn Transport> = Arc::new(
        ReqwestTransport::new(COMPATIBLE_PROVIDER)
            .with_idle_timeout(StreamIdleTimeout::new(transport_idle)),
    );
    let provider_idle = StreamIdleTimeout::new(std::time::Duration::from_secs(2));
    let mut providers = ProviderRegistry::new();
    providers.register_fallible(COMPATIBLE_PROVIDER, move |spec| {
        let provider =
            zuno_provider_compatible::CompatibleProvider::new(spec, Arc::clone(&transport), None)?
                .with_idle_timeout(provider_idle);
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    });

    let spec = Spec::new(COMPATIBLE_PROVIDER)
        .with_surface(ApiSurface::Chat)
        .with_base_url(format!("http://{address}/v1"));
    let resolver = Resolver {
        requested_agent: "build".to_owned(),
        system_prompt: String::new(),
        prompt_assembly: None,
        runtime_prompt_policy: RuntimePromptPolicy::default(),
        max_steps: None,
        requested_provider: "provider".to_owned(),
        requested_model: "model".to_owned(),
        wire_model: "model".to_owned(),
        reasoning_options: serde_json::Map::new(),
        spec,
        orchestration_seed: None,
    };
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open turn fixture database");
    zuno_db::migration::apply(&mut connection).expect("apply turn fixture schema");
    let fixture_plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &fixture_plan.project, now).expect("persist fixture project");
    let session =
        resolve_session(&mut connection, &fixture_plan, now).expect("create fixture session");
    persist_user_message(
        &connection,
        UserMessageInput {
            session_id: &session.id,
            agent: "build",
            provider_id: "provider",
            model_id: "model",
            text: "Show the streamed response.",
            message_id: None,
            now,
        },
    )
    .expect("persist fixture prompt");
    let interrupt = InterruptSignal::new();
    let dispatcher = ToolRegistryDispatcher::new(
        Vec::new(),
        Vec::new(),
        Arc::new(crate::cmd::tool_runtime::HeadlessApproval),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let (sender, receiver) = zuno_engine::r#loop::event_channel();

    let started = std::time::Instant::now();
    let (outcome, events) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(
            run_turn(
                RunTurnRequest::new(session.id, "turn-idle-timeout", DynamicContext::default(),),
                TurnContext::new(
                    &mut connection,
                    &providers,
                    &resolver,
                    &dispatcher,
                    &interrupt,
                ),
                sender,
            ),
            collect_turn_events(receiver)
        )
    })
    .await
    .expect("the real turn must finish inside its one-second test budget");
    let elapsed = started.elapsed();

    if finish {
        server.await.expect("progressing fixture completes");
    } else {
        server.abort();
        let _ = server.await;
    }
    (outcome, events, elapsed)
}

fn visible_text(events: &[TurnEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::Provider {
                event: StreamEvent::TextDelta(text),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_stalled_provider_ends_a_real_turn_with_partial_text_and_a_visible_idle_error() {
    let idle = std::time::Duration::from_millis(75);
    let (outcome, events, elapsed) = run_compatible_turn(
        vec![(std::time::Duration::ZERO, chat_delta("PARTIAL_T166"))],
        false,
        idle,
    )
    .await;

    assert_eq!(
        visible_text(&events),
        "PARTIAL_T166",
        "text emitted before the stall must remain on the user-visible event surface"
    );
    let error = outcome.expect_err("a held-open provider socket must end the turn");
    let rendered = describe_turn_failure(&error, None);
    assert!(rendered.contains("idle timeout"), "{rendered}");
    assert!(
        rendered.contains(zuno_llm::sse::STREAM_IDLE_TIMEOUT_ENV),
        "{rendered}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "the stalled turn exceeded its user-visible bound: {elapsed:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            TurnEvent::Provider {
                event: StreamEvent::RetryRollback { .. },
                ..
            }
        )),
        "the turn retried a partial response instead of failing visibly"
    );
}

#[tokio::test]
async fn a_slow_but_progressing_real_turn_outlives_one_transport_idle_window() {
    let interval = std::time::Duration::from_millis(60);
    let idle = std::time::Duration::from_millis(150);
    let (outcome, events, elapsed) = run_compatible_turn(
        vec![
            (std::time::Duration::ZERO, chat_delta("SLOW_")),
            (interval, chat_delta("BUT_")),
            (interval, chat_delta("STILL_")),
            (interval, chat_delta("MOVING")),
            (std::time::Duration::ZERO, chat_end()),
        ],
        true,
        idle,
    )
    .await;

    outcome.expect("progress inside each idle window must complete the real turn");
    assert_eq!(visible_text(&events), "SLOW_BUT_STILL_MOVING");
    assert!(
        elapsed > idle,
        "the fixture did not outlive one idle window: {elapsed:?} <= {idle:?}"
    );
}

/// Two providers, each with two models, both reachable with the credentials present.
fn two_provider_catalog() -> Catalog {
    let document: zuno_llm::catalog::models_dev::CatalogDocument = serde_json::from_str(
        r#"{"amazon-bedrock":{"id":"amazon-bedrock","name":"Bedrock","env":[],
             "npm":"@ai-sdk/amazon-bedrock",
             "models":{"claude":{"id":"claude","name":"Claude","limit":{"context":1,"output":1}},
                       "nova":{"id":"nova","name":"Nova","limit":{"context":1,"output":1}}}},
           "myopenai":{"id":"myopenai","name":"My OpenAI","env":[],
             "npm":"@ai-sdk/openai-compatible","api":"https://gateway.internal/v1",
             "models":{"gpt-5":{"id":"gpt-5","name":"GPT-5","limit":{"context":1,"output":1}},
                       "o4":{"id":"o4","name":"O4","limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config =
        serde_json::from_str(r#"{"provider":{"amazon-bedrock":{},"myopenai":{}}}"#)
            .expect("config");
    Catalog::resolve(&document, &ResolveInput::new().with_config(&config))
}

/// The owner's defect: `/model` listed one provider while `zuno models` listed ten.
///
/// The picker's data came from the resolved plan's single provider, so no second vendor's
/// model could reach the surface however the view rendered it. Asserting on *distinct
/// provider prefixes* rather than on a count is what makes this fail for that cause: a
/// list that grew but stayed inside one provider still fails here.
#[test]
fn the_picker_enumeration_spans_every_provider_the_catalog_holds() {
    let catalog = two_provider_catalog();

    let offered = picker_models(&catalog);
    let providers = offered
        .iter()
        .filter_map(|choice| choice.id.split_once('/').map(|(provider, _)| provider))
        .collect::<std::collections::BTreeSet<_>>();

    assert!(
        providers.len() >= 2,
        "the picker was offered {} provider(s) from a catalog holding 2: {offered:?}",
        providers.len()
    );
    assert_eq!(
        offered
            .iter()
            .map(|choice| choice.id.clone())
            .collect::<Vec<_>>(),
        catalog.model_lines(),
        "the picker must enumerate through the same function `zuno models` prints from, \
         or the two surfaces can disagree again"
    );
    assert!(
        offered
            .iter()
            .any(|choice| choice.name == "GPT-5" && choice.provider == "My OpenAI"),
        "the picker projection discarded catalog display names: {offered:?}"
    );
    // Pins the defect's mechanism, not just its symptom: the session provider's own slice
    // is what used to be handed over, and it can never span two providers.
    let session_slice = catalog
        .provider("amazon-bedrock")
        .expect("the fixture resolves bedrock")
        .models
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        session_slice.len() < offered.len(),
        "the fixture cannot distinguish one provider's slice from the whole catalog"
    );
}

/// Every assertion below calls [`tool_runtime::assemble`] — the one function
/// `zuno run`, `zuno serve` and the TUI all reach — and never a registry built here.
///
/// That distinction is the whole point. `task` and `skill` were fully implemented and
/// tested in `zuno-tools` while unregistered in the production assembly, and every one
/// of those tests passed the entire time, because each built its own registry. A slot
/// missing from the composition root is only observable from the composition root.
mod production_registry {
    use super::*;
    use std::path::Path;

    #[derive(Debug)]
    struct Fixture {
        _directory: tempfile::TempDir,
        _goal_spill: tempfile::TempDir,
        ids: Vec<String>,
        intents: std::collections::BTreeMap<String, zuno_tool::ToolUiIntent>,
    }

    const DYNAMIC_TOOL_ID: &str = "codegraph_query";

    struct DynamicTool {
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl zuno_tool::Tool for DynamicTool {
        fn id(&self) -> &str {
            DYNAMIC_TOOL_ID
        }

        fn description(&self) -> &str {
            self.description
        }

        fn raw_parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: zuno_tool::ToolContext,
        ) -> Result<zuno_tool::ToolOutput, zuno_error::ToolError> {
            Ok(zuno_tool::ToolOutput::text(self.description, "ok"))
        }
    }

    fn dynamic_tool(description: &'static str) -> Arc<dyn zuno_tool::Tool> {
        Arc::new(DynamicTool { description })
    }

    #[derive(Clone)]
    struct FixedMcpLoader(Vec<Arc<dyn zuno_tool::Tool>>);

    impl zuno_tools::registry::McpToolLoader for FixedMcpLoader {
        fn tools(&self) -> Vec<zuno_tools::registry::CustomTool> {
            self.0.clone()
        }
    }

    fn authority_for(
        tool: &Arc<dyn zuno_tool::Tool>,
    ) -> Arc<[zuno_orchestration::ToolSchemaIdentity]> {
        Arc::from(vec![tool.definition().schema_identity()])
    }

    fn try_assemble_with(
        skills: zuno_catalog::skill::Skills,
        config: zuno_config::schema::Config,
    ) -> Result<Fixture, String> {
        try_assemble_for_agent_with("orchestrator", skills, config)
    }

    fn try_assemble_for_agent_with(
        agent_name: &str,
        skills: zuno_catalog::skill::Skills,
        config: zuno_config::schema::Config,
    ) -> Result<Fixture, String> {
        try_assemble_for_agent_runtime(agent_name, skills, config, None, None)
    }

    fn try_assemble_for_agent_runtime(
        agent_name: &str,
        skills: zuno_catalog::skill::Skills,
        config: zuno_config::schema::Config,
        mcp_loader: Option<Arc<dyn zuno_tools::registry::McpToolLoader>>,
        tool_authority: Option<Arc<[zuno_orchestration::ToolSchemaIdentity]>>,
    ) -> Result<Fixture, String> {
        let directory = tempfile::TempDir::new().expect("temporary tool workspace");
        let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
        let empty_agents = zuno_config::schema::ordered::OrderedMap::new();
        let configured_agents = config.agent.as_ref().unwrap_or(&empty_agents);
        let selected_definition = zuno_catalog::agent::resolve(configured_agents, &[])
            .into_iter()
            .find(|entry| entry.name == agent_name)
            .unwrap_or_else(|| panic!("native Agent `{agent_name}`"));
        let selected_agent = agent_profile(selected_definition, directory.path(), &config);
        let runtime = tool_runtime::assemble(
            directory.path(),
            None,
            &Env::empty(),
            &config,
            &selected_agent,
            tool_runtime::ToolSelection {
                provider_id: "provider",
                model_id: "model",
                manifest: Arc::new(zuno_harness::ToolManifest::standard()),
                contributions: Arc::new(zuno_harness::ToolContributions::default()),
                question: None,
                interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
                background_executions: test_background_executions(directory.path()),
                sandbox: test_sandbox(),
                todo_store: Arc::new(
                    zuno_db::Pool::open(&zuno_paths::DbLocation::Memory)
                        .expect("in-memory todo store"),
                ),
                goal_store: Arc::new(
                    GoalStore::open_memory(goal_spill.path().to_owned())
                        .expect("in-memory goal store"),
                ),
                mcp_loader,
                skills: Arc::new(skills),
                capability: test_capability(),
                delegation: test_delegation(),
                product_agents: test_product_agents(),
                workflows: test_workflows(),
                councils: test_councils(),
                job_controller: test_job_controller(),
                memory: None,
                tool_authority,
            },
        )?;
        let ids = runtime
            .tools
            .iter()
            .map(|tool| tool.id().to_owned())
            .collect::<Vec<_>>();
        let intents = runtime
            .tools
            .iter()
            .map(|tool| (tool.id().to_owned(), tool.ui_intent()))
            .collect();
        Ok(Fixture {
            _directory: directory,
            _goal_spill: goal_spill,
            ids,
            intents,
        })
    }

    fn assemble_with(skills: zuno_catalog::skill::Skills) -> Fixture {
        try_assemble_with(skills, zuno_config::schema::Config::default())
            .expect("production registry assembles")
    }

    fn assemble() -> Fixture {
        assemble_with(zuno_catalog::skill::Skills::default())
    }

    #[test]
    fn delegated_work_agents_inherit_matching_mcp_tools_from_the_parent_attempt() {
        for agent in ["deep", "general"] {
            let tool = dynamic_tool("query the indexed code graph");
            let fixture = try_assemble_for_agent_runtime(
                agent,
                zuno_catalog::skill::Skills::default(),
                zuno_config::schema::Config::default(),
                Some(Arc::new(FixedMcpLoader(vec![Arc::clone(&tool)]))),
                Some(authority_for(&tool)),
            )
            .unwrap_or_else(|error| panic!("{agent} registry assembles: {error}"));

            assert_eq!(fixture.ids, vec![DYNAMIC_TOOL_ID], "{agent}");
        }
    }

    #[test]
    fn read_only_agents_do_not_inherit_arbitrary_mcp_tools() {
        for agent in ["explorer", "oracle"] {
            let tool = dynamic_tool("query the indexed code graph");
            let fixture = try_assemble_for_agent_runtime(
                agent,
                zuno_catalog::skill::Skills::default(),
                zuno_config::schema::Config::default(),
                Some(Arc::new(FixedMcpLoader(vec![Arc::clone(&tool)]))),
                Some(authority_for(&tool)),
            )
            .unwrap_or_else(|error| panic!("{agent} registry assembles: {error}"));

            assert!(fixture.ids.is_empty(), "{agent}: {:?}", fixture.ids);
        }
    }

    #[test]
    fn a_read_only_agent_can_explicitly_opt_into_one_known_mcp_tool() {
        let config = zuno_config::schema::Config::from_json_str(
            Path::new("zuno.json"),
            r#"{
                "agents": {
                    "explorer": {
                        "permission": {
                            "rules": {"codegraph_query": "allow"}
                        }
                    }
                }
            }"#,
        )
        .expect("Agent permission parses");
        let tool = dynamic_tool("query the indexed code graph");
        let fixture = try_assemble_for_agent_runtime(
            "explorer",
            zuno_catalog::skill::Skills::default(),
            config,
            Some(Arc::new(FixedMcpLoader(vec![Arc::clone(&tool)]))),
            Some(authority_for(&tool)),
        )
        .expect("explorer registry assembles");

        assert_eq!(fixture.ids, vec![DYNAMIC_TOOL_ID]);
    }

    #[test]
    fn a_child_cannot_gain_an_mcp_tool_absent_from_the_parent_attempt() {
        let tool = dynamic_tool("query the indexed code graph");
        let fixture = try_assemble_for_agent_runtime(
            "deep",
            zuno_catalog::skill::Skills::default(),
            zuno_config::schema::Config::default(),
            Some(Arc::new(FixedMcpLoader(vec![tool]))),
            Some(Arc::from(
                Vec::<zuno_orchestration::ToolSchemaIdentity>::new(),
            )),
        )
        .expect("deep registry assembles");

        assert!(fixture.ids.is_empty(), "{:?}", fixture.ids);
    }

    #[test]
    fn a_same_name_mcp_tool_with_a_changed_schema_is_not_inherited() {
        let parent_tool = dynamic_tool("parent schema");
        let child_tool = dynamic_tool("changed child schema");
        let fixture = try_assemble_for_agent_runtime(
            "deep",
            zuno_catalog::skill::Skills::default(),
            zuno_config::schema::Config::default(),
            Some(Arc::new(FixedMcpLoader(vec![child_tool]))),
            Some(authority_for(&parent_tool)),
        )
        .expect("deep registry assembles");

        assert!(fixture.ids.is_empty(), "{:?}", fixture.ids);
    }

    #[test]
    fn explicit_user_denies_override_role_level_mcp_inheritance() {
        let config = zuno_config::schema::Config::from_json_str(
            Path::new("zuno.json"),
            r#"{"permission":{"rules":{"codegraph_query":"deny"}}}"#,
        )
        .expect("permission profile parses");
        let tool = dynamic_tool("query the indexed code graph");
        let fixture = try_assemble_for_agent_runtime(
            "deep",
            zuno_catalog::skill::Skills::default(),
            config,
            Some(Arc::new(FixedMcpLoader(vec![Arc::clone(&tool)]))),
            Some(authority_for(&tool)),
        )
        .expect("deep registry assembles");

        assert!(fixture.ids.is_empty(), "{:?}", fixture.ids);
    }

    #[test]
    fn explicit_parallel_without_a_key_fails_before_the_tool_runtime_is_published() {
        let config = zuno_config::schema::Config::from_json_str(
            Path::new("zuno.json"),
            r#"{"web_search":{"provider":"parallel"}}"#,
        )
        .expect("parallel profile parses");

        let error = try_assemble_with(zuno_catalog::skill::Skills::default(), config)
            .expect_err("Parallel must never be registered without its credential");

        assert!(error.contains("parallel"), "{error}");
        assert!(error.contains("PARALLEL_API_KEY"), "{error}");
    }

    #[test]
    fn advertises_the_skill_tool_so_a_skill_body_can_be_loaded_on_demand() {
        let fixture = assemble();

        assert!(
            fixture.ids.iter().any(|id| id == zuno_tools::SKILL_WIRE_ID),
            "the production registry has no `{}`, so no discovered skill can be loaded; \
             visible tools: {:?}",
            zuno_tools::SKILL_WIRE_ID,
            fixture.ids
        );
    }

    #[test]
    fn advertises_the_task_tool_so_work_can_be_delegated_to_a_subagent() {
        let fixture = assemble();

        assert!(
            fixture.ids.iter().any(|id| id == zuno_tools::TASK_WIRE_ID),
            "the production registry has no `{}`, so the model cannot delegate at all; \
             visible tools: {:?}",
            zuno_tools::TASK_WIRE_ID,
            fixture.ids
        );
    }

    #[test]
    fn direct_build_mode_hides_every_subagent_intent_tool() {
        let config = zuno_config::schema::Config::from_json_str(
            Path::new("opencode.json"),
            r#"{
                "agents": {
                    "general": {"mode": "subagent"}
                },
                "workflows": {
                    "bounded": {
                        "nodes": [
                            {"id": "work", "agent": "general"}
                        ]
                    }
                },
                "productAgent": {
                    "codex-review": {
                        "kind": "codex",
                        "enabled": true,
                        "command": "codex",
                        "toolName": "subagent_codex_review",
                        "permissionMode": "never"
                    }
                }
            }"#,
        )
        .expect("build-mode config");

        let fixture =
            try_assemble_for_agent_with("build", zuno_catalog::skill::Skills::default(), config)
                .expect("direct build registry assembles");

        for id in [
            zuno_tools::TASK_WIRE_ID,
            zuno_tools::WORKFLOW_WIRE_ID,
            "subagent_codex_review",
        ] {
            assert!(
                !fixture.ids.iter().any(|candidate| candidate == id),
                "direct build exposed `{id}`: {:?}",
                fixture.ids
            );
        }
        assert!(
            fixture
                .intents
                .values()
                .all(|intent| *intent != zuno_tool::ToolUiIntent::Subagent),
            "direct build retained a subagent-intent tool: {:?}",
            fixture.intents
        );
    }

    #[test]
    fn advertises_job_cancel_only_because_the_live_controller_is_wired() {
        let fixture = assemble();

        assert!(
            fixture
                .ids
                .iter()
                .any(|id| id == zuno_tools::JOB_CANCEL_WIRE_ID),
            "the production registry has no `{}`; visible tools: {:?}",
            zuno_tools::JOB_CANCEL_WIRE_ID,
            fixture.ids
        );
    }

    #[test]
    fn product_agents_are_default_off_and_enabled_instances_register_static_tools() {
        let disabled = assemble();
        assert!(
            !disabled.ids.iter().any(
                |id| id.starts_with("subagent_codex") || id.starts_with("subagent_claude_code")
            ),
            "default configuration exposed product agents: {:?}",
            disabled.ids
        );

        let config = zuno_config::schema::Config::from_json_str(
            Path::new("opencode.json"),
            r#"{
                "productAgent": {
                    "codex-review": {
                        "kind": "codex",
                        "enabled": true,
                        "command": "codex",
                        "toolName": "subagent_codex_review",
                        "permissionMode": "never"
                    },
                    "claude-audit": {
                        "kind": "claude-code",
                        "enabled": true,
                        "command": "claude",
                        "toolName": "subagent_claude_audit",
                        "permissionMode": "dontAsk"
                    }
                }
            }"#,
        )
        .expect("product-agent config");
        let enabled =
            try_assemble_with(zuno_catalog::skill::Skills::default(), config).expect("assemble");

        for id in ["subagent_codex_review", "subagent_claude_audit"] {
            assert!(
                enabled.ids.iter().any(|candidate| candidate == id),
                "{enabled:?}"
            );
            assert_eq!(
                enabled.intents.get(id),
                Some(&zuno_tool::ToolUiIntent::Subagent)
            );
        }
    }

    #[test]
    fn duplicate_or_native_product_tool_names_fail_before_a_turn_starts() {
        for (document, expected) in [
            (
                r#"{
                    "productAgent": {
                        "one": {"kind":"codex","enabled":true,"toolName":"same"},
                        "two": {"kind":"claude-code","enabled":true,"toolName":"same"}
                    }
                }"#,
                "distinct toolName",
            ),
            (
                r#"{
                    "productAgent": {
                        "one": {"kind":"codex","enabled":true,"toolName":"task"}
                    }
                }"#,
                "collides with a native tool",
            ),
            (
                r#"{
                    "productAgent": {
                        "one": {"kind":"codex","enabled":true,"toolName":"memory_propose"}
                    }
                }"#,
                "collides with a native tool",
            ),
            (
                r#"{
                    "productAgent": {
                        "one": {"kind":"codex","enabled":true,"toolName":"goal_get"}
                    }
                }"#,
                "collides with a native tool",
            ),
        ] {
            let config =
                zuno_config::schema::Config::from_json_str(Path::new("opencode.json"), document)
                    .expect("config parses before assembly validation");
            let error = try_assemble_with(zuno_catalog::skill::Skills::default(), config)
                .expect_err("invalid static tool registration");
            assert!(error.contains(expected), "{error}");
        }
    }

    /// The `skill` tool must answer from the same set the prompt advertised.
    ///
    /// One load shared by both consumers, so a name in `<skill_index>` is
    /// necessarily a name the tool can load. Two loads would let them disagree.
    #[tokio::test]
    async fn the_skill_tool_answers_from_the_very_set_the_prompt_was_built_from() {
        use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

        let skills = zuno_catalog::skill::Skills::from_loaded([
            zuno_catalog::skill::Skill::embedded_at_path(
                "wired",
                Some("proves the registry holds this exact set".to_owned()),
                PathBuf::from("/skills/wired/SKILL.md"),
                "the body the model must receive",
            ),
        ]);
        let advertised = skills.render(zuno_catalog::skill::Form::Verbose);
        let directory = tempfile::TempDir::new().expect("temporary tool workspace");
        let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
        let config = zuno_config::schema::Config::default();
        let selected_agent = agent_profile(agent("build"), directory.path(), &config);
        let runtime = tool_runtime::assemble(
            directory.path(),
            None,
            &Env::empty(),
            &config,
            &selected_agent,
            tool_runtime::ToolSelection {
                provider_id: "provider",
                model_id: "model",
                manifest: Arc::new(zuno_harness::ToolManifest::standard()),
                contributions: Arc::new(zuno_harness::ToolContributions::default()),
                question: None,
                interaction_policy: zuno_goal::InteractionPolicy::WorkOnDemand,
                background_executions: test_background_executions(directory.path()),
                sandbox: test_sandbox(),
                todo_store: Arc::new(
                    zuno_db::Pool::open(&zuno_paths::DbLocation::Memory)
                        .expect("in-memory todo store"),
                ),
                goal_store: Arc::new(
                    GoalStore::open_memory(goal_spill.path().to_owned())
                        .expect("in-memory goal store"),
                ),
                mcp_loader: None,
                skills: Arc::new(skills),
                capability: test_capability(),
                delegation: test_delegation(),
                product_agents: test_product_agents(),
                workflows: test_workflows(),
                councils: test_councils(),
                job_controller: test_job_controller(),
                memory: None,
                tool_authority: None,
            },
        )
        .expect("production registry assembles");

        assert!(advertised.contains("<name>wired</name>"));
        let tool = runtime
            .tools
            .iter()
            .find(|tool| tool.id() == zuno_tools::SKILL_WIRE_ID)
            .expect("the assembled registry advertises `skill`");
        let output = tool
            .invoke(
                serde_json::json!({"action": "load", "name": "wired"}),
                ToolContext::new(
                    "ses_registry",
                    "msg_registry",
                    "call_registry",
                    "build",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                ),
            )
            .await
            .expect("the advertised skill loads through the assembled tool");

        assert!(
            output.output.contains("the body the model must receive"),
            "{}",
            output.output
        );
        assert!(
            output.output.contains("Resource root: `/skills/wired`")
                && output.output.contains("action `read_resource`"),
            "the model did not receive authoritative resource guidance:\n{}",
            output.output
        );
    }
}

/// Skill discovery must stay model-visible without embedding every description.
mod skill_prompt {
    use super::*;

    fn skill(name: &str, description: &str) -> zuno_catalog::skill::Skill {
        zuno_catalog::skill::Skill::embedded_at_path(
            name,
            Some(description.to_owned()),
            PathBuf::from(format!("/skills/{name}/SKILL.md")),
            "body",
        )
    }

    fn resolver() -> Resolver {
        traced_resolver("AGENT PROMPT")
    }

    #[test]
    fn council_routing_is_typed_and_scoped_to_one_resolver_clone() {
        let resolver = resolver();
        let routed = resolver
            .with_prompt_section(
                "routing.council",
                "zuno-tui:/council",
                "Invoke council_run exactly once.",
            )
            .expect("Council routing block");

        let base = resolver
            .prompt_assembly
            .as_ref()
            .expect("structured base prompt")
            .envelope();
        let routed_envelope = routed
            .prompt_assembly
            .as_ref()
            .expect("structured routed prompt")
            .envelope();
        assert!(base.routing.is_empty());
        assert_eq!(routed_envelope.routing.len(), 1);
        assert_eq!(
            routed_envelope.routing[0].content(),
            "Invoke council_run exactly once."
        );
        assert_eq!(resolver.system_prompt, "AGENT PROMPT");
        assert_eq!(
            routed.system_prompt,
            "AGENT PROMPT\n\nInvoke council_run exactly once."
        );
    }

    #[test]
    fn a_discovered_skill_reaches_the_prompt_without_displacing_the_agents_own() {
        let skills = zuno_catalog::skill::Skills::from_loaded([skill("deploy", "Ship it.")]);
        let mut resolver = resolver();
        announce_skills(&mut resolver, &skills, 0, None).expect("announce skills");

        assert!(
            resolver.system_prompt.starts_with("AGENT PROMPT"),
            "the agent's own prompt must stay first: {}",
            resolver.system_prompt
        );
        assert!(resolver.system_prompt.contains("name=\"deploy\""));
        assert!(resolver.system_prompt.contains("<skill_index>"));
        assert!(
            resolver.system_prompt.contains("Ship it."),
            "the catalog must expose trigger metadata"
        );
        assert!(
            resolver.system_prompt.contains("/skills/deploy/SKILL.md"),
            "the catalog must expose the exact source needed for loading"
        );
        let policy = resolver
            .system_prompt
            .find(SKILL_USAGE_POLICY)
            .expect("skill trigger policy");
        let catalog = resolver
            .system_prompt
            .find("name=\"deploy\"")
            .expect("skill catalogue");
        assert!(
            policy < catalog,
            "the model saw the catalogue before the rules that make it actionable"
        );
        assert!(
            resolver.system_prompt.contains("action `search`"),
            "the prompt does not explain how descriptions are discovered"
        );
        assert!(
            resolver.system_prompt.contains("action `load`"),
            "the prompt does not require loading the selected skill body"
        );
    }

    #[test]
    fn required_skills_resolve_unique_sources_in_configured_order() {
        let skills = zuno_catalog::skill::Skills::from_loaded([
            zuno_catalog::skill::Skill::embedded_at_path(
                "review",
                Some("Review guidance".to_owned()),
                PathBuf::from("/skills/review/SKILL.md"),
                "review body",
            ),
            zuno_catalog::skill::Skill::embedded_at_path(
                "codegraph",
                Some("CodeGraph guidance".to_owned()),
                PathBuf::from("/skills/codegraph/SKILL.md"),
                "codegraph body",
            ),
        ]);
        let configured = vec!["codegraph".to_owned(), "review".to_owned()];

        let resolved =
            resolve_required_skill_identities("explorer", Some(configured.as_slice()), &skills)
                .expect("required Skills resolve");

        assert_eq!(
            resolved,
            vec![
                SelectedSkillIdentity {
                    name: "codegraph".to_owned(),
                    source: "/skills/codegraph/SKILL.md".to_owned(),
                },
                SelectedSkillIdentity {
                    name: "review".to_owned(),
                    source: "/skills/review/SKILL.md".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn required_skills_fail_closed_when_missing_or_ambiguous() {
        let missing = resolve_required_skill_identities(
            "explorer",
            Some(&["codegraph".to_owned()]),
            &zuno_catalog::skill::Skills::default(),
        )
        .expect_err("missing required Skill must stop turn resolution");
        assert!(
            missing.contains("agents.explorer.requiredSkills"),
            "{missing}"
        );
        assert!(missing.contains("codegraph"), "{missing}");

        let skills = zuno_catalog::skill::Skills::from_loaded([
            zuno_catalog::skill::Skill::embedded_at_path(
                "codegraph",
                None,
                PathBuf::from("/skills/one/SKILL.md"),
                "one",
            ),
            zuno_catalog::skill::Skill::embedded_at_path(
                "codegraph",
                None,
                PathBuf::from("/skills/two/SKILL.md"),
                "two",
            ),
        ]);
        let ambiguous =
            resolve_required_skill_identities("explorer", Some(&["codegraph".to_owned()]), &skills)
                .expect_err("ambiguous required Skill must stop turn resolution");
        assert!(
            ambiguous.contains("agents.explorer.requiredSkills"),
            "{ambiguous}"
        );
        assert!(ambiguous.contains("/skills/one/SKILL.md"), "{ambiguous}");
        assert!(ambiguous.contains("/skills/two/SKILL.md"), "{ambiguous}");
    }

    #[tokio::test]
    async fn required_skills_load_before_explicit_mentions_and_deduplicate_by_source() {
        let skills = zuno_catalog::skill::Skills::from_loaded([
            zuno_catalog::skill::Skill::embedded_at_path(
                "codegraph",
                Some("Navigate indexed code.".to_owned()),
                PathBuf::from("/skills/codegraph/SKILL.md"),
                "# Complete codegraph guidance",
            ),
        ]);
        let required = vec![SelectedSkillIdentity {
            name: "codegraph".to_owned(),
            source: "/skills/codegraph/SKILL.md".to_owned(),
        }];
        let mut resolver = resolver();
        let mut loaded = BTreeSet::new();

        let first = preload_required_skills(
            &mut resolver,
            &skills,
            &mut loaded,
            &required,
            SELECTED_SKILL_PROMPT_MAX_BYTES,
        )
        .await
        .expect("required Skill preloads");
        let explicit = preload_explicit_skills(
            &mut resolver,
            &skills,
            &mut loaded,
            "use codegraph for this task",
            SELECTED_SKILL_PROMPT_MAX_BYTES,
        )
        .await
        .expect("explicit Skill scan");

        assert_eq!(first, required);
        assert!(explicit.is_empty());
        assert_eq!(
            resolver
                .prompt_assembly
                .as_ref()
                .expect("prompt")
                .envelope()
                .selected_skills
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_explicit_unique_skill_is_loaded_before_the_first_model_request() {
        let skills = zuno_catalog::skill::Skills::from_loaded([
            zuno_catalog::skill::Skill::embedded_at_path(
                "codegraph",
                Some("Navigate indexed code.".to_owned()),
                PathBuf::from("/skills/codegraph/SKILL.md"),
                "# Complete codegraph guidance",
            ),
        ]);
        let mut resolver = resolver();
        announce_skills(&mut resolver, &skills, 0, None).expect("announce skills");
        let mut loaded = BTreeSet::new();

        let selected = preload_explicit_skills(
            &mut resolver,
            &skills,
            &mut loaded,
            "请按照codegraph指导分析本项目",
            SELECTED_SKILL_PROMPT_MAX_BYTES,
        )
        .await
        .expect("preload explicit skill");

        assert_eq!(
            selected,
            vec![SelectedSkillIdentity {
                name: "codegraph".to_owned(),
                source: "/skills/codegraph/SKILL.md".to_owned(),
            }]
        );
        let envelope = resolver
            .prompt_assembly
            .as_ref()
            .expect("structured prompt")
            .envelope();
        assert_eq!(envelope.selected_skills.len(), 1);
        assert_eq!(
            envelope.selected_skills[0].content(),
            "# Complete codegraph guidance"
        );
        assert!(
            resolver
                .system_prompt
                .contains("# Complete codegraph guidance"),
            "the first request would not receive the named Skill body"
        );
    }

    #[tokio::test]
    async fn ambiguous_names_and_identifier_substrings_are_not_preloaded() {
        let skills = zuno_catalog::skill::Skills::from_loaded([
            zuno_catalog::skill::Skill::embedded_at_path(
                "review",
                Some("first review".to_owned()),
                PathBuf::from("/skills/one/SKILL.md"),
                "one",
            ),
            zuno_catalog::skill::Skill::embedded_at_path(
                "review",
                Some("second review".to_owned()),
                PathBuf::from("/skills/two/SKILL.md"),
                "two",
            ),
            zuno_catalog::skill::Skill::embedded_at_path(
                "git",
                Some("Git guidance".to_owned()),
                PathBuf::from("/skills/git/SKILL.md"),
                "git body",
            ),
        ]);
        let mut resolver = resolver();
        let mut loaded = BTreeSet::new();

        let selected = preload_explicit_skills(
            &mut resolver,
            &skills,
            &mut loaded,
            "review the github project",
            SELECTED_SKILL_PROMPT_MAX_BYTES,
        )
        .await
        .expect("ambiguous names are deferred to source-aware tool loading");

        assert!(selected.is_empty());
        assert!(loaded.is_empty());
        assert!(
            resolver
                .prompt_assembly
                .as_ref()
                .expect("prompt")
                .envelope()
                .selected_skills
                .is_empty()
        );
    }

    #[test]
    fn selected_skill_prompt_blocks_restore_from_the_latest_receipt() {
        let mut connection =
            zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        let mut recorded = PromptAssembly::new();
        recorded
            .push("agent.base", "native:build", "BUILD")
            .expect("base");
        recorded
            .push_selected_skill("release", "/skills/release/SKILL.md", "RELEASE BODY")
            .expect("selected skill");
        let system = recorded.system_messages();
        let developer = Vec::new();
        let projection = zuno_engine::prompt::PromptProviderProjection {
            system_messages: &system,
            developer_context: &developer,
        };
        let data = serde_json::to_string(&Value::Object(
            recorded.event_properties("build", 1, projection, projection),
        ))
        .expect("encode receipt");
        connection
            .execute(
                "INSERT INTO event_sequence (aggregate_id, seq) VALUES ('ses_restore', 0)",
                [],
            )
            .expect("event sequence");
        connection
            .execute(
                "INSERT INTO event (id, aggregate_id, seq, type, data) \
                 VALUES ('evt_restore', 'ses_restore', 0, 'session.prompt.assembled.1', ?1)",
                [data],
            )
            .expect("prompt receipt");
        let mut restored_prompt = resolver();

        let restored = restore_selected_skills(
            &connection,
            "ses_restore",
            &mut restored_prompt,
            SELECTED_SKILL_PROMPT_MAX_BYTES,
        )
        .expect("restore selected Skill");

        assert!(restored.contains(&SelectedSkillIdentity {
            name: "release".to_owned(),
            source: "/skills/release/SKILL.md".to_owned(),
        }));
        assert_eq!(
            restored_prompt
                .prompt_assembly
                .as_ref()
                .expect("prompt")
                .envelope()
                .selected_skills[0]
                .content(),
            "RELEASE BODY"
        );
    }

    #[tokio::test]
    async fn selected_skill_bodies_share_one_aggregate_prompt_budget() {
        let skills = zuno_catalog::skill::Skills::from_loaded([
            zuno_catalog::skill::Skill::embedded_at_path(
                "first",
                Some("First".to_owned()),
                PathBuf::from("/skills/first/SKILL.md"),
                "123456",
            ),
            zuno_catalog::skill::Skill::embedded_at_path(
                "second",
                Some("Second".to_owned()),
                PathBuf::from("/skills/second/SKILL.md"),
                "abcdef",
            ),
        ]);
        let mut resolver = resolver();
        let mut loaded = BTreeSet::new();

        preload_selected_skill(
            &mut resolver,
            &skills,
            &mut loaded,
            "first",
            "/skills/first/SKILL.md",
            10,
        )
        .await
        .expect("first body fits");
        let error = preload_selected_skill(
            &mut resolver,
            &skills,
            &mut loaded,
            "second",
            "/skills/second/SKILL.md",
            10,
        )
        .await
        .expect_err("the second body exceeds the aggregate budget");

        assert!(error.contains("selected Skill `second`"), "{error}");
        assert!(error.contains("10-byte aggregate prompt budget"), "{error}");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            resolver
                .prompt_assembly
                .as_ref()
                .expect("prompt")
                .envelope()
                .selected_skills
                .len(),
            1
        );
    }

    #[test]
    fn selected_skill_prompt_budget_is_separate_from_the_metadata_budget() {
        let config = zuno_config::schema::SkillsConfig {
            max_context_tokens: Some(std::num::NonZeroU32::new(1).expect("non-zero")),
            max_selected_context_tokens: Some(std::num::NonZeroU32::new(3_000).expect("non-zero")),
            ..zuno_config::schema::SkillsConfig::default()
        };

        assert_eq!(
            selected_skill_prompt_budget(1_000_000, Some(&config)),
            3_000 * APPROX_BYTES_PER_TOKEN
        );
        assert_eq!(
            skill_metadata_budget(1_000_000, Some(&config)),
            APPROX_BYTES_PER_TOKEN
        );
    }

    #[test]
    fn restored_selected_skills_fail_closed_when_the_current_model_budget_is_smaller() {
        let mut connection =
            zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        let mut recorded = PromptAssembly::new();
        recorded
            .push_selected_skill("large", "/skills/large/SKILL.md", "body larger than budget")
            .expect("selected skill");
        let system = recorded.system_messages();
        let developer = Vec::new();
        let projection = zuno_engine::prompt::PromptProviderProjection {
            system_messages: &system,
            developer_context: &developer,
        };
        let data = serde_json::to_string(&Value::Object(
            recorded.event_properties("build", 1, projection, projection),
        ))
        .expect("encode receipt");
        connection
            .execute(
                "INSERT INTO event_sequence (aggregate_id, seq) VALUES ('ses_restore_budget', 0)",
                [],
            )
            .expect("event sequence");
        connection
            .execute(
                "INSERT INTO event (id, aggregate_id, seq, type, data) \
                 VALUES ('evt_restore_budget', 'ses_restore_budget', 0, \
                 'session.prompt.assembled.1', ?1)",
                [data],
            )
            .expect("prompt receipt");
        let mut restored_prompt = resolver();

        let error =
            restore_selected_skills(&connection, "ses_restore_budget", &mut restored_prompt, 4)
                .expect_err("a model switch must not silently restore an over-budget body");

        assert!(error.contains("selected Skill `large`"), "{error}");
        assert!(
            restored_prompt
                .prompt_assembly
                .as_ref()
                .expect("prompt")
                .envelope()
                .selected_skills
                .is_empty()
        );
    }

    #[test]
    fn an_empty_catalogue_leaves_the_prompt_byte_identical() {
        let mut resolver = resolver();
        let before = resolver.system_prompt.clone();
        announce_skills(
            &mut resolver,
            &zuno_catalog::skill::Skills::default(),
            0,
            None,
        )
        .expect("empty catalogue");

        assert_eq!(resolver.system_prompt.as_bytes(), before.as_bytes());
    }

    #[test]
    fn skill_policy_owns_the_runtime_authority_guard_once() {
        let skills =
            zuno_catalog::skill::Skills::from_loaded([skill("release", "release workflow")]);
        let mut resolver = resolver();

        announce_skills(&mut resolver, &skills, 1_000_000, None).expect("announce skill index");

        assert_eq!(
            resolver
                .system_prompt
                .matches("A Skill does not grant tools, permissions")
                .count(),
            1
        );
    }

    /// Large descriptions must not make any skill name undiscoverable.
    #[test]
    fn a_large_corpus_keeps_every_source_identity_by_shortening_descriptions() {
        let padding = "d".repeat(2_000);
        let skills = zuno_catalog::skill::Skills::from_loaded(
            (0..137).map(|at| skill(&format!("skill-{at:03}"), &padding)),
        );
        let mut resolver = resolver();
        announce_skills(&mut resolver, &skills, 1_000_000, None).expect("announce skill index");

        assert!(
            resolver.system_prompt.len()
                < "AGENT PROMPT".len()
                    + MAX_SKILL_METADATA_TOKEN_BUDGET * APPROX_BYTES_PER_TOKEN
                    + SKILL_USAGE_POLICY.len()
                    + 512,
            "the compact index exceeded its budget: {} bytes",
            resolver.system_prompt.len()
        );
        assert!(resolver.system_prompt.contains("name=\"skill-000\""));
        assert!(resolver.system_prompt.contains("name=\"skill-136\""));
        assert!(
            resolver
                .system_prompt
                .contains("/skills/skill-136/SKILL.md")
        );
        assert!(
            !resolver.system_prompt.contains(&padding),
            "large descriptions leaked back into the base prompt"
        );
    }

    #[test]
    fn a_future_name_set_past_the_index_budget_remains_searchable_without_a_warning() {
        let long = "x".repeat(900);
        let skills = zuno_catalog::skill::Skills::from_loaded(
            (0..32).map(|at| skill(&format!("skill-{at:03}-{long}"), "searchable capability")),
        );
        let mut resolver = resolver();

        announce_skills(&mut resolver, &skills, 0, None).expect("announce partial skill index");

        assert!(
            resolver.system_prompt.contains("Catalog coverage:")
                || resolver.system_prompt.contains("metadata budget omitted"),
            "{}",
            resolver.system_prompt
        );
        assert!(
            resolver.system_prompt.contains("action `list` or `search`")
                || resolver.system_prompt.contains("Action `list`"),
            "{}",
            resolver.system_prompt
        );
        assert!(
            !resolver.system_prompt.contains("were not advertised"),
            "a partial convenience index must not be reported as lost capability"
        );
    }
}

/// The `AGENTS.md`-class rules must reach the system prompt, or they govern nothing.
///
/// [`zuno_config::Instructions`] was a complete, tested port with **zero** production
/// callers: a user could write `AGENTS.md` at either level, or list files in
/// `instructions`, and none of it was ever sent. These assertions run the real
/// discovery against real files, so they cover the seam and the semantics together.
mod instruction_prompt {
    use super::*;
    use std::path::Path;

    fn env_for(root: &Path) -> Env {
        Env::empty()
            .with(
                zuno_paths::env::HOME,
                root.join("home").to_string_lossy().into_owned(),
            )
            .with(
                zuno_paths::env::XDG_CONFIG_HOME,
                root.join("home/.config").to_string_lossy().into_owned(),
            )
    }

    fn write(path: &Path, body: impl AsRef<[u8]>) {
        std::fs::create_dir_all(path.parent().expect("a parent directory")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    fn options(
        root: &Path,
        directory: PathBuf,
        instructions: Vec<String>,
    ) -> zuno_config::InstructionOptions {
        zuno_config::InstructionOptions::new(
            directory,
            Some(root.join("repo")),
            &env_for(root),
            instructions,
        )
    }

    fn resolver() -> Resolver {
        traced_resolver("AGENT PROMPT")
    }

    async fn inject(options: &zuno_config::InstructionOptions) -> (Resolver, Vec<String>) {
        let loaded = zuno_config::Instructions::discover(options).load().await;
        let mut resolver = resolver();
        let mut notes = Vec::new();
        announce_instructions(&mut resolver, &loaded, &mut notes).expect("announce instructions");
        (resolver, notes)
    }

    #[tokio::test]
    async fn the_global_rule_file_reaches_the_system_prompt() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let global = root.path().join("home/.config/zuno/AGENTS.md");
        write(&global, "GLOBAL_RULE_MARKER");
        std::fs::create_dir_all(root.path().join("repo")).expect("mkdir repo");

        let (resolver, notes) =
            inject(&options(root.path(), root.path().join("repo"), Vec::new())).await;

        assert!(
            resolver.system_prompt.starts_with("AGENT PROMPT"),
            "the agent's own prompt must stay first: {}",
            resolver.system_prompt
        );
        assert!(
            resolver.system_prompt.contains("GLOBAL_RULE_MARKER"),
            "the global rule file never reached the prompt: {}",
            resolver.system_prompt
        );
        assert!(
            resolver
                .system_prompt
                .contains(&format!("Instructions from: {}", global.display())),
            "the oracle's header must name the source: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[tokio::test]
    async fn the_project_cascade_reaches_the_prompt_at_every_level() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "ROOT_RULE_MARKER");
        write(&repo.join("sub/AGENTS.md"), "SUB_RULE_MARKER");

        let (resolver, notes) = inject(&options(root.path(), repo.join("sub"), Vec::new())).await;

        let sub_at = resolver
            .system_prompt
            .find("SUB_RULE_MARKER")
            .expect("the nearest level must reach the prompt");
        let root_at = resolver
            .system_prompt
            .find("ROOT_RULE_MARKER")
            .expect("the worktree level must reach the prompt too, not only the nearest");
        assert!(
            root_at < sub_at,
            "the project cascade must render root to cwd so the nearest rule has the highest later priority: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// Cross-product instruction files never participate in Zuno's native cascade.
    #[tokio::test]
    async fn claude_md_is_never_loaded_implicitly() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "ROOT_RULE_MARKER");
        write(&repo.join("sub/CLAUDE.md"), "SUB_CLAUDE_MARKER");

        let (resolver, notes) = inject(&options(root.path(), repo.join("sub"), Vec::new())).await;

        assert!(
            resolver.system_prompt.contains("ROOT_RULE_MARKER"),
            "{}",
            resolver.system_prompt
        );
        assert!(
            !resolver.system_prompt.contains("SUB_CLAUDE_MARKER"),
            "Zuno must not load `CLAUDE.md` implicitly: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[tokio::test]
    async fn configured_instruction_entries_reach_the_prompt() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("docs/house-style.md"), "CONFIGURED_RULE_MARKER");
        write(
            &root.path().join("home/tilde-rules.md"),
            "TILDE_RULE_MARKER",
        );

        let (resolver, notes) = inject(&options(
            root.path(),
            repo,
            vec!["docs/*.md".to_owned(), "~/tilde-rules.md".to_owned()],
        ))
        .await;

        assert!(
            resolver.system_prompt.contains("CONFIGURED_RULE_MARKER"),
            "an `instructions` glob never reached the prompt: {}",
            resolver.system_prompt
        );
        assert!(
            resolver.system_prompt.contains("TILDE_RULE_MARKER"),
            "a `~/`-relative `instructions` entry never reached the prompt: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// The common case — no rule file anywhere — must cost nothing and say nothing.
    ///
    /// Byte equality, not "contains": a stray `\n\n` for an absent file would ride in
    /// front of every request for the life of the session and invalidate a prompt cache
    /// that had no reason to move.
    #[tokio::test]
    async fn a_project_with_no_rule_file_leaves_the_prompt_byte_identical() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let (resolver, notes) = inject(&options(root.path(), repo, Vec::new())).await;

        assert_eq!(
            resolver.system_prompt.as_bytes(),
            b"AGENT PROMPT",
            "an absent instruction file must add no bytes at all"
        );
        assert!(
            notes.is_empty(),
            "a missing rule file is the normal case and must be silent: {notes:?}"
        );
    }

    /// An unreadable file is reported, once, and never silently skipped.
    ///
    /// A rule the user wrote and believes is in force, that the agent never received,
    /// is the worst of the three outcomes — worse than a hard failure, which they would
    /// at least notice. The count matters as much as the text: this is surfaced from a
    /// load that happens once per host, not once per turn.
    #[tokio::test]
    async fn an_unreadable_rule_file_is_reported_exactly_once() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), [0xff_u8, 0xfe, 0x00, 0x9c]);

        let (resolver, notes) = inject(&options(root.path(), repo.clone(), Vec::new())).await;

        assert_eq!(
            resolver.system_prompt.as_bytes(),
            b"AGENT PROMPT",
            "an unreadable file must not contribute bytes"
        );
        assert_eq!(
            notes.len(),
            1,
            "an unreadable rule file must be reported once — no more, and never zero: \
             {notes:?}"
        );
        assert!(
            notes[0].contains(&repo.join("AGENTS.md").display().to_string()),
            "the report must name the file the user has to fix: {}",
            notes[0]
        );
        assert!(
            notes[0].contains("could not be read"),
            "the report must say what went wrong: {}",
            notes[0]
        );
    }

    /// Past the budget a whole file is dropped and named — never cut mid-rule.
    #[tokio::test]
    async fn an_oversized_rule_file_is_dropped_whole_and_the_drop_is_reported() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        let oversized = repo.join("AGENTS.md");
        write(
            &oversized,
            format!(
                "OVERSIZED_RULE_MARKER{}",
                "r".repeat(INSTRUCTION_PROMPT_BUDGET)
            ),
        );
        write(&root.path().join("home/small.md"), "SMALL_RULE_MARKER");

        let (resolver, notes) =
            inject(&options(root.path(), repo, vec!["~/small.md".to_owned()])).await;

        assert!(
            !resolver.system_prompt.contains("OVERSIZED_RULE_MARKER"),
            "a file past the budget must be dropped whole, not truncated into a rule \
             that says something else"
        );
        assert!(
            resolver.system_prompt.len() <= "AGENT PROMPT".len() + 2 + INSTRUCTION_PROMPT_BUDGET,
            "the prompt exceeded the budget: {} bytes",
            resolver.system_prompt.len()
        );
        assert!(
            resolver.system_prompt.contains("SMALL_RULE_MARKER"),
            "one oversized file must not starve the rest: {}",
            resolver.system_prompt
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains(&oversized.display().to_string()),
            "the report must name the file: {}",
            notes[0]
        );
        assert!(
            notes[0].contains("none of its rules are in force"),
            "the report must say the rules are not in effect, not merely that bytes were \
             trimmed: {}",
            notes[0]
        );
        assert!(
            !notes[0].contains("OVERSIZED_RULE_MARKER"),
            "instruction contents are user-authored and must never be echoed: {}",
            notes[0]
        );
    }
}

/// Instruction files must be injected once, and between memory and the skills.
///
/// Two independent failures this pins. **Absence**: the injection deleted compiles,
/// lints and passes every behavioural test above, because those call
/// [`announce_instructions`] directly rather than through the composition root — the
/// exact shape of the original defect, where the whole module had no caller.
/// **Order**: the oracle assembles `[...environment, ...instructions, ...skills]`
/// (`session/prompt.ts:1257-1269`), and moving this call past `announce_skills` would
/// silently invert precedence between a user's rule and a skill's description.
#[test]
fn instruction_files_are_injected_once_between_memory_and_the_skill_catalogue() {
    let turn = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/turn.rs"),
    )
    .expect("turn.rs is readable");

    let memory_at = turn
        .find("configure_resident_memory(&mut plan.resolver, &plan.config, memory_paths.clone())?;")
        .expect("the resident-memory call site moved; this test's anchors need updating");
    let instructions_at = turn
        .find("announce_instructions(&mut plan.resolver")
        .expect(
            "`turn.rs` no longer injects instruction files, so a user's `AGENTS.md` reaches \
             no request and nothing reports it",
        );
    let skills_at = turn
        .find("announce_skills(")
        .expect("the skill-catalogue call site moved; this test's anchors need updating");

    assert!(
        memory_at < instructions_at && instructions_at < skills_at,
        "instruction files must be assembled after memory and before the skill \
         catalogue, mirroring the oracle's segment order"
    );
    assert_eq!(
        turn.matches("announce_instructions(&mut plan.resolver")
            .count(),
        1,
        "instruction files must be injected at exactly one site: a second call would \
         charge the user for every rule file twice on every request"
    );
    assert!(
        turn.contains("instructions,\n            delegation_facts,"),
        "`TurnPlan` no longer carries the loaded instruction files, so the read would \
         have to repeat per turn"
    );
}

/// The three wirings this task closed, asserted at their production call sites.
///
/// # Why a source scan and not a behavioural assertion
///
/// The same reason [`only_this_module_composes_a_turn`] is one, and the defect class
/// is identical: each of these was **absent**, and absence produced no error, no
/// warning, and no failing test — the model was simply told less than the build could
/// do. Reaching these through behaviour needs a resolved catalog, a credential and a
/// live provider, which is why nothing covered them for as long as it did.
///
/// A scan is crude and it is also the only check that fails the moment someone deletes
/// one of these lines, because deleting any of them compiles, passes clippy, and
/// passes every other test in this workspace.
#[test]
fn the_headless_surfaces_wire_every_capability_the_tui_has() {
    let cmd = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
    let read = |name: &str| {
        std::fs::read_to_string(cmd.join(name)).unwrap_or_else(|_| panic!("{name} is readable"))
    };

    let turn = read("turn.rs");
    assert!(
        turn.contains("announce_skills(")
            && turn.contains("&mut plan.resolver,\n                &plan.skills,"),
        "`turn.rs` no longer injects the skill catalogue into the system prompt, so \
         discovery runs and the model is told about none of it"
    );
    assert!(
        turn.contains("skills: Arc::clone(&plan.skills)"),
        "`turn.rs` no longer hands the loaded skills to the tool assembly, so the \
         `skill` tool would answer from a different set than the prompt advertised"
    );
    assert!(
        turn.contains("delegation: super::tool_runtime::Delegation {"),
        "`turn.rs` no longer supplies a delegation host; `task` cannot be registered"
    );
    assert!(
        turn.contains("presets: plan.presets.clone()")
            && read("tool_runtime.rs").contains(".with_presets(presets)"),
        "the active team Preset no longer reaches the production `task` tool, so child \
         delegation would silently fall back to the parent session model"
    );

    for surface in ["run.rs", "serve.rs"] {
        let source = read(surface);
        assert!(
            source.contains("mcp_runtime::McpRuntime::from_config"),
            "`{surface}` no longer builds an MCP runtime, so the same configuration \
             that gives the TUI its MCP tools gives this surface none"
        );
        assert!(
            source.contains("mcp.shutdown()"),
            "`{surface}` no longer closes its MCP transports, leaving a remote \
             server's session open on the far side"
        );
    }
    assert!(
        read("run.rs").contains("TurnHost::open_with_mcp"),
        "`zuno run` must reach the constructor that takes a catalog"
    );
    assert!(
        read("serve.rs").contains("TurnHost::open_with_runtime_and_mcp"),
        "`zuno serve` must reach the constructor that takes a catalog"
    );
}

#[test]
fn every_extension_contribution_reaches_its_native_consumer() {
    let cmd = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
    let turn = std::fs::read_to_string(cmd.join("turn.rs")).expect("turn.rs is readable");
    let tui = std::fs::read_to_string(cmd.join("tui.rs")).expect("tui.rs is readable");

    for required in [
        "zuno_extension::discover_static",
        "zuno_extension::resolve_active",
        "zuno_extension::resolve_desired",
        "extensions.agents()",
        "with_overlay(extensions.skills().iter().cloned())",
        "zuno_extension::lifecycle_tools",
        "default_profile_with_tools",
        "orchestration_capabilities_bundle",
        "self.extensions.workflows()",
        "plan.command_registry(env, mcp.as_ref())",
        "plan.extensions.prompt_section()",
    ] {
        assert!(
            turn.contains(required),
            "`turn.rs` no longer wires extension capability `{required}` into a native consumer"
        );
    }

    assert!(
        tui.contains(".desired_revision(driver.host.extension_scope())")
            && tui.contains("driver.host.extension_revision()")
            && tui.contains(
                "next.extension_composition = super::turn::ExtensionComposition::Desired"
            )
            && tui.contains("driver.remount.request(RemountRequest::plain(next))"),
        "the long-lived TUI no longer prepares a desired extension composition after a lifecycle \
         change"
    );
    assert!(
        turn.contains(".begin_transition(&transaction)")
            && turn.contains(".acquire_active(&plan.extension_scope, plan.extension_revision)")
            && turn.contains("profile_runtime.activate_profile(profile).await"),
        "extension composition ownership is no longer coupled to profile startup"
    );
}

/// A placeholder title is not a name, so no surface should be handed one.
///
/// `session.title` is `NOT NULL` and `create` fills it with `New session - <instant>`, so
/// the raw column is never empty and a surface reading it directly would print that string
/// as though the user had chosen it — then replace it a second later with the generated
/// name. The filter is what makes "unnamed" expressible, and it reuses the generator's own
/// predicate so the two cannot disagree about which titles are real.
#[test]
fn turn_a_placeholder_session_title_reads_as_no_title_at_all() {
    for placeholder in [
        format!(
            "{}2026-08-07T00:00:00.000Z",
            zuno_db::session::PARENT_TITLE_PREFIX
        ),
        format!(
            "{}2026-08-07T00:00:00.000Z",
            zuno_db::session::CHILD_TITLE_PREFIX
        ),
        String::new(),
    ] {
        assert!(
            zuno_db::session::is_default_title(&placeholder),
            "`{placeholder}` must be recognised as a placeholder, or the sidebar will \
             display it as a chosen name"
        );
    }

    assert!(
        !zuno_db::session::is_default_title("Refactoring user service"),
        "a generated name was mistaken for a placeholder, so a named session would render \
         as unnamed"
    );
}

// ---------------------------------------------------------------------------
// Generation controls: the catalog's output limit and the agent's sampling
//
// # How these differ from the tests that let the effort defect through
//
// Those tests hand-wrote the body key they then asserted (`reasoning_effort`), so
// they proved `extraBody` is an identity function and said nothing about what a
// session emits — production spelled the key `reasoningEffort` and the two never
// met. Every test below writes only **user-facing configuration** — a provider
// block, a model's `limit.output`, an `agent` entry — and asserts on the **body a
// real provider builds**. No test here names an intermediate value, so none can be
// satisfied by a fixture that production never produces.
// ---------------------------------------------------------------------------

/// A resolved catalog model from a user-shaped config, plus the catalog itself.
///
/// The `limit.output` and `agent` blocks are the only inputs, because they are the
/// only things a user writes.
fn generation_catalog(
    model_id: &str,
    output_limit: Option<u64>,
    provider_options: serde_json::Value,
) -> Catalog {
    generation_catalog_with_temperature(model_id, output_limit, provider_options, true)
}

fn generation_catalog_with_temperature(
    model_id: &str,
    output_limit: Option<u64>,
    provider_options: serde_json::Value,
    temperature: bool,
) -> Catalog {
    let mut model = serde_json::Map::from_iter([
        ("id".to_owned(), serde_json::json!(model_id)),
        ("name".to_owned(), serde_json::json!("Generation fixture")),
        ("temperature".to_owned(), serde_json::json!(temperature)),
        // Declared for every fixture model, not just the reasoning ones: a model
        // whose catalog entry omits it resolves to no reasoning controls whatever
        // level is chosen, which would make a variant assertion pass for the wrong
        // reason.
        ("reasoning".to_owned(), serde_json::json!(true)),
        (
            "variants".to_owned(),
            serde_json::json!({
                "high": {"reasoningEffort": "high"},
                "low": {"reasoningEffort": "low"}
            }),
        ),
    ]);
    if let Some(output) = output_limit {
        model.insert(
            "limit".to_owned(),
            serde_json::json!({"context": 100_000, "output": output}),
        );
    }
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "stub": {
                "id": "stub",
                "name": "Generation fixture",
                "env": [],
                "transport": "openai-compatible",
                "options": provider_options,
                "models": { model_id: serde_json::Value::Object(model) },
            }
        }
    }))
    .expect("generation fixture config");
    Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    )
}

#[test]
fn declared_reasoning_variants_enable_selection_without_a_coarse_capability_flag() {
    let model_id = "us.anthropic.claude-opus-5";
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "myopenai": {
                "name": "My OpenAI",
                "transport": "openai-compatible",
                "options": {"baseURL": "https://gateway.example/v1"},
                "models": {
                    model_id: {
                        "name": "Claude Opus 5",
                        "variants": {
                            "low": {"reasoningEffort": "low"},
                            "medium": {"reasoningEffort": "medium"},
                            "high": {"reasoningEffort": "high"},
                            "xhigh": {"reasoningEffort": "xhigh"},
                            "max": {"reasoningEffort": "max"}
                        }
                    }
                }
            }
        }
    }))
    .expect("the user-shaped provider config parses");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let model = catalog
        .model("myopenai", model_id)
        .expect("the configured model resolves");

    assert!(
        !model.capabilities.reasoning,
        "the fixture must reproduce the omitted coarse capability"
    );
    assert_eq!(
        selectable_reasoning_efforts(model),
        vec![
            zuno_llm::effort::ReasoningEffort::Low,
            zuno_llm::effort::ReasoningEffort::Medium,
            zuno_llm::effort::ReasoningEffort::High,
            zuno_llm::effort::ReasoningEffort::Xhigh,
            zuno_llm::effort::ReasoningEffort::Max,
        ]
    );
    assert_eq!(
        session_reasoning_options(
            Some(zuno_llm::effort::ReasoningEffort::Xhigh),
            model,
            &serde_json::Map::new(),
        )
        .get("reasoningEffort"),
        Some(&serde_json::json!("xhigh")),
        "an explicitly declared level must reach the native provider request"
    );
}

/// One agent from user-shaped config, through the real deserializer and merge.
///
/// Deliberately not [`agent`]: that helper constructs the struct field by field,
/// which would let a test pass while the config schema dropped the key on the way
/// in. Going through `AgentConfig` means the JSON a user writes is the input.
fn configured_agent(definition: serde_json::Value) -> zuno_catalog::agent::Agent {
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "agents": { "tuned": definition }
    }))
    .expect("agent fixture config");
    zuno_catalog::agent::resolve(
        config.agent.as_ref().expect("the agent map deserializes"),
        &[],
    )
    .into_iter()
    .find(|entry| entry.name == "tuned")
    .expect("the configured agent resolves")
}

/// The Chat body a real provider sends for `model_id`, resolved as production does.
fn generation_body(
    catalog: &Catalog,
    model_id: &str,
    agent: &zuno_catalog::agent::Agent,
) -> serde_json::Value {
    let model = catalog
        .model("stub", model_id)
        .expect("the generation fixture model resolves");
    let spec = with_agent_options(
        model_spec(catalog, model, &Env::empty()).expect("the generation fixture spec resolves"),
        agent,
        model.capabilities.temperature,
    );
    let provider = zuno_provider_compatible::CompatibleProvider::new(
        spec,
        Arc::new(zuno_provider_compatible::ReqwestTransport::new(
            "generation",
        )),
        None,
    )
    .expect("the generation fixture provider builds");
    let mut request = zuno_llm::registry::CompletionRequest::new(
        model.api.id.clone(),
        vec![zuno_llm::event::Message {
            role: zuno_llm::event::Role::User,
            content: vec![zuno_llm::event::RequestContentBlock::Text {
                text: "Say hello.".to_owned(),
            }],
        }],
    )
    .with_tools(vec![zuno_llm::registry::ToolSchema {
        name: "read".to_owned(),
        description: "Read a file".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }]);
    request.parameters = session_reasoning_options(
        turn_effort(None, agent, "stub", model_id, None),
        model,
        &agent.options,
    );
    provider.body_for(&request)
}

#[test]
fn model_defaults_reach_a_responses_request_as_reasoning_effort_and_summary() {
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "myopenai": {
                "transport": "openai",
                "surface": "responses",
                "options": {"baseURL": "https://gateway.example/v1"},
                "models": {
                    "reasoner": {
                        "reasoning": true,
                        "options": {
                            "reasoningEffort": "max",
                            "reasoningSummary": "auto"
                        },
                        "variants": {
                            "max": {"reasoningEffort": "max"},
                            "low": {"reasoningEffort": "low"}
                        }
                    }
                }
            }
        }
    }))
    .expect("Responses provider config");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let model = catalog
        .model("myopenai", "reasoner")
        .expect("configured model");
    let agent = agent("build");
    let provider = zuno_provider_compatible::CompatibleProvider::new(
        model_spec(&catalog, model, &Env::empty()).expect("provider spec"),
        Arc::new(zuno_provider_compatible::ReqwestTransport::new(
            "responses-reasoning",
        )),
        None,
    )
    .expect("compatible provider");
    let mut request = zuno_llm::registry::CompletionRequest::new(
        model.api.id.clone(),
        vec![zuno_llm::event::Message::new(
            zuno_llm::event::Role::User,
            "Solve this carefully.",
        )],
    );
    request.parameters = session_reasoning_options(None, model, &agent.options);

    let body = provider.body_for(&request);
    assert_eq!(
        body["reasoning"],
        serde_json::json!({"effort": "max", "summary": "auto"})
    );
    assert!(
        body.get("input").is_some(),
        "Responses must use `input`: {body}"
    );
    assert!(
        body.get("messages").is_none(),
        "the configured Responses surface fell back to Chat Completions: {body}"
    );

    let selected = session_reasoning_options(
        Some(zuno_llm::effort::ReasoningEffort::Low),
        model,
        &agent.options,
    );
    assert_eq!(selected["reasoningEffort"], serde_json::json!("low"));
    assert_eq!(selected["reasoningSummary"], serde_json::json!("auto"));
}

#[test]
fn a_models_declared_output_limit_reaches_the_request_body() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "capped", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(16_384)),
        "the catalog's `limit.output` never reached the body, so every request runs on \
         the vendor's own default: measured against a capture stub, upstream sends \
         `max_tokens` where this build sent nothing"
    );
}

#[test]
fn an_output_limit_above_the_ceiling_is_clamped_rather_than_forwarded() {
    let catalog = generation_catalog(
        "huge",
        Some(1_000_000),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "huge", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(32_000)),
        "a catalog row claiming a million output tokens was forwarded verbatim; \
         `ProviderTransform.maxOutputTokens` clamps to 32_000"
    );
}

#[test]
fn a_model_declaring_no_output_limit_still_sends_a_cap() {
    let catalog = generation_catalog(
        "uncapped",
        None,
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "uncapped", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(32_000)),
        "an absent `limit.output` deserialises to 0, and `max_tokens: 0` asks the model \
         for an empty completion"
    );
}

#[test]
fn a_configured_output_limit_outranks_the_catalogs_own() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1",
            "maxTokens": 2_048
        }),
    );
    let body = generation_body(&catalog, "capped", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(2_048)),
        "the catalog default overwrote an explicitly configured cap, so a user lowering \
         `maxTokens` to control cost had no way to do it"
    );
}

#[test]
fn a_null_output_limit_suppresses_the_catalog_default_on_responses() {
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "stub": {
                "name": "Responses fixture",
                "transport": "openai",
                "surface": "responses",
                "options": {
                    "baseURL": "https://gateway.example/v1",
                    "maxTokens": null
                },
                "models": {
                    "mixed": {
                        "name": "Mixed model gateway",
                        "reasoning": true,
                        "tool_call": true,
                        "limit": {
                            "context": 272_000,
                            "output": 64_000
                        }
                    }
                }
            }
        }
    }))
    .expect("Responses provider config");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let body = generation_body(&catalog, "mixed", &agent("build"));

    assert!(
        body.get("max_output_tokens").is_none(),
        "`maxTokens: null` must suppress the generic output ceiling on Responses: {body}"
    );
    assert!(
        body.get("max_tokens").is_none(),
        "a Responses request must not fall back to the Chat output-limit field: {body}"
    );
}

#[test]
fn an_agents_sampling_declarations_reach_the_request_body() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let tuned = configured_agent(serde_json::json!({
        "model": "stub/capped",
        "temperature": 0.21,
        "top_p": 0.87
    }));
    let body = generation_body(&catalog, "capped", &tuned);

    assert_eq!(
        body.get("temperature"),
        Some(&serde_json::json!(0.21)),
        "`agents.tuned.temperature` was parsed, merged, and listed, and then the request \
         went out on the provider's default"
    );
    assert_eq!(
        body.get("top_p"),
        Some(&serde_json::json!(0.87)),
        "`top_p` is the config spelling and `topP` the option spelling; a request \
         missing the field means the rename was dropped rather than applied"
    );
}

#[test]
fn an_agent_temperature_is_omitted_when_the_model_rejects_it() {
    let catalog = generation_catalog_with_temperature(
        "no-temperature",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
        false,
    );
    let tuned = configured_agent(serde_json::json!({
        "model": "stub/no-temperature",
        "temperature": 0.21
    }));
    let body = generation_body(&catalog, "no-temperature", &tuned);

    assert_eq!(
        body.get("temperature"),
        None,
        "a native or configured agent temperature must not be sent to a model whose \
         catalog capability explicitly rejects it"
    );
}

#[test]
fn an_agents_option_bag_reaches_the_provider_and_can_override_the_cap() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let tuned = configured_agent(serde_json::json!({
        "model": "stub/capped",
        "options": {
            "maxTokens": 4_096,
            "toolChoice": "required",
            "extraBody": {"service_tier": "flex"}
        }
    }));
    let body = generation_body(&catalog, "capped", &tuned);

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(4_096)),
        "`agents.tuned.options` never reached the provider, so an agent could not raise \
         or lower the cap the catalog set"
    );
    assert_eq!(
        body.get("tool_choice"),
        Some(&serde_json::json!("required")),
        "a configured `toolChoice` was accepted and dropped, leaving the model free to \
         answer without calling the tool the agent required"
    );
    assert_eq!(
        body.get("service_tier"),
        Some(&serde_json::json!("flex")),
        "`extraBody` inside an agent's options is the documented channel for a \
         provider-specific body key, and it did not arrive"
    );
}

#[test]
fn no_tool_choice_is_sent_when_none_was_configured() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "capped", &agent("build"));

    assert_eq!(
        body.get("tool_choice"),
        None,
        "`auto` is what OpenAI documents as the default when tools are present, so \
         sending it unprompted changes the bytes without changing the behaviour — and \
         asks it of endpoints that reject a value they do not implement"
    );
}

#[test]
fn an_agents_variant_selects_the_models_declared_reasoning_options() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let reasoner = configured_agent(serde_json::json!({
        "model": "stub/reasoner",
        "variant": "high"
    }));
    let body = generation_body(&catalog, "reasoner", &reasoner);

    assert_eq!(
        body.get("reasoning_effort"),
        Some(&serde_json::json!("high")),
        "`agents.reasoner.variant` was accepted and never resolved, so an agent \
         configured to think hard reasoned at the provider's default"
    );
}

#[test]
fn a_variant_is_ignored_on_a_model_the_agent_did_not_declare() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let elsewhere = configured_agent(serde_json::json!({
        "model": "stub/other-model",
        "variant": "high"
    }));
    let body = generation_body(&catalog, "reasoner", &elsewhere);

    assert_eq!(
        body.get("reasoning_effort"),
        None,
        "a variant names a level the agent's OWN model declares; carried onto a model \
         switched to by hand it selects a level that name does not mean on this model"
    );
}

#[test]
fn a_session_chosen_effort_outranks_the_agents_variant() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let reasoner = configured_agent(serde_json::json!({
        "model": "stub/reasoner",
        "variant": "high"
    }));

    assert_eq!(
        turn_effort(
            Some(zuno_llm::effort::ReasoningEffort::Low),
            &reasoner,
            "stub",
            "reasoner",
            None,
        ),
        Some(zuno_llm::effort::ReasoningEffort::Low),
        "the effort picker is a live user action and the agent's variant a configured \
         default, so the picker must win"
    );
    let _ = catalog;
}

#[test]
fn explicit_cli_variant_selects_the_models_exact_declared_options() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let model = catalog
        .model("stub", "reasoner")
        .expect("fixture model resolves");
    let agent = configured_agent(serde_json::json!({
        "model": "stub/reasoner"
    }));

    let resolved = resolve_turn_reasoning(
        TurnReasoningSelection {
            session: None,
            explicit_variant: Some("high"),
            thinking: false,
        },
        &agent,
        "stub",
        "reasoner",
        None,
        model,
    )
    .expect("declared variant resolves");

    assert_eq!(
        resolved.effort,
        Some(zuno_llm::effort::ReasoningEffort::High)
    );
    assert_eq!(
        resolved.options.get("reasoningEffort"),
        Some(&serde_json::json!("high"))
    );
    assert_eq!(resolved.variant.as_deref(), Some("high"));
}

#[test]
fn explicit_named_cli_variant_preserves_the_complete_declared_option_object() {
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "stub": {
                "transport": "openai-compatible",
                "options": {"baseURL": "https://example.invalid/v1"},
                "models": {
                    "reasoner": {
                        "reasoning": true,
                        "variants": {
                            "deliberate": {
                                "reasoningEffort": "max",
                                "reasoningSummary": "detailed",
                                "vendorMode": "deep"
                            }
                        }
                    }
                }
            }
        }
    }))
    .expect("named variant config");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let model = catalog
        .model("stub", "reasoner")
        .expect("fixture model resolves");
    let agent = configured_agent(serde_json::json!({
        "model": "stub/reasoner"
    }));

    let resolved = resolve_turn_reasoning(
        TurnReasoningSelection {
            session: None,
            explicit_variant: Some("deliberate"),
            thinking: false,
        },
        &agent,
        "stub",
        "reasoner",
        None,
        model,
    )
    .expect("named variant resolves");

    assert_eq!(resolved.effort, None);
    assert_eq!(resolved.variant.as_deref(), Some("deliberate"));
    assert_eq!(
        resolved.options,
        serde_json::Map::from_iter([
            ("reasoningEffort".to_owned(), serde_json::json!("max")),
            ("reasoningSummary".to_owned(), serde_json::json!("detailed")),
            ("vendorMode".to_owned(), serde_json::json!("deep")),
        ])
    );
}

#[test]
fn named_only_variant_models_reject_canonical_cli_reasoning_controls() {
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "stub": {
                "transport": "openai-compatible",
                "options": {"baseURL": "https://example.invalid/v1"},
                "models": {
                    "reasoner": {
                        "reasoning": true,
                        "variants": {
                            "deliberate": {
                                "reasoningEffort": "max",
                                "vendorMode": "deep"
                            }
                        }
                    }
                }
            }
        }
    }))
    .expect("named-only variant config");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let model = catalog
        .model("stub", "reasoner")
        .expect("fixture model resolves");
    let agent = configured_agent(serde_json::json!({
        "model": "stub/reasoner"
    }));

    let variant_error = resolve_turn_reasoning(
        TurnReasoningSelection {
            session: None,
            explicit_variant: Some("max"),
            thinking: false,
        },
        &agent,
        "stub",
        "reasoner",
        None,
        model,
    )
    .expect_err("canonical variant must be explicitly declared");
    assert!(variant_error.contains("available variants: deliberate"));

    let thinking_error = resolve_turn_reasoning(
        TurnReasoningSelection {
            session: None,
            explicit_variant: None,
            thinking: true,
        },
        &agent,
        "stub",
        "reasoner",
        None,
        model,
    )
    .expect_err("--thinking must not synthesize a generic scale");
    assert!(thinking_error.contains("declares no enabled reasoning level"));
    assert!(selectable_reasoning_efforts(model).is_empty());
    assert!(
        session_reasoning_options(
            Some(zuno_llm::effort::ReasoningEffort::Max),
            model,
            &agent.options,
        )
        .is_empty()
    );
}

#[test]
fn cli_thinking_chooses_high_or_the_strongest_declared_non_off_level() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let model = catalog
        .model("stub", "reasoner")
        .expect("fixture model resolves");
    let agent = configured_agent(serde_json::json!({
        "model": "stub/reasoner"
    }));

    let resolved = resolve_turn_reasoning(
        TurnReasoningSelection {
            session: None,
            explicit_variant: None,
            thinking: true,
        },
        &agent,
        "stub",
        "reasoner",
        None,
        model,
    )
    .expect("thinking resolves");

    assert_eq!(
        resolved.effort,
        Some(zuno_llm::effort::ReasoningEffort::High)
    );
    assert_eq!(resolved.variant.as_deref(), Some("high"));
}

#[test]
fn an_unknown_explicit_variant_is_rejected_before_the_provider_request() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let model = catalog
        .model("stub", "reasoner")
        .expect("fixture model resolves");
    let agent = configured_agent(serde_json::json!({
        "model": "stub/reasoner"
    }));

    let error = resolve_turn_reasoning(
        TurnReasoningSelection {
            session: None,
            explicit_variant: Some("unavailable"),
            thinking: false,
        },
        &agent,
        "stub",
        "reasoner",
        None,
        model,
    )
    .expect_err("unknown variant must fail before HTTP");

    assert!(error.contains("unavailable"), "{error}");
    assert!(error.contains("high"), "{error}");
    assert!(error.contains("low"), "{error}");
}

#[test]
fn the_generation_controls_are_wired_into_the_turns_own_resolution() {
    let turn = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/turn.rs"),
    )
    .expect("turn.rs is readable");

    assert!(
        turn.contains("spec: with_agent_options(")
            && turn.contains("model_spec(&catalog, catalog_model, env)?")
            && turn.contains("catalog_model.capabilities.temperature"),
        "`TurnPlan::resolve` no longer overlays the agent's options onto the resolved \
         spec under the selected model's capabilities, so `temperature`, `top_p` and \
         `options` are parsed, listed, and dropped — the defect this pair of tests \
         exists to catch. A behavioural test alone cannot see it, because it calls \
         the helper the turn stopped calling."
    );
    assert!(
        turn.contains("let definition = agent.definition();")
            && turn.contains("let effort = turn_effort(")
            && turn.contains("routed_variant,"),
        "`TurnPlan::resolve` no longer carries the resolved profile's agent definition \
         into `turn_effort`, so an agent configured with a `variant` can run at the \
         provider's default"
    );
    assert!(
        turn.contains("generation::MAX_TOKENS, json!(output_ceiling(model))"),
        "`model_spec` no longer defaults the output cap from the catalog, so every \
         request runs uncapped"
    );
}

mod reflection_runtime {
    use super::*;
    use futures::stream;
    use std::sync::Mutex;
    use zuno_agent::reflection::{
        ReflectionMemoryEntry, ReflectionMemoryScope, ReflectionTurn, TranscriptEvent,
        TurnDelivery, TurnTranscript,
    };
    use zuno_llm::registry::{Capabilities, ProviderStream};

    const SESSION_ID: &str = "ses_reflection_runtime";

    #[derive(Debug)]
    struct ScriptedProvider {
        events: Vec<StreamEvent>,
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "reflection-provider"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tool_calls: true,
                ..Capabilities::text_only()
            }
        }

        fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
            self.requests
                .lock()
                .expect("reflection request lock")
                .push(request);
            Box::pin(stream::iter(self.events.clone().into_iter().map(Ok)))
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        memory: Arc<MemoryService>,
        events: zuno_db::event_log::SessionEventLog,
        fork: ReflectionFork,
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    fn fixture(provider_events: Vec<StreamEvent>) -> Fixture {
        let directory = tempfile::tempdir().expect("temporary memory directory");
        let pool =
            Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
        let mut connection = pool.open_connection().expect("database connection");
        zuno_db::migration::apply(&mut connection).expect("initialize schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                 VALUES ('project', '/workspace', 1, 1, '[]');
                 INSERT INTO session (
                     id, project_id, slug, directory, title, version, time_created, time_updated
                 ) VALUES (
                     'ses_reflection_runtime', 'project', 'reflection-runtime', '/workspace',
                     'Reflection runtime', 'zuno', 1, 1
                 );",
            )
            .expect("seed reflection session");
        drop(connection);

        let memory = Arc::new(MemoryService::new(
            Arc::clone(&pool),
            ScopePaths::at(
                directory.path().join("global/MEMORY.md"),
                directory.path().join("project/RULES.md"),
            ),
            ScopeLimits::default(),
            PromotionPolicy::Review,
        ));
        let events = zuno_db::event_log::SessionEventLog::new(Arc::clone(&pool));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runner = ProviderReflectionRunner {
            provider: Arc::new(ScriptedProvider {
                events: provider_events,
                requests: Arc::clone(&requests),
            }),
            model: EngineModel::new(
                Spec::new("reflection-provider"),
                "small-model",
                ApiSurface::Chat,
            ),
            events: zuno_db::event_log::SessionEventLog::new(pool),
        };
        let fork = ReflectionFork::new(
            Arc::new(runner),
            erase(zuno_tools::MemoryTool::reflection(Arc::clone(&memory))),
        );
        Fixture {
            _directory: directory,
            memory,
            events,
            fork,
            requests,
        }
    }

    async fn run(fixture: &Fixture) -> Result<(), ReflectionError> {
        fixture
            .fork
            .spawn_after_turn(
                ReflectionTurn::new(
                    TurnDelivery::new(true, false),
                    TurnTranscript::new(vec![
                        TranscriptEvent::user("remember the verified repository gate"),
                        TranscriptEvent::assistant("The gate passed and is reusable."),
                    ]),
                    ToolContext::new(
                        SESSION_ID,
                        "msg_delivered",
                        "call_reflection",
                        "build",
                        Arc::new(AllowAll),
                        Arc::new(NeverInterrupted),
                    ),
                )
                .with_resident_memory(vec![ReflectionMemoryEntry::new(
                    ReflectionMemoryScope::Project,
                    "Use cargo check before the workspace test suite.",
                )]),
            )
            .expect("reflection task")
            .await
            .expect("reflection supervisor task")
    }

    fn tool_events(name: &str, input: &str, with_end: bool) -> Vec<StreamEvent> {
        let mut events = vec![
            StreamEvent::ToolUseStart {
                id: "call_memory".to_owned(),
                name: name.to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call_memory".to_owned(),
                delta: input.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_memory".to_owned(),
            },
        ];
        if with_end {
            events.push(StreamEvent::MessageEnd {
                stop_reason: Some(zuno_llm::event::FinishReason::ToolCalls),
            });
        }
        events
    }

    #[tokio::test]
    async fn provider_reflection_persists_request_outcome_and_pending_candidate() {
        let fixture = fixture(tool_events(
            "memory_propose",
            r#"{"target":"project","action":"add","content":"run cargo test","reason":"verified repository gate","confidence":0.98}"#,
            true,
        ));

        run(&fixture).await.expect("reflection review");

        let candidates = fixture.memory.candidates().expect("memory candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].status,
            zuno_types::MemoryCandidateStatus::Pending
        );
        let events = fixture
            .events
            .read_after(SESSION_ID, None)
            .expect("reflection events");
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["memory.reflection.request", "memory.reflection.outcome"]
        );
        assert_eq!(events[0].properties["tool"]["name"], "memory_propose");
        assert_eq!(
            events[0].properties["residentMemory"][0]["scope"],
            "project"
        );
        assert_eq!(
            events[0].properties["residentMemory"][0]["content"],
            "Use cargo check before the workspace test suite."
        );
        assert!(
            events[0].properties["prompt"]
                .as_str()
                .is_some_and(|prompt| prompt.contains("prefer replace over add"))
        );
        assert_eq!(events[1].properties["status"], "completed");
        assert_eq!(events[1].properties["toolCalls"], 1);
        let requests = fixture.requests.lock().expect("reflection requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].request_context(),
            Some(&ProviderRequestContext::Reflection),
            "reflection must not join the foreground provider conversation"
        );
    }

    #[tokio::test]
    async fn malformed_denied_and_truncated_reflection_streams_are_durable_failures() {
        for (events, expected) in [
            (
                tool_events("memory_propose", "{", true),
                "malformed tool arguments",
            ),
            (
                tool_events("shell", r#"{"command":"echo forbidden"}"#, true),
                "denied non-whitelisted tool",
            ),
            (
                tool_events(
                    "memory_propose",
                    r#"{"target":"project","action":"add","content":"x","reason":"y","confidence":1}"#,
                    false,
                ),
                "ended before MessageEnd",
            ),
        ] {
            let fixture = fixture(events);
            let error = run(&fixture)
                .await
                .expect_err("invalid reflection stream must fail the durable job");
            assert!(
                error.to_string().contains(expected),
                "missing `{expected}` in {error}"
            );

            assert!(
                fixture.memory.candidates().expect("candidates").is_empty(),
                "failed reflection wrote a candidate"
            );
            let events = fixture
                .events
                .read_after(SESSION_ID, None)
                .expect("reflection events");
            let outcome = events.last().expect("failure outcome");
            assert_eq!(outcome.event_type, "memory.reflection.outcome");
            assert_eq!(outcome.properties["status"], "failed");
            assert!(
                outcome.properties["error"]
                    .as_str()
                    .is_some_and(|error| error.contains(expected)),
                "missing `{expected}` in {}",
                outcome.properties["error"]
            );
        }
    }

    #[test]
    fn durable_reflection_transcript_replays_delivered_text_and_terminal_tool_results() {
        let mut connection =
            zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open database");
        zuno_db::migration::apply(&mut connection).expect("initialize schema");
        let fixture_plan = plan("/workspace", SessionChoice::New);
        let now = 1_780_000_000_000;
        ensure_project(&connection, &fixture_plan.project, now).expect("persist project");
        let session = resolve_session(&mut connection, &fixture_plan, now).expect("create session");
        let (user, user_parts) = prepare_user_message(
            UserMessageInput {
                session_id: &session.id,
                agent: "build",
                provider_id: "provider",
                model_id: "model",
                text: "verify the gate",
                message_id: Some("msg_reflection_user"),
                now,
            },
            None,
        )
        .expect("prepare user message");
        persist_prepared_user_message(&connection, &user, &user_parts)
            .expect("persist user message");

        let assistant = zuno_db::message::MessageRecord::from_json(json!({
            "id": "msg_reflection_assistant",
            "sessionID": session.id,
            "role": "assistant",
            "time": {"created": now + 1, "completed": now + 2},
            "parentID": user.id,
            "modelID": "model",
            "providerID": "provider",
            "mode": "build",
            "agent": "build",
            "path": {"cwd": "/workspace", "root": "/workspace"},
            "cost": 0,
            "tokens": {
                "input": 1,
                "output": 1,
                "reasoning": 0,
                "cache": {"read": 0, "write": 0}
            },
            "finish": "stop"
        }))
        .expect("assistant message");
        let parts = [
            json!({
                "id": "prt_reflection_text",
                "sessionID": session.id,
                "messageID": assistant.id,
                "type": "text",
                "text": "I verified the gate."
            }),
            json!({
                "id": "prt_reflection_failed",
                "sessionID": session.id,
                "messageID": assistant.id,
                "type": "tool",
                "callID": "call_failed",
                "tool": "shell",
                "state": {
                    "status": "error",
                    "input": {"command": "cargo test"},
                    "error": "first attempt failed"
                }
            }),
            json!({
                "id": "prt_reflection_succeeded",
                "sessionID": session.id,
                "messageID": assistant.id,
                "type": "tool",
                "callID": "call_succeeded",
                "tool": "shell",
                "state": {
                    "status": "completed",
                    "input": {"command": "cargo test"},
                    "output": "all tests passed"
                }
            }),
        ]
        .into_iter()
        .enumerate()
        .map(|(offset, part)| {
            zuno_db::message::PartRecord::from_json(part, now + 2 + offset as i64)
                .expect("assistant part")
        })
        .collect::<Vec<_>>();
        let store = zuno_db::message::MessageStore::new(&connection);
        store
            .put_message_at(&assistant, now + 1)
            .expect("persist assistant");
        for part in &parts {
            store
                .put_part_at(part, part.time_created)
                .expect("persist assistant part");
        }

        let transcript =
            durable_reflection_transcript(&connection, &session.id, "msg_reflection_assistant")
                .expect("durable transcript");
        assert_eq!(
            transcript,
            TurnTranscript::new(vec![
                TranscriptEvent::user("verify the gate"),
                TranscriptEvent::assistant("I verified the gate."),
                TranscriptEvent::command(
                    "cargo test",
                    zuno_agent::reflection::CommandOutcome::failed("first attempt failed"),
                ),
                TranscriptEvent::command(
                    "cargo test",
                    zuno_agent::reflection::CommandOutcome::succeeded("all tests passed"),
                ),
            ])
        );
        assert!(
            durable_reflection_transcript(&connection, &session.id, "msg_missing")
                .expect_err("missing delivered message must fail")
                .contains("missing from durable history")
        );
    }
}
