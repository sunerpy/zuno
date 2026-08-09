use std::io;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{
    CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use base64::Engine as _;
use oc_pty::{
    AttachOptions, ConnectToken, CreateInput, PtyId, PtyInfo, PtyOutput, ReplayCursor, TicketScope,
    UpdateInput,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::Data;
use super::error::ApiError;
use super::state::ApiState;

const REPLAY_CHUNK: usize = 64 * 1024;
const MAX_CLIENT_FRAME: u64 = 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
pub struct ConnectTokenQuery {
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
    #[serde(rename = "location[workspace]")]
    workspace_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    ticket: Option<String>,
    cursor: Option<i64>,
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
    #[serde(rename = "location[workspace]")]
    workspace_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectTokenResponse {
    location: TokenLocation,
    data: ConnectToken,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenLocation {
    directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    project: TokenProject,
}

#[derive(Debug, Serialize)]
struct TokenProject {
    id: &'static str,
    directory: String,
}

struct ClientFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

pub async fn list(State(state): State<ApiState>) -> Json<Data<Vec<PtyInfo>>> {
    Json(Data::new(state.pty().list()))
}

pub async fn create(
    State(state): State<ApiState>,
    Json(input): Json<CreateInput>,
) -> Result<Json<Data<PtyInfo>>, ApiError> {
    Ok(Json(Data::new(state.pty().create(input)?)))
}

pub async fn get(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
) -> Result<Json<Data<PtyInfo>>, ApiError> {
    Ok(Json(Data::new(state.pty().get(&PtyId::from_raw(pty_id))?)))
}

pub async fn update(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
    Json(input): Json<UpdateInput>,
) -> Result<Json<Data<PtyInfo>>, ApiError> {
    Ok(Json(Data::new(
        state.pty().update(&PtyId::from_raw(pty_id), input)?,
    )))
}

pub async fn remove(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.pty().remove(&PtyId::from_raw(pty_id))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn connect_token(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
    Query(query): Query<ConnectTokenQuery>,
    headers: HeaderMap,
) -> Result<Json<ConnectTokenResponse>, ApiError> {
    if headers
        .get("x-opencode-ticket")
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err(ApiError::Forbidden);
    }
    let pty_id = PtyId::from_raw(pty_id);
    state.pty().get(&pty_id)?;
    let directory = query
        .directory
        .unwrap_or_else(|| state.directory().to_owned());
    let scope = TicketScope {
        pty_id,
        directory: Some(directory.clone()),
        workspace_id: query.workspace_id.clone(),
    };
    let data = state.pty().tickets().issue(scope);
    Ok(Json(ConnectTokenResponse {
        location: TokenLocation {
            directory: directory.clone(),
            workspace_id: query.workspace_id,
            project: TokenProject {
                id: oc_paths::GLOBAL_PROJECT_ID,
                directory,
            },
        },
        data,
    }))
}

pub async fn connect(
    State(state): State<ApiState>,
    Path(pty_id): Path<String>,
    Query(query): Query<ConnectQuery>,
    mut request: Request,
) -> Result<Response, ApiError> {
    let pty_id = PtyId::from_raw(pty_id);
    state.pty().get(&pty_id)?;
    let directory = query
        .directory
        .unwrap_or_else(|| state.directory().to_owned());
    let scope = TicketScope {
        pty_id: pty_id.clone(),
        directory: Some(directory),
        workspace_id: query.workspace_id,
    };
    let ticket = query.ticket.as_deref().ok_or(ApiError::Forbidden)?;
    if !state.pty().tickets().consume(ticket, &scope) {
        return Err(ApiError::Forbidden);
    }

    let key = websocket_key(request.headers())?;
    let on_upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>()
        .ok_or(ApiError::InvalidRequest("connection is not upgradable"))?;
    let cursor = match query.cursor {
        None => ReplayCursor::Full,
        Some(-1) => ReplayCursor::Tail,
        Some(value) if value >= 0 => ReplayCursor::From(value as u64),
        Some(_) => return Err(ApiError::InvalidRequest("invalid PTY cursor")),
    };
    let attachment = state.pty().attach(
        &pty_id,
        AttachOptions {
            cursor,
            ..AttachOptions::default()
        },
    )?;
    let pty = state.pty().clone();
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upgrade.await {
            let _ = serve_socket(upgraded, pty, pty_id, attachment).await;
        }
    });

    let accept = websocket_accept(&key);
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(CONNECTION, "upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_ACCEPT, accept)
        .body(Body::empty())
        .map_err(|_| ApiError::InvalidRequest("invalid WebSocket response"))
}

fn websocket_key(headers: &HeaderMap) -> Result<HeaderValue, ApiError> {
    if !header_contains(headers, CONNECTION, "upgrade")
        || !header_equals(headers, UPGRADE, "websocket")
        || !header_equals(headers, SEC_WEBSOCKET_VERSION, "13")
    {
        return Err(ApiError::InvalidRequest("invalid WebSocket upgrade"));
    }
    headers
        .get(SEC_WEBSOCKET_KEY)
        .cloned()
        .ok_or(ApiError::InvalidRequest("invalid WebSocket upgrade"))
}

fn header_equals(
    headers: &HeaderMap,
    name: axum::http::header::HeaderName,
    expected: &str,
) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn header_contains(
    headers: &HeaderMap,
    name: axum::http::header::HeaderName,
    expected: &str,
) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().eq_ignore_ascii_case(expected))
        })
    })
}

fn websocket_accept(key: &HeaderValue) -> String {
    let mut input = Vec::with_capacity(key.as_bytes().len() + 36);
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, &input);
    base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
}

async fn serve_socket(
    upgraded: hyper::upgrade::Upgraded,
    pty: oc_pty::PtyService,
    pty_id: PtyId,
    mut attachment: oc_pty::Attachment,
) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(hyper_util::rt::TokioIo::new(upgraded));
    for chunk in attachment.replay.chunks(REPLAY_CHUNK) {
        write_server_frame(&mut writer, 2, chunk).await?;
    }
    write_server_frame(&mut writer, 2, &meta_frame(attachment.cursor)).await?;
    let mut fragmented = Vec::new();
    let mut fragmented_opcode = None;
    loop {
        tokio::select! {
            frame = read_client_frame(&mut reader) => {
                let frame = frame?;
                match frame.opcode {
                    0 => {
                        if fragmented_opcode.is_none() {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected continuation frame"));
                        }
                        append_fragment(&mut fragmented, &frame.payload)?;
                        if frame.fin {
                            write_input(&pty, &pty_id, fragmented_opcode.take().unwrap_or(2), &fragmented);
                            fragmented.clear();
                        }
                    }
                    1 | 2 if frame.fin => write_input(&pty, &pty_id, frame.opcode, &frame.payload),
                    1 | 2 => {
                        if fragmented_opcode.replace(frame.opcode).is_some() {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "nested fragmented frame"));
                        }
                        append_fragment(&mut fragmented, &frame.payload)?;
                    }
                    8 => {
                        write_server_frame(&mut writer, 8, &frame.payload).await?;
                        return Ok(());
                    }
                    9 => write_server_frame(&mut writer, 10, &frame.payload).await?,
                    10 => {}
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported WebSocket opcode")),
                }
            }
            output = attachment.output.recv() => {
                match output {
                    Some(PtyOutput::Chunk(chunk)) => write_server_frame(&mut writer, 2, &chunk).await?,
                    Some(PtyOutput::Lagged { cursor, .. }) => {
                        write_server_frame(&mut writer, 2, &meta_frame(cursor)).await?;
                    }
                    Some(PtyOutput::Ended { .. }) | None => {
                        write_server_frame(&mut writer, 8, &1000_u16.to_be_bytes()).await?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn append_fragment(target: &mut Vec<u8>, chunk: &[u8]) -> io::Result<()> {
    if target.len().saturating_add(chunk.len()) > MAX_CLIENT_FRAME as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fragmented frame is too large",
        ));
    }
    target.extend_from_slice(chunk);
    Ok(())
}

fn write_input(pty: &oc_pty::PtyService, pty_id: &PtyId, opcode: u8, payload: &[u8]) {
    if opcode == 1 && std::str::from_utf8(payload).is_err() {
        return;
    }
    if opcode == 2 && std::str::from_utf8(payload).is_err() {
        return;
    }
    let _ = pty.write(pty_id, payload);
}

fn meta_frame(cursor: u64) -> Vec<u8> {
    let mut frame = vec![0];
    frame.extend_from_slice(format!("{{\"cursor\":{cursor}}}").as_bytes());
    frame
}

async fn read_client_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<ClientFrame> {
    let mut head = [0_u8; 2];
    reader.read_exact(&mut head).await?;
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0f;
    if head[0] & 0x70 != 0 || head[1] & 0x80 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid client frame",
        ));
    }
    let mut length = u64::from(head[1] & 0x7f);
    if length == 126 {
        let mut extended = [0_u8; 2];
        reader.read_exact(&mut extended).await?;
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0_u8; 8];
        reader.read_exact(&mut extended).await?;
        length = u64::from_be_bytes(extended);
    }
    if length > MAX_CLIENT_FRAME || matches!(opcode, 8..=10) && (!fin || length > 125) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client frame is too large",
        ));
    }
    let mut mask = [0_u8; 4];
    reader.read_exact(&mut mask).await?;
    let mut payload = vec![0_u8; length as usize];
    reader.read_exact(&mut payload).await?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % mask.len()];
    }
    Ok(ClientFrame {
        fin,
        opcode,
        payload,
    })
}

async fn write_server_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    opcode: u8,
    payload: &[u8],
) -> io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | opcode);
    match payload.len() {
        0..=125 => head.push(payload.len() as u8),
        126..=65_535 => {
            head.push(126);
            head.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        _ => {
            head.push(127);
            head.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }
    writer.write_all(&head).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}
