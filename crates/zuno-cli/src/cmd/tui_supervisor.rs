//! Persistent local supervisor and retained-TUI attachment.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crossterm::terminal;
use reqwest::{Client, Method, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use url::form_urlencoded;
use uuid::Uuid;
use zuno_pty::{ConnectToken, CreateInput, PtyInfo, PtyStatus, TerminalSize, UpdateInput};

use crate::command::TuiArgs;

const STATE_SCHEMA_VERSION: u32 = 1;
const START_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const RESIZE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
const MAX_SERVER_FRAME_BYTES: u64 = 4 * 1024 * 1024;
const DETACH_BYTE: u8 = 0x1d; // Ctrl+]
const SUPERVISOR_USERNAME: &str = "zuno-supervisor";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SupervisorState {
    schema_version: u32,
    instance_id: String,
    pid: u32,
    address: String,
    username: String,
    password: String,
    directory: String,
    time_started: i64,
}

#[derive(Debug, Deserialize)]
struct DataEnvelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct ConnectTokenEnvelope {
    data: ConnectToken,
}

pub(super) struct SupervisorStateGuard {
    path: PathBuf,
    instance_id: String,
}

impl Drop for SupervisorStateGuard {
    fn drop(&mut self) {
        let remove = read_state(&self.path)
            .ok()
            .flatten()
            .is_some_and(|state| state.instance_id == self.instance_id);
        if remove {
            let _removed = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn publish_server_state(
    path: &Path,
    address: SocketAddr,
    directory: &str,
) -> Result<SupervisorStateGuard, String> {
    let password = std::env::var("ZUNO_SERVER_PASSWORD")
        .map_err(|_| "supervisor server is missing ZUNO_SERVER_PASSWORD".to_owned())?;
    if password.is_empty() {
        return Err("supervisor server password cannot be empty".to_owned());
    }
    let username =
        std::env::var("ZUNO_SERVER_USERNAME").unwrap_or_else(|_| SUPERVISOR_USERNAME.to_owned());
    let instance_id = Uuid::new_v4().simple().to_string();
    let state = SupervisorState {
        schema_version: STATE_SCHEMA_VERSION,
        instance_id: instance_id.clone(),
        pid: std::process::id(),
        address: address.to_string(),
        username,
        password,
        directory: directory.to_owned(),
        time_started: zuno_db::message::now_millis(),
    };
    write_private_state(path, &state)?;
    Ok(SupervisorStateGuard {
        path: path.to_owned(),
        instance_id,
    })
}

pub(super) fn execute(args: &TuiArgs) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(to_string)?;
    runtime.block_on(async {
        if args.background_list {
            return match running_supervisor().await? {
                Some(state) => list(&SupervisorApi::new(state)?).await,
                None => {
                    println!("background TUI supervisor is not running");
                    Ok(())
                }
            };
        }
        if args.background_shutdown {
            return shutdown_supervisor().await;
        }
        if let Some(pty_id) = args.background_stop.as_deref() {
            let state = running_supervisor()
                .await?
                .ok_or_else(|| "background TUI supervisor is not running".to_owned())?;
            let client = SupervisorApi::new(state)?;
            client.remove(pty_id).await?;
            println!("stopped background TUI {pty_id}");
            return Ok(());
        }
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err("attaching a background TUI requires an interactive terminal".to_owned());
        }
        let state = match args.attach.as_deref() {
            Some(_) => running_supervisor()
                .await?
                .ok_or_else(|| "background TUI supervisor is not running".to_owned())?,
            None => ensure_supervisor().await?,
        };
        let client = SupervisorApi::new(state)?;
        let pty_id = match args.attach.as_deref() {
            Some(pty_id) => pty_id.to_owned(),
            None => client.create_tui(args).await?.id.to_string(),
        };
        eprintln!("attaching {pty_id}; press Ctrl+] to detach");
        client.attach(&pty_id).await?;
        eprintln!("detached from {pty_id}; reattach with `zuno tui --attach {pty_id}`");
        Ok(())
    })
}

struct SupervisorApi {
    state: SupervisorState,
    http: Client,
}

impl SupervisorApi {
    fn new(state: SupervisorState) -> Result<Self, String> {
        let http =
            zuno_network::direct_client_builder(zuno_network::DirectPurpose::LoopbackControlPlane)
                .timeout(HTTP_TIMEOUT)
                .build()
                .map_err(to_string)?;
        Ok(Self { state, http })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.state.address, path)
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .basic_auth(&self.state.username, Some(&self.state.password))
    }

    async fn list(&self) -> Result<Vec<PtyInfo>, String> {
        decode(
            self.request(Method::GET, "/api/pty")
                .send()
                .await
                .map_err(to_string)?,
        )
        .await
    }

    async fn create_tui(&self, args: &TuiArgs) -> Result<PtyInfo, String> {
        let executable = std::env::current_exe().map_err(to_string)?;
        let cwd = std::env::current_dir().map_err(to_string)?;
        let size = current_size();
        let input = CreateInput {
            command: Some(executable.to_string_lossy().into_owned()),
            args: Some(nested_tui_args(args)),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            title: Some(background_title(args)),
            env: Some(std::collections::HashMap::from([(
                "ZUNO_SUPERVISED_TUI".to_owned(),
                "1".to_owned(),
            )])),
            size: Some(size),
        };
        decode(
            self.request(Method::POST, "/api/pty")
                .json(&input)
                .send()
                .await
                .map_err(to_string)?,
        )
        .await
    }

    async fn remove(&self, pty_id: &str) -> Result<(), String> {
        ensure_success(
            self.request(Method::DELETE, &format!("/api/pty/{pty_id}"))
                .send()
                .await
                .map_err(to_string)?,
        )
        .await?;
        Ok(())
    }

    async fn resize(&self, pty_id: &str, size: TerminalSize) -> Result<(), String> {
        let _: PtyInfo = decode(
            self.request(Method::PUT, &format!("/api/pty/{pty_id}"))
                .json(&UpdateInput {
                    title: None,
                    size: Some(size),
                })
                .send()
                .await
                .map_err(to_string)?,
        )
        .await?;
        Ok(())
    }

    async fn ticket(&self, pty_id: &str) -> Result<String, String> {
        let response = self
            .request(Method::POST, &format!("/api/pty/{pty_id}/connect-token"))
            .header("x-zuno-ticket", "1")
            .query(&[("location[directory]", self.state.directory.as_str())])
            .send()
            .await
            .map_err(to_string)?;
        let response: ConnectTokenEnvelope = decode_direct(response).await?;
        Ok(response.data.ticket)
    }

    async fn attach(&self, pty_id: &str) -> Result<(), String> {
        self.resize(pty_id, current_size()).await?;
        let ticket = self.ticket(pty_id).await?;
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("ticket", &ticket);
        query.append_pair("location[directory]", &self.state.directory);
        let path = format!("/api/pty/{pty_id}/connect?{}", query.finish());
        let stream = tokio::net::TcpStream::connect(&self.state.address)
            .await
            .map_err(to_string)?;
        let stream = websocket_upgrade(stream, &self.state, &path).await?;
        let _raw = RawModeGuard::enter()?;
        drive_socket(stream, self, pty_id).await
    }
}

async fn list(client: &SupervisorApi) -> Result<(), String> {
    let sessions = client.list().await?;
    if sessions.is_empty() {
        println!("no retained background TUI sessions");
        return Ok(());
    }
    println!("{:<38} {:<8} {:<8} TITLE", "ID", "STATUS", "PID");
    for session in sessions {
        println!(
            "{:<38} {:<8} {:<8} {}",
            session.id,
            match session.status {
                PtyStatus::Running => "running",
                PtyStatus::Exited => "exited",
            },
            session.pid,
            session.title
        );
    }
    Ok(())
}

async fn ensure_supervisor() -> Result<SupervisorState, String> {
    if let Some(state) = running_supervisor().await? {
        return Ok(state);
    }
    let path = state_path();
    start_supervisor(&path).await
}

async fn running_supervisor() -> Result<Option<SupervisorState>, String> {
    let path = state_path();
    let Some(state) = read_state(&path)? else {
        return Ok(None);
    };
    if !process_alive(state.pid) {
        let _removed = fs::remove_file(path);
        return Ok(None);
    }
    let client = SupervisorApi::new(state.clone())?;
    client.list().await.map_err(|error| {
        format!(
            "supervisor process {} is alive but its API is unavailable: {error}",
            state.pid
        )
    })?;
    Ok(Some(state))
}

async fn shutdown_supervisor() -> Result<(), String> {
    let Some(state) = running_supervisor().await? else {
        println!("background TUI supervisor is not running");
        return Ok(());
    };
    terminate_process(state.pid)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while process_alive(state.pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if process_alive(state.pid) {
        return Err(format!(
            "background TUI supervisor process {} did not stop",
            state.pid
        ));
    }
    let _removed = fs::remove_file(state_path());
    println!("stopped background TUI supervisor");
    Ok(())
}

async fn start_supervisor(path: &Path) -> Result<SupervisorState, String> {
    let directory = path
        .parent()
        .ok_or_else(|| "supervisor state path has no parent".to_owned())?;
    fs::create_dir_all(directory).map_err(to_string)?;
    set_private_directory(directory)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("server.log"))
        .map_err(to_string)?;
    let password = Uuid::new_v4().simple().to_string();
    let executable = std::env::current_exe().map_err(to_string)?;
    let mut command = detached_server_command(&executable);
    command
        .args([
            OsString::from("serve"),
            OsString::from("--hostname"),
            OsString::from("127.0.0.1"),
            OsString::from("--port"),
            OsString::from("0"),
            OsString::from("--supervisor-state"),
            path.as_os_str().to_owned(),
        ])
        .env("ZUNO_SERVER_USERNAME", SUPERVISOR_USERNAME)
        .env("ZUNO_SERVER_PASSWORD", password)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(to_string)?))
        .stderr(Stdio::from(log));
    let mut child = command.spawn().map_err(to_string)?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(to_string)? {
            return Err(format!(
                "background TUI supervisor exited during startup with {status}"
            ));
        }
        if let Some(state) = read_state(path)?
            && state.pid == child.id()
        {
            let client = SupervisorApi::new(state.clone())?;
            if client.list().await.is_ok() {
                return Ok(state);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "background TUI supervisor did not become ready; inspect `{}`",
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("server.log")
                    .display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn nested_tui_args(args: &TuiArgs) -> Vec<String> {
    let mut nested = vec!["tui".to_owned()];
    if let Some(prompt) = &args.prompt {
        nested.extend(["--prompt".to_owned(), prompt.clone()]);
    }
    if let Some(model) = &args.model {
        nested.extend(["--model".to_owned(), model.clone()]);
    }
    if let Some(agent) = &args.agent {
        nested.extend(["--agent".to_owned(), agent.clone()]);
    }
    if args.r#continue {
        nested.push("--continue".to_owned());
    }
    if let Some(session) = &args.session {
        nested.extend(["--session".to_owned(), session.clone()]);
    }
    if args.auto {
        nested.push("--auto".to_owned());
    }
    nested
}

fn background_title(args: &TuiArgs) -> String {
    args.session.as_ref().map_or_else(
        || "Zuno TUI".to_owned(),
        |session| format!("Zuno {session}"),
    )
}

fn current_size() -> TerminalSize {
    terminal::size().map_or_else(
        |_| TerminalSize::default(),
        |(cols, rows)| TerminalSize {
            rows: rows.max(1),
            cols: cols.max(1),
        },
    )
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(to_string)?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _restored = terminal::disable_raw_mode();
    }
}

async fn drive_socket(
    stream: tokio::net::TcpStream,
    client: &SupervisorApi,
    pty_id: &str,
) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut input = [0_u8; 8192];
    let mut resize = tokio::time::interval(RESIZE_INTERVAL);
    resize.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut size = current_size();
    loop {
        tokio::select! {
            read = stdin.read(&mut input) => {
                let read = read.map_err(to_string)?;
                if read == 0 {
                    write_client_frame(&mut writer, 8, &1000_u16.to_be_bytes()).await.map_err(to_string)?;
                    return Ok(());
                }
                let bytes = &input[..read];
                if let Some(detach) = bytes.iter().position(|byte| *byte == DETACH_BYTE) {
                    if detach > 0 {
                        write_client_frame(&mut writer, 2, &bytes[..detach]).await.map_err(to_string)?;
                    }
                    write_client_frame(&mut writer, 8, &1000_u16.to_be_bytes()).await.map_err(to_string)?;
                    return Ok(());
                }
                write_client_frame(&mut writer, 2, bytes).await.map_err(to_string)?;
            }
            frame = read_server_frame(&mut reader) => {
                let frame = frame.map_err(to_string)?;
                match frame.opcode {
                    1 | 2 => {
                        if !is_meta_frame(&frame.payload) {
                            stdout.write_all(&frame.payload).await.map_err(to_string)?;
                            stdout.flush().await.map_err(to_string)?;
                        }
                    }
                    8 => return Ok(()),
                    9 => write_client_frame(&mut writer, 10, &frame.payload).await.map_err(to_string)?,
                    10 => {}
                    _ => return Err(format!("supervisor sent unsupported WebSocket opcode {}", frame.opcode)),
                }
            }
            _ = resize.tick() => {
                let next = current_size();
                if next != size {
                    size = next;
                    let _resized = client.resize(pty_id, size).await;
                }
            }
        }
    }
}

struct ServerFrame {
    opcode: u8,
    payload: Vec<u8>,
}

async fn websocket_upgrade(
    mut stream: tokio::net::TcpStream,
    state: &SupervisorState,
    path: &str,
) -> Result<tokio::net::TcpStream, String> {
    let key = STANDARD.encode(Uuid::new_v4().as_bytes());
    let authorization = STANDARD.encode(format!("{}:{}", state.username, state.password));
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\
         Authorization: Basic {authorization}\r\n\r\n",
        state.address
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(to_string)?;
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() >= MAX_HANDSHAKE_BYTES {
            return Err("WebSocket upgrade response exceeded its header limit".to_owned());
        }
        let byte = stream.read_u8().await.map_err(to_string)?;
        response.push(byte);
    }
    let response = String::from_utf8(response).map_err(to_string)?;
    let status = response.lines().next().unwrap_or_default();
    if !status.contains(" 101 ") {
        return Err(format!("WebSocket upgrade failed: {status}"));
    }
    Ok(stream)
}

async fn read_server_frame(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<ServerFrame> {
    let mut head = [0_u8; 2];
    reader.read_exact(&mut head).await?;
    if head[0] & 0x70 != 0 || head[0] & 0x80 == 0 || head[1] & 0x80 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid server WebSocket frame",
        ));
    }
    let opcode = head[0] & 0x0f;
    let mut length = u64::from(head[1] & 0x7f);
    if length == 126 {
        length = u64::from(reader.read_u16().await?);
    } else if length == 127 {
        length = reader.read_u64().await?;
    }
    if length > MAX_SERVER_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server WebSocket frame is too large",
        ));
    }
    let mut payload = vec![0_u8; length as usize];
    reader.read_exact(&mut payload).await?;
    Ok(ServerFrame { opcode, payload })
}

async fn write_client_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    writer.write_u8(0x80 | opcode).await?;
    let length = payload.len();
    if length < 126 {
        writer.write_u8(0x80 | length as u8).await?;
    } else if length <= usize::from(u16::MAX) {
        writer.write_u8(0x80 | 126).await?;
        writer.write_u16(length as u16).await?;
    } else {
        writer.write_u8(0x80 | 127).await?;
        writer.write_u64(length as u64).await?;
    }
    let mask_source = Uuid::new_v4();
    let mask = &mask_source.as_bytes()[..4];
    writer.write_all(mask).await?;
    for (index, byte) in payload.iter().enumerate() {
        writer.write_u8(*byte ^ mask[index % mask.len()]).await?;
    }
    writer.flush().await
}

fn is_meta_frame(payload: &[u8]) -> bool {
    payload
        .strip_prefix(&[0])
        .is_some_and(|json| serde_json::from_slice::<serde_json::Value>(json).is_ok())
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    Ok(decode_direct::<DataEnvelope<T>>(response).await?.data)
}

async fn decode_direct<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    let response = ensure_success(response).await?;
    response.json().await.map_err(to_string)
}

async fn ensure_success(response: Response) -> Result<Response, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(format!("supervisor API returned {status}: {body}"))
}

fn state_path() -> PathBuf {
    zuno_paths::data()
        .join("supervisor")
        .join("tui-supervisor.json")
}

fn read_state(path: &Path) -> Result<Option<SupervisorState>, String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut body = String::new();
    file.read_to_string(&mut body).map_err(to_string)?;
    let state: SupervisorState = serde_json::from_str(&body).map_err(to_string)?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported background TUI supervisor state schema {}",
            state.schema_version
        ));
    }
    Ok(Some(state))
}

fn write_private_state(path: &Path, state: &SupervisorState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "supervisor state path has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(to_string)?;
    set_private_directory(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("supervisor"),
        Uuid::new_v4().simple()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(to_string)?;
    let body = serde_json::to_vec(state).map_err(to_string)?;
    file.write_all(&body).map_err(to_string)?;
    file.sync_all().map_err(to_string)?;
    fs::rename(&temporary, path).map_err(to_string)?;
    Ok(())
}

fn set_private_directory(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700)).map_err(to_string)?;
    }
    Ok(())
}

#[cfg(unix)]
fn detached_server_command(executable: &Path) -> Command {
    use std::os::unix::process::CommandExt as _;
    let mut command = Command::new("nohup");
    command.arg(executable);
    command.process_group(0);
    command
}

#[cfg(windows)]
fn detached_server_command(executable: &Path) -> Command {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(executable);
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    command
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<(), String> {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .map_err(to_string)?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("could not terminate supervisor process {pid}: {status}"))
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        })
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<(), String> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .status()
        .map_err(to_string)?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("could not terminate supervisor process {pid}: {status}"))
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_tui_arguments_strip_supervisor_controls_and_keep_turn_selection() {
        let args = TuiArgs {
            prompt: Some("continue diagnosis".to_owned()),
            model: Some("provider/model".to_owned()),
            agent: Some("deep".to_owned()),
            r#continue: false,
            session: Some("ses_123".to_owned()),
            auto: true,
            background: true,
            attach: None,
            background_list: false,
            background_stop: None,
            background_shutdown: false,
        };
        assert_eq!(
            nested_tui_args(&args),
            [
                "tui",
                "--prompt",
                "continue diagnosis",
                "--model",
                "provider/model",
                "--agent",
                "deep",
                "--session",
                "ses_123",
                "--auto",
            ]
        );
    }

    #[tokio::test]
    async fn client_frames_are_masked_and_round_trip_the_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        write_client_frame(&mut writer, 2, b"hello")
            .await
            .expect("write frame");
        let first = reader.read_u8().await.expect("first");
        let second = reader.read_u8().await.expect("second");
        assert_eq!(first, 0x82);
        assert_eq!(second & 0x80, 0x80);
        assert_eq!(second & 0x7f, 5);
        let mut mask = [0_u8; 4];
        reader.read_exact(&mut mask).await.expect("mask");
        let mut payload = [0_u8; 5];
        reader.read_exact(&mut payload).await.expect("payload");
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
        assert_eq!(&payload, b"hello");
    }
}
