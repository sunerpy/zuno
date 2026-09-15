use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncWrite;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use super::Agent;
use super::EOF_REQUEST_DRAIN_GRACE;
use super::INITIALIZED;
use super::INITIALIZING;
use super::InFlight;
use super::InFlightRequest;
use super::MAX_INBOUND_FRAME_BYTES;
use super::OUTBOUND_FRAME_CHANNEL_CAPACITY;
use super::RequestId;
use super::RequestTermination;
use super::RpcError;
use super::ServeError;
use super::UNINITIALIZED;
use super::client::ClientConnection;
use super::client::PendingState;
use super::frame::FrameRead;
use super::frame::id_key;
use super::frame::read_frame;
use super::frame::write_frames;
use super::lock;

pub async fn serve_stdio<A>(agent: A) -> Result<(), ServeError>
where
    A: Agent,
{
    serve(agent, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn serve<A, R, W>(agent: A, input: R, output: W) -> Result<(), ServeError>
where
    A: Agent,
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (output_tx, output_rx) = mpsc::channel(OUTBOUND_FRAME_CHANNEL_CAPACITY);
    let (writer_stopped_tx, mut writer_stopped) = oneshot::channel();
    let writer = tokio::spawn(async move {
        let result = write_frames(output, output_rx).await;
        let _notified = writer_stopped_tx.send(());
        result
    });
    let client = ClientConnection {
        output: output_tx,
        pending: Arc::new(Mutex::new(PendingState::default())),
        next_id: Arc::new(AtomicU64::new(1)),
        deferred: None,
        scoped_requests: None,
    };
    let agent = Arc::new(agent);
    let initialized = Arc::new(AtomicU8::new(UNINITIALIZED));
    let in_flight = Arc::new(Mutex::new(InFlight::new()));
    let mut reader = BufReader::new(input);
    let mut requests = JoinSet::new();
    let mut clean_eof = false;

    let loop_result = async {
        loop {
            // A JoinSet holds each finished task's entry until it is joined, so a
            // session that stays connected for days would grow one entry per frame it
            // handled, and the EOF drain would spend its grace on tasks that already
            // responded instead of on the ones still running.
            while requests.try_join_next().is_some() {}
            let incoming = tokio::select! {
                frame = read_frame(&mut reader, MAX_INBOUND_FRAME_BYTES) => frame?,
                _ = &mut writer_stopped => return Err(ServeError::WriterClosed),
            };
            let frame = match incoming {
                FrameRead::Eof => {
                    clean_eof = true;
                    break;
                }
                FrameRead::Oversized => {
                    client
                        .response(
                            Value::Null,
                            Err(RpcError::invalid_request(format!(
                                "ACP frame exceeds the {MAX_INBOUND_FRAME_BYTES}-byte limit"
                            ))),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                }
                FrameRead::Frame(buffer) => match serde_json::from_slice::<Value>(&buffer) {
                    Ok(frame) => frame,
                    Err(error) => {
                        eprintln!("ACP parse error: {error}");
                        client
                            .response(Value::Null, Err(RpcError::new(-32700, "Parse error")))
                            .await
                            .map_err(|_| ServeError::WriterClosed)?;
                        continue;
                    }
                },
            };
            if frame.get("method").is_none() {
                client.resolve_response(&frame);
                continue;
            }
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                client
                    .response(
                        id,
                        Err(RpcError::invalid_request("method must be a string")),
                    )
                    .await
                    .map_err(|_| ServeError::WriterClosed)?;
                continue;
            };
            if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                client
                    .response(id, Err(RpcError::invalid_request("jsonrpc must be 2.0")))
                    .await
                    .map_err(|_| ServeError::WriterClosed)?;
                continue;
            }
            let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));

            if method == "$/cancel_request" {
                if let Some(request_id) = params.get("requestId")
                    && let Some((withdrawn, request)) = take_in_flight(&in_flight, request_id)
                {
                    agent
                        .request_cancelled(&request.method, &withdrawn, &request.params)
                        .await;
                    if let Some(cancel) = request.cancel {
                        let _ignored = cancel.send(RequestTermination::Withdrawn);
                    }
                }
                // A peer withdraws only a request it authored. An unknown
                // inbound id must not cancel an unrelated agent-to-client RPC.
                continue;
            }

            if method.starts_with("session/") && initialized.load(Ordering::Acquire) != INITIALIZED
            {
                if let Some(id) = frame.get("id").cloned() {
                    client
                        .response(
                            id,
                            Err(RpcError::invalid_request(
                                "initialize must complete before session methods",
                            )),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                }
                continue;
            }

            if let Some(id) = frame.get("id").cloned() {
                let Some(request_identity) = RequestId::from_json(&id) else {
                    client
                        .response(
                            Value::Null,
                            Err(RpcError::invalid_request("invalid request id")),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                };
                let request_key = request_identity.wire_key.clone();
                if method == "initialize"
                    && initialized
                        .compare_exchange(
                            UNINITIALIZED,
                            INITIALIZING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                {
                    client
                        .response(
                            id,
                            Err(RpcError::invalid_request(
                                "initialize is already in progress or complete",
                            )),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                }

                let request_method = method.to_owned();
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let response_ready = Arc::new(AtomicBool::new(false));
                let duplicate = {
                    let mut active = lock(&in_flight);
                    if active.contains_key(&request_key) {
                        true
                    } else {
                        active.insert(
                            request_key.clone(),
                            InFlightRequest {
                                identity: request_identity.clone(),
                                cancel: Some(cancel_tx),
                                method: request_method.clone(),
                                params: params.clone(),
                                response_ready: Arc::clone(&response_ready),
                            },
                        );
                        false
                    }
                };
                if duplicate {
                    if method == "initialize" {
                        initialized.store(UNINITIALIZED, Ordering::Release);
                    }
                    client
                        .response(id, Err(RpcError::invalid_request("duplicate request id")))
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                }

                let agent = Arc::clone(&agent);
                let client = client.clone();
                let initialized = Arc::clone(&initialized);
                let in_flight = Arc::clone(&in_flight);
                let method = request_method;
                requests.spawn(async move {
                    let request_client = client.request_scoped();
                    let result = tokio::select! {
                        result = agent.request(&method, &request_identity, params, request_client.clone()) => result,
                        termination = cancel_rx => {
                            if let Err(error) = request_client.cancel_scoped_requests().await {
                                eprintln!("ACP child request cancellation failed: {error}");
                            }
                            Err(termination.unwrap_or(RequestTermination::Disconnected).error())
                        },
                    };
                    response_ready.store(true, Ordering::Release);
                    if let Err(error) = request_client.cancel_scoped_requests().await {
                        eprintln!("ACP child request cleanup failed: {error}");
                    }
                    if method == "initialize" {
                        initialized.store(
                            if result.is_ok() {
                                INITIALIZED
                            } else {
                                UNINITIALIZED
                            },
                            Ordering::Release,
                        );
                    }
                    lock(&in_flight).remove(&request_key);
                    let succeeded = result.is_ok();
                    if let Err(error) = client.response(id, result).await {
                        eprintln!("ACP response failed: {error}");
                    } else if succeeded
                        && let Err(error) = request_client.flush_after_response().await
                    {
                        eprintln!("ACP deferred notification failed: {error}");
                    }
                });
            } else {
                if method == "initialize" {
                    continue;
                }
                if method == "session/cancel" {
                    // Bind cancellation before admitting a later frame. Running
                    // this notification in the task set lets a following prompt
                    // overtake even a cancellation already received on the wire.
                    if let Err(error) = agent.notification(method, params, client.clone()).await {
                        eprintln!("ACP notification failed: {error}");
                    }
                    continue;
                }
                let agent = Arc::clone(&agent);
                let client = client.clone();
                let method = method.to_owned();
                requests.spawn(async move {
                    if let Err(error) = agent.notification(&method, params, client).await {
                        eprintln!("ACP notification failed: {error}");
                    }
                });
            }
        }
        Ok::<(), ServeError>(())
    }
    .await;

    if clean_eof && loop_result.is_ok() {
        drain_accepted_requests_at_eof(&mut requests).await;
    }
    let cancellations = {
        let mut active = lock(&in_flight);
        active
            .extract_if(|_, request| {
                !clean_eof
                    || loop_result.is_err()
                    || !request.response_ready.load(Ordering::Acquire)
            })
            .collect::<Vec<_>>()
    };
    for (_, request) in cancellations {
        agent
            .request_disconnected(&request.method, &request.identity, &request.params)
            .await;
        if let Some(cancel) = request.cancel {
            let _ignored = cancel.send(RequestTermination::Disconnected);
        }
    }
    client.close_pending(RpcError::internal("ACP connection closed"));
    while requests.join_next().await.is_some() {}
    let close_output = client.close_output().await;
    drop(agent);
    drop(client);
    let writer_result = writer.await?;
    loop_result?;
    close_output.map_err(|_| ServeError::WriterClosed)?;
    writer_result?;
    Ok(())
}

fn take_in_flight(
    in_flight: &Mutex<InFlight>,
    request_id: &Value,
) -> Option<(RequestId, InFlightRequest)> {
    let request_key = id_key(request_id)?;
    let mut active = lock(in_flight);
    let request = active.get_mut(&request_key)?;
    if request.response_ready.load(Ordering::Acquire) {
        return None;
    }
    let cancel = request.cancel.take()?;
    Some((
        request.identity.clone(),
        InFlightRequest {
            identity: request.identity.clone(),
            cancel: Some(cancel),
            method: request.method.clone(),
            params: request.params.clone(),
            response_ready: Arc::clone(&request.response_ready),
        },
    ))
}

async fn drain_accepted_requests_at_eof(requests: &mut JoinSet<()>) {
    let deadline = tokio::time::Instant::now() + EOF_REQUEST_DRAIN_GRACE;
    while !requests.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, requests.join_next()).await {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}
