use std::io::{IsTerminal as _, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Deserialize;
use url::Url;
use zuno_auth::{
    API_KEY_METHOD, BrowserAuthorization, Credential, LoginMethodKind, LoginMethodRegistry,
    OpenAiOauthClient, Secret,
};
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
        (Some(target), None) => target,
        (None, Some(provider)) => provider,
        (None, None) => {
            return Err(
                "provider is required; run `zuno auth login <provider>` or pass --provider"
                    .to_owned(),
            );
        }
    };
    let document = catalog_document(env, layout)?;
    let provider_id =
        resolve_provider(&document, requested).unwrap_or_else(|| requested.to_owned());
    if !valid_provider_id(&provider_id) {
        return Err(format!("Unknown provider {requested:?}"));
    }

    let default_for_piped_input =
        (method.is_none() && !std::io::stdin().is_terminal()).then_some(API_KEY_METHOD);
    let selected = LoginMethodRegistry::native()
        .resolve(&provider_id, method.or(default_for_piped_input))
        .map_err(|error| error.to_string())?;
    match selected.kind() {
        LoginMethodKind::ApiKey => login_api_key(store, &provider_id),
        LoginMethodKind::OAuthBrowser => login_chatgpt_browser(store, &provider_id),
        LoginMethodKind::OAuthDevice => login_chatgpt_device(store, &provider_id),
    }
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
    fn a_positional_bare_value_is_a_provider_and_a_url_remains_well_known() {
        assert!(!looks_like_url("openai"));
        assert!(looks_like_url("https://provider.example.test"));
        assert!(looks_like_url("http://127.0.0.1:3000"));
    }
}
