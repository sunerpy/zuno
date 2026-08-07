//! Process-local HTTP server discovery for standalone maintenance clients.
//!
//! Servers bind ephemeral ports by default, so a separate CLI cannot infer the
//! active-session endpoint. Each loopback listener therefore owns one small URL
//! file for exactly its lifetime. Crashed processes may leave stale files; callers
//! must still connect and validate the endpoint before treating it as reachable.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const REGISTRY_DIRECTORY: &str = "servers";

#[derive(Debug)]
pub(crate) struct LocalServerRegistration {
    path: PathBuf,
}

impl Drop for LocalServerRegistration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn register(address: SocketAddr) -> io::Result<Option<LocalServerRegistration>> {
    if !address.ip().is_loopback() {
        return Ok(None);
    }
    register_in(oc_paths::state(), address).map(Some)
}

/// Returns candidate loopback base URLs in stable order.
///
/// A URL file is discovery evidence only. It may be stale, and consumers must
/// successfully call `/api/session/active` before claiming the server is reachable.
#[must_use]
pub fn local_server_urls() -> Vec<String> {
    read_from(oc_paths::state())
}

fn register_in(root: &Path, address: SocketAddr) -> io::Result<LocalServerRegistration> {
    let directory = root.join(REGISTRY_DIRECTORY);
    std::fs::create_dir_all(&directory)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = directory.join(format!(
        "{}-{}-{}.url",
        std::process::id(),
        address.port(),
        nonce
    ));
    let url = format!("http://{address}");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(url.as_bytes())?;
    file.sync_all()?;
    Ok(LocalServerRegistration { path })
}

fn read_from(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join(REGISTRY_DIRECTORY)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| validated_loopback_url(raw.trim()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validated_loopback_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    if url.scheme() != "http"
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.username() != ""
        || url.password().is_some()
        || url.port().is_none()
    {
        return None;
    }
    let ip: IpAddr = url.host_str()?.parse().ok()?;
    ip.is_loopback().then(|| raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_visible_only_for_its_lifetime() {
        let root = tempfile::tempdir().expect("temporary state root");
        let registration = register_in(root.path(), "127.0.0.1:43125".parse().expect("address"))
            .expect("register loopback endpoint");
        assert_eq!(read_from(root.path()), ["http://127.0.0.1:43125"]);
        drop(registration);
        assert!(read_from(root.path()).is_empty());
    }

    #[test]
    fn discovery_rejects_non_loopback_or_structured_urls() {
        for raw in [
            "https://127.0.0.1:1",
            "http://192.0.2.1:1",
            "http://127.0.0.1:1/path",
            "http://user@127.0.0.1:1",
            "not a URL",
        ] {
            assert_eq!(validated_loopback_url(raw), None, "{raw}");
        }
    }
}
