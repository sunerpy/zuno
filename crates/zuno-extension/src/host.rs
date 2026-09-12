use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, watch};
use zuno_error::ToolError;
use zuno_runtime::{Component, EffectError, PrepareContext, ProfileBundle, RuntimeError};
use zuno_tool::{
    Tool, ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy,
    ToolUiIntent,
};

use crate::manifest::{
    PluginCapability, PluginRuntime, PluginToolConcurrency, PluginToolDefinition, PluginToolEffect,
    PluginToolReplay, PluginToolUiIntent,
};
use crate::{PackageOrigin, ResolvedExtensions};

mod process;
mod wasi;

/// Exact protocol negotiated by every executable plugin host.
pub const PLUGIN_PROTOCOL_VERSION: &str = "zuno.plugin/1";
/// Canonical Component Model interface implemented by WASI plugins.
///
/// The embedded copy keeps release builds independent of repository-relative
/// runtime files. A test keeps it byte-identical to the repository-facing WIT.
pub const PLUGIN_WIT: &str = include_str!("plugin.wit");

const RUNTIME_BUNDLE_ID: &str = "zuno.extension-runtime";
const RUNTIME_COMPONENT_ID: &str = "zuno.extension-runtime.hosts";
const RUNTIME_EFFECT_ID: &str = "plugin-hosts";

/// Metadata key under which a cancelled call states whether its outcome is decided.
///
/// The consumer is `zuno-engine`'s dispatcher, which reads `uncertain` from here to
/// decide whether the interruption it records needs authoritative inspection; absent
/// metadata reads as a clean cooperative return. That crate is handed this crate's tools
/// rather than depending on it, so the spelling lives here too and a test pins it.
const METADATA_CANCELLATION_KEY: &str = "cancellation";

/// Profile additions required by the currently resolved extension composition.
pub struct RuntimeSurface {
    tools: Vec<Arc<dyn Tool>>,
    bundle: Option<ProfileBundle>,
}

impl RuntimeSurface {
    /// Executable tool proxies in manifest order.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Consume the lifecycle bundle, when at least one runtime is present.
    #[must_use]
    pub fn take_bundle(&mut self) -> Option<ProfileBundle> {
        self.bundle.take()
    }
}

/// Build executable tool proxies and the lifecycle component that owns their hosts.
pub fn runtime_surface(
    extensions: &ResolvedExtensions,
    workspace: &Path,
) -> Result<RuntimeSurface, RuntimeSurfaceError> {
    let hosts = Arc::new(PluginHostSet::default());
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut specs = Vec::new();
    for resolved in extensions.packages() {
        let Some(runtime) = &resolved.package.runtime else {
            continue;
        };
        let manifest = match &resolved.origin {
            PackageOrigin::Static { manifest } => manifest,
            PackageOrigin::Process => {
                return Err(RuntimeSurfaceError::DynamicExecutable {
                    package: resolved.package.id.clone(),
                });
            }
        };
        let root = manifest
            .parent()
            .ok_or_else(|| RuntimeSurfaceError::MissingPackageRoot {
                package: resolved.package.id.clone(),
                manifest: manifest.clone(),
            })?
            .to_path_buf();
        let spec = RuntimeSpec {
            package: resolved.package.id.clone(),
            root_literal: plugin_path_literal(&resolved.package.id, &root)?,
            root,
            workspace_literal: plugin_path_literal(&resolved.package.id, workspace)?,
            workspace: workspace.to_path_buf(),
            runtime: runtime.clone(),
        };
        for (name, definition) in resolved.package.tools.iter() {
            tools.push(Arc::new(PluginTool::new(
                resolved.package.id.clone(),
                name.to_owned(),
                definition.clone(),
                Arc::clone(&hosts),
            )));
        }
        specs.push(spec);
    }
    let bundle = (!specs.is_empty()).then(|| {
        ProfileBundle::new(RUNTIME_BUNDLE_ID)
            .with_component(PluginRuntimeComponent::new(specs, hosts))
    });
    Ok(RuntimeSurface { tools, bundle })
}

/// The exact string a plugin boundary may carry for one native path.
///
/// Both hosts hand paths to the plugin as text — JSON for the process runtime, WIT
/// `string` arguments for the WASI runtime — and text there is UTF-8 only. A path that is
/// not valid UTF-8 therefore has no representation to send: serializing it into `json!`
/// panics and a lossy conversion hands the plugin a substituted path that resolves
/// nowhere. Refusing the composition here keeps that decision at one boundary, before any
/// runtime starts, so neither host has to guess later.
///
/// A representable path is returned byte-for-byte. This is deliberately not
/// [`zuno_paths::wire_path`], which folds `\` to `/` on every platform: that rendering is
/// for display and durable metadata, where a stable separator matters more than the exact
/// name, but `packageRoot`/`workspace` is the only statement a plugin gets about which
/// tree it owns and the process runtime acts on it natively and unconfined. On Linux and
/// macOS `\` is an ordinary filename byte, so folding it here would hand a plugin a
/// different, possibly existing, directory — a substitution no reader could detect,
/// which is the failure this boundary exists to prevent.
fn plugin_path_literal(package: &str, path: &Path) -> Result<String, RuntimeSurfaceError> {
    path.to_str()
        .ok_or_else(|| RuntimeSurfaceError::UnrepresentablePath {
            package: package.to_owned(),
            path: path.to_path_buf(),
        })
        .map(str::to_owned)
}

/// Failure while projecting a resolved package set into executable hosts.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeSurfaceError {
    #[error(
        "process-local extension package `{package}` cannot declare executable runtime code; install it as a static package"
    )]
    DynamicExecutable { package: String },
    #[error(
        "extension package `{package}` manifest {} has no package directory",
        manifest.display()
    )]
    MissingPackageRoot { package: String, manifest: PathBuf },
    #[error(
        "extension package `{package}` cannot run: {} is not valid UTF-8, so it cannot be sent to a plugin",
        path.display()
    )]
    UnrepresentablePath { package: String, path: PathBuf },
}

#[derive(Clone)]
pub(crate) struct RuntimeSpec {
    pub(crate) package: String,
    pub(crate) root: PathBuf,
    /// [`Self::root`] exactly as the plugin boundary carries it.
    ///
    /// Validated by [`plugin_path_literal`], which is the only place a path becomes text
    /// for a plugin: the hosts carry the string so neither can re-render a path itself.
    pub(crate) root_literal: String,
    pub(crate) workspace: PathBuf,
    /// [`Self::workspace`] exactly as the plugin boundary carries it.
    pub(crate) workspace_literal: String,
    pub(crate) runtime: PluginRuntime,
}

impl RuntimeSpec {
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_millis(self.runtime.timeout_ms())
    }

    pub(crate) fn capability_names(&self) -> Vec<String> {
        self.runtime
            .capabilities()
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect()
    }
}

/// One runtime request after native tool validation and authorization.
#[derive(Clone)]
pub(crate) struct PluginInvocation {
    pub(crate) tool: String,
    pub(crate) arguments: Value,
    pub(crate) session_id: String,
    pub(crate) message_id: String,
    pub(crate) call_id: String,
    pub(crate) agent: String,
    pub(crate) interrupt: Arc<dyn zuno_tool::InterruptHandle>,
}

/// One successful runtime response.
pub(crate) struct PluginResult {
    pub(crate) title: String,
    pub(crate) output: String,
    pub(crate) metadata: Map<String, Value>,
}

/// Runtime provider interface shared by the WASI and process hosts.
#[async_trait]
pub(crate) trait PluginHost: Send + Sync {
    async fn invoke(&self, request: PluginInvocation) -> Result<PluginResult, PluginHostError>;
    async fn shutdown(&self) -> Result<(), PluginHostError>;
}

/// Typed failure from an executable plugin boundary.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginHostError {
    #[error("plugin `{package}` failed to start: {message}")]
    Start { package: String, message: String },
    #[error("plugin `{package}` is protocol-incompatible: {message}")]
    Incompatible { package: String, message: String },
    #[error("plugin `{package}` tool `{tool}` failed: {message}")]
    Failed {
        package: String,
        tool: String,
        message: String,
    },
    #[error("plugin `{package}` {operation} timed out after {elapsed:?}")]
    Timeout {
        package: String,
        operation: String,
        elapsed: Duration,
    },
    #[error(
        "plugin `{package}` call was cancelled{}{}",
        if *dispatched {
            "; the call had already been dispatched, so what it changed is undecided"
        } else {
            " before the call was dispatched"
        },
        cleanup
            .as_deref()
            .map(|detail| format!("; {detail}"))
            .unwrap_or_default()
    )]
    Cancelled {
        package: String,
        /// Whether the call had reached the plugin when the interrupt was serviced.
        ///
        /// Only the host that dispatched the call can tell a cancellation that stopped
        /// nothing from one that killed a plugin partway through a side effect, so it
        /// says which here instead of leaving every reader to assume the former.
        ///
        /// This is an observation, not a verdict: it stays separate from [`Self::cleanup`]
        /// all the way to the report, because merging them into one `uncertain` bit gets
        /// the safety verdict right and then makes the sentence attached to it assert a
        /// dispatch the host had explicitly determined did not happen.
        ///
        /// The cancelled tool is deliberately absent: the only consumer is
        /// [`PluginTool::execute`], which already holds the tool identity the client card
        /// and the model see, so a host cannot spell that subject differently from the
        /// tool it was asked to run.
        dispatched: bool,
        /// Why stopping the plugin was not authoritative, when it was not.
        ///
        /// `Some` means the host could not confirm the plugin is no longer running, so
        /// the outcome is undecided whatever the dispatch state was: a process that
        /// survived termination can still be acting on the call.
        cleanup: Option<String>,
    },
    #[error("plugin `{package}` outcome is uncertain during {operation}: {message}")]
    Uncertain {
        package: String,
        operation: String,
        message: String,
    },
    #[error("plugin `{package}` failed to stop authoritatively: {message}")]
    Stop { package: String, message: String },
    #[error("plugin `{package}` runtime host is {state}")]
    Unavailable {
        package: String,
        state: &'static str,
    },
}

impl PluginHostError {
    /// Whether authoritative state must be inspected before any replay.
    #[must_use]
    pub const fn is_uncertain(&self) -> bool {
        matches!(
            self,
            Self::Uncertain { .. }
                | Self::Stop { .. }
                | Self::Cancelled {
                    dispatched: true,
                    ..
                }
                | Self::Cancelled {
                    cleanup: Some(_),
                    ..
                }
        )
    }
}

#[derive(Default)]
enum PluginHostSetState {
    #[default]
    Unpublished,
    Published {
        by_package: BTreeMap<String, Arc<PluginHostSlot>>,
        in_start_order: Vec<Arc<PluginHostSlot>>,
    },
    Closed,
}

#[derive(Default)]
struct PluginHostSet {
    state: RwLock<PluginHostSetState>,
}

impl PluginHostSet {
    fn publish(&self, specs: Vec<RuntimeSpec>) -> Result<(), EffectError> {
        let mut by_package = BTreeMap::new();
        let mut in_start_order = Vec::with_capacity(specs.len());
        for spec in specs {
            let package = spec.package.clone();
            let slot = Arc::new(PluginHostSlot::new(spec));
            if by_package
                .insert(package.clone(), Arc::clone(&slot))
                .is_some()
            {
                return Err(EffectError::new(format!(
                    "plugin runtime package `{package}` was published more than once"
                )));
            }
            in_start_order.push(slot);
        }

        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*state, PluginHostSetState::Unpublished) {
            return Err(EffectError::new(
                "plugin host set was already published before lifecycle start",
            ));
        }
        *state = PluginHostSetState::Published {
            by_package,
            in_start_order,
        };
        Ok(())
    }

    fn withdraw(&self) -> Vec<Arc<PluginHostSlot>> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *state, PluginHostSetState::Closed) {
            PluginHostSetState::Published { in_start_order, .. } => in_start_order,
            PluginHostSetState::Unpublished | PluginHostSetState::Closed => Vec::new(),
        }
    }

    async fn invoke(
        &self,
        package: &str,
        request: PluginInvocation,
    ) -> Result<PluginResult, PluginHostError> {
        let slot = match &*self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            PluginHostSetState::Published { by_package, .. } => by_package.get(package).cloned(),
            PluginHostSetState::Unpublished | PluginHostSetState::Closed => None,
        }
        .ok_or_else(|| PluginHostError::Unavailable {
            package: package.to_owned(),
            state: "not active in the current profile",
        })?;
        slot.invoke(request).await
    }
}

type HostStartResult = Result<Arc<dyn PluginHost>, PluginHostError>;
type HostStopResult = Result<(), PluginHostError>;

struct SharedCompletion<T> {
    outcome: watch::Sender<Option<T>>,
}

impl<T> SharedCompletion<T>
where
    T: Clone,
{
    fn pending() -> Self {
        let (outcome, _receiver) = watch::channel(None);
        Self { outcome }
    }

    fn complete(&self, outcome: T) {
        self.outcome.send_replace(Some(outcome));
    }

    async fn wait(&self) -> T {
        let mut outcome = self.outcome.subscribe();
        loop {
            if let Some(outcome) = outcome.borrow_and_update().clone() {
                return outcome;
            }
            outcome
                .changed()
                .await
                .expect("plugin host completion sender remains owned by the slot");
        }
    }
}

enum PluginHostSlotState {
    Dormant,
    Starting(Arc<SharedCompletion<HostStartResult>>),
    Active(Arc<dyn PluginHost>),
    Stopping(Arc<SharedCompletion<HostStopResult>>),
    Closed,
}

struct PluginHostSlot {
    package: String,
    spec: RuntimeSpec,
    state: Mutex<PluginHostSlotState>,
}

impl PluginHostSlot {
    fn new(spec: RuntimeSpec) -> Self {
        Self {
            package: spec.package.clone(),
            spec,
            state: Mutex::new(PluginHostSlotState::Dormant),
        }
    }

    async fn invoke(
        self: &Arc<Self>,
        request: PluginInvocation,
    ) -> Result<PluginResult, PluginHostError> {
        let host = self.host().await?;
        host.invoke(request).await
    }

    async fn host(self: &Arc<Self>) -> HostStartResult {
        enum StartDecision {
            Start(Arc<SharedCompletion<HostStartResult>>),
            Wait(Arc<SharedCompletion<HostStartResult>>),
            Ready(Arc<dyn PluginHost>),
            Unavailable(&'static str),
        }

        let decision = {
            let mut state = self.state.lock().await;
            match &*state {
                PluginHostSlotState::Dormant => {
                    let attempt = Arc::new(SharedCompletion::pending());
                    *state = PluginHostSlotState::Starting(Arc::clone(&attempt));
                    StartDecision::Start(attempt)
                }
                PluginHostSlotState::Starting(attempt) => StartDecision::Wait(Arc::clone(attempt)),
                PluginHostSlotState::Active(host) => StartDecision::Ready(Arc::clone(host)),
                PluginHostSlotState::Stopping(_) => StartDecision::Unavailable("stopping"),
                PluginHostSlotState::Closed => StartDecision::Unavailable("closed"),
            }
        };

        match decision {
            StartDecision::Start(attempt) => {
                self.spawn_start(Arc::clone(&attempt));
                attempt.wait().await
            }
            StartDecision::Wait(attempt) => attempt.wait().await,
            StartDecision::Ready(host) => Ok(host),
            StartDecision::Unavailable(state) => Err(PluginHostError::Unavailable {
                package: self.package.clone(),
                state,
            }),
        }
    }

    fn spawn_start(self: &Arc<Self>, attempt: Arc<SharedCompletion<HostStartResult>>) {
        let slot = Arc::clone(self);
        let task = tokio::spawn(async move {
            let outcome = start_host(slot.spec.clone()).await;
            let mut state = slot.state.lock().await;
            let owns_attempt = matches!(
                &*state,
                PluginHostSlotState::Starting(current)
                    if Arc::ptr_eq(current, &attempt)
            );
            if owns_attempt {
                *state = match &outcome {
                    Ok(host) => PluginHostSlotState::Active(Arc::clone(host)),
                    Err(_) => PluginHostSlotState::Dormant,
                };
                attempt.complete(outcome);
                return;
            }
            drop(state);

            let outcome = match outcome {
                Ok(host) => match host.shutdown().await {
                    Ok(()) => Err(PluginHostError::Unavailable {
                        package: slot.package.clone(),
                        state: "closed during startup",
                    }),
                    Err(cleanup) => Err(cleanup),
                },
                Err(error) => Err(error),
            };
            attempt.complete(outcome);
        });
        drop(task);
    }

    async fn close(&self) -> HostStopResult {
        enum StopDecision {
            WaitForStart(Arc<SharedCompletion<HostStartResult>>),
            Stop {
                host: Arc<dyn PluginHost>,
                attempt: Arc<SharedCompletion<HostStopResult>>,
            },
            Wait(Arc<SharedCompletion<HostStopResult>>),
            Done,
        }

        loop {
            let decision = {
                let mut state = self.state.lock().await;
                match &*state {
                    PluginHostSlotState::Dormant => {
                        *state = PluginHostSlotState::Closed;
                        StopDecision::Done
                    }
                    PluginHostSlotState::Starting(attempt) => {
                        StopDecision::WaitForStart(Arc::clone(attempt))
                    }
                    PluginHostSlotState::Active(host) => {
                        let host = Arc::clone(host);
                        let attempt = Arc::new(SharedCompletion::pending());
                        *state = PluginHostSlotState::Stopping(Arc::clone(&attempt));
                        StopDecision::Stop { host, attempt }
                    }
                    PluginHostSlotState::Stopping(attempt) => {
                        StopDecision::Wait(Arc::clone(attempt))
                    }
                    PluginHostSlotState::Closed => StopDecision::Done,
                }
            };

            match decision {
                StopDecision::WaitForStart(attempt) => {
                    let _outcome = attempt.wait().await;
                }
                StopDecision::Stop { host, attempt } => {
                    let outcome = host.shutdown().await;
                    {
                        let mut state = self.state.lock().await;
                        *state = PluginHostSlotState::Closed;
                    }
                    attempt.complete(outcome.clone());
                    return outcome;
                }
                StopDecision::Wait(attempt) => return attempt.wait().await,
                StopDecision::Done => return Ok(()),
            }
        }
    }
}

struct PluginRuntimeComponent {
    specs: Vec<RuntimeSpec>,
    hosts: Arc<PluginHostSet>,
}

impl PluginRuntimeComponent {
    fn new(specs: Vec<RuntimeSpec>, hosts: Arc<PluginHostSet>) -> Self {
        Self { specs, hosts }
    }
}

#[async_trait]
impl Component for PluginRuntimeComponent {
    fn id(&self) -> &str {
        RUNTIME_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let specs = self.specs.clone();
        let hosts = Arc::clone(&self.hosts);
        context.effect(RUNTIME_EFFECT_ID, move || async move {
            hosts.publish(specs)?;
            Ok(move || async move {
                let slots = hosts.withdraw();
                stop_host_slots(slots).await
            })
        })
    }
}

async fn start_host(spec: RuntimeSpec) -> HostStartResult {
    match &spec.runtime {
        PluginRuntime::Wasi { .. } => wasi::start(spec).await,
        PluginRuntime::Process { .. } => process::start(spec).await,
    }
}

async fn stop_host_slots(mut slots: Vec<Arc<PluginHostSlot>>) -> Result<(), EffectError> {
    let mut failures = Vec::new();
    while let Some(slot) = slots.pop() {
        if let Err(error) = slot.close().await {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(EffectError::new(failures.join("; ")))
    }
}

struct PluginTool {
    package: String,
    name: String,
    definition: PluginToolDefinition,
    hosts: Arc<PluginHostSet>,
}

impl PluginTool {
    fn new(
        package: String,
        name: String,
        definition: PluginToolDefinition,
        hosts: Arc<PluginHostSet>,
    ) -> Self {
        Self {
            package,
            name,
            definition,
            hosts,
        }
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn presentation(&self) -> zuno_types::activity::InvocationPresentation {
        use zuno_types::activity::{
            ActivityName, InvocationAction, InvocationPresentation, InvocationSource,
        };
        InvocationPresentation {
            action: match self.definition.ui_intent {
                PluginToolUiIntent::Subagent => InvocationAction::Agent,
                PluginToolUiIntent::Generic => InvocationAction::Tool,
            },
            source: ActivityName::new(&self.package)
                .map(|extension| InvocationSource::Extension { extension })
                .unwrap_or(InvocationSource::Unknown),
        }
    }

    fn id(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        match self.definition.replay {
            PluginToolReplay::Never => ToolReplayPolicy::Never,
            PluginToolReplay::Safe => ToolReplayPolicy::Safe,
        }
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        match self.definition.concurrency {
            PluginToolConcurrency::Exclusive => ToolConcurrencyPolicy::Exclusive,
            PluginToolConcurrency::ParallelSafe => ToolConcurrencyPolicy::ParallelSafe,
            PluginToolConcurrency::IsolatedBackground => ToolConcurrencyPolicy::IsolatedBackground,
        }
    }

    fn ui_intent(&self) -> ToolUiIntent {
        match self.definition.ui_intent {
            PluginToolUiIntent::Generic => ToolUiIntent::Generic,
            PluginToolUiIntent::Subagent => ToolUiIntent::Subagent,
        }
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        match self.definition.effect {
            PluginToolEffect::ReadOnly => ToolEffect::ReadOnly,
            PluginToolEffect::UserMediated => ToolEffect::UserMediated,
            PluginToolEffect::Delegating => ToolEffect::Delegating,
            PluginToolEffect::SideEffecting => ToolEffect::SideEffecting,
        }
    }

    fn raw_parameters_schema(&self) -> Value {
        self.definition.parameters.clone()
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let request = PluginInvocation {
            tool: self.name.clone(),
            arguments: args,
            session_id: ctx.session_id,
            message_id: ctx.message_id,
            call_id: ctx.call_id,
            agent: ctx.agent,
            interrupt: ctx.interrupt,
        };
        let result = match self.hosts.invoke(&self.package, request).await {
            Ok(result) => result,
            // A cancelled call is a settled report rather than a failure. A `ToolError`
            // carries no certainty claim, and the dispatcher reads a claimless
            // cancellation as a clean cooperative return — which is how a plugin killed
            // partway through a side effect came to be published as completed cleanup.
            Err(source) => match source.cancellation_report(&self.name) {
                Ok(settled) => return Ok(settled),
                Err(source) => return Err(plugin_tool_error(self.name.clone(), source)),
            },
        };
        // `cancellation` is a host-issued claim the dispatcher reads as a certainty
        // statement, so a plugin does not get to write it: the untrusted side of this
        // boundary cannot forge, or overwrite, what only the host can observe.
        let mut metadata = result.metadata;
        if metadata.remove(METADATA_CANCELLATION_KEY).is_some() {
            tracing::warn!(
                package = %self.package,
                tool = %self.name,
                key = METADATA_CANCELLATION_KEY,
                "dropped a plugin-supplied reserved metadata key"
            );
        }
        Ok(ToolOutput {
            title: result.title,
            output: result.output,
            metadata,
            attachments: Vec::new(),
            presentation: None,
            continuation: zuno_tool::ToolContinuation::Continue,
        })
    }
}

/// The settled report of a plugin call a user interrupt stopped.
///
/// The notice states the outcome the model must act on and the metadata states the same
/// facts for the dispatcher and the client surfaces, because a sentence alone cannot be
/// read back by a projection and metadata alone leaves the model with no instruction.
///
/// Two independent facts decide what is said, and both are reported. `dispatched` is the
/// host's own observation of whether the plugin was handed the call; `cleanup` is why
/// stopping the plugin was not confirmed, when it was not. Either one alone makes the
/// outcome undecided, so the verdict is their disjunction — but the sentence is not: a
/// cancellation that is undecided *because the stop failed* must not tell the model the
/// call was sent, because the host determined it was not, and the model would then reason
/// about partial side effects of bytes that never left Zuno. The durable detail follows
/// the same split, so the record cannot disagree with the observation it was written from.
fn cancelled_output(
    package: &str,
    tool: &str,
    dispatched: bool,
    cleanup: Option<&str>,
) -> ToolOutput {
    // A stop that was not confirmed cannot be reported as decided, whatever the dispatch
    // state was: the plugin may still be running this call.
    let uncertain = dispatched || cleanup.is_some();
    let (notice, detail) = match (dispatched, cleanup) {
        (true, None) => (
            format!(
                "Cancelled by the user. Plugin `{package}` had already been sent this `{tool}` \
                 call and was stopped before it reported an outcome. Inspect the authoritative \
                 state this call would have changed before deciding what to do next; it must not \
                 be re-run on the assumption that it did nothing."
            ),
            "the call was dispatched and the plugin was stopped before it reported an outcome",
        ),
        (true, Some(cleanup)) => (
            format!(
                "Cancelled by the user. Plugin `{package}` had already been sent this `{tool}` \
                 call, and stopping the plugin was not confirmed ({cleanup}), so it may still be \
                 running this call. Inspect the authoritative state this call would have changed \
                 before deciding what to do next; it must not be re-run on the assumption that \
                 it did nothing."
            ),
            "the call was dispatched and the plugin was not confirmed stopped",
        ),
        (false, None) => (
            format!(
                "Cancelled by the user. The `{tool}` call was stopped before plugin `{package}` \
                 received it, so nothing ran."
            ),
            "the call never reached the plugin",
        ),
        (false, Some(cleanup)) => (
            format!(
                "Cancelled by the user. The `{tool}` call was stopped before plugin `{package}` \
                 received it, but stopping the plugin was not confirmed ({cleanup}), so it may \
                 still be running and may still read this call. Inspect the authoritative state \
                 before deciding what to do next; it must not be re-run on the assumption that \
                 it did nothing."
            ),
            "the call had not reached the plugin, but the plugin was not confirmed stopped and \
             may still read it",
        ),
    };
    let mut claim = json!({
        "cancelled": true,
        "authoritative": !uncertain,
        "uncertain": uncertain,
        "dispatched": dispatched,
        "detail": detail,
    });
    if let Some(cleanup) = cleanup {
        claim["stopped"] = json!(false);
        claim["cleanup"] = json!(cleanup);
    }
    ToolOutput::text(format!("{tool} cancelled"), notice)
        .with_metadata(METADATA_CANCELLATION_KEY, claim)
}

impl PluginHostError {
    /// The settled cancellation report for one tool call, or this failure unchanged.
    ///
    /// The only place a `Cancelled` host failure becomes a tool result, so the report and
    /// the two facts behind it cannot be assembled differently by two callers. The subject
    /// is the tool's own identity, never the host's internal operation label: the notice
    /// and the card title name the call the user cancelled, not the `tools/call` frame or
    /// the WIT export that carried it.
    fn cancellation_report(self, tool: &str) -> Result<ToolOutput, Self> {
        match self {
            Self::Cancelled {
                package,
                dispatched,
                cleanup,
            } => Ok(cancelled_output(
                &package,
                tool,
                dispatched,
                cleanup.as_deref(),
            )),
            other => Err(other),
        }
    }
}

fn plugin_tool_error(tool: String, source: PluginHostError) -> ToolError {
    if source.is_uncertain() || matches!(&source, PluginHostError::Timeout { .. }) {
        return ToolError::Transient {
            tool,
            retry_after: None,
            source: Box::new(source),
        };
    }
    ToolError::Failed {
        tool,
        source: Box::new(source),
    }
}

pub(crate) fn capabilities_contain(
    capabilities: &[PluginCapability],
    needle: PluginCapability,
) -> bool {
    capabilities.contains(&needle)
}

#[derive(Clone)]
pub(crate) struct SecretRedactor {
    secrets: Vec<String>,
}

impl SecretRedactor {
    pub(crate) fn from_process() -> Self {
        let secrets = std::env::vars_os()
            .filter_map(|(name, value)| {
                let name = name.to_string_lossy().to_ascii_uppercase();
                let sensitive = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
                    .iter()
                    .any(|marker| name.contains(marker));
                let value = value.to_string_lossy();
                (sensitive && value.len() >= 4).then(|| value.into_owned())
            })
            .collect();
        Self { secrets }
    }

    pub(crate) fn safe(&self, value: impl AsRef<str>) -> String {
        let mut safe = value.as_ref().to_owned();
        for secret in &self.secrets {
            safe = safe.replace(secret, "[REDACTED]");
        }
        safe.lines()
            .map(|line| {
                let lower = line.to_ascii_lowercase();
                if lower.contains("authorization: bearer ")
                    || lower.contains("api_key=")
                    || lower.contains("apikey=")
                {
                    "[REDACTED CREDENTIAL LINE]".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_wit_matches_the_repository_contract() {
        let repository = include_str!("../../../wit/zuno-plugin/plugin.wit");
        assert_eq!(PLUGIN_WIT, repository);
    }

    #[test]
    fn uncertain_and_timeout_host_failures_reach_typed_tool_recovery() {
        for source in [
            PluginHostError::Uncertain {
                package: "review-kit".to_owned(),
                operation: "tools/call".to_owned(),
                message: "reply was lost".to_owned(),
            },
            PluginHostError::Timeout {
                package: "review-kit".to_owned(),
                operation: "tools/call".to_owned(),
                elapsed: Duration::from_secs(30),
            },
        ] {
            let error = plugin_tool_error("review".to_owned(), source);
            assert!(error.is_retryable(), "{error}");
            assert!(matches!(error, ToolError::Transient { .. }));
        }
    }

    #[test]
    fn a_cancellation_states_its_certainty_where_the_dispatcher_reads_it() {
        // The dispatcher spells this key independently, so a rename on either side must
        // fail here rather than silently stop being read.
        assert_eq!(METADATA_CANCELLATION_KEY, "cancellation");

        let undecided = cancelled_output("review-kit", "review", true, None);
        let claim = &undecided.metadata[METADATA_CANCELLATION_KEY];
        assert_eq!(claim["cancelled"], json!(true));
        assert_eq!(claim["uncertain"], json!(true));
        assert_eq!(claim["authoritative"], json!(false));
        assert_eq!(claim["dispatched"], json!(true));
        assert!(
            undecided.output.contains("Inspect the authoritative state"),
            "{}",
            undecided.output
        );

        let nothing_ran = cancelled_output("review-kit", "review", false, None);
        let claim = &nothing_ran.metadata[METADATA_CANCELLATION_KEY];
        assert_eq!(claim["uncertain"], json!(false));
        assert_eq!(claim["authoritative"], json!(true));
        assert_eq!(claim["dispatched"], json!(false));
        assert!(
            nothing_ran.output.contains("nothing ran"),
            "{}",
            nothing_ran.output
        );
    }

    #[test]
    fn a_cancellation_after_dispatch_requires_authoritative_inspection() {
        assert!(
            PluginHostError::Cancelled {
                package: "review-kit".to_owned(),
                dispatched: true,
                cleanup: None,
            }
            .is_uncertain()
        );
        assert!(
            !PluginHostError::Cancelled {
                package: "review-kit".to_owned(),
                dispatched: false,
                cleanup: None,
            }
            .is_uncertain()
        );
    }

    /// A cancellation whose cleanup failed is undecided, and says why without inventing
    /// a dispatch.
    ///
    /// `terminate` failing means the plugin was not confirmed stopped — the Windows case
    /// is `taskkill /f /t` exiting non-zero against a live tree — so the process may still
    /// be holding, and acting on, the call this outcome describes. The host observed that
    /// the request never reached the plugin, so neither the model-visible notice nor the
    /// durable detail may say it was sent: a model told a side-effecting call was delivered
    /// reasons about half-applied state for bytes that never left Zuno, and the durable
    /// record then disagrees with the host's own observation. The report is produced by the
    /// production entry point `PluginTool::execute` uses, not by re-deriving the arguments
    /// here.
    #[test]
    fn an_unconfirmed_stop_cannot_report_a_decided_cancellation() {
        let cleanup = "stopping the plugin failed: taskkill failed for process tree 4242 \
                       with status exit code: 128";
        let error = PluginHostError::Cancelled {
            package: "review-kit".to_owned(),
            dispatched: false,
            cleanup: Some(cleanup.to_owned()),
        };
        assert!(error.is_uncertain(), "{error}");
        assert!(error.to_string().contains("taskkill failed"), "{error}");
        assert!(
            error.to_string().contains("before the call was dispatched"),
            "{error}"
        );

        let settled = error
            .cancellation_report("review_outline")
            .expect("a cancellation settles as a report");
        let claim = &settled.metadata[METADATA_CANCELLATION_KEY];
        assert_eq!(claim["uncertain"], json!(true), "{claim}");
        assert_eq!(claim["authoritative"], json!(false), "{claim}");
        assert_eq!(claim["stopped"], json!(false), "{claim}");
        assert_eq!(claim["dispatched"], json!(false), "{claim}");
        let detail = claim["detail"].as_str().expect("a durable detail");
        assert_eq!(
            detail,
            "the call had not reached the plugin, but the plugin was not confirmed stopped \
             and may still read it",
            "the durable record states both facts as observed"
        );
        assert!(
            settled.output.contains("Inspect the authoritative state"),
            "{}",
            settled.output
        );
        assert!(
            settled.output.contains("may still be running"),
            "{}",
            settled.output
        );
        assert!(
            settled
                .output
                .contains("stopped before plugin `review-kit` received it"),
            "{}",
            settled.output
        );
        for invented in ["had already been sent", "was dispatched"] {
            assert!(
                !settled.output.contains(invented),
                "the notice claims a dispatch the host observed did not happen: {}",
                settled.output
            );
            assert!(
                !detail.contains(invented),
                "the durable detail claims a dispatch the host observed did not happen: {detail}"
            );
        }
    }

    /// A dispatched call whose stop was also unconfirmed states both facts.
    ///
    /// The fourth combination is the one that must not lose either half: the model needs
    /// "the call was delivered" *and* "the plugin may still be running it".
    #[test]
    fn a_dispatched_call_with_an_unconfirmed_stop_states_both_facts() {
        let cleanup = "stopping the plugin failed: taskkill failed for process tree 4242 \
                       with status exit code: 128";
        let settled = PluginHostError::Cancelled {
            package: "review-kit".to_owned(),
            dispatched: true,
            cleanup: Some(cleanup.to_owned()),
        }
        .cancellation_report("review_outline")
        .expect("a cancellation settles as a report");
        let claim = &settled.metadata[METADATA_CANCELLATION_KEY];
        assert_eq!(claim["uncertain"], json!(true), "{claim}");
        assert_eq!(claim["dispatched"], json!(true), "{claim}");
        assert_eq!(claim["stopped"], json!(false), "{claim}");
        assert_eq!(
            claim["detail"],
            json!("the call was dispatched and the plugin was not confirmed stopped"),
            "{claim}"
        );
        assert!(
            settled.output.contains("had already been sent"),
            "{}",
            settled.output
        );
        assert!(
            settled.output.contains("taskkill failed"),
            "{}",
            settled.output
        );
    }

    /// A plugin cannot write the metadata key the dispatcher reads as a host claim.
    ///
    /// `cancellation` is a statement only the host can make — the dispatcher reads
    /// `cancellation.uncertain` from whatever settled output reaches its interrupt arm — and
    /// a plugin result's metadata is untrusted input that is otherwise passed through
    /// verbatim. A plugin that returns the key on an ordinary success must not have it read
    /// back as a certainty the host issued.
    #[tokio::test]
    async fn a_plugin_cannot_supply_the_reserved_cancellation_metadata() {
        struct ClaimingHost;

        #[async_trait]
        impl PluginHost for ClaimingHost {
            async fn invoke(
                &self,
                _request: PluginInvocation,
            ) -> Result<PluginResult, PluginHostError> {
                let mut metadata = Map::new();
                metadata.insert(
                    METADATA_CANCELLATION_KEY.to_owned(),
                    json!({"uncertain": false, "authoritative": true}),
                );
                metadata.insert("words".to_owned(), json!(4));
                Ok(PluginResult {
                    title: "Outline".to_owned(),
                    output: "done".to_owned(),
                    metadata,
                })
            }

            async fn shutdown(&self) -> Result<(), PluginHostError> {
                Ok(())
            }
        }

        let hosts = Arc::new(PluginHostSet::default());
        let slot = Arc::new(PluginHostSlot::new(RuntimeSpec {
            package: "review-kit".to_owned(),
            root: PathBuf::new(),
            root_literal: String::new(),
            workspace: PathBuf::new(),
            workspace_literal: String::new(),
            runtime: PluginRuntime::Process {
                command: "unused".to_owned(),
                args: Vec::new(),
                capabilities: vec![PluginCapability::HostFull],
                timeout_ms: 1,
            },
        }));
        *slot.state.lock().await = PluginHostSlotState::Active(Arc::new(ClaimingHost));
        let mut by_package = BTreeMap::new();
        by_package.insert("review-kit".to_owned(), Arc::clone(&slot));
        *hosts
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PluginHostSetState::Published {
            by_package,
            in_start_order: vec![slot],
        };
        let tool = PluginTool::new(
            "review-kit".to_owned(),
            "review_outline".to_owned(),
            serde_json::from_value(json!({"description": "outline a review"}))
                .expect("a valid tool definition"),
            hosts,
        );

        let output = tool
            .execute(
                json!({}),
                ToolContext::new(
                    "ses_extension",
                    "msg_extension",
                    "call_outline",
                    "build",
                    Arc::new(zuno_tool::AllowAll),
                    Arc::new(zuno_tool::NeverInterrupted),
                ),
            )
            .await
            .expect("the call succeeds");

        assert_eq!(
            output.metadata.get(METADATA_CANCELLATION_KEY),
            None,
            "{:?}",
            output.metadata
        );
        assert_eq!(output.metadata["words"], json!(4));
    }

    /// A path a plugin acts on natively crosses the boundary byte-for-byte.
    ///
    /// `zuno_paths::wire_path` folds `\` to `/` on every platform, so rendering these
    /// fields with it would tell a plugin on Linux or macOS that it owns
    /// `/tmp/zuno/ws` when the directory the user selected is named `zuno\ws`.
    #[test]
    fn a_plugin_path_keeps_every_byte_of_the_directory_it_names() {
        let native = Path::new(r"/tmp/zuno\ws");
        assert_eq!(
            plugin_path_literal("review-kit", native).expect("a representable path"),
            r"/tmp/zuno\ws",
        );
        assert_ne!(
            plugin_path_literal("review-kit", native).unwrap(),
            zuno_paths::wire_path(native),
            "a display rendering is not what a plugin may act on"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_no_plugin_boundary_can_carry_is_refused_by_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let native = PathBuf::from(OsStr::from_bytes(b"/tmp/zuno-\xff-workspace"));
        let error = plugin_path_literal("review-kit", &native)
            .expect_err("an odd-byte path has no plugin representation");
        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
    }

    #[test]
    fn authoritative_plugin_rejections_remain_terminal_tool_failures() {
        let error = plugin_tool_error(
            "review".to_owned(),
            PluginHostError::Failed {
                package: "review-kit".to_owned(),
                tool: "review".to_owned(),
                message: "invalid repository".to_owned(),
            },
        );
        assert!(!error.is_retryable());
        assert!(matches!(error, ToolError::Failed { .. }));
    }
}
