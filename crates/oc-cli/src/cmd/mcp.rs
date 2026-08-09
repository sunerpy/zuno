use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use oc_auth::{McpAuthStore, McpCredentials};
use oc_config::schema::Config;
use oc_config::schema::mcp::{McpOauth, McpRemote, McpServerConfig};
use oc_mcp::{RemoteClient, RemoteConnect};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use crate::command::{McpAddArgs, McpArgs, McpAuthCommand, McpCommand};
use crate::environment::StartupEnvironment;

const OAUTH_TIMEOUT: Duration = Duration::from_secs(180);

pub(super) fn execute(args: &McpArgs, environment: &StartupEnvironment) -> Result<(), String> {
    if let Some(McpCommand::Add(args)) = args.command.as_ref() {
        let layout = oc_paths::Layout::resolve(environment.resolved());
        return add(args, &layout);
    }
    let context = Context::resolve(environment)?;
    match args.command.as_ref().ok_or("mcp subcommand is required")? {
        McpCommand::Add(_) => unreachable!("mcp add is handled before config discovery"),
        McpCommand::List => list(&context),
        McpCommand::Auth(args) => match args.command.as_ref() {
            Some(McpAuthCommand::List) => auth_list(&context),
            None => authenticate(
                args.name
                    .as_deref()
                    .ok_or("MCP server name is required in non-interactive mode")?,
                &context,
            ),
        },
        McpCommand::Logout { name } => logout(name.as_deref(), &context),
        McpCommand::Debug { name } => debug(name, &context),
    }
}

struct Context {
    config: Config,
    auth: McpAuthStore,
}

impl Context {
    fn resolve(environment: &StartupEnvironment) -> Result<Self, String> {
        let directory = std::env::current_dir().map_err(|error| error.to_string())?;
        let project = oc_paths::project::resolve_project(&directory);
        let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
        let env = environment.resolved();
        let layout = oc_paths::Layout::resolve(env);
        let config = oc_config::discovery::discover_with(
            &oc_config::discovery::DiscoveryOptions::new(&directory, worktree, env.clone()),
        )
        .map_err(|error| error.report())?;
        let auth = McpAuthStore::resolve(&layout);
        Ok(Self { config, auth })
    }
}

fn list(context: &Context) -> Result<(), String> {
    println!("MCP Servers");
    let Some(servers) = context.config.mcp.as_ref() else {
        println!("No MCP servers configured");
        println!("Add servers with: opencode-rust mcp add");
        return Ok(());
    };
    if servers.is_empty() {
        println!("No MCP servers configured");
        println!("Add servers with: opencode-rust mcp add");
        return Ok(());
    }

    for (name, server) in servers {
        let (status, detail) = match server {
            McpServerConfig::Local(local) => (
                if local.enabled == Some(false) {
                    "disabled"
                } else {
                    "not initialized"
                },
                local.command.join(" "),
            ),
            McpServerConfig::Remote(remote) => (
                if remote.enabled == Some(false) {
                    "disabled"
                } else {
                    "not initialized"
                },
                remote.url.clone(),
            ),
            McpServerConfig::Toggle(toggle) => (
                if toggle.enabled {
                    "not initialized"
                } else {
                    "disabled"
                },
                "configuration override".to_owned(),
            ),
        };
        println!("{name} {status}");
        println!("    {detail}");
    }
    println!(
        "{} server{}",
        servers.len(),
        if servers.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

fn auth_list(context: &Context) -> Result<(), String> {
    println!("MCP OAuth Status");
    let credentials = context.auth.all().map_err(|error| error.to_string())?;
    let oauth_servers = oauth_servers(&context.config);
    if oauth_servers.is_empty() {
        println!("No OAuth-capable MCP servers configured");
        println!("Done");
        return Ok(());
    }

    for (name, remote) in &oauth_servers {
        let status = credentials
            .entries
            .get(*name)
            .filter(|entry| entry.server_url.as_deref() == Some(remote.url.as_str()))
            .and_then(|entry| entry.tokens.as_ref())
            .map_or("not authenticated", |tokens| {
                if tokens
                    .expires_at
                    .is_some_and(|expiry| expiry <= now_millis())
                {
                    "expired"
                } else {
                    "authenticated"
                }
            });
        println!("{name} {status}");
        println!("    {}", remote.url);
    }
    println!(
        "{} OAuth-capable server{}",
        oauth_servers.len(),
        if oauth_servers.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

fn logout(name: Option<&str>, context: &Context) -> Result<(), String> {
    println!("MCP OAuth Logout");
    let credentials = context.auth.all().map_err(|error| error.to_string())?;
    if credentials.entries.is_empty() {
        println!("No MCP OAuth credentials stored");
        println!("Done");
        return Ok(());
    }
    if !credentials.skipped.is_empty() {
        return Err(format!(
            "refusing to update {} because entries could not be decoded: {}",
            context.auth.path().display(),
            credentials.skipped.join(", ")
        ));
    }
    let name = name.ok_or_else(|| {
        format!(
            "MCP server name is required in non-interactive mode; available credentials: {}",
            credentials
                .entries
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if !credentials.entries.contains_key(name) {
        println!("No credentials found for: {name}");
        println!("Done");
        return Ok(());
    }
    context
        .auth
        .remove(name)
        .map_err(|error| error.to_string())?;
    println!("Removed OAuth credentials for {name}");
    println!("Done");
    Ok(())
}

fn add(args: &McpAddArgs, layout: &oc_paths::Layout) -> Result<(), String> {
    let has_options = args.url.is_some()
        || !args.env.is_empty()
        || !args.header.is_empty()
        || !args.server_command.is_empty();
    let name = args.name.as_deref().ok_or_else(|| {
        if has_options {
            "A server name is required for non-interactive MCP configuration"
        } else {
            "MCP server name and either --url or a command after -- are required in non-interactive mode"
        }
        .to_owned()
    })?;
    if args.url.is_some() != args.server_command.is_empty() {
        return Err("Provide either --url <url> or a command after --".to_owned());
    }

    let value = if let Some(url) = &args.url {
        reqwest::Url::parse(url).map_err(|_| format!("Invalid URL: {url}"))?;
        if !args.env.is_empty() {
            return Err("--env is only valid for local MCP servers".to_owned());
        }
        let headers = parse_assignments(&args.header, "header")?;
        let mut server = Map::from_iter([
            ("type".to_owned(), Value::String("remote".to_owned())),
            ("url".to_owned(), Value::String(url.clone())),
        ]);
        if !headers.is_empty() {
            server.insert(
                "headers".to_owned(),
                serde_json::to_value(headers).map_err(to_string)?,
            );
        }
        Value::Object(server)
    } else {
        if !args.header.is_empty() {
            return Err("--header is only valid for remote MCP servers".to_owned());
        }
        let environment = parse_assignments(&args.env, "environment variable")?;
        let mut server = Map::from_iter([
            ("type".to_owned(), Value::String("local".to_owned())),
            (
                "command".to_owned(),
                serde_json::to_value(&args.server_command).map_err(to_string)?,
            ),
        ]);
        if !environment.is_empty() {
            server.insert(
                "environment".to_owned(),
                serde_json::to_value(environment).map_err(to_string)?,
            );
        }
        Value::Object(server)
    };

    let path = writable_config_path(layout.config());
    update_json_config(&path, name, value)?;
    println!("MCP server {name:?} added to {}", path.display());
    Ok(())
}

fn debug(name: &str, context: &Context) -> Result<(), String> {
    let remote = remote_server(&context.config, name)?;
    println!("MCP OAuth Debug");
    println!("Server: {name}");
    println!("URL: {}", remote.url);
    print_safe_credentials(context.auth.all().map_err(|error| error.to_string())?, name);

    let runtime = runtime()?;
    runtime.block_on(async {
        match RemoteClient::connect_with_store(name.to_owned(), remote, context.auth.clone()).await
        {
            Ok(RemoteConnect::Connected(client)) => {
                println!("Connection: connected ({:?})", client.transport());
                match client.list_tools().await {
                    Ok(tools) => println!("Tools: {}", tools.len()),
                    Err(error) => println!("Tools: unavailable ({error})"),
                }
                client.close().await;
                Ok(())
            }
            Ok(RemoteConnect::AuthorizationRequired(request)) => {
                println!("Connection: authentication required");
                println!("Authorization URL: {}", request.authorization_url());
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        }
    })
}

fn authenticate(name: &str, context: &Context) -> Result<(), String> {
    let remote = remote_server(&context.config, name)?;
    println!("Starting OAuth flow...");
    let runtime = runtime()?;
    runtime.block_on(async {
        match RemoteClient::connect_with_store(name.to_owned(), remote, context.auth.clone()).await
        {
            Ok(RemoteConnect::Connected(client)) => {
                println!("{name} is already authenticated");
                client.close().await;
                Ok(())
            }
            Ok(RemoteConnect::AuthorizationRequired(request)) => {
                let authorization_url = request.authorization_url().to_owned();
                let redirect = redirect_uri(&authorization_url)?;
                let listener = bind_callback(&redirect).await?;
                println!("Authorize in your browser:");
                println!("{authorization_url}");
                println!("Waiting for authorization...");
                let (code, state) = wait_for_callback(listener, &redirect).await?;
                match request.finish(&code, &state).await.map_err(to_string)? {
                    RemoteConnect::Connected(client) => {
                        println!("Authentication successful!");
                        client.close().await;
                        Ok(())
                    }
                    RemoteConnect::AuthorizationRequired(_) => {
                        Err("OAuth provider requested authorization again".to_owned())
                    }
                }
            }
            Err(error) => Err(error.to_string()),
        }
    })
}

fn remote_server<'a>(config: &'a Config, name: &str) -> Result<&'a McpRemote, String> {
    let server = config
        .mcp
        .as_ref()
        .and_then(|servers| servers.get(name))
        .ok_or_else(|| format!("MCP server not found: {name}"))?;
    let McpServerConfig::Remote(remote) = server else {
        return Err(format!("MCP server {name} is not a remote server"));
    };
    if matches!(remote.oauth, Some(McpOauth::Disabled(_))) {
        return Err(format!("MCP server {name} has OAuth explicitly disabled"));
    }
    Ok(remote)
}

fn oauth_servers(config: &Config) -> Vec<(&str, &McpRemote)> {
    config
        .mcp
        .iter()
        .flat_map(|servers| servers.iter())
        .filter_map(|(name, server)| match server {
            McpServerConfig::Remote(remote)
                if !matches!(remote.oauth, Some(McpOauth::Disabled(_))) =>
            {
                Some((name, remote))
            }
            _ => None,
        })
        .collect()
}

fn print_safe_credentials(credentials: McpCredentials, name: &str) {
    let Some(entry) = credentials.entries.get(name) else {
        println!("Credentials: not stored");
        return;
    };
    println!("Credentials: stored");
    println!("Access token: {}", presence(entry.tokens.as_ref()));
    println!(
        "Refresh token: {}",
        presence(
            entry
                .tokens
                .as_ref()
                .and_then(|tokens| tokens.refresh_token.as_ref())
        )
    );
    println!(
        "Client registration: {}",
        presence(entry.client_info.as_ref())
    );
    println!("PKCE verifier: {}", presence(entry.code_verifier.as_ref()));
    println!("OAuth state: {}", presence(entry.oauth_state.as_ref()));
}

fn presence<T>(value: Option<&T>) -> &'static str {
    if value.is_some() { "present" } else { "absent" }
}

fn parse_assignments(values: &[String], kind: &str) -> Result<BTreeMap<String, String>, String> {
    values
        .iter()
        .map(|value| {
            let (key, value) = value
                .split_once('=')
                .filter(|(key, _)| !key.is_empty())
                .ok_or_else(|| format!("Invalid {kind}: {value}. Expected KEY=VALUE"))?;
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn writable_config_path(directory: &Path) -> PathBuf {
    let json = directory.join("opencode.json");
    if json.exists() {
        return json;
    }
    let jsonc = directory.join("opencode.jsonc");
    if jsonc.exists() {
        return jsonc;
    }
    json
}

fn update_json_config(path: &Path, name: &str, server: Value) -> Result<(), String> {
    if path
        .extension()
        .is_some_and(|extension| extension == "jsonc")
    {
        return Err(format!(
            "cannot safely update {}: this build has no comment-preserving JSONC editor; add the MCP entry manually or provide an opencode.json target",
            path.display()
        ));
    }
    let mut root = if path.exists() {
        serde_json::from_slice::<Value>(&fs::read(path).map_err(to_string)?).map_err(to_string)?
    } else {
        json!({"$schema": "https://opencode.ai/config.json"})
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
    let mcp = object
        .entry("mcp")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| format!("the mcp field in {} must be an object", path.display()))?;
    mcp.insert(name.to_owned(), server);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(to_string)?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&root).map_err(to_string)?;
    let result = fs::write(&temp, bytes)
        .and_then(|()| fs::rename(&temp, path))
        .map_err(to_string);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn redirect_uri(authorization_url: &str) -> Result<reqwest::Url, String> {
    let authorization = reqwest::Url::parse(authorization_url).map_err(to_string)?;
    let redirect = authorization
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value.into_owned())
        .ok_or("authorization URL does not contain a redirect_uri")?;
    let redirect = reqwest::Url::parse(&redirect).map_err(to_string)?;
    if redirect.scheme() != "http"
        || !matches!(
            redirect.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        )
    {
        return Err("OAuth callback must use a loopback HTTP redirect URI".to_owned());
    }
    Ok(redirect)
}

async fn bind_callback(redirect: &reqwest::Url) -> Result<TcpListener, String> {
    let host = match redirect.host_str() {
        Some("localhost" | "127.0.0.1") => "127.0.0.1",
        Some("[::1]") => "[::1]",
        _ => return Err("OAuth callback host is not loopback".to_owned()),
    };
    let port = redirect
        .port_or_known_default()
        .ok_or("OAuth callback redirect URI has no port")?;
    TcpListener::bind((host, port))
        .await
        .map_err(|error| format!("failed to bind OAuth callback on {host}:{port}: {error}"))
}

async fn wait_for_callback(
    listener: TcpListener,
    redirect: &reqwest::Url,
) -> Result<(String, String), String> {
    let (mut stream, _) = tokio::time::timeout(OAUTH_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "timed out waiting for OAuth callback".to_owned())?
        .map_err(to_string)?;
    let callback = read_callback_url(&mut stream, redirect).await;
    let success = callback.is_ok();
    write_callback_response(&mut stream, success).await?;
    callback
}

async fn read_callback_url(
    stream: &mut TcpStream,
    redirect: &reqwest::Url,
) -> Result<(String, String), String> {
    let mut bytes = vec![0_u8; 16 * 1024];
    let count = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut bytes))
        .await
        .map_err(|_| "timed out reading OAuth callback".to_owned())?
        .map_err(to_string)?;
    let request = std::str::from_utf8(&bytes[..count]).map_err(to_string)?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or("invalid OAuth callback request")?;
    let callback = redirect.join(target).map_err(to_string)?;
    if callback.path() != redirect.path() {
        return Err("OAuth callback path did not match the configured redirect URI".to_owned());
    }
    let code = callback
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .ok_or("OAuth callback did not contain a code")?;
    let state = callback
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("OAuth callback did not contain state")?;
    Ok((code, state))
}

async fn write_callback_response(stream: &mut TcpStream, success: bool) -> Result<(), String> {
    let body = if success {
        "Authentication complete. You can close this window."
    } else {
        "Authentication failed. Return to the terminal for details."
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        if success { "200 OK" } else { "400 Bad Request" },
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(to_string)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(i64::MAX, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(to_string)
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignments_split_only_the_first_equals_sign() {
        assert_eq!(
            parse_assignments(&["TOKEN=a=b".to_owned()], "header").expect("assignment"),
            BTreeMap::from([("TOKEN".to_owned(), "a=b".to_owned())])
        );
        assert!(parse_assignments(&["broken".to_owned()], "header").is_err());
        assert!(parse_assignments(&["=broken".to_owned()], "header").is_err());
    }

    #[test]
    fn json_update_preserves_unrelated_fields() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("opencode.json");
        fs::write(&path, r#"{"theme":"dark","mcp":{"old":{"enabled":false}}}"#)
            .expect("seed config");
        update_json_config(
            &path,
            "new",
            json!({"type":"remote","url":"https://example.com/mcp"}),
        )
        .expect("update config");
        let value: Value =
            serde_json::from_slice(&fs::read(path).expect("read config")).expect("JSON");
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mcp"]["old"]["enabled"], false);
        assert_eq!(value["mcp"]["new"]["type"], "remote");
    }

    #[test]
    fn existing_jsonc_is_never_rewritten() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("opencode.jsonc");
        let original = b"{ // keep me\n}\n";
        fs::write(&path, original).expect("seed JSONC");
        let error = update_json_config(&path, "new", json!({"enabled":false}))
            .expect_err("JSONC must be rejected");
        assert!(error.contains("comment-preserving JSONC editor"));
        assert_eq!(fs::read(path).expect("read JSONC"), original);
    }
}
