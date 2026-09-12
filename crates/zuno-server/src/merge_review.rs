//! Public review downloads use a separate read-only capability. They need no
//! active Agent lease and never expose a Worker credential to the browser.
use crate::gateway_configuration::GatewayConfigurationResolver;
use futures::StreamExt;
use std::sync::Arc;
use zuno_application::{ApplicationError, workspace_merge::MergeContentRequest};
use zuno_identity::gateway::GatewayTicketAuthority;
use zuno_postgres::PostgresBackend;
use zuno_types::identity::PrincipalScope;

pub struct MergeReviewReader {
    backend: PostgresBackend,
    tickets: Arc<GatewayTicketAuthority>,
    assignments: Arc<dyn GatewayConfigurationResolver>,
    client: reqwest::Client,
}
impl MergeReviewReader {
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
