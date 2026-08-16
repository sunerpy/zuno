#[cfg(all(feature = "wasm", unix))]
mod enabled {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use url::Url;
    use zuno_engine::terminal_lease::{TerminalBroker, TerminalLease};
    use zuno_error::BoxSource;
    use zuno_llm::catalog::availability::Availability;
    use zuno_llm::catalog::models_dev::CatalogStatus;
    use zuno_llm::catalog::resolved::{
        ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel, ResolvedProvider,
    };
    use zuno_llm::event::{Message, Role};
    use zuno_paths::ResolvedProject;
    use zuno_plugin::{
        AuthHook, ChatContext, ChatHeadersOutput, HookBus, HookInvocation, HookName,
        JsDiagnosticKind, JsHostConfig, JsHostPolicy, JsPluginLoad, JsPluginSpec, Plugin,
        PluginDiagnosticKind, PluginLoad, PluginManifest, PluginProcessSpec, PluginTools,
        ProviderContext, ProviderHook, ProviderSource, SUPPORTED_JS_PLUGINS, TextCompleteInput,
        TextCompleteOutput, WasmPluginLoad, WasmPluginSpec, WasmResourceLimits,
        load_js_plugins_ordered, load_plugins_ordered, load_wasm_plugins_ordered,
    };
    use zuno_testkit::FakeTerminalOwner;

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

    /// An observation wrapper, not a stand-in for the plugin it wraps.
    ///
    /// It forwards every hook to `inner` — and only ever to hooks `inner`'s own
    /// manifest declares — while recording *where in configuration order the bus
    /// reached this plugin*. The recording is the only synthetic thing here:
    /// [`HookBus`] filters dispatch by manifest, so a hook no plugin shares
    /// (and the four plugins in the real coexistence tests share none, see
    /// [`three_tiers_follow_configuration_order`]) would otherwise leave order
    /// unobservable. The wrapped plugins' own behaviour is asserted separately,
    /// from the hooks they really implement.
    struct TrackedPlugin {
        inner: Arc<dyn Plugin>,
        manifest: PluginManifest,
        tier: &'static str,
        disposed: Arc<Mutex<Vec<&'static str>>>,
        observed: Arc<Mutex<Vec<String>>>,
        health: HealthCheck,
        text_mutation: Option<TextMutation>,
    }

    impl TrackedPlugin {
        fn new(
            inner: Arc<dyn Plugin>,
            tier: &'static str,
            disposed: Arc<Mutex<Vec<&'static str>>>,
            observed: Arc<Mutex<Vec<String>>>,
            health: HealthCheck,
            text_mutation: Option<TextMutation>,
        ) -> Arc<Self> {
            let mut hooks = inner.manifest().hooks().to_vec();
            // Dispose so a surviving tier is provably reached at teardown, and
            // TextComplete so one dispatch crosses every tier — including tiers
            // whose real hook set omits it, which `call` never forwards to.
            for required in [HookName::Dispose, HookName::TextComplete] {
                if !hooks.contains(&required) {
                    hooks.push(required);
                }
            }
            let manifest =
                PluginManifest::new(inner.manifest().id(), hooks).expect("tracked plugin manifest");
            Arc::new(Self {
                inner,
                manifest,
                tier,
                disposed,
                observed,
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

        fn tools(&self) -> PluginTools {
            if (self.health)() {
                self.inner.tools()
            } else {
                PluginTools::new()
            }
        }

        fn auth(&self) -> Option<AuthHook> {
            (self.health)().then(|| self.inner.auth()).flatten()
        }

        fn provider(&self) -> Option<ProviderHook> {
            (self.health)().then(|| self.inner.provider()).flatten()
        }

        async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
            if !(self.health)() {
                return Ok(());
            }
            let name = hook.name();
            lock(&self.observed).push(format!("{}:{name}", self.tier));
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
            let observed = Arc::new(Mutex::new(Vec::new()));
            let rust_plugin = Arc::clone(&rust.plugins()[0]);
            let rust_health_plugin = Arc::clone(&rust_plugin);
            let rust_tracked: Arc<dyn Plugin> = TrackedPlugin::new(
                rust_plugin,
                "rust",
                Arc::clone(&disposed),
                Arc::clone(&observed),
                Arc::new(move || rust_health_plugin.is_enabled()),
                None,
            );

            let wasm_plugin = Arc::clone(&wasm.plugins()[0]);
            let wasm_health_plugin = Arc::clone(&wasm_plugin);
            let wasm_tracked: Arc<dyn Plugin> = TrackedPlugin::new(
                wasm_plugin,
                "wasm",
                Arc::clone(&disposed),
                Arc::clone(&observed),
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
                    Arc::clone(&observed),
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

    /// Criterion 7 against the user's own auth plugins rather than a fixture.
    ///
    /// The four tiers share no mutating hook, and that is a property of the real
    /// packages, not a shortcut: measured on this host, antigravity registers
    /// `event`/`tool`/`auth`, kiro registers `config`/`auth`/`provider`/
    /// `chat.headers`, the example Rust plugin registers `chat.params`/
    /// `shell.env`/`experimental.text.complete`, and a WebAssembly component can
    /// only mutate `experimental.chat.system.transform` (`wasm.rs`'s
    /// `apply_hook_output`). So configuration order is asserted three ways: the
    /// bus's own plugin sequence, one dispatch crossing all four
    /// ([`TrackedPlugin`] records position), and the two real `auth` hooks
    /// composing into an order-sensitive vector — that last one being a real
    /// mutation by both real plugins.
    ///
    /// `config` is deliberately never dispatched. Kiro's real `config` hook calls
    /// `bootstrapAuthIfNeeded` (`dist/plugin/auth-bootstrap.js:44`), which writes
    /// a placeholder entry into the user's `~/.local/share/opencode/auth.json`.
    /// `effort` is likewise not asserted, for the reason recorded at
    /// `tests/js.rs`'s real-kiro header test: it is chosen inside the plugin's own
    /// AWS client on an outbound request needing live credentials and network.
    #[tokio::test]
    async fn three_tiers_follow_configuration_order() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let order = [
            RealTier::Antigravity,
            RealTier::Rust,
            RealTier::Kiro,
            RealTier::Wasm,
        ];
        let Some(session) = real_tiers_or_skip(
            "three_tiers_follow_configuration_order",
            temp.path(),
            healthy_wasm_component(),
            &order,
            None,
        )
        .await
        else {
            return;
        };
        assert!(session.rust.diagnostics().is_empty());
        assert!(session.wasm.diagnostics().is_empty());
        assert_eq!(
            session.ids(),
            [
                supported_spec(ANTIGRAVITY_PACKAGE).as_str(),
                "integration-rust",
                KIRO_PROVIDER,
                "integration-wasm",
            ],
            "the bus must hold the four real identities in configuration order"
        );
        assert!(
            HookName::ALL.iter().all(|hook| {
                session
                    .bus
                    .plugins()
                    .iter()
                    .filter(|plugin| {
                        plugin.manifest().supports(*hook)
                            && *hook != HookName::Dispose
                            && *hook != HookName::TextComplete
                    })
                    .count()
                    < 4
            }),
            "a hook shared by all four tiers would make the position log redundant; assert the \
             shared mutation directly instead"
        );

        let output = session.complete("x").await;

        assert_eq!(
            session.take_observed(),
            observations(&order, HookName::TextComplete),
            "one dispatch must reach all four tiers in configuration order"
        );
        assert_eq!(
            output, "x!|x!",
            "the example Rust plugin's real suffix must land before the WebAssembly tier doubles \
             the text, because that is the configured order"
        );
        assert_eq!(
            session.auth_providers().await,
            [ANTIGRAVITY_PROVIDER, KIRO_PROVIDER],
            "both real auth hooks must compose in configuration order"
        );
        assert_eq!(
            session.kiro_request_kind("compaction").await.as_deref(),
            Some("compaction"),
            "the real kiro chat.headers hook must still serve from inside the four-plugin bus"
        );
        assert_eq!(
            session.kiro_request_kind("build").await,
            None,
            "a non-compaction turn must not receive the request-kind header, otherwise the \
             assertion above would pass for any input"
        );

        session.dispose().await;
        assert_eq!(
            lock(&session.disposed).clone(),
            order.map(RealTier::label),
            "teardown must reach every tier in configuration order"
        );
        session.shutdown().await;
        wait_for_processes_to_exit(&[session.rust_pid]).await;
    }

    #[tokio::test]
    async fn reversing_configuration_order_reverses_real_plugin_dispatch() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let order = [
            RealTier::Wasm,
            RealTier::Kiro,
            RealTier::Rust,
            RealTier::Antigravity,
        ];
        let Some(session) = real_tiers_or_skip(
            "reversing_configuration_order_reverses_real_plugin_dispatch",
            temp.path(),
            healthy_wasm_component(),
            &order,
            None,
        )
        .await
        else {
            return;
        };

        let output = session.complete("x").await;

        assert_eq!(
            session.take_observed(),
            observations(&order, HookName::TextComplete)
        );
        assert_eq!(
            output, "x|x!",
            "doubling now precedes the Rust suffix, so the order assertion cannot pass for any \
             configuration"
        );
        assert_eq!(
            session.auth_providers().await,
            [KIRO_PROVIDER, ANTIGRAVITY_PROVIDER],
            "the real auth vector must follow the reversed configuration too"
        );
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&[session.rust_pid]).await;
    }

    #[tokio::test]
    async fn killing_the_rust_tier_leaves_the_real_auth_plugins_serving() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let order = [
            RealTier::Rust,
            RealTier::Wasm,
            RealTier::Kiro,
            RealTier::Antigravity,
        ];
        let Some(session) = real_tiers_or_skip(
            "killing_the_rust_tier_leaves_the_real_auth_plugins_serving",
            temp.path(),
            healthy_wasm_component(),
            &order,
            None,
        )
        .await
        else {
            return;
        };
        kill_process(session.rust_pid);
        wait_until(Duration::from_secs(3), || {
            !session.rust.plugins()[0].is_enabled()
        })
        .await;
        session.take_observed();

        let output = session.complete("x").await;

        assert_eq!(
            session.take_observed(),
            observations(
                &[RealTier::Wasm, RealTier::Kiro, RealTier::Antigravity],
                HookName::TextComplete
            ),
            "only the killed tier may drop out of dispatch"
        );
        assert_eq!(
            output, "x|x",
            "the killed tier's suffix must be the only loss"
        );
        let diagnostics = session.rust.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::Crashed);
        assert_eq!(
            session.auth_providers().await,
            [KIRO_PROVIDER, ANTIGRAVITY_PROVIDER],
            "both real auth plugins must survive a sibling crash"
        );
        assert_eq!(
            session.kiro_request_kind("compaction").await.as_deref(),
            Some("compaction"),
            "the real kiro hook must still inject its header after the Rust tier died"
        );
        assert!(session.wasm.plugins()[0].is_enabled());
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&[session.rust_pid]).await;
    }

    #[tokio::test]
    async fn a_runaway_wasm_tier_leaves_the_real_auth_plugins_serving() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let order = [
            RealTier::Antigravity,
            RealTier::Wasm,
            RealTier::Kiro,
            RealTier::Rust,
        ];
        let Some(session) = real_tiers_or_skip(
            "a_runaway_wasm_tier_leaves_the_real_auth_plugins_serving",
            temp.path(),
            runaway_wasm_component(),
            &order,
            None,
        )
        .await
        else {
            return;
        };

        let first = session.complete("x").await;
        session.take_observed();
        let second = session.complete("x").await;

        assert_eq!(first, "x!", "the runaway tier must not mutate the text");
        assert_eq!(second, "x!");
        assert_eq!(
            session.take_observed(),
            observations(
                &[RealTier::Antigravity, RealTier::Kiro, RealTier::Rust],
                HookName::TextComplete
            ),
            "only the runaway tier may drop out of dispatch"
        );
        let diagnostics = session.wasm.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].kind, PluginDiagnosticKind::TimedOut);
        assert!(!session.wasm.plugins()[0].is_enabled());
        assert!(session.rust.plugins()[0].is_enabled());
        assert_eq!(
            session.auth_providers().await,
            [ANTIGRAVITY_PROVIDER, KIRO_PROVIDER]
        );
        assert_eq!(
            session.kiro_request_kind("compaction").await.as_deref(),
            Some("compaction")
        );
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&[session.rust_pid]).await;
    }

    #[tokio::test]
    async fn starving_one_real_auth_plugin_leaves_its_javascript_sibling_serving() {
        let temp = tempfile::tempdir().expect("integration tempdir");
        let order = [
            RealTier::Antigravity,
            RealTier::Kiro,
            RealTier::Rust,
            RealTier::Wasm,
        ];
        let Some(session) = real_tiers_or_skip(
            "starving_one_real_auth_plugin_leaves_its_javascript_sibling_serving",
            temp.path(),
            healthy_wasm_component(),
            &order,
            Some(RealTier::Kiro),
        )
        .await
        else {
            return;
        };
        let starved = session.js_load(RealTier::Kiro);
        assert!(starved.plugins().is_empty());
        assert_eq!(
            starved.diagnostics().len(),
            1,
            "{:?}",
            starved.diagnostics()
        );
        assert_eq!(
            starved.diagnostics()[0].kind,
            JsDiagnosticKind::MissingRuntime
        );
        assert_eq!(
            session.ids(),
            [
                supported_spec(ANTIGRAVITY_PACKAGE).as_str(),
                "integration-rust",
                "integration-wasm",
            ],
            "the starved plugin must be the only one missing from the bus"
        );

        let output = session.complete("x").await;

        assert_eq!(
            session.take_observed(),
            observations(
                &[RealTier::Antigravity, RealTier::Rust, RealTier::Wasm],
                HookName::TextComplete
            )
        );
        assert_eq!(output, "x!|x!");
        assert_eq!(
            session.auth_providers().await,
            [ANTIGRAVITY_PROVIDER],
            "the surviving real auth plugin must still register its provider"
        );
        assert_eq!(
            session.kiro_request_kind("compaction").await,
            None,
            "nothing may inject kiro's header once kiro is gone, otherwise the positive \
             assertions elsewhere prove nothing about kiro"
        );
        session.dispose().await;
        session.shutdown().await;
        wait_for_processes_to_exit(&[session.rust_pid]).await;
    }

    #[tokio::test]
    async fn three_synthetic_tiers_follow_configuration_order() {
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

    const PLUGIN_CACHE: &str = "/config/.cache/opencode";
    const ANTIGRAVITY_PACKAGE: &str = "opencode-antigravity-auth";
    const KIRO_PACKAGE: &str = "@sunerpy/opencode-kiro-auth";
    const ANTIGRAVITY_PROVIDER: &str = "google";
    const KIRO_PROVIDER: &str = "kiro-auth";
    const KIRO_REQUEST_KIND_HEADER: &str = "x-opencode-kiro-request-kind";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RealTier {
        Antigravity,
        Kiro,
        Rust,
        Wasm,
    }

    impl RealTier {
        const fn package(self) -> Option<&'static str> {
            match self {
                Self::Antigravity => Some(ANTIGRAVITY_PACKAGE),
                Self::Kiro => Some(KIRO_PACKAGE),
                Self::Rust | Self::Wasm => None,
            }
        }

        const fn label(self) -> &'static str {
            match self {
                Self::Antigravity => "antigravity",
                Self::Kiro => "kiro",
                Self::Rust => "rust",
                Self::Wasm => "wasm",
            }
        }
    }

    struct RealTiers {
        bus: HookBus,
        rust: PluginLoad,
        wasm: WasmPluginLoad,
        js: Vec<(RealTier, JsPluginLoad)>,
        rust_pid: u32,
        observed: Arc<Mutex<Vec<String>>>,
        disposed: Arc<Mutex<Vec<&'static str>>>,
    }

    impl RealTiers {
        async fn load(
            root: &Path,
            wasm_component: Vec<u8>,
            order: &[RealTier],
            starved: Option<RealTier>,
        ) -> Result<Self, String> {
            let absent = [ANTIGRAVITY_PACKAGE, KIRO_PACKAGE]
                .into_iter()
                .map(installed_package)
                .filter(|path| !path.is_dir())
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            if !absent.is_empty() {
                return Err(format!(
                    "the real auth plugin package(s) {} are absent, so real three-tier \
                     coexistence was NOT verified on this host",
                    absent.join(", ")
                ));
            }

            let rust_pid_file = root.join("rust.pid");
            let rust = load_plugins_ordered(vec![rust_spec(&rust_pid_file, Duration::ZERO)]).await;
            assert_eq!(rust.plugins().len(), 1, "{:?}", rust.diagnostics());
            let rust_pid = read_pid(&rust_pid_file);

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

            let mut js = Vec::new();
            for tier in [RealTier::Antigravity, RealTier::Kiro] {
                let package = tier.package().expect("a JavaScript tier names its package");
                let starve = starved == Some(tier);
                js.push((tier, load_real_js(root, package, starve).await));
            }

            for (tier, load) in &js {
                if starved == Some(*tier) {
                    continue;
                }
                if load.plugins().is_empty() {
                    let reason = format!(
                        "{} did not load ({:?}), so real three-tier coexistence was NOT verified \
                         on this host",
                        tier.package().unwrap_or(tier.label()),
                        load.diagnostics()
                    );
                    rust.shutdown().await;
                    for (_, load) in &js {
                        load.shutdown().await;
                    }
                    return Err(reason);
                }
            }

            let disposed = Arc::new(Mutex::new(Vec::new()));
            let observed = Arc::new(Mutex::new(Vec::new()));

            let rust_plugin = Arc::clone(&rust.plugins()[0]);
            let rust_health = Arc::clone(&rust_plugin);
            let rust_tracked: Arc<dyn Plugin> = TrackedPlugin::new(
                rust_plugin,
                RealTier::Rust.label(),
                Arc::clone(&disposed),
                Arc::clone(&observed),
                Arc::new(move || rust_health.is_enabled()),
                None,
            );

            let wasm_plugin = Arc::clone(&wasm.plugins()[0]);
            let wasm_health = Arc::clone(&wasm_plugin);
            let wasm_tracked: Arc<dyn Plugin> = TrackedPlugin::new(
                wasm_plugin,
                RealTier::Wasm.label(),
                Arc::clone(&disposed),
                Arc::clone(&observed),
                Arc::new(move || wasm_health.is_enabled()),
                Some(duplicate_text),
            );

            let mut tracked_js = Vec::new();
            for (tier, load) in &js {
                if let Some(plugin) = load.plugins().first() {
                    let plugin = Arc::clone(plugin);
                    let health = Arc::clone(&plugin);
                    tracked_js.push((
                        *tier,
                        TrackedPlugin::new(
                            plugin,
                            tier.label(),
                            Arc::clone(&disposed),
                            Arc::clone(&observed),
                            // A version-compatibility warning is not a crash:
                            // antigravity declares `@opencode-ai/plugin ^0.15.30`
                            // against a host reporting 1.18.13 and still serves,
                            // so only fault kinds may mark it unhealthy.
                            Arc::new(move || {
                                !health.diagnostics().iter().any(|diagnostic| {
                                    matches!(
                                        diagnostic.kind,
                                        JsDiagnosticKind::Crashed
                                            | JsDiagnosticKind::TimedOut
                                            | JsDiagnosticKind::Protocol
                                            | JsDiagnosticKind::FailedToLoad
                                    )
                                })
                            }),
                            None,
                        ) as Arc<dyn Plugin>,
                    ));
                }
            }

            let plugins = order
                .iter()
                .filter_map(|tier| match tier {
                    RealTier::Rust => Some(Arc::clone(&rust_tracked)),
                    RealTier::Wasm => Some(Arc::clone(&wasm_tracked)),
                    RealTier::Antigravity | RealTier::Kiro => tracked_js
                        .iter()
                        .find(|(loaded, _)| loaded == tier)
                        .map(|(_, plugin)| Arc::clone(plugin)),
                })
                .collect();

            Ok(Self {
                bus: HookBus::new(plugins),
                rust,
                wasm,
                js,
                rust_pid,
                observed,
                disposed,
            })
        }

        fn ids(&self) -> Vec<&str> {
            self.bus
                .plugins()
                .iter()
                .map(|plugin| plugin.manifest().id())
                .collect()
        }

        fn take_observed(&self) -> Vec<String> {
            std::mem::take(&mut *lock(&self.observed))
        }

        fn js_load(&self, tier: RealTier) -> &JsPluginLoad {
            self.js
                .iter()
                .find(|(loaded, _)| *loaded == tier)
                .map(|(_, load)| load)
                .unwrap_or_else(|| panic!("{tier:?} has a JavaScript load"))
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

        async fn auth_providers(&self) -> Vec<String> {
            let mut output = Vec::new();
            self.bus
                .dispatch(HookInvocation::Auth {
                    output: &mut output,
                })
                .await
                .expect("collecting auth hooks");
            output.into_iter().map(|auth| auth.provider).collect()
        }

        async fn kiro_request_kind(&self, agent: &str) -> Option<String> {
            let model = kiro_model();
            let provider = ProviderContext {
                source: ProviderSource::Config,
                info: kiro_provider(),
                options: serde_json::Map::new(),
            };
            let context = ChatContext {
                session_id: "ses_criterion_seven",
                agent,
                model: &model,
                provider: &provider,
                message: Message::new(Role::User, "summarize"),
            };
            let mut output = ChatHeadersOutput::default();
            self.bus
                .dispatch(HookInvocation::ChatHeaders {
                    input: &context,
                    output: &mut output,
                })
                .await
                .expect("the real kiro chat.headers hook runs through the bus");
            output.headers.get(KIRO_REQUEST_KIND_HEADER).cloned()
        }

        async fn dispose(&self) {
            self.bus
                .dispatch(HookInvocation::Dispose)
                .await
                .expect("dispose surviving tiers");
        }

        async fn shutdown(&self) {
            self.rust.shutdown().await;
            for (_, load) in &self.js {
                load.shutdown().await;
            }
        }
    }

    /// Loads the four real tiers, or prints a named skip and yields `None`.
    ///
    /// A silent `return` here would let a green suite imply coverage of the two
    /// real auth plugins that it never had — the exact failure this test exists
    /// to close — so an absent package is announced with the test's own name.
    async fn real_tiers_or_skip(
        test: &str,
        root: &Path,
        wasm_component: Vec<u8>,
        order: &[RealTier],
        starved: Option<RealTier>,
    ) -> Option<RealTiers> {
        match RealTiers::load(root, wasm_component, order, starved).await {
            Ok(session) => Some(session),
            Err(reason) => {
                eprintln!("SKIPPED {test}: {reason}");
                None
            }
        }
    }

    fn observations(order: &[RealTier], hook: HookName) -> Vec<String> {
        order
            .iter()
            .map(|tier| format!("{}:{hook}", tier.label()))
            .collect()
    }

    fn supported_spec(package: &str) -> String {
        let entry = SUPPORTED_JS_PLUGINS
            .iter()
            .find(|supported| supported.package == package)
            .unwrap_or_else(|| {
                panic!("zuno_plugin::SUPPORTED_JS_PLUGINS no longer lists {package}")
            });
        format!("{}@{}", entry.package, entry.version)
    }

    fn installed_package(package: &str) -> PathBuf {
        Path::new(PLUGIN_CACHE).join(format!(
            "packages/{}/node_modules/{package}",
            supported_spec(package)
        ))
    }

    async fn load_real_js(root: &Path, package: &str, starve_runtime: bool) -> JsPluginLoad {
        let mut config = host_config(root).cache_dir(PathBuf::from(PLUGIN_CACHE));
        if starve_runtime {
            config = config.runtime_search_path(OsString::new());
        }
        load_js_plugins_ordered(vec![JsPluginSpec::new(supported_spec(package))], config).await
    }

    fn kiro_model() -> ResolvedModel {
        ResolvedModel {
            id: "claude-sonnet-4-5".to_owned(),
            provider_id: KIRO_PROVIDER.to_owned(),
            name: "Claude Sonnet 4.5".to_owned(),
            family: "claude".to_owned(),
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

    fn kiro_provider() -> ResolvedProvider {
        ResolvedProvider {
            id: KIRO_PROVIDER.to_owned(),
            name: "Kiro".to_owned(),
            env: Vec::new(),
            options: serde_json::Map::new(),
            availability: Availability::none(),
            models: BTreeMap::new(),
        }
    }

    fn rust_spec(pid_file: &Path, hook_sleep: Duration) -> PluginProcessSpec {
        PluginProcessSpec::new("integration-rust", "/bin/sh")
            .arg("-c")
            .arg("printf '%s' \"$$\" > \"$1\"; exec \"$2\"")
            .arg("zuno-integration-rust-wrapper")
            .arg(pid_file.as_os_str())
            .arg(env!("CARGO_BIN_EXE_zuno-example-plugin"))
            .env("ZUNO_EXAMPLE_PLUGIN_ID", "integration-rust")
            .env(
                "ZUNO_EXAMPLE_SLEEP_HOOK_MS",
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
wasm_integration_skip!(three_synthetic_tiers_follow_configuration_order);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(reversing_configuration_order_reverses_real_plugin_dispatch);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(killing_the_rust_tier_leaves_the_real_auth_plugins_serving);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(a_runaway_wasm_tier_leaves_the_real_auth_plugins_serving);
#[cfg(not(all(feature = "wasm", unix)))]
wasm_integration_skip!(starving_one_real_auth_plugin_leaves_its_javascript_sibling_serving);
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
