pub mod bridge;
mod config;
pub mod host;
mod loader;
mod plugin;
pub mod runtime;
pub mod spec;

pub use config::{JsHostConfig, JsHostPolicy};
pub use host::{
    JS_PROTOCOL_VERSION, JsHandle, JsHost, JsHostBuilder, JsHostError, JsHostLimits, JsInitReport,
    JsPluginInput, LimitBreach, SHIM_SOURCE,
};
pub use loader::{
    JS_COMPAT_OPENCODE_VERSION, JsDiagnostic, JsDiagnosticKind, JsPluginLoad, SUPPORTED_JS_PLUGINS,
    SupportedJsPlugin, load_js_plugins_ordered,
};
pub use plugin::JsPlugin;
pub use runtime::{
    JsRuntime, JsRuntimeKind, MissingJsRuntime, discover_runtime, discover_runtime_in,
};
pub use spec::{
    JsPluginSpec, PLUGIN_PEER_PACKAGE, PackageManifest, PluginKind as JsPluginKind, PluginSource,
    REPORTED_PLUGIN_API_VERSION, ResolvedJsPlugin, SpecError, VersionGate,
};
