//! Credential storage for provider authentication and MCP OAuth, at mode `0600`.
//!
//! Two files under the layout's data directory, both shared byte-for-byte with
//! the TypeScript `opencode` binary that may have written them:
//!
//! - [`AuthStore`] over `auth.json` — a provider-keyed map of three credential
//!   shapes, with `ZUNO_AUTH_CONTENT` replacing reads.
//! - [`McpAuthStore`] over `mcp-auth.json` — per-MCP-server OAuth tokens, dynamic
//!   client registration, the PKCE verifier, the CSRF state, and the server URL.
//!
//! # Scope
//!
//! Storage. Nothing here performs an OAuth flow, refreshes a token, decides which
//! provider a model belongs to, or asks whether a credential has expired. Those
//! are separate concerns and are deliberately absent.
//!
//! # Two properties this crate exists to guarantee
//!
//! **The file is never readable by anyone but its owner.** `0600` is passed to
//! `open(2)` at creation rather than applied afterwards, so there is no interval
//! in which the tokens sit on disk at the process umask. A file that was already
//! permissive is repaired on write and warned about on read — read, not refuse:
//! the 1.18.12 binary reads a `0644` `auth.json` happily, and a Rust binary that
//! refused would lock a user out of every model they have configured. See
//! [`store`].
//!
//! The failure this is guarding against is not hypothetical. The reference
//! implementation in `.omo/refs/claw-code` writes its `credentials.json` through
//! `save_oauth_credentials` in `rust/crates/runtime/src/oauth.rs` and the file
//! contains no `set_permissions`, no `PermissionsExt`, and no `mode(` anywhere in
//! it — the credentials land at whatever the umask allows, world-readable on a
//! default `022`.
//!
//! **A credential cannot reach a log.** Every secret-bearing field is a
//! [`Secret`], whose `Debug` and `Display` both render `<redacted>`, so a
//! `#[derive(Debug)]` on an enclosing struct, a `tracing` field, an
//! `assert_eq!` failure, or a panic payload cannot carry a token out of the
//! process. [`Secret::expose`] is the single, greppable way to read one.
//!
//! # Example
//!
//! ```
//! use zuno_auth::{AuthStore, Credential, Secret};
//!
//! let dir = tempfile::tempdir()?;
//! let store = AuthStore::new(dir.path().join("auth.json"));
//!
//! store.set(
//!     "openai",
//!     Credential::Api { key: Secret::new("sk-example"), metadata: None },
//! )?;
//!
//! let loaded = store.all()?;
//! assert_eq!(loaded.get("openai").map(Credential::kind), Some("api"));
//!
//! // Nothing that renders the credential renders the key.
//! assert!(!format!("{loaded:?}").contains("sk-example"));
//!
//! # #[cfg(unix)]
//! # {
//! use std::os::unix::fs::PermissionsExt;
//! let mode = std::fs::metadata(store.path())?.permissions().mode() & 0o777;
//! assert_eq!(mode, 0o600);
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Oracle map
//!
//! | this crate | oracle |
//! | --- | --- |
//! | [`Credential`] | `packages/opencode/src/auth/index.ts:14-35` |
//! | [`AuthStore::all`] and `ZUNO_AUTH_CONTENT` | `auth/index.ts:58-67` |
//! | [`AuthStore::set`], [`AuthStore::remove`] | `auth/index.ts:73-89` |
//! | [`Entry`], [`Tokens`], [`ClientInfo`] | `packages/opencode/src/mcp/auth.ts:9-32` |
//! | [`McpAuthStore`] reads and writes | `mcp/auth.ts:65-142` |
//! | the `0600` write | `auth/index.ts:79`, `mcp/auth.ts:80`, `packages/core/src/fs-util.ts:110-113` |
//! | the file paths | `zuno_paths::Layout::auth_file`, `zuno_paths::Layout::mcp_auth_file` |

pub mod error;
pub mod mcp;
pub mod provider;
pub mod secret;
pub mod store;

pub use crate::error::AuthError;
pub use crate::mcp::{ClientInfo, Entry, McpAuthStore, McpCredentials, Tokens};
pub use crate::provider::{AuthStore, Credential, Credentials, OAUTH_DUMMY_KEY, ZUNO_AUTH_CONTENT};
pub use crate::secret::{REDACTED, Secret};
pub use crate::store::{CREDENTIAL_FILE_MODE, PermissionWarning};

#[cfg(test)]
mod tests {
    use super::*;

    /// The end-to-end promise of the crate, over both files at once: every shape
    /// survives a write and a read, the files are `0600`, and nothing that
    /// renders them renders a secret.
    #[test]
    fn both_stores_keep_their_secrets_and_their_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = AuthStore::new(dir.path().join("auth.json"));
        let mcp = McpAuthStore::new(dir.path().join("mcp-auth.json"));

        auth.set(
            "anthropic",
            Credential::Oauth {
                refresh: Secret::new("REFRESH-CANARY"),
                access: Secret::new("ACCESS-CANARY"),
                expires: 1_893_456_000_000,
                account_id: Some("acct-1".to_owned()),
                enterprise_url: None,
            },
        )
        .expect("set oauth");
        auth.set(
            "openai",
            Credential::Api {
                key: Secret::new("APIKEY-CANARY"),
                metadata: None,
            },
        )
        .expect("set api");
        auth.set(
            "acme",
            Credential::WellKnown {
                key: Secret::new("WKKEY-CANARY"),
                token: Secret::new("WKTOKEN-CANARY"),
            },
        )
        .expect("set wellknown");

        mcp.update_tokens(
            "acme-mcp",
            Tokens {
                access_token: Secret::new("MCPACCESS-CANARY"),
                refresh_token: Some(Secret::new("MCPREFRESH-CANARY")),
                expires_at: Some(1_893_456_000_000),
                scope: Some("read".to_owned()),
            },
            Some("https://mcp.example.test/sse"),
        )
        .expect("update tokens");
        mcp.update_code_verifier("acme-mcp", Secret::new("VERIFIER-CANARY"))
            .expect("verifier");

        let credentials = auth.all().expect("auth all");
        let mcp_credentials = mcp.all().expect("mcp all");
        assert_eq!(credentials.len(), 3);
        assert_eq!(mcp_credentials.len(), 1);

        let rendered = format!("{credentials:#?} {mcp_credentials:#?}");
        for canary in [
            "REFRESH-CANARY",
            "ACCESS-CANARY",
            "APIKEY-CANARY",
            "WKKEY-CANARY",
            "WKTOKEN-CANARY",
            "MCPACCESS-CANARY",
            "MCPREFRESH-CANARY",
            "VERIFIER-CANARY",
        ] {
            assert!(
                !rendered.contains(canary),
                "{canary} leaked into {rendered}"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [auth.path(), mcp.path()] {
                let mode = std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, CREDENTIAL_FILE_MODE, "{}", path.display());
            }
        }
    }
}
