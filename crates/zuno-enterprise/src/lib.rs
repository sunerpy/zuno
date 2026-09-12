//! Enterprise composition roots. Runtime business state machines remain in the
//! shared kernel and backend components.

mod children;
pub mod config;
mod http;
mod identity;
pub use http::serve as serve_tls;
pub mod profile;
pub mod service;
pub mod web;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid enterprise configuration: {0}")]
    Configuration(String),
    #[error("enterprise service I/O failed")]
    Io(#[from] std::io::Error),
    #[error("enterprise state service failed")]
    Application(#[from] zuno_application::ApplicationError),
    #[error("enterprise identity configuration failed")]
    Identity(#[from] zuno_identity::IdentityError),
    #[error("enterprise browser configuration failed")]
    Login(#[from] zuno_identity::login::LoginError),
    #[error("enterprise Worker failed")]
    Worker(#[from] zuno_worker::runtime::WorkerError),
    #[error("enterprise profile lifecycle failed")]
    Runtime(#[from] zuno_runtime::RuntimeError),
}

pub(crate) fn invalid(message: &str) -> Error {
    Error::Configuration(message.to_owned())
}
