use super::*;
use zuno_application::{
    environment::EnvironmentSnapshot,
    workspace_transfer::{
        SnapshotTransferCompletion, SnapshotTransferPurpose, SnapshotTransferRequest,
    },
};

impl GatewayExecutionService {
    pub(super) async fn transfer_snapshot(
        &self,
        context: &GatewayExecutionContext,
        purpose: SnapshotTransferPurpose,
    ) -> Result<EnvironmentSnapshot, ApplicationError> {
        let _permit = self
            .snapshot_imports
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApplicationError::Unavailable)?;
        let request = SnapshotTransferRequest {
            lease: context.lease.clone(),
            purpose,
        };
        let issued = self.state.snapshot_ticket(&request).await?;
        let assigned = &issued.context.assignment;
        if assigned.request != request
            || assigned.target_gateway_id != self.id
            || context.workspace_source.as_ref() != Some(&assigned.source)
        {
            return Err(ApplicationError::Forbidden);
        }
        let cached = if let Some(snapshot) = &issued.context.snapshot {
            assigned.validate_snapshot(snapshot)?;
            self.gateway
                .has_snapshot(&request.lease.owner, snapshot)
                .await?
        } else {
            false
        };
        let snapshot = if cached {
            issued
                .context
                .snapshot
                .clone()
                .ok_or(ApplicationError::Conflict)?
        } else {
            let downloaded = self.state.download_snapshot(&issued).await?;
            self.gateway
                .receive_snapshot(assigned, &downloaded.snapshot, downloaded.input)
                .await?;
            downloaded.snapshot
        };
        // Receiving bytes does not restore execution authority. Recheck current
        // policy, Job lease and source lineage before publishing a target.
        let current = self.state.snapshot_fact(&request).await?;
        if current.assignment.digest() != assigned.digest()
            || current.snapshot.as_ref() != Some(&snapshot)
        {
            return Err(ApplicationError::Forbidden);
        }
        Ok(snapshot)
    }
}

pub(super) async fn export(
    State(service): State<GatewayExecutionService>,
    headers: HeaderMap,
    Json(request): Json<SnapshotTransferRequest>,
) -> Result<Response, Failure> {
    let permit = service
        .snapshot_exports
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure(ApplicationError::Unavailable))?;
    let values = headers.get_all(zuno_worker::GATEWAY_SNAPSHOT_TICKET_HEADER);
    if values.iter().count() != 1 {
        return Err(Failure(ApplicationError::Forbidden));
    }
    let ticket = zuno_identity::gateway::GatewaySnapshotTicket::try_from(
        values
            .iter()
            .next()
            .and_then(|v| v.to_str().ok())
            .ok_or(Failure(ApplicationError::Forbidden))?
            .to_owned(),
    )
    .map_err(|_| Failure(ApplicationError::Forbidden))?;
    let context = service.state.snapshot_context(&ticket, &request).await?;
    let assigned = context.assignment;
    if assigned.source.gateway_id != service.id || assigned.request != request {
        return Err(Failure(ApplicationError::Forbidden));
    }
    let (snapshot, file) = service.gateway.export_snapshot(&assigned).await?;
    if context
        .snapshot
        .as_ref()
        .is_some_and(|expected| expected != &snapshot)
    {
        return Err(Failure(ApplicationError::Conflict));
    }
    service
        .state
        .snapshot_completed(&SnapshotTransferCompletion {
            assignment: assigned,
            snapshot: snapshot.clone(),
        })
        .await?;
    use futures::StreamExt;
    use tokio::io::AsyncReadExt;
    let stream = tokio_util::io::ReaderStream::new(file.take(snapshot.bytes)).map(move |chunk| {
        let _retained = &permit;
        chunk
    });
    let mut response = Response::new(axum::body::Body::from_stream(stream));
    use axum::http::HeaderValue;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-tar"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    for (key, value) in [
        ("content-length", snapshot.bytes.to_string()),
        ("x-zuno-snapshot-revision", snapshot.revision.to_string()),
        ("x-zuno-snapshot-sha256", snapshot.sha256),
    ] {
        headers.insert(
            key,
            HeaderValue::from_str(&value).map_err(|e| Failure(ApplicationError::storage(e)))?,
        );
    }
    Ok(response)
}
