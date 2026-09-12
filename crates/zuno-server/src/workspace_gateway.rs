//! Authenticated workspace upload and review transfers. User tickets have
//! separate purposes and never expose an Agent Worker execution credential.
use crate::gateway_configuration::GatewayConfigurationResolver;
use futures::StreamExt;
use std::sync::Arc;
use zuno_application::{ApplicationError, workspace_merge::MergeContentRequest};
use zuno_identity::gateway::GatewayTicketAuthority;
use zuno_postgres::PostgresBackend;
use zuno_types::identity::PrincipalScope;

pub struct GatewayWorkspaceClient {
    backend: PostgresBackend,
    tickets: Arc<GatewayTicketAuthority>,
    assignments: Arc<dyn GatewayConfigurationResolver>,
    client: reqwest::Client,
}
impl GatewayWorkspaceClient {
    pub async fn begin_import(
        &self,
        principal: &PrincipalScope,
        session: &zuno_types::identity::SessionId,
        configuration: zuno_application::runtime::ConfigurationRef,
        request: zuno_application::workspace_import::BeginWorkspaceImport,
    ) -> Result<zuno_application::workspace_import::WorkspaceImportView, ApplicationError> {
        use zuno_application::workspace_import::WorkspaceImportStore;
        let target = self
            .assignments
            .resolve(principal.tenant_id(), &configuration, session)?;
        self.backend
            .begin_import(
                principal,
                session,
                request,
                configuration,
                target.gateway_id,
                target.environment,
            )
            .await
    }
    pub async fn upload(
        &self,
        principal: &PrincipalScope,
        request: zuno_application::workspace_import::WorkspaceUploadRequest,
        length: u64,
        body: axum::body::Body,
    ) -> Result<zuno_application::workspace_import::WorkspaceImportView, ApplicationError> {
        use zuno_application::workspace_import::{
            WorkspaceImportReceipt, WorkspaceImportState, WorkspaceImportStore,
        };
        let state = self
            .backend
            .import_view(principal, &request.session_id, &request.import_id)
            .await?;
        let assigned = self
            .backend
            .import_assignment(principal, &request.session_id, &request.import_id)
            .await?;
        if length != assigned.bytes {
            return Err(ApplicationError::Invalid(
                "archive length differs from its declared input".to_owned(),
            ));
        }
        if state.state == WorkspaceImportState::Ready {
            let mut stream = body.into_data_stream();
            let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
            let mut received = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(ApplicationError::storage)?;
                received = received
                    .checked_add(chunk.len() as u64)
                    .ok_or(ApplicationError::Conflict)?;
                if received > length {
                    return Err(ApplicationError::Conflict);
                }
                digest.update(&chunk);
            }
            if received != length || hex_digest(digest.finish()) != assigned.sha256 {
                return Err(ApplicationError::Conflict);
            }
            return Ok(state);
        }
        let target = self.assignments.resolve(
            principal.tenant_id(),
            &assigned.configuration,
            &assigned.session_id,
        )?;
        if target.gateway_id != assigned.gateway_id || target.environment != assigned.environment {
            return Err(ApplicationError::Conflict);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(ApplicationError::storage)?
            .as_millis();
        let ticket = self
            .tickets
            .issue_import(
                principal,
                target.gateway_id,
                &request,
                i64::try_from(now).map_err(ApplicationError::storage)?,
            )
            .map_err(ApplicationError::storage)?;
        let mut url = url::Url::parse(&target.endpoint)
            .map_err(ApplicationError::storage)?
            .join(zuno_worker::GATEWAY_IMPORT_PATH)
            .map_err(ApplicationError::storage)?;
        url.query_pairs_mut()
            .append_pair("sessionId", request.session_id.as_str())
            .append_pair("importId", request.import_id.as_str());
        let mut transferred = 0u64;
        let stream = body.into_data_stream().map(move |chunk| {
            let chunk = chunk.map_err(std::io::Error::other)?;
            transferred = transferred
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| std::io::Error::other("archive size overflow"))?;
            if transferred > length {
                return Err(std::io::Error::other("archive exceeds declared length"));
            }
            Ok::<_, std::io::Error>(chunk)
        });
        let response = self
            .client
            .post(url)
            .header(zuno_worker::GATEWAY_IMPORT_TICKET_HEADER, ticket.expose())
            .header("content-type", "application/x-tar")
            .header("content-length", length)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        if !response.status().is_success() {
            return Err(match response.status().as_u16() {
                400 => {
                    ApplicationError::Invalid("gateway rejected the workspace archive".to_owned())
                }
                401 | 403 => ApplicationError::Forbidden,
                404 => ApplicationError::NotFound,
                409 => ApplicationError::Conflict,
                _ => ApplicationError::Unavailable,
            });
        }
        let mut chunks = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|_| ApplicationError::Unavailable)?;
            if bytes.len() + chunk.len()
                > zuno_application::environment::wire::MAX_GATEWAY_FRAME_BYTES
            {
                return Err(ApplicationError::Conflict);
            }
            bytes.extend_from_slice(&chunk);
        }
        let receipt: WorkspaceImportReceipt =
            serde_json::from_slice(&bytes).map_err(ApplicationError::storage)?;
        assigned.validate_receipt(&receipt)?;
        let current = self
            .backend
            .import_view(principal, &request.session_id, &request.import_id)
            .await?;
        if current.state != WorkspaceImportState::Ready {
            return Err(ApplicationError::Conflict);
        }
        Ok(current)
    }
    pub fn new(
        backend: PostgresBackend,
        tickets: Arc<GatewayTicketAuthority>,
        assignments: Arc<dyn GatewayConfigurationResolver>,
        certificate: Option<reqwest::Certificate>,
    ) -> Result<Self, ApplicationError> {
        let mut builder = zuno_network::client_builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(600));
        if let Some(certificate) = certificate {
            builder = builder.add_root_certificate(certificate);
        }
        Ok(Self {
            backend,
            tickets,
            assignments,
            client: builder.build().map_err(ApplicationError::storage)?,
        })
    }
    pub async fn content(
        &self,
        viewer: &PrincipalScope,
        request: MergeContentRequest,
    ) -> Result<axum::response::Response, ApplicationError> {
        let (admission, _) = self
            .backend
            .workspace_merge_for_approval(viewer, &request.approval_id)
            .await?;
        // This also rejects non-changed paths and missing sides before minting
        // a capability. Redemption rechecks the current approval viewer policy.
        let (_, content_context) = self
            .backend
            .workspace_merge_content(viewer, &request)
            .await?;
        let runtime = self.backend.runtime(viewer.tenant_id().clone());
        use zuno_application::runtime::RuntimeStore;
        let job = runtime
            .get(&admission.lease.owner, &admission.lease.job_id)
            .await?;
        let assignment =
            self.assignments
                .resolve(viewer.tenant_id(), &job.configuration, &job.session_id)?;
        if assignment.gateway_id != admission.gateway_id
            || assignment.environment != admission.environment.spec
        {
            return Err(ApplicationError::Conflict);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(ApplicationError::storage)?
            .as_millis();
        let now = i64::try_from(now).map_err(ApplicationError::storage)?;
        let ticket = self
            .tickets
            .issue_read(viewer, assignment.gateway_id, &request, now)
            .map_err(ApplicationError::storage)?;
        let endpoint = url::Url::parse(&assignment.endpoint)
            .map_err(ApplicationError::storage)?
            .join(zuno_worker::GATEWAY_MERGE_READ_PATH)
            .map_err(ApplicationError::storage)?;
        let response = self
            .client
            .post(endpoint)
            .header(zuno_worker::GATEWAY_READ_TICKET_HEADER, ticket.expose())
            .json(&request)
            .send()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        if !response.status().is_success() {
            return Err(match response.status().as_u16() {
                401 | 403 => ApplicationError::Forbidden,
                404 => ApplicationError::NotFound,
                409 => ApplicationError::Conflict,
                _ => ApplicationError::Unavailable,
            });
        }
        let size = response
            .content_length()
            .ok_or(ApplicationError::Conflict)?;
        if size > 512 * 1024 * 1024 {
            return Err(ApplicationError::Conflict);
        }
        let hash = response
            .headers()
            .get("x-zuno-content-sha256")
            .and_then(|value| value.to_str().ok())
            .ok_or(ApplicationError::Conflict)?
            .to_owned();
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ApplicationError::Conflict);
        }
        if content_context
            .expected
            .content_descriptor()
            .is_some_and(|expected| expected != (size, hash.clone()))
        {
            return Err(ApplicationError::Conflict);
        }
        let expected = hash.clone();
        let empty = hex_digest(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &[]));
        if size == 0 && hash != empty {
            return Err(ApplicationError::Conflict);
        }
        let stream = futures::stream::try_unfold(
            (
                Box::pin(response.bytes_stream()),
                0u64,
                Some(aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256)),
            ),
            move |(mut stream, mut received, mut digest)| {
                let expected = expected.clone();
                async move {
                    let Some(chunk) = stream.next().await else {
                        if received != size {
                            return Err(std::io::Error::other(
                                "content ended before its immutable size",
                            ));
                        }
                        return Ok(None);
                    };
                    let chunk = chunk.map_err(std::io::Error::other)?;
                    received = received
                        .checked_add(chunk.len() as u64)
                        .ok_or_else(|| std::io::Error::other("content size overflow"))?;
                    if received > size {
                        return Err(std::io::Error::other("content exceeded its immutable size"));
                    }
                    if let Some(digest) = digest.as_mut() {
                        digest.update(&chunk);
                    }
                    if received == size
                        && let Some(finished) = digest.take()
                        && hex_digest(finished.finish()) != expected
                    {
                        return Err(std::io::Error::other("content digest mismatch"));
                    }
                    Ok(Some((chunk, (stream, received, digest))))
                }
            },
        );
        let mut output = axum::response::Response::new(axum::body::Body::from_stream(stream));
        let headers = output.headers_mut();
        use axum::http::{HeaderValue, header};
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            "x-zuno-content-sha256",
            HeaderValue::from_str(&hash).map_err(ApplicationError::storage)?,
        );
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&size.to_string()).map_err(ApplicationError::storage)?,
        );
        Ok(output)
    }
}

fn hex_digest(digest: aws_lc_rs::digest::Digest) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
