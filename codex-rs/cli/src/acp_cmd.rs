use anyhow::Context;
use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_client::EnvironmentManager;
use codex_app_server_client::ExecServerRuntimePaths;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_arg0::Arg0DispatchPaths;
use codex_cloud_config::cloud_config_bundle_loader_for_storage;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLoadOptions;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::bootstrap_auth_config;
use codex_core::config::find_codex_home;
use codex_core::config::load_config_toml_with_layer_stack;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_tui::Cli as TuiCli;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;
use std::sync::Arc;

/// Start the native ACP projection over the same in-process App Server runtime
/// used by other Codex frontends.
///
/// ACP is only a protocol edge here. Configuration, durable thread state,
/// approvals, cancellation, and execution remain owned by App Server/core.
pub(crate) async fn run(
    root_config_overrides: CliConfigOverrides,
    interactive: TuiCli,
    arg0_paths: Arg0DispatchPaths,
    strict_config: bool,
) -> anyhow::Result<()> {
    if interactive.prompt.is_some() || !interactive.images.is_empty() {
        anyhow::bail!("`zuno acp` does not accept an initial prompt or images");
    }

    let loader_overrides = loader_overrides_for_profile(interactive.config_profile_v2.as_ref())?;
    let shared = interactive.shared.into_inner();
    if shared.oss || shared.oss_provider.is_some() {
        anyhow::bail!(
            "`zuno acp` does not accept --oss or --local-provider; configure the provider in the selected profile instead"
        );
    }

    let mut cli_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    if interactive.web_search {
        cli_overrides.push((
            "web_search".to_string(),
            toml::Value::String("live".to_string()),
        ));
    }

    let approval_policy = if shared.dangerously_bypass_approvals_and_sandbox {
        Some(AskForApproval::Never)
    } else {
        interactive.approval_policy.map(Into::into)
    };
    let sandbox_mode = if shared.dangerously_bypass_approvals_and_sandbox {
        Some(codex_protocol::config_types::SandboxMode::DangerFullAccess)
    } else {
        shared.sandbox_mode.map(Into::into)
    };

    let cwd = match shared.cwd.as_deref() {
        Some(cwd) => AbsolutePathBuf::relative_to_current_dir(cwd),
        None => AbsolutePathBuf::current_dir(),
    }
    .context("failed to resolve ACP working directory")?;
    let codex_home = find_codex_home().context("failed to resolve Zuno home")?;
    let bootstrap_config = load_config_toml_with_layer_stack(
        codex_home.as_path(),
        Some(&cwd),
        cli_overrides.clone(),
        ConfigLoadOptions {
            loader_overrides: loader_overrides.clone(),
            strict_config,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
        },
    )
    .await
    .context("failed to load bootstrap configuration for ACP")?;
    let cloud_config_bundle = cloud_config_bundle_loader_for_storage(
        bootstrap_auth_config(codex_home.as_path(), &bootstrap_config)
            .context("failed to resolve ACP cloud configuration authentication")?,
        /*enable_codex_api_key_env*/ false,
    )
    .await
    .context("failed to initialize ACP cloud configuration authentication")?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .cli_overrides(cli_overrides.clone())
        .loader_overrides(loader_overrides.clone())
        .strict_config(strict_config)
        .cloud_config_bundle(cloud_config_bundle.clone())
        .fallback_cwd(Some(cwd.to_path_buf()))
        .harness_overrides(ConfigOverrides {
            model: shared.model,
            approval_policy,
            sandbox_mode,
            cwd: Some(cwd.to_path_buf()),
            codex_self_exe: arg0_paths.codex_self_exe.clone(),
            codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe.clone(),
            main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe.clone(),
            bypass_hook_trust: shared.bypass_hook_trust.then_some(true),
            additional_writable_roots: shared.add_dir,
            ..Default::default()
        })
        .build()
        .await
        .context("failed to load ACP configuration")?;

    let config_warnings = config
        .startup_warnings
        .iter()
        .map(|warning| ConfigWarningNotification {
            summary: warning.clone(),
            details: None,
            path: None,
            range: None,
        })
        .collect();
    let state_db = codex_core::init_state_db(&config).await;
    let local_runtime_paths = ExecServerRuntimePaths::from_optional_paths(
        arg0_paths.codex_self_exe.clone(),
        arg0_paths.codex_linux_sandbox_exe.clone(),
    )?;
    #[cfg(target_os = "macos")]
    let local_runtime_paths = local_runtime_paths.with_allowed_symlinked_codex_home(
        codex_config::allowed_symlinked_codex_home(&config.config_layer_stack, &config.codex_home),
    );
    let environment_manager = EnvironmentManager::from_codex_home(
        &config.codex_home,
        Some(local_runtime_paths),
        config.http_client_factory(),
    )
    .await
    .context("failed to initialize ACP execution environments")?;

    zuno_acp::serve_in_process_stdio(InProcessClientStartArgs {
        arg0_paths,
        config: Arc::new(config),
        cli_overrides,
        loader_overrides,
        strict_config,
        cloud_config_bundle,
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db,
        environment_manager: Arc::new(environment_manager),
        config_warnings,
        session_source: SessionSource::Custom("acp".to_string()),
        enable_codex_api_key_env: false,
        client_name: "zuno-acp".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    })
    .await
    .context("native ACP server failed")
}

fn loader_overrides_for_profile(
    profile: Option<&codex_utils_cli::ProfileV2Name>,
) -> anyhow::Result<LoaderOverrides> {
    let codex_home = find_codex_home()?;
    Ok(match profile {
        Some(profile) => LoaderOverrides {
            user_config_path: Some(codex_core::config::resolve_profile_v2_config_path(
                &codex_home,
                profile,
            )),
            user_config_profile: Some(profile.clone()),
            ..Default::default()
        },
        None => LoaderOverrides::default(),
    })
}
