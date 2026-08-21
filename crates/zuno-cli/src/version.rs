//! Zuno package and build identities.

/// Cargo's package version for this Rust implementation.
pub const RUST_PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build identity shown to operators.
///
/// Release builds may inject a commit, tag, or reproducible-build identifier.
/// Local builds remain honest by falling back to the Rust package version.
pub const BUILD_ID: &str = match option_env!("ZUNO_RUST_BUILD_ID") {
    Some(identity) => identity,
    None => RUST_PACKAGE_VERSION,
};

/// The short operator-facing version.
#[must_use]
pub const fn version() -> &'static str {
    RUST_PACKAGE_VERSION
}

/// The operator-facing identity.
#[must_use]
pub fn long_version() -> String {
    format!("Zuno {BUILD_ID} (Rust package {RUST_PACKAGE_VERSION})")
}

/// The HTTP identity for this implementation.
#[must_use]
pub fn user_agent() -> String {
    format!("zuno/{RUST_PACKAGE_VERSION} (build {BUILD_ID})")
}
