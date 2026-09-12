//! Immutable, bounded browser assets loaded only from an operator-selected
//! deployment directory. Request paths never cause filesystem access.

use crate::{Error, invalid};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::io::AsyncReadExt as _;

#[derive(Clone)]
struct Asset {
    body: Bytes,
    mime: &'static str,
    etag: String,
}
#[derive(Clone)]
pub struct WebAssets {
    assets: Arc<BTreeMap<String, Asset>>,
}

impl WebAssets {
    pub async fn load(root: &Path) -> Result<Self, Error> {
        if !root.is_absolute() {
            return Err(invalid("webAssetsDirectory must be absolute"));
        }
        let metadata = tokio::fs::symlink_metadata(root).await?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid("webAssetsDirectory must be a real directory"));
        }
        let mut queue = vec![(root.to_owned(), String::new())];
        let mut assets = BTreeMap::new();
        let mut total = 0u64;
        let mut directories = 0usize;
        while let Some((path, prefix)) = queue.pop() {
            directories += 1;
            if directories > 32 {
                return Err(invalid("too many Web asset directories"));
            }
            let mut entries = tokio::fs::read_dir(path).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid("Web asset names must be UTF-8"))?;
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                    || name == "."
                    || name == ".."
                {
                    return Err(invalid("invalid Web asset name"));
                }
                let key = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                let kind = entry.file_type().await?;
                if kind.is_symlink() {
                    return Err(invalid("Web asset symlinks are not allowed"));
                }
                if kind.is_dir() {
                    queue.push((entry.path(), key));
                    continue;
                }
                if !kind.is_file() {
                    return Err(invalid("Web assets must be regular files"));
                }
                let Some(mime) = mime(&key) else {
                    return Err(invalid("unsupported Web asset type"));
                };
                if key.ends_with(".html") && key != "index.html" {
                    return Err(invalid("only index.html may be served as Web HTML"));
                }
                let file = tokio::fs::File::open(entry.path()).await?;
                let metadata = file.metadata().await?;
                if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 {
                    return Err(invalid("Web asset exceeds its bound"));
                }
                let mut bytes = Vec::new();
                file.take(8 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                if bytes.len() > 8 * 1024 * 1024 {
                    return Err(invalid("Web asset grew beyond its bound"));
                }
                total += bytes.len() as u64;
                if total > 32 * 1024 * 1024 || assets.len() >= 128 {
                    return Err(invalid("Web bundle exceeds its bound"));
                }
                let digest = Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let etag = format!("\"{digest}\"");
                assets.insert(
                    key,
                    Asset {
                        body: Bytes::from(bytes),
                        mime,
                        etag,
                    },
                );
            }
        }
        if !assets.contains_key("index.html") {
            return Err(invalid("Web bundle is missing index.html"));
        }
        Ok(Self {
            assets: Arc::new(assets),
        })
    }
    pub fn router(self) -> Router {
        Router::new()
            .route("/", get(|| async { Redirect::temporary("/app/") }))
            .route("/app", get(|| async { Redirect::temporary("/app/") }))
            .route("/app/", get(index))
            .route("/app/assets/{*path}", get(asset))
            .with_state(self)
    }
    fn response(&self, key: &str, headers: &HeaderMap) -> Response {
        let Some(asset) = self.assets.get(key) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let unchanged = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            == Some(asset.etag.as_str());
        let mut response=Response::builder().status(if unchanged {StatusCode::NOT_MODIFIED} else {StatusCode::OK})
            .header(header::CONTENT_TYPE,asset.mime)
            .header(header::ETAG,&asset.etag)
            .header(header::CACHE_CONTROL,"no-cache")
            .header(header::X_CONTENT_TYPE_OPTIONS,"nosniff")
            .header(header::REFERRER_POLICY,"no-referrer")
            .header(header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'");
        if !unchanged {
            response = response.header(header::CONTENT_LENGTH, asset.body.len());
        }
        response
            .body(if unchanged {
                Body::empty()
            } else {
                Body::from(asset.body.clone())
            })
            .expect("static headers")
    }
}
fn mime(path: &str) -> Option<&'static str> {
    Some(match path.rsplit('.').next()? {
        "html" => "text/html; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => return None,
    })
}
async fn index(State(assets): State<WebAssets>, headers: HeaderMap) -> Response {
    assets.response("index.html", &headers)
}
async fn asset(
    State(assets): State<WebAssets>,
    RoutePath(path): RoutePath<String>,
    headers: HeaderMap,
) -> Response {
    if path.split('/').any(|part| {
        part.is_empty()
            || part == "."
            || part == ".."
            || !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }) {
        return StatusCode::NOT_FOUND.into_response();
    }
    assets.response(&format!("assets/{path}"), &headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn assets_are_frozen_and_browser_headers_do_not_allow_script_injection() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("index.html"), "<html>preview</html>")
            .await
            .unwrap();
        let assets = WebAssets::load(dir.path()).await.unwrap();
        tokio::fs::write(dir.path().join("index.html"), "changed")
            .await
            .unwrap();
        let response = assets.response("index.html", &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("unsafe-inline")
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            response.headers()[header::ETAG].clone(),
        );
        assert_eq!(
            assets.response("index.html", &headers).status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            assets.response("../index.html", &headers).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            "<html>preview</html>"
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn assets_cannot_follow_a_symlink_out_of_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        tokio::fs::write(outside.path().join("secret.html"), "private")
            .await
            .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.html"),
            dir.path().join("index.html"),
        )
        .unwrap();
        assert!(WebAssets::load(dir.path()).await.is_err());
    }
}
