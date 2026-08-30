//! Amazon Bedrock: SigV4 signing and the binary EventStream framing.

mod credentials;
mod error;
mod eventstream;
mod provider;
mod sigv4;

pub use credentials::{
    CREDENTIAL_CHAIN_ORDER, CredentialChainConfig, CredentialError, CredentialResolver,
    CredentialSource, ResolvedCredentials,
};
pub use error::{PROVIDER_ID, classify_bedrock_error};
pub use eventstream::{
    BedrockDecodeError, BedrockEventDecoder, BedrockPayloadError, CrcKind, EventStreamDecoder,
    EventStreamError, EventStreamMessage, HeaderValue,
};
pub use provider::{
    BedrockBuildError, BedrockConfig, BedrockOperation, BedrockProvider, factory, mantle_surface,
};
pub use sigv4::{AwsCredentials, SigV4Error, SigV4Signer, SigningOutput};
