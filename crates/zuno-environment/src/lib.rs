//! The execution gateway owns its Docker socket and receipt ledger. Command
//! containers receive only a session workspace and bounded execution resources.

mod archive;
mod authority;
mod docker;
mod gateway;
mod ledger;

pub use authority::OrganizationOperationAuthority;
pub use gateway::DockerGateway;

use zuno_application::ApplicationError;

fn storage(error: impl std::error::Error + Send + Sync + 'static) -> ApplicationError {
    ApplicationError::storage(error)
}
