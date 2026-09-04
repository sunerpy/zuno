use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use wasmtime::component::{Component, Linker, ResourceTable, TypedFunc};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::{
    PLUGIN_PROTOCOL_VERSION, PluginHost, PluginHostError, PluginInvocation, PluginResult,
    RuntimeSpec, SecretRedactor, capabilities_contain,
};
use crate::{PluginCapability, PluginRuntime};

const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const INTERRUPT_GRACE: Duration = Duration::from_secs(1);

/// Whether guest code ever ran, decided exactly once between the worker and its interrupt.
///
/// A queued call and the arm that cancels it race for the same answer: the worker blocks on
/// the instance lock, so an interrupt serviced while it waits can truthfully report that
/// nothing ran — but only if the worker can no longer enter the guest afterwards. A plain
/// flag cannot express that: the arm reads it, reports "nothing ran", and the worker then
/// acquires the lock, re-arms its own epoch deadline past the increment, and runs. The
/// claim is therefore a single compare-and-exchange that one side wins.
struct GuestEntry(AtomicU8);

impl GuestEntry {
    const QUEUED: u8 = 0;
    const ENTERED: u8 = 1;
    const ABANDONED: u8 = 2;

    fn new() -> Self {
        Self(AtomicU8::new(Self::QUEUED))
    }

    /// Claim the right to enter the guest; `false` once the call has been abandoned.
    fn claim(&self) -> bool {
        self.0
            .compare_exchange(
                Self::QUEUED,
                Self::ENTERED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Withdraw a call that has not entered the guest; `false` once it has.
    fn abandon(&self) -> bool {
        self.0
            .compare_exchange(
                Self::QUEUED,
                Self::ABANDONED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

type InitializeFn = TypedFunc<(String, String, Vec<String>), (Result<String, String>,)>;
type InvokeFn = TypedFunc<
    (String, String, String, String, String, String),
    (Result<(String, String, String), String>,),
>;
type ShutdownFn = TypedFunc<(), (Result<(), String>,)>;

pub(super) async fn start(spec: RuntimeSpec) -> Result<Arc<dyn PluginHost>, PluginHostError> {
    let PluginRuntime::Wasi {
        artifact,
        capabilities,
        environment,
        fuel,
        memory_mib,
        timeout_ms: _,
    } = &spec.runtime
    else {
        unreachable!("WASI provider received a non-WASI runtime");
    };
    let artifact = spec.root.join(artifact);
    let metadata = std::fs::metadata(&artifact).map_err(|error| PluginHostError::Start {
        package: spec.package.clone(),
        message: format!("cannot inspect {}: {error}", artifact.display()),
    })?;
    if !metadata.is_file() {
        return Err(PluginHostError::Start {
            package: spec.package,
            message: format!("{} is not a component file", artifact.display()),
        });
    }
    if metadata.len() > MAX_COMPONENT_BYTES {
        return Err(PluginHostError::Start {
            package: spec.package,
            message: format!(
                "{} is {} bytes; the component limit is {MAX_COMPONENT_BYTES}",
                artifact.display(),
                metadata.len()
            ),
        });
    }

    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .consume_fuel(true)
        .epoch_interruption(true);
    let engine = Engine::new(&config).map_err(|error| PluginHostError::Start {
        package: spec.package.clone(),
        message: error.to_string(),
    })?;
    let state = build_instance(
        &engine,
        &artifact,
        &spec.workspace,
        capabilities,
        environment,
        *fuel,
        *memory_mib,
    )
    .map_err(|message| PluginHostError::Start {
        package: spec.package.clone(),
        message,
    })?;
    let timeout = spec.timeout();
    let capability_names = spec.capability_names();
    let host = Arc::new(WasiPluginHost {
        package: spec.package.clone(),
        workspace_literal: spec.workspace_literal,
        capabilities: capability_names,
        timeout,
        fuel: *fuel,
        engine,
        state: Arc::new(Mutex::new(Some(state))),
        poisoned: Arc::new(AtomicBool::new(false)),
        redactor: SecretRedactor::from_process(),
    });
    if let Err(error) = host.initialize().await {
        let cleanup = host.shutdown().await;
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(PluginHostError::Start {
                package: spec.package,
                message: format!("{error}; startup cleanup failed: {cleanup}"),
            }),
        };
    }
    Ok(host)
}

fn build_instance(
    engine: &Engine,
    artifact: &std::path::Path,
    workspace: &std::path::Path,
    capabilities: &[PluginCapability],
    environment: &[String],
    fuel: u64,
    memory_mib: u64,
) -> Result<WasiInstance, String> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|error| error.to_string())?;
    let component = Component::from_file(engine, artifact).map_err(|error| error.to_string())?;
    let mut builder = WasiCtxBuilder::new();
    builder.allow_blocking_current_thread(true);
    if capabilities_contain(capabilities, PluginCapability::WorkspaceWrite) {
        builder
            .preopened_dir(workspace, "/workspace", FsPerms::ReadWrite)
            .map_err(|error| error.to_string())?;
        builder.initial_cwd("/workspace");
    } else if capabilities_contain(capabilities, PluginCapability::WorkspaceRead) {
        builder
            .preopened_dir(workspace, "/workspace", FsPerms::ReadOnly)
            .map_err(|error| error.to_string())?;
        builder.initial_cwd("/workspace");
    }
    if capabilities_contain(capabilities, PluginCapability::Network) {
        builder
            .inherit_network()
            .allow_ip_name_lookup(true)
            .allow_tcp(true)
            .allow_udp(true);
    }
    for name in environment {
        let Some(value) = std::env::var_os(name) else {
            continue;
        };
        // A WASI environment is UTF-8 by construction, so a value that is not valid UTF-8
        // has no faithful representation here. The guest is left seeing the variable unset
        // — a state it must already handle, and one it can detect — rather than a
        // substituted value it cannot tell apart from the real one; an allowlist commonly
        // names path-bearing variables, and on a POSIX host those can hold any bytes.
        // Refusing the start instead would let one odd byte in an allowlisted `PATH` take
        // the whole extension down, which is a worse answer than an absent variable.
        let Some(value) = value.to_str() else {
            tracing::warn!(
                variable = %name,
                "allowlisted environment value is not valid UTF-8; the WASI guest is not given it"
            );
            continue;
        };
        builder.env(name, value);
    }
    let mut store = Store::new(
        engine,
        WasiState {
            table: ResourceTable::new(),
            context: builder.build(),
            limits: StoreLimitsBuilder::new()
                .memory_size(
                    usize::try_from(memory_mib.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX),
                )
                .instances(32)
                .memories(8)
                .tables(8)
                .table_elements(100_000)
                .build(),
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(fuel).map_err(|error| error.to_string())?;
    store.set_epoch_deadline(1);
    let instance = linker
        .instantiate(&mut store, &component)
        .map_err(|error| error.to_string())?;
    let initialize = instance
        .get_typed_func::<(String, String, Vec<String>), (Result<String, String>,)>(
            &mut store,
            "initialize",
        )
        .map_err(|error| format!("missing compatible `initialize` export: {error}"))?;
    let invoke = instance
        .get_typed_func::<
            (String, String, String, String, String, String),
            (Result<(String, String, String), String>,),
        >(&mut store, "invoke")
        .map_err(|error| format!("missing compatible `invoke` export: {error}"))?;
    let shutdown = instance
        .get_typed_func::<(), (Result<(), String>,)>(&mut store, "shutdown")
        .map_err(|error| format!("missing compatible `shutdown` export: {error}"))?;
    Ok(WasiInstance {
        store,
        initialize,
        invoke,
        shutdown,
    })
}

struct WasiState {
    table: ResourceTable,
    context: WasiCtx,
    limits: StoreLimits,
}

impl WasiView for WasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

struct WasiInstance {
    store: Store<WasiState>,
    initialize: InitializeFn,
    invoke: InvokeFn,
    shutdown: ShutdownFn,
}

struct WasiPluginHost {
    package: String,
    /// The workspace exactly as the guest's WIT `string` argument carries it.
    ///
    /// Validated when the runtime surface was built, so the guest never receives a
    /// substituted or separator-folded root it cannot tell apart from the real one.
    workspace_literal: String,
    capabilities: Vec<String>,
    timeout: Duration,
    fuel: u64,
    engine: Engine,
    state: Arc<Mutex<Option<WasiInstance>>>,
    poisoned: Arc<AtomicBool>,
    redactor: SecretRedactor,
}

impl WasiPluginHost {
    async fn initialize(&self) -> Result<(), PluginHostError> {
        let package = self.package.clone();
        let workspace = self.workspace_literal.clone();
        let capabilities = self.capabilities.clone();
        let negotiated = self
            .call("initialize", None, false, move |instance| {
                instance
                    .initialize
                    .call(&mut instance.store, (package, workspace, capabilities))
                    .map(|(result,)| result)
                    .map_err(|error| error.to_string())
            })
            .await?;
        match negotiated {
            Ok(version) if version == PLUGIN_PROTOCOL_VERSION => Ok(()),
            Ok(version) => Err(PluginHostError::Incompatible {
                package: self.package.clone(),
                message: format!(
                    "initialize returned protocol `{version}`; expected `{PLUGIN_PROTOCOL_VERSION}`"
                ),
            }),
            Err(message) => Err(PluginHostError::Start {
                package: self.package.clone(),
                message: self.redactor.safe(message),
            }),
        }
    }

    async fn call<T, F>(
        &self,
        operation: &str,
        interrupt: Option<Arc<dyn zuno_tool::InterruptHandle>>,
        uncertain_after_start: bool,
        call: F,
    ) -> Result<T, PluginHostError>
    where
        T: Send + 'static,
        F: FnOnce(&mut WasiInstance) -> Result<T, String> + Send + 'static,
    {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(PluginHostError::Uncertain {
                package: self.package.clone(),
                operation: operation.to_owned(),
                message: "the WASI instance was withdrawn after an earlier uncertain outcome"
                    .to_owned(),
            });
        }
        let state = Arc::clone(&self.state);
        let poisoned = Arc::clone(&self.poisoned);
        let fuel = self.fuel;
        let entry = Arc::new(GuestEntry::new());
        let claim = Arc::clone(&entry);
        let mut task = tokio::task::spawn_blocking(move || {
            let mut guard = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if poisoned.load(Ordering::Acquire) {
                return Err("the WASI instance was withdrawn before this call started".to_owned());
            }
            // Guest code runs from here, and `set_epoch_deadline` below re-arms past any
            // increment an interrupt has already made, so this is the last point at which
            // the call can be withdrawn. Claiming first means an interrupt that has
            // already reported "nothing ran" cannot be contradicted by this call.
            if !claim.claim() {
                return Err(
                    "the call was withdrawn before it reached the guest and did not run".to_owned(),
                );
            }
            let instance = guard
                .as_mut()
                .ok_or_else(|| "the WASI instance is no longer active".to_owned())?;
            instance
                .store
                .set_fuel(fuel)
                .map_err(|error| error.to_string())?;
            instance.store.set_epoch_deadline(1);
            call(instance)
        });
        enum Exit<T> {
            Complete(Result<Result<T, String>, tokio::task::JoinError>),
            TimedOut,
            Cancelled,
        }
        let exit = if let Some(interrupt) = interrupt {
            tokio::select! {
                result = &mut task => Exit::Complete(result),
                () = tokio::time::sleep(self.timeout) => Exit::TimedOut,
                () = interrupt.notified() => Exit::Cancelled,
            }
        } else {
            tokio::select! {
                result = &mut task => Exit::Complete(result),
                () = tokio::time::sleep(self.timeout) => Exit::TimedOut,
            }
        };
        match exit {
            Exit::Complete(Ok(Ok(value))) => Ok(value),
            Exit::Complete(Ok(Err(message))) => {
                self.poison();
                Err(PluginHostError::Uncertain {
                    package: self.package.clone(),
                    operation: operation.to_owned(),
                    message: self.redactor.safe(message),
                })
            }
            Exit::Complete(Err(error)) => {
                self.poison();
                Err(PluginHostError::Uncertain {
                    package: self.package.clone(),
                    operation: operation.to_owned(),
                    message: format!("WASI worker failed: {error}"),
                })
            }
            Exit::TimedOut | Exit::Cancelled => {
                // Withdraw the call before anything else: while this arm waits, a call
                // still queued behind the instance lock could otherwise enter the guest
                // after the outcome below has already said it never did.
                let entered = !entry.abandon();
                self.engine.increment_epoch();
                let settled = tokio::time::timeout(INTERRUPT_GRACE, task).await;
                if !matches!(settled, Ok(Ok(Ok(_)))) {
                    self.poison();
                }
                match exit {
                    Exit::Cancelled => Err(PluginHostError::Cancelled {
                        package: self.package.clone(),
                        // Epoch interruption stops the guest at an arbitrary instruction,
                        // so anything a call that entered the guest already wrote through
                        // its preopen survives the instance that is being withdrawn. This
                        // reports whether it entered, as observed; the report derives the
                        // verdict from that and from `cleanup`, and only a call whose
                        // dispatch matters is given an interrupt to be cancelled by.
                        dispatched: entered,
                        // An interrupted guest that did not return within the grace window
                        // is still running — epoch interruption cannot preempt a blocked
                        // host call — so withdrawing the instance is not a confirmed stop.
                        cleanup: (entered && settled.is_err()).then(|| {
                            format!(
                                "the interrupted instance did not stop within \
                                 {INTERRUPT_GRACE:?} and was withdrawn while still running"
                            )
                        }),
                    }),
                    Exit::TimedOut if !uncertain_after_start => Err(PluginHostError::Timeout {
                        package: self.package.clone(),
                        operation: operation.to_owned(),
                        elapsed: self.timeout,
                    }),
                    Exit::TimedOut => Err(PluginHostError::Uncertain {
                        package: self.package.clone(),
                        operation: operation.to_owned(),
                        message: format!(
                            "execution exceeded {:?}; the instance was interrupted and withdrawn",
                            self.timeout
                        ),
                    }),
                    Exit::Complete(_) => unreachable!(),
                }
            }
        }
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.try_lock() {
            state.take();
        }
    }
}

#[async_trait]
impl PluginHost for WasiPluginHost {
    async fn invoke(&self, request: PluginInvocation) -> Result<PluginResult, PluginHostError> {
        let tool = request.tool.clone();
        let arguments =
            serde_json::to_string(&request.arguments).map_err(|error| PluginHostError::Failed {
                package: self.package.clone(),
                tool: tool.clone(),
                message: error.to_string(),
            })?;
        let result = self
            .call(
                &format!("tool `{tool}`"),
                Some(request.interrupt),
                true,
                move |instance| {
                    instance
                        .invoke
                        .call(
                            &mut instance.store,
                            (
                                request.tool,
                                arguments,
                                request.session_id,
                                request.message_id,
                                request.call_id,
                                request.agent,
                            ),
                        )
                        .map(|(result,)| result)
                        .map_err(|error| error.to_string())
                },
            )
            .await?;
        let (title, output, metadata) = result.map_err(|message| PluginHostError::Failed {
            package: self.package.clone(),
            tool: tool.clone(),
            message: self.redactor.safe(message),
        })?;
        let metadata = match serde_json::from_str::<Value>(&metadata) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.poison();
                return Err(PluginHostError::Uncertain {
                    package: self.package.clone(),
                    operation: format!("tool `{tool}` response"),
                    message: format!("metadata was not valid JSON: {error}"),
                });
            }
        };
        let metadata = match metadata {
            Value::Null => Map::new(),
            Value::Object(metadata) => metadata,
            _ => {
                self.poison();
                return Err(PluginHostError::Uncertain {
                    package: self.package.clone(),
                    operation: format!("tool `{tool}` response"),
                    message: "metadata JSON was not an object".to_owned(),
                });
            }
        };
        Ok(PluginResult {
            title,
            output,
            metadata,
        })
    }

    async fn shutdown(&self) -> Result<(), PluginHostError> {
        if self.poisoned.load(Ordering::Acquire) {
            return match self.state.try_lock() {
                Ok(mut state) => {
                    state.take();
                    Ok(())
                }
                Err(_) => Err(PluginHostError::Stop {
                    package: self.package.clone(),
                    message:
                        "a withdrawn WASI worker is still running; cleanup is not authoritative"
                            .to_owned(),
                }),
            };
        }
        let result = self
            .call("shutdown", None, false, |instance| {
                instance
                    .shutdown
                    .call(&mut instance.store, ())
                    .map(|(result,)| result)
                    .map_err(|error| error.to_string())
            })
            .await;
        self.poison();
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(PluginHostError::Stop {
                package: self.package.clone(),
                message: self.redactor.safe(message),
            }),
            Err(PluginHostError::Timeout { .. }) => Err(PluginHostError::Stop {
                package: self.package.clone(),
                message: format!("shutdown exceeded {:?}", self.timeout),
            }),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One side wins: a withdrawn call can never enter the guest afterwards.
    ///
    /// This is the whole certainty claim for a cancellation serviced while the call was
    /// still queued behind the instance lock. With a plain flag the interrupt arm could
    /// report "nothing ran" and the worker could then acquire the lock, re-arm its epoch
    /// deadline past the increment, and run the guest anyway.
    #[test]
    fn a_withdrawn_wasi_call_cannot_still_enter_the_guest() {
        let withdrawn = GuestEntry::new();
        assert!(withdrawn.abandon(), "an unclaimed call can be withdrawn");
        assert!(
            !withdrawn.claim(),
            "a withdrawn call must not reach the guest after the outcome was reported"
        );

        let running = GuestEntry::new();
        assert!(running.claim(), "an unclaimed call may enter the guest");
        assert!(
            !running.abandon(),
            "a call already in the guest cannot be reported as never dispatched"
        );
        assert!(!running.claim(), "the guest is entered exactly once");
    }
}
