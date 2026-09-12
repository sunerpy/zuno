//! Explicit deployment configuration. Secret values are read separately and are
//! never part of the immutable definition or any client/Worker DTO.

use crate::{Error, invalid};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt as _;
use zuno_application::runtime::{
    ConfigurationRef, JobInputModel, JobInputSelection, LeaseDuration,
};
use zuno_config::schema::provider::{ProviderSurface, ProviderTransport};
use zuno_identity::{
    EntraConfig, OAuth2Authority, OAuth2IntrospectionConfig, OAuth2JwtConfig, worker::WorkerSubject,
};
use zuno_permission::enterprise::OrganizationPolicy;
use zuno_types::identity::{ConfigurationId, GatewayId, PrincipalKey, TenantId, WorkspaceId};

const MAX_FILE_BYTES: u64 = 1024 * 1024;

pub async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let bytes = read_file(path, MAX_FILE_BYTES).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| invalid("the JSON file does not match its typed configuration"))
}
pub async fn read_file(path: &Path, maximum: u64) -> Result<Vec<u8>, Error> {
    if !path.is_absolute() {
        return Err(invalid("deployment file paths must be absolute"));
    }
    let file = tokio::fs::File::open(path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(invalid("deployment file is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > maximum {
        return Err(invalid("deployment file exceeds its bound"));
    }
    Ok(bytes)
}
pub async fn secret(path: &Path) -> Result<zuno_auth::Secret, Error> {
    let bytes = read_file(path, 65536).await?;
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("secret file is not UTF-8"))?
        .trim();
    if value.is_empty() || value.contains(['\0', '\n', '\r']) {
        return Err(invalid("secret file must contain one nonempty value"));
    }
    Ok(zuno_auth::Secret::new(value))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceConfig {
    pub state_directory: PathBuf,
    pub service: ServiceRole,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRole {
    Worker(WorkerConfig),
    Gateway(GatewayConfig),
    ControlPlane(Box<ControlConfig>),
    Migrate(MigrationConfig),
    Identity(IdentityConfig),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TlsConfig {
    pub listen: SocketAddr,
    pub certificate_file: PathBuf,
    pub private_key_file: PathBuf,
    #[serde(default = "connections")]
    pub max_connections: u32,
}
fn connections() -> u32 {
    256
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateClientConfig {
    pub endpoint: String,
    pub access_token_file: PathBuf,
    pub root_certificate: Option<PathBuf>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerConfig {
    pub instance_prefix: String,
    pub state: StateClientConfig,
    pub definitions: Vec<PathBuf>,
    #[serde(default)]
    pub credentials: BTreeMap<String, ModelCredential>,
    #[serde(default = "one")]
    pub slots: u32,
    #[serde(default = "poll")]
    pub poll_millis: u64,
    #[serde(default = "renew")]
    pub renew_millis: u64,
    #[serde(default = "drain")]
    pub drain_seconds: u64,
    /// Optional live snapshot publication interval; null disables transient progress.
    #[serde(default = "live_interval")]
    pub live_millis: Option<u64>,
}
fn one() -> u32 {
    1
}
fn poll() -> u64 {
    500
}
fn renew() -> u64 {
    5000
}
fn drain() -> u64 {
    30
}
fn live_interval() -> Option<u64> {
    Some(500)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCredential {
    pub api_key_file: Option<PathBuf>,
    /// Custom model trust roots currently use the compatible transport.
    pub root_certificate: Option<PathBuf>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayConfig {
    pub id: GatewayId,
    pub tls: TlsConfig,
    pub state: StateClientConfig,
    pub docker_socket: PathBuf,
    #[serde(default = "poll")]
    pub delivery_millis: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url_file: PathBuf,
    pub root_certificate: Option<PathBuf>,
    #[serde(default = "pool")]
    pub max_connections: u32,
}
fn pool() -> u32 {
    16
}
impl DatabaseConfig {
    pub async fn options(&self) -> Result<zuno_postgres::PostgresOptions, Error> {
        Ok(zuno_postgres::PostgresOptions {
            url: secret(&self.url_file).await?.expose().to_owned(),
            root_certificate: self.root_certificate.clone(),
            max_connections: self.max_connections,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeyFile {
    pub id: String,
    pub path: PathBuf,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeyFiles {
    pub active: String,
    pub keys: Vec<KeyFile>,
}
impl KeyFiles {
    pub async fn load(&self) -> Result<Vec<(String, Vec<u8>)>, Error> {
        if self.keys.is_empty() || self.keys.len() > 8 {
            return Err(invalid("configure 1–8 signing keys"));
        }
        let mut keys = Vec::new();
        for key in &self.keys {
            keys.push((key.id.clone(), read_file(&key.path, 64).await?))
        }
        Ok(keys)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewaySubject {
    pub subject: WorkerSubject,
    pub gateway_id: GatewayId,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefinitionKey {
    pub id: ConfigurationId,
    pub version: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlConfig {
    pub tenant_id: TenantId,
    pub tls: TlsConfig,
    pub database: DatabaseConfig,
    pub user_identity: VerifierConfig,
    pub service_identity: VerifierConfig,
    pub workers: BTreeSet<WorkerSubject>,
    pub gateways: Vec<GatewaySubject>,
    pub job_keys: KeyFiles,
    pub gateway_keys: KeyFiles,
    pub definitions: Vec<PathBuf>,
    pub active_definitions: Vec<DefinitionKey>,
    pub browser: Option<BrowserConfig>,
    /// Optional immutable browser bundle; requires the OIDC BFF.
    #[serde(default)]
    pub web_assets_directory: Option<PathBuf>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default = "lease")]
    pub lease_millis: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryConfig {
    pub concurrent_transactions: usize,
    pub global_characters: usize,
    pub project_characters: usize,
    pub transaction_timeout_millis: u64,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            concurrent_transactions: 4,
            global_characters: 2200,
            project_characters: 3000,
            transaction_timeout_millis: 10000,
        }
    }
}
impl MemoryConfig {
    pub fn limits(&self) -> zuno_postgres::MemoryStoreLimits {
        zuno_postgres::MemoryStoreLimits {
            concurrent_transactions: self.concurrent_transactions,
            scopes: zuno_memory::ScopeLimits::new(self.global_characters, self.project_characters),
            transaction_timeout: std::time::Duration::from_millis(self.transaction_timeout_millis),
        }
    }
}
fn lease() -> u32 {
    30000
}
impl ControlConfig {
    pub fn lease(&self) -> Result<LeaseDuration, Error> {
        Ok(LeaseDuration::new(self.lease_millis)?)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserConfig {
    pub authority: OAuth2Authority,
    pub client_id: String,
    pub client_secret_file: PathBuf,
    pub redirect_uri: String,
    pub scopes: BTreeSet<String>,
    pub root_certificate: Option<PathBuf>,
    pub encryption_keys: KeyFiles,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationConfig {
    pub database: DatabaseConfig,
    pub runtime_role: String,
    pub bootstrap: Option<BootstrapConfig>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapConfig {
    pub administrator: PrincipalKey,
    pub policy: OrganizationPolicy,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdentityConfig {
    pub verifier: VerifierConfig,
    pub access_token_file: PathBuf,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum VerifierConfig {
    Jwt {
        config: OAuth2JwtConfig,
        root_certificate: Option<PathBuf>,
    },
    Entra {
        config: EntraConfig,
    },
    Introspection {
        config: OAuth2IntrospectionConfig,
        client_id: String,
        client_secret_file: PathBuf,
    },
}

/// Share these logical bytes across control plane and compatible Workers.
/// Physical secret files and service endpoints/credentials are separate.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Definition {
    pub id: ConfigurationId,
    pub version: u32,
    pub workspace: WorkspaceDefinition,
    pub agent: AgentDefinition,
    pub model: ModelDefinition,
    pub budget: BudgetDefinition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<zuno_orchestration::WorkflowTemplateDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub councils: Vec<CouncilDefinition>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilDefinition {
    pub preset: zuno_orchestration::CouncilPresetDescriptor,
    pub synthesis: ConfigurationRef,
    /// Original seat Agent name -> an explicit completion profile using the
    /// same model. Repairs cannot rerun a seat's external tools.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repairs: BTreeMap<String, ConfigurationRef>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegationDefinition {
    pub targets: Vec<ConfigurationRef>,
    pub maximum_depth: u32,
    pub maximum_children: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceDefinition {
    pub id: WorkspaceId,
    pub title: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDefinition {
    pub name: String,
    pub system_prompt: String,
    pub max_steps: NonZeroU32,
    #[serde(default, skip_serializing_if = "AgentExecutionMode::is_agent")]
    pub mode: AgentExecutionMode,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionMode {
    #[default]
    Agent,
    Completion,
}
impl AgentExecutionMode {
    fn is_agent(&self) -> bool {
        *self == Self::Agent
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelDefinition {
    pub provider_id: String,
    pub model_id: String,
    pub transport: ProviderTransport,
    pub surface: Option<ProviderSurface>,
    pub base_url: Option<String>,
    pub region: Option<String>,
    pub project: Option<String>,
    pub api_version: Option<String>,
    pub credential: Option<String>,
    pub context_tokens: NonZeroU64,
    pub max_output_tokens: NonZeroU32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetDefinition {
    pub tokens: NonZeroU64,
    pub tool_calls: NonZeroU32,
    pub duration_seconds: NonZeroU64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentDefinition {
    pub gateway_id: GatewayId,
    pub endpoint: String,
    pub image: String,
    pub memory_bytes: u64,
    pub pids_limit: u32,
    pub cpu_millis: u32,
}
impl Definition {
    pub fn reference(&self) -> ConfigurationRef {
        ConfigurationRef {
            id: self.id.clone(),
            version: self.version,
            sha256: zuno_orchestration::sha256_json(&serde_json::json!(self)),
        }
    }
    pub fn selection(&self) -> JobInputSelection {
        JobInputSelection {
            agent: self.agent.name.clone(),
            model: JobInputModel {
                provider_id: self.model.provider_id.clone(),
                model_id: self.model.model_id.clone(),
            },
        }
    }
    pub fn validate(&self) -> Result<(), Error> {
        self.reference().validate()?;
        self.selection().validate()?;
        if let Some(delegation) = &self.delegation
            && (delegation.targets.is_empty()
                || delegation.targets.len() > 64
                || !(1..=16).contains(&delegation.maximum_depth)
                || !(1..=256).contains(&delegation.maximum_children))
        {
            return Err(invalid("invalid configured delegation targets or limits"));
        }
        if let Some(delegation) = &self.delegation {
            for target in &delegation.targets {
                target.validate()?;
            }
        }
        if self.agent.system_prompt.trim().is_empty()
            || self.agent.system_prompt.len() > 262144
            || self.agent.max_steps.get() > 4096
            || self.model.context_tokens.get() < 1024
            || self.model.context_tokens.get() > 16_000_000
            || u64::from(self.model.max_output_tokens.get()) >= self.model.context_tokens.get()
            || self.budget.tokens.get() > 1_000_000_000
            || self.budget.tool_calls.get() > 100000
            || self.budget.duration_seconds.get() > 86400
        {
            return Err(invalid("invalid Agent, model or budget bounds"));
        }
        if let Some(endpoint) = &self.model.base_url {
            https_endpoint(endpoint)?;
        }
        match (&self.agent.mode, &self.environment) {
            (AgentExecutionMode::Completion, None)
                if self.delegation.is_none()
                    && self.workflows.is_empty()
                    && self.councils.is_empty() => {}
            (AgentExecutionMode::Agent, Some(environment)) => {
                https_endpoint(&environment.endpoint)?;
                zuno_application::environment::EnvironmentSpec {
                    id: zuno_types::identity::EnvironmentId::new("validation")
                        .expect("fixed identity"),
                    session_id: zuno_types::identity::SessionId::new("validation")
                        .expect("fixed identity"),
                    image: environment.image.clone(),
                    memory_bytes: environment.memory_bytes,
                    pids_limit: environment.pids_limit,
                    cpu_millis: environment.cpu_millis,
                }
                .validate()?;
            }
            _ => {
                return Err(invalid(
                    "completion mode has no environment or delegation; agent mode requires an environment",
                ));
            }
        }
        Ok(())
    }
}
pub fn https_endpoint(value: &str) -> Result<url::Url, Error> {
    let url = url::Url::parse(value).map_err(|_| invalid("invalid service endpoint"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "service endpoints require HTTPS without embedded credentials",
        ));
    }
    Ok(url)
}

pub async fn definitions(paths: &[PathBuf]) -> Result<Vec<Definition>, Error> {
    if paths.is_empty() || paths.len() > 64 {
        return Err(invalid("configure 1–64 immutable definitions"));
    }
    let mut definitions = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        let definition: Definition = read_json(path).await?;
        definition.validate()?;
        if !seen.insert((definition.id.clone(), definition.version)) {
            return Err(invalid("duplicate configuration ID/version"));
        }
        definitions.push(definition);
    }
    Ok(definitions)
}

#[cfg(test)]
mod execution_mode_tests {
    use super::*;
    fn definition() -> Definition {
        serde_json::from_str(include_str!("../../../enterprise/examples/definition.json")).unwrap()
    }
    #[test]
    fn legacy_agent_definitions_keep_their_exact_normalized_reference() {
        let value = definition();
        value.validate().unwrap();
        let encoded = serde_json::to_value(&value).unwrap();
        assert!(encoded["agent"].get("mode").is_none());
        assert!(encoded["environment"].is_object());
        let mut original: serde_json::Value =
            serde_json::from_str(include_str!("../../../enterprise/examples/definition.json"))
                .unwrap();
        // These nullable model fields were already emitted by the original
        // definition serializer, even when absent from the operator's file.
        for field in ["region", "project", "apiVersion"] {
            original["model"]
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), serde_json::Value::Null);
        }
        assert_eq!(encoded, original);
        assert_eq!(
            value.reference().sha256,
            zuno_orchestration::sha256_json(&original)
        );
    }
    #[test]
    fn completion_profiles_require_no_execution_environment_or_delegation() {
        let mut value = definition();
        value.agent.mode = AgentExecutionMode::Completion;
        assert!(value.validate().is_err());
        value.environment = None;
        value.validate().unwrap();
        let encoded = serde_json::to_value(&value).unwrap();
        assert!(encoded.get("environment").is_none());
        assert_eq!(encoded["agent"]["mode"], "completion");
        value.delegation = Some(DelegationDefinition {
            targets: vec![value.reference()],
            maximum_depth: 2,
            maximum_children: 4,
        });
        assert!(value.validate().is_err());
    }
}
