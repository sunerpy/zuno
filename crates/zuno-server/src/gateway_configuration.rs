//! Host-owned mapping from an immutable Job configuration to its gateway.

use std::collections::BTreeSet;

use zuno_application::{ApplicationError, environment::EnvironmentSpec, runtime::ConfigurationRef};
use zuno_types::identity::{EnvironmentId, GatewayId, SessionId, TenantId};

pub use zuno_application::environment::wire::GatewayAssignment;

/// Deployment configuration, not a client DTO. Multiple snapshots may remain
/// installed during a rolling upgrade; a Job must match its exact snapshot.
pub struct GatewayDeployment {
    pub tenant: TenantId,
    pub configuration: ConfigurationRef,
    pub gateway_id: GatewayId,
    pub endpoint: url::Url,
    pub image: String,
    pub memory_bytes: u64,
    pub pids_limit: u32,
    pub cpu_millis: u32,
}

pub trait GatewayConfigurationResolver: Send + Sync {
    fn resolve(
        &self,
        tenant: &TenantId,
        configuration: &ConfigurationRef,
        session: &SessionId,
    ) -> Result<GatewayAssignment, ApplicationError>;
}

pub struct ConfiguredGateways {
    deployments: Vec<GatewayDeployment>,
}

impl ConfiguredGateways {
    pub fn new(deployments: Vec<GatewayDeployment>) -> Result<Self, ApplicationError> {
        if deployments.is_empty() || deployments.len() > 128 {
            return Err(ApplicationError::Invalid(
                "configure 1–128 immutable gateway deployments".to_owned(),
            ));
        }
        let mut seen = BTreeSet::new();
        for entry in &deployments {
            let endpoint = &entry.endpoint;
            if endpoint.scheme() != "https"
                || endpoint.host_str().is_none()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
                || !endpoint.path().ends_with('/')
                || entry.configuration.version == 0
                || entry.configuration.sha256.len() != 64
                || !entry
                    .configuration
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || !seen.insert((
                    entry.tenant.clone(),
                    entry.configuration.id.clone(),
                    entry.configuration.version,
                ))
            {
                return Err(ApplicationError::Invalid(
                    "invalid or ambiguous gateway deployment".to_owned(),
                ));
            }
            entry
                .environment(&SessionId::new("validation").expect("fixed identity"))?
                .validate()?;
        }
        Ok(Self { deployments })
    }
}

impl GatewayDeployment {
    fn environment(&self, session: &SessionId) -> Result<EnvironmentSpec, ApplicationError> {
        Ok(EnvironmentSpec {
            // The distinct identifier types share an opaque value. Docker names
            // still bind the owner plus this environment ID through a digest.
            id: EnvironmentId::new(session.as_str()).map_err(ApplicationError::storage)?,
            session_id: session.clone(),
            image: self.image.clone(),
            memory_bytes: self.memory_bytes,
            pids_limit: self.pids_limit,
            cpu_millis: self.cpu_millis,
        })
    }
}

impl GatewayConfigurationResolver for ConfiguredGateways {
    fn resolve(
        &self,
        tenant: &TenantId,
        configuration: &ConfigurationRef,
        session: &SessionId,
    ) -> Result<GatewayAssignment, ApplicationError> {
        let deployment = self
            .deployments
            .iter()
            .find(|entry| entry.tenant == *tenant && entry.configuration == *configuration)
            .ok_or_else(|| {
                ApplicationError::Invalid(
                    "the Job's gateway configuration is not installed".to_owned(),
                )
            })?;
        Ok(GatewayAssignment {
            gateway_id: deployment.gateway_id.clone(),
            endpoint: deployment.endpoint.to_string(),
            environment: deployment.environment(session)?,
        })
    }
}
