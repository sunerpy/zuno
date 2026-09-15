use super::DEFAULT_DISPOSE_GRACE;
use super::OneShotAgentBackend;
use super::OneShotAgentBackendKind;
use super::OneShotAgentError;
use super::OneShotAgentFailureCategory;
use super::OneShotAgentFailureStage;
use super::OneShotAgentFuture;
use super::OneShotAgentRequest;
use super::OneShotAgentResult;
use super::OneShotProcessSandboxConfig;
use super::claude_code::ProcessTree;
use super::claude_code::spawn_owned_process;
use super::claude_code::terminate_process;
use super::prepare_external_process_command;
use super::valid_sha256;
use super::validate_request;
use super::verify_executable_sha256;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Child;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const ACP_PROTOCOL_VERSION: u64 = 1;
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_OPTION_CHARS: usize = 256;

/// Configuration for one external ACP stdio Agent provider instance.
///
/// The command, environment, model, effort, and mode belong to a trusted profile
/// or plugin. A workflow only names the mounted provider; it never supplies a
/// command, credential, or permission bypass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpBackendConfig {
    pub executable: PathBuf,
    /// Optional immutable executable digest checked again immediately before spawn.
    pub executable_sha256: Option<String>,
    pub args: Vec<OsString>,
    pub model: Option<String>,
    /// Logical provider ID forwarded in the Zuno ACP metadata extension.
    pub model_provider: Option<String>,
    pub reasoning_effort: Option<String>,
    pub mode: Option<String>,
    pub permissions: Option<String>,
    pub env: BTreeMap<String, String>,
    pub sandbox: Option<OneShotProcessSandboxConfig>,
    pub startup_timeout: Duration,
    pub run_timeout: Option<Duration>,
    pub dispose_grace: Duration,
    pub max_message_bytes: usize,
}

impl Default for AcpBackendConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::new(),
            executable_sha256: None,
            args: Vec::new(),
            model: None,
            model_provider: None,
            reasoning_effort: None,
            mode: None,
            permissions: None,
            env: BTreeMap::new(),
            sandbox: None,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            run_timeout: None,
            dispose_grace: DEFAULT_DISPOSE_GRACE,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

/// One-shot ACP client backed by a supervised external stdio process.
#[derive(Clone, Debug)]
pub struct AcpBackend {
    config: AcpBackendConfig,
}

impl AcpBackend {
    pub fn new(config: AcpBackendConfig) -> Result<Self, OneShotAgentError> {
        let invalid = config.executable.as_os_str().is_empty()
            || config
                .executable_sha256
                .as_deref()
                .is_some_and(|digest| !valid_sha256(digest))
            || config.startup_timeout.is_zero()
            || config.run_timeout.is_some_and(|timeout| timeout.is_zero())
            || config.dispose_grace.is_zero()
            || config.max_message_bytes == 0
            || [
                config.model.as_deref(),
                config.model_provider.as_deref(),
                config.reasoning_effort.as_deref(),
                config.mode.as_deref(),
                config.permissions.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(invalid_option);
        if invalid {
            return Err(one_shot_error(
                OneShotAgentFailureStage::Validate,
                OneShotAgentFailureCategory::InvalidRequest,
            ));
        }
        Ok(Self { config })
    }

    async fn run_protocol(
        &self,
        child: &mut Child,
        process_tree: &ProcessTree,
        request: OneShotAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<OneShotAgentResult, OneShotAgentError> {
        let stdin = child.stdin.take().ok_or_else(|| {
            one_shot_error(
                OneShotAgentFailureStage::Start,
                OneShotAgentFailureCategory::Process,
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            one_shot_error(
                OneShotAgentFailureStage::Start,
                OneShotAgentFailureCategory::Process,
            )
        })?;
        let mut wire = AcpWire::new(stdin, stdout, self.config.max_message_bytes);
        let startup_deadline = Some(Instant::now() + self.config.startup_timeout);
        let run_deadline = self.config.run_timeout.map(|limit| Instant::now() + limit);
        let mut answer = String::new();

        let initialized = wire
            .request(
                "initialize",
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": "Zuno",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                startup_deadline,
                &cancellation,
                &mut answer,
            )
            .await
            .map_err(|error| startup_error(error, OneShotAgentFailureStage::Start))?;
        if initialized.get("protocolVersion").and_then(Value::as_u64) != Some(ACP_PROTOCOL_VERSION)
        {
            return Err(one_shot_error(
                OneShotAgentFailureStage::Start,
                OneShotAgentFailureCategory::InvalidResult,
            ));
        }

        let session = wire
            .request(
                "session/new",
                self.new_session_params(&request),
                startup_deadline,
                &cancellation,
                &mut answer,
            )
            .await
            .map_err(|error| startup_error(error, OneShotAgentFailureStage::Start))?;
        let session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                one_shot_error(
                    OneShotAgentFailureStage::Start,
                    OneShotAgentFailureCategory::InvalidResult,
                )
            })?;

        for (method, params) in self.session_options(&session_id) {
            wire.request(method, params, startup_deadline, &cancellation, &mut answer)
                .await
                .map_err(|error| startup_error(error, OneShotAgentFailureStage::Start))?;
        }

        let prompt_result = wire
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": request.prompt }],
                }),
                run_deadline,
                &cancellation,
                &mut answer,
            )
            .await;
        if matches!(prompt_result, Err(WireError::Cancelled)) {
            let _ = wire
                .notify("session/cancel", json!({ "sessionId": session_id }))
                .await;
        }
        let prompt_result = prompt_result.map_err(run_error)?;
        let stop_reason = prompt_result
            .get("stopReason")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                one_shot_error(
                    OneShotAgentFailureStage::Decode,
                    OneShotAgentFailureCategory::InvalidResult,
                )
            })?;
        if stop_reason == "cancelled" {
            return Err(one_shot_error(
                OneShotAgentFailureStage::Run,
                OneShotAgentFailureCategory::Aborted,
            ));
        }
        if answer.trim().is_empty()
            && let Some(text) = response_text(&prompt_result)
        {
            answer.push_str(text);
        }
        if answer.trim().is_empty() {
            return Err(one_shot_error(
                OneShotAgentFailureStage::Decode,
                OneShotAgentFailureCategory::InvalidResult,
            ));
        }

        // An ACP provider owns a long-lived server process. One-shot dispatch
        // closes the exact process generation after its terminal response.
        terminate_process(child, process_tree, self.config.dispose_grace).await;
        Ok(OneShotAgentResult {
            backend: OneShotAgentBackendKind::Acp,
            final_answer: answer,
            product_session_id: Some(session_id),
        })
    }

    fn new_session_params(&self, request: &OneShotAgentRequest) -> Value {
        let mut zuno = Map::new();
        insert_option(&mut zuno, "model", self.config.model.as_deref());
        insert_option(
            &mut zuno,
            "modelProvider",
            self.config.model_provider.as_deref(),
        );
        insert_option(&mut zuno, "effort", self.config.reasoning_effort.as_deref());
        insert_option(&mut zuno, "permissions", self.config.permissions.as_deref());
        json!({
            "cwd": request.cwd.as_path().to_string_lossy(),
            "mcpServers": [],
            "_meta": { "zuno": zuno },
        })
    }

    fn session_options(&self, session_id: &str) -> Vec<(&'static str, Value)> {
        let mut options = Vec::new();
        if let Some(model) = &self.config.model {
            options.push((
                "session/set_model",
                json!({ "sessionId": session_id, "modelId": model }),
            ));
        }
        for (id, value) in [
            ("reasoning_effort", self.config.reasoning_effort.as_deref()),
            ("mode", self.config.mode.as_deref()),
            ("permissions", self.config.permissions.as_deref()),
        ] {
            if let Some(value) = value {
                options.push((
                    "session/set_config_option",
                    json!({ "sessionId": session_id, "configId": id, "value": value }),
                ));
            }
        }
        options
    }
}

impl OneShotAgentBackend for AcpBackend {
    fn kind(&self) -> OneShotAgentBackendKind {
        OneShotAgentBackendKind::Acp
    }

    fn capabilities(&self) -> crate::AgentBackendCapabilities {
        crate::AgentBackendCapabilities::acp()
    }

    fn run<'a>(
        &'a self,
        request: OneShotAgentRequest,
        cancellation: CancellationToken,
    ) -> OneShotAgentFuture<'a> {
        Box::pin(async move {
            validate_request(self.kind(), &request)?;
            verify_executable_sha256(
                self.kind(),
                &self.config.executable,
                self.config.executable_sha256.as_deref(),
            )
            .await?;
            let mut command = prepare_external_process_command(
                self.kind(),
                &self.config.executable,
                self.config.args.clone(),
                &self.config.env,
                &request.cwd,
                self.config.sandbox.as_ref(),
            )?;
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            let (mut child, process_tree) = spawn_owned_process(&mut command).map_err(|_| {
                one_shot_error(
                    OneShotAgentFailureStage::Start,
                    OneShotAgentFailureCategory::Process,
                )
            })?;
            let outcome = self
                .run_protocol(&mut child, &process_tree, request, cancellation)
                .await;
            if outcome.is_err() {
                terminate_process(&mut child, &process_tree, self.config.dispose_grace).await;
            }
            outcome
        })
    }
}

fn invalid_option(value: &str) -> bool {
    value.trim().is_empty()
        || value.trim() != value
        || value.chars().count() > MAX_OPTION_CHARS
        || value.chars().any(char::is_control)
}

fn insert_option(target: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        target.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn response_text(response: &Value) -> Option<&str> {
    response
        .get("text")
        .or_else(|| response.get("message"))
        .or_else(|| response.get("result"))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn one_shot_error(
    stage: OneShotAgentFailureStage,
    category: OneShotAgentFailureCategory,
) -> OneShotAgentError {
    OneShotAgentError::new(OneShotAgentBackendKind::Acp, stage, category)
}

fn startup_error(error: WireError, stage: OneShotAgentFailureStage) -> OneShotAgentError {
    let category = match error {
        WireError::Cancelled => OneShotAgentFailureCategory::Aborted,
        WireError::Timeout | WireError::Limit => OneShotAgentFailureCategory::Limit,
        WireError::Invalid | WireError::Remote => OneShotAgentFailureCategory::InvalidResult,
        WireError::Io | WireError::Closed => OneShotAgentFailureCategory::Process,
    };
    one_shot_error(stage, category)
}

fn run_error(error: WireError) -> OneShotAgentError {
    let category = match error {
        WireError::Cancelled => OneShotAgentFailureCategory::Aborted,
        WireError::Timeout | WireError::Limit => OneShotAgentFailureCategory::Limit,
        WireError::Remote => OneShotAgentFailureCategory::ProductError,
        WireError::Invalid => OneShotAgentFailureCategory::InvalidResult,
        WireError::Io | WireError::Closed => OneShotAgentFailureCategory::Unknown,
    };
    one_shot_error(OneShotAgentFailureStage::Run, category)
}

mod wire;

use wire::AcpWire;
use wire::WireError;

#[cfg(all(test, unix))]
#[path = "acp_tests.rs"]
mod tests;
