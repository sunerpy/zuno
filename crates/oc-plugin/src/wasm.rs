//! Optional in-process WebAssembly component plugins.
//!
//! Components receive hook input and mutable output as JSON strings. The empty
//! component linker is the capability boundary: imports, including WASI
//! filesystem and socket interfaces, are rejected before instantiation. A
//! future capability grant must be represented explicitly in [`WasmPluginSpec`]
//! and linked one interface at a time; installing a broad WASI context here
//! would violate the host's security contract.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use async_trait::async_trait;
use oc_error::BoxSource;
use serde::Deserialize;
use serde_json::{Value, json};
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Instance, Linker, Val};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};

use crate::{
    ChatSystemTransformOutput, HookInvocation, HookName, Plugin, PluginDiagnostic,
    PluginDiagnosticKind, PluginManifest,
};

/// The guest contract implemented by a component plugin.
///
/// Every function corresponds one-for-one with [`HookName::ALL`]. JSON keeps
/// the component ABI stable while the Rust payload structs evolve. A guest
/// returns the complete replacement output JSON; hooks without mutable output
/// return `null`.
pub const WASM_HOOK_WIT: &str = r#"
package opencode:plugin;

world hooks {
    export dispose: func(input-json: string, output-json: string) -> string;
    export event: func(input-json: string, output-json: string) -> string;
    export config: func(input-json: string, output-json: string) -> string;
    export tool: func(input-json: string, output-json: string) -> string;
    export auth: func(input-json: string, output-json: string) -> string;
    export provider: func(input-json: string, output-json: string) -> string;
    export chat-message: func(input-json: string, output-json: string) -> string;
    export chat-params: func(input-json: string, output-json: string) -> string;
    export chat-headers: func(input-json: string, output-json: string) -> string;
    export permission-ask: func(input-json: string, output-json: string) -> string;
    export command-execute-before: func(input-json: string, output-json: string) -> string;
    export tool-execute-before: func(input-json: string, output-json: string) -> string;
    export shell-env: func(input-json: string, output-json: string) -> string;
    export tool-execute-after: func(input-json: string, output-json: string) -> string;
    export experimental-chat-messages-transform: func(input-json: string, output-json: string) -> string;
    export experimental-chat-system-transform: func(input-json: string, output-json: string) -> string;
    export experimental-provider-small-model: func(input-json: string, output-json: string) -> string;
    export experimental-session-compacting: func(input-json: string, output-json: string) -> string;
    export experimental-compaction-autocontinue: func(input-json: string, output-json: string) -> string;
    export experimental-text-complete: func(input-json: string, output-json: string) -> string;
    export tool-definition: func(input-json: string, output-json: string) -> string;
}
"#;

const HOOK_EXPORTS: [(HookName, &str); 21] = [
    (HookName::Dispose, "dispose"),
    (HookName::Event, "event"),
    (HookName::Config, "config"),
    (HookName::Tool, "tool"),
    (HookName::Auth, "auth"),
    (HookName::Provider, "provider"),
    (HookName::ChatMessage, "chat-message"),
    (HookName::ChatParams, "chat-params"),
    (HookName::ChatHeaders, "chat-headers"),
    (HookName::PermissionAsk, "permission-ask"),
    (HookName::CommandExecuteBefore, "command-execute-before"),
    (HookName::ToolExecuteBefore, "tool-execute-before"),
    (HookName::ShellEnv, "shell-env"),
    (HookName::ToolExecuteAfter, "tool-execute-after"),
    (
        HookName::ChatMessagesTransform,
        "experimental-chat-messages-transform",
    ),
    (
        HookName::ChatSystemTransform,
        "experimental-chat-system-transform",
    ),
    (
        HookName::ProviderSmallModel,
        "experimental-provider-small-model",
    ),
    (
        HookName::SessionCompacting,
        "experimental-session-compacting",
    ),
    (
        HookName::CompactionAutocontinue,
        "experimental-compaction-autocontinue",
    ),
    (HookName::TextComplete, "experimental-text-complete"),
    (HookName::ToolDefinition, "tool-definition"),
];

/// CPU, wall-clock, and linear-memory budgets applied independently to each
/// component invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmResourceLimits {
    pub fuel: u64,
    pub epoch_deadline: Duration,
    pub memory_bytes: usize,
}

impl Default for WasmResourceLimits {
    fn default() -> Self {
        Self {
            fuel: 100_000,
            epoch_deadline: Duration::from_millis(100),
            memory_bytes: 16 * 1024 * 1024,
        }
    }
}

/// One configured component and its independent resource policy.
#[derive(Debug, Clone)]
pub struct WasmPluginSpec {
    name: String,
    component: Arc<[u8]>,
    limits: WasmResourceLimits,
}

impl WasmPluginSpec {
    /// Create an import-free component specification from binary WebAssembly or
    /// component text format.
    #[must_use]
    pub fn new(name: impl Into<String>, component: impl Into<Arc<[u8]>>) -> Self {
        Self {
            name: name.into(),
            component: component.into(),
            limits: WasmResourceLimits::default(),
        }
    }

    /// Override the default per-invocation resource budgets.
    #[must_use]
    pub const fn limits(mut self, limits: WasmResourceLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Ordered component load result with contained startup and runtime failures.
pub struct WasmPluginLoad {
    plugins: Vec<Arc<WasmPlugin>>,
    startup_diagnostics: Vec<PluginDiagnostic>,
}

impl WasmPluginLoad {
    #[must_use]
    pub fn plugins(&self) -> &[Arc<WasmPlugin>] {
        &self.plugins
    }

    /// Adapt successful components to the shared sequential hook bus.
    #[must_use]
    pub fn hook_bus(&self) -> crate::HookBus {
        crate::HookBus::new(
            self.plugins
                .iter()
                .cloned()
                .map(|plugin| plugin as Arc<dyn Plugin>)
                .collect(),
        )
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        let mut diagnostics = self.startup_diagnostics.clone();
        for plugin in &self.plugins {
            diagnostics.extend(plugin.diagnostics());
        }
        diagnostics
    }
}

/// Compile and instantiate components in configuration order.
#[must_use]
pub fn load_wasm_plugins_ordered(specs: Vec<WasmPluginSpec>) -> WasmPluginLoad {
    let mut plugins = Vec::new();
    let mut startup_diagnostics = Vec::new();
    for spec in specs {
        match WasmPlugin::load(spec) {
            Ok(plugin) => plugins.push(Arc::new(plugin)),
            Err(diagnostic) => startup_diagnostics.push(diagnostic),
        }
    }
    WasmPluginLoad {
        plugins,
        startup_diagnostics,
    }
}

/// A resident component instance adapted to the common [`Plugin`] interface.
pub struct WasmPlugin {
    manifest: PluginManifest,
    engine: Engine,
    limits: WasmResourceLimits,
    runtime: Mutex<Runtime>,
    enabled: AtomicBool,
    diagnostics: Mutex<Vec<PluginDiagnostic>>,
}

struct Runtime {
    store: Store<HostState>,
    instance: Instance,
}

struct HostState {
    limits: StoreLimits,
}

impl WasmPlugin {
    fn load(spec: WasmPluginSpec) -> Result<Self, PluginDiagnostic> {
        let configured_name = spec.name.clone();
        let result = Self::try_load(spec);
        result.map_err(|error| PluginDiagnostic {
            plugin: configured_name,
            hook: None,
            kind: PluginDiagnosticKind::FailedToLoad,
            message: error.to_string(),
        })
    }

    fn try_load(spec: WasmPluginSpec) -> Result<Self, WasmHostError> {
        if spec.name.trim().is_empty() {
            return Err(WasmHostError::Protocol(
                "component plugin name must not be empty".to_owned(),
            ));
        }
        if spec.limits.fuel == 0
            || spec.limits.epoch_deadline.is_zero()
            || spec.limits.memory_bytes == 0
        {
            return Err(WasmHostError::Protocol(
                "component resource limits must all be non-zero".to_owned(),
            ));
        }

        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        let engine = Engine::new(&config).map_err(WasmHostError::Runtime)?;
        let component =
            Component::new(&engine, spec.component.as_ref()).map_err(WasmHostError::Runtime)?;

        let component_type = component.component_type();
        let imports = component_type
            .imports(&engine)
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>();
        if !imports.is_empty() {
            return Err(WasmHostError::CapabilityDenied(imports.join(", ")));
        }

        let hooks = HOOK_EXPORTS
            .iter()
            .filter_map(|(hook, export)| {
                component_type
                    .get_export(&engine, export)
                    .filter(|item| matches!(item.ty, ComponentItem::ComponentFunc(_)))
                    .map(|_| *hook)
            })
            .collect::<Vec<_>>();
        if hooks.is_empty() {
            return Err(WasmHostError::Protocol(
                "component exports no opencode hook functions".to_owned(),
            ));
        }
        let manifest = PluginManifest::new(&spec.name, hooks)
            .map_err(|error| WasmHostError::Protocol(error.to_string()))?;

        let host_state = HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(spec.limits.memory_bytes)
                .table_elements(10_000)
                .instances(32)
                .tables(8)
                .memories(8)
                .trap_on_grow_failure(true)
                .build(),
        };
        let mut store = Store::new(&engine, host_state);
        store.limiter(|state| &mut state.limits);
        let linker = Linker::new(&engine);
        let instance = run_bounded(&engine, &mut store, spec.limits, |store| {
            linker
                .instantiate(store, &component)
                .map_err(WasmHostError::Runtime)
        })?;

        Ok(Self {
            manifest,
            engine,
            limits: spec.limits,
            runtime: Mutex::new(Runtime { store, instance }),
            enabled: AtomicBool::new(true),
            diagnostics: Mutex::new(Vec::new()),
        })
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        lock(&self.diagnostics).clone()
    }

    fn dispatch_component(&self, hook: &mut HookInvocation<'_>) {
        if !self.is_enabled() {
            return;
        }
        let name = hook.name();
        let export = hook_export(name);
        let (input, output) = encode_hook(hook);
        let mut runtime = lock(&self.runtime);
        let Runtime { store, instance } = &mut *runtime;
        let result = run_bounded(&self.engine, store, self.limits, |store| {
            let function = instance.get_func(&mut *store, export).ok_or_else(|| {
                WasmHostError::Protocol(format!("component lost hook export `{export}`"))
            })?;
            let mut results = [Val::String(String::new())];
            function
                .call(
                    &mut *store,
                    &[Val::String(input), Val::String(output)],
                    &mut results,
                )
                .map_err(WasmHostError::Runtime)?;
            match std::mem::replace(&mut results[0], Val::String(String::new())) {
                Val::String(value) => Ok(value),
                other => Err(WasmHostError::Protocol(format!(
                    "hook `{export}` returned {}, expected string",
                    val_kind(&other)
                ))),
            }
        })
        .and_then(|output| apply_hook_output(hook, &output));
        if let Err(error) = result {
            self.disable(name, error);
        }
    }

    fn disable(&self, hook: HookName, error: WasmHostError) {
        if self.enabled.swap(false, Ordering::SeqCst) {
            let diagnostic = PluginDiagnostic {
                plugin: self.manifest.id().to_owned(),
                hook: Some(hook.to_string()),
                kind: error.diagnostic_kind(),
                message: error.to_string(),
            };
            tracing::warn!(
                plugin = %diagnostic.plugin,
                hook = ?diagnostic.hook,
                message = %diagnostic.message,
                "disabled WebAssembly component plugin"
            );
            lock(&self.diagnostics).push(diagnostic);
        }
    }
}

#[async_trait]
impl Plugin for WasmPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
        self.dispatch_component(hook);
        Ok(())
    }
}

fn run_bounded<T>(
    engine: &Engine,
    store: &mut Store<HostState>,
    limits: WasmResourceLimits,
    operation: impl FnOnce(&mut Store<HostState>) -> Result<T, WasmHostError>,
) -> Result<T, WasmHostError> {
    store
        .set_fuel(limits.fuel)
        .map_err(WasmHostError::Runtime)?;
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();

    let (finished, receiver) = mpsc::channel();
    let epoch_engine = engine.clone();
    let timer = std::thread::Builder::new()
        .name("oc-wasm-epoch".to_owned())
        .spawn(move || {
            if receiver.recv_timeout(limits.epoch_deadline).is_err() {
                epoch_engine.increment_epoch();
            }
        })
        .map_err(|error| WasmHostError::Timer(error.to_string()))?;
    let result = operation(store);
    let _finished = finished.send(());
    timer
        .join()
        .map_err(|_| WasmHostError::Timer("epoch timer panicked".to_owned()))?;
    result
}

fn encode_hook(hook: &HookInvocation<'_>) -> (String, String) {
    let name = hook.name().as_str();
    let (input, output) = match hook {
        HookInvocation::ChatSystemTransform { input, output } => (
            json!({
                "sessionID": input.session_id,
                "model": input.model,
            }),
            json!({ "system": output.system }),
        ),
        _ => (json!({ "hook": name }), Value::Null),
    };
    (input.to_string(), output.to_string())
}

fn apply_hook_output(hook: &mut HookInvocation<'_>, output: &str) -> Result<(), WasmHostError> {
    match hook {
        HookInvocation::ChatSystemTransform { output: target, .. } => {
            let decoded: WireSystemOutput = serde_json::from_str(output)
                .map_err(|error| WasmHostError::Protocol(error.to_string()))?;
            **target = ChatSystemTransformOutput {
                system: decoded.system,
            };
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
struct WireSystemOutput {
    system: Vec<String>,
}

fn hook_export(hook: HookName) -> &'static str {
    HOOK_EXPORTS
        .iter()
        .find_map(|(candidate, export)| (*candidate == hook).then_some(*export))
        .expect("every HookName must have a WIT export")
}

fn val_kind(value: &Val) -> &'static str {
    match value {
        Val::Bool(_) => "bool",
        Val::S8(_) => "s8",
        Val::U8(_) => "u8",
        Val::S16(_) => "s16",
        Val::U16(_) => "u16",
        Val::S32(_) => "s32",
        Val::U32(_) => "u32",
        Val::S64(_) => "s64",
        Val::U64(_) => "u64",
        Val::Float32(_) => "float32",
        Val::Float64(_) => "float64",
        Val::Char(_) => "char",
        Val::String(_) => "string",
        Val::List(_) => "list",
        Val::Map(_) => "map",
        Val::Record(_) => "record",
        Val::Tuple(_) => "tuple",
        Val::Variant(_, _) => "variant",
        Val::Enum(_) => "enum",
        Val::Option(_) => "option",
        Val::Result(_) => "result",
        Val::Flags(_) => "flags",
        Val::Resource(_) => "resource",
        Val::Future(_) => "future",
        Val::Stream(_) => "stream",
        Val::ErrorContext(_) => "error-context",
    }
}

#[derive(Debug, thiserror::Error)]
enum WasmHostError {
    #[error("WebAssembly component runtime failed: {0}")]
    Runtime(wasmtime::Error),
    #[error("WebAssembly component protocol failed: {0}")]
    Protocol(String),
    #[error(
        "component requested denied host imports: {0}; grant named capabilities explicitly instead of installing ambient WASI"
    )]
    CapabilityDenied(String),
    #[error("WebAssembly epoch timer failed: {0}")]
    Timer(String),
}

impl WasmHostError {
    fn diagnostic_kind(&self) -> PluginDiagnosticKind {
        match self {
            Self::Runtime(error)
                if matches!(
                    error.downcast_ref::<Trap>(),
                    Some(Trap::OutOfFuel | Trap::Interrupt)
                ) =>
            {
                PluginDiagnosticKind::TimedOut
            }
            Self::Protocol(_) => PluginDiagnosticKind::Protocol,
            Self::Runtime(_) | Self::CapabilityDenied(_) | Self::Timer(_) => {
                PluginDiagnosticKind::Crashed
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Instant;

    use oc_llm::catalog::models_dev::CatalogStatus;
    use oc_llm::catalog::resolved::{
        ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel,
    };

    use super::*;
    use crate::ChatSystemTransformInput;

    fn model() -> ResolvedModel {
        ResolvedModel {
            id: "model".to_owned(),
            provider_id: "provider".to_owned(),
            name: "Model".to_owned(),
            family: "test".to_owned(),
            release_date: String::new(),
            status: CatalogStatus::Active,
            api: ModelApi::default(),
            capabilities: ModelCapabilities::default(),
            cost: ModelCost::default(),
            limit: ModelLimit::default(),
            options: serde_json::Map::new(),
            headers: BTreeMap::new(),
            variants: BTreeMap::new(),
        }
    }

    fn system_component(body: &str, data: &str) -> String {
        let escaped = data.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            r#"(component
                (core module $m
                    (memory (export "memory") 1)
                    (global $heap (mut i32) (i32.const 4096))
                    (func (export "realloc")
                        (param i32 i32 i32 i32) (result i32)
                        global.get $heap
                        global.get $heap
                        local.get 3
                        i32.add
                        global.set $heap)
                    (data (i32.const 64) "{escaped}")
                    (func (export "hook")
                        (param i32 i32 i32 i32) (result i32)
                        {body})
                )
                (core instance $i (instantiate $m))
                (alias core export $i "memory" (core memory $memory))
                (alias core export $i "realloc" (core func $realloc))
                (type $hook (func
                    (param "input-json" string)
                    (param "output-json" string)
                    (result string)))
                (func $hook (type $hook)
                    (canon lift (core func $i "hook")
                        (memory $memory)
                        (realloc $realloc)
                        string-encoding=utf8))
                (export "experimental-chat-system-transform" (func $hook))
            )"#
        )
    }

    fn returning_component(output: &str) -> String {
        let body = format!(
            "i32.const 0 i32.const 64 i32.store \
             i32.const 4 i32.const {} i32.store i32.const 0",
            output.len()
        );
        system_component(&body, output)
    }

    #[test]
    fn wasm_wit_mirrors_every_authoritative_hook() {
        assert_eq!(HOOK_EXPORTS.map(|(hook, _)| hook), HookName::ALL);
        for (_, export) in HOOK_EXPORTS {
            assert!(
                WASM_HOOK_WIT.contains(&format!("export {export}: func")),
                "WIT is missing {export}"
            );
        }
    }

    #[tokio::test]
    async fn wasm_system_hook_mutation_is_applied() {
        let component = returning_component(r#"{"system":["base","from wasm"]}"#);
        let load =
            load_wasm_plugins_ordered(vec![WasmPluginSpec::new("fixture", component.into_bytes())]);
        assert!(load.diagnostics().is_empty(), "{:?}", load.diagnostics());
        let model = model();
        let mut output = ChatSystemTransformOutput {
            system: vec!["base".to_owned()],
        };

        load.hook_bus()
            .dispatch(HookInvocation::ChatSystemTransform {
                input: &ChatSystemTransformInput {
                    session_id: Some("ses"),
                    model: &model,
                },
                output: &mut output,
            })
            .await
            .expect("WASM failures degrade to diagnostics");

        assert_eq!(output.system, ["base", "from wasm"]);
        assert!(load.diagnostics().is_empty(), "{:?}", load.diagnostics());
    }

    #[tokio::test]
    async fn wasm_runaway_is_fuel_halted_within_a_bounded_time() {
        let component = system_component("(loop $forever br $forever) i32.const 0", "");
        let limits = WasmResourceLimits {
            fuel: 10_000,
            epoch_deadline: Duration::from_millis(200),
            memory_bytes: 1024 * 1024,
        };
        let load = load_wasm_plugins_ordered(vec![
            WasmPluginSpec::new("runaway", component.into_bytes()).limits(limits),
        ]);
        assert!(load.diagnostics().is_empty(), "{:?}", load.diagnostics());
        let model = model();
        let mut output = ChatSystemTransformOutput::default();
        let started = Instant::now();

        load.hook_bus()
            .dispatch(HookInvocation::ChatSystemTransform {
                input: &ChatSystemTransformInput {
                    session_id: None,
                    model: &model,
                },
                output: &mut output,
            })
            .await
            .expect("runaway plugin is isolated");

        assert!(started.elapsed() < Duration::from_secs(2));
        let diagnostics = load.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::TimedOut);
        assert_eq!(
            diagnostics[0].hook.as_deref(),
            Some("experimental.chat.system.transform")
        );
        assert!(!load.plugins()[0].is_enabled());
    }

    #[tokio::test]
    async fn wasm_memory_growth_beyond_limit_disables_only_the_component() {
        let component = system_component("i32.const 32 memory.grow drop i32.const 0", "");
        let limits = WasmResourceLimits {
            fuel: 100_000,
            epoch_deadline: Duration::from_millis(200),
            memory_bytes: 64 * 1024,
        };
        let load = load_wasm_plugins_ordered(vec![
            WasmPluginSpec::new("memory", component.into_bytes()).limits(limits),
        ]);
        let model = model();
        let mut output = ChatSystemTransformOutput::default();

        load.hook_bus()
            .dispatch(HookInvocation::ChatSystemTransform {
                input: &ChatSystemTransformInput {
                    session_id: None,
                    model: &model,
                },
                output: &mut output,
            })
            .await
            .expect("memory failure is isolated");

        let diagnostics = load.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::Crashed);
        assert!(!load.plugins()[0].is_enabled());
    }

    #[test]
    fn wasm_filesystem_import_is_denied_with_a_diagnostic() {
        let component = br#"(component
            (type $filesystem (instance))
            (import "wasi:filesystem/types@0.2.0" (instance $filesystem))
        )"#
        .to_vec();

        let load = load_wasm_plugins_ordered(vec![WasmPluginSpec::new("fs", component)]);

        assert!(load.plugins().is_empty());
        let diagnostics = load.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::FailedToLoad);
        assert!(diagnostics[0].message.contains("wasi:filesystem/types"));
        assert!(diagnostics[0].message.contains("denied host imports"));
    }
}
