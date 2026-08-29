use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zuno_db::Pool;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SubmissionState};
use zuno_db::message::{MessageRecord, MessageStore, PartKind, PartRecord};
use zuno_db::{Connection, migration, open};
use zuno_engine::hooks::TurnHooks;
use zuno_engine::interrupt::{InterruptSignal, SoftInterruptMessage, SoftInterruptSource};
use zuno_engine::r#loop::{
    AgentModelResolver, AvailableTools, DispatchRequest, PreparedToolDispatch, ResolvedAgent,
    ResolvedModel, RunTurnRequest, ToolDispatchResult, ToolDispatcher, TurnContext, TurnError,
    TurnEvent, TurnOutcome, TurnRecovery, event_channel, hydrate_retained_history,
    hydrate_retained_history_tail, project_history, project_history_owned, retained_history,
    run_turn,
};
use zuno_engine::prompt::{PromptAssembly, PromptAssemblyError, RuntimePromptPolicy};
use zuno_engine::status::{SessionControl, SessionRunRegistry};
use zuno_error::{ProviderError, ProviderProtocolFailure, ProviderStreamFailure};
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::event::{FinishReason, PromptAccounting, RequestContentBlock, Role, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry,
    ProviderRequestContext, ProviderStream, RequestPurpose, Spec, ToolSchema,
};
use zuno_orchestration::{
    AgentAttemptIdentity, AttemptSeed, AttemptSnapshot, CapabilityContents, CapabilitySnapshot,
    PackIdentity, sha256_text,
};
use zuno_tool::{ToolDefinition, ToolOutput, ToolUiIntent};

const SESSION_ID: &str = "ses_loop_test";

#[derive(Debug)]
struct ScriptedResponse {
    events: Vec<Result<StreamEvent, ProviderError>>,
    hang_after: bool,
}

impl ScriptedResponse {
    fn complete(events: Vec<StreamEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            hang_after: false,
        }
    }

    fn failed(events: Vec<StreamEvent>, error: ProviderError) -> Self {
        let mut events = events.into_iter().map(Ok).collect::<Vec<_>>();
        events.push(Err(error));
        Self {
            events,
            hang_after: false,
        }
    }

    fn hanging(events: Vec<StreamEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            hang_after: true,
        }
    }
}

#[derive(Debug)]
struct FakeProvider {
    responses: Mutex<VecDeque<ScriptedResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl FakeProvider {
    fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.lock().expect("request lock").push(request);
        let response = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .expect("one scripted response per provider request");
        let events = stream::iter(response.events);
        if response.hang_after {
            Box::pin(events.chain(stream::pending()))
        } else {
            Box::pin(events)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FakeResolver;

impl AgentModelResolver for FakeResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build")
            .then(|| ResolvedAgent::new("build", "You are a deterministic test agent."))
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "fake" && model_id == "fake-model")
            .then(|| ResolvedModel::new(Spec::new("fake"), "fake-model", ApiSurface::Default))
    }
}

#[derive(Debug, Clone, Copy)]
struct OversizedPromptResolver;

impl AgentModelResolver for OversizedPromptResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| {
            let mut prompt = PromptAssembly::new();
            prompt
                .push(
                    "instructions.global",
                    "/config/.config/zuno/AGENTS.md",
                    "A".repeat(64 * 1024),
                )
                .expect("large AGENTS section is structurally valid");
            prompt
                .push_selected_skill(
                    "selected-test-skill",
                    "/workspace/.zuno/skills/selected-test-skill/SKILL.md",
                    "S".repeat(1024),
                )
                .expect("selected Skill section is structurally valid");
            ResolvedAgent::new("build", "").with_prompt_assembly(prompt)
        })
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        FakeResolver.resolve_model(provider_id, model_id)
    }
}

#[derive(Debug, Clone, Copy)]
struct LimitedResolver {
    max_steps: NonZeroU32,
}

impl LimitedResolver {
    fn new(max_steps: u32) -> Self {
        Self {
            max_steps: NonZeroU32::new(max_steps).expect("test step limit is non-zero"),
        }
    }
}

impl AgentModelResolver for LimitedResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| {
            ResolvedAgent::new("build", "You are a deterministic test agent.")
                .with_max_steps(self.max_steps)
        })
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        FakeResolver.resolve_model(provider_id, model_id)
    }
}

#[derive(Debug, Clone, Copy)]
struct TraceResolver;

fn trace_seed() -> Arc<AttemptSeed> {
    Arc::new(AttemptSeed {
        capability: CapabilitySnapshot::new(
            PackIdentity {
                id: "test-orchestration".to_owned(),
                version: "1".to_owned(),
                upstream_revision: "test@trace".to_owned(),
            },
            7,
            sha256_text("trace permission policy"),
            CapabilityContents::default(),
        ),
        agent: AgentAttemptIdentity {
            name: "build".to_owned(),
            source_id: "native:build".to_owned(),
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

impl AgentModelResolver for TraceResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| {
            let mut assembly = PromptAssembly::new();
            assembly
                .push("agent.base", "native:build", "BASE")
                .expect("base section");
            assembly
                .push("instructions.project.0", "/workspace/AGENTS.md", "RULES")
                .expect("instruction section");
            ResolvedAgent::new("build", assembly.render())
                .with_prompt_assembly(assembly)
                .with_orchestration_seed(trace_seed())
        })
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "fake" && model_id == "fake-model")
            .then(|| ResolvedModel::new(Spec::new("fake"), "fake-model", ApiSurface::Default))
    }
}

#[derive(Debug, Clone, Copy)]
struct PolicyResolver;

impl AgentModelResolver for PolicyResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| {
            ResolvedAgent::new("build", "BUILD").with_runtime_prompt_policy(
                RuntimePromptPolicy::new(
                    Some(vec!["explorer".to_owned()]),
                    Some("Delegate only bounded read-only exploration.".to_owned()),
                    true,
                ),
            )
        })
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "fake" && model_id == "fake-model")
            .then(|| ResolvedModel::new(Spec::new("fake"), "fake-model", ApiSurface::Default))
    }
}

#[derive(Debug)]
struct AppendingSystemHook;

#[async_trait]
impl TurnHooks for AppendingSystemHook {
    async fn transform_system(
        &self,
        _session_id: &str,
        _model: &ResolvedModel,
        system: &mut Vec<String>,
    ) -> Result<(), String> {
        system.push("HOOK".to_owned());
        Ok(())
    }
}

#[derive(Debug)]
struct ExpandingToolsHook;

#[async_trait]
impl TurnHooks for ExpandingToolsHook {
    async fn prepare_request(
        &self,
        _input: zuno_engine::hooks::RequestHookInput<'_>,
        request: &mut CompletionRequest,
    ) -> Result<(), String> {
        request.tools.push(ToolSchema {
            name: "surprise".to_owned(),
            description: "A tool absent from the locked registry snapshot.".to_owned(),
            parameters: json!({"type": "object"}),
        });
        Ok(())
    }
}

#[derive(Debug)]
struct PlanOnlyRequestHook;

#[async_trait]
impl TurnHooks for PlanOnlyRequestHook {
    async fn prepare_request(
        &self,
        _input: zuno_engine::hooks::RequestHookInput<'_>,
        request: &mut CompletionRequest,
    ) -> Result<(), String> {
        request.tools.retain(|tool| tool.name == "plan_update");
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FakeDispatcher {
    calls: Mutex<Vec<DispatchRequest>>,
}

#[derive(Debug)]
struct SnapshotDispatcher {
    definitions: Vec<ToolDefinition>,
}

#[async_trait]
impl ToolDispatcher for SnapshotDispatcher {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(self.definitions.clone(), McpToolStatus::Ready)
    }

    async fn prepare(&self, _request: DispatchRequest) -> PreparedToolDispatch {
        panic!("snapshot-only test must not dispatch a tool")
    }
}

impl FakeDispatcher {
    fn calls(&self) -> Vec<DispatchRequest> {
        self.calls.lock().expect("dispatch lock").clone()
    }
}

#[async_trait]
impl ToolDispatcher for FakeDispatcher {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(
            vec![ToolDefinition {
                id: "echo".to_owned(),
                display_name: "echo-runtime".to_owned(),
                description: "Echo text.".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
                ui_intent: zuno_tool::ToolUiIntent::Generic,
            }],
            McpToolStatus::Ready,
        )
    }

    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        self.calls
            .lock()
            .expect("dispatch lock")
            .push(request.clone());
        let text = request.call.input["text"]
            .as_str()
            .unwrap_or("missing text");
        PreparedToolDispatch::ready(ToolDispatchResult::success(ToolOutput::text("echo", text)))
    }
}

fn seeded() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-loop', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);"
        ))
        .expect("seed project and session");
    connection
}

fn seeded_shared_pool_with_goal_schema() -> Arc<Pool> {
    let pool =
        Arc::new(Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared memory database"));
    let mut connection = pool
        .open_connection()
        .expect("open shared schema connection");
    migration::apply(&mut connection).expect("apply shared schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-loop', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);"
        ))
        .expect("seed shared project and session");
    connection
        .execute_batch(
            "CREATE TABLE goal (
                session_id TEXT PRIMARY KEY NOT NULL,
                goal_id TEXT NOT NULL,
                revision INTEGER NOT NULL CHECK(revision >= 1),
                objective TEXT NOT NULL,
                success_criteria TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN (
                    'active',
                    'paused',
                    'blocked',
                    'usage_limited',
                    'budget_limited',
                    'complete',
                    'cancelled'
                )),
                blocked_reason TEXT,
                token_budget INTEGER,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                usage_known INTEGER NOT NULL DEFAULT 1 CHECK(usage_known IN (0, 1)),
                time_used_seconds INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
        )
        .expect("install the canonical Goal table");
    drop(connection);
    pool
}

fn seed_durable_compaction_state(pool: Arc<Pool>) {
    let connection = pool
        .open_connection()
        .expect("open durable state connection");
    connection
        .execute_batch(
            r#"
            INSERT INTO goal (
                session_id, goal_id, revision, objective, success_criteria, status,
                blocked_reason, token_budget, tokens_used, usage_known,
                time_used_seconds, created_at_ms, updated_at_ms
            ) VALUES (
                'ses_loop_test', 'goal_compaction', 3,
                'preserve durable state across compaction',
                '["goal remains","report recovers once"]', 'active',
                NULL, 100000, 321, 1, 9, 100, 101
            );
            INSERT INTO work_plan (
                session_id, id, goal_id, revision, title, steps, time_created, time_updated
            ) VALUES (
                'ses_loop_test', 'plan_compaction', 'goal_compaction', 2,
                'Durable compaction plan',
                '[{"id":"inspect","title":"Inspect durable state","status":"in_progress"}]',
                102, 103
            );
            INSERT INTO work_item (
                id, session_id, goal_id, plan_step_id, parent_id, subject, description,
                active_form, status, priority, dependencies, owner, revision,
                tokens_used, usage_known, time_used_ms, time_created, time_updated
            ) VALUES (
                'todo_compaction', 'ses_loop_test', 'goal_compaction', 'inspect', NULL,
                'Inspect durable state', 'Verify Goal, Plan, Job, report, and receipt',
                'Inspecting durable state', 'in_progress', 'high', '[]', 'build', 4,
                55, 1, 700, 104, 105
            );
            "#,
        )
        .expect("seed Goal, Plan, and WorkItem");
    drop(connection);

    let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
    jobs.create(zuno_db::job::NewAgentJob::new(
        "job_compaction",
        SESSION_ID,
        zuno_db::job::JobSubject::child_session("ses_child_compaction"),
        zuno_db::job::ReportDelivery::NextStep,
        106,
    ))
    .expect("create durable child Job");
    jobs.settle(
        "job_compaction",
        zuno_db::job::JobSettlement::completed(
            json!({
                "finalText": "child completed durable inspection",
                "goalID": "goal_compaction",
                "planID": "plan_compaction",
                "workItemID": "todo_compaction"
            }),
            107,
            Some(NewSessionInput::new(
                "input_compaction_report",
                SESSION_ID,
                json!({
                    "kind": "subagentReport",
                    "jobID": "job_compaction",
                    "text": "durable report",
                    "references": {
                        "goalID": "goal_compaction",
                        "planID": "plan_compaction",
                        "workItemID": "todo_compaction"
                    }
                }),
                InputDelivery::Queue,
                107,
            )),
        ),
    )
    .expect("settle Job with one next-step report");
}

#[derive(Debug, Clone, PartialEq)]
struct DurableCompactionSnapshot {
    goal: Value,
    plan: Value,
    work_items: Vec<Value>,
    job: zuno_db::job::AgentJob,
    report: zuno_db::inbox::SessionInput,
    prompt_receipts: Vec<(String, String)>,
}

fn json_row(connection: &Connection, sql: &str) -> Value {
    let encoded: String = connection
        .query_row(sql, [], |row| row.get(0))
        .expect("read durable JSON row");
    serde_json::from_str(&encoded).expect("decode durable JSON row")
}

fn json_rows(connection: &Connection, sql: &str) -> Vec<Value> {
    let mut statement = connection.prepare(sql).expect("prepare durable JSON rows");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query durable JSON rows")
        .map(|row| {
            let encoded = row.expect("read durable JSON row");
            serde_json::from_str(&encoded).expect("decode durable JSON row")
        })
        .collect()
}

fn prompt_receipts(connection: &Connection) -> Vec<(String, String)> {
    let mut statement = connection
        .prepare(
            "SELECT id, data FROM event \
             WHERE aggregate_id = ?1 AND type = 'session.prompt.assembled.1' \
             ORDER BY seq",
        )
        .expect("prepare Prompt receipt query");
    statement
        .query_map([SESSION_ID], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query Prompt receipts")
        .map(|row| row.expect("read Prompt receipt"))
        .collect()
}

fn latest_prompt_receipt_id(connection: &Connection) -> String {
    prompt_receipts(connection)
        .last()
        .expect("at least one Prompt receipt")
        .0
        .clone()
}

fn durable_compaction_snapshot(
    connection: &Connection,
    pool: Arc<Pool>,
) -> DurableCompactionSnapshot {
    let goal = json_row(
        connection,
        "SELECT json_object(
            'sessionID', session_id,
            'goalID', goal_id,
            'revision', revision,
            'objective', objective,
            'successCriteria', json(success_criteria),
            'status', status,
            'blockedReason', blocked_reason,
            'tokenBudget', token_budget,
            'tokensUsed', tokens_used,
            'usageKnown', usage_known,
            'timeUsedSeconds', time_used_seconds,
            'createdAtMs', created_at_ms,
            'updatedAtMs', updated_at_ms
         ) FROM goal WHERE session_id = 'ses_loop_test'",
    );
    let plan = json_row(
        connection,
        "SELECT json_object(
            'sessionID', session_id,
            'id', id,
            'goalID', goal_id,
            'revision', revision,
            'title', title,
            'steps', json(steps),
            'timeCreated', time_created,
            'timeUpdated', time_updated
         ) FROM work_plan WHERE session_id = 'ses_loop_test'",
    );
    let work_items = json_rows(
        connection,
        "SELECT json_object(
            'id', id,
            'sessionID', session_id,
            'goalID', goal_id,
            'planStepID', plan_step_id,
            'parentID', parent_id,
            'subject', subject,
            'description', description,
            'activeForm', active_form,
            'status', status,
            'priority', priority,
            'dependencies', json(dependencies),
            'owner', owner,
            'revision', revision,
            'tokensUsed', tokens_used,
            'usageKnown', usage_known,
            'timeUsedMs', time_used_ms,
            'timeCreated', time_created,
            'timeUpdated', time_updated
         ) FROM work_item WHERE session_id = 'ses_loop_test' ORDER BY id",
    );
    let job = zuno_db::job::AgentJobStore::new(Arc::clone(&pool))
        .get("job_compaction")
        .expect("read durable Job");
    let report = SessionInbox::new(pool)
        .get(SESSION_ID, "input_compaction_report")
        .expect("read durable report")
        .expect("durable report exists");
    DurableCompactionSnapshot {
        goal,
        plan,
        work_items,
        job,
        report,
        prompt_receipts: prompt_receipts(connection),
    }
}

fn durable_compaction_context(
    connection: &Connection,
    pool: Arc<Pool>,
    original_receipt_id: &str,
) -> String {
    let snapshot = durable_compaction_snapshot(connection, pool);
    format!(
        "Durable execution references recovered from SQLite after compaction:\n\
         - Goal: {}\n\
         - Plan: {}\n\
         - Todo/WorkItem: {}\n\
         - Job: {}\n\
         - Unconsumed report: {}\n\
         - Prior Prompt receipt: {original_receipt_id}",
        snapshot.goal["goalID"].as_str().expect("Goal reference"),
        snapshot.plan["id"].as_str().expect("Plan reference"),
        snapshot.work_items[0]["id"]
            .as_str()
            .expect("WorkItem reference"),
        snapshot.job.id,
        snapshot.report.id,
    )
}

fn scalar_count(connection: &Connection, sql: &str) -> i64 {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("read scalar count")
}

fn put_user(connection: &Connection, id: &str, created: i64, text: &str) {
    let message = MessageRecord::from_json(json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "build",
        "model": { "providerID": "fake", "modelID": "fake-model" }
    }))
    .expect("valid user message");
    let part = PartRecord::from_json(
        json!({
            "id": format!("prt_{id}"),
            "sessionID": SESSION_ID,
            "messageID": id,
            "type": "text",
            "text": text
        }),
        created,
    )
    .expect("valid user part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist user message");
    store
        .put_part_at(&part, created)
        .expect("persist user part");
}

fn put_pending_tool(
    connection: &Connection,
    message_id: &str,
    part_id: &str,
    created: i64,
    call_id: &str,
) {
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1 },
        "parentID": "msg_before_repair",
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.0,
        "tokens": {
            "input": 0,
            "output": 0,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "tool-calls"
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "tool",
            "callID": call_id,
            "tool": "echo",
            "state": {
                "status": "pending",
                "input": { "text": "orphaned" },
                "raw": "{\"text\":\"orphaned\"}"
            }
        }),
        created,
    )
    .expect("valid pending tool part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist pending tool part");
}

fn put_assistant_text(
    connection: &Connection,
    id: &str,
    created: i64,
    parent_id: &str,
    text: &str,
) {
    let message = MessageRecord::from_json(json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1 },
        "parentID": parent_id,
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "stop"
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        json!({
            "id": format!("prt_{id}"),
            "sessionID": SESSION_ID,
            "messageID": id,
            "type": "text",
            "text": text
        }),
        created,
    )
    .expect("valid assistant text");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist assistant text");
}

fn put_completed_tool_output(
    connection: &Connection,
    message_id: &str,
    part_id: &str,
    created: i64,
    output: &str,
) {
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1 },
        "parentID": "msg_old_user",
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "tool-calls"
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "tool",
            "callID": "call-large",
            "tool": "read",
            "state": {
                "status": "completed",
                "input": { "filePath": "/large.txt" },
                "output": output
            }
        }),
        created,
    )
    .expect("valid completed tool part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist completed tool part");
}

fn put_successful_compaction(
    connection: &Connection,
    marker_id: &str,
    summary_id: &str,
    tail_start_id: &str,
    created: i64,
) {
    let marker = MessageRecord::from_json(json!({
        "id": marker_id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "compaction",
        "model": { "providerID": "fake", "modelID": "fake-model" }
    }))
    .expect("valid compaction marker");
    let marker_part = PartRecord::from_json(
        json!({
            "id": format!("prt_{marker_id}"),
            "sessionID": SESSION_ID,
            "messageID": marker_id,
            "type": "compaction",
            "auto": true,
            "overflow": true,
            "tail_start_id": tail_start_id
        }),
        created,
    )
    .expect("valid compaction part");
    let summary = MessageRecord::from_json(json!({
        "id": summary_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "parentID": marker_id,
        "time": { "created": created + 1, "completed": created + 2 },
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "compaction",
        "agent": "compaction",
        "summary": true,
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "stop"
    }))
    .expect("valid compaction summary");
    let summary_part = PartRecord::from_json(
        json!({
            "id": format!("prt_{summary_id}"),
            "sessionID": SESSION_ID,
            "messageID": summary_id,
            "type": "text",
            "text": "Earlier work was summarized without changing the retained tail."
        }),
        created + 1,
    )
    .expect("valid summary text");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&marker, created)
        .expect("persist compaction marker");
    store
        .put_part_at(&marker_part, created)
        .expect("persist compaction part");
    store
        .put_message_at(&summary, created + 2)
        .expect("persist compaction summary");
    store
        .put_part_at(&summary_part, created + 2)
        .expect("persist summary text");
}

fn put_incomplete_compaction(
    connection: &Connection,
    marker_id: &str,
    summary_id: &str,
    tail_start_id: &str,
    created: i64,
    summary_text: Option<&str>,
    failed: bool,
) {
    let marker = MessageRecord::from_json(json!({
        "id": marker_id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "compaction",
        "model": { "providerID": "fake", "modelID": "fake-model" }
    }))
    .expect("valid compaction marker");
    let marker_part = PartRecord::from_json(
        json!({
            "id": format!("prt_{marker_id}"),
            "sessionID": SESSION_ID,
            "messageID": marker_id,
            "type": "compaction",
            "auto": true,
            "overflow": true,
            "tail_start_id": tail_start_id
        }),
        created,
    )
    .expect("valid compaction part");
    let mut summary_data = json!({
        "id": summary_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "parentID": marker_id,
        "time": { "created": created + 1, "completed": created + 2 },
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "compaction",
        "agent": "compaction",
        "summary": true,
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": if failed { "error" } else { "stop" }
    });
    if failed {
        summary_data["error"] = json!({
            "name": "CompactionError",
            "data": { "message": "summary failed", "isRetryable": false }
        });
    }
    let summary = MessageRecord::from_json(summary_data).expect("valid compaction summary");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&marker, created)
        .expect("persist compaction marker");
    store
        .put_part_at(&marker_part, created)
        .expect("persist compaction part");
    store
        .put_message_at(&summary, created + 2)
        .expect("persist compaction summary");
    if let Some(text) = summary_text {
        let part = PartRecord::from_json(
            json!({
                "id": format!("prt_{summary_id}"),
                "sessionID": SESSION_ID,
                "messageID": summary_id,
                "type": "text",
                "text": text
            }),
            created + 1,
        )
        .expect("valid summary text");
        store
            .put_part_at(&part, created + 2)
            .expect("persist summary text");
    }
}

fn assert_loader_projection_matches_full_reference(connection: &Connection) {
    const SYSTEM: &str = "You are a deterministic test agent.";
    let full = MessageStore::new(connection)
        .hydrate_session(SESSION_ID)
        .expect("full reference history");
    let expected = project_history(SYSTEM, retained_history(&full))
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let optimized = hydrate_retained_history(connection, SESSION_ID).expect("optimized history");
    let actual = project_history_owned(SYSTEM, optimized);

    assert_eq!(
        serde_json::to_vec(&actual).expect("serialize optimized projection"),
        serde_json::to_vec(&expected).expect("serialize reference projection"),
        "optimized loader changed provider-visible history bytes"
    );
}

fn assert_dropping_first_history_message_changes_projection(connection: &Connection) {
    const SYSTEM: &str = "You are a deterministic test agent.";
    let full = MessageStore::new(connection)
        .hydrate_session(SESSION_ID)
        .expect("full reference history");
    assert!(
        full.len() > 1,
        "the sensitivity fixture needs a non-empty history prefix"
    );
    let complete = project_history(SYSTEM, &full)
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let without_first = project_history(SYSTEM, &full[1..])
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();

    assert_ne!(
        serde_json::to_vec(&without_first).expect("serialize truncated projection"),
        serde_json::to_vec(&complete).expect("serialize full projection"),
        "fixture is vacuous: dropping its first history message did not change provider-visible bytes"
    );
}

fn registry(provider: &Arc<FakeProvider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    let provider = Arc::clone(provider);
    registry.register("fake", move |_spec| provider.clone());
    registry
}

fn request(turn_id: &str) -> RunTurnRequest {
    RunTurnRequest::new(SESSION_ID, turn_id, DynamicContext::default())
}

async fn collect_events(mut receiver: mpsc::Receiver<TurnEvent>) -> Vec<TurnEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

async fn run_single_text_turn(
    connection: &mut Connection,
    turn_id: &str,
    dynamic_context: DynamicContext,
    response: &str,
) -> Arc<FakeProvider> {
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta(response.to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, turn_id, dynamic_context),
        TurnContext::new(connection, &providers, &resolver, &dispatcher, &interrupt),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));
    provider
}

fn full_turn_responses() -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("I will use echo.".to_owned()),
            StreamEvent::ToolUseStart {
                id: "call-1".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call-1".to_owned(),
                delta: r#"{"text":"hello"}"#.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call-1".to_owned(),
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("echo returned hello".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]
}

fn echo_tool_response(call: usize) -> ScriptedResponse {
    let call_id = format!("call-{call}");
    ScriptedResponse::complete(vec![
        StreamEvent::ToolUseStart {
            id: call_id.clone(),
            name: "echo".to_owned(),
        },
        StreamEvent::ToolInputDelta {
            id: call_id.clone(),
            delta: format!(r#"{{"text":"round-{call}"}}"#),
        },
        StreamEvent::ToolUseEnd { id: call_id },
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ])
}

#[tokio::test]
async fn an_unconfigured_agent_has_no_implicit_step_limit() {
    let mut connection = seeded();
    put_user(
        &connection,
        "msg_unbounded",
        10,
        "keep using the tool until done",
    );
    let mut responses = (1..=101).map(echo_tool_response).collect::<Vec<_>>();
    responses.push(ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("finished after one hundred and one tool rounds".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ]));
    let provider = Arc::new(FakeProvider::new(responses));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-unbounded"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 102, .. })
    ));
    assert_eq!(provider.requests().len(), 102);
    assert_eq!(dispatcher.calls().len(), 101);
}

#[tokio::test]
async fn an_explicit_step_limit_adds_one_tool_free_text_finalization() {
    let mut connection = seeded();
    put_user(
        &connection,
        "msg_limited",
        10,
        "use one tool then summarize",
    );
    let provider = Arc::new(FakeProvider::new(vec![
        echo_tool_response(1),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("completed the tool call; nothing remains".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = LimitedResolver::new(1);
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-limited"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 2, .. })
    ));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert!(
        requests[1].tools.is_empty(),
        "the configured limit must close tool authority before finalization"
    );
    assert!(
        requests[1]
            .developer_context
            .iter()
            .any(|section| section.contains("user-configured tool-step limit")),
        "the final request must explain why only a text response is allowed"
    );

    let finalization: String = connection
        .query_row(
            "SELECT data FROM event \
             WHERE aggregate_id = ?1 \
               AND type = 'session.provider.request.1' \
               AND json_extract(data, '$.step') = 2",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("finalization provider request event");
    let finalization: Value = serde_json::from_str(&finalization).expect("finalization event JSON");
    assert_eq!(
        finalization["stepLimitFinalization"]["maxToolSteps"],
        json!(1)
    );
    assert_eq!(
        finalization["stepLimitFinalization"]["instructionSha256"],
        json!(sha256_text(
            finalization["stepLimitFinalization"]["instruction"]
                .as_str()
                .expect("instruction text")
        ))
    );
}

async fn run_full_turn_once() -> (Vec<TurnEvent>, Vec<CompletionRequest>, Vec<DispatchRequest>) {
    let mut connection = seeded();
    put_user(&connection, "msg_user", 10, "echo hello");
    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-full"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert_eq!(
        outcome.expect("full turn succeeds"),
        TurnOutcome::Completed {
            assistant_message_id: "msg_turn-full_0002".to_owned(),
            steps: 2,
            unresolved_tool_failures: Vec::new(),
        }
    );

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate completed turn");
    let assistants: Vec<_> = hydrated
        .iter()
        .filter(|message| message.info.role.as_str() == "assistant")
        .collect();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].parts.len(), 2);
    assert_eq!(assistants[1].parts.len(), 1);

    (events, provider.requests(), dispatcher.calls())
}

#[tokio::test]
async fn one_durable_session_identity_spans_every_tool_continuation() {
    let (_events, requests, _calls) = run_full_turn_once().await;

    assert_eq!(
        requests.len(),
        2,
        "the fixture must exercise a tool continuation"
    );
    for request in &requests {
        let context = request
            .request_context()
            .expect("foreground turns carry typed provider routing context");
        assert_eq!(context.purpose(), RequestPurpose::MainTurn);
        assert_eq!(
            context
                .session_identity()
                .expect("main turn has affinity")
                .as_str(),
            SESSION_ID
        );
    }
    assert_eq!(
        requests[0].request_context(),
        requests[1].request_context(),
        "tool continuation must keep the exact durable session affinity"
    );
}

#[tokio::test]
async fn a_child_turn_uses_its_own_session_instead_of_the_parent_affinity() {
    let mut connection = seeded();
    connection
        .execute(
            "UPDATE session SET parent_id = ?1 WHERE id = ?2",
            ["ses_parent", SESSION_ID],
        )
        .expect("mark the fixture as a child session");
    put_user(&connection, "msg_child_affinity", 10, "answer as child");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("child answer".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-child-affinity"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    outcome.expect("child turn succeeds");

    let requests = provider.requests();
    let context = requests[0]
        .request_context()
        .expect("child turn carries typed context");
    assert_eq!(
        context,
        &ProviderRequestContext::ChildTurn(
            zuno_llm::registry::ProviderSessionIdentity::parse(SESSION_ID)
                .expect("valid child session id")
        )
    );
    assert_ne!(
        context.session_identity().expect("child affinity").as_str(),
        "ses_parent",
        "a child request must not join the parent provider conversation"
    );
}

#[tokio::test]
async fn loop_persists_ordered_prompt_provenance_and_the_post_hook_prompt() {
    let mut connection = seeded();
    put_user(&connection, "msg_prompt_trace", 10, "trace the prompt");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("done".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = TraceResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-prompt-trace"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(Arc::new(AppendingSystemHook)),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));

    let (prompt_event_id, stored_type, data): (String, String, String) = connection
        .query_row(
            "SELECT id, type, data FROM event \
             WHERE aggregate_id = ?1 AND type = 'session.prompt.assembled.1'",
            [SESSION_ID],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("prompt trace event");
    assert_eq!(stored_type, "session.prompt.assembled.1");
    let trace: Value = serde_json::from_str(&data).expect("prompt trace JSON");
    assert_eq!(trace["agent"], "build");
    assert_eq!(trace["step"], 1);
    assert_eq!(trace["turnId"], "turn-prompt-trace");
    assert_eq!(trace["hookTransformed"], true);
    assert_eq!(trace["actualSystemPrompt"], "BASE\n\nRULES\n\nHOOK");
    assert_eq!(
        trace["providerProjection"]["developer"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        trace["actualProviderProjection"]["developer"],
        trace["providerProjection"]["developer"]
    );
    assert_eq!(
        trace["sections"]
            .as_array()
            .expect("section array")
            .iter()
            .map(|section| section["id"].as_str().expect("section id"))
            .collect::<Vec<_>>(),
        vec![
            "agent.base",
            "runtime.intent",
            "runtime.execution",
            "runtime.verification",
            "instructions.project.0",
        ]
    );
    assert_eq!(trace["sections"][0]["source"], "native:build");
    assert_eq!(
        trace["sections"][1]["source"],
        "zuno-runtime:runtime.intent"
    );
    assert_eq!(trace["sections"][4]["source"], "/workspace/AGENTS.md");
    let seed = trace_seed();
    assert_eq!(
        trace["capabilitySnapshot"],
        serde_json::to_value(&seed.capability).expect("capability snapshot JSON")
    );
    assert_eq!(
        trace["capabilitySnapshotID"],
        serde_json::to_value(seed.capability.identity().expect("capability identity"))
            .expect("capability identity JSON")
    );

    let mut statement = connection
        .prepare(
            "SELECT data FROM event \
             WHERE aggregate_id = ?1 AND type = 'session.provider.request.1' ORDER BY seq",
        )
        .expect("prepare provider request events");
    let lifecycle = statement
        .query_map([SESSION_ID], |row| row.get::<_, String>(0))
        .expect("query provider request events")
        .map(|row| {
            serde_json::from_str::<Value>(&row.expect("provider request event"))
                .expect("provider request JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle.len(), 2);
    assert_eq!(lifecycle[0]["status"], "started");
    assert_eq!(lifecycle[1]["status"], "completed");
    assert_eq!(lifecycle[0]["requestID"], lifecycle[1]["requestID"]);
    assert_eq!(lifecycle[0]["turnID"], "turn-prompt-trace");
    assert_eq!(lifecycle[0]["promptReceiptID"], prompt_event_id);
    assert_eq!(lifecycle[0]["requestPurpose"], "main-turn");
    assert_eq!(lifecycle[0]["affinityAttached"], true);
    assert_eq!(lifecycle[0]["affinitySource"], "durable-session");
    assert!(lifecycle[0]["estimatedPromptTokens"].as_u64().is_some());
    let snapshot: AttemptSnapshot =
        serde_json::from_value(lifecycle[0]["orchestrationSnapshot"].clone())
            .expect("provider request orchestration snapshot");
    assert_eq!(snapshot.capability, seed.capability);
    assert_eq!(snapshot.agent, seed.agent);
    assert_eq!(
        snapshot.prompt.event_id.as_deref(),
        Some(prompt_event_id.as_str())
    );
    assert_eq!(snapshot.prompt.actual_sha256, trace["actualSha256"]);
    assert_eq!(snapshot.prompt.assembly_sha256, trace["assemblySha256"]);
    assert_eq!(
        lifecycle[0]["orchestrationSnapshotID"],
        serde_json::to_value(snapshot.identity().expect("attempt identity"))
            .expect("attempt identity JSON")
    );

    let usage = zuno_db::session::get(&connection, SESSION_ID)
        .expect("session usage")
        .usage
        .snapshot();
    assert!(usage.estimated_pending_prompt_tokens.is_some());
    assert!(usage.confirmed.is_empty());

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        &requests[0].messages[..3],
        &[
            zuno_llm::event::Message::new(Role::System, "BASE"),
            zuno_llm::event::Message::new(Role::System, "RULES"),
            zuno_llm::event::Message::new(Role::System, "HOOK"),
        ],
        "kernel, developer rules, and hook output must keep separate provider messages"
    );
}

#[tokio::test]
async fn loop_rejects_an_oversized_complete_prompt_before_calling_the_provider() {
    let mut connection = seeded();
    put_user(
        &connection,
        "msg_oversized_prompt",
        10,
        "answer without trimming any instructions",
    );
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("provider must not be called".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = OversizedPromptResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-oversized-prompt").with_context_limit(8 * 1024),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    let error = outcome.expect_err("the complete prompt must exceed the 8K context window");

    assert!(matches!(
        error,
        TurnError::PromptAssembly(PromptAssemblyError::ContextLimitExceeded {
            estimated_prompt_tokens,
            context_limit: 8192,
        }) if estimated_prompt_tokens > 8192
    ));
    assert_eq!(error.recovery(), TurnRecovery::Fail);
    assert!(
        provider.requests().is_empty(),
        "an impossible prompt entered the provider retry path"
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, TurnEvent::ProviderRequestStarted { .. })),
        "an impossible prompt emitted a provider request start event"
    );
}

#[tokio::test]
async fn runtime_policy_is_rendered_from_the_post_hook_tool_subset() {
    let mut connection = seeded();
    put_user(
        &connection,
        "msg_runtime_policy_snapshot",
        10,
        "use the effective capabilities",
    );
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("done".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = PolicyResolver;
    let definition = |id: &str| ToolDefinition {
        id: id.to_owned(),
        display_name: id.to_owned(),
        description: format!("{id} test tool"),
        parameters: json!({"type": "object"}),
        ui_intent: ToolUiIntent::Generic,
    };
    let dispatcher = SnapshotDispatcher {
        definitions: vec![
            definition("apply_patch"),
            definition("task"),
            definition("plan_update"),
        ],
    };
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-runtime-policy-snapshot"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(Arc::new(PlanOnlyRequestHook)),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));

    let requests = provider.requests();
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["plan_update"]
    );
    let runtime = requests[0].developer_context.join("\n");
    assert!(runtime.contains("Use a durable Plan whenever it improves progress visibility"));
    assert!(runtime.contains("Todo items are optional concrete work beneath Plan steps"));
    assert!(runtime.contains("Durable Goal, Plan, Todo"));
    assert!(!runtime.contains("explorer"));
    assert!(!runtime.contains("editing surface"));
    assert!(!runtime.contains("Delegate only"));

    let trace: String = connection
        .query_row(
            "SELECT data FROM event \
             WHERE aggregate_id = ?1 AND type = 'session.prompt.assembled.1'",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("prompt receipt");
    let trace: Value = serde_json::from_str(&trace).expect("prompt receipt JSON");
    assert_eq!(
        trace["sections"]
            .as_array()
            .expect("sections")
            .iter()
            .filter_map(|section| section["id"].as_str())
            .filter(|id| id.starts_with("runtime."))
            .collect::<Vec<_>>(),
        [
            "runtime.intent",
            "runtime.execution",
            "runtime.verification",
            "runtime.persistence",
        ]
    );
}

#[tokio::test]
async fn prepare_request_hooks_cannot_expand_the_locked_tool_schema_set() {
    let mut connection = seeded();
    put_user(&connection, "msg_hook_tools", 10, "test hook authority");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("should not run".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-hook-tools"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(Arc::new(ExpandingToolsHook)),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    let error = outcome.expect_err("a request hook must not expand locked tools");
    let TurnError::Hook(detail) = error else {
        panic!("expected Hook error, got {error}");
    };
    assert!(detail.contains("prepare_request"), "{detail}");
    assert!(detail.contains("surprise"), "{detail}");
    assert!(
        provider.requests().is_empty(),
        "the provider must not receive an authority-expanding request"
    );
}

#[tokio::test]
async fn loop_routes_dynamic_goal_and_memory_outside_user_history() {
    let mut connection = seeded();
    put_user(&connection, "msg_dynamic_user", 10, "continue the goal");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("done".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(
            SESSION_ID,
            "turn-dynamic-context",
            DynamicContext::new("ACTIVE GOAL").with_memory("RESIDENT MEMORY"),
        ),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    outcome.expect("dynamic-context turn succeeds");

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        &requests[0].developer_context[requests[0].developer_context.len() - 2..],
        ["ACTIVE GOAL", "RESIDENT MEMORY"]
    );
    assert!(
        requests[0]
            .developer_context
            .iter()
            .any(|context| context.contains("Durable Goal, Plan, Todo"))
    );
    let dynamic_text_leaked = requests[0].messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                RequestContentBlock::Text { text }
                    if text == "ACTIVE GOAL" || text == "RESIDENT MEMORY"
            )
        })
    });
    assert!(
        !dynamic_text_leaked,
        "dynamic policy leaked into replayable history"
    );
}

#[tokio::test]
async fn loop_injects_a_durable_background_report_at_the_tool_safe_point() {
    let pool = Arc::new(
        Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared in-memory loop pool"),
    );
    {
        let mut connection = pool.get().expect("seed connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(&format!(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-loop', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);"
            ))
            .expect("seed project and session");
    }
    let mut connection = pool.get().expect("turn connection");
    put_user(&connection, "msg_user", 10, "echo hello");
    let inbox = SessionInbox::new(Arc::clone(&pool));
    inbox
        .admit(NewSessionInput::new(
            "msg_steer",
            SESSION_ID,
            json!({
                "kind": "subagentReport",
                "jobID": "job_report",
                "childSessionID": "ses_child",
                "status": "completed",
                "text": "include benchmark",
                "metadata": {
                    "schemaVersion": 1,
                    "agent": "explorer",
                    "finalText": "include benchmark"
                }
            }),
            InputDelivery::Steer,
            11,
        ))
        .expect("admit steer");
    let run_registry = SessionRunRegistry::new();
    let guard = run_registry.begin_turn(SESSION_ID).expect("live turn");
    run_registry
        .queue_soft_interrupt(
            SESSION_ID,
            SoftInterruptMessage {
                input_id: Some("msg_steer".to_owned()),
                content: "include benchmark".to_owned(),
                images: Vec::new(),
                urgent: false,
                source: SoftInterruptSource::BackgroundTask,
            },
        )
        .expect("queue steer");

    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("turn-steer"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, &inbox),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome.expect("steered turn succeeds"),
        TurnOutcome::Completed { steps: 2, .. }
    ));
    assert!(inbox.pending(SESSION_ID).expect("pending inbox").is_empty());
    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate steered turn");
    let steer = hydrated
        .iter()
        .find(|message| message.info.id == "msg_steer")
        .expect("steer became a logged user message");
    assert_eq!(steer.parts[0].data["text"], "include benchmark");
    assert_eq!(steer.info.data["taskReport"]["agent"], "explorer");
    assert_eq!(
        steer.info.data["taskReport"]["finalText"],
        "include benchmark"
    );
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn live_input_persistence_failure_rolls_back_without_losing_the_promoted_input() {
    let pool = Arc::new(
        Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared in-memory loop pool"),
    );
    {
        let mut connection = pool.get().expect("seed connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(&format!(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-loop', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);
                 CREATE TRIGGER reject_live_input_part \
                   BEFORE INSERT ON part \
                   WHEN NEW.message_id = 'msg_atomic_steer' \
                 BEGIN \
                   SELECT RAISE(ABORT, 'injected live-input persistence failure'); \
                 END;"
            ))
            .expect("seed project, session, and failure trigger");
    }
    let mut connection = pool.get().expect("turn connection");
    put_user(&connection, "msg_user", 10, "echo hello");
    let inbox = SessionInbox::new(Arc::clone(&pool));
    inbox
        .admit(NewSessionInput::new(
            "msg_atomic_steer",
            SESSION_ID,
            json!({"kind": "user", "prompt": {"text": "recover this input"}}),
            InputDelivery::Steer,
            11,
        ))
        .expect("admit steer");
    let run_registry = SessionRunRegistry::new();
    let guard = run_registry.begin_turn(SESSION_ID).expect("live turn");
    run_registry
        .queue_soft_interrupt(
            SESSION_ID,
            SoftInterruptMessage {
                input_id: Some("msg_atomic_steer".to_owned()),
                content: "recover this input".to_owned(),
                images: Vec::new(),
                urgent: false,
                source: SoftInterruptSource::User,
            },
        )
        .expect("queue steer");

    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("turn-atomic-steer"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, &inbox),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    outcome.expect_err("the injected persistence failure must stop the turn");
    let stored = inbox
        .get(SESSION_ID, "msg_atomic_steer")
        .expect("read inbox row")
        .expect("promoted input remains durable");
    assert_eq!(stored.state, SubmissionState::Promoted);
    assert_eq!(stored.error, None);
    assert!(
        MessageStore::new(&connection)
            .message("msg_atomic_steer")
            .is_err(),
        "the user message insert must roll back with the failed part"
    );
}

async fn collect_and_steer_hanging_provider(
    mut receiver: mpsc::Receiver<TurnEvent>,
    inbox: SessionInbox,
    control: SessionControl,
) -> Vec<TurnEvent> {
    let mut events = Vec::new();
    let mut steered = false;
    while let Some(event) = receiver.recv().await {
        if !steered
            && matches!(
                &event,
                TurnEvent::Provider {
                    event: StreamEvent::TextDelta(text),
                    ..
                } if text == "partial before steer"
            )
        {
            inbox
                .admit(NewSessionInput::new(
                    "msg_live_steer",
                    SESSION_ID,
                    json!({"kind": "user", "prompt": {"text": "change direction now"}}),
                    InputDelivery::Steer,
                    11,
                ))
                .expect("admit live steer");
            control
                .queue_soft_interrupt(SoftInterruptMessage {
                    input_id: Some("msg_live_steer".to_owned()),
                    content: "change direction now".to_owned(),
                    images: Vec::new(),
                    urgent: false,
                    source: SoftInterruptSource::User,
                })
                .expect("wake the active turn");
            steered = true;
        }
        events.push(event);
    }
    assert!(steered, "the provider never produced the steering trigger");
    events
}

#[tokio::test]
async fn loop_live_steer_wakes_a_hanging_provider_and_restarts_with_the_new_input() {
    let pool = Arc::new(
        Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared in-memory loop pool"),
    );
    {
        let mut connection = pool.get().expect("seed connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(&format!(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-loop', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);"
            ))
            .expect("seed project and session");
    }
    let mut connection = pool.get().expect("turn connection");
    put_user(&connection, "msg_user", 10, "start the long answer");
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let run_registry = SessionRunRegistry::new();
    let control = run_registry.control(SESSION_ID);
    let guard = run_registry.begin_turn(SESSION_ID).expect("live turn");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::hanging(vec![StreamEvent::TextDelta(
            "partial before steer".to_owned(),
        )]),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("answer after steer".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-live-steer"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, &inbox),
        sender,
    );
    let collector = collect_and_steer_hanging_provider(receiver, inbox.clone(), control);
    let (outcome, events) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(turn, collector)
    })
    .await
    .expect("a live steer must wake the hanging provider");

    assert!(matches!(
        outcome.expect("steered turn succeeds"),
        TurnOutcome::Completed { steps: 2, .. }
    ));
    assert!(matches!(
        events.last(),
        Some(TurnEvent::TurnCompleted { steps: 2, .. })
    ));
    assert!(inbox.pending(SESSION_ID).expect("pending inbox").is_empty());
    assert_eq!(provider.requests().len(), 2);

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate live-steered turn");
    let steer = hydrated
        .iter()
        .find(|message| message.info.id == "msg_live_steer")
        .expect("live steer became a logged user message");
    assert_eq!(steer.parts[0].data["text"], "change direction now");
    let partial = hydrated
        .iter()
        .find(|message| message.info.id == "msg_turn-live-steer_0001")
        .expect("partial assistant was checkpointed before steering");
    assert_eq!(partial.info.data["finish"], "steer");
    assert!(
        partial.info.data.get("error").is_none(),
        "steering is not a hard turn abort"
    );
}

async fn collect_and_interrupt_retry_backoff(
    mut receiver: mpsc::Receiver<TurnEvent>,
    interrupt: InterruptSignal,
) -> (Vec<TurnEvent>, Duration) {
    let mut events = Vec::new();
    let mut fired_at = None;
    while let Some(event) = receiver.recv().await {
        if fired_at.is_none()
            && matches!(
                &event,
                TurnEvent::Provider {
                    event: StreamEvent::RetryRollback { .. },
                    ..
                }
            )
        {
            fired_at = Some(Instant::now());
            interrupt.fire();
        }
        events.push(event);
    }
    let elapsed = fired_at
        .expect("the retry rollback fires the interrupt")
        .elapsed();
    (events, elapsed)
}

#[tokio::test]
async fn loop_hard_interrupt_wakes_provider_retry_backoff_without_replaying() {
    let mut connection = seeded();
    put_user(&connection, "msg_retry_interrupt", 10, "retry slowly");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::failed(
            Vec::new(),
            ProviderError::Transient {
                status: Some(503),
                source: None,
            },
        ),
        ScriptedResponse::hanging(Vec::new()),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-retry-interrupt"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let collector = collect_and_interrupt_retry_backoff(receiver, interrupt.clone());
    let (outcome, (events, elapsed)) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(turn, collector)
    })
    .await
    .expect("hard interrupt must wake provider retry backoff");

    assert!(matches!(
        outcome.expect("interrupt is a normal turn outcome"),
        TurnOutcome::Interrupted { steps: 1, .. }
    ));
    assert!(
        elapsed < Duration::from_millis(100),
        "retry backoff ignored hard cancellation for {elapsed:?}"
    );
    assert!(matches!(
        events.last(),
        Some(TurnEvent::TurnInterrupted { steps: 1, .. })
    ));
    assert_eq!(
        provider.requests().len(),
        1,
        "cancellation started another provider attempt before stopping"
    );
}

async fn collect_and_steer_retry_backoff(
    mut receiver: mpsc::Receiver<TurnEvent>,
    inbox: SessionInbox,
    control: SessionControl,
) -> (Vec<TurnEvent>, Duration) {
    let mut events = Vec::new();
    let mut fired_at = None;
    while let Some(event) = receiver.recv().await {
        if fired_at.is_none()
            && matches!(
                &event,
                TurnEvent::Provider {
                    event: StreamEvent::RetryRollback { .. },
                    ..
                }
            )
        {
            inbox
                .admit(NewSessionInput::new(
                    "msg_retry_steer",
                    SESSION_ID,
                    json!({"kind": "user", "prompt": {"text": "do this instead"}}),
                    InputDelivery::Steer,
                    11,
                ))
                .expect("admit retry steer");
            fired_at = Some(Instant::now());
            control
                .queue_soft_interrupt(SoftInterruptMessage {
                    input_id: Some("msg_retry_steer".to_owned()),
                    content: "do this instead".to_owned(),
                    images: Vec::new(),
                    urgent: false,
                    source: SoftInterruptSource::User,
                })
                .expect("wake retry backoff");
        }
        events.push(event);
    }
    let elapsed = fired_at
        .expect("the retry rollback queues the steer")
        .elapsed();
    (events, elapsed)
}

#[tokio::test]
async fn loop_live_steer_wakes_provider_retry_backoff_without_replaying_stale_input() {
    let pool = Arc::new(
        Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared in-memory loop pool"),
    );
    {
        let mut connection = pool.get().expect("seed connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(&format!(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-loop', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);"
            ))
            .expect("seed project and session");
    }
    let mut connection = pool.get().expect("turn connection");
    put_user(&connection, "msg_user", 10, "use the stale direction");
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let run_registry = SessionRunRegistry::new();
    let control = run_registry.control(SESSION_ID);
    let guard = run_registry.begin_turn(SESSION_ID).expect("live turn");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::failed(
            Vec::new(),
            ProviderError::Transient {
                status: Some(503),
                source: None,
            },
        ),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("new direction complete".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-retry-steer"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, &inbox),
        sender,
    );
    let collector = collect_and_steer_retry_backoff(receiver, inbox.clone(), control);
    let (outcome, (_events, elapsed)) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(turn, collector)
    })
    .await
    .expect("live steer must wake provider retry backoff");

    assert!(matches!(
        outcome.expect("steered turn succeeds"),
        TurnOutcome::Completed { steps: 2, .. }
    ));
    assert!(
        elapsed < Duration::from_millis(100),
        "retry backoff ignored live steering for {elapsed:?}"
    );
    assert!(inbox.pending(SESSION_ID).expect("pending inbox").is_empty());
    assert_eq!(
        provider.requests().len(),
        2,
        "the stale provider request was replayed before the steered step"
    );
    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate retry-steered turn");
    assert!(hydrated.iter().any(|message| {
        message.info.id == "msg_retry_steer" && message.parts[0].data["text"] == "do this instead"
    }));
}

fn without_prompt_estimates(events: &[TurnEvent]) -> Vec<TurnEvent> {
    events
        .iter()
        .cloned()
        .map(|event| match event {
            TurnEvent::ProviderRequestStarted {
                step,
                message_count,
                ..
            } => TurnEvent::ProviderRequestStarted {
                step,
                message_count,
                estimated_prompt_tokens: 0,
            },
            event => event,
        })
        .collect()
}

fn expected_full_turn_events() -> Vec<TurnEvent> {
    vec![
        TurnEvent::TurnStarted {
            session_id: SESSION_ID.to_owned(),
        },
        TurnEvent::AgentResolved {
            step: 1,
            agent: "build".to_owned(),
        },
        TurnEvent::ModelResolved {
            step: 1,
            provider_id: "fake".to_owned(),
            model_id: "fake-model".to_owned(),
        },
        TurnEvent::AssistantMessageCreated {
            step: 1,
            message_id: "msg_turn-full_0001".to_owned(),
        },
        TurnEvent::ToolSnapshotLocked {
            step: 1,
            tool_ids: vec!["echo".to_owned()],
            rebuilt_for_late_mcp: false,
        },
        TurnEvent::ProviderRequestStarted {
            step: 1,
            message_count: 5,
            estimated_prompt_tokens: 0,
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::TextDelta("I will use echo.".to_owned()),
        },
        TurnEvent::ToolCallStarted {
            step: 1,
            call_id: "call-1".to_owned(),
            display_name: "echo-runtime".to_owned(),
            name: "echo".to_owned(),
            ui_intent: ToolUiIntent::Generic,
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolUseStart {
                id: "call-1".to_owned(),
                name: "echo".to_owned(),
            },
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolInputDelta {
                id: "call-1".to_owned(),
                delta: r#"{"text":"hello"}"#.to_owned(),
            },
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolUseEnd {
                id: "call-1".to_owned(),
            },
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        },
        TurnEvent::AssistantCheckpointed {
            step: 1,
            message_id: "msg_turn-full_0001".to_owned(),
            interrupted: false,
        },
        TurnEvent::ToolDispatchStarted {
            step: 1,
            call_id: "call-1".to_owned(),
            display_name: "echo-runtime".to_owned(),
            name: "echo".to_owned(),
            ui_intent: ToolUiIntent::Generic,
        },
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: "call-1".to_owned(),
            display_name: "echo-runtime".to_owned(),
            name: "echo".to_owned(),
            title: "echo".to_owned(),
            output: "hello".to_owned(),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        },
        TurnEvent::ToolResultAppended {
            step: 1,
            call_id: "call-1".to_owned(),
            is_error: false,
        },
        TurnEvent::StepCompleted {
            step: 1,
            finish_reason: Some(FinishReason::ToolCalls),
        },
        TurnEvent::AgentResolved {
            step: 2,
            agent: "build".to_owned(),
        },
        TurnEvent::ModelResolved {
            step: 2,
            provider_id: "fake".to_owned(),
            model_id: "fake-model".to_owned(),
        },
        TurnEvent::AssistantMessageCreated {
            step: 2,
            message_id: "msg_turn-full_0002".to_owned(),
        },
        TurnEvent::ToolSnapshotLocked {
            step: 2,
            tool_ids: vec!["echo".to_owned()],
            rebuilt_for_late_mcp: false,
        },
        TurnEvent::ProviderRequestStarted {
            step: 2,
            message_count: 7,
            estimated_prompt_tokens: 0,
        },
        TurnEvent::Provider {
            step: 2,
            event: StreamEvent::TextDelta("echo returned hello".to_owned()),
        },
        TurnEvent::Provider {
            step: 2,
            event: StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        },
        TurnEvent::AssistantCheckpointed {
            step: 2,
            message_id: "msg_turn-full_0002".to_owned(),
            interrupted: false,
        },
        TurnEvent::StepCompleted {
            step: 2,
            finish_reason: Some(FinishReason::Stop),
        },
        TurnEvent::TurnCompleted {
            assistant_message_id: "msg_turn-full_0002".to_owned(),
            steps: 2,
        },
    ]
}

#[tokio::test]
async fn loop_full_turn_emits_the_exact_sequence_deterministically() {
    let expected = expected_full_turn_events();
    let mut rendered_runs = Vec::new();

    for run_index in 0..3 {
        let (events, requests, calls) = run_full_turn_once().await;
        assert_eq!(
            without_prompt_estimates(&events),
            expected,
            "event sequence changed"
        );
        let estimates = events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ProviderRequestStarted {
                    estimated_prompt_tokens,
                    ..
                } => Some(*estimated_prompt_tokens),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(estimates.len(), 2);
        assert!(estimates.iter().all(|estimate| *estimate > 0));
        assert_eq!(requests.len(), 2);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call.name, "echo");
        assert_eq!(calls[0].call.input, json!({ "text": "hello" }));

        let second = &requests[1];
        let blocks: Vec<&RequestContentBlock> = second
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            RequestContentBlock::ToolUse { id, .. } if id == "call-1"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            RequestContentBlock::ToolResult { tool_use_id, content, is_error }
                if tool_use_id == "call-1" && content == "hello" && *is_error == Some(false)
        )));

        if run_index == 0 {
            eprintln!(
                "HAPPY_QA event_count={} transcript={events:#?}",
                events.len()
            );
        }
        rendered_runs.push(format!("{events:#?}").into_bytes());
    }

    assert!(
        rendered_runs.windows(2).all(|pair| pair[0] == pair[1]),
        "the three event transcripts must be byte-identical"
    );
}

#[tokio::test]
async fn loop_accepts_interleaved_parallel_tool_streams_and_dispatches_in_model_order() {
    let mut connection = seeded();
    put_user(&connection, "msg_parallel_user", 10, "run two tools");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::complete(vec![
            StreamEvent::ToolUseStart {
                id: "call-a".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolUseStart {
                id: "call-b".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call-b".to_owned(),
                delta: r#"{"text":"second"}"#.to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call-a".to_owned(),
                delta: r#"{"text":"first"}"#.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call-b".to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call-a".to_owned(),
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("done".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-parallel"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 2, .. })
    ));
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].call.id, "call-a");
    assert_eq!(calls[0].call.input, json!({ "text": "first" }));
    assert_eq!(calls[1].call.id, "call-b");
    assert_eq!(calls[1].call.input, json!({ "text": "second" }));
    let first_snapshot = calls[0]
        .orchestration_snapshot
        .as_ref()
        .expect("first tool orchestration snapshot");
    let second_snapshot = calls[1]
        .orchestration_snapshot
        .as_ref()
        .expect("second tool orchestration snapshot");
    assert!(
        Arc::ptr_eq(first_snapshot, second_snapshot),
        "all calls admitted by one provider Attempt must share one immutable snapshot"
    );
    assert_eq!(first_snapshot.turn_id, "turn-parallel");
    assert_eq!(first_snapshot.step, 1);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ToolDispatchStarted { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["call-a", "call-b"]
    );
}

#[tokio::test]
async fn loop_never_replays_malformed_tool_arguments_as_a_non_object() {
    let mut connection = seeded();
    put_user(
        &connection,
        "msg_malformed_tool_user",
        10,
        "call the tool with malformed arguments",
    );
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::complete(vec![
            StreamEvent::ToolUseStart {
                id: "call-malformed".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call-malformed".to_owned(),
                delta: r#"{"text":"unfinished""#.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call-malformed".to_owned(),
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("recovered".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-malformed-tool"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 2, .. })
    ));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let replayed_inputs = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            RequestContentBlock::ToolUse { input, .. } => Some(input),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !replayed_inputs.is_empty(),
        "the tool exchange was not replayed"
    );
    assert!(
        replayed_inputs.iter().all(|input| input.is_object()),
        "provider-bound tool arguments must always be JSON objects: {replayed_inputs:#?}"
    );

    let assistant = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate malformed tool checkpoint")
        .into_iter()
        .find(|message| message.info.id == "msg_turn-malformed-tool_0001")
        .expect("first assistant checkpoint");
    let tool = assistant
        .parts
        .iter()
        .find(|part| part.kind == PartKind::Tool)
        .expect("malformed tool checkpoint");
    assert!(tool.data["state"]["input"].is_object());
    assert_eq!(
        tool.data["state"]["raw"],
        serde_json::Value::String(r#"{"text":"unfinished""#.to_owned())
    );
}

#[tokio::test]
async fn loop_rejects_a_completed_assistant_message_with_zero_parts() {
    let mut connection = seeded();
    put_user(
        &connection,
        "msg_empty_user",
        10,
        "do not accept an empty answer",
    );
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-empty-assistant"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    let error = outcome.expect_err("a zero-part assistant message must not complete the turn");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("fake"), "{diagnostic}");
    assert!(diagnostic.contains("empty"), "{diagnostic}");
    assert!(matches!(
        error,
        TurnError::EmptyAssistantMessage {
            provider_id,
            step: 1,
        } if provider_id == "fake"
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. }))
    );

    let assistant = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate rejected empty turn")
        .into_iter()
        .find(|message| message.info.id == "msg_turn-empty-assistant_0001")
        .expect("empty assistant checkpoint remains inspectable");
    assert!(assistant.parts.is_empty());
}

#[tokio::test]
async fn loop_accepts_a_tool_only_assistant_step_as_non_empty() {
    let mut connection = seeded();
    put_user(&connection, "msg_tool_only_user", 10, "use the echo tool");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::complete(vec![
            StreamEvent::ToolUseStart {
                id: "call-tool-only".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call-tool-only".to_owned(),
                delta: r#"{"text":"hello"}"#.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call-tool-only".to_owned(),
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("done".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-tool-only"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 2, .. })
    ));
    let assistants = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate tool-only turn")
        .into_iter()
        .filter(|message| message.info.role.as_str() == "assistant")
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].parts.len(), 1, "tool part counts as output");
    assert_eq!(assistants[0].parts[0].kind, PartKind::Tool);
}

#[tokio::test]
async fn loop_provider_retry_replaces_typed_partial_stream_and_persists_attempts() {
    let mut connection = seeded();
    put_user(&connection, "msg_retry_user", 10, "retry once");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::failed(
            vec![
                StreamEvent::TextDelta("discarded attempt".to_owned()),
                StreamEvent::ToolUseStart {
                    id: "discarded-call".to_owned(),
                    name: "echo".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "discarded-call".to_owned(),
                    delta: r#"{"text":"must not run"}"#.to_owned(),
                },
            ],
            ProviderError::Stream {
                code: ProviderStreamFailure::UpstreamStreamIncomplete,
                source: None,
            },
        ),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("completed replay".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-retry-503"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].request_context(),
        requests[1].request_context(),
        "replacement attempts must retain the durable session affinity"
    );
    assert!(
        dispatcher.calls().is_empty(),
        "a tool from the discarded attempt must never be dispatched"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
        }
    )));

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate retried turn");
    let assistant = hydrated
        .iter()
        .find(|message| message.info.id == "msg_turn-retry-503_0001")
        .expect("retried assistant was persisted");
    let text = assistant
        .parts
        .iter()
        .find(|part| part.kind == PartKind::Text)
        .expect("successful replay text was persisted");
    assert_eq!(text.data["text"], "completed replay");

    let mut statement = connection
        .prepare(
            "SELECT data FROM event \
             WHERE aggregate_id = ?1 AND type = 'session.provider.attempt.1' ORDER BY seq",
        )
        .expect("prepare provider attempt events");
    let attempts = statement
        .query_map([SESSION_ID], |row| row.get::<_, String>(0))
        .expect("query provider attempt events")
        .map(|row| {
            serde_json::from_str::<Value>(&row.expect("provider attempt event"))
                .expect("provider attempt JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 4);
    assert_eq!(attempts[0]["status"], "started");
    assert_eq!(attempts[0]["attempt"], 1);
    assert_eq!(attempts[1]["status"], "failed");
    assert_eq!(attempts[1]["attempt"], 1);
    assert_eq!(attempts[1]["partialOutput"], true);
    assert_eq!(
        attempts[1]["providerErrorCode"],
        "upstream_stream_incomplete"
    );
    assert_eq!(attempts[1]["retryable"], true);
    assert_eq!(attempts[1]["partialOutputRetryPermitted"], true);
    assert_eq!(attempts[2]["status"], "started");
    assert_eq!(attempts[2]["attempt"], 2);
    assert_eq!(attempts[3]["status"], "completed");
    assert_eq!(attempts[3]["attempt"], 2);
    assert_eq!(attempts[0]["attemptID"], attempts[1]["attemptID"]);
    assert_eq!(attempts[2]["attemptID"], attempts[3]["attemptID"]);
    assert_ne!(attempts[0]["attemptID"], attempts[2]["attemptID"]);
}

#[tokio::test]
async fn loop_provider_retry_is_bounded_and_surfaces_the_final_transient_failure() {
    let mut connection = seeded();
    put_user(&connection, "msg_retry_limit_user", 10, "keep failing");
    let failures = (0..3)
        .map(|_| {
            ScriptedResponse::failed(
                Vec::new(),
                ProviderError::Transient {
                    status: Some(503),
                    source: None,
                },
            )
        })
        .collect();
    let provider = Arc::new(FakeProvider::new(failures));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-retry-limit"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Err(TurnError::Provider(ProviderError::Transient {
            status: Some(503),
            ..
        }))
    ));
    assert_eq!(provider.requests().len(), 3);
    let rollbacks = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                TurnEvent::Provider {
                    event: StreamEvent::RetryRollback { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(rollbacks, 2);
}

#[tokio::test]
async fn loop_checkpoints_partial_reasoning_and_tools_when_stream_processing_fails() {
    let mut connection = seeded();
    put_user(&connection, "msg_partial_failure_user", 10, "start a tool");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::failed(
        vec![
            StreamEvent::ReasoningStart,
            StreamEvent::ReasoningDelta("checking".to_owned()),
            StreamEvent::ToolUseStart {
                id: "call-partial".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call-partial".to_owned(),
                delta: r#"{"text":"unfinished"#.to_owned(),
            },
        ],
        ProviderError::Transient {
            status: None,
            source: None,
        },
    )]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-partial-failure"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Err(TurnError::Provider(ProviderError::Transient {
            status: None,
            ..
        }))
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        TurnEvent::AssistantCheckpointed {
            message_id,
            interrupted: false,
            ..
        } if message_id == "msg_turn-partial-failure_0001"
    )));
    assert!(
        dispatcher.calls().is_empty(),
        "partial tools must not execute"
    );

    let assistant = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate failed assistant")
        .into_iter()
        .find(|message| message.info.id == "msg_turn-partial-failure_0001")
        .expect("failed assistant checkpoint");
    assert_eq!(assistant.info.data["error"]["name"], "provider");
    assert_eq!(assistant.info.data["finish"], "error");
    assert!(
        assistant
            .parts
            .iter()
            .any(|part| { part.kind == PartKind::Reasoning && part.data["text"] == "checking" })
    );
    let tool = assistant
        .parts
        .iter()
        .find(|part| part.kind == PartKind::Tool)
        .expect("partial tool was durably closed");
    assert_eq!(tool.data["callID"], "call-partial");
    assert_eq!(tool.data["state"]["status"], "error");
    assert_eq!(
        tool.data["state"]["error"],
        "[Tool execution skipped because the turn failed]"
    );
}

#[tokio::test]
async fn loop_provider_retry_never_replays_a_protocol_failure() {
    let mut connection = seeded();
    put_user(&connection, "msg_permanent_user", 10, "do not retry");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::failed(
            Vec::new(),
            ProviderError::Protocol {
                code: ProviderProtocolFailure::InvalidUpstreamToolCall,
                source: None,
            },
        ),
        ScriptedResponse::complete(vec![StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        }]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-permanent"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Err(TurnError::Provider(ProviderError::Protocol {
            code: ProviderProtocolFailure::InvalidUpstreamToolCall,
            ..
        }))
    ));
    assert_eq!(provider.requests().len(), 1);
    assert!(!events.iter().any(|event| matches!(
        event,
        TurnEvent::Provider {
            event: StreamEvent::RetryRollback { .. },
            ..
        }
    )));

    let terminal: String = connection
        .query_row(
            "SELECT data FROM event \
             WHERE aggregate_id = ?1 AND type = 'session.provider.attempt.1' \
               AND json_extract(data, '$.status') = 'failed'",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("failed protocol attempt");
    let terminal: Value = serde_json::from_str(&terminal).expect("protocol attempt JSON");
    assert_eq!(terminal["providerErrorCode"], "invalid_upstream_tool_call");
    assert_eq!(terminal["retryable"], false);
}

#[tokio::test]
async fn loop_repairs_a_missing_tool_result_before_the_provider_sees_history() {
    let mut connection = seeded();
    put_user(&connection, "msg_before_repair", 10, "start tool");
    put_pending_tool(
        &connection,
        "msg_orphaned_assistant",
        "prt_orphaned_tool",
        20,
        "call-orphaned",
    );
    put_user(&connection, "msg_after_repair", 30, "continue safely");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("continued".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-repair"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));
    assert_eq!(
        events[1],
        TurnEvent::HistoryRepaired {
            repaired_tool_results: 1,
        }
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let mut saw_use = false;
    let mut saw_result = false;
    for message in &request.messages {
        for block in &message.content {
            match block {
                RequestContentBlock::ToolUse { id, .. } if id == "call-orphaned" => {
                    saw_use = true;
                }
                RequestContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == "call-orphaned" => {
                    saw_result = true;
                    assert_eq!(content, "[Tool execution was interrupted]");
                    assert_eq!(*is_error, Some(true));
                }
                RequestContentBlock::Text { .. }
                | RequestContentBlock::ResourceLink { .. }
                | RequestContentBlock::SignedThinking { .. }
                | RequestContentBlock::ProviderEncryptedReasoning { .. }
                | RequestContentBlock::ToolUse { .. }
                | RequestContentBlock::ToolResult { .. }
                | RequestContentBlock::Image { .. } => {}
            }
        }
    }
    assert!(
        saw_use && saw_result,
        "provider must see a repaired tool pair"
    );

    let repaired = MessageStore::new(&connection)
        .part("prt_orphaned_tool")
        .expect("read repaired tool part");
    assert_eq!(repaired.data["state"]["status"], "error");
    assert_eq!(
        repaired.data["state"]["error"],
        "[Tool execution was interrupted]"
    );
    eprintln!(
        "REPAIR_QA provider_request={request:#?} db_part={:#?}",
        repaired.to_json()
    );
}

/// Todo 113's two-phase loader is allowed to avoid decoding only the history that
/// [`retained_history`] would discard. The provider-visible bytes are the invariant:
/// changing their order loses the append-only prompt-cache hit, while omitting any
/// retained block changes the model's prompt. The pending tool before the boundary
/// separately proves repair still scans beyond the retained suffix.
#[tokio::test]
async fn loop_compacted_prefix_is_byte_identical_without_decoding_the_discarded_head() {
    const SYSTEM: &str = "You are a deterministic test agent.";
    let mut connection = seeded();
    put_user(&connection, "msg_old_user", 10, "old request");
    put_pending_tool(
        &connection,
        "msg_old_pending",
        "prt_old_pending",
        15,
        "call-old-pending",
    );
    let large_output = "x".repeat(2 * 1024 * 1024);
    put_completed_tool_output(
        &connection,
        "msg_old_large",
        "prt_old_large",
        20,
        &large_output,
    );
    put_user(&connection, "msg_tail_user", 30, "retained request");
    put_assistant_text(
        &connection,
        "msg_tail_assistant",
        40,
        "msg_tail_user",
        "retained answer",
    );
    put_successful_compaction(
        &connection,
        "msg_compaction_marker",
        "msg_compaction_summary",
        "msg_tail_user",
        50,
    );
    put_user(&connection, "msg_current_user", 60, "current request");

    // Reference path: the pre-113 implementation decoded every part and trimmed
    // only afterwards. Its serialized projection is the byte-level oracle.
    let full = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("full reference history");
    let expected_messages = project_history(SYSTEM, retained_history(&full))
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let expected_prefix = serde_json::to_vec(&expected_messages).expect("serialize reference");
    let full_part_bytes = full
        .iter()
        .flat_map(|message| &message.parts)
        .map(|part| {
            serde_json::to_vec(&part.data)
                .expect("serialize part")
                .len()
        })
        .sum::<usize>();

    // Optimized path: all retained prompt messages must remain, in byte-identical
    // order, while the multi-megabyte pre-compaction tool blob is never resident.
    let optimized = hydrate_retained_history(&connection, SESSION_ID).expect("retained history");
    let optimized_messages = project_history(SYSTEM, retained_history(&optimized))
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let owned_messages = project_history_owned(SYSTEM, optimized.clone());
    let optimized_prefix =
        serde_json::to_vec(&optimized_messages).expect("serialize optimized prefix");
    let optimized_part_bytes = optimized
        .iter()
        .flat_map(|message| &message.parts)
        .map(|part| {
            serde_json::to_vec(&part.data)
                .expect("serialize part")
                .len()
        })
        .sum::<usize>();

    assert_eq!(
        optimized_prefix, expected_prefix,
        "the request prefix changed byte-for-byte"
    );
    assert_eq!(
        optimized_messages, expected_messages,
        "a prompt-bearing message was dropped or reordered"
    );
    assert_eq!(
        serde_json::to_vec(&owned_messages).expect("serialize consuming projection"),
        expected_prefix,
        "the consuming projection changed the provider-visible bytes"
    );
    assert!(
        optimized_part_bytes * 100 < full_part_bytes,
        "the discarded head was still decoded: full={full_part_bytes}, optimized={optimized_part_bytes}"
    );
    assert!(
        optimized
            .iter()
            .all(|message| message.info.id != "msg_old_large"),
        "the pre-compaction large tool message remains resident"
    );

    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("done".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("turn-compacted-prefix"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        serde_json::to_vec(&requests[0].messages).expect("serialize provider request"),
        expected_prefix,
        "run_turn did not send the reference prefix"
    );

    let repaired = MessageStore::new(&connection)
        .part("prt_old_pending")
        .expect("read pre-boundary pending tool");
    assert_eq!(
        repaired.data["state"]["status"], "error",
        "tool repair stopped at the compaction boundary"
    );
}

#[tokio::test]
async fn compaction_preserves_durable_execution_state_and_restart_recovery_is_idempotent() {
    let pool = seeded_shared_pool_with_goal_schema();
    let mut connection = pool
        .open_connection()
        .expect("open first process connection");
    put_user(&connection, "msg_discarded_user", 10, "discarded request");
    put_assistant_text(
        &connection,
        "msg_discarded_assistant",
        20,
        "msg_discarded_user",
        "discarded answer",
    );
    put_user(
        &connection,
        "msg_before_compaction",
        30,
        "create the durable execution state",
    );

    let first_provider = run_single_text_turn(
        &mut connection,
        "turn-before-durable-compaction",
        DynamicContext::default(),
        "durable state created",
    )
    .await;
    assert_eq!(first_provider.requests().len(), 1);
    let original_receipt_id = latest_prompt_receipt_id(&connection);

    seed_durable_compaction_state(Arc::clone(&pool));
    let inbox = SessionInbox::new(Arc::clone(&pool));
    inbox
        .promote_id(SESSION_ID, "input_compaction_report")
        .expect("promote the report before simulated process loss")
        .expect("the report is queued");
    let before = durable_compaction_snapshot(&connection, Arc::clone(&pool));
    assert_eq!(before.report.state.as_str(), "promoted");
    assert_eq!(before.prompt_receipts.len(), 1);
    assert_eq!(before.prompt_receipts[0].0, original_receipt_id);

    let compaction_time = zuno_db::message::now_millis().saturating_add(100);
    put_successful_compaction(
        &connection,
        "msg_durable_compaction_marker",
        "msg_durable_compaction_summary",
        "msg_before_compaction",
        compaction_time,
    );
    put_user(
        &connection,
        "msg_after_compaction",
        compaction_time.saturating_add(10),
        "resume from durable state",
    );
    drop(connection);

    let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
    let first_recovery = jobs
        .pending_reports_for(SESSION_ID)
        .expect("recover pending report after restart");
    let second_recovery = jobs
        .pending_reports_for(SESSION_ID)
        .expect("repeat recovery idempotently");
    assert_eq!(
        first_recovery, second_recovery,
        "recovery must reuse the same durable Job and report reference"
    );
    assert_eq!(first_recovery.len(), 1);
    assert_eq!(first_recovery[0].id, "job_compaction");
    assert_eq!(
        first_recovery[0].report_input_id.as_deref(),
        Some("input_compaction_report")
    );

    let mut restarted = pool
        .open_connection()
        .expect("open restarted process connection");
    let recovered_context =
        durable_compaction_context(&restarted, Arc::clone(&pool), &original_receipt_id);
    let second_provider = run_single_text_turn(
        &mut restarted,
        "turn-after-durable-compaction",
        DynamicContext::new(recovered_context.clone()),
        "resumed from durable state",
    )
    .await;

    let after = durable_compaction_snapshot(&restarted, Arc::clone(&pool));
    assert_eq!(after.goal, before.goal, "compaction changed the Goal row");
    assert_eq!(after.plan, before.plan, "compaction changed the Plan row");
    assert_eq!(
        after.work_items, before.work_items,
        "compaction changed the Todo/WorkItem rows"
    );
    assert_eq!(after.job, before.job, "compaction changed the settled Job");
    assert_eq!(after.report.id, before.report.id);
    assert_eq!(after.report.prompt, before.report.prompt);
    assert_eq!(
        after.report.admitted_sequence, before.report.admitted_sequence,
        "restart recovery admitted a duplicate report"
    );
    assert_eq!(after.report.state.as_str(), "queued");
    assert_eq!(
        after.report.revision,
        before.report.revision.saturating_add(1),
        "the promoted report must be recovered exactly once"
    );

    assert_eq!(second_provider.requests().len(), 1);
    assert_eq!(
        second_provider.requests()[0].developer_context.last(),
        Some(&recovered_context),
        "the restarted request did not receive the durable references"
    );
    assert_eq!(after.prompt_receipts.len(), 2);
    assert_eq!(
        after
            .prompt_receipts
            .iter()
            .filter(|(id, _)| id == &original_receipt_id)
            .count(),
        1,
        "the pre-compaction Prompt receipt was deleted or duplicated"
    );
    let latest_receipt: Value =
        serde_json::from_str(&after.prompt_receipts[1].1).expect("latest prompt receipt JSON");
    let persisted_projection = latest_receipt
        .get("actualProviderProjection")
        .unwrap_or(&latest_receipt["providerProjection"]);
    assert_eq!(
        persisted_projection["developer"]
            .as_array()
            .and_then(|sections| sections.last())
            .and_then(Value::as_str),
        Some(recovered_context.as_str()),
        "the durable recovery projection was not captured by the new Prompt receipt"
    );

    assert_eq!(
        scalar_count(
            &restarted,
            "SELECT COUNT(*) FROM session_input \
             WHERE session_id = 'ses_loop_test' AND id = 'input_compaction_report'",
        ),
        1,
        "restart recovery duplicated the report input"
    );
    assert_eq!(
        scalar_count(
            &restarted,
            "SELECT COUNT(*) FROM event \
             WHERE aggregate_id = 'ses_loop_test' \
               AND type = 'session.input.recovered.1'",
        ),
        1,
        "repeated recovery must not emit a second recovery transition"
    );
}

#[test]
fn loop_failed_compaction_falls_back_to_byte_identical_full_history() {
    let connection = seeded();
    put_user(
        &connection,
        "msg_old_user",
        10,
        "history before failed summary",
    );
    put_user(
        &connection,
        "msg_tail_user",
        20,
        "tail that must not replace history",
    );
    put_incomplete_compaction(
        &connection,
        "msg_failed_marker",
        "msg_failed_summary",
        "msg_tail_user",
        30,
        None,
        true,
    );
    put_user(&connection, "msg_current_user", 40, "current request");

    assert_loader_projection_matches_full_reference(&connection);
}

#[test]
fn loop_empty_compaction_summary_falls_back_to_byte_identical_full_history() {
    let connection = seeded();
    put_user(
        &connection,
        "msg_old_user",
        10,
        "history before empty summary",
    );
    put_user(
        &connection,
        "msg_tail_user",
        20,
        "tail that must not replace history",
    );
    put_incomplete_compaction(
        &connection,
        "msg_empty_marker",
        "msg_empty_summary",
        "msg_tail_user",
        30,
        Some(""),
        false,
    );
    put_user(&connection, "msg_current_user", 40, "current request");

    assert_loader_projection_matches_full_reference(&connection);
}

#[test]
fn loop_successful_compaction_with_missing_tail_falls_back_to_byte_identical_full_history() {
    let connection = seeded();
    put_user(
        &connection,
        "msg_old_user",
        10,
        "history before dangling boundary",
    );
    put_user(
        &connection,
        "msg_tail_user",
        20,
        "real tail remains part of history",
    );
    put_successful_compaction(
        &connection,
        "msg_dangling_marker",
        "msg_dangling_summary",
        "msg_missing_tail",
        30,
    );
    put_user(&connection, "msg_current_user", 40, "current request");

    assert_dropping_first_history_message_changes_projection(&connection);
    assert_loader_projection_matches_full_reference(&connection);
}

#[test]
fn loop_without_compaction_marker_is_byte_identical_to_full_history() {
    let connection = seeded();
    put_user(&connection, "msg_first_user", 10, "first request");
    put_assistant_text(
        &connection,
        "msg_first_assistant",
        20,
        "msg_first_user",
        "first answer",
    );
    put_user(&connection, "msg_current_user", 30, "current request");

    assert_loader_projection_matches_full_reference(&connection);
}

#[test]
fn retained_history_tail_hydrates_only_the_bounded_suffix() {
    let connection = seeded();
    for index in 0..5 {
        put_user(
            &connection,
            &format!("msg_tail_{index}"),
            10 + index,
            &format!("request {index}"),
        );
    }

    let retained = hydrate_retained_history_tail(&connection, SESSION_ID, 2, u64::MAX)
        .expect("bounded history");

    assert_eq!(retained.omitted, 3);
    assert_eq!(
        retained
            .messages
            .iter()
            .map(|message| message.info.id.as_str())
            .collect::<Vec<_>>(),
        vec!["msg_tail_3", "msg_tail_4"]
    );
}

#[test]
fn retained_history_byte_bound_rejects_an_oversized_tail_before_json_hydration() {
    let connection = seeded();
    put_user(
        &connection,
        "msg_oversized_tail",
        10,
        "temporary valid payload",
    );
    let oversized = serde_json::to_string(&json!({
        "type": "unknown-future-part",
        "payload": "x".repeat(4_096),
    }))
    .expect("encode oversized corrupt fixture");
    connection
        .execute(
            "UPDATE part SET data = ?1 WHERE id = ?2",
            [oversized.as_str(), "prt_msg_oversized_tail"],
        )
        .expect("replace part with oversized undecodable JSON");

    let retained = hydrate_retained_history_tail(&connection, SESSION_ID, 8, 1_024)
        .expect("oversized tail must be omitted before part decoding");

    assert!(retained.messages.is_empty());
    assert_eq!(retained.omitted, 1);
}

async fn collect_and_interrupt(
    mut receiver: mpsc::Receiver<TurnEvent>,
    interrupt: InterruptSignal,
) -> (Vec<TurnEvent>, Duration) {
    let mut events = Vec::new();
    let mut fired_at = None;
    while let Some(event) = receiver.recv().await {
        if fired_at.is_none()
            && matches!(
                &event,
                TurnEvent::Provider {
                    event: StreamEvent::TextDelta(text),
                    ..
                } if text == "partial checkpoint"
            )
        {
            fired_at = Some(Instant::now());
            interrupt.fire();
        }
        events.push(event);
    }
    let elapsed = fired_at
        .expect("the first text delta fires the interrupt")
        .elapsed();
    (events, elapsed)
}

#[tokio::test]
async fn loop_mid_stream_interrupt_finishes_within_100ms_and_checkpoints_db() {
    let mut connection = seeded();
    put_user(&connection, "msg_interrupt_user", 10, "stream forever");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::hanging(vec![
        StreamEvent::TextDelta("partial checkpoint".to_owned()),
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-interrupt"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let collector = collect_and_interrupt(receiver, interrupt.clone());
    let (outcome, (events, elapsed)) = tokio::join!(turn, collector);
    assert_eq!(
        outcome.expect("interrupt is a normal turn outcome"),
        TurnOutcome::Interrupted {
            assistant_message_id: Some("msg_turn-interrupt_0001".to_owned()),
            steps: 1,
        }
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "interrupt took {elapsed:?}"
    );
    assert!(matches!(
        events.last(),
        Some(TurnEvent::TurnInterrupted {
            assistant_message_id: Some(message_id),
            steps: 1,
        }) if message_id == "msg_turn-interrupt_0001"
    ));

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate interrupted turn");
    let assistant = hydrated
        .iter()
        .find(|message| message.info.id == "msg_turn-interrupt_0001")
        .expect("interrupted assistant was persisted");
    let text = assistant
        .parts
        .iter()
        .find(|part| part.kind == PartKind::Text)
        .expect("partial text was checkpointed");
    assert_eq!(text.data["text"], "partial checkpoint");
    assert_eq!(assistant.info.data["error"]["name"], "AbortError");
    assert_eq!(
        assistant.info.data["error"]["data"]["message"],
        zuno_engine::r#loop::INTERRUPTED_TURN_NOTICE
    );
    assert!(assistant.info.data["time"]["completed"].is_number());
    eprintln!(
        "INTERRUPT_QA elapsed={elapsed:?} db_message={:#?} db_parts={:#?}",
        assistant.info.to_json(),
        assistant
            .parts
            .iter()
            .map(PartRecord::to_json)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn loop_head_interrupt_starts_no_provider_request() {
    let mut connection = seeded();
    put_user(&connection, "msg_head_user", 10, "do not start");
    let provider = Arc::new(FakeProvider::new(Vec::new()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    interrupt.fire();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-head"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert_eq!(
        outcome.expect("head interrupt is a normal outcome"),
        TurnOutcome::Interrupted {
            assistant_message_id: None,
            steps: 0,
        }
    );
    assert_eq!(
        events,
        vec![
            TurnEvent::TurnStarted {
                session_id: SESSION_ID.to_owned(),
            },
            TurnEvent::TurnInterrupted {
                assistant_message_id: None,
                steps: 0,
            },
        ]
    );
    assert!(provider.requests().is_empty());
}

/// The reply must sort after the prompt no matter what the clock says.
///
/// Todo 105 regressed exactly this. Moving the prompt's write to immediately before
/// the loop left it in the same millisecond as the first assistant record, ties are
/// broken by the random uuid in the id, and the losing half of those flips filed the
/// reply ahead of the prompt. Step 2 then hydrated a different message at index 1
/// than step 1 had sent and the append-only tracker refused the request.
///
/// A same-millisecond tie only reproduces that a fraction of the time, so this pins
/// the general invariant instead: a prompt stamped ahead of the current clock. An
/// unclamped reply carries `now_millis()`, sorts before that prompt every single
/// time, and this test fails deterministically. Clock skew and an imported session
/// reach the same state in production.
#[tokio::test]
async fn loop_reply_sorts_after_a_prompt_stamped_ahead_of_the_clock() {
    let ahead = zuno_db::message::now_millis() + 60_000;
    let mut connection = seeded();
    put_user(&connection, "msg_user", ahead, "echo hello");
    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-skew"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    assert_eq!(
        outcome.expect("a prompt ahead of the clock must not break the request prefix"),
        TurnOutcome::Completed {
            assistant_message_id: "msg_turn-skew_0002".to_owned(),
            steps: 2,
            unresolved_tool_failures: Vec::new(),
        }
    );

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate the completed turn");
    let order: Vec<&str> = hydrated
        .iter()
        .map(|message| message.info.role.as_str())
        .collect();
    assert_eq!(
        order,
        vec!["user", "assistant", "assistant"],
        "the persisted order put a reply ahead of the prompt it answers"
    );
    assert!(
        hydrated
            .windows(2)
            .all(|pair| pair[0].info.time_created < pair[1].info.time_created),
        "each record must carry a strictly later stamp than the one before it, so the \
         order does not depend on which random id sorts first"
    );
    let tool = hydrated[1]
        .parts
        .iter()
        .find(|part| part.kind == PartKind::Tool)
        .expect("first assistant tool part");
    assert_eq!(tool.data["tool"], "echo");
    assert_eq!(tool.data["displayName"], "echo-runtime");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.messages[1].role,
            Role::User,
            "the prompt must stay at index 1 of every request in the turn"
        );
    }
}

#[test]
fn loop_test_fixture_uses_no_live_network_provider() {
    let provider = FakeProvider::new(Vec::new());
    assert_eq!(provider.id(), "fake");
    assert!(provider.requests().is_empty());
    assert_eq!(Value::Null, Value::Null);
    assert_eq!(Role::User, Role::User);
}

/// Usage arriving *after* the finish reason, which is where every OpenAI-compatible
/// endpoint puts it (`stream_options.include_usage` sends a final chunk whose `choices`
/// is empty and whose `usage` is populated).
///
/// The existing projector test applies `TokenUsage` *before* `MessageEnd` — an ordering
/// no real provider produces — so it proved the projector worked while the loop was
/// discarding the event upstream of it. Measured consequence: every assistant row in a
/// nine-session database carried `input: 0, output: 0`.
fn trailing_usage_responses() -> Vec<ScriptedResponse> {
    vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("The answer is 3.".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
        StreamEvent::TokenUsage {
            input_tokens: Some(4210),
            output_tokens: Some(186),
            cache_read_input_tokens: Some(1024),
            cache_write_input_tokens: Some(64),
            accounting: PromptAccounting::CacheInsideInput,
        },
    ])]
}

#[tokio::test]
async fn loop_records_token_usage_that_arrives_after_the_finish_reason() {
    let mut connection = seeded();
    put_user(&connection, "msg_user", 10, "what resolver version?");
    let provider = Arc::new(FakeProvider::new(trailing_usage_responses()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-usage"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert_eq!(
        outcome.expect("the turn succeeds"),
        TurnOutcome::Completed {
            assistant_message_id: "msg_turn-usage_0001".to_owned(),
            steps: 1,
            unresolved_tool_failures: Vec::new(),
        },
        "reading past the finish reason must not change how the turn ends"
    );

    // The event must reach whoever is watching — the TUI's status strip reads it there.
    assert!(
        events.iter().any(|event| matches!(
            event,
            TurnEvent::Provider {
                event: StreamEvent::TokenUsage {
                    input_tokens: Some(4210),
                    output_tokens: Some(186),
                    ..
                },
                ..
            }
        )),
        "the trailing usage event was never published:\n{events:#?}"
    );

    // And it must reach the row `update_usage` writes, which is what every later
    // consumer — cost reporting, compaction thresholds — reads.
    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate the finished turn");
    let assistant = hydrated
        .iter()
        .find(|message| message.info.role.as_str() == "assistant")
        .expect("one assistant message");
    let tokens = assistant
        .info
        .data
        .get("tokens")
        .and_then(serde_json::Value::as_object)
        .expect("the assistant row carries a tokens object");
    assert_eq!(
        tokens.get("input").and_then(serde_json::Value::as_u64),
        Some(4210),
        "input tokens were dropped: {tokens:?}"
    );
    assert_eq!(
        tokens.get("output").and_then(serde_json::Value::as_u64),
        Some(186),
        "output tokens were dropped: {tokens:?}"
    );
    let cache = tokens
        .get("cache")
        .and_then(serde_json::Value::as_object)
        .expect("a cache object");
    assert_eq!(
        cache.get("read").and_then(serde_json::Value::as_u64),
        Some(1024)
    );
    assert_eq!(
        cache.get("write").and_then(serde_json::Value::as_u64),
        Some(64)
    );
    assert_eq!(
        tokens.get("accounting").and_then(serde_json::Value::as_str),
        Some("cache-inside-input")
    );
    let session = zuno_db::session::get(&connection, SESSION_ID).expect("read session usage");
    assert!(session.usage.known);
    assert_eq!(session.usage.tokens.input, 3_122);
    assert_eq!(session.usage.tokens.output, 186);
    assert_eq!(session.usage.tokens.cache_read, 1_024);
    assert_eq!(session.usage.tokens.cache_write, 64);
    assert_eq!(session.usage.last_prompt_tokens, Some(4_210));
}

#[tokio::test]
async fn loop_provider_failure_preserves_the_last_confirmed_session_usage() {
    let mut connection = seeded();
    put_user(&connection, "msg_usage_ok_user", 10, "record usage");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("first response".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(4_210),
                output_tokens: Some(186),
                cache_read_input_tokens: Some(1_024),
                cache_write_input_tokens: Some(64),
                accounting: PromptAccounting::CacheInsideInput,
            },
        ]),
        ScriptedResponse::failed(
            Vec::new(),
            ProviderError::Fatal {
                status: Some(400),
                source: None,
            },
        ),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();

    let (first_sender, first_receiver) = event_channel();
    let first = run_turn(
        request("turn-usage-confirmed"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        first_sender,
    );
    let (first_outcome, _events) = tokio::join!(first, collect_events(first_receiver));
    first_outcome.expect("first turn succeeds");

    let confirmed = zuno_db::session::get(&connection, SESSION_ID)
        .expect("read confirmed usage")
        .usage;
    assert!(confirmed.known);
    assert_eq!(confirmed.last_prompt_tokens, Some(4_210));

    put_user(&connection, "msg_usage_failed_user", 20, "trigger failure");
    let (failed_sender, failed_receiver) = event_channel();
    let failed = run_turn(
        request("turn-usage-failed"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        failed_sender,
    );
    let (failed_outcome, _events) = tokio::join!(failed, collect_events(failed_receiver));
    assert!(matches!(
        failed_outcome,
        Err(TurnError::Provider(ProviderError::Fatal {
            status: Some(400),
            ..
        }))
    ));

    let after_failure = zuno_db::session::get(&connection, SESSION_ID)
        .expect("read usage after failure")
        .usage;
    assert_eq!(after_failure.cost, confirmed.cost);
    assert_eq!(after_failure.tokens, confirmed.tokens);
    assert_eq!(
        after_failure.last_prompt_tokens,
        confirmed.last_prompt_tokens
    );
    assert_eq!(after_failure.context_limit, confirmed.context_limit);
    assert_eq!(after_failure.accounting, confirmed.accounting);
    assert_eq!(after_failure.known, confirmed.known);
    assert_eq!(after_failure.last_confirmed_at, confirmed.last_confirmed_at);
    assert!(
        after_failure
            .estimated_pending_prompt_tokens
            .is_some_and(|estimate| estimate > 0),
        "the rejected request should retain its local prompt estimate"
    );
    assert_eq!(
        after_failure.failed_turns, confirmed.failed_turns,
        "the replaceable TurnHost, not the built-in engine driver, owns top-level failure counting"
    );
    let failed_assistant = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate failed turn")
        .into_iter()
        .find(|message| message.info.id == "msg_turn-usage-failed_0001")
        .expect("failed assistant checkpoint");
    assert!(
        failed_assistant.info.data.get("tokens").is_none(),
        "a request rejected before usage must not persist a synthetic zero snapshot"
    );
}

#[tokio::test]
async fn loop_does_not_wait_forever_for_a_provider_that_streams_past_its_own_finish() {
    // The bound on the trailing drain. A provider that keeps sending after saying it
    // finished must not be able to hold the turn open, so the step ends once the budget
    // is spent rather than following the stream.
    let mut connection = seeded();
    put_user(&connection, "msg_user", 10, "hello");
    let mut events = vec![
        StreamEvent::TextDelta("done".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ];
    events.extend((0..64).map(|_| StreamEvent::StatusDetail {
        detail: "keep-alive".to_owned(),
    }));
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse {
        events: events.into_iter().map(Ok).collect(),
        hang_after: true,
    }]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-chatty"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let finished = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(turn, collect_events(receiver))
    })
    .await;
    let (outcome, published) = finished.expect("the turn must not hang on a chatty provider");
    assert_eq!(
        outcome.expect("the turn succeeds"),
        TurnOutcome::Completed {
            assistant_message_id: "msg_turn-chatty_0001".to_owned(),
            steps: 1,
            unresolved_tool_failures: Vec::new(),
        }
    );
    let trailing = published
        .iter()
        .filter(|event| {
            matches!(
                event,
                TurnEvent::Provider {
                    event: StreamEvent::StatusDetail { .. },
                    ..
                }
            )
        })
        .count();
    assert!(
        trailing <= usize::from(zuno_engine::r#loop::TRAILING_FRAME_BUDGET),
        "the drain read {trailing} trailing frames, past its budget of {}",
        zuno_engine::r#loop::TRAILING_FRAME_BUDGET
    );
}
