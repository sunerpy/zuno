use std::collections::BTreeMap;

use oc_llm::catalog::resolved::{
    JsonMap, ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel,
};
use oc_llm::catalog::{Catalog, CatalogSource, CatalogStatus, ResolveInput};
use serde::Serialize;

use crate::command::ModelsArgs;
use crate::environment::StartupEnvironment;

const OPENCODE_ENABLE_EXPERIMENTAL_MODELS: &str = "OPENCODE_ENABLE_EXPERIMENTAL_MODELS";

pub(super) fn execute(args: &ModelsArgs, environment: &StartupEnvironment) -> Result<(), String> {
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    let project = oc_paths::project::resolve_project(&directory);
    let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
    let env = environment.resolved();
    let layout = oc_paths::Layout::resolve(env);
    let config = oc_config::discovery::discover_with(&oc_config::discovery::DiscoveryOptions::new(
        &directory,
        worktree,
        env.clone(),
    ))
    .map_err(|error| error.to_string())?;
    let credentials = oc_auth::AuthStore::resolve(&layout, env)
        .all()
        .map_err(|error| error.to_string())?
        .entries;
    let source = CatalogSource::resolve(env, &layout);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;

    let document = runtime.block_on(async {
        if args.refresh {
            source
                .refresh(true)
                .await
                .map_err(|error| error.to_string())?;
            println!("Models cache refreshed");
        }
        source.load().await.map_err(|error| error.to_string())
    })?;
    let input = ResolveInput::new()
        .with_config(&config)
        .with_credentials(credentials)
        .with_env(
            env.iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        )
        .with_experimental_models(env.flag(OPENCODE_ENABLE_EXPERIMENTAL_MODELS));
    let catalog = Catalog::resolve(&document, &input);

    if let Some(provider_id) = args.provider.as_deref() {
        let provider = catalog
            .provider(provider_id)
            .ok_or_else(|| format!("Provider not found: {provider_id}"))?;
        print_models(provider_id, &provider.models, args.verbose)?;
        return Ok(());
    }

    for provider_id in catalog.provider_ids() {
        let provider = catalog
            .provider(provider_id)
            .expect("provider_ids only returns catalog providers");
        print_models(provider_id, &provider.models, args.verbose)?;
    }
    Ok(())
}

fn print_models(
    provider_id: &str,
    models: &BTreeMap<String, ResolvedModel>,
    verbose: bool,
) -> Result<(), String> {
    let mut model_ids: Vec<&str> = models.keys().map(String::as_str).collect();
    model_ids.sort_by(|left, right| oc_llm::catalog::collate::compare(left, right));
    for model_id in model_ids {
        let model = models
            .get(model_id)
            .expect("model id came from the provider model map");
        println!("{provider_id}/{model_id}");
        if verbose {
            println!(
                "{}",
                serde_json::to_string_pretty(&VerboseModel::from(model))
                    .map_err(|error| error.to_string())?
            );
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct VerboseModel<'a> {
    id: &'a str,
    #[serde(rename = "providerID")]
    provider_id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    family: Option<&'a str>,
    api: VerboseApi<'a>,
    status: CatalogStatus,
    headers: &'a BTreeMap<String, String>,
    options: &'a JsonMap,
    cost: &'a ModelCost,
    limit: VerboseLimit,
    capabilities: &'a ModelCapabilities,
    release_date: &'a str,
    variants: &'a BTreeMap<String, JsonMap>,
}

impl<'a> From<&'a ResolvedModel> for VerboseModel<'a> {
    fn from(model: &'a ResolvedModel) -> Self {
        Self {
            id: &model.id,
            provider_id: &model.provider_id,
            name: &model.name,
            family: (!model.family.is_empty()).then_some(model.family.as_str()),
            api: VerboseApi::from(&model.api),
            status: model.status,
            headers: &model.headers,
            options: &model.options,
            cost: &model.cost,
            limit: VerboseLimit::from(model.limit),
            capabilities: &model.capabilities,
            release_date: &model.release_date,
            variants: &model.variants,
        }
    }
}

#[derive(Serialize)]
struct VerboseApi<'a> {
    id: &'a str,
    url: &'a str,
    npm: &'a str,
}

impl<'a> From<&'a ModelApi> for VerboseApi<'a> {
    fn from(api: &'a ModelApi) -> Self {
        Self {
            id: &api.id,
            url: &api.url,
            npm: &api.npm,
        }
    }
}

#[derive(Serialize)]
struct VerboseLimit {
    context: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<f64>,
    output: f64,
}

impl From<ModelLimit> for VerboseLimit {
    fn from(limit: ModelLimit) -> Self {
        Self {
            context: limit.context,
            input: limit.input,
            output: limit.output,
        }
    }
}
