use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::{
    HandlerError, HookCall, HookResult, HostInfo, InitializeParams, InitializeResult,
    PROTOCOL_VERSION, Plugin, ToolCall,
};

/// Serve one plugin over process standard input and output.
///
/// # Errors
/// Returns [`ServeError`] when framing fails or the plugin definition is invalid.
pub async fn serve(plugin: Plugin) -> Result<(), ServeError> {
    serve_io(plugin, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve the protocol on injected I/O so plugin authors can exercise framing in tests.
///
/// # Errors
/// Returns [`ServeError`] for I/O, JSON framing, or invalid plugin metadata.
pub async fn serve_io<R, W>(plugin: Plugin, reader: R, mut writer: W) -> Result<(), ServeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    plugin.validate()?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut initialized = false;
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        let frame = line.trim_end_matches(['\r', '\n']);
        if frame.is_empty() {
            continue;
        }
        let request: RpcRequest = serde_json::from_str(frame)?;
        let response = handle(&plugin, &mut initialized, request).await;
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        writer.flush().await?;
    }
}

async fn handle(plugin: &Plugin, initialized: &mut bool, request: RpcRequest) -> RpcResponse {
    let id = request.id;
    if request.jsonrpc != "2.0" {
        return RpcResponse::error(id, -32600, "jsonrpc must be 2.0");
    }
    let result = match request.method.as_str() {
        "plugin.initialize" => initialize(plugin, initialized, request.params),
        "hook.call" if *initialized => call_hook(plugin, request.params).await,
        "tool.call" if *initialized => call_tool(plugin, request.params).await,
        "hook.call" | "tool.call" => Err(RpcFailure::new(-32002, "plugin is not initialized")),
        _ => Err(RpcFailure::new(-32601, "method not found")),
    };
    match result {
        Ok(result) => RpcResponse::success(id, result),
        Err(error) => RpcResponse::error(id, error.code, error.message),
    }
}

fn initialize(plugin: &Plugin, initialized: &mut bool, params: Value) -> Result<Value, RpcFailure> {
    if *initialized {
        return Err(RpcFailure::new(-32003, "plugin is already initialized"));
    }
    let params: InitializeParams = decode(params)?;
    if !params
        .protocol_versions
        .iter()
        .any(|version| version == PROTOCOL_VERSION)
    {
        return Err(RpcFailure::new(
            -32001,
            format!("plugin supports protocol {PROTOCOL_VERSION}"),
        ));
    }
    let _host: HostInfo = params.host;
    *initialized = true;
    encode(InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_owned(),
        plugin: plugin.manifest(),
    })
}

async fn call_hook(plugin: &Plugin, params: Value) -> Result<Value, RpcFailure> {
    let call: HookCall = decode(params)?;
    let call = plugin.call_hook(call).await.map_err(RpcFailure::handler)?;
    encode(HookResult {
        output: call.output,
    })
}

async fn call_tool(plugin: &Plugin, params: Value) -> Result<Value, RpcFailure> {
    let call: ToolCall = decode(params)?;
    let output = plugin.call_tool(call).await.map_err(RpcFailure::handler)?;
    encode(output)
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, RpcFailure> {
    serde_json::from_value(value)
        .map_err(|error| RpcFailure::new(-32602, format!("invalid params: {error}")))
}

fn encode<T: Serialize>(value: T) -> Result<Value, RpcFailure> {
    serde_json::to_value(value)
        .map_err(|error| RpcFailure::new(-32603, format!("could not encode result: {error}")))
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

impl RpcResponse {
    fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

struct RpcFailure {
    code: i64,
    message: String,
}

impl RpcFailure {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn handler(error: HandlerError) -> Self {
        Self::new(-32010, error.to_string())
    }
}

/// The stdio server cannot continue without a valid frame or valid plugin metadata.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("plugin definition is invalid")]
    Build(#[from] crate::BuildError),
    #[error("plugin stdio failed")]
    Io(#[from] io::Error),
    #[error("plugin received malformed JSON-RPC")]
    Json(#[from] serde_json::Error),
}
