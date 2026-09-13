//! Native file-state inspection for exact uncertain calls.
//!
//! The producer runs inside the concrete write/edit/apply_patch implementations,
//! after authorization and before effects. It records resolved targets in the
//! host event log, not in arbitrary ToolOutput metadata. The inspector reads the
//! actual files and commits observations with exact part markers in one transaction.
//! It neither resumes execution nor changes the original uncertain outcome.

#[path = "uncertain_fs.rs"]
mod fs;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use zuno_db::Pool;
use zuno_db::event_log::{NewSessionEvent, append_in};
use zuno_db::message::{MessageRole, MessageStore, PartKind, PartRecord, now_millis};
use zuno_db::session_work_cycle::CycleStop;
use zuno_error::{DbError, ToolError};
use zuno_orchestration::{ToolSchemaIdentity, sha256_json};
use zuno_tool::{PermissionAsk, ToolContext, ToolEffect};

pub use fs::{
    FileMetadata, FileObservation, MAX_FILE_BYTES, MAX_TARGETS, MAX_TOTAL_BYTES, READ_DEADLINE,
    WorkspaceIdentity,
};
pub const INTENT_EVENT: &str = "native.filesystem.intent";
pub const INSPECTION_EVENT: &str = "native.filesystem.inspection";
pub const MAX_PARTS: usize = 16;
const MAX_PART_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FileInspectionError {
    #[error("unsupported file inspection ({part_id:?}): {reason}")]
    Unsupported {
        part_id: Option<String>,
        reason: String,
    },
    #[error("file inspection conflict: {0}")]
    Conflict(String),
    #[error("file inspection limit: {0}")]
    Bounds(String),
    #[error("file inspection interrupted")]
    Interrupted,
    #[error("file inspection read deadline expired")]
    Timeout,
    #[error("inspection worker failed; inspect durable state before any retry: {0}")]
    Worker(String),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Permission(#[from] ToolError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeFileKind {
    Write,
    Edit,
    ApplyPatch,
}

impl NativeFileKind {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Edit => "edit",
            Self::ApplyPatch => "apply_patch",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            "write" => Some(Self::Write),
            "edit" => Some(Self::Edit),
            "apply_patch" => Some(Self::ApplyPatch),
            _ => None,
        }
    }
}

fn normalized_input(kind: NativeFileKind, mut input: Value) -> Result<Value, FileInspectionError> {
    zuno_tool::guard::strip_cross_cutting(&mut input);
    Ok(match kind {
        NativeFileKind::Write => {
            serde_json::to_value(serde_json::from_value::<crate::write::WriteParams>(input)?)?
        }
        NativeFileKind::Edit => {
            serde_json::to_value(serde_json::from_value::<crate::edit::EditParams>(input)?)?
        }
        NativeFileKind::ApplyPatch => serde_json::to_value(serde_json::from_value::<
            crate::apply_patch::ApplyPatchParams,
        >(input)?)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileIntent {
    part_id: String,
    session_id: String,
    message_id: String,
    call_id: String,
    turn_id: String,
    cycle_id: String,
    kind: NativeFileKind,
    tool_schema: ToolSchemaIdentity,
    input_sha256: String,
    workspace: WorkspaceIdentity,
    targets: Vec<PathBuf>,
    recorded_at_ms: i64,
}

/// Only the concrete native implementations can publish an intent through this
/// type. There is no public method accepting a caller's claimed proof.
pub struct NativeFileIntentRecorder {
    database: Arc<Pool>,
    workspace: Arc<fs::Workspace>,
    schemas: BTreeMap<String, ToolSchemaIdentity>,
}

impl NativeFileIntentRecorder {
    pub(crate) fn record<T: Serialize>(
        &self,
        kind: NativeFileKind,
        params: &T,
        targets: Vec<PathBuf>,
        ctx: &ToolContext,
    ) -> Result<(), ToolError> {
        // A composed subcall is retained inside its parent's result, not as an
        // independent engine part. Preserve its normal behavior; the parent
        // operation remains outside this inspector's standalone-file domain.
        if ctx.depth > 0 {
            return Ok(());
        }
        // Unsupported platforms preserve ordinary file-tool behavior. They never
        // emit an inspection witness or advertise a successful inspection backend.
        if !self.workspace.supported() {
            return Ok(());
        }
        let result = self.record_inner(kind, params, targets, ctx);
        result.map_err(|source| ToolError::Failed {
            tool: kind.id().to_owned(),
            source: Box::new(source),
        })
    }

    fn record_inner<T: Serialize>(
        &self,
        kind: NativeFileKind,
        params: &T,
        targets: Vec<PathBuf>,
        ctx: &ToolContext,
    ) -> Result<(), FileInspectionError> {
        self.workspace.revalidate()?;
        // External paths remain governed by the original tool's permissions, but
        // cannot obtain a witness for this workspace-only inspector.
        if targets
            .iter()
            .any(|target| self.workspace.relative(target).is_err())
        {
            return Ok(());
        }
        let targets = targets
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if targets.is_empty() || targets.len() > fs::MAX_TARGETS {
            return Ok(());
        }
        let snapshot = ctx
            .orchestration_snapshot()
            .ok_or_else(|| conflict("native intent requires the actual immutable Attempt"))?;
        let origin = ctx.permission_origin();
        if origin.session_id() != snapshot.owner.session_id
            || ctx.session_id != origin.session_id()
            || ctx.message_id != origin.message_id()
            || ctx.call_id != origin.call_id()
        {
            return Err(conflict("native invocation coordinates disagree"));
        }
        let cycle_id = snapshot
            .cycle_id
            .as_deref()
            .ok_or_else(|| conflict("native intent has no work cycle"))?;
        let scope = InspectionScope {
            session_id: origin.session_id().to_owned(),
            cycle_id: cycle_id.to_owned(),
            actor: FileInspectionActor::Model {
                turn_id: snapshot.turn_id.clone(),
            },
            fence: None,
        };
        let input_sha256 = sha256_json(&serde_json::to_value(params)?);
        let schema = self
            .schemas
            .get(kind.id())
            .expect("native factory supplies every schema");
        self.database.try_transaction(|tx| {
            scope.validate(tx)?;
            self.validate_session_workspace(tx, &scope.session_id)?;
            let mut query = tx
                .prepare(
                    "SELECT id FROM part WHERE session_id=?1 AND message_id=?2 \
                 AND json_extract(data,'$.callID')=?3 LIMIT 2",
                )
                .map_err(zuno_db::open::map_error)?;
            let ids = query
                .query_map(
                    (origin.session_id(), origin.message_id(), origin.call_id()),
                    |row| row.get::<_, String>(0),
                )
                .map_err(zuno_db::open::map_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(zuno_db::open::map_error)?;
            if ids.len() != 1 {
                return Err(conflict("native intent requires one checkpointed part"));
            }
            let store = MessageStore::new(tx);
            let part = store.part(&ids[0])?;
            validate_owner(&store, &part, origin.session_id())?;
            let state = part
                .data
                .get("state")
                .ok_or_else(|| conflict("missing tool state"))?;
            if part.data.get("tool").and_then(Value::as_str) != Some(kind.id())
                || state["status"] != "pending"
                || state
                    .get("dispatchedAtMs")
                    .and_then(Value::as_i64)
                    .is_none()
                || part.data.get("toolSchemaIdentity") != Some(&serde_json::to_value(schema)?)
                || sha256_json(&normalized_input(kind, state["input"].clone())?) != input_sha256
            {
                return Err(conflict(
                    "native intent does not match the actual dispatched part",
                ));
            }
            if find_intent(tx, origin.session_id(), &part.id)?.is_some() {
                return Err(conflict(
                    "this native invocation already has an intent; do not replay it",
                ));
            }
            let intent = FileIntent {
                part_id: part.id,
                session_id: scope.session_id.clone(),
                message_id: origin.message_id().to_owned(),
                call_id: origin.call_id().to_owned(),
                turn_id: snapshot.turn_id.clone(),
                cycle_id: cycle_id.to_owned(),
                kind,
                tool_schema: schema.clone(),
                input_sha256,
                workspace: self.workspace.identity.clone(),
                targets,
                recorded_at_ms: now_millis(),
            };
            append_in(tx, &scope.session_id, event(INTENT_EVENT, &intent)?)?;
            Ok(())
        })
    }

    fn validate_session_workspace(
        &self,
        connection: &Connection,
        session_id: &str,
    ) -> Result<(), FileInspectionError> {
        validate_session_workspace(connection, session_id, &self.workspace.identity)
    }
}

/// The native adapter owns an actual exclusive SessionRunGuard. This interface
/// avoids an engine dependency and must not be implemented by model arguments or
/// a no-op production token. Its Arc is retained through the commit worker.
pub trait NativeInspectionGuard: Send + Sync + 'static {
    fn session_id(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileInspectionActor {
    NativeControl,
    Model {
        #[serde(rename = "turnId")]
        turn_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct CycleFence {
    cycle_id: String,
    last_turn_id: Option<String>,
    stopped: Option<CycleStop>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct InspectionScope {
    session_id: String,
    cycle_id: String,
    actor: FileInspectionActor,
    fence: Option<CycleFence>,
}

impl InspectionScope {
    fn native(
        expected_cycle_id: &str,
        ctx: &ToolContext,
        guard: &dyn NativeInspectionGuard,
    ) -> Result<Self, FileInspectionError> {
        if expected_cycle_id.trim().is_empty() {
            return Err(conflict("inspection needs the expected current cycle"));
        }
        let origin = ctx.permission_origin();
        if ctx.orchestration_snapshot().is_some() || guard.session_id() != origin.session_id() {
            return Err(conflict(
                "native inspection requires the owning native lease and no provider Attempt",
            ));
        }
        Ok(Self {
            session_id: origin.session_id().to_owned(),
            cycle_id: expected_cycle_id.to_owned(),
            actor: FileInspectionActor::NativeControl,
            fence: None,
        })
    }

    fn model(ctx: &ToolContext) -> Result<Self, FileInspectionError> {
        let snapshot = ctx
            .orchestration_snapshot()
            .ok_or_else(|| conflict("model inspection requires an immutable Attempt"))?;
        let cycle_id = snapshot
            .cycle_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| conflict("model inspection has no cycle"))?;
        if snapshot.owner.session_id != ctx.permission_origin().session_id()
            || snapshot.turn_id.trim().is_empty()
        {
            return Err(conflict(
                "model inspection Attempt does not match its owner",
            ));
        }
        Ok(Self {
            session_id: snapshot.owner.session_id.clone(),
            cycle_id: cycle_id.to_owned(),
            actor: FileInspectionActor::Model {
                turn_id: snapshot.turn_id.clone(),
            },
            fence: None,
        })
    }

    fn current_fence(&self, connection: &Connection) -> Result<CycleFence, FileInspectionError> {
        let cycle = zuno_db::session_work_cycle::current_in(connection, &self.session_id)?
            .ok_or_else(|| conflict("inspection has no current work cycle"))?;
        if cycle.cycle_id != self.cycle_id {
            return Err(conflict(
                "inspection cycle changed; report aliases grant no authority",
            ));
        }
        if let FileInspectionActor::Model { turn_id } = &self.actor
            && (cycle.stopped.is_some() || cycle.active_turn_id.as_ref() != Some(turn_id))
        {
            return Err(conflict(
                "model inspection turn changed or its cycle stopped",
            ));
        }
        Ok(CycleFence {
            cycle_id: cycle.cycle_id,
            last_turn_id: cycle.active_turn_id,
            stopped: cycle.stopped,
        })
    }

    fn validate(&self, connection: &Connection) -> Result<(), FileInspectionError> {
        let current = self.current_fence(connection)?;
        if self
            .fence
            .as_ref()
            .is_some_and(|expected| expected != &current)
        {
            return Err(conflict("inspection cycle/last-turn/stop snapshot changed"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedFileCall {
    pub part_id: String,
    pub call_id: String,
    pub intent_event_id: String,
    pub targets: Vec<FileObservation>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInspectionReceipt {
    pub event_id: String,
    pub session_id: String,
    pub cycle_id: String,
    pub started_at_ms: i64,
    pub observed_at_ms: i64,
    pub recorded_at_ms: i64,
    pub source: &'static str,
    pub actor: FileInspectionActor,
    pub calls: Vec<InspectedFileCall>,
}

/// A real native service. No model-supplied receipt or verifier is accepted.
#[derive(Clone)]
pub struct FileInspector {
    database: Arc<Pool>,
    workspace: Arc<fs::Workspace>,
}

struct PreparedCall {
    part: PartRecord,
    intent: FileIntent,
    intent_event_id: String,
}
struct PreparedInspection {
    scope: InspectionScope,
    calls: Vec<PreparedCall>,
    _native_guard: Option<Arc<dyn NativeInspectionGuard>>,
}

impl FileInspector {
    /// Open a service over the configured workspace, including after a restart.
    /// This does not activate models, clear gates, or inspect a file's contents.
    pub fn open(database: Arc<Pool>, workspace: &Path) -> Result<Self, FileInspectionError> {
        Ok(Self {
            database,
            workspace: Arc::new(fs::Workspace::open(workspace)?),
        })
    }

    pub fn supported(&self) -> bool {
        self.workspace.supported()
    }

    pub(crate) fn recorder(
        &self,
        schemas: impl IntoIterator<Item = ToolSchemaIdentity>,
    ) -> Arc<NativeFileIntentRecorder> {
        Arc::new(NativeFileIntentRecorder {
            database: Arc::clone(&self.database),
            workspace: Arc::clone(&self.workspace),
            schemas: schemas
                .into_iter()
                .map(|schema| (schema.name.clone(), schema))
                .collect(),
        })
    }

    /// Inspect under the native caller's exclusive session lease. A stopped cycle
    /// and its retained last-turn fence may be inspected, never cleared or resumed.
    /// There is no fabricated provider Attempt; the native actor is recorded.
    pub async fn inspect_native(
        &self,
        part_ids: Vec<String>,
        expected_cycle_id: String,
        ctx: ToolContext,
        guard: Arc<dyn NativeInspectionGuard>,
    ) -> Result<FileInspectionReceipt, FileInspectionError> {
        let scope = InspectionScope::native(&expected_cycle_id, &ctx, guard.as_ref())?;
        self.inspect_scoped(part_ids, ctx, scope, Some(guard)).await
    }

    /// Model-originated callers retain strict current-turn and non-stopped-cycle
    /// authority. This API is not a model tool registration.
    pub async fn inspect_model(
        &self,
        part_ids: Vec<String>,
        ctx: ToolContext,
    ) -> Result<FileInspectionReceipt, FileInspectionError> {
        let scope = InspectionScope::model(&ctx)?;
        self.inspect_scoped(part_ids, ctx, scope, None).await
    }

    async fn inspect_scoped(
        &self,
        part_ids: Vec<String>,
        ctx: ToolContext,
        scope: InspectionScope,
        guard: Option<Arc<dyn NativeInspectionGuard>>,
    ) -> Result<FileInspectionReceipt, FileInspectionError> {
        if ctx.interrupt.is_set() {
            return Err(FileInspectionError::Interrupted);
        }
        if !self.supported() {
            return Err(unsupported(
                None,
                "no bounded native file inspector on this platform",
            ));
        }
        if part_ids.is_empty()
            || part_ids.len() > MAX_PARTS
            || part_ids
                .iter()
                .any(|id| id.trim().is_empty() || id.len() > 512)
            || part_ids.iter().collect::<BTreeSet<_>>().len() != part_ids.len()
        {
            return Err(FileInspectionError::Bounds(
                "provide 1..=16 distinct part IDs".to_owned(),
            ));
        }
        let service = self.clone();
        let prepared =
            tokio::task::spawn_blocking(move || service.prepare(scope, &part_ids, guard))
                .await
                .map_err(worker)??;
        let targets = prepared
            .calls
            .iter()
            .flat_map(|call| call.intent.targets.iter().cloned())
            .collect::<BTreeSet<_>>();
        if targets.len() > fs::MAX_TARGETS {
            return Err(FileInspectionError::Bounds(
                "inspection contains more than 64 targets".to_owned(),
            ));
        }
        for target in &targets {
            if ctx.interrupt.is_set() {
                return Err(FileInspectionError::Interrupted);
            }
            let ask = PermissionAsk::new(
                "read",
                zuno_paths::wire_path(&self.workspace.relative(target)?),
            )
            .with_tool_effect(ToolEffect::ReadOnly);
            // Native controls do not pass through the model dispatcher's
            // interrupt/select boundary. A hook or human approval may wait
            // indefinitely, so race the same context signal here as well.
            tokio::select! {
                biased;
                () = ctx.interrupt.notified() => return Err(FileInspectionError::Interrupted),
                result = ctx.ask("uncertain_inspect", ask) => result?,
            }
        }
        if ctx.interrupt.is_set() {
            return Err(FileInspectionError::Interrupted);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelRead(Arc::clone(&cancelled));
        let control = fs::ReadControl {
            cancelled,
            interrupt: Arc::clone(&ctx.interrupt),
            deadline: Instant::now() + fs::READ_DEADLINE,
        };
        let root = Arc::clone(&self.workspace);
        let started_at_ms = now_millis();
        let read = tokio::task::spawn_blocking(move || {
            let mut remaining = fs::MAX_TOTAL_BYTES;
            targets
                .into_iter()
                .map(|target| {
                    let observation = root.observe(&target, &control, &mut remaining)?;
                    Ok((target, observation))
                })
                .collect::<Result<BTreeMap<_, _>, FileInspectionError>>()
        });
        let observations = tokio::time::timeout(fs::READ_DEADLINE, read)
            .await
            .map_err(|_| FileInspectionError::Timeout)?
            .map_err(worker)??;
        drop(cancel_on_drop);
        let observed_at_ms = now_millis();
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            service.commit(prepared, observations, started_at_ms, observed_at_ms, &ctx)
        })
        .await
        .map_err(worker)?
    }

    fn prepare(
        &self,
        mut scope: InspectionScope,
        ids: &[String],
        guard: Option<Arc<dyn NativeInspectionGuard>>,
    ) -> Result<PreparedInspection, FileInspectionError> {
        let connection = self.database.get()?;
        scope.fence = Some(scope.current_fence(&connection)?);
        validate_session_workspace(&connection, &scope.session_id, &self.workspace.identity)?;
        let store = MessageStore::new(&connection);
        let mut calls = Vec::new();
        for id in ids {
            let length: i64 = connection
                .query_row(
                    "SELECT length(CAST(data AS BLOB)) FROM part WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .map_err(zuno_db::open::map_error)?;
            if length < 0 || length as usize > MAX_PART_BYTES {
                return Err(FileInspectionError::Bounds(
                    "tool part exceeds the inspection limit".to_owned(),
                ));
            }
            let part = store.part(id)?;
            validate_owner(&store, &part, &scope.session_id)?;
            let kind = part
                .data
                .get("tool")
                .and_then(Value::as_str)
                .and_then(NativeFileKind::from_id)
                .ok_or_else(|| {
                    unsupported(
                        Some(id),
                        "shell/remote and non-native file operations have no inspector",
                    )
                })?;
            let state = &part.data["state"];
            if state["outcome"] != "uncertain"
                || !matches!(state["status"].as_str(), Some("error" | "completed"))
                || !state["uncertain"]["reconciledAtMs"].is_null()
            {
                return Err(conflict("requested part is not pending uncertain"));
            }
            let (intent_event_id, intent) = find_intent(&connection, &scope.session_id, id)?
                .ok_or_else(|| {
                    unsupported(
                        Some(id),
                        "no host-owned native filesystem intent; a schema or path is not proof",
                    )
                })?;
            if intent.part_id != part.id
                || intent.session_id != part.session_id
                || intent.message_id != part.message_id
                || intent.kind != kind
                || part.data["callID"].as_str() != Some(&intent.call_id)
                || state["uncertain"]["callID"].as_str() != Some(&intent.call_id)
                || state["uncertain"]["tool"].as_str() != Some(kind.id())
                || part.data.get("toolSchemaIdentity")
                    != Some(&serde_json::to_value(&intent.tool_schema)?)
                || sha256_json(&normalized_input(kind, state["input"].clone())?)
                    != intent.input_sha256
                || intent.workspace != self.workspace.identity
                || intent.targets.is_empty()
                || intent.targets.len() > fs::MAX_TARGETS
            {
                return Err(conflict(
                    "native intent does not match this exact uncertain invocation",
                ));
            }
            let observed = state["uncertain"]["observedAtMs"]
                .as_i64()
                .ok_or_else(|| conflict("uncertain observation time is absent"))?;
            if observed < intent.recorded_at_ms || observed > now_millis() {
                return Err(conflict(
                    "uncertain observation does not follow its native intent",
                ));
            }
            let applied = state["uncertain"]["appliedPaths"]
                .as_array()
                .ok_or_else(|| conflict("uncertain appliedPaths is malformed"))?;
            for path in applied {
                let path = path
                    .as_str()
                    .ok_or_else(|| conflict("invalid applied path"))?;
                if !intent
                    .targets
                    .iter()
                    .any(|target| zuno_paths::wire_path(target) == path)
                {
                    return Err(conflict(
                        "reported applied path is outside the native target set",
                    ));
                }
            }
            for target in &intent.targets {
                self.workspace.relative(target)?;
            }
            calls.push(PreparedCall {
                part,
                intent,
                intent_event_id,
            });
        }
        Ok(PreparedInspection {
            scope,
            calls,
            _native_guard: guard,
        })
    }

    fn commit(
        &self,
        prepared: PreparedInspection,
        observations: BTreeMap<PathBuf, FileObservation>,
        started_at_ms: i64,
        observed_at_ms: i64,
        ctx: &ToolContext,
    ) -> Result<FileInspectionReceipt, FileInspectionError> {
        self.database.try_transaction(|tx| {
            if ctx.interrupt.is_set() { return Err(FileInspectionError::Interrupted); }
            prepared.scope.validate(tx)?;
            validate_session_workspace(tx, &prepared.scope.session_id, &self.workspace.identity)?;
            self.workspace.revalidate()?;
            let store = MessageStore::new(tx);
            for call in &prepared.calls {
                if store.part(&call.part.id)? != call.part
                    || find_intent(tx, &prepared.scope.session_id, &call.part.id)?
                        != Some((call.intent_event_id.clone(), call.intent.clone()))
                {
                    return Err(conflict("uncertain invocation changed during inspection"));
                }
            }
            let calls = prepared.calls.iter().map(|call| InspectedFileCall {
                part_id: call.part.id.clone(), call_id: call.intent.call_id.clone(),
                intent_event_id: call.intent_event_id.clone(),
                targets: call.intent.targets.iter().map(|path| observations[path].clone()).collect(),
            }).collect();
            let mut receipt = FileInspectionReceipt {
                event_id: String::new(), session_id: prepared.scope.session_id.clone(),
                cycle_id: prepared.scope.cycle_id.clone(), started_at_ms, observed_at_ms,
                recorded_at_ms: now_millis(), source: "native_file_state",
                actor: prepared.scope.actor.clone(), calls,
            };
            let mut receipt_data = serde_json::to_value(&receipt)?;
            receipt_data.as_object_mut().expect("receipt object").remove("eventId");
            let recorded = append_in(tx, &prepared.scope.session_id, event(INSPECTION_EVENT, &json!({
                "receipt": receipt_data,
                "scope": prepared.scope,
                "originalParts": prepared.calls.iter().map(|call| call.part.to_json()).collect::<Vec<_>>(),
                "intents": prepared.calls.iter().map(|call| &call.intent).collect::<Vec<_>>(),
                "originalOutcome": "uncertain", "replayAuthorized": false,
            }))?)?;
            receipt.event_id = recorded.id;
            for call in prepared.calls {
                let mut part = call.part;
                let uncertain = part.data.get_mut("state")
                    .and_then(|state| state.get_mut("uncertain"))
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| conflict("uncertain state changed"))?;
                uncertain.insert("reconciledAtMs".to_owned(), json!(receipt.recorded_at_ms));
                uncertain.insert("inspection".to_owned(), json!({
                    "eventID": receipt.event_id, "source": receipt.source,
                    "observedAtMs": receipt.observed_at_ms,
                }));
                store.put_part_at(&part, receipt.recorded_at_ms)?;
            }
            Ok(receipt)
        })
    }
}

struct CancelRead(Arc<AtomicBool>);
impl Drop for CancelRead {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn validate_owner(
    store: &MessageStore<'_>,
    part: &PartRecord,
    session: &str,
) -> Result<(), FileInspectionError> {
    let message = store.message(&part.message_id)?;
    if part.kind != PartKind::Tool
        || part.session_id != session
        || message.session_id != session
        || message.role != MessageRole::Assistant
    {
        return Err(conflict(
            "part must belong to this session's assistant message",
        ));
    }
    Ok(())
}

fn validate_session_workspace(
    connection: &Connection,
    session_id: &str,
    workspace: &WorkspaceIdentity,
) -> Result<(), FileInspectionError> {
    let session = zuno_db::session::get(connection, session_id)?;
    if Path::new(&session.directory).canonicalize()? != workspace.path {
        return Err(conflict(
            "session directory does not match the native workspace",
        ));
    }
    Ok(())
}

fn find_intent(
    connection: &Connection,
    session: &str,
    part: &str,
) -> Result<Option<(String, FileIntent)>, FileInspectionError> {
    let mut query = connection.prepare(
        "SELECT id, data FROM event WHERE aggregate_id=?1 AND type='native.filesystem.intent.1' \
         AND json_extract(data,'$.partId')=?2 LIMIT 2",
    ).map_err(zuno_db::open::map_error)?;
    let entries = query
        .query_map((session, part), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(zuno_db::open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(zuno_db::open::map_error)?;
    if entries.len() > 1 {
        return Err(conflict("ambiguous native intent"));
    }
    entries
        .into_iter()
        .next()
        .map(|(id, data)| Ok((id, serde_json::from_str(&data)?)))
        .transpose()
}

fn event(kind: &str, value: &impl Serialize) -> Result<NewSessionEvent, FileInspectionError> {
    let value = serde_json::to_value(value)?;
    Ok(NewSessionEvent::new(
        kind,
        value
            .as_object()
            .expect("native event is an object")
            .clone(),
    )?)
}

fn conflict(message: &str) -> FileInspectionError {
    FileInspectionError::Conflict(message.to_owned())
}
fn unsupported(part: Option<&str>, message: &str) -> FileInspectionError {
    FileInspectionError::Unsupported {
        part_id: part.map(str::to_owned),
        reason: message.to_owned(),
    }
}
fn worker(error: tokio::task::JoinError) -> FileInspectionError {
    FileInspectionError::Worker(error.to_string())
}
