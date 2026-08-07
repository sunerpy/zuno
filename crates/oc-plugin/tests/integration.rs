#[cfg(all(feature = "wasm", unix))]
mod enabled {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use oc_engine::terminal_lease::{TerminalBroker, TerminalLease};
    use oc_error::BoxSource;
    use oc_paths::ResolvedProject;
    use oc_plugin::{
        HookBus, HookInvocation, HookName, JsDiagnosticKind, JsHostConfig, JsHostPolicy,
        JsPluginLoad, JsPluginSpec, Plugin, PluginDiagnosticKind, PluginLoad, PluginManifest,
        PluginProcessSpec, TextCompleteInput, TextCompleteOutput, WasmPluginLoad, WasmPluginSpec,
        WasmResourceLimits, load_js_plugins_ordered, load_plugins_ordered,
        load_wasm_plugins_ordered,
    };
    use oc_testkit::FakeTerminalOwner;
    use url::Url;

    const JS_FIXTURE: &str = r#"
import { appendFileSync, writeFileSync } from "node:fs";

export default {
  id: "integration-js",
  server: async (_input, options) => {
    writeFileSync(options.pidFile, String(process.pid));
    return {
      dispose: async () => appendFileSync(options.disposeFile, "js\n"),
      "experimental.text.complete": async (_input, output) => {
        output.text = `[${output.text}]`;
      },
    };
  },
};
"#;

    type HealthCheck = Arc<dyn Fn() -> bool + Send + Sync>;
    type TextMutation = fn(&mut TextCompleteOutput);

    struct TrackedPlugin {
        inner: Arc<dyn Plugin>,
        manifest: PluginManifest,
        tier: &'static str,
        disposed: Arc<Mutex<Vec<&'static str>>>,
        health: HealthCheck,
        text_mutation: Option<TextMutation>,
    }

    impl TrackedPlugin {
        fn new(
            inner: Arc<dyn Plugin>,
            tier: &'static str,
            disposed: Arc<Mutex<Vec<&'static str>>>,
            health: HealthCheck,
            text_mutation: Option<TextMutation>,
        ) -> Arc<Self> {
            let mut hooks = inner.manifest().hooks().to_vec();
            if !hooks.contains(&HookName::Dispose) {
                hooks.push(HookName::Dispose);
            }
            let manifest = PluginManifest::new(format!("integration-{tier}"), hooks)
                .expect("tracked plugin manifest");
            Arc::new(Self {
                inner,
                manifest,
                tier,
                disposed,
                health,
                text_mutation,
            })
        }
    }

    #[async_trait]
    impl Plugin for TrackedPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
            if !(self.health)() {
                return Ok(());
            }
            let name = hook.name();
            if self.inner.manifest().supports(name) {
                self.inner.call(hook).await?;
            }
            if !(self.health)() {
                return Ok(());
            }
            match hook {
                HookInvocation::Dispose => lock(&self.disposed).push(self.tier),
                HookInvocation::TextComplete { output, .. } => {
                    if let Some(mutate) = self.text_mutation {
                        mutate(output);
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum Tier {
        Rust,
        Wasm,
        Js,
    }

    struct TierSession {
        bus: HookBus,
        rust: PluginLoad,
        wasm: WasmPluginLoad,
        js: JsPluginLoad,
        rust_pid: u32,
        js_pid: Option<u32>,
        disposed: Arc<Mutex<Vec<&'static str>>>,
        js_dispose_file: PathBuf,
    }

    impl TierSession {
        async fn load(
            root: &Path,
            wasm_component: Vec<u8>,
            order: &[Tier],
            rust_hook_sleep: Duration,
            js_search_path: Option<OsString>,
        ) -> Self {
            let rust_pid_file = root.join("rust.pid");
            let rust = load_plugins_ordered(vec![rust_spec(&rust_pid_file, rust_hook_sleep)]).await;
            assert_eq!(rust.plugins().len(), 1, "{:?}", rust.diagnostics());
            let rust_pid = read_pid(&rust_pid_file);

            let js_pid_file = root.join("js.pid");
            let js_dispose_file = root.join("js.dispose");
            let js_entry = write_js_fixture(root);
            let mut js_config = host_config(root);
            if let Some(path) = js_search_path {
                js_config = js_config.runtime_search_path(path);
            }
            let js = load_js_plugins_ordered(
                vec![
                    JsPluginSpec::new(format!("file:{}", js_entry.display())).options(
                        serde_json::json!({
                            "pidFile": js_pid_file,
                            "disposeFile": js_dispose_file,
                        }),
                    ),
                ],
                js_config,
            )
            .await;
            let js_pid = (!js.plugins().is_empty()).then(|| read_pid(&js_pid_file));

            let wasm = load_wasm_plugins_ordered(vec![
                WasmPluginSpec::new("integration-wasm", wasm_component).limits(
                    WasmResourceLimits {
                        fuel: 10_000,
                        epoch_deadline: Duration::from_millis(200),
                        memory_bytes: 1024 * 1024,
                    },
                ),
            ]);
            assert_eq!(wasm.plugins().len(), 1, "{:?}", wasm.diagnostics());

            let disposed = Arc::new(Mutex::new(Vec::new()));
            let rust_plugin = Arc::clone(&rust.plugins()[0]);
            let rust_health_plugin = Arc::clone(&rust_plugin);
            let rust_tracked: Arc<dyn Plugin> = TrackedPlugin::new(
                rust_plugin,
                "rust",
                Arc::clone(&disposed),
                Arc::new(move || rust_health_plugin.is_enabled()),
                None,
            );

            let wasm_plugin = Arc::clone(&wasm.plugins()[0]);
            let wasm_health_plugin = Arc::clone(&wasm_plugin);
            let wasm_tracked: Arc<dyn Plugin> = TrackedPlugin::new(
                wasm_plugin,
                "wasm",
                Arc::clone(&disposed),
                Arc::new(move || wasm_health_plugin.is_enabled()),
                Some(duplicate_text),
            );

            let js_tracked = js.plugins().first().map(|plugin| {
                let plugin = Arc::clone(plugin);
                let health_plugin = Arc::clone(&plugin);
                TrackedPlugin::new(
                    plugin,
                    "js",
                    Arc::clone(&disposed),
                    Arc::new(move || health_plugin.diagnostics().is_empty()),
                    None,
                ) as Arc<dyn Plugin>
            });

            let plugins = order
                .iter()
                .filter_map(|tier| match tier {
                    Tier::Rust => Some(Arc::clone(&rust_tracked)),
                    Tier::Wasm => Some(Arc::clone(&wasm_tracked)),
                    Tier::Js => js_tracked.clone(),
                })
                .collect();

            Self {
                bus: HookBus::new(plugins),
                rust,
                wasm,
                js,
                rust_pid,
                js_pid,
                disposed,
                js_dispose_file,
            }
        }

        async fn complete(&self, initial: &str) -> String {
            let mut output = TextCompleteOutput {
                text: initial.to_owned(),
            };
            self.bus
                .dispatch(HookInvocation::TextComplete {
                    input: &TextCompleteInput {
                        session_id: "session",
                        message_id: "message",
                        part_id: "part",
                    },
                    output: &mut output,
                })
                .await
                .expect("tier failures are contained by their hosts");
            output.text
        }

        async fn dispose(&self) {
            self.bus
                .dispatch(HookInvocation::Dispose)
                .await
                .expect("dispose surviving tiers");
        }

        async fn shutdown(&self) {
            tokio::join!(self.rust.shutdown(), self.js.shutdown());
        }

        fn pids(&self) -> Vec<u32> {
            let mut pids = vec![self.rust_pid];
            pids.extend(self.js_pid);
            pids
        }
    }

    #[tokio::test]
    async fn three_tiers_follow_configuration_order() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let session = TierSession::load(
            temp.path(),
            healthy_wasm_component(),
            &[Tier::Js, Tier::Rust, Tier::Wasm],
            Duration::ZERO,
            None,
        )
        .await;
        assert_eq!(
            session.js.plugins().len(),
            1,
            "{:?}",
            session.js.diagnostics()
        );
        assert!(session.rust.diagnostics().is_empty());
        assert!(session.wasm.diagnostics().is_empty());
        assert_eq!(
            session
                .bus
                .plugins()
                .iter()
                .map(|plugin| plugin.manifest().id())
                .collect::<Vec<_>>(),
            ["integration-js", "integration-rust", "integration-wasm"]
        );

        let output = session.complete("x").await;

        assert_eq!(output, "[x]!|[x]!");
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&session.pids()).await;
    }

    #[tokio::test]
    async fn killed_jsonrpc_degrades_only_the_rust_tier() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let session = TierSession::load(
            temp.path(),
            healthy_wasm_component(),
            &[Tier::Rust, Tier::Wasm, Tier::Js],
            Duration::from_secs(2),
            None,
        )
        .await;
        assert_eq!(
            session.js.plugins().len(),
            1,
            "{:?}",
            session.js.diagnostics()
        );
        let pid = session.rust_pid;
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            kill_process(pid);
        });

        let output = session.complete("x").await;
        killer.await.expect("PID killer task");

        assert_eq!(output, "[x|x]");
        assert!(!session.rust.plugins()[0].is_enabled());
        let diagnostics = session.rust.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::Crashed);
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&session.pids()).await;
    }

    #[tokio::test]
    async fn runaway_wasm_degrades_only_the_wasm_tier() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let session = TierSession::load(
            temp.path(),
            runaway_wasm_component(),
            &[Tier::Rust, Tier::Wasm, Tier::Js],
            Duration::ZERO,
            None,
        )
        .await;
        assert_eq!(
            session.js.plugins().len(),
            1,
            "{:?}",
            session.js.diagnostics()
        );

        let output = session.complete("x").await;

        assert_eq!(output, "[x!]");
        assert!(session.rust.plugins()[0].is_enabled());
        let diagnostics = session.wasm.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::TimedOut);
        assert!(!session.wasm.plugins()[0].is_enabled());
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&session.pids()).await;
    }

    #[tokio::test]
    async fn missing_js_runtime_degrades_only_the_js_tier() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let empty_path = temp.path().join("no-js-runtime");
        std::fs::create_dir(&empty_path).expect("empty runtime search directory");
        let session = TierSession::load(
            temp.path(),
            healthy_wasm_component(),
            &[Tier::Rust, Tier::Js, Tier::Wasm],
            Duration::ZERO,
            Some(OsString::from(empty_path)),
        )
        .await;

        assert!(session.js.plugins().is_empty());
        assert_eq!(session.js.diagnostics().len(), 1);
        assert_eq!(
            session.js.diagnostics()[0].kind,
            JsDiagnosticKind::MissingRuntime
        );
        let output = session.complete("x").await;
        assert_eq!(output, "x!|x!");
        assert!(session.rust.plugins()[0].is_enabled());
        assert!(session.wasm.plugins()[0].is_enabled());
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&session.pids()).await;
    }

    #[tokio::test]
    async fn dispose_reaches_every_survivor_after_a_sibling_crash() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let session = TierSession::load(
            temp.path(),
            healthy_wasm_component(),
            &[Tier::Rust, Tier::Wasm, Tier::Js],
            Duration::ZERO,
            None,
        )
        .await;
        assert_eq!(
            session.js.plugins().len(),
            1,
            "{:?}",
            session.js.diagnostics()
        );
        kill_process(session.rust_pid);
        wait_until(Duration::from_secs(3), || {
            !session.rust.plugins()[0].is_enabled()
        })
        .await;

        session.dispose().await;

        let disposed = lock(&session.disposed).clone();
        for tier in ["wasm", "js"] {
            assert!(
                disposed.contains(&tier),
                "dispose did not reach surviving {tier} tier; observed {disposed:?}"
            );
        }
        assert!(
            !disposed.contains(&"rust"),
            "crashed rust tier is not a surviving plugin: {disposed:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&session.js_dispose_file).expect("JavaScript dispose marker"),
            "js\n"
        );
        session.shutdown().await;
        wait_for_processes_to_exit(&session.pids()).await;
    }

    #[tokio::test]
    async fn shutdown_reaps_every_recorded_child_pid() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let session = TierSession::load(
            temp.path(),
            healthy_wasm_component(),
            &[Tier::Rust, Tier::Wasm, Tier::Js],
            Duration::ZERO,
            None,
        )
        .await;
        assert_eq!(
            session.js.plugins().len(),
            1,
            "{:?}",
            session.js.diagnostics()
        );
        let pids = session.pids();
        assert_eq!(pids.len(), 2, "both owned child PIDs must be recorded");
        assert!(pids.iter().all(|pid| process_exists(*pid)));

        session.dispose().await;
        session.shutdown().await;

        wait_for_processes_to_exit(&pids).await;
    }

    fn rust_spec(pid_file: &Path, hook_sleep: Duration) -> PluginProcessSpec {
        PluginProcessSpec::new("integration-rust", "/bin/sh")
            .arg("-c")
            .arg("printf '%s' \"$$\" > \"$1\"; exec \"$2\"")
            .arg("oc-integration-rust-wrapper")
            .arg(pid_file.as_os_str())
            .arg(env!("CARGO_BIN_EXE_oc-example-plugin"))
            .env("OC_EXAMPLE_PLUGIN_ID", "integration-rust")
            .env(
                "OC_EXAMPLE_SLEEP_HOOK_MS",
                hook_sleep.as_millis().to_string(),
            )
            .timeout(Duration::from_secs(4))
    }

    fn host_config(root: &Path) -> JsHostConfig {
        let owner = Arc::new(FakeTerminalOwner::new());
        let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
        JsHostConfig::new(
            ResolvedProject {
                previous: None,
                id: "integration-project".to_owned(),
                directory: root.to_path_buf(),
                vcs: None,
            },
            Url::parse("http://127.0.0.1:4096").expect("integration server URL"),
            terminal,
        )
        .directory(root)
        .worktree(root)
        .cache_dir(root.join("cache"))
        .policy(JsHostPolicy::default())
    }

    fn write_js_fixture(root: &Path) -> PathBuf {
        let path = root.join("integration-plugin.mjs");
        std::fs::write(&path, JS_FIXTURE).expect("write JavaScript integration fixture");
        path
    }

    fn duplicate_text(output: &mut TextCompleteOutput) {
        output.text = format!("{0}|{0}", output.text);
    }

    fn healthy_wasm_component() -> Vec<u8> {
        wasm_component(
            "i32.const 0 i32.const 64 i32.store i32.const 4 i32.const 4 i32.store i32.const 0",
            "null",
        )
    }

    fn runaway_wasm_component() -> Vec<u8> {
        wasm_component("(loop $forever br $forever) i32.const 0", "")
    }

    fn wasm_component(body: &str, data: &str) -> Vec<u8> {
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
                    (data (i32.const 64) "{data}")
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
                (export "dispose" (func $hook))
                (export "experimental-text-complete" (func $hook))
            )"#
        )
        .into_bytes()
    }

    fn read_pid(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read recorded PID {}: {error}", path.display()))
            .parse()
            .unwrap_or_else(|error| panic!("parse recorded PID {}: {error}", path.display()))
    }

    fn kill_process(pid: u32) {
        let status = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap_or_else(|error| panic!("kill JSON-RPC child {pid}: {error}"));
        assert!(status.success(), "kill JSON-RPC child {pid}: {status}");
    }

    fn process_exists(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    async fn wait_for_processes_to_exit(pids: &[u32]) {
        wait_until(Duration::from_secs(3), || {
            pids.iter().all(|pid| !process_exists(*pid))
        })
        .await;
        let orphans = pids
            .iter()
            .copied()
            .filter(|pid| process_exists(*pid))
            .collect::<Vec<_>>();
        assert!(
            orphans.is_empty(),
            "owned child PIDs were not reaped: {orphans:?}"
        );
    }

    async fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + timeout;
        while !predicate() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(not(all(feature = "wasm", unix)))]
macro_rules! wasm_integration_skip {
    ($name:ident) => {
        #[test]
        fn $name() {
            eprintln!(concat!(
                "skipping ",
                stringify!($name),
                ": the three-tier integration suite requires the `wasm` feature and Unix PID controls"
            ));
        }
    };
}

#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(three_tiers_follow_configuration_order);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(killed_jsonrpc_degrades_only_the_rust_tier);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(runaway_wasm_degrades_only_the_wasm_tier);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(missing_js_runtime_degrades_only_the_js_tier);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(dispose_reaches_every_survivor_after_a_sibling_crash);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(shutdown_reaps_every_recorded_child_pid);
