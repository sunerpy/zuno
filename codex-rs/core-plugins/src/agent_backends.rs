use codex_plugin::PLUGIN_AGENT_BACKENDS_API_VERSION;
use codex_plugin::PluginAgentBackendCommand;
use codex_plugin::PluginAgentBackendDeclaration;
use codex_plugin::PluginAgentBackendKind;
use codex_plugin::validate_plugin_segment;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Deserializer;
use serde::de::Error as SerdeError;
use serde::de::MapAccess;
use serde::de::Visitor;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Component;
use std::path::Path;
use tokio::io::AsyncReadExt;

const MAX_AGENT_BACKENDS_DOCUMENT_BYTES: u64 = 256 * 1024;
const MAX_AGENT_BACKENDS: usize = 64;
const MAX_ARGUMENTS: usize = 128;
const MAX_ARGUMENT_CHARS: usize = 4096;
const MAX_ENV_VARS: usize = 128;
const MAX_ENV_VAR_CHARS: usize = 256;
const MAX_COMMAND_CHARS: usize = 4096;
const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 20_000;
const MAX_STARTUP_TIMEOUT_MS: u64 = 300_000;
const MAX_RUN_TIMEOUT_MS: u64 = 86_400_000;
const DEFAULT_DISPOSE_GRACE_MS: u64 = 3_000;
const MAX_DISPOSE_GRACE_MS: u64 = 60_000;
const DEFAULT_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawAgentBackendsDocument {
    api_version: String,
    #[serde(default, deserialize_with = "deserialize_agent_backends")]
    backends: BTreeMap<String, RawAgentBackend>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawAgentBackend {
    kind: RawAgentBackendKind,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    command_windows: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env_vars: Vec<String>,
    #[serde(default)]
    startup_timeout_ms: Option<u64>,
    #[serde(default)]
    run_timeout_ms: Option<u64>,
    #[serde(default)]
    dispose_grace_ms: Option<u64>,
    #[serde(default)]
    max_message_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawAgentBackendKind {
    NativeCodex,
    ClaudeCode,
    Acp,
}

fn deserialize_agent_backends<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, RawAgentBackend>, D::Error>
where
    D: Deserializer<'de>,
{
    struct AgentBackendsVisitor;

    impl<'de> Visitor<'de> for AgentBackendsVisitor {
        type Value = BTreeMap<String, RawAgentBackend>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object of uniquely named Agent backend declarations")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut backends = BTreeMap::new();
            while let Some((id, backend)) = map.next_entry::<String, RawAgentBackend>()? {
                if backends.insert(id.clone(), backend).is_some() {
                    return Err(A::Error::custom(format!(
                        "duplicate Agent backend id {id:?}"
                    )));
                }
            }
            Ok(backends)
        }
    }

    deserializer.deserialize_map(AgentBackendsVisitor)
}

pub(crate) async fn load_plugin_agent_backends(
    plugin_root: &AbsolutePathBuf,
    document_path: Option<&AbsolutePathBuf>,
    plugin_version: Option<&str>,
) -> Result<Vec<PluginAgentBackendDeclaration>, String> {
    let Some(document_path) = document_path else {
        return Ok(Vec::new());
    };
    let metadata = tokio::fs::symlink_metadata(document_path.as_path())
        .await
        .map_err(|error| format!("failed to inspect Agent backend declarations: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Agent backend declarations path is not a regular file".to_string());
    }
    if metadata.len() > MAX_AGENT_BACKENDS_DOCUMENT_BYTES {
        return Err(format!(
            "Agent backend declarations exceed {MAX_AGENT_BACKENDS_DOCUMENT_BYTES} bytes"
        ));
    }
    let canonical_root = tokio::fs::canonicalize(plugin_root.as_path())
        .await
        .map_err(|error| format!("failed to resolve Agent backend plugin root: {error}"))?;
    let canonical_document = tokio::fs::canonicalize(document_path.as_path())
        .await
        .map_err(|error| format!("failed to resolve Agent backend declarations: {error}"))?;
    if !canonical_document.starts_with(&canonical_root) {
        return Err(
            "Agent backend declarations resolve outside the installed plugin root".to_string(),
        );
    }
    let contents = tokio::fs::read(document_path.as_path())
        .await
        .map_err(|error| format!("failed to read Agent backend declarations: {error}"))?;
    let raw: RawAgentBackendsDocument = serde_json::from_slice(&contents)
        .map_err(|error| format!("invalid Agent backend declarations: {error}"))?;
    let mut declarations = validate_document(plugin_root, document_path, plugin_version, raw)?;
    bind_source_generations(&canonical_root, &contents, &mut declarations).await?;
    Ok(declarations)
}

async fn bind_source_generations(
    canonical_root: &Path,
    document: &[u8],
    declarations: &mut [PluginAgentBackendDeclaration],
) -> Result<(), String> {
    let document_digest = hex_digest(document);
    for declaration in declarations {
        let command_digest = match declaration.command_for_current_platform() {
            Some(PluginAgentBackendCommand::PluginPath(command)) => {
                validate_package_command(canonical_root, declaration, command).await?;
                Some(digest_file(command.as_path()).await.map_err(|error| {
                    format!(
                        "failed to hash package command for Agent backend {:?}: {error}",
                        declaration.local_id
                    )
                })?)
            }
            Some(PluginAgentBackendCommand::Name(_)) | None => None,
        };
        declaration.source_digest.clone_from(&document_digest);
        declaration.executable_digest.clone_from(&command_digest);
        declaration.source_generation = format!(
            "zuno/plugin-agent-backend-source/v1:{}:{document_digest}:{}",
            declaration
                .plugin_version
                .as_deref()
                .unwrap_or("unversioned"),
            command_digest.as_deref().unwrap_or("external"),
        );
    }
    Ok(())
}

async fn validate_package_command(
    canonical_root: &Path,
    declaration: &PluginAgentBackendDeclaration,
    command: &AbsolutePathBuf,
) -> Result<(), String> {
    let metadata = tokio::fs::symlink_metadata(command.as_path())
        .await
        .map_err(|error| {
            format!(
                "failed to inspect package command for Agent backend {:?}: {error}",
                declaration.local_id
            )
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "package command for Agent backend {:?} is not a regular file",
            declaration.local_id
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "package command for Agent backend {:?} is not executable",
                declaration.local_id
            ));
        }
    }
    let canonical_command = tokio::fs::canonicalize(command.as_path())
        .await
        .map_err(|error| {
            format!(
                "failed to resolve package command for Agent backend {:?}: {error}",
                declaration.local_id
            )
        })?;
    if !canonical_command.starts_with(canonical_root) {
        return Err(format!(
            "package command for Agent backend {:?} resolves outside the installed plugin root",
            declaration.local_id
        ));
    }
    Ok(())
}

async fn digest_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(hex_from_digest(digest.finalize()));
        }
        digest.update(&buffer[..read]);
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    hex_from_digest(Sha256::digest(bytes))
}

fn hex_from_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_document(
    plugin_root: &AbsolutePathBuf,
    source_path: &AbsolutePathBuf,
    plugin_version: Option<&str>,
    raw: RawAgentBackendsDocument,
) -> Result<Vec<PluginAgentBackendDeclaration>, String> {
    if raw.api_version != PLUGIN_AGENT_BACKENDS_API_VERSION {
        return Err(format!(
            "unsupported Agent backend declarations apiVersion {:?}; expected {PLUGIN_AGENT_BACKENDS_API_VERSION:?}",
            raw.api_version
        ));
    }
    if raw.backends.len() > MAX_AGENT_BACKENDS {
        return Err(format!(
            "Agent backend declarations contain more than {MAX_AGENT_BACKENDS} entries"
        ));
    }

    raw.backends
        .into_iter()
        .map(|(local_id, raw)| {
            validate_backend(plugin_root, source_path, plugin_version, local_id, raw)
        })
        .collect()
}

fn validate_backend(
    plugin_root: &AbsolutePathBuf,
    source_path: &AbsolutePathBuf,
    plugin_version: Option<&str>,
    local_id: String,
    raw: RawAgentBackend,
) -> Result<PluginAgentBackendDeclaration, String> {
    validate_plugin_segment(&local_id, "Agent backend id")?;
    if local_id.chars().count() > 64 {
        return Err(format!(
            "invalid Agent backend id {local_id:?}: must contain at most 64 characters"
        ));
    }
    let command = raw
        .command
        .as_deref()
        .map(|command| validate_command(plugin_root, "command", command))
        .transpose()?;
    let command_windows = raw
        .command_windows
        .as_deref()
        .map(|command| validate_command(plugin_root, "commandWindows", command))
        .transpose()?;
    let args = validate_args(raw.args)?;
    let env_vars = validate_env_vars(raw.env_vars)?;
    let startup_timeout_ms = raw.startup_timeout_ms.unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS);
    let dispose_grace_ms = raw.dispose_grace_ms.unwrap_or(DEFAULT_DISPOSE_GRACE_MS);
    let max_message_bytes = raw.max_message_bytes.unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
    if startup_timeout_ms == 0 || startup_timeout_ms > MAX_STARTUP_TIMEOUT_MS {
        return Err(format!(
            "Agent backend {local_id:?} startupTimeoutMs must be between 1 and {MAX_STARTUP_TIMEOUT_MS}"
        ));
    }
    if raw
        .run_timeout_ms
        .is_some_and(|value| value == 0 || value > MAX_RUN_TIMEOUT_MS)
    {
        return Err(format!(
            "Agent backend {local_id:?} runTimeoutMs must be between 1 and {MAX_RUN_TIMEOUT_MS}"
        ));
    }
    if dispose_grace_ms == 0 || dispose_grace_ms > MAX_DISPOSE_GRACE_MS {
        return Err(format!(
            "Agent backend {local_id:?} disposeGraceMs must be between 1 and {MAX_DISPOSE_GRACE_MS}"
        ));
    }
    if max_message_bytes == 0 || max_message_bytes > MAX_MESSAGE_BYTES {
        return Err(format!(
            "Agent backend {local_id:?} maxMessageBytes must be between 1 and {MAX_MESSAGE_BYTES}"
        ));
    }

    let kind = match raw.kind {
        RawAgentBackendKind::NativeCodex => {
            if command.is_some()
                || command_windows.is_some()
                || !args.is_empty()
                || !env_vars.is_empty()
                || raw.startup_timeout_ms.is_some()
                || raw.run_timeout_ms.is_some()
                || raw.dispose_grace_ms.is_some()
                || raw.max_message_bytes.is_some()
            {
                return Err(format!(
                    "Agent backend {local_id:?} uses native-codex and cannot declare process settings"
                ));
            }
            PluginAgentBackendKind::NativeCodex
        }
        RawAgentBackendKind::ClaudeCode => {
            if !args.is_empty() || raw.startup_timeout_ms.is_some() {
                return Err(format!(
                    "Agent backend {local_id:?} uses claude-code; arguments are host-owned"
                ));
            }
            PluginAgentBackendKind::ClaudeCode
        }
        RawAgentBackendKind::Acp => {
            let command_is_package_local =
                matches!(command, Some(PluginAgentBackendCommand::PluginPath(_)));
            let windows_command_is_package_local = command_windows
                .as_ref()
                .is_none_or(|command| matches!(command, PluginAgentBackendCommand::PluginPath(_)));
            if !command_is_package_local || !windows_command_is_package_local {
                return Err(format!(
                    "Agent backend {local_id:?} uses acp and requires immutable ./ package commands on every configured platform"
                ));
            }
            PluginAgentBackendKind::Acp
        }
    };

    Ok(PluginAgentBackendDeclaration {
        local_id,
        kind,
        plugin_version: plugin_version.map(str::to_string),
        source_path: source_path.clone(),
        source_digest: String::new(),
        executable_digest: None,
        source_generation: String::new(),
        command,
        command_windows,
        args,
        env_vars,
        startup_timeout_ms,
        run_timeout_ms: raw.run_timeout_ms,
        dispose_grace_ms,
        max_message_bytes,
    })
}

fn validate_command(
    plugin_root: &AbsolutePathBuf,
    field: &str,
    command: &str,
) -> Result<PluginAgentBackendCommand, String> {
    let invalid = command.is_empty()
        || command.trim() != command
        || command.chars().count() > MAX_COMMAND_CHARS
        || command.chars().any(char::is_control);
    if invalid {
        return Err(format!("invalid Agent backend {field}"));
    }
    if let Some(relative) = command.strip_prefix("./") {
        if relative.contains(['\\', ':']) {
            return Err(format!(
                "invalid Agent backend {field}: package paths use portable forward-slash segments"
            ));
        }
        let relative = Path::new(relative);
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(format!(
                "invalid Agent backend {field}: package path must stay below the plugin root"
            ));
        }
        return Ok(PluginAgentBackendCommand::PluginPath(
            plugin_root.join(relative),
        ));
    }
    if Path::new(command).is_absolute()
        || command.contains(['/', '\\', ':'])
        || matches!(command, "." | "..")
    {
        return Err(format!(
            "invalid Agent backend {field}: use a bare executable name or a ./ package path"
        ));
    }
    Ok(PluginAgentBackendCommand::Name(command.to_string()))
}

fn validate_args(args: Vec<String>) -> Result<Vec<String>, String> {
    if args.len() > MAX_ARGUMENTS {
        return Err(format!(
            "Agent backend arguments exceed the {MAX_ARGUMENTS} entry limit"
        ));
    }
    if args.iter().any(|argument| {
        argument.chars().count() > MAX_ARGUMENT_CHARS || argument.chars().any(char::is_control)
    }) {
        return Err("Agent backend arguments contain an invalid value".to_string());
    }
    Ok(args)
}

fn validate_env_vars(env_vars: Vec<String>) -> Result<Vec<String>, String> {
    if env_vars.len() > MAX_ENV_VARS {
        return Err(format!(
            "Agent backend envVars exceed the {MAX_ENV_VARS} entry limit"
        ));
    }
    let mut unique = BTreeSet::new();
    for name in env_vars {
        let valid = !name.is_empty()
            && name.chars().count() <= MAX_ENV_VAR_CHARS
            && name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || index > 0 && byte.is_ascii_digit()
            });
        if !valid {
            return Err(format!(
                "invalid Agent backend environment variable {name:?}"
            ));
        }
        unique.insert(name);
    }
    Ok(unique.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn root(temp_dir: &TempDir) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute temp dir")
    }

    #[test]
    fn validates_provider_declarations_without_model_or_workflow_routes() {
        let temp_dir = TempDir::new().expect("tempdir");
        let plugin_root = root(&temp_dir);
        let source_path = plugin_root.join("agent-backends.json");
        let definitions = validate_document(
            &plugin_root,
            &source_path,
            Some("1.0.0"),
            serde_json::from_str(
                r#"{
                  "apiVersion": "zuno.agent-backends/v1",
                  "backends": {
                    "native": {"kind": "native-codex"},
                    "claude": {
                      "kind": "claude-code",
                      "command": "claude",
                      "envVars": ["CLAUDE_CONFIG_DIR"]
                    },
                    "review": {
                      "kind": "acp",
                      "command": "./bin/review-acp",
                      "commandWindows": "./bin/review-acp.exe",
                      "args": ["--stdio"],
                      "envVars": ["AWS_PROFILE", "AWS_PROFILE"]
                    }
                  }
                }"#,
            )
            .expect("document"),
        )
        .expect("valid declarations");

        assert_eq!(definitions.len(), 3);
        let review = definitions
            .iter()
            .find(|definition| definition.local_id == "review")
            .expect("ACP declaration");
        assert_eq!(review.kind, PluginAgentBackendKind::Acp);
        assert_eq!(review.plugin_version.as_deref(), Some("1.0.0"));
        assert_eq!(review.source_path, source_path);
        assert_eq!(review.args, ["--stdio"]);
        assert_eq!(review.env_vars, ["AWS_PROFILE"]);
        assert_eq!(
            review.command,
            Some(PluginAgentBackendCommand::PluginPath(
                root(&temp_dir).join("bin/review-acp")
            ))
        );
    }

    #[test]
    fn rejects_model_routes_and_unsafe_commands() {
        let temp_dir = TempDir::new().expect("tempdir");
        let with_model = serde_json::from_str::<RawAgentBackendsDocument>(
            r#"{
              "apiVersion": "zuno.agent-backends/v1",
              "backends": {"review": {"kind": "acp", "command": "review", "model": "opus"}}
            }"#,
        )
        .expect_err("model must remain executionProfile-owned");
        assert!(with_model.to_string().contains("unknown field `model`"));

        let traversal: RawAgentBackendsDocument = serde_json::from_str(
            r#"{
              "apiVersion": "zuno.agent-backends/v1",
              "backends": {"review": {"kind": "acp", "command": "./../outside"}}
            }"#,
        )
        .expect("raw document");
        let plugin_root = root(&temp_dir);
        assert!(
            validate_document(
                &plugin_root,
                &plugin_root.join("agent-backends.json"),
                None,
                traversal,
            )
            .is_err()
        );

        let external_windows_command: RawAgentBackendsDocument = serde_json::from_str(
            r#"{
              "apiVersion": "zuno.agent-backends/v1",
              "backends": {
                "review": {
                  "kind": "acp",
                  "command": "./bin/review-acp",
                  "commandWindows": "review-acp.exe"
                }
              }
            }"#,
        )
        .expect("raw document");
        assert!(
            validate_document(
                &plugin_root,
                &plugin_root.join("agent-backends.json"),
                None,
                external_windows_command,
            )
            .is_err(),
            "ACP commandWindows must not escape package content binding"
        );
    }

    #[test]
    fn rejects_duplicate_ids_and_out_of_range_limits() {
        let duplicate = serde_json::from_str::<RawAgentBackendsDocument>(
            r#"{
              "apiVersion": "zuno.agent-backends/v1",
              "backends": {
                "review": {"kind": "acp", "command": "review"},
                "review": {"kind": "acp", "command": "other"}
              }
            }"#,
        )
        .expect_err("duplicate ids must not be overwritten");
        assert!(duplicate.to_string().contains("duplicate Agent backend id"));

        let temp_dir = TempDir::new().expect("tempdir");
        let plugin_root = root(&temp_dir);
        let source_path = plugin_root.join("agent-backends.json");
        let invalid: RawAgentBackendsDocument = serde_json::from_str(
            r#"{
              "apiVersion": "zuno.agent-backends/v1",
              "backends": {
                "review": {
                  "kind": "acp",
                  "command": "review",
                  "startupTimeoutMs": 0,
                  "maxMessageBytes": 67108865
                }
              }
            }"#,
        )
        .expect("raw document");
        assert!(
            validate_document(&plugin_root, &source_path, None, invalid).is_err(),
            "invalid process limits must fail closed"
        );
    }
}
