//! The compatibility identity and the Rust build identity serve different peers.
//!
//! npm plugins inspect `engines.opencode` as a semver range and are skipped when
//! the running version does not satisfy it (`plugin/shared.ts:194-204`, called
//! from `plugin/loader.ts:123-130`). That peer must see the pinned compatibility
//! baseline. Operators and HTTP peers must instead be able to identify this Rust
//! build, so the long display and user agent never masquerade as the TypeScript
//! binary.

/// The version supplied to npm plugin compatibility checks.
pub const COMPATIBILITY_VERSION: &str = "1.18.13";

/// Cargo's package version for this Rust implementation.
pub const RUST_PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build identity shown to operators.
///
/// Release builds may inject a commit, tag, or reproducible-build identifier.
/// Local builds remain honest by falling back to the Rust package version.
pub const BUILD_ID: &str = match option_env!("OPENCODE_RUST_BUILD_ID") {
    Some(identity) => identity,
    None => RUST_PACKAGE_VERSION,
};

/// The short version intentionally matches the plugin compatibility baseline.
#[must_use]
pub const fn compatibility_version() -> &'static str {
    COMPATIBILITY_VERSION
}

/// The operator-facing identity, including both identities without conflating them.
#[must_use]
pub fn long_version() -> String {
    format!(
        "opencode-rust {BUILD_ID} (Rust package {RUST_PACKAGE_VERSION}; plugin compatibility {COMPATIBILITY_VERSION})"
    )
}

/// The HTTP identity for this implementation.
///
/// Starting with `opencode-rust/` is load-bearing: telemetry and server logs can
/// distinguish this implementation even though plugin semver checks see 1.18.13.
#[must_use]
pub fn user_agent() -> String {
    format!(
        "opencode-rust/{RUST_PACKAGE_VERSION} (build {BUILD_ID}; compatible-opencode/{COMPATIBILITY_VERSION})"
    )
}
