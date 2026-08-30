//! Per-request project selection used by Zuno clients.
//!
//! The observable names are `x-zuno-directory` and `directory`. Query wins over
//! header, then the server's startup directory is the fallback. Values are
//! URI-decoded once after extraction;
//! query parsing has already decoded its form layer, which is why the SDK's
//! rewritten `%252Fworkspace` query and its `%2Fworkspace` header converge.

use axum::http::{HeaderMap, Uri};

/// The directory selected for this request.
///
/// Handlers receive it with `Extension<RequestDirectory>`. The string is not
/// canonicalized: relative paths and symlinks remain observable to the instance
/// loader just as they are in the upstream request contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestDirectory(String);

impl RequestDirectory {
    /// Borrows the selected spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Takes the selected spelling.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

pub(crate) fn resolve(uri: &Uri, headers: &HeaderMap, fallback: &str) -> RequestDirectory {
    let from_query = uri.query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(name, value)| name == "directory" && !value.is_empty())
            .map(|(_, value)| decode_component(&value))
    });
    let from_header = headers
        .get("x-zuno-directory")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(decode_component);
    RequestDirectory(
        from_query
            .or(from_header)
            .unwrap_or_else(|| fallback.to_owned()),
    )
}

fn decode_component(input: &str) -> String {
    if !input.as_bytes().contains(&b'%') {
        return input.to_owned();
    }

    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let Some(high) = bytes.get(index + 1).and_then(|byte| hex(*byte)) else {
            return input.to_owned();
        };
        let Some(low) = bytes.get(index + 2).and_then(|byte| hex(*byte)) else {
            return input.to_owned();
        };
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).unwrap_or_else(|_| input.to_owned())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_percent_encoding_stays_literal() {
        assert_eq!(decode_component("/work/%ZZ"), "/work/%ZZ");
        assert_eq!(decode_component("/work/%FF"), "/work/%FF");
    }

    #[test]
    fn header_decoding_does_not_turn_a_plus_into_a_space() {
        assert_eq!(decode_component("/work/a+b"), "/work/a+b");
    }
}
