#![cfg(any(unix, windows))]

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use zuno_db::event_log::SessionEventLog;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord, now_millis};
use zuno_db::session_work_cycle::{CycleStop, SessionWorkCycle};
use zuno_db::{Pool, migration, session};
use zuno_error::ToolError;
use zuno_paths::DbLocation;
use zuno_tool::{AllowAll, DenyAll, NeverInterrupted, Tool, ToolContext};
use zuno_tools::uncertain::{
    FileInspectionActor, FileInspectionError, FileInspectionReceipt, FileInspector,
    INSPECTION_EVENT, INTENT_EVENT, NativeInspectionGuard,
};
use zuno_tools::{FileFormatter, FileTools};
use zuno_types::execution::CollaborationMode;

const SESSION: &str = "file-inspection-session";
const MESSAGE: &str = "native-message";
const CYCLE: &str = "file-cycle";
const TURN: &str = "mutation-turn";

/// A real post-write filesystem failure: the native tool wrote the file, then its
/// formatter moved that file before the native final read. No verifier is faked.
struct MoveAfterWrite;

#[async_trait]
impl FileFormatter for MoveAfterWrite {
    async fn format(&self, path: &Path) -> std::io::Result<bool> {
        std::fs::rename(path, path.with_extension("uncertain-held"))?;
        Ok(false)
    }
}

struct Fixture {
    root: tempfile::TempDir,
    database_directory: tempfile::TempDir,
    pool: Arc<Pool>,
    tools: FileTools,
    inspector: Arc<FileInspector>,
    run_gate: Arc<tokio::sync::Mutex<()>>,
}

struct TestNativeLease {
    _exclusive: tokio::sync::OwnedMutexGuard<()>,
    session_id: &'static str,
}

impl NativeInspectionGuard for TestNativeLease {
    fn session_id(&self) -> &str {
        self.session_id
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("workspace");
        let database_directory = tempfile::tempdir().expect("database directory");
        let pool = Arc::new(
            Pool::open(&DbLocation::File(
                database_directory.path().join("state.db"),
            ))
            .expect("pool"),
        );
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute(
                    "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) \
                 VALUES('project',?1,1,1,'[]')",
                    [root.path().to_string_lossy().as_ref()],
                )
                .expect("project");
        }
        pool.transaction(|tx| {
            session::create(
                tx,
                &session::SessionCreate::new(
                    SESSION,
                    "inspection",
                    "project",
                    root.path().to_string_lossy().as_ref(),
                    root.path().to_string_lossy().as_ref(),
                    "inspection",
                    "test",
                )
                .at(1),
            )?;
            MessageStore::new(tx).put_message_at(
                &MessageRecord::from_json(json!({
                    "id":MESSAGE, "sessionID":SESSION, "role":"assistant", "time":{"created":1}
                }))?,
                1,
            )?;
            let mut execution =
                zuno_db::session_execution::seed_in(tx, SESSION, CollaborationMode::Work, None, 1)?;
            execution.cycle_id = Some(CYCLE.to_owned());
            zuno_db::session_execution::update_in(tx, execution.revision, execution)?;
            zuno_db::session_work_cycle::save_in(
                tx,
                &SessionWorkCycle {
                    session_id: SESSION.to_owned(),
                    cycle_id: CYCLE.to_owned(),
                    anchor_message_id: None,
                    goal_id: None,
                    plan_id: None,
                    active_turn_id: None,
                    todo_ids: Default::default(),
                    resumed_goal_cycles: Default::default(),
                    stopped: None,
                    scheduling: None,
                },
                1,
            )
        })
        .expect("session");
        let (tools, inspector) = FileTools::with_inspector_and_formatter(
            Arc::clone(&pool),
            root.path(),
            Arc::new(MoveAfterWrite),
        )
        .expect("real native factory");
        Self {
            root,
            database_directory,
            pool,
            tools,
            inspector,
            run_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn lease(&self) -> Arc<dyn NativeInspectionGuard> {
        Arc::new(TestNativeLease {
            _exclusive: Arc::clone(&self.run_gate).lock_owned().await,
            session_id: SESSION,
        })
    }

    async fn inspect_pending(
        &self,
        part_ids: Vec<String>,
        expected_cycle_id: String,
        ctx: ToolContext,
    ) -> Result<FileInspectionReceipt, FileInspectionError> {
        self.inspector
            .inspect_native(part_ids, expected_cycle_id, ctx, self.lease().await)
            .await
    }

    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        match name {
            "write" => Arc::clone(&self.tools.write),
            "edit" => Arc::clone(&self.tools.edit),
            "apply_patch" => Arc::clone(&self.tools.apply_patch),
            _ => panic!("test native tool"),
        }
    }

    fn update_cycle(&self, action: impl FnOnce(&mut SessionWorkCycle)) {
        self.pool
            .transaction(|tx| {
                let mut cycle =
                    zuno_db::session_work_cycle::current_in(tx, SESSION)?.expect("cycle");
                action(&mut cycle);
                if cycle.cycle_id != CYCLE {
                    let mut state =
                        zuno_db::session_execution::read_in(tx, SESSION)?.expect("state");
                    state.cycle_id = Some(cycle.cycle_id.clone());
                    zuno_db::session_execution::update_in(tx, state.revision, state)?;
                }
                zuno_db::session_work_cycle::save_in(tx, &cycle, now_millis())
            })
            .expect("cycle");
    }

    fn checkpoint(&self, id: &str, tool: &Arc<dyn Tool>, input: Value) {
        self.update_cycle(|cycle| cycle.active_turn_id = Some(TURN.to_owned()));
        let now = now_millis();
        let part = PartRecord::from_json(json!({
            "id":id,"sessionID":SESSION,"messageID":MESSAGE,"type":"tool",
            "callID":format!("call-{id}"),"tool":tool.id(),
            "toolSchemaIdentity":tool.definition().schema_identity(),
            "state":{"status":"pending","input":input,"dispatchTracked":true,"dispatchedAtMs":now}
        }), now).expect("checkpoint");
        MessageStore::new(&self.pool.get().expect("connection"))
            .put_part_at(&part, now)
            .expect("checkpoint write");
    }

    fn settle(&self, id: &str, error: &ToolError) {
        let ToolError::Uncertain {
            tool,
            applied_paths,
            ..
        } = error
        else {
            panic!("expected the real native tool's uncertain outcome: {error:?}");
        };
        let connection = self.pool.get().expect("connection");
        let store = MessageStore::new(&connection);
        let mut part = store.part(id).expect("part");
        let input = part.data["state"]["input"].clone();
        let now = now_millis();
        part.data.insert(
            "state".to_owned(),
            json!({
                "status":"error","input":input,"error":error.to_string(),"outcome":"uncertain",
                "uncertain":{"tool":tool,"callID":format!("call-{id}"),"appliedPaths":applied_paths,
                    "cause":"lost_outcome","observedAtMs":now}
            }),
        );
        store
            .put_part_at(&part, now)
            .expect("persist actual outcome");
        drop(connection);
        // The production field is a retained last-turn fence, not a live flag.
        self.update_cycle(|cycle| cycle.active_turn_id = Some(TURN.to_owned()));
    }

    async fn perform(&self, id: &str, name: &str, input: Value) {
        let tool = self.tool(name);
        self.checkpoint(id, &tool, input.clone());
        let error = tool
            .invoke(input, mutation_context(id))
            .await
            .expect_err("native post-write failure");
        self.settle(id, &error);
    }

    async fn write_uncertain(&self, id: &str, name: &str, text: &str) {
        let path = self.root.path().join(name);
        self.perform(id, "write", json!({"filePath":path,"content":text}))
            .await;
        std::fs::rename(path.with_extension("uncertain-held"), &path)
            .expect("restore current file");
    }

    async fn read_before_edit(&self, name: &str) {
        self.tools
            .read
            .invoke(
                json!({"filePath":self.root.path().join(name)}),
                control_context(),
            )
            .await
            .expect("native pre-edit read");
    }

    fn part(&self, id: &str) -> PartRecord {
        MessageStore::new(&self.pool.get().expect("connection"))
            .part(id)
            .expect("part")
    }

    fn events(&self, kind: &str) -> Vec<zuno_db::event_log::SessionEvent> {
        SessionEventLog::new(Arc::clone(&self.pool))
            .read_of_type_after(SESSION, kind, None)
            .expect("events")
    }

    fn still_pending(&self, id: &str) -> bool {
        self.part(id).data["state"]["uncertain"]["reconciledAtMs"].is_null()
    }
}

fn control_context() -> ToolContext {
    ToolContext::new(
        SESSION,
        "native-control",
        "inspect-call",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn mutation_context(id: &str) -> ToolContext {
    let snapshot = serde_json::from_value(json!({
        "schemaVersion":4,"turnId":TURN,"cycleId":CYCLE,"step":1,
        "capability":{
            "schemaVersion":4,"pack":{"id":"test","version":"1","upstreamRevision":"test"},
            "extensionRevision":0,"permissionPolicySha256":"policy",
            "sandbox":{"mode":"workspace-write","network":"deny","writableRoots":[],"protectedPaths":[]},
            "profiles":[],"presets":[],"councils":[],"workflows":[],"skills":[]
        },
        "owner":{"sessionId":SESSION,"parentSessionId":null,"parentAttempt":null,"workflow":null,"workflowNode":null},
        "agent":{"name":"build","sourceId":"builtin://build","definitionSha256":"definition","permissionSha256":"permission","promptPolicySha256":"prompt"},
        "model":{"providerId":"fixture","modelId":"fixture","wireModelId":"fixture","surface":"responses","reasoningSha256":"reasoning","preset":null},
        "selectedSkills":[],"prompt":{"eventId":null,"assemblySha256":"assembly","actualSha256":"actual"},"tools":[]
    })).expect("immutable Attempt");
    ToolContext::new(
        SESSION,
        MESSAGE,
        format!("call-{id}"),
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
    .with_orchestration_snapshot(Arc::new(snapshot))
}

#[tokio::test]
async fn real_native_write_has_an_evidenced_inspection_success_path() {
    let f = Fixture::new();
    f.write_uncertain("write-part", "target.txt", "actual current bytes")
        .await;
    assert_eq!(f.events(INTENT_EVENT).len(), 1);
    let before = f.part("write-part");
    let receipt = f
        .inspect_pending(
            vec!["write-part".to_owned()],
            CYCLE.to_owned(),
            control_context(),
        )
        .await
        .expect("real file inspection");
    let observation = &receipt.calls[0].targets[0];
    assert_eq!(
        observation.sha256.as_deref(),
        Some(hex::encode(Sha256::digest(b"actual current bytes")).as_str())
    );
    assert_eq!(observation.metadata.as_ref().unwrap().length, 20);
    assert!(observation.exists);
    assert_eq!(receipt.source, "native_file_state");
    assert_eq!(receipt.actor, FileInspectionActor::NativeControl);
    let after = f.part("write-part");
    assert_eq!(after.data["state"]["outcome"], "uncertain");
    assert_eq!(after.data["state"]["status"], "error");
    assert_eq!(after.data["state"]["error"], before.data["state"]["error"]);
    assert!(!f.still_pending("write-part"));
    let events = f.events(INSPECTION_EVENT);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, receipt.event_id);
    assert_eq!(events[0].properties["originalOutcome"], "uncertain");
    assert_eq!(events[0].properties["replayAuthorized"], false);
}

#[tokio::test]
async fn real_native_edit_observes_the_current_bytes_instead_of_claiming_the_edit_succeeded() {
    let f = Fixture::new();
    std::fs::write(f.root.path().join("edit.txt"), "before").unwrap();
    f.read_before_edit("edit.txt").await;
    f.perform(
        "edit-part",
        "edit",
        json!({
            "filePath":f.root.path().join("edit.txt"),
            "edits":[{"oldString":"before","newString":"after"}],
            "intent":"an injected intent does not alter the native args digest"
        }),
    )
    .await;
    std::fs::write(f.root.path().join("edit.txt"), "a later external state").unwrap();
    let receipt = f
        .inspect_pending(
            vec!["edit-part".to_owned()],
            CYCLE.to_owned(),
            control_context(),
        )
        .await
        .unwrap();
    assert_eq!(
        receipt.calls[0].targets[0].sha256,
        Some(hex::encode(Sha256::digest(b"a later external state")))
    );
    assert_eq!(f.part("edit-part").data["state"]["outcome"], "uncertain");
}

#[tokio::test]
async fn a_partial_native_patch_inspects_move_source_destination_and_not_yet_applied_targets() {
    let f = Fixture::new();
    std::fs::write(f.root.path().join("source.txt"), "before\n").unwrap();
    std::fs::write(f.root.path().join("doomed.txt"), "still here\n").unwrap();
    f.read_before_edit("source.txt").await;
    f.read_before_edit("doomed.txt").await;
    f.perform("patch-part", "apply_patch", json!({"patchText":
        "*** Begin Patch\n*** Update File: source.txt\n*** Move to: destination.txt\n@@\n-before\n+after\n*** Delete File: doomed.txt\n*** End Patch"
    })).await;
    std::fs::rename(
        f.root.path().join("destination.uncertain-held"),
        f.root.path().join("destination.txt"),
    )
    .unwrap();
    let receipt = f
        .inspect_pending(
            vec!["patch-part".to_owned()],
            CYCLE.to_owned(),
            control_context(),
        )
        .await
        .expect("inspect all parsed targets");
    let targets = &receipt.calls[0].targets;
    assert_eq!(targets.len(), 3);
    assert!(
        !targets
            .iter()
            .find(|target| target.target.ends_with("source.txt"))
            .unwrap()
            .exists
    );
    assert!(
        targets
            .iter()
            .find(|target| target.target.ends_with("doomed.txt"))
            .unwrap()
            .exists
    );
    assert_eq!(
        targets
            .iter()
            .find(|target| target.target.ends_with("destination.txt"))
            .unwrap()
            .sha256,
        Some(hex::encode(Sha256::digest(b"after\n")))
    );
}

#[tokio::test]
async fn a_restart_reuses_real_durable_intent_without_a_verifier_or_memory_ack() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "retained").await;
    let reopened_pool = Arc::new(
        Pool::open(&DbLocation::File(
            f.database_directory.path().join("state.db"),
        ))
        .unwrap(),
    );
    let reopened = FileInspector::open(reopened_pool, f.root.path()).unwrap();
    reopened
        .inspect_native(
            vec!["part".to_owned()],
            CYCLE.to_owned(),
            control_context(),
            f.lease().await,
        )
        .await
        .unwrap();
    assert!(!f.still_pending("part"));
}

#[tokio::test]
async fn a_modern_schema_and_tool_output_metadata_without_native_intent_are_unsupported() {
    let f = Fixture::new();
    let old = FileTools::with_formatter(f.root.path(), Arc::new(MoveAfterWrite)).unwrap();
    let input = json!({"filePath":f.root.path().join("legacy.txt"),"content":"legacy"});
    f.checkpoint("legacy", &old.write, input.clone());
    let error = old
        .write
        .invoke(input, mutation_context("legacy"))
        .await
        .unwrap_err();
    f.settle("legacy", &error);
    assert!(f.part("legacy").data.contains_key("toolSchemaIdentity"));
    let mut part = f.part("legacy");
    part.data["state"]["metadata"] =
        json!({"native.filesystem.intent":{"source":"native","verified":true}});
    MessageStore::new(&f.pool.get().unwrap())
        .put_part(&part)
        .unwrap();
    let result = f
        .inspect_pending(
            vec!["legacy".to_owned()],
            CYCLE.to_owned(),
            control_context(),
        )
        .await;
    assert!(matches!(
        result,
        Err(FileInspectionError::Unsupported { .. })
    ));
    assert!(f.still_pending("legacy"));
    assert!(f.events(INTENT_EVENT).is_empty());
}

#[tokio::test]
async fn shell_and_remote_calls_are_typed_unsupported() {
    for tool in ["shell", "aws", "deploy"] {
        let f = Fixture::new();
        f.write_uncertain("part", "file.txt", "bytes").await;
        let mut part = f.part("part");
        part.data["tool"] = json!(tool);
        part.data["state"]["uncertain"]["tool"] = json!(tool);
        MessageStore::new(&f.pool.get().unwrap())
            .put_part(&part)
            .unwrap();
        assert!(matches!(
            f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context())
                .await,
            Err(FileInspectionError::Unsupported { .. })
        ));
        assert!(f.still_pending("part"));
    }
}

#[tokio::test]
async fn existing_read_permission_denial_prevents_observation_and_marking() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    let mut ctx = control_context();
    ctx.permission = Arc::new(DenyAll);
    assert!(matches!(
        f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), ctx)
            .await,
        Err(FileInspectionError::Permission(_))
    ));
    assert!(f.still_pending("part"));
    assert!(f.events(INSPECTION_EVENT).is_empty());
}

#[tokio::test]
async fn changed_args_or_schema_cannot_adopt_another_native_intent() {
    for schema in [false, true] {
        let f = Fixture::new();
        f.write_uncertain("part", "file.txt", "bytes").await;
        let mut part = f.part("part");
        if schema {
            part.data["toolSchemaIdentity"]["schemaSha256"] = json!("different");
        } else {
            part.data["state"]["input"]["content"] = json!("different");
        }
        MessageStore::new(&f.pool.get().unwrap())
            .put_part(&part)
            .unwrap();
        assert!(matches!(
            f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context())
                .await,
            Err(FileInspectionError::Conflict(_))
        ));
        assert!(f.still_pending("part"));
    }
}

#[tokio::test]
async fn exact_cycle_and_turn_are_required_report_aliases_do_not_authorize_inspection() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    f.update_cycle(|cycle| cycle.active_turn_id = Some("later-turn".to_owned()));
    assert!(matches!(
        f.inspector
            .inspect_model(vec!["part".to_owned()], mutation_context("part"))
            .await,
        Err(FileInspectionError::Conflict(_))
    ));
    f.update_cycle(|cycle| {
        cycle.cycle_id = "replacement".to_owned();
        cycle.resumed_goal_cycles.insert(CYCLE.to_owned());
    });
    assert!(matches!(
        f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context())
            .await,
        Err(FileInspectionError::Conflict(_))
    ));
    assert!(f.still_pending("part"));
}

#[tokio::test]
async fn native_control_inspects_stopped_cycles_and_retained_last_turn_without_resuming() {
    for stopped in [false, true] {
        let f = Fixture::new();
        f.write_uncertain("part", "file.txt", "bytes").await;
        f.update_cycle(|cycle| {
            cycle.active_turn_id = Some("last-completed-turn".to_owned());
            if stopped {
                cycle.stopped = Some(CycleStop {
                    turn_id: Some(TURN.to_owned()),
                    input_id: None,
                    user_cancelled: true,
                    at_ms: now_millis(),
                });
            }
        });
        let before =
            zuno_db::session_work_cycle::current_in(&f.pool.get().unwrap(), SESSION).unwrap();
        let receipt = f
            .inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context())
            .await
            .expect("native guard owns the stopped/current scope");
        assert_eq!(receipt.actor, FileInspectionActor::NativeControl);
        assert_eq!(
            zuno_db::session_work_cycle::current_in(&f.pool.get().unwrap(), SESSION).unwrap(),
            before
        );
        assert!(!f.still_pending("part"));
        let event = &f.events(INSPECTION_EVENT)[0];
        assert_eq!(event.properties["scope"]["actor"]["kind"], "native_control");
        assert_eq!(
            event.properties["scope"]["fence"]["lastTurnId"],
            "last-completed-turn"
        );
    }
}

#[derive(Clone, Copy)]
enum FenceChange {
    Cycle,
    LastTurn,
    Stop,
}

struct ChangeFenceOnRead {
    pool: Arc<Pool>,
    change: FenceChange,
}

#[async_trait]
impl zuno_tool::PermissionAsker for ChangeFenceOnRead {
    async fn ask(
        &self,
        _: zuno_tool::PermissionOrigin<'_>,
        _: &str,
        _: zuno_tool::PermissionAsk,
    ) -> Result<(), ToolError> {
        self.pool
            .transaction(|tx| {
                let mut cycle = zuno_db::session_work_cycle::current_in(tx, SESSION)?.unwrap();
                match self.change {
                    FenceChange::Cycle => {
                        cycle.cycle_id = "new-cycle-during-inspection".to_owned();
                        let mut execution =
                            zuno_db::session_execution::read_in(tx, SESSION)?.unwrap();
                        execution.cycle_id = Some(cycle.cycle_id.clone());
                        zuno_db::session_execution::update_in(tx, execution.revision, execution)?;
                    }
                    FenceChange::LastTurn => {
                        cycle.active_turn_id = Some("new-last-turn".to_owned())
                    }
                    FenceChange::Stop => {
                        cycle.stopped = Some(CycleStop {
                            turn_id: Some(TURN.to_owned()),
                            input_id: Some("stop-during-inspection".to_owned()),
                            user_cancelled: true,
                            at_ms: now_millis(),
                        })
                    }
                }
                zuno_db::session_work_cycle::save_in(tx, &cycle, now_millis())
            })
            .map_err(|source| ToolError::Failed {
                tool: "test-read-permission".to_owned(),
                source: Box::new(source),
            })
    }
}

#[tokio::test]
async fn native_commit_rejects_each_changed_cycle_last_turn_or_stop_fence() {
    for change in [FenceChange::Cycle, FenceChange::LastTurn, FenceChange::Stop] {
        let f = Fixture::new();
        f.write_uncertain("part", "file.txt", "bytes").await;
        let before = f.part("part");
        let mut ctx = control_context();
        ctx.permission = Arc::new(ChangeFenceOnRead {
            pool: Arc::clone(&f.pool),
            change,
        });
        let result = f
            .inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), ctx)
            .await;
        assert!(matches!(result, Err(FileInspectionError::Conflict(_))));
        assert_eq!(f.part("part"), before);
        assert!(f.events(INSPECTION_EVENT).is_empty());
    }
}

#[tokio::test]
async fn model_actor_cannot_use_the_native_stopped_scope_exception() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    f.update_cycle(|cycle| {
        cycle.stopped = Some(CycleStop {
            turn_id: Some(TURN.to_owned()),
            input_id: None,
            user_cancelled: true,
            at_ms: now_millis(),
        })
    });
    assert!(matches!(
        f.inspector
            .inspect_model(vec!["part".to_owned()], mutation_context("part"))
            .await,
        Err(FileInspectionError::Conflict(_))
    ));
    assert!(matches!(
        f.inspect_pending(
            vec!["part".to_owned()],
            CYCLE.to_owned(),
            mutation_context("part")
        )
        .await,
        Err(FileInspectionError::Conflict(_))
    ));
    assert!(f.still_pending("part"));
}

#[tokio::test]
async fn a_native_lease_for_another_session_cannot_authorize_inspection() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    let wrong_guard = Arc::new(TestNativeLease {
        _exclusive: Arc::clone(&f.run_gate).lock_owned().await,
        session_id: "another-session",
    });
    assert!(matches!(
        f.inspector
            .inspect_native(
                vec!["part".to_owned()],
                CYCLE.to_owned(),
                control_context(),
                wrong_guard
            )
            .await,
        Err(FileInspectionError::Conflict(_))
    ));
    assert!(f.still_pending("part"));
    assert!(
        f.run_gate.try_lock().is_ok(),
        "completed rejection releases the lease"
    );
}

#[tokio::test]
async fn the_model_actor_requires_an_attempt_and_exact_unstopped_turn() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    assert!(matches!(
        f.inspector
            .inspect_model(vec!["part".to_owned()], control_context())
            .await,
        Err(FileInspectionError::Conflict(_))
    ));
    let result = f
        .inspector
        .inspect_model(vec!["part".to_owned()], mutation_context("part"))
        .await
        .expect("exact model fence");
    assert_eq!(
        result.actor,
        FileInspectionActor::Model {
            turn_id: TURN.to_owned()
        }
    );
    assert_eq!(
        f.events(INSPECTION_EVENT)[0].properties["scope"]["actor"]["kind"],
        "model"
    );
}

#[derive(Default)]
struct InspectionInterrupt {
    set: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

#[async_trait]
impl zuno_tool::InterruptHandle for InspectionInterrupt {
    fn is_set(&self) -> bool {
        self.set.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn notified(&self) {
        if !self.is_set() {
            self.notify.notified().await;
        }
    }
}

struct WaitingPermission(Arc<tokio::sync::Notify>);

#[async_trait]
impl zuno_tool::PermissionAsker for WaitingPermission {
    async fn ask(
        &self,
        _: zuno_tool::PermissionOrigin<'_>,
        _: &str,
        _: zuno_tool::PermissionAsk,
    ) -> Result<(), ToolError> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn native_cancellation_interrupts_permission_wait_and_releases_the_lease() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    let waiting = Arc::new(tokio::sync::Notify::new());
    let interrupt = Arc::new(InspectionInterrupt::default());
    let mut ctx = control_context();
    ctx.permission = Arc::new(WaitingPermission(Arc::clone(&waiting)));
    ctx.interrupt = interrupt.clone();
    let inspector = Arc::clone(&f.inspector);
    let lease = f.lease().await;
    let mut task = tokio::spawn(async move {
        inspector
            .inspect_native(vec!["part".to_owned()], CYCLE.to_owned(), ctx, lease)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), waiting.notified())
        .await
        .expect("permission wait started");
    interrupt
        .set
        .store(true, std::sync::atomic::Ordering::Release);
    interrupt.notify.notify_one();
    let settled = tokio::time::timeout(std::time::Duration::from_secs(1), &mut task).await;
    if settled.is_err() {
        task.abort();
        let _ = task.await;
        panic!("native cancellation must not leave permission waiting with the run lease held");
    }
    assert!(matches!(
        settled.unwrap().unwrap(),
        Err(FileInspectionError::Interrupted)
    ));
    assert!(f.run_gate.try_lock().is_ok());
    assert!(f.still_pending("part"));
    assert!(f.events(INSPECTION_EVENT).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_substitution_is_not_followed_or_marked_inspected() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    let other = tempfile::NamedTempFile::new().unwrap();
    std::fs::remove_file(f.root.path().join("file.txt")).unwrap();
    std::os::unix::fs::symlink(other.path(), f.root.path().join("file.txt")).unwrap();
    assert!(
        f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context())
            .await
            .is_err()
    );
    assert!(f.still_pending("part"));
    assert!(f.events(INSPECTION_EVENT).is_empty());
}

#[tokio::test]
async fn oversized_files_are_rejected_without_retiring_the_obligation() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    std::fs::File::options()
        .write(true)
        .open(f.root.path().join("file.txt"))
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    assert!(matches!(
        f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context())
            .await,
        Err(FileInspectionError::Bounds(_))
    ));
    assert!(f.still_pending("part"));
}

#[tokio::test]
async fn a_failed_marker_write_rolls_back_every_marker_and_the_inspection_event() {
    let f = Fixture::new();
    f.write_uncertain("first", "first.txt", "first").await;
    f.write_uncertain("second", "second.txt", "second").await;
    f.pool
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_second BEFORE UPDATE ON part WHEN NEW.id='second' \
         BEGIN SELECT RAISE(ABORT,'fixture failure'); END;",
        )
        .unwrap();
    assert!(
        f.inspect_pending(
            vec!["first".to_owned(), "second".to_owned()],
            CYCLE.to_owned(),
            control_context()
        )
        .await
        .is_err()
    );
    assert!(f.still_pending("first") && f.still_pending("second"));
    assert!(f.events(INSPECTION_EVENT).is_empty());
}

#[tokio::test]
async fn two_inspections_have_one_committed_receipt_and_never_replay_original_tools() {
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    let (first, second) = tokio::join!(
        f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context()),
        f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context()),
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(f.events(INTENT_EVENT).len(), 1);
    assert_eq!(f.events(INSPECTION_EVENT).len(), 1);
}

#[tokio::test]
async fn original_native_authorization_precedes_intent_and_any_file_effect() {
    let f = Fixture::new();
    let input = json!({"filePath":f.root.path().join("denied.txt"),"content":"denied"});
    f.checkpoint("denied", &f.tools.write, input.clone());
    let mut ctx = mutation_context("denied");
    ctx.permission = Arc::new(DenyAll);
    assert!(f.tools.write.invoke(input, ctx).await.is_err());
    assert!(!f.root.path().join("denied.txt").exists());
    assert!(f.events(INTENT_EVENT).is_empty());
}

#[tokio::test]
async fn composed_file_calls_keep_native_behavior_without_claiming_a_standalone_intent() {
    let f = Fixture::new();
    f.update_cycle(|cycle| cycle.active_turn_id = Some(TURN.to_owned()));
    let path = f.root.path().join("nested.txt");
    let context = mutation_context("outer").for_subcall("nested-file");
    let error = f
        .tools
        .write
        .invoke(json!({"filePath":path,"content":"nested bytes"}), context)
        .await
        .expect_err("real native final read loses the moved file");
    assert!(
        matches!(error, ToolError::Uncertain { .. }),
        "an unindexed composed subcall must not be blocked by the standalone intent recorder: {error:?}"
    );
    assert_eq!(
        std::fs::read(path.with_extension("uncertain-held")).unwrap(),
        b"nested bytes"
    );
    assert!(f.events(INTENT_EVENT).is_empty());
}

#[tokio::test]
async fn intent_persistence_failure_prevents_the_original_native_write() {
    let f = Fixture::new();
    let path = f.root.path().join("never-written.txt");
    let input = json!({"filePath":path,"content":"must not land"});
    f.checkpoint("part", &f.tools.write, input.clone());
    f.pool
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_intent BEFORE INSERT ON event \
         WHEN NEW.type='native.filesystem.intent.1' \
         BEGIN SELECT RAISE(ABORT,'intent failure'); END;",
        )
        .unwrap();
    assert!(
        f.tools
            .write
            .invoke(input, mutation_context("part"))
            .await
            .is_err()
    );
    assert!(!path.exists());
    assert!(!path.with_extension("uncertain-held").exists());
    assert!(f.events(INTENT_EVENT).is_empty());
}

#[tokio::test]
async fn inspection_records_evidence_without_resuming_the_execution_pause() {
    use zuno_types::execution::{SessionPauseReason, SessionReadiness, SessionScheduling};
    let f = Fixture::new();
    f.write_uncertain("part", "file.txt", "bytes").await;
    let store = zuno_db::session_execution::SessionExecutionStore::new(Arc::clone(&f.pool));
    let state = store.get(SESSION).unwrap().unwrap();
    let paused = store
        .set_scheduling(
            SESSION,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::Paused {
                    reason: SessionPauseReason::Authentication,
                },
                ..Default::default()
            },
            now_millis(),
        )
        .unwrap();
    f.inspect_pending(vec!["part".to_owned()], CYCLE.to_owned(), control_context())
        .await
        .unwrap();
    assert_eq!(store.get(SESSION).unwrap().unwrap(), paused);
    assert!(!f.still_pending("part"));
}
