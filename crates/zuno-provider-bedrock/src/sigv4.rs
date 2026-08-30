use std::collections::BTreeMap;
use std::fmt;

use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use url::Url;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) session_token: Option<String>,
    pub(crate) expires_at: Option<OffsetDateTime>,
}

impl AwsCredentials {
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
            expires_at: None,
        }
    }

    #[must_use]
    pub fn with_session_token(mut self, session_token: impl Into<String>) -> Self {
        self.session_token = Some(session_token.into());
        self
    }

    #[must_use]
    pub fn with_expiration(mut self, expires_at: OffsetDateTime) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    #[must_use]
    pub fn expires_at(&self) -> Option<OffsetDateTime> {
        self.expires_at
    }
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AwsCredentials")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct SigV4Signer {
    region: String,
    service: String,
}

impl SigV4Signer {
    #[must_use]
    pub fn new(region: impl Into<String>, service: impl Into<String>) -> Self {
        Self {
            region: region.into(),
            service: service.into(),
        }
    }

    pub fn sign(
        &self,
        method: &str,
        url: &Url,
        headers: &BTreeMap<String, String>,
        body: &[u8],
        credentials: &AwsCredentials,
        amz_date: &str,
    ) -> Result<SigningOutput, SigV4Error> {
        validate_amz_date(amz_date)?;
        let host = canonical_host(url)?;
        let payload_hash = sha256_hex(body);
        let mut canonical = BTreeMap::<String, String>::new();
        canonical.insert("host".to_owned(), host);
        canonical.insert("x-amz-date".to_owned(), amz_date.to_owned());
        if let Some(token) = &credentials.session_token {
            canonical.insert("x-amz-security-token".to_owned(), token.clone());
        }
        for (name, value) in headers {
            let name = name.to_ascii_lowercase();
            if name == "authorization" || name == "host" || name == "x-amz-date" {
                continue;
            }
            let value = normalize_header_value(value);
            canonical
                .entry(name)
                .and_modify(|current| {
                    current.push(',');
                    current.push_str(&value);
                })
                .or_insert(value);
        }

        let signed_headers = canonical.keys().cloned().collect::<Vec<_>>().join(";");
        let canonical_headers = canonical
            .iter()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect::<String>();
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.to_ascii_uppercase(),
            canonical_uri(url.path()),
            canonical_query(url),
            canonical_headers,
            signed_headers,
            payload_hash,
        );
        let date_stamp = &amz_date[..8];
        let credential_scope =
            format!("{date_stamp}/{}/{}/aws4_request", self.region, self.service);
        let string_to_sign = format!(
            "{ALGORITHM}\n{amz_date}\n{credential_scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let date_key = hmac_sha256(
            format!("AWS4{}", credentials.secret_access_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let region_key = hmac_sha256(&date_key, self.region.as_bytes());
        let service_key = hmac_sha256(&region_key, self.service.as_bytes());
        let signing_key = hmac_sha256(&service_key, b"aws4_request");
        let signature = hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
        let authorization = format!(
            "{ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        );

        let mut output_headers = canonical;
        output_headers.insert("authorization".to_owned(), authorization.clone());
        Ok(SigningOutput {
            canonical_request,
            string_to_sign,
            signature,
            authorization,
            signed_headers,
            payload_hash,
            headers: output_headers,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningOutput {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
    pub authorization: String,
    pub signed_headers: String,
    pub payload_hash: String,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SigV4Error {
    #[error("SigV4 timestamp must use YYYYMMDDTHHMMSSZ, got `{0}`")]
    InvalidTimestamp(String),
    #[error("SigV4 request URL has no host: {0}")]
    MissingHost(String),
}

fn validate_amz_date(value: &str) -> Result<(), SigV4Error> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 16
        && bytes[0..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'Z';
    if valid {
        Ok(())
    } else {
        Err(SigV4Error::InvalidTimestamp(value.to_owned()))
    }
}

fn canonical_host(url: &Url) -> Result<String, SigV4Error> {
    let raw = url
        .host_str()
        .ok_or_else(|| SigV4Error::MissingHost(url.to_string()))?;
    let host = if raw.contains(':') {
        format!("[{raw}]")
    } else {
        raw.to_ascii_lowercase()
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    path.split('/')
        .map(|segment| uri_encode(&percent_decode(segment.as_bytes()), true))
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_query(url: &Url) -> String {
    let mut pairs = url
        .query_pairs()
        .map(|(name, value)| {
            (
                uri_encode(name.as_bytes(), true),
                uri_encode(value.as_bytes(), true),
            )
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn normalize_header_value(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn percent_decode(value: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0usize;
    while index < value.len() {
        if value[index] == b'%'
            && index + 2 < value.len()
            && let (Some(high), Some(low)) =
                (hex_digit(value[index + 1]), hex_digit(value[index + 2]))
        {
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(value[index]);
            index += 1;
        }
    }
    decoded
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn uri_encode(value: &[u8], encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for &byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else if byte == b'/' && !encode_slash {
            encoded.push('/');
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

pub(crate) fn encode_path_segment(value: &str) -> String {
    uri_encode(value.as_bytes(), true)
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

fn sha256_hex(value: &[u8]) -> String {
    hex(&Sha256::digest(value))
}

fn hex(value: &[u8]) -> String {
    let mut output = String::with_capacity(value.len() * 2);
    for &byte in value {
        output.push(HEX_LOWER[(byte >> 4) as usize] as char);
        output.push(HEX_LOWER[(byte & 0x0f) as usize] as char);
    }
    output
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut normalized = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_SIZE];
    let mut outer_pad = [0x5cu8; BLOCK_SIZE];
    for index in 0..BLOCK_SIZE {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_hash);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encoding_preserves_path_boundaries_and_encoded_slashes() {
        assert_eq!(canonical_uri("/model/a%2Fb:c"), "/model/a%2Fb%3Ac");
        assert_eq!(canonical_uri("/test$file.text"), "/test%24file.text");
    }

    #[test]
    fn hmac_matches_rfc_4231_case_one() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}
