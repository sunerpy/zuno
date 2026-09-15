use codex_agent_extension::AcpBackend;
use codex_agent_extension::AcpBackendConfig;
use codex_agent_extension::AgentBackendCapabilities;
use codex_agent_extension::AgentBackendFactory;
use codex_agent_extension::AgentBackendFactoryError;
use codex_agent_extension::AgentBackendFactoryMount;
use codex_agent_extension::AgentBackendFactoryRegistry;
use codex_agent_extension::AgentBackendId;
use codex_agent_extension::AgentBackendOptionScope;
use codex_agent_extension::AgentBackendRegistryError;
use codex_agent_extension::AgentBackendRequirements;
use codex_agent_extension::AgentRunner;
use codex_agent_extension::ClaudeCodeBackend;
use codex_agent_extension::ClaudeCodeBackendConfig;
use codex_agent_extension::ClaudeCodePermissionMode;
use codex_agent_extension::NativeCodexBackend;
use codex_agent_extension::OneShotAgentBackend;
use codex_agent_extension::OneShotAgentBackendKind;
use codex_agent_extension::OneShotAgentError;
use codex_agent_extension::OneShotAgentFailureCategory;
use codex_agent_extension::OneShotAgentFailureStage;
use codex_agent_extension::OneShotProcessSandboxConfig;
use codex_agent_extension::ResolvedAgentBackendFactory;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::windows_sandbox::WindowsSandboxLevelExt;
use codex_plugin::EffectivePluginAgentBackend;
use codex_plugin::PluginAgentBackendCommand;
use codex_plugin::PluginAgentBackendDeclaration;
use codex_plugin::PluginAgentBackendKind;
use codex_protocol::ThreadId;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxKind;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::time::Duration;

/// Host-owned state used to construct one Profile-bound backend instance.
pub(super) struct WorkflowAgentBackendContext {
    pub(super) thread_manager: Arc<ThreadManager>,
    pub(super) parent_thread_id: ThreadId,
    pub(super) config: Config,
}

struct NativeCodexBackendFactory;

impl AgentBackendFactory<WorkflowAgentBackendContext> for NativeCodexBackendFactory {
    fn kind(&self) -> OneShotAgentBackendKind {
        OneShotAgentBackendKind::NativeCodex
    }

    fn capabilities(&self) -> AgentBackendCapabilities {
        AgentBackendCapabilities::native_codex()
    }

    fn revision(&self) -> String {
        "zuno/native-codex/v1".to_string()
    }

    fn build(
        &self,
        context: &WorkflowAgentBackendContext,
    ) -> Result<Arc<dyn OneShotAgentBackend>, OneShotAgentError> {
        Ok(native_codex_backend(context))
    }
}

#[derive(Clone, Debug)]
struct PinnedExecutable {
    path: PathBuf,
    digest: String,
}

struct ClaudeCodeBackendFactory {
    executable: Option<PinnedExecutable>,
    revision: String,
}

impl ClaudeCodeBackendFactory {
    fn from_current_path() -> Self {
        let executable = pin_named_executable("claude");
        let revision = executable.as_ref().map_or_else(
            || "zuno/claude-code/v2:unavailable".to_string(),
            |executable| pinned_factory_revision("zuno/claude-code/v2", executable),
        );
        Self {
            executable,
            revision,
        }
    }
}

impl AgentBackendFactory<WorkflowAgentBackendContext> for ClaudeCodeBackendFactory {
    fn kind(&self) -> OneShotAgentBackendKind {
        OneShotAgentBackendKind::ClaudeCode
    }

    fn capabilities(&self) -> AgentBackendCapabilities {
        AgentBackendCapabilities::claude_code()
    }

    fn revision(&self) -> String {
        self.revision.clone()
    }

    fn build(
        &self,
        context: &WorkflowAgentBackendContext,
    ) -> Result<Arc<dyn OneShotAgentBackend>, OneShotAgentError> {
        claude_code_backend(
            context,
            /*declaration*/ None,
            &BTreeMap::new(),
            self.executable.as_ref(),
        )
    }
}

/// One declaration-owned factory. The declaration selects only the product
/// adapter and process boundary; the user-owned execution profile still owns
/// model, reasoning, permission, provider, and sandbox configuration.
struct PluginAgentBackendFactory {
    declaration: PluginAgentBackendDeclaration,
    revision: String,
    environment: BTreeMap<String, String>,
    executable: Option<PinnedExecutable>,
}

impl AgentBackendFactory<WorkflowAgentBackendContext> for PluginAgentBackendFactory {
    fn kind(&self) -> OneShotAgentBackendKind {
        backend_kind(self.declaration.kind)
    }

    fn capabilities(&self) -> AgentBackendCapabilities {
        match self.declaration.kind {
            PluginAgentBackendKind::NativeCodex => AgentBackendCapabilities::native_codex(),
            PluginAgentBackendKind::ClaudeCode => AgentBackendCapabilities::claude_code(),
            PluginAgentBackendKind::Acp => AgentBackendCapabilities::acp(),
        }
    }

    fn revision(&self) -> String {
        self.revision.clone()
    }

    fn build(
        &self,
        context: &WorkflowAgentBackendContext,
    ) -> Result<Arc<dyn OneShotAgentBackend>, OneShotAgentError> {
        match self.declaration.kind {
            PluginAgentBackendKind::NativeCodex => Ok(native_codex_backend(context)),
            PluginAgentBackendKind::ClaudeCode => claude_code_backend(
                context,
                Some(&self.declaration),
                &self.environment,
                self.executable.as_ref(),
            ),
            PluginAgentBackendKind::Acp => acp_backend(
                context,
                &self.declaration,
                &self.environment,
                self.executable.as_ref(),
            ),
        }
    }
}

fn backend_kind(kind: PluginAgentBackendKind) -> OneShotAgentBackendKind {
    match kind {
        PluginAgentBackendKind::NativeCodex => OneShotAgentBackendKind::NativeCodex,
        PluginAgentBackendKind::ClaudeCode => OneShotAgentBackendKind::ClaudeCode,
        PluginAgentBackendKind::Acp => OneShotAgentBackendKind::Acp,
    }
}

fn native_codex_backend(context: &WorkflowAgentBackendContext) -> Arc<dyn OneShotAgentBackend> {
    Arc::new(NativeCodexBackend::new(
        AgentRunner::new(Arc::downgrade(&context.thread_manager)),
        context.parent_thread_id,
        context.config.clone(),
    ))
}

fn claude_code_backend(
    context: &WorkflowAgentBackendContext,
    declaration: Option<&PluginAgentBackendDeclaration>,
    environment: &BTreeMap<String, String>,
    pinned_executable: Option<&PinnedExecutable>,
) -> Result<Arc<dyn OneShotAgentBackend>, OneShotAgentError> {
    let mut backend = ClaudeCodeBackendConfig {
        model: context.config.model.clone(),
        reasoning_effort: context
            .config
            .model_reasoning_effort
            .as_ref()
            .map(ToString::to_string),
        permission_mode: claude_permission_mode(&context.config),
        sandbox: Some(external_process_sandbox(&context.config)),
        ..ClaudeCodeBackendConfig::default()
    };
    if let Some(declaration) = declaration {
        if let Some(command) = declaration.command_for_current_platform() {
            backend.executable = PathBuf::from(command.as_os_str());
        }
        backend.executable_sha256 = declaration.executable_digest.clone();
        backend.env.clone_from(environment);
        backend.run_timeout = declaration.run_timeout_ms.map(Duration::from_millis);
        backend.dispose_grace = Duration::from_millis(declaration.dispose_grace_ms);
        backend.output_limit_bytes = declaration.max_message_bytes;
    }
    let pinned_executable = pinned_executable
        .ok_or_else(|| invalid_plugin_backend(OneShotAgentBackendKind::ClaudeCode))?;
    backend.executable.clone_from(&pinned_executable.path);
    backend.executable_sha256 = Some(pinned_executable.digest.clone());
    ClaudeCodeBackend::new(backend).map(|backend| Arc::new(backend) as Arc<dyn OneShotAgentBackend>)
}

fn acp_backend(
    context: &WorkflowAgentBackendContext,
    declaration: &PluginAgentBackendDeclaration,
    environment: &BTreeMap<String, String>,
    pinned_executable: Option<&PinnedExecutable>,
) -> Result<Arc<dyn OneShotAgentBackend>, OneShotAgentError> {
    declaration
        .command_for_current_platform()
        .ok_or_else(|| invalid_plugin_backend(OneShotAgentBackendKind::Acp))?;
    let pinned_executable =
        pinned_executable.ok_or_else(|| invalid_plugin_backend(OneShotAgentBackendKind::Acp))?;
    let permissions = Some(permission_profile_id(&context.config));
    let backend = AcpBackendConfig {
        executable: pinned_executable.path.clone(),
        executable_sha256: Some(pinned_executable.digest.clone()),
        args: declaration.args.iter().map(OsString::from).collect(),
        model: context.config.model.clone(),
        model_provider: Some(context.config.model_provider_id.clone()),
        reasoning_effort: context
            .config
            .model_reasoning_effort
            .as_ref()
            .map(ToString::to_string),
        permissions,
        env: environment.clone(),
        sandbox: Some(external_process_sandbox(&context.config)),
        startup_timeout: Duration::from_millis(declaration.startup_timeout_ms),
        run_timeout: declaration.run_timeout_ms.map(Duration::from_millis),
        dispose_grace: Duration::from_millis(declaration.dispose_grace_ms),
        max_message_bytes: declaration.max_message_bytes,
        ..AcpBackendConfig::default()
    };
    AcpBackend::new(backend).map(|backend| Arc::new(backend) as Arc<dyn OneShotAgentBackend>)
}

fn external_process_sandbox(config: &Config) -> OneShotProcessSandboxConfig {
    OneShotProcessSandboxConfig {
        permission_profile: config.permissions.effective_permission_profile(),
        workspace_roots: config.workspace_roots.clone(),
        codex_home: config.codex_home.clone(),
        codex_self_exe: config.codex_self_exe.clone(),
        codex_linux_sandbox_exe: config.codex_linux_sandbox_exe.clone(),
        managed_network_configured: config.permissions.network.is_some(),
        use_legacy_landlock: config.features.use_legacy_landlock(),
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::from_config(
            config,
        ),
        windows_sandbox_private_desktop: config.permissions.windows_sandbox_private_desktop,
    }
}

fn forwarded_environment(names: &[String]) -> Result<BTreeMap<String, String>, ()> {
    names
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
        .map(|(name, value)| {
            value
                .into_string()
                .map(|value| (name.clone(), value))
                .map_err(|_| ())
        })
        .collect()
}

fn permission_profile_id(config: &Config) -> String {
    if config.explicit_permission_profile_mode
        && let Some(profile) = config.permissions.active_permission_profile()
    {
        return profile.id;
    }

    let effective = config.permissions.effective_permission_profile();
    match &effective {
        PermissionProfile::Disabled => BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS.to_string(),
        PermissionProfile::Managed { .. } if permission_profile_is_read_only(&effective) => {
            BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string()
        }
        PermissionProfile::Managed { .. } => BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string(),
        PermissionProfile::External { .. } => BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string(),
    }
}

fn claude_permission_mode(config: &Config) -> ClaudeCodePermissionMode {
    let active = config.permissions.active_permission_profile();
    let effective = config.permissions.effective_permission_profile();
    let managed_read_only = permission_profile_is_read_only(&effective);
    if active
        .as_ref()
        .is_some_and(|profile| profile.id == BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS)
        && matches!(effective, PermissionProfile::Disabled)
    {
        ClaudeCodePermissionMode::BypassPermissions
    } else if active.as_ref().is_some_and(|profile| {
        profile.id == BUILT_IN_PERMISSION_PROFILE_READ_ONLY
            || profile.extends.as_deref() == Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY)
    }) || managed_read_only
    {
        ClaudeCodePermissionMode::Plan
    } else {
        // Non-interactive and fail-closed: Claude declines permission prompts
        // rather than widening a managed or custom Codex profile.
        ClaudeCodePermissionMode::DontAsk
    }
}

fn permission_profile_is_read_only(profile: &PermissionProfile) -> bool {
    let file_system = profile.file_system_sandbox_policy();
    matches!(file_system.kind, FileSystemSandboxKind::Restricted)
        && !file_system
            .entries
            .iter()
            .any(|entry| entry.access.can_write())
}

fn invalid_plugin_backend(kind: OneShotAgentBackendKind) -> OneShotAgentError {
    OneShotAgentError {
        backend: kind,
        stage: OneShotAgentFailureStage::Validate,
        category: OneShotAgentFailureCategory::InvalidRequest,
        exit_code: None,
    }
}

struct WorkflowAgentBackendFactorySet {
    registry: AgentBackendFactoryRegistry<WorkflowAgentBackendContext>,
    /// Exact mount generations owned by this immutable factory snapshot.
    _mounts: Vec<AgentBackendFactoryMount<WorkflowAgentBackendContext>>,
    plugin_backends: Vec<EffectivePluginAgentBackend>,
    environment_digests: BTreeMap<String, String>,
}

pub(super) struct ResolvedWorkflowAgentBackendFactory {
    factory: ResolvedAgentBackendFactory<WorkflowAgentBackendContext>,
    plugin: Option<EffectivePluginAgentBackend>,
    environment_digest: Option<String>,
}

impl ResolvedWorkflowAgentBackendFactory {
    pub(super) fn descriptor(&self) -> &codex_agent_extension::AgentBackendDescriptor {
        self.factory.descriptor()
    }

    pub(super) fn plugin(&self) -> Option<&EffectivePluginAgentBackend> {
        self.plugin.as_ref()
    }

    pub(super) fn environment_digest(&self) -> Option<&str> {
        self.environment_digest.as_deref()
    }

    pub(super) fn build(
        &self,
        context: &WorkflowAgentBackendContext,
    ) -> Result<Arc<dyn OneShotAgentBackend>, AgentBackendFactoryError> {
        self.factory.build(context)
    }
}

impl WorkflowAgentBackendFactorySet {
    fn new(
        mut plugin_backends: Vec<EffectivePluginAgentBackend>,
    ) -> Result<Self, AgentBackendRegistryError> {
        plugin_backends.sort_unstable_by(|left, right| {
            left.id.cmp(&right.id).then_with(|| {
                left.plugin_identity
                    .plugin_id
                    .cmp(&right.plugin_identity.plugin_id)
            })
        });
        let mut seen = BTreeSet::new();
        for backend in &plugin_backends {
            let id = AgentBackendId::new(backend.id.clone())?;
            if !seen.insert(id.clone()) {
                return Err(AgentBackendRegistryError::Duplicate { id });
            }
        }

        let registry = AgentBackendFactoryRegistry::new();
        let mut mounts = Vec::with_capacity(/*built-ins*/ 3 + plugin_backends.len());
        let mut environment_digests = BTreeMap::new();
        let native: Arc<dyn AgentBackendFactory<WorkflowAgentBackendContext>> =
            Arc::new(NativeCodexBackendFactory);
        mount_factory(&registry, &mut mounts, "native-codex", Arc::clone(&native))?;
        // Compatibility alias only; both IDs resolve through the same factory
        // contract and neither defines a workflow or model route.
        mount_factory(&registry, &mut mounts, "codex", native)?;
        mount_factory(
            &registry,
            &mut mounts,
            "claude-code",
            Arc::new(ClaudeCodeBackendFactory::from_current_path()),
        )?;
        for backend in &mut plugin_backends {
            let id = AgentBackendId::new(backend.id.clone())?;
            let executable = pin_plugin_backend_executable(backend)
                .map_err(|()| AgentBackendRegistryError::InvalidRevision { id: id.clone() })?;
            validate_plugin_backend_generation(backend)
                .map_err(|()| AgentBackendRegistryError::InvalidRevision { id: id.clone() })?;
            let environment = forwarded_environment(&backend.declaration.env_vars)
                .map_err(|()| AgentBackendRegistryError::InvalidRevision { id: id.clone() })?;
            environment_digests.insert(backend.id.clone(), environment_digest(&environment));
            mount_factory(
                &registry,
                &mut mounts,
                &backend.id,
                Arc::new(PluginAgentBackendFactory {
                    declaration: backend.declaration.clone(),
                    revision: plugin_backend_revision(backend, &environment, executable.as_ref()),
                    environment,
                    executable,
                }),
            )?;
        }

        Ok(Self {
            registry,
            _mounts: mounts,
            plugin_backends,
            environment_digests,
        })
    }
}

fn pin_plugin_backend_executable(
    backend: &mut EffectivePluginAgentBackend,
) -> Result<Option<PinnedExecutable>, ()> {
    if backend.declaration.kind == PluginAgentBackendKind::NativeCodex {
        return Ok(None);
    }
    let command = backend
        .declaration
        .command_for_current_platform()
        .ok_or(())?;
    let executable = match command {
        PluginAgentBackendCommand::Name(name) => pin_named_executable(name).ok_or(())?,
        PluginAgentBackendCommand::PluginPath(path) => {
            pin_executable_path(path.as_path()).ok_or(())?
        }
    };
    backend.declaration.executable_digest = Some(executable.digest.clone());
    Ok(Some(executable))
}

fn pin_named_executable(name: &str) -> Option<PinnedExecutable> {
    let path = which::which(name).ok()?;
    pin_executable_path(&path)
}

fn pin_executable_path(path: &Path) -> Option<PinnedExecutable> {
    let path = std::fs::canonicalize(path).ok()?;
    let digest = current_file_digest(&path, /*executable*/ true)?;
    Some(PinnedExecutable { path, digest })
}

fn pinned_factory_revision(prefix: &str, executable: &PinnedExecutable) -> String {
    let mut input = Vec::new();
    push_revision_text(&mut input, prefix);
    push_revision_os_str(&mut input, executable.path.as_os_str());
    push_revision_text(&mut input, &executable.digest);
    let digest = Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}:{digest}")
}

fn environment_digest(environment: &BTreeMap<String, String>) -> String {
    let mut input = Vec::new();
    for (name, value) in environment {
        push_revision_text(&mut input, name);
        push_revision_text(&mut input, value);
    }
    Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_plugin_backend_generation(backend: &EffectivePluginAgentBackend) -> Result<(), ()> {
    if current_file_digest(
        backend.declaration.source_path.as_path(),
        /*executable*/ false,
    )
    .as_deref()
        != Some(backend.declaration.source_digest.as_str())
    {
        return Err(());
    }
    if let Some(PluginAgentBackendCommand::PluginPath(command)) =
        backend.declaration.command_for_current_platform()
    {
        let expected = backend.declaration.executable_digest.as_deref().ok_or(())?;
        if current_file_digest(command.as_path(), /*executable*/ true).as_deref() != Some(expected)
        {
            return Err(());
        }
    }
    Ok(())
}

fn current_file_digest(path: &Path, _executable: bool) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if _executable && metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            return Some(
                digest
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            );
        }
        digest.update(&buffer[..read]);
    }
}

fn plugin_backend_revision(
    backend: &EffectivePluginAgentBackend,
    environment: &BTreeMap<String, String>,
    executable: Option<&PinnedExecutable>,
) -> String {
    let mut input = Vec::new();
    push_revision_text(&mut input, "zuno/plugin-agent-backend/v1");
    push_revision_text(&mut input, &backend.id);
    push_revision_text(&mut input, &backend.plugin_identity.plugin_id);
    push_revision_optional_text(
        &mut input,
        backend.plugin_identity.remote_plugin_id.as_deref(),
    );
    push_revision_text(
        &mut input,
        match backend.declaration.kind {
            PluginAgentBackendKind::NativeCodex => "native-codex",
            PluginAgentBackendKind::ClaudeCode => "claude-code",
            PluginAgentBackendKind::Acp => "acp",
        },
    );
    push_revision_text(&mut input, &backend.declaration.source_generation);
    push_revision_optional_text(&mut input, backend.declaration.plugin_version.as_deref());
    push_revision_os_str(
        &mut input,
        backend.declaration.source_path.as_path().as_os_str(),
    );
    push_revision_text(&mut input, &backend.declaration.source_digest);
    push_revision_optional_text(&mut input, backend.declaration.executable_digest.as_deref());
    push_revision_command(&mut input, backend.declaration.command.as_ref());
    push_revision_command(&mut input, backend.declaration.command_windows.as_ref());
    for argument in &backend.declaration.args {
        push_revision_text(&mut input, argument);
    }
    input.extend_from_slice(&backend.declaration.startup_timeout_ms.to_be_bytes());
    input.extend_from_slice(
        &backend
            .declaration
            .run_timeout_ms
            .unwrap_or_default()
            .to_be_bytes(),
    );
    input.extend_from_slice(&backend.declaration.dispose_grace_ms.to_be_bytes());
    input.extend_from_slice(&(backend.declaration.max_message_bytes as u64).to_be_bytes());
    for (name, value) in environment {
        push_revision_text(&mut input, name);
        push_revision_text(&mut input, value);
    }
    if let Some(executable) = executable {
        input.push(1);
        push_revision_os_str(&mut input, executable.path.as_os_str());
        push_revision_text(&mut input, &executable.digest);
    } else {
        input.push(0);
    }
    let digest = Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("zuno/plugin-agent-backend/v1:{digest}")
}

fn push_revision_optional_text(target: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            target.push(1);
            push_revision_text(target, value);
        }
        None => target.push(0),
    }
}

fn push_revision_text(target: &mut Vec<u8>, value: &str) {
    push_revision_bytes(target, value.as_bytes());
}

fn push_revision_command(target: &mut Vec<u8>, command: Option<&PluginAgentBackendCommand>) {
    match command {
        Some(PluginAgentBackendCommand::Name(name)) => {
            target.push(1);
            push_revision_text(target, name);
        }
        Some(PluginAgentBackendCommand::PluginPath(path)) => {
            target.push(2);
            push_revision_os_str(target, path.as_path().as_os_str());
        }
        None => target.push(0),
    }
}

fn push_revision_bytes(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}

#[cfg(unix)]
fn push_revision_os_str(target: &mut Vec<u8>, value: &std::ffi::OsStr) {
    use std::os::unix::ffi::OsStrExt;
    push_revision_bytes(target, value.as_bytes());
}

#[cfg(windows)]
fn push_revision_os_str(target: &mut Vec<u8>, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    let encoded = value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    push_revision_bytes(target, &encoded);
}

#[cfg(not(any(unix, windows)))]
fn push_revision_os_str(target: &mut Vec<u8>, value: &std::ffi::OsStr) {
    push_revision_bytes(target, value.to_string_lossy().as_bytes());
}

fn mount_factory(
    registry: &AgentBackendFactoryRegistry<WorkflowAgentBackendContext>,
    mounts: &mut Vec<AgentBackendFactoryMount<WorkflowAgentBackendContext>>,
    id: &str,
    factory: Arc<dyn AgentBackendFactory<WorkflowAgentBackendContext>>,
) -> Result<(), AgentBackendRegistryError> {
    let id = AgentBackendId::new(id)?;
    mounts.push(registry.mount(id, factory)?);
    Ok(())
}

/// Factory composition for generic workflow Agent routes.
///
/// The registry owns no business workflow. Plugin declarations are resolved
/// from the exact effective plugin set for an execution profile and installed
/// as one immutable snapshot. Replacing the snapshot is transactional: readers
/// either resolve the old generation or the new generation, never a partially
/// unmounted registry.
pub(super) struct WorkflowAgentBackendFactories {
    current: RwLock<Arc<WorkflowAgentBackendFactorySet>>,
}

impl WorkflowAgentBackendFactories {
    #[allow(
        clippy::expect_used,
        reason = "built-in provider ids and capabilities are static source invariants"
    )]
    pub(super) fn with_native_providers() -> Self {
        Self {
            current: RwLock::new(Arc::new(
                WorkflowAgentBackendFactorySet::new(Vec::new())
                    .expect("static Agent backend factories must be valid"),
            )),
        }
    }

    fn snapshot_for_plugin_backends(
        &self,
        plugin_backends: Vec<EffectivePluginAgentBackend>,
    ) -> Result<Arc<WorkflowAgentBackendFactorySet>, AgentBackendRegistryError> {
        let replacement = Arc::new(WorkflowAgentBackendFactorySet::new(plugin_backends)?);
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        if current.plugin_backends != replacement.plugin_backends
            || current.environment_digests != replacement.environment_digests
        {
            *current = Arc::clone(&replacement);
            return Ok(replacement);
        }
        Ok(Arc::clone(&current))
    }

    pub(super) fn resolve(
        &self,
        agent_ref: &str,
        config: &Config,
        plugin_backends: Vec<EffectivePluginAgentBackend>,
    ) -> Result<ResolvedWorkflowAgentBackendFactory, AgentBackendRegistryError> {
        let id = AgentBackendId::new(agent_ref)?;
        // Retain the exact immutable factory generation chosen for this call.
        // Another profile may replace the process-wide current snapshot as soon
        // as this method returns without changing this resolved generation.
        let current = self.snapshot_for_plugin_backends(plugin_backends)?;
        let factory = current
            .registry
            .resolve(&id, &profile_requirements(config))?;
        let plugin = current
            .plugin_backends
            .iter()
            .find(|backend| backend.id == agent_ref)
            .cloned();
        let environment_digest = current.environment_digests.get(agent_ref).cloned();
        Ok(ResolvedWorkflowAgentBackendFactory {
            factory,
            plugin,
            environment_digest,
        })
    }

    #[allow(
        dead_code,
        reason = "kept as the simple host-side construction convenience"
    )]
    pub(super) fn build(
        &self,
        agent_ref: &str,
        context: &WorkflowAgentBackendContext,
        plugin_backends: Vec<EffectivePluginAgentBackend>,
    ) -> Result<Arc<dyn OneShotAgentBackend>, AgentBackendFactoryError> {
        self.resolve(agent_ref, &context.config, plugin_backends)
            .map_err(AgentBackendFactoryError::Registry)?
            .build(context)
    }

    #[cfg(test)]
    fn sync_plugin_backends(
        &self,
        plugin_backends: Vec<EffectivePluginAgentBackend>,
    ) -> Result<(), AgentBackendRegistryError> {
        self.snapshot_for_plugin_backends(plugin_backends).map(drop)
    }

    #[cfg(test)]
    fn ids(&self) -> Vec<String> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .registry
            .list()
            .into_iter()
            .map(|descriptor| descriptor.id.to_string())
            .collect()
    }
}

fn profile_requirements(config: &Config) -> AgentBackendRequirements {
    AgentBackendRequirements {
        model: option_profile_scope(config.model.is_some()),
        reasoning_effort: option_profile_scope(config.model_reasoning_effort.is_some()),
        // Every shipped factory consumes the profile: native Codex receives the
        // full permission object, ACP receives its stable profile id, and
        // Claude Code receives the conservative mapped non-interactive mode.
        permission_mode: AgentBackendOptionScope::Profile,
        service_tier: option_profile_scope(config.service_tier.is_some()),
        ..AgentBackendRequirements::one_shot()
    }
}

fn option_profile_scope(configured: bool) -> AgentBackendOptionScope {
    if configured {
        AgentBackendOptionScope::Profile
    } else {
        AgentBackendOptionScope::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_plugin::PluginAgentBackendCommand;
    use codex_utils_plugins::PluginIdentity;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    fn plugin_backend(
        id: &str,
        plugin_id: &str,
        kind: PluginAgentBackendKind,
    ) -> EffectivePluginAgentBackend {
        let source_path = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path_checked(
            std::env::temp_dir().join(format!(
                "zuno-agent-backend-test-{}-{}-{}.json",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
                id.replace('/', "-")
            )),
        )
        .expect("absolute fixture source");
        std::fs::write(source_path.as_path(), b"fixture declaration")
            .expect("write fixture declaration");
        let source_digest = current_file_digest(source_path.as_path(), /*executable*/ false)
            .expect("hash fixture declaration");
        let executable_path =
            source_path
                .as_path()
                .with_extension(if cfg!(windows) { "exe" } else { "bin" });
        std::fs::write(&executable_path, b"fixture executable").expect("write fixture executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable_path)
                .expect("fixture executable metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable_path, permissions)
                .expect("make fixture executable");
        }
        let executable_path =
            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path_checked(executable_path)
                .expect("absolute fixture executable");
        EffectivePluginAgentBackend {
            id: id.to_string(),
            plugin_identity: PluginIdentity {
                plugin_id: plugin_id.to_string(),
                remote_plugin_id: None,
            },
            declaration: PluginAgentBackendDeclaration {
                local_id: id
                    .rsplit_once('/')
                    .map_or(id, |(_, local)| local)
                    .to_string(),
                kind,
                plugin_version: Some("1.0.0".to_string()),
                source_path,
                source_digest,
                executable_digest: None,
                source_generation: "fixture-source-generation".to_string(),
                command: matches!(
                    kind,
                    PluginAgentBackendKind::Acp | PluginAgentBackendKind::ClaudeCode
                )
                .then(|| PluginAgentBackendCommand::PluginPath(executable_path)),
                command_windows: None,
                args: Vec::new(),
                env_vars: Vec::new(),
                startup_timeout_ms: 20_000,
                run_timeout_ms: None,
                dispose_grace_ms: 3_000,
                max_message_bytes: 8 * 1024 * 1024,
            },
        }
    }

    #[test]
    fn default_factory_inventory_is_provider_only_and_sorted() {
        let factories = WorkflowAgentBackendFactories::with_native_providers();
        assert_eq!(factories.ids(), ["claude-code", "codex", "native-codex"]);
    }

    #[test]
    fn plugin_factory_revision_covers_declaration_and_owner_generation() {
        let base = plugin_backend("team/review", "team@test", PluginAgentBackendKind::Acp);
        let mut changed_argument = base.clone();
        changed_argument
            .declaration
            .args
            .push("--stdio".to_string());
        let mut changed_owner = base.clone();
        changed_owner.plugin_identity.remote_plugin_id = Some("remote-generation-2".to_string());
        let mut changed_source = base.clone();
        changed_source.declaration.source_generation = "fixture-source-generation-2".to_string();

        let empty_environment = BTreeMap::new();
        let base_revision = plugin_backend_revision(&base, &empty_environment, None);
        assert!(base_revision.starts_with("zuno/plugin-agent-backend/v1:"));
        assert_ne!(
            base_revision,
            plugin_backend_revision(&changed_argument, &empty_environment, None)
        );
        assert_ne!(
            base_revision,
            plugin_backend_revision(&changed_owner, &empty_environment, None)
        );
        assert_ne!(
            base_revision,
            plugin_backend_revision(&changed_source, &empty_environment, None)
        );
        assert_ne!(
            base_revision,
            plugin_backend_revision(
                &base,
                &BTreeMap::from([("TOKEN".to_string(), "rotated".to_string())]),
                None,
            )
        );
    }

    #[test]
    fn pinned_executable_bytes_change_the_factory_revision() {
        let mut backend = plugin_backend("team/review", "team@test", PluginAgentBackendKind::Acp);
        let first = pin_plugin_backend_executable(&mut backend)
            .expect("pin first executable")
            .expect("external backend has an executable");
        let first_revision = plugin_backend_revision(&backend, &BTreeMap::new(), Some(&first));
        std::fs::write(&first.path, b"changed fixture executable")
            .expect("change fixture executable");
        let second = pin_plugin_backend_executable(&mut backend)
            .expect("pin changed executable")
            .expect("external backend has an executable");
        let second_revision = plugin_backend_revision(&backend, &BTreeMap::new(), Some(&second));
        assert_ne!(first.digest, second.digest);
        assert_ne!(first_revision, second_revision);
    }

    #[test]
    fn plugin_factories_replace_as_one_atomic_inventory() {
        let factories = WorkflowAgentBackendFactories::with_native_providers();
        factories
            .sync_plugin_backends(vec![plugin_backend(
                "team/review",
                "team@test",
                PluginAgentBackendKind::Acp,
            )])
            .expect("mount plugin backend");
        assert_eq!(
            factories.ids(),
            ["claude-code", "codex", "native-codex", "team/review"]
        );

        factories
            .sync_plugin_backends(vec![plugin_backend(
                "other/build",
                "other@test",
                PluginAgentBackendKind::NativeCodex,
            )])
            .expect("replace plugin backend snapshot");
        assert_eq!(
            factories.ids(),
            ["claude-code", "codex", "native-codex", "other/build"]
        );
    }

    #[test]
    fn admitted_factory_snapshot_survives_another_profile_replacement() {
        let factories = WorkflowAgentBackendFactories::with_native_providers();
        let first = factories
            .snapshot_for_plugin_backends(vec![plugin_backend(
                "first/review",
                "first@test",
                PluginAgentBackendKind::Acp,
            )])
            .expect("first profile snapshot");
        let second = factories
            .snapshot_for_plugin_backends(vec![plugin_backend(
                "second/review",
                "second@test",
                PluginAgentBackendKind::Acp,
            )])
            .expect("second profile snapshot");

        let ids = |snapshot: &WorkflowAgentBackendFactorySet| {
            snapshot
                .registry
                .list()
                .into_iter()
                .map(|descriptor| descriptor.id.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&first),
            ["claude-code", "codex", "first/review", "native-codex"]
        );
        assert_eq!(
            ids(&second),
            ["claude-code", "codex", "native-codex", "second/review"]
        );
    }

    #[test]
    fn forwarded_environment_digest_replaces_an_otherwise_identical_snapshot() {
        let factories = WorkflowAgentBackendFactories::with_native_providers();
        let backend = plugin_backend(
            "team/review",
            "team@test",
            PluginAgentBackendKind::NativeCodex,
        );
        let mut stale = WorkflowAgentBackendFactorySet::new(vec![backend.clone()])
            .expect("construct stale plugin snapshot");
        stale.environment_digests.insert(
            "team/review".to_string(),
            "stale-environment-digest".to_string(),
        );
        *factories
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(stale);

        let refreshed = factories
            .snapshot_for_plugin_backends(vec![backend])
            .expect("refresh plugin snapshot");

        assert_ne!(
            refreshed
                .environment_digests
                .get("team/review")
                .map(String::as_str),
            Some("stale-environment-digest")
        );
    }

    #[test]
    fn duplicate_plugin_backend_ids_fail_without_replacing_current_snapshot() {
        let factories = WorkflowAgentBackendFactories::with_native_providers();
        let existing = plugin_backend("team/review", "team@test", PluginAgentBackendKind::Acp);
        factories
            .sync_plugin_backends(vec![existing.clone()])
            .expect("mount initial plugin backend");
        let duplicate = plugin_backend("team/review", "other@test", PluginAgentBackendKind::Acp);

        assert!(matches!(
            factories.sync_plugin_backends(vec![existing, duplicate]),
            Err(AgentBackendRegistryError::Duplicate { .. })
        ));
        assert_eq!(
            factories.ids(),
            ["claude-code", "codex", "native-codex", "team/review"]
        );
    }

    #[test]
    fn claude_profile_rejects_unsupported_service_tier_before_build() {
        let factories = WorkflowAgentBackendFactories::with_native_providers();
        let current = factories
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let descriptor = current
            .registry
            .list()
            .into_iter()
            .find(|descriptor| descriptor.id.as_str() == "claude-code")
            .expect("Claude factory");
        let requirements = AgentBackendRequirements {
            service_tier: AgentBackendOptionScope::Profile,
            ..AgentBackendRequirements::one_shot()
        };
        assert!(!descriptor.capabilities.supports(&requirements));
    }

    #[test]
    fn invalid_agent_ref_is_a_typed_registry_error() {
        assert!(matches!(
            AgentBackendId::new(" whitespace "),
            Err(AgentBackendRegistryError::InvalidId { .. })
        ));
    }
}
