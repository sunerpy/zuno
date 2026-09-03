use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::HeaderValue;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::{Host, Url};

use crate::AwsCredentials;

pub const CREDENTIAL_CHAIN_ORDER: [CredentialSource; 6] = [
    CredentialSource::Explicit,
    CredentialSource::Environment,
    CredentialSource::Profile,
    CredentialSource::Sso,
    CredentialSource::Container,
    CredentialSource::Imds,
];

const CREDENTIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CREDENTIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Explicit,
    Environment,
    Profile,
    Sso,
    Container,
    Imds,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCredentials {
    pub credentials: AwsCredentials,
    pub source: CredentialSource,
}

#[derive(Clone, Default)]
pub struct CredentialChainConfig {
    pub explicit: Option<AwsCredentials>,
    pub profile: Option<String>,
    pub credentials_file: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub sso_cache_dir: Option<PathBuf>,
    pub sso_endpoint: Option<Url>,
    pub imds_endpoint: Option<Url>,
    pub disable_imds: bool,
}

impl std::fmt::Debug for CredentialChainConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialChainConfig")
            .field("explicit", &self.explicit)
            .field("profile", &self.profile)
            .field("credentials_file", &self.credentials_file)
            .field("config_file", &self.config_file)
            .field("sso_cache_dir", &self.sso_cache_dir)
            .field("sso_endpoint", &self.sso_endpoint)
            .field("imds_endpoint", &self.imds_endpoint)
            .field("disable_imds", &self.disable_imds)
            .finish()
    }
}

#[derive(Clone)]
pub struct CredentialResolver {
    config: CredentialChainConfig,
    network_client: reqwest::Client,
    metadata_client: reqwest::Client,
}

impl std::fmt::Debug for CredentialResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialResolver")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl CredentialResolver {
    pub fn new(config: CredentialChainConfig) -> Result<Self, CredentialError> {
        let network_client = zuno_network::client_builder()
            .connect_timeout(CREDENTIAL_CONNECT_TIMEOUT)
            .timeout(CREDENTIAL_REQUEST_TIMEOUT)
            .build()
            .map_err(CredentialError::Http)?;
        let metadata_client = metadata_client().map_err(CredentialError::Http)?;
        Ok(Self::with_clients(config, network_client, metadata_client))
    }

    /// Reuse a provider's proxy-aware client while keeping cloud metadata direct.
    pub fn with_network_client(
        config: CredentialChainConfig,
        network_client: reqwest::Client,
    ) -> reqwest::Result<Self> {
        Ok(Self::with_clients(
            config,
            network_client,
            metadata_client()?,
        ))
    }

    /// Supply both credential transports explicitly.
    #[must_use]
    pub fn with_clients(
        config: CredentialChainConfig,
        network_client: reqwest::Client,
        metadata_client: reqwest::Client,
    ) -> Self {
        Self {
            config,
            network_client,
            metadata_client,
        }
    }

    pub async fn resolve(&self) -> Result<ResolvedCredentials, CredentialError> {
        if let Some(credentials) = &self.config.explicit {
            return Ok(resolved(credentials.clone(), CredentialSource::Explicit));
        }
        if let Some(credentials) = environment_credentials()? {
            return Ok(resolved(credentials, CredentialSource::Environment));
        }

        let profile = self
            .config
            .profile
            .clone()
            .or_else(|| std::env::var("AWS_PROFILE").ok())
            .unwrap_or_else(|| "default".to_owned());
        let files = ProfileFiles::resolve(&self.config)?;
        if let Some(profile_credentials) = load_profile(&files, &profile)? {
            match profile_credentials {
                ProfileCredentials::Static(credentials) => {
                    return Ok(resolved(credentials, CredentialSource::Profile));
                }
                ProfileCredentials::Sso(sso) => {
                    let credentials = self.resolve_sso(&files, &sso).await?;
                    return Ok(resolved(credentials, CredentialSource::Sso));
                }
            }
        }

        if let Some(credentials) = self.resolve_container().await? {
            return Ok(resolved(credentials, CredentialSource::Container));
        }
        let imds_disabled = self.config.disable_imds
            || std::env::var("AWS_EC2_METADATA_DISABLED")
                .is_ok_and(|value| value.eq_ignore_ascii_case("true"));
        if !imds_disabled && let Some(credentials) = self.resolve_imds().await? {
            return Ok(resolved(credentials, CredentialSource::Imds));
        }
        Err(CredentialError::NotFound { profile })
    }

    async fn resolve_sso(
        &self,
        files: &ProfileFiles,
        profile: &SsoProfile,
    ) -> Result<AwsCredentials, CredentialError> {
        let cache_dir = self
            .config
            .sso_cache_dir
            .clone()
            .unwrap_or_else(|| files.aws_dir.join("sso/cache"));
        let token = find_sso_token(&cache_dir, &profile.start_url)?;
        if token.expires_at <= OffsetDateTime::now_utc() {
            return Err(CredentialError::SsoTokenExpired {
                start_url: profile.start_url.clone(),
            });
        }
        let endpoint = match &self.config.sso_endpoint {
            Some(endpoint) => endpoint.clone(),
            None => Url::parse(&format!(
                "https://portal.sso.{}.amazonaws.com/federation/credentials",
                profile.region
            ))
            .map_err(|source| CredentialError::InvalidEndpoint {
                endpoint: profile.region.clone(),
                source,
            })?,
        };
        let response = self
            .network_client
            .get(endpoint)
            .query(&[
                ("account_id", profile.account_id.as_str()),
                ("role_name", profile.role_name.as_str()),
            ])
            .header("x-amz-sso_bearer_token", token.access_token)
            .send()
            .await
            .map_err(CredentialError::Http)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(CredentialError::Http)?;
        if !status.is_success() {
            return Err(CredentialError::CredentialService {
                source_name: "SSO",
                status: status.as_u16(),
            });
        }
        let payload: SsoRoleResponse =
            serde_json::from_slice(&bytes).map_err(|source| CredentialError::InvalidJson {
                source_name: "SSO",
                source,
            })?;
        let expiration = OffsetDateTime::from_unix_timestamp(
            payload.role_credentials.expiration.div_euclid(1_000),
        )
        .map_err(|_| CredentialError::InvalidExpiration { source_name: "SSO" })?;
        Ok(AwsCredentials::new(
            payload.role_credentials.access_key_id,
            payload.role_credentials.secret_access_key,
        )
        .with_session_token(payload.role_credentials.session_token)
        .with_expiration(expiration))
    }

    async fn resolve_container(&self) -> Result<Option<AwsCredentials>, CredentialError> {
        let endpoint =
            match (
                std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").ok(),
                std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI").ok(),
            ) {
                (Some(relative), _) => Url::parse(&format!("http://169.254.170.2{relative}"))
                    .map_err(|source| CredentialError::InvalidEndpoint {
                        endpoint: relative,
                        source,
                    })?,
                (None, Some(full)) => validate_container_endpoint(&full)?,
                (None, None) => return Ok(None),
            };
        let client = if is_local_metadata_endpoint(&endpoint) {
            &self.metadata_client
        } else {
            &self.network_client
        };
        let mut request = client.get(endpoint);
        if let Some(token) = container_authorization_token()? {
            request = request.header("authorization", token);
        }
        let response = request.send().await.map_err(CredentialError::Http)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(CredentialError::Http)?;
        if !status.is_success() {
            return Err(CredentialError::CredentialService {
                source_name: "container metadata",
                status: status.as_u16(),
            });
        }
        parse_metadata_credentials(&bytes, "container metadata").map(Some)
    }

    async fn resolve_imds(&self) -> Result<Option<AwsCredentials>, CredentialError> {
        let endpoint = match self.config.imds_endpoint.clone().or_else(|| {
            std::env::var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
                .ok()
                .and_then(|value| Url::parse(&value).ok())
        }) {
            Some(endpoint) => endpoint,
            None => Url::parse("http://169.254.169.254").expect("static IMDS URL is valid"),
        };
        let token_url = endpoint.join("/latest/api/token").map_err(|source| {
            CredentialError::InvalidEndpoint {
                endpoint: endpoint.to_string(),
                source,
            }
        })?;
        let token_response = match self
            .metadata_client
            .put(token_url)
            .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            Ok(_) | Err(_) => return Ok(None),
        };
        let token_bytes = token_response
            .bytes()
            .await
            .map_err(CredentialError::Http)?;
        let token = strict_utf8(&token_bytes, "IMDS token")?;
        let role_url = endpoint
            .join("/latest/meta-data/iam/security-credentials/")
            .map_err(|source| CredentialError::InvalidEndpoint {
                endpoint: endpoint.to_string(),
                source,
            })?;
        let role_response = self
            .metadata_client
            .get(role_url.clone())
            .header("x-aws-ec2-metadata-token", token)
            .send()
            .await
            .map_err(CredentialError::Http)?;
        if !role_response.status().is_success() {
            return Ok(None);
        }
        let role_bytes = role_response.bytes().await.map_err(CredentialError::Http)?;
        let role = strict_utf8(&role_bytes, "IMDS role name")?.trim();
        if role.is_empty() || role.contains(['/', '\\']) {
            return Err(CredentialError::InvalidMetadata {
                source_name: "IMDS role name",
            });
        }
        let credentials_url =
            role_url
                .join(role)
                .map_err(|source| CredentialError::InvalidEndpoint {
                    endpoint: role_url.to_string(),
                    source,
                })?;
        let response = self
            .metadata_client
            .get(credentials_url)
            .header("x-aws-ec2-metadata-token", token)
            .send()
            .await
            .map_err(CredentialError::Http)?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let bytes = response.bytes().await.map_err(CredentialError::Http)?;
        parse_metadata_credentials(&bytes, "IMDS").map(Some)
    }
}

fn metadata_client() -> reqwest::Result<reqwest::Client> {
    zuno_network::direct_client_builder(zuno_network::DirectPurpose::CloudMetadata)
        .connect_timeout(CREDENTIAL_CONNECT_TIMEOUT)
        .timeout(CREDENTIAL_REQUEST_TIMEOUT)
        .build()
}

fn is_local_metadata_endpoint(endpoint: &Url) -> bool {
    match endpoint.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => {
            address.is_loopback()
                || matches!(address.octets(), [169, 254, 170, 2] | [169, 254, 170, 23])
        }
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn resolved(credentials: AwsCredentials, source: CredentialSource) -> ResolvedCredentials {
    ResolvedCredentials {
        credentials,
        source,
    }
}

fn environment_credentials() -> Result<Option<AwsCredentials>, CredentialError> {
    let access_key = std::env::var("AWS_ACCESS_KEY_ID")
        .ok()
        .or_else(|| std::env::var("AWS_ACCESS_KEY").ok());
    let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
    match (access_key, secret_key) {
        (None, None) => Ok(None),
        (Some(access_key), Some(secret_key)) => {
            let mut credentials = AwsCredentials::new(access_key, secret_key);
            if let Ok(token) = std::env::var("AWS_SESSION_TOKEN") {
                credentials = credentials.with_session_token(token);
            }
            Ok(Some(credentials))
        }
        _ => Err(CredentialError::IncompleteEnvironment),
    }
}

#[derive(Debug)]
struct ProfileFiles {
    aws_dir: PathBuf,
    credentials: PathBuf,
    config: PathBuf,
}

impl ProfileFiles {
    fn resolve(config: &CredentialChainConfig) -> Result<Self, CredentialError> {
        let home = dirs::home_dir().ok_or(CredentialError::HomeDirectoryUnavailable)?;
        let aws_dir = home.join(".aws");
        let credentials = config
            .credentials_file
            .clone()
            .or_else(|| std::env::var_os("AWS_SHARED_CREDENTIALS_FILE").map(PathBuf::from))
            .unwrap_or_else(|| aws_dir.join("credentials"));
        let config_path = config
            .config_file
            .clone()
            .or_else(|| std::env::var_os("AWS_CONFIG_FILE").map(PathBuf::from))
            .unwrap_or_else(|| aws_dir.join("config"));
        Ok(Self {
            aws_dir,
            credentials,
            config: config_path,
        })
    }
}

enum ProfileCredentials {
    Static(AwsCredentials),
    Sso(SsoProfile),
}

#[derive(Debug)]
struct SsoProfile {
    start_url: String,
    region: String,
    account_id: String,
    role_name: String,
}

fn load_profile(
    files: &ProfileFiles,
    profile: &str,
) -> Result<Option<ProfileCredentials>, CredentialError> {
    let credentials = read_ini_optional(&files.credentials)?;
    let config = read_ini_optional(&files.config)?;
    let config_section = if profile == "default" {
        "default".to_owned()
    } else {
        format!("profile {profile}")
    };
    let mut values = BTreeMap::new();
    if let Some(section) = config.get(&config_section) {
        values.extend(section.clone());
    }
    if let Some(section) = credentials.get(profile) {
        values.extend(section.clone());
    }
    if values.is_empty() {
        return Ok(None);
    }

    match (
        values.get("aws_access_key_id"),
        values.get("aws_secret_access_key"),
    ) {
        (Some(access_key), Some(secret_key)) => {
            let mut credentials = AwsCredentials::new(access_key, secret_key);
            if let Some(token) = values.get("aws_session_token") {
                credentials = credentials.with_session_token(token);
            }
            return Ok(Some(ProfileCredentials::Static(credentials)));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(CredentialError::InvalidProfile {
                profile: profile.to_owned(),
                detail: "static credentials require both access and secret keys".to_owned(),
            });
        }
        (None, None) => {}
    }

    let session_values = values
        .get("sso_session")
        .and_then(|name| config.get(&format!("sso-session {name}")));
    let field = |name: &str| {
        values
            .get(name)
            .or_else(|| session_values.and_then(|section| section.get(name)))
            .cloned()
    };
    let Some(start_url) = field("sso_start_url") else {
        return Ok(None);
    };
    let region = required_profile_field(profile, "sso_region", field("sso_region"))?;
    let account_id = required_profile_field(profile, "sso_account_id", field("sso_account_id"))?;
    let role_name = required_profile_field(profile, "sso_role_name", field("sso_role_name"))?;
    Ok(Some(ProfileCredentials::Sso(SsoProfile {
        start_url,
        region,
        account_id,
        role_name,
    })))
}

fn required_profile_field(
    profile: &str,
    name: &str,
    value: Option<String>,
) -> Result<String, CredentialError> {
    value.ok_or_else(|| CredentialError::InvalidProfile {
        profile: profile.to_owned(),
        detail: format!("SSO profile is missing `{name}`"),
    })
}

type Ini = BTreeMap<String, BTreeMap<String, String>>;

fn read_ini_optional(path: &Path) -> Result<Ini, CredentialError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_ini(path, &text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ini::new()),
        Err(source) => Err(CredentialError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn parse_ini(path: &Path, text: &str) -> Result<Ini, CredentialError> {
    let mut output = Ini::new();
    let mut current = None::<String>;
    for (line_index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            let section = section.trim();
            if section.is_empty() {
                return Err(CredentialError::InvalidIni {
                    path: path.to_path_buf(),
                    line: line_index + 1,
                });
            }
            current = Some(section.to_owned());
            output.entry(section.to_owned()).or_default();
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(CredentialError::InvalidIni {
                path: path.to_path_buf(),
                line: line_index + 1,
            });
        };
        let Some(section) = &current else {
            return Err(CredentialError::InvalidIni {
                path: path.to_path_buf(),
                line: line_index + 1,
            });
        };
        output
            .entry(section.clone())
            .or_default()
            .insert(name.trim().to_owned(), value.trim().to_owned());
    }
    Ok(output)
}

// `Debug` is deliberately not derived: these fields carry live AWS secrets, and a
// derived `Debug` would print them into any diagnostic that formats the value.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SsoCacheToken {
    start_url: String,
    access_token: String,
    expires_at: String,
}

struct ParsedSsoToken {
    access_token: String,
    expires_at: OffsetDateTime,
}

fn find_sso_token(cache_dir: &Path, start_url: &str) -> Result<ParsedSsoToken, CredentialError> {
    let entries = std::fs::read_dir(cache_dir).map_err(|source| CredentialError::Io {
        path: cache_dir.to_path_buf(),
        source,
    })?;
    let mut selected = None::<ParsedSsoToken>;
    for entry in entries {
        let entry = entry.map_err(|source| CredentialError::Io {
            path: cache_dir.to_path_buf(),
            source,
        })?;
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(token) = serde_json::from_slice::<SsoCacheToken>(&bytes) else {
            continue;
        };
        if token.start_url != start_url {
            continue;
        }
        let expires_at = parse_timestamp(&token.expires_at)
            .ok_or(CredentialError::InvalidExpiration { source_name: "SSO" })?;
        if selected
            .as_ref()
            .is_none_or(|current| expires_at > current.expires_at)
        {
            selected = Some(ParsedSsoToken {
                access_token: token.access_token,
                expires_at,
            });
        }
    }
    selected.ok_or_else(|| CredentialError::SsoTokenNotFound {
        start_url: start_url.to_owned(),
        cache_dir: cache_dir.to_path_buf(),
    })
}

fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok().or_else(|| {
        value
            .strip_suffix("UTC")
            .and_then(|value| OffsetDateTime::parse(&format!("{value}Z"), &Rfc3339).ok())
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SsoRoleResponse {
    role_credentials: SsoRoleCredentials,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SsoRoleCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    expiration: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataCredentials {
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: Option<String>,
}

fn parse_metadata_credentials(
    bytes: &[u8],
    source_name: &'static str,
) -> Result<AwsCredentials, CredentialError> {
    let payload: MetadataCredentials =
        serde_json::from_slice(bytes).map_err(|source| CredentialError::InvalidJson {
            source_name,
            source,
        })?;
    let mut credentials = AwsCredentials::new(payload.access_key_id, payload.secret_access_key)
        .with_session_token(payload.token);
    if let Some(expiration) = payload.expiration {
        credentials = credentials.with_expiration(
            parse_timestamp(&expiration)
                .ok_or(CredentialError::InvalidExpiration { source_name })?,
        );
    }
    Ok(credentials)
}

fn validate_container_endpoint(value: &str) -> Result<Url, CredentialError> {
    let endpoint = Url::parse(value).map_err(|source| CredentialError::InvalidEndpoint {
        endpoint: value.to_owned(),
        source,
    })?;
    let host = endpoint.host_str().unwrap_or_default();
    let secure = endpoint.scheme() == "https";
    let local_http = endpoint.scheme() == "http"
        && matches!(
            host,
            "localhost" | "127.0.0.1" | "::1" | "169.254.170.2" | "169.254.170.23"
        );
    if secure || local_http {
        Ok(endpoint)
    } else {
        Err(CredentialError::UnsafeContainerEndpoint {
            endpoint: value.to_owned(),
        })
    }
}

fn container_authorization_token() -> Result<Option<HeaderValue>, CredentialError> {
    if let Ok(path) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        let token = std::fs::read_to_string(&path).map_err(|source| CredentialError::Io {
            path: PathBuf::from(path),
            source,
        })?;
        return HeaderValue::from_str(token.trim()).map(Some).map_err(|_| {
            CredentialError::InvalidMetadata {
                source_name: "container authorization token",
            }
        });
    }
    std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN")
        .ok()
        .map(|token| {
            HeaderValue::from_str(&token).map_err(|_| CredentialError::InvalidMetadata {
                source_name: "container authorization token",
            })
        })
        .transpose()
}

fn strict_utf8<'a>(bytes: &'a [u8], source_name: &'static str) -> Result<&'a str, CredentialError> {
    std::str::from_utf8(bytes).map_err(|_| CredentialError::InvalidMetadata { source_name })
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error(
        "no AWS credentials found after checking explicit, environment, profile/SSO, container, and IMDS sources (profile `{profile}`)"
    )]
    NotFound { profile: String },
    #[error("AWS environment credentials are incomplete")]
    IncompleteEnvironment,
    #[error("the home directory required for AWS profile discovery is unavailable")]
    HomeDirectoryUnavailable,
    #[error("failed to read AWS credential file `{path}`")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid AWS INI file `{path}` at line {line}")]
    InvalidIni { path: PathBuf, line: usize },
    #[error("invalid AWS profile `{profile}`: {detail}")]
    InvalidProfile { profile: String, detail: String },
    #[error("no SSO token for `{start_url}` was found in `{cache_dir}`")]
    SsoTokenNotFound {
        start_url: String,
        cache_dir: PathBuf,
    },
    #[error("the cached SSO token for `{start_url}` has expired")]
    SsoTokenExpired { start_url: String },
    #[error("AWS {source_name} returned HTTP {status}")]
    CredentialService {
        source_name: &'static str,
        status: u16,
    },
    #[error("AWS {source_name} returned malformed JSON")]
    InvalidJson {
        source_name: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("AWS {source_name} returned an invalid expiration")]
    InvalidExpiration { source_name: &'static str },
    #[error("AWS {source_name} returned invalid metadata")]
    InvalidMetadata { source_name: &'static str },
    #[error("invalid AWS endpoint `{endpoint}`")]
    InvalidEndpoint {
        endpoint: String,
        #[source]
        source: url::ParseError,
    },
    #[error(
        "container credential endpoint `{endpoint}` must use HTTPS or an AWS-approved local HTTP host"
    )]
    UnsafeContainerEndpoint { endpoint: String },
    #[error("AWS credential HTTP request failed")]
    Http(#[source] reqwest::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_chain_order_matches_aws_precedence_for_supported_sources() {
        assert_eq!(
            CREDENTIAL_CHAIN_ORDER,
            [
                CredentialSource::Explicit,
                CredentialSource::Environment,
                CredentialSource::Profile,
                CredentialSource::Sso,
                CredentialSource::Container,
                CredentialSource::Imds,
            ]
        );
    }

    #[test]
    fn static_profile_and_sso_session_sections_parse() {
        let credentials = parse_ini(
            Path::new("credentials"),
            "[dev]\naws_access_key_id = AKID\naws_secret_access_key = SECRET\n",
        )
        .expect("credentials INI");
        assert_eq!(credentials["dev"]["aws_access_key_id"], "AKID");

        let config = parse_ini(
            Path::new("config"),
            "[profile corp]\nsso_session=company\nsso_account_id=123\nsso_role_name=Dev\n\
             [sso-session company]\nsso_start_url=https://example.awsapps.com/start\nsso_region=us-east-1\n",
        )
        .expect("config INI");
        assert_eq!(config["profile corp"]["sso_session"], "company");
        assert_eq!(config["sso-session company"]["sso_region"], "us-east-1");
    }

    #[test]
    fn credentials_never_render_secret_material() {
        let credentials =
            AwsCredentials::new("AKID-CANARY", "SECRET-CANARY").with_session_token("TOKEN-CANARY");
        let rendered = format!("{credentials:?}");
        for secret in ["AKID-CANARY", "SECRET-CANARY", "TOKEN-CANARY"] {
            assert!(!rendered.contains(secret), "secret leaked: {rendered}");
        }
    }

    #[test]
    fn insecure_container_full_uri_is_rejected() {
        assert!(validate_container_endpoint("http://example.com/credentials").is_err());
        assert!(validate_container_endpoint("http://127.0.0.1/credentials").is_ok());
        assert!(validate_container_endpoint("https://credentials.example.com/path").is_ok());
    }

    #[test]
    fn local_container_metadata_endpoints_are_classified_for_direct_access() {
        for endpoint in [
            "http://127.0.0.1/credentials",
            "http://localhost/credentials",
            "http://169.254.170.2/credentials",
            "http://169.254.170.23/credentials",
            "https://[::1]/credentials",
        ] {
            let endpoint = Url::parse(endpoint).expect("valid local metadata endpoint");
            assert!(
                is_local_metadata_endpoint(&endpoint),
                "{endpoint} must bypass ambient proxies"
            );
        }
        assert!(!is_local_metadata_endpoint(
            &Url::parse("https://credentials.example.com/path").expect("valid remote endpoint")
        ));
    }

    #[tokio::test]
    async fn imds_uses_the_direct_client_when_the_network_proxy_is_unreachable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/latest/api/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("imds-token"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/latest/meta-data/iam/security-credentials/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("zuno-role"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/latest/meta-data/iam/security-credentials/zuno-role"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "AccessKeyId": "AKID",
                "SecretAccessKey": "SECRET",
                "Token": "TOKEN"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let network_client = zuno_network::client_builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").expect("broken proxy URL"))
            .build()
            .expect("network client");
        let metadata_client =
            zuno_network::direct_client_builder(zuno_network::DirectPurpose::CloudMetadata)
                .build()
                .expect("direct metadata client");
        let resolver = CredentialResolver::with_clients(
            CredentialChainConfig {
                imds_endpoint: Some(Url::parse(&server.uri()).expect("mock IMDS URL")),
                ..CredentialChainConfig::default()
            },
            network_client,
            metadata_client,
        );

        let credentials = resolver
            .resolve_imds()
            .await
            .expect("IMDS resolution")
            .expect("IMDS credentials");
        assert_eq!(credentials.access_key_id, "AKID");
        assert_eq!(credentials.secret_access_key, "SECRET");
        assert_eq!(credentials.session_token.as_deref(), Some("TOKEN"));
    }
}
