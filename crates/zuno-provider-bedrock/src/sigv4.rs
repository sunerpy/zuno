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
            // Normalized like every other header value, not merely because SigV4 folds
            // header whitespace, but because the redaction below is line-oriented: a value
            // that still carried a newline would put its own tail on the next canonical
            // line, where nothing named `x-amz-security-token` prefixes it and the
            // redactor cannot see it. One call makes the single-line shape a property of
            // the value rather than an assumption about the issuer's format.
            canonical.insert(
                "x-amz-security-token".to_owned(),
                normalize_header_value(token),
            );
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
            canonical_request: CanonicalRequest(canonical_request),
            string_to_sign,
            signature: Redacted(signature),
            authorization: Redacted(authorization),
            signed_headers,
            payload_hash,
            headers: SigningHeaders(output_headers),
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SigningOutput {
    /// The canonical request, whose own rendering hides the session-token line.
    pub canonical_request: CanonicalRequest,
    /// `AWS4-HMAC-SHA256`, the timestamp, the credential scope, and a digest of the
    /// canonical request. Carries no credential material, so it renders in full: the
    /// scope is a date, a region and a service name, and the digest is one-way.
    pub string_to_sign: String,
    pub signature: Redacted,
    pub authorization: Redacted,
    pub signed_headers: String,
    pub payload_hash: String,
    /// The headers to send, whose own rendering hides both credential-bearing values.
    pub headers: SigningHeaders,
}

/// Redacted like [`AwsCredentials`], for the same reason: `headers` carries the
/// temporary `x-amz-security-token` and the `authorization` line carries the request
/// signature, so a derived `Debug` would put a live AWS credential into any log line or
/// error chain that formats a value embedding this one.
///
/// A redaction that lives only here covers `{signing:?}` and nothing else. The three
/// credential-bearing fields are typed so that formatting *one field* —
/// `tracing::debug!(canonical = %signing.canonical_request)`, the exact line this struct
/// invites — is redacted by the field's own `Display` and `Debug`.
impl fmt::Debug for SigningOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SigningOutput")
            .field("canonical_request", &self.canonical_request)
            .field("string_to_sign", &self.string_to_sign)
            .field("signature", &self.signature)
            .field("authorization", &self.authorization)
            .field("signed_headers", &self.signed_headers)
            .field("payload_hash", &self.payload_hash)
            .field("headers", &self.headers)
            .finish()
    }
}

/// The headers a signed request carries, with a rendering that hides the credentials.
///
/// A plain `BTreeMap` field was redacted by [`SigningOutput`]'s own `Debug` and by nothing
/// else, so `tracing::debug!(headers = ?signing.headers)` — the natural line to add while
/// debugging a 403 — wrote the live `x-amz-security-token` and the whole
/// `AWS4-HMAC-SHA256 Credential=…, Signature=…` line to the log. The values are still
/// reachable, but only by iterating them into a request, which is what the sink they are
/// for looks like.
#[derive(Clone, PartialEq, Eq)]
pub struct SigningHeaders(BTreeMap<String, String>);

impl SigningHeaders {
    /// The value of one header, for a byte-exact assertion against a known answer.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Every header name and value, in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl IntoIterator for SigningHeaders {
    type Item = (String, String);
    type IntoIter = std::collections::btree_map::IntoIter<String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl fmt::Debug for SigningHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&RedactedHeaders(&self.0), formatter)
    }
}

/// The canonical header line that carries a temporary credential.
const SESSION_TOKEN_HEADER: &str = "x-amz-security-token:";

/// A signing artifact that renders as `<redacted>` however it is formatted.
///
/// The signature is a MAC over the request derived from the secret key, and the
/// `Authorization` line carries both it and the access key id. Neither belongs in a log,
/// and both are `String`-shaped, which is exactly the shape that gets interpolated into
/// one. The plaintext is still reachable, but only through a call that says so.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(String);

impl Redacted {
    /// The plaintext value. Never hand this to a logging or error-rendering sink.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The plaintext bytes, for a byte-exact comparison against a known answer.
    #[must_use]
    pub fn expose_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl fmt::Display for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// The canonical request, minus the one line that carries a credential.
///
/// Redacting the whole value would be self-defeating: reading this string is how a
/// signature mismatch gets diagnosed, so a blanket `<redacted>` would push the next
/// debugger straight to [`expose`](Self::expose) and put the session token in the log
/// anyway. Only the `x-amz-security-token` header line is replaced; the method, path,
/// query, header set, signed-header list and payload hash — everything that explains a
/// mismatch — render verbatim.
#[derive(Clone, PartialEq, Eq)]
pub struct CanonicalRequest(String);

impl CanonicalRequest {
    /// The exact bytes that were signed, session token included.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The signed bytes, for a byte-exact comparison against a known answer.
    #[must_use]
    pub fn expose_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// The canonical request with the session token replaced.
    ///
    /// Splits on `\n` rather than iterating [`str::lines`] so the result is the input
    /// byte for byte wherever no token line is present, including a trailing newline.
    #[must_use]
    pub fn redacted(&self) -> String {
        self.0
            .split('\n')
            .map(|line| {
                if line.starts_with(SESSION_TOKEN_HEADER) {
                    format!("{SESSION_TOKEN_HEADER}<redacted>")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl fmt::Debug for CanonicalRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.redacted(), formatter)
    }
}

impl fmt::Display for CanonicalRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted())
    }
}

/// The canonical header map with every credential-bearing value replaced.
struct RedactedHeaders<'headers>(&'headers BTreeMap<String, String>);

impl fmt::Debug for RedactedHeaders<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_map()
            .entries(self.0.iter().map(|(name, value)| {
                let rendered: &dyn fmt::Debug =
                    if matches!(name.as_str(), "authorization" | "x-amz-security-token") {
                        &"<redacted>"
                    } else {
                        value
                    };
                (name, rendered)
            }))
            .finish()
    }
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

    /// `SigningOutput` is `pub` and re-exported, so one added `tracing::debug!(?signing)`
    /// would otherwise put a live AWS session token and the request signature verbatim
    /// into a log line or a persisted error chain.
    #[test]
    fn signing_output_never_renders_the_session_token_or_signature() {
        let credentials = AwsCredentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG")
            .with_session_token("FwoGZXIvYXdzEExampleSessionToken");
        let signing = SigV4Signer::new("us-east-1", "bedrock")
            .sign(
                "POST",
                &Url::parse(
                    "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse-stream",
                )
                .expect("static URL"),
                &BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
                b"{}",
                &credentials,
                "20260904T000000Z",
            )
            .expect("signing succeeds");

        // Every way a caller can format one of these values, not just `{signing:?}`:
        // a redacting `Debug` on the struct does nothing for
        // `tracing::debug!(canonical = %signing.canonical_request)`, which formats the
        // field itself.
        for rendered in [
            format!("{signing:?}"),
            format!("{signing:#?}"),
            format!("{signing}", signing = signing.canonical_request),
            format!("{:?}", signing.canonical_request),
            format!("{}", signing.signature),
            format!("{:?}", signing.signature),
            format!("{}", signing.authorization),
            format!("{:?}", signing.authorization),
            // The header map is a public field and the natural thing to log while
            // debugging a 403, so its own rendering has to be redacted too — the enclosing
            // struct's `Debug` never runs for `?signing.headers`.
            format!("{:?}", signing.headers),
            format!("{:#?}", signing.headers),
            signing.string_to_sign.clone(),
        ] {
            assert!(
                !rendered.contains("FwoGZXIvYXdzEExampleSessionToken"),
                "the temporary session token must never be rendered: {rendered}"
            );
            assert!(
                !rendered.contains(signing.signature.expose()),
                "the request signature must never be rendered: {rendered}"
            );
            assert!(
                !rendered.contains("AWS4-HMAC-SHA256 Credential="),
                "the authorization line must never be rendered: {rendered}"
            );
        }

        // The token is redacted in the rendering, not dropped from the signature: the
        // signed bytes still carry it, and the rendered form still carries everything a
        // signature mismatch is diagnosed with.
        let signed_bytes = signing.canonical_request.expose();
        assert!(
            signed_bytes.contains("x-amz-security-token:FwoGZXIvYXdzEExampleSessionToken"),
            "the canonical request must still sign the token: {signed_bytes}"
        );
        let diagnostic = signing.canonical_request.to_string();
        assert!(
            diagnostic.contains("x-amz-security-token:<redacted>")
                && diagnostic.contains("content-type:application/json")
                && diagnostic.contains("x-amz-date:20260904T000000Z")
                && diagnostic.ends_with(&signing.payload_hash),
            "only the credential line is replaced: {diagnostic}"
        );
    }

    /// A session token that is not one line.
    ///
    /// The canonical-request redactor is line-oriented, so a value carrying a newline used
    /// to leave its tail on a line no prefix matched — redaction that depended on the
    /// issuer's format. The value is folded at the insert site instead, so the single-line
    /// shape is a property of what was signed.
    #[test]
    fn a_session_token_containing_a_newline_is_folded_before_it_is_signed() {
        let credentials = AwsCredentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG")
            .with_session_token("FwoGZXIvYXdzEExample\nTAIL_SECRET");
        let signing = SigV4Signer::new("us-east-1", "bedrock")
            .sign(
                "POST",
                &Url::parse(
                    "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse-stream",
                )
                .expect("static URL"),
                &BTreeMap::new(),
                b"{}",
                &credentials,
                "20260904T000000Z",
            )
            .expect("signing succeeds");

        assert!(
            signing
                .canonical_request
                .expose()
                .contains("x-amz-security-token:FwoGZXIvYXdzEExample TAIL_SECRET"),
            "the value is folded onto one canonical line: {}",
            signing.canonical_request.expose()
        );
        for rendered in [
            signing.canonical_request.to_string(),
            format!("{:?}", signing.canonical_request),
            format!("{signing:?}"),
            format!("{:?}", signing.headers),
        ] {
            assert!(
                !rendered.contains("TAIL_SECRET"),
                "no rendering may carry any part of the token: {rendered}"
            );
        }
    }

    #[test]
    fn hmac_matches_rfc_4231_case_one() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}
