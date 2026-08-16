use std::io::{IsTerminal as _, Read as _};

use serde::Deserialize;
use zuno_auth::{Credential, Secret};
use zuno_llm::catalog::{CatalogDocument, CatalogSource};

use crate::command::{ProvidersArgs, ProvidersCommand};
use crate::environment::StartupEnvironment;

pub(super) fn execute(
    args: &ProvidersArgs,
    environment: &StartupEnvironment,
) -> Result<(), String> {
    let command = args
        .command
        .as_ref()
        .ok_or("providers subcommand is required")?;
    let env = environment.resolved();
    let layout = zuno_paths::Layout::resolve(env);
    let store = zuno_auth::AuthStore::resolve(&layout, env);

    match command {
        ProvidersCommand::List => list(&store, env, &layout),
        ProvidersCommand::Login {
            url,
            provider,
            method,
        } => login(
            &store,
            env,
            &layout,
            url.as_deref(),
            provider.as_deref(),
            method.as_deref(),
        ),
        ProvidersCommand::Logout { provider } => logout(&store, env, &layout, provider.as_deref()),
    }
}

fn list(
    store: &zuno_auth::AuthStore,
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
) -> Result<(), String> {
    let credentials = store.all().map_err(|error| error.to_string())?;
    let document = catalog_document(env, layout)?;

    println!("Credentials {}", display_path(store.path(), env));
    for (provider_id, credential) in &credentials.entries {
        println!(
            "{} {}",
            provider_name(&document, provider_id),
            credential.kind()
        );
    }
    println!(
        "{} credential{}",
        credentials.len(),
        if credentials.len() == 1 { "" } else { "s" }
    );

    let mut active = Vec::new();
    for (provider_id, provider) in &document {
        for variable in &provider.env {
            if env.truthy_value(variable).is_some() {
                active.push((provider_name(&document, provider_id), variable.as_str()));
            }
        }
    }
    if !active.is_empty() {
        println!();
        println!("Environment");
        for (provider, variable) in &active {
            println!("{provider} {variable}");
        }
        println!(
            "{} environment variable{}",
            active.len(),
            if active.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn login(
    store: &zuno_auth::AuthStore,
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
    url: Option<&str>,
    provider: Option<&str>,
    method: Option<&str>,
) -> Result<(), String> {
    if let Some(url) = url {
        if method.is_some() || provider.is_some() {
            return Err("URL login cannot be combined with --provider or --method".to_owned());
        }
        return login_well_known(store, url);
    }
    if let Some(method) = method
        && !method.eq_ignore_ascii_case("api")
        && !method.eq_ignore_ascii_case("api key")
    {
        return Err(format!(
            "login method {method:?} requires a provider plugin, which is unavailable in pure headless mode"
        ));
    }

    let requested = provider.ok_or(
        "provider selection is interactive upstream; pass --provider <id-or-name> for headless login",
    )?;
    let document = catalog_document(env, layout)?;
    let provider_id =
        resolve_provider(&document, requested).unwrap_or_else(|| requested.to_owned());
    if !valid_provider_id(&provider_id) {
        return Err(format!("Unknown provider {requested:?}"));
    }

    if std::io::stdin().is_terminal() {
        eprint!("Enter your API key: ");
    }
    let key = read_stdin_secret()?;
    store
        .set(
            &provider_id,
            Credential::Api {
                key: Secret::new(key),
                metadata: None,
            },
        )
        .map_err(|error| error.to_string())?;
    println!("Done");
    Ok(())
}

fn logout(
    store: &zuno_auth::AuthStore,
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
    provider: Option<&str>,
) -> Result<(), String> {
    let credentials = store.all().map_err(|error| error.to_string())?;
    if credentials.is_empty() {
        return Err("No credentials found".to_owned());
    }
    let requested = provider.ok_or(
        "provider selection is interactive upstream; pass the provider id or name for headless logout",
    )?;
    let document = catalog_document(env, layout)?;
    let provider_id = credentials
        .entries
        .keys()
        .find(|id| {
            id.as_str() == requested || provider_name(&document, id).eq_ignore_ascii_case(requested)
        })
        .cloned()
        .ok_or_else(|| format!("Unknown configured provider {requested:?}"))?;
    store
        .remove(&provider_id)
        .map_err(|error| error.to_string())?;
    println!("Logout successful");
    Ok(())
}

fn catalog_document(
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
) -> Result<CatalogDocument, String> {
    CatalogSource::resolve(env, layout)
        .load_from_disk()
        .map(|document| document.unwrap_or_default())
        .map_err(|error| error.to_string())
}

fn provider_name(document: &CatalogDocument, provider_id: &str) -> String {
    document
        .get(provider_id)
        .map_or_else(|| provider_id.to_owned(), |provider| provider.name.clone())
}

fn resolve_provider(document: &CatalogDocument, requested: &str) -> Option<String> {
    document
        .iter()
        .find(|(id, provider)| {
            id.as_str() == requested || provider.name.eq_ignore_ascii_case(requested)
        })
        .map(|(id, _)| id.clone())
}

fn valid_provider_id(provider: &str) -> bool {
    !provider.is_empty()
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn read_stdin_secret() -> Result<String, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| error.to_string())?;
    let value = input.trim().to_owned();
    if value.is_empty() {
        return Err("API key is required".to_owned());
    }
    Ok(value)
}

fn display_path(path: &std::path::Path, env: &zuno_paths::Env) -> String {
    if let Some(home) = env.truthy_value(zuno_paths::env::HOME)
        && let Ok(relative) = path.strip_prefix(home)
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

#[derive(Debug, Deserialize)]
struct WellKnown {
    auth: WellKnownAuth,
}

#[derive(Debug, Deserialize)]
struct WellKnownAuth {
    command: Vec<String>,
    env: String,
}

fn login_well_known(store: &zuno_auth::AuthStore, raw_url: &str) -> Result<(), String> {
    let url = raw_url.trim_end_matches('/');
    if !(url.starts_with("https://") || url.starts_with("http://127.0.0.1")) {
        return Err("well-known provider login requires HTTPS (or loopback HTTP)".to_owned());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let metadata: WellKnown = runtime.block_on(async {
        reqwest::get(format!("{url}/.well-known/opencode"))
            .await
            .map_err(|error| format!("Failed to load auth provider metadata from {url}: {error}"))?
            .error_for_status()
            .map_err(|error| format!("Failed to load auth provider metadata from {url}: {error}"))?
            .json()
            .await
            .map_err(|error| format!("Failed to load auth provider metadata from {url}: {error}"))
    })?;
    let (program, arguments) = metadata
        .auth
        .command
        .split_first()
        .ok_or("auth provider metadata returned an empty command")?;
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("Failed to run auth provider command: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "auth provider command exited with {}",
            output.status
        ));
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|error| format!("auth provider command returned non-UTF-8 output: {error}"))?
        .trim()
        .to_owned();
    if token.is_empty() {
        return Err("auth provider command returned an empty token".to_owned());
    }
    store
        .set(
            url,
            Credential::WellKnown {
                key: Secret::new(metadata.auth.env),
                token: Secret::new(token),
            },
        )
        .map_err(|error| error.to_string())?;
    println!("Logged into {url}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_deliberately_narrow() {
        assert!(valid_provider_id("openai"));
        assert!(valid_provider_id("amazon-bedrock"));
        assert!(!valid_provider_id("OpenAI"));
        assert!(!valid_provider_id("../openai"));
        assert!(!valid_provider_id(""));
    }

    #[test]
    fn provider_name_matches_case_insensitively() {
        let document: CatalogDocument = serde_json::from_str(
            r#"{"openai":{"id":"openai","name":"OpenAI","env":[],"models":{}}}"#,
        )
        .expect("catalog");
        assert_eq!(
            resolve_provider(&document, "openai").as_deref(),
            Some("openai")
        );
        assert_eq!(
            resolve_provider(&document, "OPENAI").as_deref(),
            Some("openai")
        );
    }
}
