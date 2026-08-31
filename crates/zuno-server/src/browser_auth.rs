//! Loopback-only browser bootstrap and authority-bound session cookies.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::constant_time;
use aws_lc_rs::digest;
use aws_lc_rs::hmac;
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};
use axum::http::{HeaderMap, Method, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const KEY_BYTES: usize = 32;
const TOKEN_BYTES: usize = 32;
const COOKIE_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;

/// Process-local browser authentication state.
#[derive(Clone)]
pub(crate) struct BrowserAuth {
    inner: Arc<Inner>,
}

struct Inner {
    authority: String,
    origin: String,
    cookie_name: String,
    signing_key: hmac::Key,
    launch_token: Mutex<Option<[u8; TOKEN_BYTES]>>,
}

impl BrowserAuth {
    pub(crate) fn open(
        authority: String,
        key_path: &Path,
    ) -> Result<(Self, String), std::io::Error> {
        let signing_key = load_or_create_key(key_path)?;
        let mut launch_token = [0_u8; TOKEN_BYTES];
        SystemRandom::new()
            .fill(&mut launch_token)
            .map_err(|_| std::io::Error::other("could not generate browser launch token"))?;
        let authority_digest = digest::digest(&digest::SHA256, authority.as_bytes());
        let cookie_name = format!("zuno_browser_{}", hex_prefix(authority_digest.as_ref(), 12));
        let token = URL_SAFE_NO_PAD.encode(launch_token);
        let origin = format!("http://{authority}");
        let bootstrap_uri = format!("{origin}/auth/browser?token={token}");
        Ok((
            Self {
                inner: Arc::new(Inner {
                    authority,
                    origin,
                    cookie_name,
                    signing_key: hmac::Key::new(hmac::HMAC_SHA256, &signing_key),
                    launch_token: Mutex::new(Some(launch_token)),
                }),
            },
            bootstrap_uri,
        ))
    }

    pub(crate) fn exchange(&self, raw_query: Option<&str>) -> Option<String> {
        let mut tokens = raw_query
            .into_iter()
            .flat_map(|query| url::form_urlencoded::parse(query.as_bytes()))
            .filter_map(|(name, value)| (name == "token").then_some(value.into_owned()));
        let token = tokens.next()?;
        if tokens.next().is_some() {
            return None;
        }
        let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
        if decoded.len() != TOKEN_BYTES {
            return None;
        }
        let mut slot = self
            .inner
            .launch_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expected = slot.as_ref()?;
        constant_time::verify_slices_are_equal(expected, &decoded).ok()?;
        slot.take();
        drop(slot);
        Some(self.issue_cookie())
    }

    pub(crate) fn authorizes_cookie(&self, method: &Method, headers: &HeaderMap) -> bool {
        let Some(value) = unique_cookie(headers, &self.inner.cookie_name) else {
            return false;
        };
        if !self.verify_cookie(&value) {
            return false;
        }
        if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return true;
        }
        let mut origins = headers.get_all(header::ORIGIN).iter();
        let Some(origin) = origins.next().and_then(|value| value.to_str().ok()) else {
            return false;
        };
        origins.next().is_none() && origin == self.inner.origin
    }

    fn issue_cookie(&self) -> String {
        let expires = unix_seconds().saturating_add(COOKIE_LIFETIME_SECS);
        let payload = format!("{}\n{expires}", self.inner.authority);
        let signature = hmac::sign(&self.inner.signing_key, payload.as_bytes());
        let value = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload.as_bytes()),
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        );
        format!(
            "{}={value}; Max-Age={COOKIE_LIFETIME_SECS}; HttpOnly; SameSite=Strict; Path=/",
            self.inner.cookie_name
        )
    }

    fn verify_cookie(&self, value: &str) -> bool {
        let Some((payload, signature)) = value.split_once('.') else {
            return false;
        };
        let Ok(payload) = URL_SAFE_NO_PAD.decode(payload) else {
            return false;
        };
        let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
            return false;
        };
        if hmac::verify(&self.inner.signing_key, &payload, &signature).is_err() {
            return false;
        }
        let Ok(payload) = std::str::from_utf8(&payload) else {
            return false;
        };
        let Some((authority, expires)) = payload.split_once('\n') else {
            return false;
        };
        authority == self.inner.authority
            && expires
                .parse::<u64>()
                .is_ok_and(|expires| unix_seconds() <= expires)
    }
}

impl std::fmt::Debug for BrowserAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserAuth")
            .field("authority", &self.inner.authority)
            .field("cookie_name", &self.inner.cookie_name)
            .finish_non_exhaustive()
    }
}

fn unique_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().ok()?;
        for cookie in value.split(';') {
            let Some((candidate, value)) = cookie.trim().split_once('=') else {
                continue;
            };
            if candidate == name {
                if found.is_some() {
                    return None;
                }
                found = Some(value.to_owned());
            }
        }
    }
    found
}

fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES], std::io::Error> {
    if let Ok(key) = read_key(path) {
        return Ok(key);
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("browser auth key has no parent directory"))?;
    fs::create_dir_all(parent)?;
    set_directory_private(parent)?;
    let mut key = [0_u8; KEY_BYTES];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| std::io::Error::other("could not generate browser signing key"))?;
    match create_key(path, &key) {
        Ok(()) => Ok(key),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_key(path),
        Err(error) => Err(error),
    }
}

fn read_key(path: &Path) -> Result<[u8; KEY_BYTES], std::io::Error> {
    let mut file = File::open(path)?;
    let mut key = Vec::new();
    file.read_to_end(&mut key)?;
    let key: [u8; KEY_BYTES] = key.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "browser auth key must contain exactly 32 bytes",
        )
    })?;
    set_file_private(path)?;
    Ok(key)
}

fn create_key(path: &Path, key: &[u8; KEY_BYTES]) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("browser auth key has no parent directory"))?;
    let mut nonce = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| std::io::Error::other("could not generate browser key temporary name"))?;
    let temporary = parent.join(format!(
        ".browser-auth-key-{}.tmp",
        hex_prefix(&nonce, nonce.len() * 2)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let publish = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(key)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        set_file_private(path)?;
        sync_directory(parent)
    })();
    let cleanup = fs::remove_file(&temporary);
    match (publish, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) | (Err(error), _) => Err(error),
    }
}

#[cfg(unix)]
fn set_directory_private(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_private(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn set_file_private(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn hex_prefix(bytes: &[u8], digits: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(digits);
    for byte in bytes {
        if output.len() == digits {
            break;
        }
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        if output.len() == digits {
            break;
        }
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_token(uri: &str) -> &str {
        uri.split_once("?token=")
            .map(|(_, token)| token)
            .expect("bootstrap URI has one token")
    }

    fn cookie_header(set_cookie: &str) -> &str {
        set_cookie
            .split(';')
            .next()
            .expect("set-cookie has a name/value pair")
    }

    #[test]
    fn launch_token_is_single_use_and_cookie_requires_origin_for_mutation() {
        let temp = tempfile::tempdir().expect("browser auth directory");
        let (auth, uri) = BrowserAuth::open("127.0.0.1:4321".to_owned(), &temp.path().join("key"))
            .expect("browser auth");
        let token = launch_token(&uri);
        assert!(auth.exchange(None).is_none());
        assert!(
            auth.exchange(Some(&format!("token={token}&token={token}")))
                .is_none()
        );
        let set_cookie = auth
            .exchange(Some(&format!("token={token}")))
            .expect("valid token exchanges");
        assert!(auth.exchange(Some(&format!("token={token}"))).is_none());
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        assert!(set_cookie.contains("Path=/"));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            cookie_header(&set_cookie).parse().expect("cookie header"),
        );
        assert!(auth.authorizes_cookie(&Method::GET, &headers));
        assert!(!auth.authorizes_cookie(&Method::POST, &headers));
        headers.insert(
            header::ORIGIN,
            "http://127.0.0.1:4321".parse().expect("origin"),
        );
        assert!(auth.authorizes_cookie(&Method::POST, &headers));
        headers.insert(
            header::ORIGIN,
            "http://127.0.0.1:4322".parse().expect("origin"),
        );
        assert!(!auth.authorizes_cookie(&Method::POST, &headers));
    }

    #[test]
    fn signing_key_persists_but_cookie_is_bound_to_authority() {
        let temp = tempfile::tempdir().expect("browser auth directory");
        let path = temp.path().join("server/browser-auth.key");
        let (first, uri) =
            BrowserAuth::open("127.0.0.1:4321".to_owned(), &path).expect("first process");
        let cookie = first
            .exchange(Some(&format!("token={}", launch_token(&uri))))
            .expect("exchange");
        let cookie = cookie_header(&cookie);

        let (same_authority, _) =
            BrowserAuth::open("127.0.0.1:4321".to_owned(), &path).expect("restart");
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, cookie.parse().expect("cookie"));
        assert!(same_authority.authorizes_cookie(&Method::GET, &headers));

        let (other_authority, _) =
            BrowserAuth::open("127.0.0.1:4322".to_owned(), &path).expect("other authority");
        assert!(!other_authority.authorizes_cookie(&Method::GET, &headers));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path)
                    .expect("key metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn expired_or_tampered_cookie_is_rejected() {
        let temp = tempfile::tempdir().expect("browser auth directory");
        let (auth, _) = BrowserAuth::open("127.0.0.1:4321".to_owned(), &temp.path().join("key"))
            .expect("browser auth");
        let payload = b"127.0.0.1:4321\n1";
        let signature = hmac::sign(&auth.inner.signing_key, payload);
        let expired = format!(
            "{}={}.{}",
            auth.inner.cookie_name,
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, expired.parse().expect("cookie"));
        assert!(!auth.authorizes_cookie(&Method::GET, &headers));

        let tampered = format!("{}=invalid.invalid", auth.inner.cookie_name);
        headers.insert(header::COOKIE, tampered.parse().expect("cookie"));
        assert!(!auth.authorizes_cookie(&Method::GET, &headers));
    }

    #[test]
    fn concurrent_initialization_publishes_one_complete_signing_key() {
        let temp = tempfile::tempdir().expect("browser auth directory");
        let path = Arc::new(temp.path().join("server/browser-auth.key"));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                BrowserAuth::open("127.0.0.1:4321".to_owned(), &path)
                    .expect("concurrent browser auth")
            }));
        }
        let (first, first_uri) = threads
            .remove(0)
            .join()
            .expect("first initializer did not panic");
        let (second, _) = threads
            .remove(0)
            .join()
            .expect("second initializer did not panic");
        let cookie = first
            .exchange(Some(&format!("token={}", launch_token(&first_uri))))
            .expect("first token exchanges");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            cookie_header(&cookie).parse().expect("cookie"),
        );
        assert!(
            second.authorizes_cookie(&Method::GET, &headers),
            "both processes must load the same atomically-published key"
        );
        assert_eq!(
            fs::metadata(path.as_ref())
                .expect("published key metadata")
                .len(),
            KEY_BYTES as u64
        );
    }
}
