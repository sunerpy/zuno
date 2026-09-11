use futures::StreamExt as _;
use reqwest::{Client, Method};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use zuno_application::ApplicationError;
use zuno_application::environment::{OperationOutput, OutputChannel, OutputCursor, OutputPage};

#[derive(Clone)]
pub(crate) struct Docker {
    client: Client,
}
impl Docker {
    #[cfg(target_os = "linux")]
    pub(crate) async fn connect(socket: &Path) -> Result<Self, ApplicationError> {
        use std::os::unix::fs::FileTypeExt;
        let metadata = std::fs::symlink_metadata(socket).map_err(super::storage)?;
        if !metadata.file_type().is_socket() {
            return Err(ApplicationError::Invalid(
                "execution backend requires a Unix socket".to_owned(),
            ));
        }
        let client = zuno_network::client_builder()
            .unix_socket(socket.to_owned())
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(super::storage)?;
        let docker = Self { client };
        let info = docker.json(Method::GET, "/info", None).await?;
        let rootless = info
            .get("SecurityOptions")
            .and_then(Value::as_array)
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|value| value.as_str() == Some("name=rootless"))
            });
        if !rootless
            || info.get("CgroupDriver").and_then(Value::as_str) != Some("systemd")
            || info.get("MemoryLimit").and_then(Value::as_bool) != Some(true)
            || info.get("PidsLimit").and_then(Value::as_bool) != Some(true)
            || info.get("CpuCfsQuota").and_then(Value::as_bool) != Some(true)
        {
            return Err(ApplicationError::Invalid(
                "rootless Docker with enforced memory, CPU and PID limits is required".to_owned(),
            ));
        }
        Ok(docker)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) async fn connect(_socket: &Path) -> Result<Self, ApplicationError> {
        Err(ApplicationError::Invalid(
            "the rootless Docker gateway requires Linux".to_owned(),
        ))
    }

    pub(crate) async fn bytes(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        limit: usize,
    ) -> Result<Vec<u8>, ApplicationError> {
        let response = self.response(method, path, body).await?;
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ApplicationError::Unavailable)?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(ApplicationError::Invalid(
                    "Docker response exceeds the configured bound".to_owned(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn response(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<reqwest::Response, ApplicationError> {
        let mut request = self
            .client
            .request(method, format!("http://localhost{path}"));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        match response.status().as_u16() {
            200..=299 | 304 => {}
            404 => return Err(ApplicationError::NotFound),
            409 => return Err(ApplicationError::Conflict),
            500..=599 => return Err(ApplicationError::Unavailable),
            _ => {
                return Err(ApplicationError::Invalid(
                    "Docker rejected the bounded operation".to_owned(),
                ));
            }
        }
        Ok(response)
    }

    pub(crate) async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApplicationError> {
        let bytes = self.bytes(method, path, body, 4 * 1024 * 1024).await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(super::storage)
    }

    pub(crate) async fn log_page(
        &self,
        path: &str,
        cursor: OutputCursor,
        maximum: u32,
    ) -> Result<OutputPage, ApplicationError> {
        if !(1..=1024 * 1024).contains(&maximum)
            || (cursor.offset > 0
                && cursor.prefix_sha256.as_ref().is_none_or(|digest| {
                    digest.len() != 64
                        || !digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                }))
        {
            return Err(ApplicationError::Invalid(
                "invalid output page cursor or size".to_owned(),
            ));
        }
        let response = self.response(Method::GET, path, None).await?;
        let stream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
        let mut reader = tokio_util::io::StreamReader::new(stream);
        let mut digest = Sha256::new();
        let mut position = 0u64;
        let mut retained = 0usize;
        let mut chunks = Vec::new();
        let mut validated = cursor.offset == 0;
        loop {
            let mut header = [0u8; 8];
            let read = reader
                .read(&mut header[..1])
                .await
                .map_err(super::storage)?;
            if read == 0 {
                if position < cursor.offset || !validated {
                    return Err(ApplicationError::Conflict);
                }
                return Ok(OutputPage {
                    chunks,
                    next: OutputCursor {
                        offset: position,
                        prefix_sha256: Some(hex::encode(digest.finalize())),
                    },
                    end_of_available: true,
                });
            }
            reader
                .read_exact(&mut header[1..])
                .await
                .map_err(super::storage)?;
            if header[1..4] != [0, 0, 0] || !matches!(header[0], 1 | 2) {
                return Err(ApplicationError::Invalid(
                    "invalid Docker output frame".to_owned(),
                ));
            }
            let mut remaining =
                u32::from_be_bytes(header[4..8].try_into().expect("four-byte length")) as usize;
            let channel = if header[0] == 1 {
                OutputChannel::Stdout
            } else {
                OutputChannel::Stderr
            };
            let mut buffer = [0u8; 8192];
            while remaining > 0 {
                let size = remaining.min(buffer.len());
                reader
                    .read_exact(&mut buffer[..size])
                    .await
                    .map_err(super::storage)?;
                remaining -= size;
                let mut part = Vec::new();
                for byte in &buffer[..size] {
                    if position == cursor.offset && !validated {
                        if cursor.prefix_sha256.as_deref()
                            != Some(hex::encode(digest.clone().finalize()).as_str())
                        {
                            return Err(ApplicationError::Conflict);
                        }
                        validated = true;
                    }
                    if position >= cursor.offset && retained >= maximum as usize {
                        if !part.is_empty() {
                            chunks.push(OperationOutput {
                                channel: channel.clone(),
                                bytes: part,
                            });
                        }
                        return Ok(OutputPage {
                            chunks,
                            next: OutputCursor {
                                offset: position,
                                prefix_sha256: Some(hex::encode(digest.finalize())),
                            },
                            end_of_available: false,
                        });
                    }
                    digest.update([header[0], *byte]);
                    position = position.checked_add(1).ok_or(ApplicationError::Conflict)?;
                    if position > cursor.offset {
                        part.push(*byte);
                        retained += 1;
                    }
                }
                if !part.is_empty() {
                    chunks.push(OperationOutput {
                        channel: channel.clone(),
                        bytes: part,
                    });
                }
                if position == cursor.offset && !validated {
                    if cursor.prefix_sha256.as_deref()
                        != Some(hex::encode(digest.clone().finalize()).as_str())
                    {
                        return Err(ApplicationError::Conflict);
                    }
                    validated = true;
                }
            }
        }
    }

    pub(crate) async fn download_archive(
        &self,
        path: &str,
        destination: &Path,
        maximum: u64,
    ) -> Result<(String, u64), ApplicationError> {
        use tokio::io::AsyncWriteExt as _;
        let response = self.response(Method::GET, path, None).await?;
        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)
            .await
            .map_err(super::storage)?;
        let mut digest = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ApplicationError::Unavailable)?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or(ApplicationError::Conflict)?;
            if size > maximum {
                return Err(ApplicationError::Invalid(
                    "workspace snapshot exceeds its byte limit".to_owned(),
                ));
            }
            digest.update(&chunk);
            file.write_all(&chunk).await.map_err(super::storage)?;
        }
        file.sync_all().await.map_err(super::storage)?;
        Ok((hex::encode(digest.finalize()), size))
    }

    pub(crate) async fn upload_archive(
        &self,
        path: &str,
        source: &Path,
        size: u64,
    ) -> Result<(), ApplicationError> {
        let file = tokio::fs::File::open(source)
            .await
            .map_err(super::storage)?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let response = self
            .client
            .put(format!("http://localhost{path}"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-tar")
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(body)
            .send()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        if !response.status().is_success() {
            return Err(ApplicationError::Unavailable);
        }
        Ok(())
    }
}
