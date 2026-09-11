//! Restore a saved Work model only while the native catalog still offers it.
//!
//! A persisted selection is not a new explicit user override. Leaving a missing
//! model unset lets TurnPlan apply its existing saved-model/config fallback.

use zuno_auth::{AuthStore, LoginMethodRegistry};
use zuno_llm::catalog::{Catalog, CatalogSource, ResolveInput};
use zuno_types::execution::TurnExecutionIdentity;

use super::TurnOptions;
use crate::environment::StartupEnvironment;

pub(super) async fn restore_model_hint(
    options: &mut TurnOptions,
    identity: &TurnExecutionIdentity,
    environment: &StartupEnvironment,
) -> Result<(), zuno_acp::RpcError> {
    // A current caller's explicit choice still outranks the saved preference.
    if options.model.is_some() {
        return Ok(());
    }
    let directory = match &options.directory {
        Some(directory) => directory.clone(),
        None => std::env::current_dir()
            .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?,
    };
    let env = environment.resolved();
    let project = zuno_paths::project::resolve_project(&directory);
    let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
    let config = zuno_config::discovery::discover_with(
        &zuno_config::discovery::DiscoveryOptions::new(&directory, worktree, env.clone()),
    )
    .map_err(|error| zuno_acp::RpcError::internal(error.report()))?;
    let layout = zuno_paths::Layout::resolve(env);
    let credentials = AuthStore::resolve(&layout, env)
        .all()
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?
        .entries;
    let loaded = CatalogSource::resolve(env, &layout)
        .load()
        .await
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
    let login_methods = LoginMethodRegistry::native();
    let input = ResolveInput::new()
        .with_config(&config)
        .with_credentials(credentials)
        .with_login_methods(&login_methods)
        .with_env(
            env.iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        )
        .with_experimental_models(env.flag("ZUNO_ENABLE_EXPERIMENTAL_MODELS"));
    let catalog = Catalog::resolve(loaded.document(), &input);
    if catalog
        .model(&identity.provider_id, &identity.model_id)
        .is_some()
    {
        options.model = Some(format!("{}/{}", identity.provider_id, identity.model_id));
        if options.effort.is_none() {
            options.effort = identity
                .reasoning
                .as_deref()
                .and_then(|reasoning| reasoning.parse().ok());
        }
    }
    Ok(())
}
