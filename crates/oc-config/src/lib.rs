//! Configuration schema, discovery, merge order, variable substitution, and legacy rejection.

pub mod discovery;
pub mod legacy;
pub mod schema;
pub mod variable;

pub use crate::discovery::DEFAULT_SCHEMA;
pub use crate::legacy::{DeprecatedForm, Deprecation};
pub use crate::schema::{Config, KNOWN_TOP_LEVEL_KEYS};
pub use crate::variable::{Missing, Source, Substitution};
