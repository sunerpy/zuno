//! The production [`ChildTurnHost`]: delegation to a real child session.
//!
//! # What was missing, and what this is
//!
//! [`zuno_tools::task::TaskTool`] — every refusal, the two-measure depth guard, the
//! model precedence ladder — was complete and tested, and `BuiltinSlot::Task` was
//! never registered, because [`ChildTurnHost`] had no production implementation. The
//! only one in the tree was `RecordingHost`, a test double. So the model was told no
//! delegation tool existed and every `zuno-tools` test passed, forever.
//!
//! This is that implementation. It creates the child session, drives its turn through
//! the same [`TurnPlan`]/[`TurnHost`] pair every surface uses, and hands back the
//! child's final assistant text.
//!
//! # Why a fresh host per delegation rather than a seam into the parent's
//!
//! [`zuno_engine::r#loop::run_turn`] takes `&mut Connection`, a provider registry and
//! **the dispatcher that is calling this tool** — so a tool cannot borrow its own
//! caller's turn context, which is exactly why `zuno-tools` states this contract and
//! satisfies it nowhere. A child is therefore composed from scratch: its own
//! connection, its own agent, its own model, its own permission-filtered tool set.
//!
//! That is not a workaround, it is the semantics. A subagent runs *as* its agent —
//! `plan`'s restrictions or `worker`'s narrower roster apply to the child and not to
//! the parent — and re-resolving is the only thing that produces that. It costs one
//! config and catalog resolution per delegation, paid once per child rather than per
//! step.
//!
//! Nesting is safe by construction: this host builds no host, it builds a
//! [`TurnHost`], whose own `task` tool gets its own copy of this host. Construction
//! does not recurse; only dispatch does, and dispatch is depth-guarded before it
//! reaches here.
//!
//! # Concurrency against the parent's live turn
//!
//! The child opens a second connection to the same database while the parent's turn
//! holds one. Sound because `zuno-db` opens every connection `WAL` with
//! `busy_timeout = 5000` (`zuno-db/src/open.rs:19-21`), and because the parent holds
//! no transaction across tool dispatch — `run_turn`'s only transaction is the
//! `touch_session` at `loop.rs:1135-1137`, which commits before any tool runs.
//!
//! # Background dispatch is refused, and why that is the honest answer
//!
//! `background: true` is [`ChildTurnError::Host`] naming what is absent. Returning a
//! job id would be worse than refusing: the tool's own description promises the caller
//! is "notified on completion", and there is no notification path —
//! [`zuno_agent::continuation::JobBoard`] models exactly this and has no production
//! caller, `SessionRunRegistry` tracks busy session ids with no agent or objective,
//! and nothing persists a running job across a restart. A caller holding an id that
//! can never be resolved, for work whose completion is never reported, is a silent
//! failure; a refusal that names the gap is one the model can act on by running the
//! work in the foreground.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;
use zuno_engine::r#loop::event_channel;
use zuno_tool::PermissionAsker;
use zuno_tools::task::{ChildTurn, ChildTurnError, ChildTurnHost, ChildTurnRequest};

use super::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan};
use crate::environment::StartupEnvironment;

/// How deep a delegation chain may be walked before the walk is called cyclic.
///
/// `session.parent_id` has no foreign key, so nothing in the schema prevents an
/// `a -> b -> a` pair; `zuno-db`'s own subtree walk keeps a visited set for the same
/// reason. A bound is enough here because any real chain is bounded by
/// `subagent_depth`, which is single digits.
const MAX_ANCESTRY_WALK: u32 = 64;

/// Delegation backed by a real child session and a real turn.
pub(crate) struct ChildSessionHost {
    environment: StartupEnvironment,
    directory: PathBuf,
    approval: Arc<dyn PermissionAsker>,
    /// Where the session database is, resolved once.
    ///
    /// Held rather than recomputed per call so every connection this host opens
    /// answers from the same database the parent turn is writing to, even if the
    /// process environment changes underneath it mid-turn.
    database: zuno_paths::DbLocation,
}

impl ChildSessionHost {
    pub(crate) fn new(
        environment: StartupEnvironment,
        directory: PathBuf,
        approval: Arc<dyn PermissionAsker>,
    ) -> Self {
        let database = zuno_paths::Layout::resolve(environment.resolved()).db_path();
        Self {
            environment,
            directory,
            approval,
            database,
        }
    }

    /// Open a connection of this host's own.
    ///
    /// Not the parent's: `run_turn` holds that one mutably for the whole turn, and a
    /// tool has no way to reach it. See the module docs on why that is sound.
    fn connect(&self) -> Result<rusqlite::Connection, ChildTurnError> {
        zuno_db::open::open(&self.database).map_err(|error| ChildTurnError::Host(error.to_string()))
    }

    /// The child session to run in: `task_id`'s, or a fresh one.
    ///
    /// A resumed session must be a child **of this parent**. Accepting any session id
    /// would let one delegation continue another session's child, which is both a
    /// confusing transcript and a way to write into a session the caller was never
    /// given.
    fn session_for(&self, request: &ChildTurnRequest) -> Result<String, ChildTurnError> {
        let mut connection = self.connect()?;
        if let Some(resume) = &request.resume_session_id {
            let existing = zuno_db::session::get(&connection, resume)
                .map_err(|_error| ChildTurnError::UnknownSession(resume.clone()))?;
            if existing.parent_id.as_deref() != Some(request.parent_session_id.as_str()) {
                return Err(ChildTurnError::UnknownSession(resume.clone()));
            }
            return Ok(existing.id);
        }

        let parent = zuno_db::session::get(&connection, &request.parent_session_id)
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        let child_id = crate::cmd::turn::prefixed_id("ses");
        let title = request
            .description
            .clone()
            .unwrap_or_else(|| format!("Delegated to {}", request.agent));
        let mut input = zuno_db::session::SessionCreate::new(
            &child_id,
            Uuid::new_v4().simple().to_string(),
            &parent.project_id,
            parent.directory.clone(),
            parent.directory.clone(),
            title,
            crate::COMPATIBILITY_VERSION,
        )
        .with_parent(&request.parent_session_id);
        input.agent = Some(request.agent.clone());
        if let Some(workspace) = parent.workspace_id.clone() {
            input = input.with_workspace(workspace);
        }
        let transaction = connection
            .transaction()
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        zuno_db::session::create(&transaction, &input)
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        transaction
            .commit()
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        Ok(child_id)
    }

    /// The text the child ended on, which is the whole point of a foreground call.
    ///
    /// The last assistant message's text parts, in part order. Reasoning and tool
    /// parts are excluded: the parent asked for an answer, and a subagent's tool
    /// traffic is precisely the context delegation exists to keep out of the parent.
    fn answer(&self, session_id: &str) -> Result<String, ChildTurnError> {
        let connection = self.connect()?;
        let store = zuno_db::message::MessageStore::new(&connection);
        let messages = store
            .messages_for_session(session_id)
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        let Some(last) = messages
            .iter()
            .rev()
            .find(|message| message.role == zuno_db::message::MessageRole::Assistant)
        else {
            return Ok(String::new());
        };
        let parts = store
            .parts_by_message_kind(
                std::slice::from_ref(&last.id),
                zuno_db::message::PartKind::Text,
            )
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        let text = parts
            .get(&last.id)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.data.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        Ok(text)
    }
}

#[async_trait]
impl ChildTurnHost for ChildSessionHost {
    async fn delegation_depth(&self, session_id: &str) -> Result<u32, ChildTurnError> {
        let connection = self.connect()?;
        let mut depth = 0_u32;
        let mut current = session_id.to_owned();
        while depth < MAX_ANCESTRY_WALK {
            let session = zuno_db::session::get(&connection, &current)
                .map_err(|error| ChildTurnError::Host(error.to_string()))?;
            match session.parent_id {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => return Ok(depth),
            }
        }
        Err(ChildTurnError::Host(format!(
            "session `{session_id}` has more than {MAX_ANCESTRY_WALK} ancestors, which \
             means its `parent_id` chain contains a cycle; delegation depth cannot be \
             established"
        )))
    }

    async fn dispatch(&self, request: ChildTurnRequest) -> Result<ChildTurn, ChildTurnError> {
        if request.background {
            return Err(ChildTurnError::Host(
                "background delegation is not available in this build: nothing tracks a \
                 running subagent job or reports its completion, so a job id would name \
                 work you could never collect. Drop `background` to run this delegation \
                 in the foreground and receive its result directly."
                    .to_owned(),
            ));
        }

        let session_id = self.session_for(&request)?;
        let options = TurnOptions {
            directory: Some(self.directory.clone()),
            model: request.model.as_ref().map(|model| model.model.clone()),
            agent: Some(request.agent.clone()),
            session: SessionChoice::Existing(session_id.clone()),
            title: request.description.clone(),
            // The delegation's own level, which until this field existed was resolved
            // by the `task` tool and then dropped here: the child ran at the
            // provider's default no matter what `effort` the caller passed.
            effort: request.effort,
        };
        let plan = TurnPlan::resolve(&options, &self.environment)
            .await
            .map_err(ChildTurnError::Host)?;
        let mut host = TurnHost::open_with_runtime_and_mcp(
            plan,
            &self.environment,
            Arc::clone(&self.approval),
            None,
            zuno_engine::status::SessionRunRegistry::new(),
            None,
        )
        .map_err(ChildTurnError::Host)?;

        // The child's events are drained rather than forwarded. A subagent's steps and
        // tool calls are what delegation exists to keep out of the parent's transcript
        // and context; the parent receives the child's answer and its session id, and
        // can read the rest by opening that session. Dropping the receiver instead
        // would make the bounded channel back-pressure the child's own turn.
        let (sender, mut receiver) = event_channel();
        let sender = host.with_event_hooks(sender);
        let drive = async {
            let outcome = host.drive(&request.prompt, sender.clone()).await;
            drop(sender);
            outcome
        };
        let drain = async { while receiver.recv().await.is_some() {} };
        let (outcome, ()) = tokio::join!(drive, drain);
        host.shutdown().await;
        outcome.map_err(ChildTurnError::Host)?;

        let output = self.answer(&session_id)?;
        Ok(ChildTurn {
            session_id,
            background_id: None,
            output,
        })
    }
}

#[cfg(test)]
#[path = "child_turn_tests.rs"]
mod tests;
