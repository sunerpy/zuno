//! The boxed cause type used in `#[source]` position.

/// The type of an underlying cause whose concrete type this crate deliberately
/// does not name.
///
/// # Why this is not the catch-all this taxonomy forbids
///
/// A `String`-wrapping variant — `Other(String)`, `Unknown { message: String }` —
/// is forbidden because it is a *classification* escape hatch: it lets a caller
/// report a failure without deciding what kind of failure it is, and it forces
/// the next layer to parse prose to find out. Once such a variant exists, every
/// author reaches for it and the taxonomy stops meaning anything.
///
/// A boxed source is the opposite. It appears only in `#[source]` position, on a
/// variant that has *already* classified the failure. It carries the cause chain
/// for humans, logs and `{:#}` rendering. No recovery decision reads it, because
/// the variant holding it already answered that question.
///
/// The type is boxed rather than concrete for two reasons. Every crate in this
/// workspace depends on `oc-error`, so naming `reqwest::Error` here would put an
/// HTTP client and a TLS stack in the dependency graph of the terminal renderer.
/// And a foreign cause — a plugin host's error, an LSP server's transport
/// failure — has no single concrete type worth committing this crate to.
///
/// Where a concrete cause is both cheap and useful, this crate names it instead:
/// [`serde_json::Error`] preserves line, column and
/// `serde_json::Error::classify`, and [`std::io::Error`] preserves
/// `std::io::ErrorKind`. Boxing those would throw away data a reporter wants.
pub type BoxSource = Box<dyn std::error::Error + Send + Sync + 'static>;
