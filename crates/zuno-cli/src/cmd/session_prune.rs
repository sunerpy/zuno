use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal as _, Write as _};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use zuno_db::artifact_gc::ArtifactGcPaths;
use zuno_db::prune::{RemoteUnshare, SharedSession, UnshareError};
use zuno_db::retention::{Liveness, LivenessProbe, RetentionKey};
use zuno_db::session_list::resolve_project;
use zuno_db::session_prune::{
    SessionPruneAction, SessionPruneProgress, SessionPruneRequest, SessionPruneScope,
};
use zuno_db::{Pool, session_prune};

use crate::command::{SessionFormat, SessionPruneArgs, SessionPruneKey};

const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

struct LocalServerProbe {
    urls: Vec<String>,
}

impl LocalServerProbe {
    fn discover() -> Self {
        Self {
            urls: zuno_server::local_server_urls(),
        }
    }

    #[cfg(test)]
    fn from_urls(urls: Vec<String>) -> Self {
        Self { urls }
    }
}

#[derive(Debug, Deserialize)]
struct ActiveResponse {
    data: BTreeMap<String, ActiveState>,
}

#[derive(Debug, Deserialize)]
struct ActiveState {
    #[serde(rename = "type")]
    kind: String,
}

impl LivenessProbe for LocalServerProbe {
    fn probe(&self) -> Liveness {
        if self.urls.is_empty() {
            return Liveness::Unreachable;
        }
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return Liveness::Unreachable;
        };
        runtime.block_on(async {
            tokio::time::timeout(PROBE_TIMEOUT, probe_urls(&self.urls))
                .await
                .unwrap_or(Liveness::Unreachable)
        })
    }
}

async fn probe_urls(urls: &[String]) -> Liveness {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(PROBE_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .build()
    else {
        return Liveness::Unreachable;
    };
    let password = std::env::var("OPENCODE_SERVER_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty());
    let username = std::env::var("OPENCODE_SERVER_USERNAME")
        .unwrap_or_else(|_| zuno_paths::env::DEFAULT_SERVER_USERNAME.to_owned());
    let mut probes = tokio::task::JoinSet::new();
    for base in urls {
        let client = client.clone();
        let url = format!("{}/api/session/active", base.trim_end_matches('/'));
        let password = password.clone();
        let username = username.clone();
        probes.spawn(async move {
            let mut request = client.get(url);
            if let Some(password) = password {
                request = request.basic_auth(username, Some(password));
            }
            let response = request.send().await.ok()?.error_for_status().ok()?;
            let active: ActiveResponse = response.json().await.ok()?;
            active
                .data
                .values()
                .all(|state| state.kind == "running")
                .then_some(active.data.into_keys().collect::<BTreeSet<_>>())
        });
    }

    let mut reached = false;
    let mut active_session_ids = BTreeSet::new();
    while let Some(result) = probes.join_next().await {
        if let Ok(Some(ids)) = result {
            reached = true;
            active_session_ids.extend(ids);
        }
    }
    if reached {
        Liveness::Reachable { active_session_ids }
    } else {
        Liveness::Unreachable
    }
}

struct UnavailableRemote;

impl RemoteUnshare for UnavailableRemote {
    fn unshare(&self, _session: &SharedSession) -> Result<(), UnshareError> {
        Err(UnshareError::new(
            "remote unshare is unavailable from the standalone CLI",
        ))
    }
}

pub(super) fn run(pool: &Pool, args: &SessionPruneArgs) -> Result<(), String> {
    let confirm_delete = confirm_delete(args, std::io::stdin().is_terminal(), || {
        eprint!("Delete the selected sessions and artifacts? [y/N] ");
        std::io::stderr()
            .flush()
            .map_err(|error| error.to_string())?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        Ok(answer)
    })?;
    let now_ms = unix_millis()?;
    let liveness = LocalServerProbe::discover();
    let output = execute(
        pool,
        args,
        confirm_delete,
        now_ms,
        &ArtifactGcPaths::in_layout(zuno_paths::global()),
        &liveness,
    )?;
    std::io::stdout()
        .write_all(&output)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn execute(
    pool: &Pool,
    args: &SessionPruneArgs,
    confirm_delete: bool,
    now_ms: i64,
    paths: &ArtifactGcPaths,
    liveness: &impl LivenessProbe,
) -> Result<Vec<u8>, String> {
    let mut connection = pool.get().map_err(|error| error.to_string())?;
    let request = SessionPruneRequest {
        older_than_days: args.older_than,
        scope: scope(&connection, args)?,
        key: match args.by {
            SessionPruneKey::Updated => RetentionKey::Updated,
            SessionPruneKey::Created => RetentionKey::Created,
        },
        action: if args.delete {
            SessionPruneAction::Delete
        } else if args.archive {
            SessionPruneAction::Archive { at_ms: now_ms }
        } else {
            SessionPruneAction::Preview
        },
        include_shared: args.include_shared,
        include_recent: args.include_recent,
        force: args.force,
        confirm_delete,
        now_ms,
    };
    let report = session_prune::execute(
        &mut connection,
        paths,
        &request,
        liveness,
        &UnavailableRemote,
        &mut |_progress: SessionPruneProgress| {},
    )
    .map_err(|error| error.to_string())?;
    match args.format {
        SessionFormat::Json => {
            session_prune::to_json_bytes(&report).map_err(|error| error.to_string())
        }
        SessionFormat::Table => Ok(render_table(&report).into_bytes()),
    }
}

fn scope(
    connection: &zuno_db::Connection,
    args: &SessionPruneArgs,
) -> Result<SessionPruneScope, String> {
    if args.all_projects {
        return Ok(SessionPruneScope::AllProjects);
    }
    if let Some(needle) = &args.project {
        let canonical = std::fs::canonicalize(needle)
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        for candidate in [Some(needle.clone()), canonical].into_iter().flatten() {
            if let Some(project) =
                resolve_project(connection, &candidate).map_err(|error| error.to_string())?
            {
                return Ok(SessionPruneScope::Project(project.id));
            }
        }
        return Err(format!("Project not found: {needle}"));
    }
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(SessionPruneScope::CurrentProject(
        zuno_paths::project::resolve_project(&directory).id,
    ))
}

fn confirm_delete(
    args: &SessionPruneArgs,
    interactive: bool,
    read_answer: impl FnOnce() -> Result<String, String>,
) -> Result<bool, String> {
    if !args.delete {
        return Ok(false);
    }
    if args.yes {
        return Ok(true);
    }
    if !interactive {
        return Err(
            "--delete requires --yes when stdin is not a TTY; nothing was changed".to_owned(),
        );
    }
    let answer = read_answer()?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(true)
    } else {
        Err("session deletion cancelled; nothing was changed".to_owned())
    }
}

fn unix_millis() -> Result<i64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    i64::try_from(millis).map_err(|error| error.to_string())
}

fn render_table(report: &session_prune::SessionPruneReport) -> String {
    let mut lines = vec![
        format!("Action\t{:?}", report.action),
        format!("Selected sessions\t{}", report.selected_session_ids.len()),
        format!("Database rows\t{}", report.database.total_rows),
        format!("Database bytes\t{}", report.database.total_bytes),
        format!("Artifact bytes\t{}", report.artifacts.total_bytes),
        format!("Cost\t${:.2}", report.database.cost),
    ];
    if !report.selected_session_ids.is_empty() {
        lines.push(String::new());
        lines.push("Session ID".to_owned());
        for session_id in &report.selected_session_ids {
            lines.push(session_id.clone());
        }
    }
    if !report.excluded.is_empty() {
        lines.push(String::new());
        lines.push("Excluded".to_owned());
        for excluded in &report.excluded {
            lines.push(format!(
                "{}\t{}",
                excluded.session_id,
                excluded.reasons.join("; ")
            ));
        }
    }
    for warning in &report.warnings {
        lines.push(format!("Warning\t{warning}"));
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use tower::ServiceExt as _;
    use zuno_paths::DbLocation;
    use zuno_server::api::{self, ApiState};
    use zuno_server::{ServerBuilder, ServerConfig};

    struct Unreachable;

    impl LivenessProbe for Unreachable {
        fn probe(&self) -> Liveness {
            Liveness::Unreachable
        }
    }

    fn args() -> SessionPruneArgs {
        SessionPruneArgs {
            older_than: 90,
            all_projects: true,
            project: None,
            by: SessionPruneKey::Updated,
            archive: false,
            delete: true,
            include_shared: false,
            include_recent: false,
            force: false,
            yes: false,
            format: SessionFormat::Json,
        }
    }

    #[test]
    fn session_prune_non_tty_delete_requires_yes_before_reading_input() {
        let error = confirm_delete(&args(), false, || panic!("must not read non-TTY stdin"))
            .expect_err("headless deletion is refused");
        assert!(error.contains("--yes"));
        assert!(error.contains("nothing was changed"));
    }

    #[test]
    fn session_prune_yes_confirms_without_reading_input() {
        let mut args = args();
        args.yes = true;
        assert!(
            confirm_delete(&args, false, || panic!("--yes must not read stdin"))
                .expect("--yes confirms")
        );
    }

    #[test]
    fn session_prune_interactive_delete_requires_an_affirmative_answer() {
        assert!(confirm_delete(&args(), true, || Ok("yes\n".to_owned())).expect("confirmed"));
        let error = confirm_delete(&args(), true, || Ok("no\n".to_owned()))
            .expect_err("negative answer cancels");
        assert!(error.contains("cancelled"));
    }

    #[tokio::test]
    async fn session_prune_cli_and_http_preview_json_are_byte_identical() {
        let temp = tempfile::tempdir().expect("temporary fixture root");
        let database = DbLocation::File(temp.path().join("opencode.db"));
        let pool = Pool::open(&database).expect("open fixture database");
        {
            let mut connection = pool.get().expect("fixture connection");
            zuno_db::migration::apply(&mut connection).expect("apply fixture schema");
            connection
                .execute(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('global', '/repo', 1, 1, '[]')",
                    [],
                )
                .expect("insert project");
            connection
                .execute(
                    "INSERT INTO session
                       (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES ('ses_old', 'global', 'old', '/repo', 'old', 'test', 1, 1)",
                    [],
                )
                .expect("insert session");
        }
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let mut preview_args = args();
        preview_args.delete = false;
        let cli = execute(
            &pool,
            &preview_args,
            false,
            i64::MAX / 2,
            &paths,
            &Unreachable,
        )
        .expect("CLI preview succeeds");
        let state = ApiState::from_pool(pool, "/repo", paths).expect("create API state");
        let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
            .with_routes(api::router(state))
            .router();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/session/prune?olderThan=90&allProjects=true")
                    .body(Body::empty())
                    .expect("preview request"),
            )
            .await
            .expect("HTTP preview responds");
        let http = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("read HTTP preview");
        assert_eq!(cli.as_slice(), http.as_ref());
    }

    #[test]
    fn session_prune_probes_local_server_and_protects_reported_active_session() {
        let temp = tempfile::tempdir().expect("temporary fixture root");
        let database = DbLocation::File(temp.path().join("opencode.db"));
        let pool = Pool::open(&database).expect("open fixture database");
        {
            let mut connection = pool.get().expect("fixture connection");
            zuno_db::migration::apply(&mut connection).expect("apply fixture schema");
            connection
                .execute(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('global', '/repo', 1, 1, '[]')",
                    [],
                )
                .expect("insert project");
            connection
                .execute(
                    "INSERT INTO session
                       (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES ('ses_active', 'global', 'active', '/repo', 'active', 'test', 1, 1)",
                    [],
                )
                .expect("insert active session");
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let address = listener.local_addr().expect("fake server address");
        let server = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            let (mut stream, _) = listener.accept().expect("accept probe");
            let mut request = [0_u8; 2048];
            let size = stream.read(&mut request).expect("read probe request");
            assert!(
                String::from_utf8_lossy(&request[..size]).starts_with("GET /api/session/active ")
            );
            let body = r#"{"data":{"ses_active":{"type":"running"}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write probe response");
        });
        let probe = LocalServerProbe::from_urls(vec![format!("http://{address}")]);
        let mut preview_args = args();
        preview_args.delete = false;
        let output = execute(
            &pool,
            &preview_args,
            false,
            i64::MAX / 2,
            &ArtifactGcPaths::from_data_root(&temp.path().join("data")),
            &probe,
        )
        .expect("CLI preview succeeds");
        let report: serde_json::Value = serde_json::from_slice(&output).expect("preview JSON");
        assert_eq!(report["selected_session_ids"], serde_json::json!([]));
        assert_eq!(report["excluded"][0]["session_id"], "ses_active");
        assert_eq!(
            report["excluded"][0]["reasons"][0],
            "reported active by a reachable server"
        );
        server.join().expect("fake server exits");
    }
}
