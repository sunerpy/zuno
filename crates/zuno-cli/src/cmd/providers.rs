use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Deserialize;
use url::Url;
use zuno_auth::{
    API_KEY_METHOD, BrowserAuthorization, Credential, LoginMethod, LoginMethodKind,
    LoginMethodRegistry, OpenAiOauthClient, Secret,
};
use zuno_llm::catalog::{CatalogDocument, CatalogSource};

use super::terminal_prompt::{self, Choice};
use crate::command::{ProvidersArgs, ProvidersCommand};
use crate::environment::StartupEnvironment;

const OTHER_PROVIDER: &str = "@other";

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
        } => login(
            &store,
            env,
            &layout,
            target.as_deref(),
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

fn methods(
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
    requested: &str,
) -> Result<(), String> {
    let document = catalog_document(env, layout)?;
    let provider_id =
        resolve_provider(&document, requested).unwrap_or_else(|| requested.to_owned());
    if !valid_provider_id(&provider_id) {
        return Err(format!("Unknown provider {requested:?}"));
    }
    println!("Login methods for {provider_id}");
    for method in LoginMethodRegistry::native().methods_for(&provider_id) {
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
) -> Result<(), String> {
    let requested = match (target, provider) {
        (Some(_), Some(_)) => {
            return Err("a positional provider cannot be combined with --provider".to_owned());
        }
        (Some(target), None) if looks_like_url(target) => {
            if method.is_some() {
                return Err("URL login cannot be combined with --method".to_owned());
            }
            return login_well_known(store, target);
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
    let document = if requested.is_none() {
        login_catalog_document(env, layout)?
    } else {
        catalog_document(env, layout)?
    };
    let config = discovered_config(env)?;
    let credentials = store.all().map_err(|error| error.to_string())?.entries;
    let providers = ProviderIndex::new(&document, &config, &credentials);
    let provider_id = match requested {
        Some(requested) => providers
            .resolve(requested)
            .unwrap_or_else(|| requested.to_owned()),
        None => select_provider(&providers)?,
    };
    if !valid_provider_id(&provider_id) {
        return Err(format!("Unknown provider {provider_id:?}"));
    }

    let selected = select_login_method(&provider_id, method)?;
    match selected.kind() {
        LoginMethodKind::ApiKey => login_api_key(store, &provider_id),
        LoginMethodKind::OAuthBrowser => login_chatgpt_browser(store, &provider_id),
        LoginMethodKind::OAuthDevice => login_chatgpt_device(store, &provider_id),
    }
}

fn select_provider(providers: &ProviderIndex) -> Result<String, String> {
    let selected = terminal_prompt::select("Select provider", providers.prompt_choices())?
        .ok_or("provider login cancelled")?;
    if selected != OTHER_PROVIDER {
        return Ok(selected);
    }

    let provider = read_provider_id()?;
    if !providers.contains(&provider) {
        eprintln!(
            "This stores a credential for {provider}; configure that provider before selecting its models."
        );
    }
    Ok(provider)
}

fn select_login_method(provider: &str, requested: Option<&str>) -> Result<LoginMethod, String> {
    let registry = LoginMethodRegistry::native();
    if let Some(requested) = requested {
        return registry
            .resolve(provider, Some(requested))
            .map_err(|error| error.to_string());
    }
    if !std::io::stdin().is_terminal() {
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

fn login_catalog_document(
    env: &zuno_paths::Env,
    layout: &zuno_paths::Layout,
) -> Result<CatalogDocument, String> {
    let source = CatalogSource::resolve(env, layout);
    if let Some(document) = source.load_from_disk().map_err(|error| error.to_string())? {
        return Ok(document);
    }
    match oauth_runtime()?.block_on(source.load()) {
        Ok(loaded) => Ok(loaded.into_document()),
        Err(error) => {
            eprintln!("Unable to refresh the provider catalog: {error}");
            Ok(CatalogDocument::new())
        }
    }
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

impl ProviderIndex {
    fn new(
        document: &CatalogDocument,
        config: &zuno_config::Config,
        credentials: &BTreeMap<String, Credential>,
    ) -> Self {
        let mut choices = document
            .iter()
            .map(|(id, provider)| {
                (
                    id.clone(),
                    ProviderChoice {
                        id: id.clone(),
                        name: provider.name.clone(),
                        configured: false,
                        credential: false,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        choices
            .entry("openai".to_owned())
            .or_insert_with(|| ProviderChoice {
                id: "openai".to_owned(),
                name: "OpenAI".to_owned(),
                configured: false,
                credential: false,
            });

        if let Some(configured) = config.provider.as_ref() {
            for (id, provider) in configured.iter() {
                let choice = choices
                    .entry(id.to_owned())
                    .or_insert_with(|| ProviderChoice {
                        id: id.to_owned(),
                        name: id.to_owned(),
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
            let choice = choices.entry(id.clone()).or_insert_with(|| ProviderChoice {
                id: id.clone(),
                name: id.clone(),
                configured: false,
                credential: false,
            });
            choice.credential = true;
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
        let mut choices = self
            .choices
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
            .collect::<Vec<_>>();
        choices.push(Choice::new(OTHER_PROVIDER, "Other").hinted("enter provider id"));
        choices
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

fn read_provider_id() -> Result<String, String> {
    loop {
        eprint!("Enter provider id: ");
        std::io::stderr()
            .flush()
            .map_err(|error| error.to_string())?;
        let mut value = String::new();
        if std::io::stdin()
            .read_line(&mut value)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Err("provider id entry cancelled".to_owned());
        }
        let value = value.trim().trim_start_matches("@ai-sdk/").to_owned();
        if valid_provider_id(&value) {
            return Ok(value);
        }
        eprintln!("Provider ids may contain only lowercase letters, digits, and hyphens.");
    }
}

fn read_api_key() -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        return read_terminal_secret();
    }
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

fn read_terminal_secret() -> Result<String, String> {
    struct RawModeGuard;

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    eprint!("Enter API key: ");
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
                        return Err("API key entry cancelled".to_owned());
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
        return Err("API key is required".to_owned());
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

fn login_well_known(store: &zuno_auth::AuthStore, raw_url: &str) -> Result<(), String> {
    let url = raw_url.trim_end_matches('/');
    if !(url.starts_with("https://") || url.starts_with("http://127.0.0.1")) {
        return Err("well-known provider login requires HTTPS (or loopback HTTP)".to_owned());
    }
    let runtime = oauth_runtime()?;
    let metadata: WellKnown = runtime.block_on(async {
        zuno_network::client()
            .get(format!("{url}{WELL_KNOWN_PATH}"))
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
    fn interactive_provider_index_includes_native_configured_and_stored_providers() {
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
                "myopenai": {"name": "My OpenAI", "transport": "openai"}
              }
            }"#,
        )
        .expect("config");
        let credentials = BTreeMap::from([("legacy".to_owned(), api_credential())]);

        let providers = ProviderIndex::new(&document, &config, &credentials);
        let ids = providers
            .choices
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["openai", "anthropic", "legacy", "myopenai", "zeta"]
        );
        assert_eq!(providers.resolve("My OpenAI").as_deref(), Some("myopenai"));
        assert!(
            providers
                .choices
                .iter()
                .find(|provider| provider.id == "legacy")
                .is_some_and(|provider| provider.credential)
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

        let providers = ProviderIndex::new(&document, &config, &BTreeMap::new());
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
    fn a_positional_bare_value_is_a_provider_and_a_url_remains_well_known() {
        assert!(!looks_like_url("openai"));
        assert!(looks_like_url("https://provider.example.test"));
        assert!(looks_like_url("http://127.0.0.1:3000"));
    }
}
