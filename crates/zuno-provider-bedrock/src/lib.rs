//! Amazon Bedrock Mantle/Runtime Responses and Converse/EventStream transports.

mod aws;
mod error;
mod eventstream;
mod provider;
mod responses;

pub use error::{PROVIDER_ID, classify_bedrock_error, classify_bedrock_error_for};
pub use eventstream::{
    BedrockDecodeError, BedrockEventDecoder, BedrockPayloadError, CrcKind, EventStreamDecoder,
    EventStreamError, EventStreamMessage, HeaderValue,
};
pub use provider::{
    BedrockBuildError, BedrockConfig, BedrockOperation, BedrockProvider, CONVERSE_PROVIDER_ID,
    factory, mantle_surface,
};
pub use responses::{
    BedrockResponsesBuildError, BedrockResponsesConfig, BedrockResponsesEndpoint,
    BedrockResponsesProvider, MANTLE_PROVIDER_ID, MANTLE_SUPPORTED_REGIONS, RUNTIME_PROVIDER_ID,
    mantle_factory, runtime_factory,
};
