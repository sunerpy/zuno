use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ContinuityError;

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<String, ContinuityError> {
    serde_json::to_vec(value)
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .map_err(ContinuityError::Encoding)
}

pub(crate) fn decode<T: DeserializeOwned>(
    value: &str,
    description: &str,
) -> Result<T, ContinuityError> {
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| {
        ContinuityError::Invalid(format!("{description} is not a valid continuity token"))
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        ContinuityError::Invalid(format!("{description} is not a valid continuity token"))
    })
}
