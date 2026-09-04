use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::OptionalExtension;
use serde::Serialize;
use zuno_catalog::lsp_config::ResolvedLsp;
use zuno_catalog::skill::discovery::SkillOptions;
use zuno_config::schema::Config;
use zuno_lsp::{Manager, RestartPolicy, ServerRegistry};
use zuno_search::{GlobRequest, GrepRequest, NeverCancelled, Ripgrep};
use zuno_snapshot::{Location, Store};

use crate::command::{
    CliSandboxMode, DebugAgentArgs, DebugArgs, DebugCommand, DebugLspCommand, DebugPromptArgs,
    DebugRgCommand, DebugSandboxArgs, DebugSandboxNetwork, DebugSnapshotCommand,
};
use crate::environment::StartupEnvironment;

pub(super) fn execute(args: &DebugArgs, environment: &StartupEnvironment) -> Result<(), String> {
    let command = args
        .command
        .as_ref()
        .ok_or("debug subcommand is required")?;
    match command {
        DebugCommand::Paths => paths(environment),
        DebugCommand::Config => {
            let context = Context::resolve(environment)?;
            config(&context)
        }
        DebugCommand::Agent(args) => {
            let context = Context::resolve(environment)?;
            agent(args, &context, environment)
        }
        DebugCommand::Prompt(args) => prompt(args),
        DebugCommand::Permissions => {
            let context = Context::resolve(environment)?;
            permissions(&context)
        }
        DebugCommand::Skill => {
            let context = Context::resolve(environment)?;
            skill(&context)
        }
        DebugCommand::Sandbox(args) => {
            let context = Context::resolve(environment)?;
            sandbox(args, &context)
        }
        DebugCommand::Rg(args) => {
            let context = Context::resolve(environment)?;
            rg(
                args.command
                    .as_ref()
                    .ok_or("debug rg subcommand is required")?,
                &context,
            )
        }
        DebugCommand::Lsp(args) => {
            let context = Context::resolve(environment)?;
            lsp(
                args.command
                    .as_ref()
                    .ok_or("debug lsp subcommand is required")?,
                &context,
            )
        }
        DebugCommand::Snapshot(args) => {
            let context = Context::resolve(environment)?;
            snapshot(
                args.command
                    .as_ref()
                    .ok_or("debug snapshot subcommand is required")?,
                &context,
            )
        }
    }
}

fn config(context: &Context) -> Result<(), String> {
    let agents = zuno_catalog::agent::load_map(
        &context.directory,
        context.worktree.as_deref(),
        &context.env,
    )
    .map_err(to_string)?;
    let commands = zuno_catalog::command::load_map(
        &context.directory,
        context.worktree.as_deref(),
        &context.env,
    )
    .map_err(to_string)?;
    let mut output = serde_json::to_value(&context.config).map_err(to_string)?;
    let object = output
        .as_object_mut()
        .ok_or("resolved config did not serialize as an object")?;
    object.insert(
        "agent".to_owned(),
        serde_json::to_value(agents.agents).map_err(to_string)?,
    );
    object.insert(
        "command".to_owned(),
        serde_json::to_value(commands).map_err(to_string)?,
    );
    normalize_json_numbers(&mut output);
    print_json(&output)
}

fn normalize_json_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_json_numbers(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                normalize_json_numbers(value);
            }
        }
        serde_json::Value::Number(number) if number.is_f64() => {
            let Some(value) = number.as_f64() else {
                return;
            };
            const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
            if value.fract() != 0.0 || value.abs() > MAX_SAFE_INTEGER {
                return;
            }
            if value >= 0.0 {
                *number = serde_json::Number::from(value as u64);
            } else {
                *number = serde_json::Number::from(value as i64);
            }
        }
        _ => {}
    }
}

fn permissions(context: &Context) -> Result<(), String> {
    let rules = context
        .config
        .permission
        .as_ref()
        .map(|permission| permission.rules.clone())
        .unwrap_or_default();
    print_json(&serde_json::json!({
        "configuredMode": context.config.permission_mode(),
        "mode": context.config.effective_permission_mode(),
        "rules": rules,
        "strictSideEffectsRequireApproval": context.config.strict_authorization(),
        "allowAllStillEnforces": [
            "explicit deny",
            "catastrophic shell denial",
            "sandbox authority",
            "argument validation"
        ],
    }))
}

fn prompt(args: &DebugPromptArgs) -> Result<(), String> {
    let pool = zuno_db::Pool::open_default().map_err(to_string)?;
    let mut connection = pool.get().map_err(to_string)?;
    zuno_db::migration::apply(&mut connection).map_err(to_string)?;
    let output = prompt_output(&connection, args)?;
    print_json(&output)
}

#[derive(Debug)]
struct ProviderPromptReceipt {
    event_id: String,
    sequence: i64,
    step: u32,
    prompt_receipt_id: String,
    estimated_prompt_tokens: Option<u64>,
}

fn prompt_output(
    connection: &rusqlite::Connection,
    args: &DebugPromptArgs,
) -> Result<serde_json::Value, String> {
    if args.session_id.is_none() && args.step.is_some() {
        return Err("`--step` requires `--session <id>`".to_owned());
    }

    let (row, provider_request) = match args.session_id.as_deref() {
        Some(session_id) => {
            let provider_request = provider_prompt_receipt(connection, session_id, args.step)?;
            let row = connection
                .query_row(
                    "SELECT id, aggregate_id, seq, data FROM event \
                     WHERE id = ?1 AND aggregate_id = ?2 \
                     AND type = 'session.prompt.assembled.1' LIMIT 1",
                    rusqlite::params![provider_request.prompt_receipt_id, session_id],
                    prompt_row,
                )
                .optional()
                .map_err(to_string)?
                .ok_or_else(|| {
                    format!(
                        "provider request `{}` references missing prompt receipt `{}` in session `{session_id}`",
                        provider_request.event_id, provider_request.prompt_receipt_id
                    )
                })?;
            (row, Some(provider_request))
        }
        None => {
            let row = connection
                .query_row(
                    "SELECT id, aggregate_id, seq, data FROM event \
                     WHERE type = 'session.prompt.assembled.1' \
                     ORDER BY rowid DESC LIMIT 1",
                    [],
                    prompt_row,
                )
                .optional()
                .map_err(to_string)?
                .ok_or_else(|| "no prompt receipt found in the database".to_owned())?;
            (row, None)
        }
    };

    let (event_id, session_id, sequence, data) = row;
    let mut properties: serde_json::Value = serde_json::from_str(&data).map_err(to_string)?;
    if !args.show_sensitive {
        redact_prompt_content(&mut properties);
    }
    let mut output = serde_json::Map::from_iter([
        ("eventId".to_owned(), serde_json::Value::String(event_id)),
        (
            "eventType".to_owned(),
            serde_json::Value::String("session.prompt.assembled.1".to_owned()),
        ),
        (
            "sessionId".to_owned(),
            serde_json::Value::String(session_id),
        ),
        (
            "sequence".to_owned(),
            serde_json::Value::Number(sequence.into()),
        ),
        ("properties".to_owned(), properties),
    ]);
    if let Some(provider_request) = provider_request {
        let mut request = serde_json::Map::from_iter([
            (
                "eventId".to_owned(),
                serde_json::Value::String(provider_request.event_id),
            ),
            (
                "sequence".to_owned(),
                serde_json::Value::Number(provider_request.sequence.into()),
            ),
            (
                "step".to_owned(),
                serde_json::Value::from(provider_request.step),
            ),
            (
                "promptReceiptID".to_owned(),
                serde_json::Value::String(provider_request.prompt_receipt_id),
            ),
        ]);
        if let Some(estimated_prompt_tokens) = provider_request.estimated_prompt_tokens {
            request.insert(
                "estimatedPromptTokens".to_owned(),
                serde_json::Value::from(estimated_prompt_tokens),
            );
        }
        output.insert(
            "providerRequest".to_owned(),
            serde_json::Value::Object(request),
        );
    }
    Ok(serde_json::Value::Object(output))
}

fn provider_prompt_receipt(
    connection: &rusqlite::Connection,
    session_id: &str,
    step: Option<std::num::NonZeroU32>,
) -> Result<ProviderPromptReceipt, String> {
    let row = match step {
        Some(step) => connection
            .query_row(
                "SELECT id, seq, \
                    json_extract(data, '$.step'), \
                    json_extract(data, '$.promptReceiptID'), \
                    json_extract(data, '$.estimatedPromptTokens') \
                 FROM event \
                 WHERE type = 'session.provider.request.1' \
                   AND aggregate_id = ?1 \
                   AND json_extract(data, '$.status') = 'started' \
                   AND CAST(json_extract(data, '$.step') AS INTEGER) = ?2 \
                   AND json_extract(data, '$.promptReceiptID') IS NOT NULL \
                 ORDER BY seq DESC LIMIT 1",
                rusqlite::params![session_id, i64::from(step.get())],
                provider_prompt_receipt_row,
            )
            .optional(),
        None => connection
            .query_row(
                "SELECT id, seq, \
                    json_extract(data, '$.step'), \
                    json_extract(data, '$.promptReceiptID'), \
                    json_extract(data, '$.estimatedPromptTokens') \
                 FROM event \
                 WHERE type = 'session.provider.request.1' \
                   AND aggregate_id = ?1 \
                   AND json_extract(data, '$.status') = 'started' \
                   AND json_extract(data, '$.promptReceiptID') IS NOT NULL \
                 ORDER BY seq DESC LIMIT 1",
                [session_id],
                provider_prompt_receipt_row,
            )
            .optional(),
    }
    .map_err(to_string)?;

    row.ok_or_else(|| match step {
        Some(step) => format!(
            "no provider request with a prompt receipt found for session `{session_id}` at step {}",
            step.get()
        ),
        None => {
            format!("no provider request with a prompt receipt found for session `{session_id}`")
        }
    })
}

fn provider_prompt_receipt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderPromptReceipt> {
    let step = row.get::<_, i64>(2)?;
    let estimated_prompt_tokens = row.get::<_, Option<i64>>(4)?;
    Ok(ProviderPromptReceipt {
        event_id: row.get(0)?,
        sequence: row.get(1)?,
        step: u32::try_from(step).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        prompt_receipt_id: row.get(3)?,
        estimated_prompt_tokens: estimated_prompt_tokens
            .map(u64::try_from)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?,
    })
}

fn prompt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, i64, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn redact_prompt_content(value: &mut serde_json::Value) {
    let Some(properties) = value.as_object_mut() else {
        return;
    };
    if let Some(sections) = properties
        .get_mut("sections")
        .and_then(serde_json::Value::as_array_mut)
    {
        for section in sections {
            if let Some(section) = section.as_object_mut() {
                section.insert("content".to_owned(), serde_json::json!("<redacted>"));
            }
        }
    }
    if properties.contains_key("actualSystemPrompt") {
        properties.insert(
            "actualSystemPrompt".to_owned(),
            serde_json::json!("<redacted>"),
        );
    }
    for key in ["providerProjection", "actualProviderProjection"] {
        if let Some(projection) = properties.get_mut(key) {
            redact_provider_projection(projection);
        }
    }
}

fn redact_provider_projection(value: &mut serde_json::Value) {
    let Some(projection) = value.as_object_mut() else {
        return;
    };
    for lane in ["system", "developer"] {
        let Some(messages) = projection
            .get_mut(lane)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for message in messages {
            *message = serde_json::json!("<redacted>");
        }
    }
}

struct Context {
    directory: PathBuf,
    worktree: Option<PathBuf>,
    env: zuno_paths::Env,
    config: Config,
}

impl Context {
    fn resolve(environment: &StartupEnvironment) -> Result<Self, String> {
        let directory = std::env::current_dir().map_err(to_string)?;
        let project = zuno_paths::project::resolve_project(&directory);
        let worktree = project.vcs.as_ref().map(|_| project.directory.clone());
        let env = environment.resolved().clone();
        let config =
            zuno_config::discovery::discover_with(&zuno_config::discovery::DiscoveryOptions::new(
                &directory,
                worktree.as_deref(),
                env.clone(),
            ))
            .map_err(|error| error.report())?;
        Ok(Self {
            directory,
            worktree,
            env,
            config,
        })
    }
}

fn paths(environment: &StartupEnvironment) -> Result<(), String> {
    let layout = zuno_paths::Layout::resolve(environment.resolved());
    print!("{}", layout.debug_paths_dump());
    Ok(())
}

fn agent(
    args: &DebugAgentArgs,
    context: &Context,
    environment: &StartupEnvironment,
) -> Result<(), String> {
    let runtime = runtime()?;
    let plan = runtime.block_on(super::turn::TurnPlan::resolve(
        &super::turn::TurnOptions {
            directory: Some(context.directory.clone()),
            agent: Some(args.name.clone()),
            ..super::turn::TurnOptions::default()
        },
        environment,
    ))?;
    let mcp_workspace = plan.runtime_workspace().to_owned();
    let mcp = runtime.block_on(async {
        let runtime = super::mcp_runtime::McpRuntime::from_config(plan.config(), &mcp_workspace)?;
        let warnings = runtime.connect().await;
        let mut diagnostics = runtime.diagnostics(warnings);
        diagnostics.cleanup_warnings = runtime.shutdown_with_diagnostics().await;
        Some(diagnostics)
    });
    print_json(&plan.debug_agent_snapshot_with_mcp(mcp.as_ref()))
}

fn skill(context: &Context) -> Result<(), String> {
    let options = SkillOptions::from_config(
        &context.directory,
        context.worktree.as_deref(),
        &context.env,
        &context.config,
    );
    let runtime = runtime()?;
    let skills = runtime.block_on(zuno_catalog::skill::load(&options));
    let described = skills
        .all()
        .iter()
        .filter(|skill| skill.catalog_description().is_some())
        .count();
    let mut names = std::collections::BTreeMap::<&str, usize>::new();
    for entry in skills.all() {
        *names.entry(&entry.name).or_default() += 1;
    }
    let warnings = skills
        .warnings()
        .iter()
        .map(|warning| {
            serde_json::json!({
                "source": warning.source(),
                "message": warning.to_string(),
            })
        })
        .collect::<Vec<_>>();
    let output = serde_json::json!({
        "view": {
            "kind": "raw_discovery",
            "agentFiltered": false,
            "extensionOverlayApplied": false,
            "effectiveViewCommand": "zuno debug agent <name>",
        },
        "summary": {
            "sourceCount": skills.all().len(),
            "describedSourceCount": described,
            "indexedSourceCount": skills.indexed_count(),
            "searchableSourceCount": skills.searchable_count(),
            "explicitSourceCount": skills.explicit_count(),
            "disabledSourceCount": skills.disabled_sources().len(),
            "uniqueNameCount": names.len(),
            "promptMetadataEnabled": context
                .config
                .skills
                .as_ref()
                .and_then(|settings| settings.include_instructions)
                != Some(false),
            "warningCount": warnings.len(),
            "ambiguousNames": names
                .into_iter()
                .filter_map(|(name, sources)| (sources > 1).then_some(serde_json::json!({
                    "name": name,
                    "sources": sources,
                })))
                .collect::<Vec<_>>(),
        },
        "disabledSources": skills.disabled_sources(),
        "warnings": warnings,
        "skills": skills.all(),
    });
    print_json(&output)
}

fn sandbox(args: &DebugSandboxArgs, context: &Context) -> Result<(), String> {
    let mode = match args.mode {
        CliSandboxMode::ReadOnly => zuno_sandbox::SandboxMode::ReadOnly,
        CliSandboxMode::WorkspaceWrite => zuno_sandbox::SandboxMode::WorkspaceWrite,
        CliSandboxMode::DangerFullAccess => zuno_sandbox::SandboxMode::DangerFullAccess,
    };
    let network = sandbox_network(mode, args.network);
    let report = zuno_sandbox::deployment_report_with_request(
        &context.directory,
        mode,
        network,
        super::tool_runtime::sandbox_backend_request(&context.config),
    );
    print_json(&report)?;
    if args.check && !report.ready {
        return Err(report
            .error
            .unwrap_or_else(|| "requested sandbox policy is not deployable".to_owned()));
    }
    Ok(())
}

fn sandbox_network(
    mode: zuno_sandbox::SandboxMode,
    requested: Option<DebugSandboxNetwork>,
) -> zuno_sandbox::NetworkAccess {
    match requested {
        Some(DebugSandboxNetwork::Deny) => zuno_sandbox::NetworkAccess::Denied,
        Some(DebugSandboxNetwork::Allow) => zuno_sandbox::NetworkAccess::Allowed,
        None if mode == zuno_sandbox::SandboxMode::DangerFullAccess => {
            zuno_sandbox::NetworkAccess::Allowed
        }
        None => zuno_sandbox::NetworkAccess::Denied,
    }
}

fn rg(command: &DebugRgCommand, context: &Context) -> Result<(), String> {
    let ripgrep = Ripgrep::discover().map_err(to_string)?;
    match command {
        DebugRgCommand::Files {
            query: _,
            glob,
            limit,
        } => {
            let request = GlobRequest::new(
                &context.directory,
                glob.as_deref().unwrap_or("**/*"),
                limit.unwrap_or(10_000),
            );
            let result = ripgrep.glob(&request, &NeverCancelled).map_err(to_string)?;
            for entry in result.items {
                println!("{}", entry.path);
            }
            Ok(())
        }
        DebugRgCommand::Search {
            pattern,
            glob,
            limit,
        } => {
            let mut request =
                GrepRequest::new(&context.directory, pattern, limit.unwrap_or(10_000));
            if let Some(include) = combined_glob(glob) {
                request = request.with_include(Some(include));
            }
            let result = ripgrep.grep(&request, &NeverCancelled).map_err(to_string)?;
            print_json(&result.items)
        }
    }
}

fn combined_glob(globs: &[String]) -> Option<String> {
    match globs {
        [] => None,
        [only] => Some(only.clone()),
        many => Some(format!("{{{}}}", many.join(","))),
    }
}

fn snapshot(command: &DebugSnapshotCommand, context: &Context) -> Result<(), String> {
    let store = Store::open(Location::discover(&context.directory));
    match command {
        DebugSnapshotCommand::Track => {
            if let Some(hash) = store.track().map_err(to_string)? {
                println!("{hash}");
            }
            Ok(())
        }
        DebugSnapshotCommand::Patch { hash } => {
            let patch = store.patch(hash).map_err(to_string)?;
            print_json(&patch)
        }
        DebugSnapshotCommand::Diff { hash } => {
            let diff = store.diff(hash).map_err(to_string)?;
            println!("{diff}");
            Ok(())
        }
    }
}

fn lsp(command: &DebugLspCommand, context: &Context) -> Result<(), String> {
    let resolved = ResolvedLsp::resolve(context.config.lsp.as_ref());
    let registry = Arc::new(ServerRegistry::offline(&resolved));
    let manager = Manager::new(
        &context.directory,
        registry,
        RestartPolicy::default(),
        std::num::NonZeroUsize::new(usize::from(
            context.config.resolved_concurrency().lsp_requests,
        ))
        .expect("configuration validates LSP concurrency"),
    );
    let runtime = runtime()?;
    runtime.block_on(async {
        let result: Result<serde_json::Value, String> = match command {
            DebugLspCommand::Diagnostics { file } => {
                let path = resolve_path(&context.directory, file)?;
                manager
                    .diagnostics(&path)
                    .await
                    .map_err(to_string)
                    .and_then(|value| serde_json::to_value(value).map_err(to_string))
            }
            DebugLspCommand::Symbols { query } => manager
                .workspace_symbols(query)
                .await
                .map_err(to_string)
                .and_then(|value| serde_json::to_value(value).map_err(to_string)),
            DebugLspCommand::DocumentSymbols { uri } => {
                let path = resolve_path(&context.directory, uri)?;
                manager
                    .document_symbols(&path)
                    .await
                    .map_err(to_string)
                    .and_then(|value| serde_json::to_value(value).map_err(to_string))
            }
        };
        manager.shutdown().await;
        let value = result.map_err(to_string)?;
        print_json(&value)
    })
}

fn resolve_path(directory: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.starts_with("file://") {
        return reqwest::Url::parse(raw)
            .map_err(to_string)?
            .to_file_path()
            .map_err(|()| format!("invalid file URI: {raw}"));
    }
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

fn print_json(value: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(to_string)?
    );
    Ok(())
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
    fn multiple_search_globs_become_one_brace_expression() {
        assert_eq!(combined_glob(&[]), None);
        assert_eq!(combined_glob(&["*.rs".to_owned()]).as_deref(), Some("*.rs"));
        assert_eq!(
            combined_glob(&["*.rs".to_owned(), "*.toml".to_owned()]).as_deref(),
            Some("{*.rs,*.toml}")
        );
    }

    #[test]
    fn sandbox_network_defaults_match_the_execution_mode() {
        assert_eq!(
            sandbox_network(zuno_sandbox::SandboxMode::WorkspaceWrite, None),
            zuno_sandbox::NetworkAccess::Denied
        );
        assert_eq!(
            sandbox_network(zuno_sandbox::SandboxMode::ReadOnly, None),
            zuno_sandbox::NetworkAccess::Denied
        );
        assert_eq!(
            sandbox_network(zuno_sandbox::SandboxMode::DangerFullAccess, None),
            zuno_sandbox::NetworkAccess::Allowed
        );
        assert_eq!(
            sandbox_network(
                zuno_sandbox::SandboxMode::DangerFullAccess,
                Some(DebugSandboxNetwork::Deny),
            ),
            zuno_sandbox::NetworkAccess::Denied
        );
    }

    #[test]
    fn prompt_debug_redacts_model_visible_bodies_but_keeps_provenance() {
        let mut receipt = serde_json::json!({
            "schemaVersion": 3,
            "assemblySha256": "assembly-digest",
            "actualSha256": "actual-digest",
            "actualSystemPrompt": "hook-transformed secret",
            "providerProjection": {
                "system": ["base secret"],
                "developer": ["runtime secret", "project secret"]
            },
            "actualProviderProjection": {
                "system": ["hook-transformed secret"],
                "developer": ["runtime secret", "hook context secret"]
            },
            "sections": [{
                "id": "instructions.project.0",
                "role": "project_instructions",
                "source": "/repo/AGENTS.md",
                "sha256": "section-digest",
                "bytes": 12,
                "content": "private rule"
            }]
        });

        redact_prompt_content(&mut receipt);

        assert_eq!(receipt["sections"][0]["content"], "<redacted>");
        assert_eq!(receipt["actualSystemPrompt"], "<redacted>");
        assert_eq!(
            receipt["providerProjection"]["system"],
            serde_json::json!(["<redacted>"])
        );
        assert_eq!(
            receipt["providerProjection"]["developer"],
            serde_json::json!(["<redacted>", "<redacted>"])
        );
        assert_eq!(
            receipt["actualProviderProjection"]["system"],
            serde_json::json!(["<redacted>"])
        );
        assert_eq!(
            receipt["actualProviderProjection"]["developer"],
            serde_json::json!(["<redacted>", "<redacted>"])
        );
        assert_eq!(receipt["sections"][0]["source"], "/repo/AGENTS.md");
        assert_eq!(receipt["sections"][0]["sha256"], "section-digest");
        assert_eq!(receipt["assemblySha256"], "assembly-digest");
    }

    #[test]
    fn prompt_debug_resolves_session_step_through_the_provider_receipt_id() {
        let connection = prompt_event_connection();
        insert_prompt_event(
            &connection,
            "evt_prompt_matching_step_but_wrong",
            "ses_example",
            1,
            serde_json::json!({
                "schemaVersion": 3,
                "step": 7,
                "sections": [{
                    "id": "wrong",
                    "source": "wrong",
                    "sha256": "wrong",
                    "estimatedTokens": 1,
                    "content": "wrong"
                }],
                "providerProjection": {"system": ["wrong"], "developer": []}
            }),
        );
        insert_prompt_event(
            &connection,
            "evt_prompt_referenced",
            "ses_example",
            2,
            serde_json::json!({
                "schemaVersion": 3,
                "step": 99,
                "sections": [{
                    "id": "runtime.intent",
                    "source": "zuno-runtime:runtime.intent",
                    "sha256": "right-digest",
                    "estimatedTokens": 11,
                    "content": "right"
                }],
                "providerProjection": {"system": ["right"], "developer": []}
            }),
        );
        insert_event(
            &connection,
            "evt_provider",
            "ses_example",
            3,
            "session.provider.request.1",
            serde_json::json!({
                "step": 7,
                "status": "started",
                "promptReceiptID": "evt_prompt_referenced",
                "estimatedPromptTokens": 42
            }),
        );

        let output = prompt_output(
            &connection,
            &DebugPromptArgs {
                session_id: Some("ses_example".to_owned()),
                step: std::num::NonZeroU32::new(7),
                show_sensitive: true,
            },
        )
        .expect("provider-linked prompt receipt");

        assert_eq!(output["eventId"], "evt_prompt_referenced");
        assert_eq!(output["properties"]["sections"][0]["id"], "runtime.intent");
        assert_eq!(
            output["properties"]["sections"][0]["source"],
            "zuno-runtime:runtime.intent"
        );
        assert_eq!(
            output["properties"]["sections"][0]["sha256"],
            "right-digest"
        );
        assert_eq!(output["properties"]["sections"][0]["estimatedTokens"], 11);
        assert_eq!(
            output["properties"]["providerProjection"]["system"],
            serde_json::json!(["right"])
        );
        assert_eq!(output["providerRequest"]["eventId"], "evt_provider");
        assert_eq!(output["providerRequest"]["estimatedPromptTokens"], 42);
    }

    #[test]
    fn prompt_debug_without_a_session_selects_the_database_latest_receipt() {
        let connection = prompt_event_connection();
        insert_prompt_event(
            &connection,
            "evt_prompt_older",
            "ses_a",
            1,
            serde_json::json!({
                "schemaVersion": 3,
                "sections": [],
                "providerProjection": {"system": ["older"], "developer": []}
            }),
        );
        insert_event(
            &connection,
            "evt_unrelated",
            "ses_a",
            2,
            "session.provider.request.1",
            serde_json::json!({
                "step": 1,
                "status": "started",
                "promptReceiptID": "evt_prompt_older"
            }),
        );
        insert_prompt_event(
            &connection,
            "evt_prompt_latest",
            "ses_b",
            1,
            serde_json::json!({
                "schemaVersion": 3,
                "sections": [],
                "providerProjection": {"system": ["latest"], "developer": []}
            }),
        );

        let output = prompt_output(
            &connection,
            &DebugPromptArgs {
                session_id: None,
                step: None,
                show_sensitive: true,
            },
        )
        .expect("database latest prompt receipt");

        assert_eq!(output["eventId"], "evt_prompt_latest");
        assert!(output.get("providerRequest").is_none());
    }

    #[test]
    fn prompt_debug_does_not_invent_a_post_hook_body_when_none_was_stored() {
        let mut receipt = serde_json::json!({
            "sections": [{"content": "private rule"}],
            "hookTransformed": false
        });

        redact_prompt_content(&mut receipt);

        assert_eq!(receipt["sections"][0]["content"], "<redacted>");
        assert!(receipt.get("actualSystemPrompt").is_none());
    }

    #[test]
    fn paths_accept_file_uris_and_workspace_relative_values() {
        let workspace = tempfile::tempdir().expect("workspace");
        let relative = workspace.path().join("src").join("lib.rs");
        assert_eq!(
            resolve_path(workspace.path(), "src/lib.rs").expect("relative path"),
            relative
        );
        let absolute = workspace.path().join("absolute.rs");
        let uri = url::Url::from_file_path(&absolute).expect("absolute file URI");
        assert_eq!(
            resolve_path(workspace.path(), uri.as_str()).expect("file URI"),
            absolute
        );
    }

    fn prompt_event_connection() -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_in_memory().expect("in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE event (
                    id TEXT PRIMARY KEY,
                    aggregate_id TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    type TEXT NOT NULL,
                    data TEXT NOT NULL
                );",
            )
            .expect("event table");
        connection
    }

    fn insert_prompt_event(
        connection: &rusqlite::Connection,
        id: &str,
        session_id: &str,
        sequence: i64,
        data: serde_json::Value,
    ) {
        insert_event(
            connection,
            id,
            session_id,
            sequence,
            "session.prompt.assembled.1",
            data,
        );
    }

    fn insert_event(
        connection: &rusqlite::Connection,
        id: &str,
        session_id: &str,
        sequence: i64,
        event_type: &str,
        data: serde_json::Value,
    ) {
        connection
            .execute(
                "INSERT INTO event (id, aggregate_id, seq, type, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, session_id, sequence, event_type, data.to_string()],
            )
            .expect("insert event");
    }
}
