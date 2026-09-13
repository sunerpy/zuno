use super::*;
use futures::TryStreamExt;
use zuno_application::{
    environment::EnvironmentSnapshot,
    workspace_transfer::{
        SnapshotTransferCompletion, SnapshotTransferContext, SnapshotTransferRequest,
    },
};
use zuno_identity::gateway::GatewaySnapshotTicket;

pub struct SnapshotDownload {
    pub snapshot: EnvironmentSnapshot,
    pub input: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
}

impl GatewayStateClient {
    pub async fn snapshot_ticket(
        &self,
        request: &SnapshotTransferRequest,
    ) -> Result<IssuedSnapshotTransfer, ApplicationError> {
        let bytes = self
            .control
            .post(
                GATEWAY_SNAPSHOT_TICKET_PATH,
                None,
                serde_json::to_vec(request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&bytes).map_err(ApplicationError::storage)
    }

    pub async fn snapshot_context(
        &self,
        ticket: &GatewaySnapshotTicket,
        request: &SnapshotTransferRequest,
    ) -> Result<SnapshotTransferContext, ApplicationError> {
        let bytes = self
            .control
            .post_header(
                GATEWAY_SNAPSHOT_RESOLVE_PATH,
                Some((GATEWAY_SNAPSHOT_TICKET_HEADER, ticket.expose())),
                serde_json::to_vec(request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&bytes).map_err(ApplicationError::storage)
    }

    pub async fn snapshot_completed(
        &self,
        completion: &SnapshotTransferCompletion,
    ) -> Result<(), ApplicationError> {
        completion
            .assignment
            .validate_snapshot(&completion.snapshot)?;
        self.control
            .post(
                GATEWAY_SNAPSHOT_COMPLETE_PATH,
                None,
                serde_json::to_vec(completion).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        Ok(())
    }

    pub async fn snapshot_fact(
        &self,
        request: &SnapshotTransferRequest,
    ) -> Result<SnapshotTransferContext, ApplicationError> {
        let bytes = self
            .control
            .post(
                GATEWAY_SNAPSHOT_FACT_PATH,
                None,
                serde_json::to_vec(request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&bytes).map_err(ApplicationError::storage)
    }

    pub async fn download_snapshot(
        &self,
        issued: &IssuedSnapshotTransfer,
    ) -> Result<SnapshotDownload, ApplicationError> {
        let assigned = &issued.context.assignment;
        let endpoint = validate_endpoint(&assigned.source.endpoint).map_err(state_error)?;
        let mut credential = HeaderValue::from_str(issued.ticket.expose())
            .map_err(|_| ApplicationError::Forbidden)?;
        credential.set_sensitive(true);
        // No gateway service token or Worker grant is forwarded to a peer.
        let response = self
            .snapshots
            .post(
                endpoint
                    .join(GATEWAY_SNAPSHOT_EXPORT_PATH)
                    .map_err(ApplicationError::storage)?,
            )
            .timeout(Duration::from_secs(600))
            .header(GATEWAY_SNAPSHOT_TICKET_HEADER, credential)
            .json(&assigned.request)
            .send()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(ApplicationError::Forbidden),
            404 => return Err(ApplicationError::NotFound),
            409 => return Err(ApplicationError::Conflict),
            _ => return Err(ApplicationError::Unavailable),
        }
        let number = |key: &str| {
            response
                .headers()
                .get(key)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or(ApplicationError::Conflict)
        };
        let snapshot = EnvironmentSnapshot {
            id: assigned.request.snapshot_id()?,
            environment_id: assigned.source.environment.id.clone(),
            revision: number("x-zuno-snapshot-revision")?,
            bytes: number("content-length")?,
            sha256: response
                .headers()
                .get("x-zuno-snapshot-sha256")
                .and_then(|v| v.to_str().ok())
                .ok_or(ApplicationError::Conflict)?
                .to_owned(),
        };
        assigned.validate_snapshot(&snapshot)?;
        if response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some("application/x-tar")
        {
            return Err(ApplicationError::Conflict);
        }
        // Headers from a peer do not establish source provenance. Compare with
        // the fact committed under the source's separate service identity.
        let fact = self.snapshot_fact(&assigned.request).await?;
        if fact.assignment.digest() != assigned.digest()
            || fact.snapshot.as_ref() != Some(&snapshot)
        {
            return Err(ApplicationError::Conflict);
        }
        let stream = Box::pin(response.bytes_stream().map_err(std::io::Error::other));
        Ok(SnapshotDownload {
            snapshot,
            input: Box::pin(tokio_util::io::StreamReader::new(stream)),
        })
    }
}
