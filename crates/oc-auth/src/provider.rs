//! `auth.json` — the provider credential map.
//!
//! A port of `packages/opencode/src/auth/index.ts`. The file is a JSON object
//! keyed by provider ID, each value one of three shapes:
//!
//! | shape | oracle | fields |
//! | --- | --- | --- |
//! | `oauth` | `auth/index.ts:14-21` | `refresh`, `access`, `expires`, `accountId?`, `enterpriseUrl?` |
//! | `api` | `auth/index.ts:23-27` | `key`, `metadata?` |
//! | `wellknown` | `auth/index.ts:29-33` | `key`, `token` |
//!
//! Discriminated by `type` (`auth/index.ts:35`). Both optional-field shapes and
//! the `accountId` spelling were confirmed against the live
//! `$XDG_DATA_HOME/opencode/auth.json` this machine's 1.18.12 binary maintains,
//! read structurally without its values.
//!
//! # This file is shared with the TypeScript binary
//!
//! Same path, same bytes, either program may have written it. Every behaviour
//! below that is not obvious from the source was pinned by running the 1.18.12
//! binary against a scratch `XDG_DATA_HOME`:
//!
//! - `opencode auth list` on a file written by [`AuthStore::set`] lists all three
//!   shapes with the right `type` for each.
//! - An entry that does not decode is **dropped from the read** and then
//!   **destroyed by the next write**. Seeding `{"type":"banana"}` and an
//!   `expires: -5` alongside two good entries, `auth list` showed only the good
//!   two, and one `auth logout` left only them on disk.
//! - An unrecognised extra field survives a read and is **stripped by the next
//!   write**.
//! - A `0644` file is read without complaint and its mode is left alone; a write
//!   repairs it to `0600`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oc_paths::{Env, Layout};

use crate::error::AuthError;
use crate::secret::Secret;
use crate::store::{self, PermissionWarning};

/// The environment variable that replaces every read of `auth.json` —
/// `auth/index.ts:59`.
pub const OPENCODE_AUTH_CONTENT: &str = "OPENCODE_AUTH_CONTENT";

/// The placeholder an OAuth provider stores where an API key would go —
/// `auth/index.ts:8`.
pub const OAUTH_DUMMY_KEY: &str = "opencode-oauth-dummy-key";

/// One provider's credential.
///
/// Every secret-bearing field is a [`Secret`], so a `{:?}` of this enum — or of
/// anything containing it — cannot print a token. `metadata`'s *values* are
/// wrapped too, because a provider is free to put a token in there; its keys stay
/// visible since they are what makes a log line useful.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum Credential {
    /// An OAuth token pair — `auth/index.ts:14-21`.
    Oauth {
        /// The long-lived token used to mint a new `access`.
        refresh: Secret,
        /// The bearer token sent with requests.
        access: Secret,
        /// Expiry as a Unix timestamp in milliseconds.
        ///
        /// `NonNegativeInt` in the oracle, so a negative value fails to decode
        /// and the entry is dropped — matching the observed behaviour.
        expires: u64,
        /// Which account the token belongs to, for providers that multiplex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        /// The base URL of a self-hosted or enterprise deployment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enterprise_url: Option<String>,
    },

    /// A plain API key — `auth/index.ts:23-27`.
    Api {
        /// The key itself.
        key: Secret,
        /// Free-form provider metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<BTreeMap<String, Secret>>,
    },

    /// A key/token pair from a well-known endpoint — `auth/index.ts:29-33`.
    #[serde(rename = "wellknown")]
    WellKnown {
        /// The key half.
        key: Secret,
        /// The token half.
        token: Secret,
    },
}

impl Credential {
    /// The `type` discriminant as it appears in the file, which is also the label
    /// `opencode auth list` prints beside the provider.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Oauth { .. } => "oauth",
            Self::Api { .. } => "api",
            Self::WellKnown { .. } => "wellknown",
        }
    }
}

/// Everything one read of `auth.json` produced.
///
/// `permissions` and `skipped` travel with the entries rather than only reaching
/// a log sink, so a caller can refuse to write over a file it could not fully
/// understand — the write would otherwise destroy the entries in `skipped`, which
/// is what the oracle does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// The decodable credentials, keyed by provider ID.
    pub entries: BTreeMap<String, Credential>,
    /// Set when the file on disk was group- or world-accessible.
    pub permissions: Option<PermissionWarning>,
    /// Provider IDs whose value did not decode, and which a write would destroy.
    pub skipped: Vec<String>,
}

impl Credentials {
    /// One provider's credential.
    #[must_use]
    pub fn get(&self, provider: &str) -> Option<&Credential> {
        self.entries.get(provider)
    }

    /// How many credentials were understood.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no credential was understood.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Reader and writer for `auth.json`.
///
/// # `OPENCODE_AUTH_CONTENT` replaces reads, not writes
///
/// When the variable holds a JSON object, it becomes the entire result of
/// [`AuthStore::all`] and the file on disk is not consulted —
/// `auth/index.ts:59-63`.
///
/// The consequence is sharp, and observed rather than inferred. Because
/// [`AuthStore::set`] and [`AuthStore::remove`] both start from `all()`, a
/// mutation performed while the variable is set writes the **variable's** content
/// plus the mutation to the **file**, erasing whatever the file held. Against the
/// 1.18.12 binary, a file holding `filealpha` and `filebeta` plus
/// `OPENCODE_AUTH_CONTENT={"envgamma":…,"filebeta":…}` plus
/// `opencode auth logout filebeta` left exactly `{"envgamma":…}` on disk;
/// `filealpha` was gone. This crate reproduces that, because a divergence would
/// mean the two binaries disagree about the user's credentials.
///
/// A malformed variable falls through to the file — the oracle's empty
/// `catch {}` at `auth/index.ts:62`, confirmed by observation.
#[derive(Clone, Debug)]
pub struct AuthStore {
    path: PathBuf,
    auth_content: Option<String>,
}

impl AuthStore {
    /// A store over an explicit path, with no environment override.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            auth_content: None,
        }
    }

    /// A store over an explicit path, honouring `OPENCODE_AUTH_CONTENT` from
    /// `env`.
    ///
    /// Taking the environment as a value rather than reading the process's own
    /// keeps this testable: parallel tests that mutated `std::env` would race.
    #[must_use]
    pub fn with_env(path: impl Into<PathBuf>, env: &Env) -> Self {
        Self {
            path: path.into(),
            auth_content: env.value(OPENCODE_AUTH_CONTENT).map(str::to_owned),
        }
    }

    /// The store for a resolved layout — `data()/auth.json` — honouring
    /// `OPENCODE_AUTH_CONTENT` from `env`.
    #[must_use]
    pub fn resolve(layout: &Layout, env: &Env) -> Self {
        Self::with_env(layout.auth_file(), env)
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether an `OPENCODE_AUTH_CONTENT` override is in effect for reads.
    #[must_use]
    pub fn has_env_override(&self) -> bool {
        self.auth_content
            .as_deref()
            .and_then(Self::parse_override)
            .is_some()
    }

    /// Parse an override value into entries, or `None` if it is not a JSON object
    /// of credentials.
    fn parse_override(content: &str) -> Option<BTreeMap<String, Credential>> {
        let raw: BTreeMap<String, serde_json::Value> = serde_json::from_str(content).ok()?;
        let mut entries = BTreeMap::new();
        for (provider, value) in raw {
            match serde_json::from_value::<Credential>(value) {
                Ok(credential) => {
                    entries.insert(provider, credential);
                }
                Err(_) => {
                    tracing::warn!(
                        provider = %provider,
                        source = OPENCODE_AUTH_CONTENT,
                        "credential is not a recognized shape and was ignored"
                    );
                }
            }
        }
        Some(entries)
    }

    /// Every credential, from `OPENCODE_AUTH_CONTENT` if it parses and from the
    /// file otherwise — `auth/index.ts:58-67`.
    ///
    /// Entries that do not decode are dropped and listed in
    /// [`Credentials::skipped`], matching the oracle's `Record.filterMap` at
    /// `auth/index.ts:66`.
    pub fn all(&self) -> Result<Credentials, AuthError> {
        if let Some(content) = &self.auth_content
            && let Some(entries) = Self::parse_override(content)
        {
            return Ok(Credentials {
                entries,
                permissions: None,
                skipped: Vec::new(),
            });
        }

        let outcome: store::Read<BTreeMap<String, serde_json::Value>> =
            store::read_json(&self.path)?;

        let mut entries = BTreeMap::new();
        let mut skipped = Vec::new();
        for (provider, value) in outcome.value {
            match serde_json::from_value::<Credential>(value) {
                Ok(credential) => {
                    entries.insert(provider, credential);
                }
                Err(_) => {
                    tracing::warn!(
                        provider = %provider,
                        path = %self.path.display(),
                        "credential is not a recognized shape; it will be lost on the next write"
                    );
                    skipped.push(provider);
                }
            }
        }

        Ok(Credentials {
            entries,
            permissions: outcome.permissions,
            skipped,
        })
    }

    /// One provider's credential — `auth/index.ts:69-71`.
    pub fn get(&self, provider: &str) -> Result<Option<Credential>, AuthError> {
        Ok(self.all()?.entries.remove(provider))
    }

    /// Store a credential under `provider`, then write the file at `0600` —
    /// `auth/index.ts:73-81`.
    ///
    /// The key is normalized by stripping trailing slashes, and both the
    /// unnormalized spelling and the `provider + "/"` spelling are removed, so a
    /// provider identified by a URL cannot end up stored twice.
    pub fn set(&self, provider: &str, credential: Credential) -> Result<(), AuthError> {
        let normalized = normalize_key(provider);
        let mut entries = self.all()?.entries;
        if normalized != provider {
            entries.remove(provider);
        }
        entries.remove(&format!("{normalized}/"));
        entries.insert(normalized, credential);
        store::write_json(&self.path, &entries)
    }

    /// Remove `provider`'s credential, then write the file at `0600` —
    /// `auth/index.ts:83-89`.
    ///
    /// Removes both the spelling given and its trailing-slash-stripped form.
    pub fn remove(&self, provider: &str) -> Result<(), AuthError> {
        let normalized = normalize_key(provider);
        let mut entries = self.all()?.entries;
        entries.remove(provider);
        entries.remove(&normalized);
        store::write_json(&self.path, &entries)
    }
}

/// `key.replace(/\/+$/, "")` — strip every trailing slash.
fn normalize_key(key: &str) -> String {
    key.trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn store_in(dir: &tempfile::TempDir) -> AuthStore {
        AuthStore::new(dir.path().join("auth.json"))
    }

    fn oauth() -> Credential {
        Credential::Oauth {
            refresh: Secret::new("rt-refresh-token-0001"),
            access: Secret::new("at-access-token-0001"),
            expires: 1_893_456_000_000,
            account_id: Some("acct-42".to_owned()),
            enterprise_url: Some("https://enterprise.example.test".to_owned()),
        }
    }

    fn api() -> Credential {
        Credential::Api {
            key: Secret::new("sk-api-key-0002"),
            metadata: Some(BTreeMap::from([
                ("label".to_owned(), Secret::new("primary")),
                ("region".to_owned(), Secret::new("us-east-2")),
            ])),
        }
    }

    fn wellknown() -> Credential {
        Credential::WellKnown {
            key: Secret::new("wk-key-0003"),
            token: Secret::new("wk-token-0003"),
        }
    }

    #[test]
    fn all_three_shapes_round_trip_field_for_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("anthropic", oauth()).expect("set oauth");
        store.set("openai", api()).expect("set api");
        store.set("acme", wellknown()).expect("set wellknown");

        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.get("anthropic"), Some(&oauth()));
        assert_eq!(loaded.get("openai"), Some(&api()));
        assert_eq!(loaded.get("acme"), Some(&wellknown()));
        assert!(loaded.skipped.is_empty());
        assert_eq!(loaded.permissions, None);
    }

    /// The on-disk field names are what the TypeScript binary reads.
    #[test]
    fn the_serialized_field_names_are_the_oracle_spellings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("anthropic", oauth()).expect("set");
        store.set("openai", api()).expect("set");
        store.set("acme", wellknown()).expect("set");

        let text = std::fs::read_to_string(store.path()).expect("read");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse");

        assert_eq!(json["anthropic"]["type"], "oauth");
        assert_eq!(json["anthropic"]["refresh"], "rt-refresh-token-0001");
        assert_eq!(json["anthropic"]["access"], "at-access-token-0001");
        assert_eq!(json["anthropic"]["expires"], 1_893_456_000_000_u64);
        assert_eq!(json["anthropic"]["accountId"], "acct-42");
        assert_eq!(
            json["anthropic"]["enterpriseUrl"],
            "https://enterprise.example.test"
        );

        assert_eq!(json["openai"]["type"], "api");
        assert_eq!(json["openai"]["key"], "sk-api-key-0002");
        assert_eq!(json["openai"]["metadata"]["region"], "us-east-2");

        assert_eq!(json["acme"]["type"], "wellknown");
        assert_eq!(json["acme"]["key"], "wk-key-0003");
        assert_eq!(json["acme"]["token"], "wk-token-0003");
    }

    /// Absent optionals must not appear as `null`; the oracle's schema would
    /// reject that and drop the whole entry.
    #[test]
    fn absent_optional_fields_are_omitted_not_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store
            .set(
                "minimal",
                Credential::Oauth {
                    refresh: Secret::new("r"),
                    access: Secret::new("a"),
                    expires: 0,
                    account_id: None,
                    enterprise_url: None,
                },
            )
            .expect("set");
        store
            .set(
                "bare",
                Credential::Api {
                    key: Secret::new("k"),
                    metadata: None,
                },
            )
            .expect("set");

        let text = std::fs::read_to_string(store.path()).expect("read");
        assert!(!text.contains("null"), "{text}");
        assert!(!text.contains("accountId"), "{text}");
        assert!(!text.contains("enterpriseUrl"), "{text}");
        assert!(!text.contains("metadata"), "{text}");
    }

    #[test]
    fn a_file_the_typescript_binary_would_write_decodes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
  "anthropic": {
    "type": "oauth",
    "refresh": "rt",
    "access": "at",
    "expires": 1893456000000,
    "accountId": "acct-42"
  },
  "openai": { "type": "api", "key": "sk-1" },
  "acme": { "type": "wellknown", "key": "k", "token": "t" }
}"#,
        )
        .expect("seed");

        let loaded = AuthStore::new(&path).all().expect("all");
        assert_eq!(loaded.len(), 3);
        assert_eq!(
            loaded.get("anthropic"),
            Some(&Credential::Oauth {
                refresh: Secret::new("rt"),
                access: Secret::new("at"),
                expires: 1_893_456_000_000,
                account_id: Some("acct-42".to_owned()),
                enterprise_url: None,
            })
        );
        assert_eq!(loaded.get("openai").map(Credential::kind), Some("api"));
        assert_eq!(loaded.get("acme").map(Credential::kind), Some("wellknown"));
    }

    /// Observed against 1.18.12: undecodable entries vanish from the read.
    #[test]
    fn undecodable_entries_are_skipped_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
  "good": { "type": "api", "key": "sk-good" },
  "notype": { "key": "sk-bad" },
  "unknowntype": { "type": "banana", "key": "sk-bad" },
  "negexpires": { "type": "oauth", "refresh": "r", "access": "a", "expires": -5 }
}"#,
        )
        .expect("seed");

        let loaded = AuthStore::new(&path).all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("good").is_some());
        assert_eq!(
            loaded.skipped,
            vec![
                "negexpires".to_owned(),
                "notype".to_owned(),
                "unknowntype".to_owned()
            ]
        );
    }

    /// Observed against 1.18.12: an unknown field survives the read and is
    /// stripped by the next write.
    #[test]
    fn unknown_fields_are_tolerated_on_read_and_dropped_on_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{ "openai": { "type": "api", "key": "sk-1", "surprise": "kept-or-dropped" } }"#,
        )
        .expect("seed");

        let store = AuthStore::new(&path);
        assert_eq!(store.all().expect("all").len(), 1);

        store.set("other", wellknown()).expect("set");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("surprise"), "{text}");
        assert!(text.contains("sk-1"), "{text}");
    }

    #[test]
    fn get_returns_none_for_an_absent_provider_and_an_absent_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        assert_eq!(store.get("nobody").expect("get"), None);
        store.set("openai", api()).expect("set");
        assert_eq!(store.get("nobody").expect("get"), None);
        assert_eq!(store.get("openai").expect("get"), Some(api()));
    }

    #[test]
    fn remove_leaves_the_other_providers_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("keep", api()).expect("set");
        store.set("drop", wellknown()).expect("set");
        store.remove("drop").expect("remove");

        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("keep").is_some());
        assert_eq!(loaded.get("drop"), None);
    }

    #[test]
    fn remove_of_an_absent_provider_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.remove("never-existed").expect("remove");
        assert!(store.all().expect("all").is_empty());
    }

    /// `auth/index.ts:74-77` — a URL-shaped provider ID must not be stored twice.
    #[test]
    fn set_normalizes_trailing_slashes_and_collapses_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);

        store.set("https://api.example.test/", api()).expect("set");
        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("https://api.example.test").is_some());
        assert_eq!(loaded.get("https://api.example.test/"), None);

        // Re-setting the slashless spelling must not create a second entry.
        store
            .set("https://api.example.test", wellknown())
            .expect("set again");
        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get("https://api.example.test"), Some(&wellknown()));

        // And neither must the slashed one.
        store.set("https://api.example.test//", api()).expect("set");
        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get("https://api.example.test"), Some(&api()));
    }

    #[test]
    fn remove_accepts_either_spelling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("https://api.example.test", api()).expect("set");
        store
            .remove("https://api.example.test/")
            .expect("remove slashed");
        assert!(store.all().expect("all").is_empty());
    }

    #[test]
    fn normalize_key_strips_only_trailing_slashes() {
        assert_eq!(normalize_key("openai"), "openai");
        assert_eq!(normalize_key("openai/"), "openai");
        assert_eq!(normalize_key("openai///"), "openai");
        assert_eq!(normalize_key("https://x/y/"), "https://x/y");
        assert_eq!(normalize_key("/leading"), "/leading");
        assert_eq!(normalize_key(""), "");
    }

    #[test]
    fn every_write_lands_at_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        for step in ["set", "remove"] {
            match step {
                "set" => store.set("openai", api()).expect("set"),
                _ => store.remove("openai").expect("remove"),
            }
            #[cfg(unix)]
            {
                let mode = std::fs::metadata(store.path())
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "after {step}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_permissive_file_warns_and_still_yields_its_credentials() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("openai", api()).expect("set");
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");

        let loaded = store.all().expect("read must still succeed");
        assert_eq!(loaded.get("openai"), Some(&api()));
        let warning = loaded.permissions.expect("warning");
        assert_eq!(warning.path, store.path());
        assert_eq!(warning.mode, 0o644);
        assert!(
            warning
                .to_string()
                .contains(&store.path().display().to_string())
        );
    }

    // -- ZUNO_AUTH_CONTENT -------------------------------------------------

    fn env(value: &str) -> Env {
        Env::empty().with("ZUNO_AUTH_CONTENT", value)
    }

    #[test]
    fn the_env_override_replaces_the_file_entirely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        AuthStore::new(&path).set("fromfile", api()).expect("seed");

        let store = AuthStore::with_env(
            &path,
            &env(r#"{"fromenv":{"type":"api","key":"sk-env-only"}}"#),
        );
        assert!(store.has_env_override());
        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get("fromfile"), None);
        assert_eq!(
            loaded.get("fromenv"),
            Some(&Credential::Api {
                key: Secret::new("sk-env-only"),
                metadata: None,
            })
        );
    }

    #[test]
    fn the_env_override_serves_get_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AuthStore::with_env(
            dir.path().join("auth.json"),
            &env(r#"{"fromenv":{"type":"wellknown","key":"k","token":"t"}}"#),
        );
        assert_eq!(
            store.get("fromenv").expect("get").map(|c| c.kind()),
            Some("wellknown")
        );
    }

    /// `auth/index.ts:62` swallows a parse failure and falls through to the file.
    #[test]
    fn a_malformed_env_override_falls_back_to_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        AuthStore::new(&path).set("fromfile", api()).expect("seed");

        for bad in ["{not json", "", "null", "[1,2]", "\"a string\"", "42"] {
            let store = AuthStore::with_env(&path, &env(bad));
            assert!(!store.has_env_override(), "{bad:?} must not override");
            let loaded = store.all().expect("all");
            assert_eq!(loaded.get("fromfile"), Some(&api()), "for {bad:?}");
        }
    }

    /// An override entry of an unrecognised shape is dropped, not fatal.
    #[test]
    fn an_undecodable_override_entry_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AuthStore::with_env(
            dir.path().join("auth.json"),
            &env(r#"{"good":{"type":"api","key":"sk-1"},"bad":{"type":"banana"}}"#),
        );
        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("good").is_some());
        assert_eq!(loaded.get("bad"), None);
    }

    /// The destructive interaction, observed against 1.18.12 and reproduced here:
    /// a mutation under an active override writes the override to the file and
    /// the file's own entries are gone.
    #[test]
    fn a_mutation_under_an_override_overwrites_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let plain = AuthStore::new(&path);
        plain.set("filealpha", api()).expect("seed alpha");
        plain.set("filebeta", wellknown()).expect("seed beta");

        let overridden = AuthStore::with_env(
            &path,
            &env(r#"{"envgamma":{"type":"api","key":"sk-env-gamma"},
                    "filebeta":{"type":"api","key":"sk-env-beta"}}"#),
        );
        overridden.remove("filebeta").expect("remove");

        // Read the file without the override to see what actually landed.
        let ondisk = plain.all().expect("all");
        assert_eq!(ondisk.len(), 1);
        assert_eq!(ondisk.get("filealpha"), None, "the oracle loses this too");
        assert_eq!(
            ondisk.get("envgamma"),
            Some(&Credential::Api {
                key: Secret::new("sk-env-gamma"),
                metadata: None,
            })
        );
    }

    #[test]
    fn no_env_key_means_no_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        AuthStore::new(&path).set("fromfile", api()).expect("seed");
        let store = AuthStore::with_env(&path, &Env::empty());
        assert!(!store.has_env_override());
        assert_eq!(store.all().expect("all").get("fromfile"), Some(&api()));
    }

    #[test]
    fn resolve_points_at_the_layouts_auth_file() {
        let layout = Layout::resolve_with(&Env::from_pairs([("HOME", "/config")]), None);
        let store = AuthStore::resolve(&layout, &Env::empty());
        assert_eq!(store.path(), layout.auth_file());
    }

    // -- redaction ---------------------------------------------------------

    #[test]
    fn debug_of_every_shape_hides_every_secret() {
        let rendered = format!("{:?}", vec![oauth(), api(), wellknown()]);
        for plaintext in [
            "rt-refresh-token-0001",
            "at-access-token-0001",
            "sk-api-key-0002",
            "primary",
            "us-east-2",
            "wk-key-0003",
            "wk-token-0003",
        ] {
            assert!(
                !rendered.contains(plaintext),
                "{plaintext} leaked: {rendered}"
            );
        }
        // Non-secret fields still show, or the redaction would be useless.
        assert!(rendered.contains("acct-42"), "{rendered}");
        assert!(rendered.contains("1893456000000"), "{rendered}");
        assert!(rendered.contains("label"), "{rendered}");
        assert!(rendered.contains("region"), "{rendered}");
    }

    #[test]
    fn debug_of_the_whole_loaded_set_hides_every_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("anthropic", oauth()).expect("set");
        store.set("openai", api()).expect("set");
        store.set("acme", wellknown()).expect("set");

        let loaded = store.all().expect("all");
        for rendered in [format!("{loaded:?}"), format!("{loaded:#?}")] {
            for plaintext in [
                "rt-refresh-token-0001",
                "at-access-token-0001",
                "sk-api-key-0002",
                "wk-token-0003",
            ] {
                assert!(!rendered.contains(plaintext), "{plaintext} leaked");
            }
            assert!(rendered.contains("anthropic"));
        }
    }

    /// A `Display` of the error must not carry a credential either — the paths
    /// and the `serde_json` position are all it should have.
    #[test]
    fn an_error_over_a_secret_bearing_file_carries_no_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{ "openai": { "key": "sk-leaky-9999" "#).expect("seed");
        let error = AuthStore::new(&path).all().expect_err("malformed");
        let rendered = format!("{error} / {error:?}");
        assert!(!rendered.contains("sk-leaky-9999"), "{rendered}");
    }
}
