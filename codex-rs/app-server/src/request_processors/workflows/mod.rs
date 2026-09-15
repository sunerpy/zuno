use crate::config_manager::ConfigManager;
use crate::outgoing_message::OutgoingMessageSender;
use codex_code_mode::CodeModeSessionProvider;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_state::SqliteConfig;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use zuno_workflows::RegisteredWorkflow;
use zuno_workflows::WorkflowDiagnostic;
use zuno_workflows::WorkflowLedger;
use zuno_workflows::WorkflowRun;

mod backend_factory;
mod binding;
mod catalog;
mod dispatcher;
mod execution;
mod host;
mod projection;

use dispatcher::NativeWorkflowAgentDispatcher;
pub(crate) use dispatcher::WorkflowAgentDispatchError;
pub(crate) use dispatcher::WorkflowAgentDispatchRequest;
pub(crate) use dispatcher::WorkflowAgentDispatcher;

const MAX_DISCOVERY_CWDS: usize = 32;
const MAX_CATALOG_SNAPSHOTS: usize = 32;
const MAX_WORKFLOW_DOCUMENT_BYTES: usize = 1024 * 1024;
const PROJECT_WORKFLOW_ROOT: &str = ".zuno/workflows";
const USER_WORKFLOW_ROOT: &str = "workflows";

#[derive(Default)]
pub(super) struct WorkflowCatalogCache {
    pub(super) snapshots: BTreeMap<String, Arc<WorkflowCatalogSnapshot>>,
    pub(super) known: BTreeMap<String, Arc<RegisteredWorkflow>>,
}

pub(super) struct WorkflowCatalogSnapshot {
    pub(super) workflows: BTreeMap<String, Arc<RegisteredWorkflow>>,
    pub(super) diagnostics: Vec<WorkflowDiagnostic>,
}

/// App-server control plane for user-owned `zuno.workflow/v1` documents.
///
/// This processor contains no product workflow. It discovers user, project,
/// and active plugin roots, then executes the selected engine through injected
/// providers and AgentBackend dispatchers.
pub(crate) struct WorkflowRequestProcessor {
    pub(super) config: Arc<Config>,
    pub(super) thread_manager: Arc<ThreadManager>,
    pub(super) code_mode_sessions: Arc<dyn CodeModeSessionProvider>,
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    pub(super) sqlite: SqliteConfig,
    pub(super) ledger: OnceCell<Arc<WorkflowLedger>>,
    pub(super) catalog: RwLock<WorkflowCatalogCache>,
    /// Run IDs with one in-process driver already admitted. This closes the
    /// window between durable `queued` admission and `active_runs` population,
    /// so an idempotent `workflow/start` retry cannot launch a second engine.
    pub(super) launching_runs: Mutex<BTreeSet<String>>,
    pub(super) active_runs: Mutex<BTreeMap<String, Arc<dyn WorkflowRun>>>,
    pub(super) agent_dispatcher: Arc<dyn WorkflowAgentDispatcher>,
    pub(super) shutdown: CancellationToken,
}

impl WorkflowRequestProcessor {
    pub(crate) fn new(
        config: Arc<Config>,
        thread_manager: Arc<ThreadManager>,
        code_mode_sessions: Arc<dyn CodeModeSessionProvider>,
        outgoing: Arc<OutgoingMessageSender>,
        config_manager: ConfigManager,
    ) -> Arc<Self> {
        let dispatcher = Arc::new(NativeWorkflowAgentDispatcher::new(
            Arc::downgrade(&thread_manager),
            config_manager,
        ));
        Self::new_with_agent_dispatcher(
            config,
            thread_manager,
            code_mode_sessions,
            outgoing,
            dispatcher,
        )
    }

    pub(crate) fn new_with_agent_dispatcher(
        config: Arc<Config>,
        thread_manager: Arc<ThreadManager>,
        code_mode_sessions: Arc<dyn CodeModeSessionProvider>,
        outgoing: Arc<OutgoingMessageSender>,
        agent_dispatcher: Arc<dyn WorkflowAgentDispatcher>,
    ) -> Arc<Self> {
        Arc::new(Self {
            sqlite: config.sqlite_config().clone(),
            config,
            thread_manager,
            code_mode_sessions,
            outgoing,
            ledger: OnceCell::new(),
            catalog: RwLock::new(WorkflowCatalogCache::default()),
            launching_runs: Mutex::new(BTreeSet::new()),
            active_runs: Mutex::new(BTreeMap::new()),
            agent_dispatcher,
            shutdown: CancellationToken::new(),
        })
    }
}
