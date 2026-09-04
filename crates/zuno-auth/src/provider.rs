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
//! the `accountId` spelling were confirmed against the live, upstream-only
//! `$XDG_DATA_HOME/opencode/auth.json` this machine's 1.18.12 binary maintains,
//! read structurally without its values. Zuno stores this shape under its own
//! `$XDG_DATA_HOME/zuno/auth.json` root instead.
//!
//! # The shape is compatible; the file is not shared
//!
//! Zuno never reads or writes the upstream path. Every wire-shape behaviour below
//! that is not obvious from the source was pinned by running the 1.18.12 binary
//! against a scratch `XDG_DATA_HOME`:
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

use zuno_paths::{Env, Layout};

use crate::error::AuthError;
use crate::secret::Secret;
use crate::store::{self, Modelled, PermissionWarning, StoreDamage, Unmodelled};

/// The environment variable that replaces every read of `auth.json` —
/// `auth/index.ts:59`.
pub const ZUNO_AUTH_CONTENT: &str = "ZUNO_AUTH_CONTENT";

/// The placeholder an OAuth provider stores where an API key would go.
pub const OAUTH_DUMMY_KEY: &str = "zuno-oauth-dummy-key";

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
    /// Every key any credential shape can put in one `auth.json` entry, in its on-disk
    /// spelling — the union over the three variants, plus the `type` discriminant.
    ///
    /// This is what tells a write which keys belong to it. A key here is written from the
    /// typed value or not written at all, so clearing `accountId`, or replacing an
    /// `oauth` credential with an `api` one, really removes the fields that go with it. A
    /// key that is *not* here belongs to whoever wrote the file and is carried through
    /// the write by [`store::unmodelled_fields`].
    ///
    /// `metadata` needs no nested shape: its value is a free-form map in the model, so
    /// every key inside it already round-trips.
    ///
    /// Adding a field to any variant means adding its spelling here.
    /// `every_modelled_key_is_declared` fails until it is.
    pub const MODELLED: &'static Modelled = &Modelled {
        keys: &[
            "type",
            "refresh",
            "access",
            "expires",
            "accountId",
            "enterpriseUrl",
            "key",
            "metadata",
            "token",
        ],
        within: &[],
    };

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
/// `permissions`, `skipped`, `preserved` and `damage` travel with the entries rather
/// than only reaching a log sink, so a caller can refuse to write over a file it could
/// not fully understand, and so a write puts back what it could not read.
///
/// `Eq` is deliberately absent: `preserved` holds arbitrary JSON, and
/// `serde_json::Value` is only `PartialEq`. Comparisons that need equality compare
/// `entries`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Credentials {
    /// The decodable credentials, keyed by provider ID.
    pub entries: BTreeMap<String, Credential>,
    /// Set when the file on disk was group- or world-accessible.
    pub permissions: Option<PermissionWarning>,
    /// Provider IDs whose value did not decode. They are absent from `entries` and
    /// kept in `preserved`, so a read still reports them and a write still keeps them.
    pub skipped: Vec<String>,
    /// The values behind `skipped`, as the source held them — see [`store::Rewritten`]
    /// for what "as held" does and does not promise.
    ///
    /// A write republishes these untouched. Without them, one `zuno auth login` over a
    /// file holding a shape this build does not model would delete that credential
    /// permanently, with the loss visible only in a log line.
    pub preserved: BTreeMap<String, serde_json::Value>,
    /// What each decoded entry held that [`Credential::MODELLED`] does not name, keyed by
    /// provider ID. Absent for an entry that held nothing extra, which is every entry a
    /// Zuno of this vintage wrote.
    ///
    /// This is the half `skipped`/`preserved` cannot see: a field a newer Zuno added to a
    /// shape this build *does* understand decodes successfully into nothing, so the entry
    /// is not skipped and the typed map written back would silently drop the field — from
    /// entries the write never touched as much as from the one it did.
    pub unmodelled: BTreeMap<String, Unmodelled>,
    /// Set when the file exists and held no store at all. `entries` is empty then,
    /// and it is empty because the store was destroyed rather than because the user
    /// has no credentials — a surface that reports one as the other tells them to
    /// log in when what they need is to restore a backup.
    pub damage: Option<StoreDamage>,
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
    ///
    /// Says nothing about `preserved`: a file holding only entries this build cannot
    /// decode is `is_empty()` and is not empty. A caller deciding whether the store has
    /// anything in it at all wants [`Credentials::names`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every provider ID the file holds, decoded or not, in one sorted sequence.
    ///
    /// The resolution a command that names a provider needs. Resolving against `entries`
    /// alone makes an entry this build could not decode unreachable: it is republished by
    /// every write, so it is permanent, and `Unknown configured provider` is the only
    /// answer a user gets when they try to remove it.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let mut names: Vec<&str> = self
            .entries
            .keys()
            .chain(self.preserved.keys())
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();
        names.into_iter()
    }

    /// Whether the file holds this provider at all, decoded or not.
    #[must_use]
    pub fn contains(&self, provider: &str) -> bool {
        self.entries.contains_key(provider) || self.preserved.contains_key(provider)
    }

    /// Whether this provider is present but was not decoded, so a surface listing it
    /// can say the build does not understand it and [`AuthStore::remove`] is how it goes
    /// away.
    #[must_use]
    pub fn is_preserved(&self, provider: &str) -> bool {
        self.preserved.contains_key(provider)
    }
}

/// Reader and writer for `auth.json`.
///
/// # `ZUNO_AUTH_CONTENT` replaces reads, not writes
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
/// `ZUNO_AUTH_CONTENT={"envgamma":…,"filebeta":…}` plus
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

    /// A store over an explicit path, honouring `ZUNO_AUTH_CONTENT` from
    /// `env`.
    ///
    /// Taking the environment as a value rather than reading the process's own
    /// keeps this testable: parallel tests that mutated `std::env` would race.
    #[must_use]
    pub fn with_env(path: impl Into<PathBuf>, env: &Env) -> Self {
        Self {
            path: path.into(),
            auth_content: env.value(ZUNO_AUTH_CONTENT).map(str::to_owned),
        }
    }

    /// The store for a resolved layout — `data()/auth.json` — honouring
    /// `ZUNO_AUTH_CONTENT` from `env`.
    #[must_use]
    pub fn resolve(layout: &Layout, env: &Env) -> Self {
        Self::with_env(layout.auth_file(), env)
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether an `ZUNO_AUTH_CONTENT` override is in effect for reads.
    #[must_use]
    pub fn has_env_override(&self) -> bool {
        self.auth_content
            .as_deref()
            .and_then(Self::parse_override)
            .is_some()
    }

    /// Parse an override value into decoded credentials and the values that did not
    /// decode, or `None` if it is not a JSON object of credentials.
    ///
    /// The undecoded values are kept for the same reason the file's are: a mutation
    /// under an override publishes the override to the file, and a shape this build
    /// does not model must survive that publication.
    fn parse_override(content: &str) -> Option<Credentials> {
        let raw: BTreeMap<String, serde_json::Value> = serde_json::from_str(content).ok()?;
        let mut entries = BTreeMap::new();
        let mut skipped = Vec::new();
        let mut preserved = BTreeMap::new();
        let mut unmodelled = BTreeMap::new();
        for (provider, value) in raw {
            match serde_json::from_value::<Credential>(value.clone()) {
                Ok(credential) => {
                    let extra = store::unmodelled_fields(&value, Credential::MODELLED);
                    if !extra.is_empty() {
                        unmodelled.insert(provider.clone(), extra);
                    }
                    entries.insert(provider, credential);
                }
                Err(_) => {
                    tracing::warn!(
                        provider = %provider,
                        source = ZUNO_AUTH_CONTENT,
                        "credential is not a recognized shape; it is kept as it stands"
                    );
                    skipped.push(provider.clone());
                    preserved.insert(provider, value);
                }
            }
        }
        Some(Credentials {
            entries,
            permissions: None,
            skipped,
            preserved,
            unmodelled,
            damage: None,
        })
    }

    /// Every credential, from `ZUNO_AUTH_CONTENT` if it parses and from the
    /// file otherwise — `auth/index.ts:58-67`.
    ///
    /// Entries that do not decode are absent from [`Credentials::entries`] and listed
    /// in [`Credentials::skipped`], matching the oracle's `Record.filterMap` at
    /// `auth/index.ts:66`. Unlike the oracle they are also kept in
    /// [`Credentials::preserved`], so the next write puts them back rather than
    /// deleting a credential this build merely failed to understand.
    ///
    /// A file that exists and holds no store is reported in [`Credentials::damage`]
    /// rather than refused, because this is the read behind `zuno auth list`,
    /// `zuno models`, `zuno auth login` and a run whose model credential comes from
    /// the environment. Refusing here would deny the commands that repair the
    /// damage — including to a user who never needed this file. The refusal belongs
    /// on the read that precedes a write: see [`AuthStore::set`].
    pub fn all(&self) -> Result<Credentials, AuthError> {
        if let Some(overridden) = self.overridden() {
            return Ok(overridden);
        }
        Ok(self.decode(store::read_json(&self.path)?))
    }

    /// [`AuthStore::all`] for a caller that will write the result back.
    ///
    /// Damage is a failure here, and absence is confirmed before it is believed;
    /// [`store::read_json_for_update`] says why. The `ZUNO_AUTH_CONTENT` override
    /// still short-circuits the file, unchanged: when it is in effect the file is not
    /// the authority for reads and a write over it ratifies nothing.
    fn all_for_update(&self) -> Result<Credentials, AuthError> {
        if let Some(overridden) = self.overridden() {
            return Ok(overridden);
        }
        Ok(self.decode(store::read_json_for_update(&self.path)?))
    }

    /// The environment's credentials, when the override is in effect.
    fn overridden(&self) -> Option<Credentials> {
        let content = self.auth_content.as_deref()?;
        Self::parse_override(content)
    }

    /// Turn one file read into credentials, moving values this build does not
    /// recognize into [`Credentials::skipped`] and [`Credentials::preserved`].
    fn decode(&self, outcome: store::Read<BTreeMap<String, serde_json::Value>>) -> Credentials {
        let mut entries = BTreeMap::new();
        let mut skipped = Vec::new();
        let mut preserved = BTreeMap::new();
        let mut unmodelled = BTreeMap::new();
        for (provider, value) in outcome.value {
            match serde_json::from_value::<Credential>(value.clone()) {
                Ok(credential) => {
                    let extra = store::unmodelled_fields(&value, Credential::MODELLED);
                    if !extra.is_empty() {
                        unmodelled.insert(provider.clone(), extra);
                    }
                    entries.insert(provider, credential);
                }
                Err(_) => {
                    tracing::warn!(
                        provider = %provider,
                        path = %self.path.display(),
                        "credential is not a recognized shape; it is kept as it stands and \
                         reported, not decoded"
                    );
                    skipped.push(provider.clone());
                    preserved.insert(provider, value);
                }
            }
        }

        Credentials {
            entries,
            permissions: outcome.permissions,
            skipped,
            preserved,
            unmodelled,
            damage: outcome.damage,
        }
    }

    /// One provider's credential — `auth/index.ts:69-71`.
    pub fn get(&self, provider: &str) -> Result<Option<Credential>, AuthError> {
        Ok(self.all()?.entries.remove(provider))
    }

    /// Store a credential under `provider`, then write the file at `0600` —
    /// `auth/index.ts:73-81`.
    ///
    /// The key is normalized by stripping trailing slashes, and *every* spelling that
    /// normalizes to the same id is collapsed into it — not the two suffixes a caller
    /// happens to have used — so a provider identified by a URL cannot end up stored
    /// twice under near-miss spellings that a normalizing lookup would later resolve to.
    ///
    /// This is a read-modify-write, so it reads through
    /// [`store::read_json_for_update`]: a store it could not read is a store it must
    /// not replace with one holding a single credential.
    ///
    /// Entries this build could not decode are republished verbatim, so logging in to
    /// one provider cannot delete another provider's credential just because its shape
    /// is unrecognized. The normalization above still applies to them: a preserved
    /// entry spelled `provider` or `provider/` is the same credential under another
    /// name and is replaced, not kept alongside.
    ///
    /// Fields a newer Zuno added to *other* providers' entries are carried through the
    /// write as well — see [`Credentials::unmodelled`]. They are **not** carried on the
    /// entry being set: this replaces that provider's credential outright, and pairing a
    /// freshly minted token with a device binding or a rotation stamp issued for the
    /// credential it replaced would attach another build's authority claims to a
    /// credential it never saw. A newer Zuno writing that entry again restores them.
    pub fn set(&self, provider: &str, credential: Credential) -> Result<(), AuthError> {
        let normalized = normalize_key(provider);
        let mut loaded = self.all_for_update()?;
        collapse_spellings_of(&mut loaded, &normalized);
        loaded.entries.insert(normalized, credential);
        store::write_json(
            &self.path,
            &store::rewrite(&loaded.entries, &loaded.unmodelled, &loaded.preserved),
        )
    }

    /// Remove `provider`'s credential, then write the file at `0600` —
    /// `auth/index.ts:83-89`.
    ///
    /// Removes every spelling that normalizes to the same id, whether the value decoded
    /// or not: a user removing a provider means that entry, and an entry this build
    /// cannot read is one they especially cannot fix any other way. `remove` is the only
    /// way to delete a credential whose shape this build does not model, so it resolves
    /// against [`Credentials::preserved`] as well as [`Credentials::entries`] — the
    /// names a caller should offer are [`Credentials::names`].
    ///
    /// Reads through [`store::read_json_for_update`] for the same reason
    /// [`AuthStore::set`] does.
    pub fn remove(&self, provider: &str) -> Result<(), AuthError> {
        let normalized = normalize_key(provider);
        let mut loaded = self.all_for_update()?;
        collapse_spellings_of(&mut loaded, &normalized);
        store::write_json(
            &self.path,
            &store::rewrite(&loaded.entries, &loaded.unmodelled, &loaded.preserved),
        )
    }
}

/// `key.replace(/\/+$/, "")` — strip every trailing slash.
fn normalize_key(key: &str) -> String {
    key.trim_end_matches('/').to_owned()
}

/// Drop every entry whose key normalizes to `normalized`, from all three maps.
///
/// One provider written under two spellings is one credential the user believes they
/// replaced or removed and did not. Enumerating suffixes missed `provider//`, which
/// [`normalize_key`] resolves to the same id and a lookup would therefore find, so the
/// test is the normalization itself. It is applied to `preserved` too: an entry this
/// build cannot decode is still that provider's, and leaving it beside the canonical key
/// is how a stale credential becomes permanent and invisible.
fn collapse_spellings_of(loaded: &mut Credentials, normalized: &str) {
    loaded
        .entries
        .retain(|key, _| normalize_key(key) != normalized);
    loaded
        .preserved
        .retain(|key, _| normalize_key(key) != normalized);
    loaded
        .unmodelled
        .retain(|key, _| normalize_key(key) != normalized);
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

    #[test]
    fn oauth_credentials_use_the_zuno_placeholder_key() {
        assert_eq!(OAUTH_DUMMY_KEY, "zuno-oauth-dummy-key");
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

    /// The reviewer's input exactly: an `auth.json` holding a credential shape this
    /// build does not recognize, and one `zuno auth login openai` over it.
    ///
    /// Before the write learned to carry undecoded values, that single login deleted
    /// the unrecognized credential permanently and said so only in a log line.
    #[test]
    fn a_login_keeps_a_credential_shape_this_build_does_not_recognize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let unrecognized = r#"{ "type": "passkey", "attestation": "AAECAwQ", "counter": 7 }"#;
        std::fs::write(
            &path,
            format!(
                r#"{{
  "anthropic": {{ "type": "api", "key": "sk-anthropic" }},
  "newer-zuno": {unrecognized}
}}"#
            ),
        )
        .expect("seed");

        let store = AuthStore::new(&path);
        // `zuno auth login openai`.
        store.set("openai", api()).expect("login");

        let published: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("the file must still be a store");
        assert_eq!(
            published.get("newer-zuno"),
            Some(&serde_json::from_str::<serde_json::Value>(unrecognized).expect("value")),
            "a login for another provider must not delete a credential it could not read"
        );
        assert!(
            published.contains_key("anthropic") && published.contains_key("openai"),
            "the credentials it did understand must still be there: {published:?}"
        );

        let loaded = store.all().expect("all");
        assert_eq!(loaded.skipped, vec!["newer-zuno".to_owned()]);
        assert_eq!(
            loaded.len(),
            2,
            "the unreadable entry is still not a credential"
        );
    }

    /// Removing a provider means that entry even when nothing can decode it: a user
    /// who cannot read the value has no other way to get rid of it.
    #[test]
    fn removing_a_provider_deletes_an_entry_this_build_cannot_decode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
  "keep": { "type": "api", "key": "sk-keep" },
  "newer-zuno": { "type": "passkey", "counter": 7 }
}"#,
        )
        .expect("seed");

        let store = AuthStore::new(&path);
        store.remove("newer-zuno").expect("logout");

        let published: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("store");
        assert!(
            !published.contains_key("newer-zuno"),
            "an explicit removal must reach an entry that did not decode: {published:?}"
        );
        assert!(published.contains_key("keep"));
        assert!(store.all().expect("all").skipped.is_empty());
    }

    /// A preserved entry is still the same provider under another spelling, so the
    /// duplicate-collapse guarantee has to reach it too — otherwise a login would
    /// leave `openai` and `openai/` side by side, one of them unreadable.
    #[test]
    fn a_login_collapses_a_duplicate_spelling_that_did_not_decode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        // The reviewer's input: a trailing *double* slash normalizes to the same id and
        // used to survive beside the canonical key forever, unreachable by any command.
        std::fs::write(
            &path,
            r#"{ "openai//": { "type": "passkey" }, "openai/": { "type": "passkey", "counter": 7 } }"#,
        )
        .expect("seed");

        let store = AuthStore::new(&path);
        store.set("openai", api()).expect("login");

        let published: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("store");
        assert_eq!(
            published.keys().collect::<Vec<_>>(),
            vec!["openai"],
            "one provider may not end up stored twice: {published:?}"
        );

        // A logout resolves the same way, from either spelling.
        std::fs::write(
            &path,
            r#"{ "openai//": { "type": "passkey" }, "openai": { "type": "api", "key": "k" } }"#,
        )
        .expect("reseed");
        store.remove("openai/").expect("logout");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "{}",
            "logout has to reach every spelling of the provider it was given"
        );
    }

    /// An unknown field survives the read *and* the next write. The oracle drops it;
    /// this is a deliberate divergence, because the field a newer Zuno added is
    /// indistinguishable from the field the oracle never knew about, and dropping it
    /// destroys durable user state that a supported build wrote.
    #[test]
    fn an_unknown_field_survives_a_login_for_a_different_provider() {
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
        assert!(text.contains("surprise"), "{text}");
        assert!(text.contains("sk-1"), "{text}");
    }

    /// The reviewer's measured input, verbatim: two entries this build decodes
    /// completely, each carrying a field it does not model, and one `set` for a third
    /// provider. Every unmodelled field had been stripped — including from `openai`,
    /// which the write never touched — while `skipped` and `preserved` stayed empty, so
    /// the store believed nothing was at risk. Adding a field is how a schema evolves,
    /// which makes this the common case rather than the exotic one.
    #[test]
    fn a_field_a_newer_zuno_added_survives_a_login_on_an_untouched_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
              "anthropic": {
                "type": "api",
                "key": "k",
                "deviceBinding": { "device": "laptop-01", "attested": true },
                "rotatesAt": 123
              },
              "openai": {
                "type": "oauth",
                "refresh": "r",
                "access": "a",
                "expires": 1,
                "tenant": "eu"
              }
            }"#,
        )
        .expect("seed");

        let store = AuthStore::new(&path);
        let loaded = store.all().expect("all");
        assert_eq!(loaded.entries.len(), 2, "both entries decode");
        assert!(
            loaded.skipped.is_empty() && loaded.preserved.is_empty(),
            "an added field does not make an entry undecodable, which is the whole gap"
        );

        store.set("google", api()).expect("one unrelated login");

        let published: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("json");
        assert_eq!(
            published["anthropic"]["deviceBinding"]["device"],
            serde_json::json!("laptop-01"),
            "{published}"
        );
        assert_eq!(
            published["anthropic"]["deviceBinding"]["attested"],
            serde_json::json!(true),
            "{published}"
        );
        assert_eq!(
            published["anthropic"]["rotatesAt"],
            serde_json::json!(123),
            "{published}"
        );
        assert_eq!(
            published["openai"]["tenant"],
            serde_json::json!("eu"),
            "an entry the write never touched must not lose a field: {published}"
        );
        assert_eq!(published["google"]["type"], serde_json::json!("api"));
        // The modelled half is still written from the typed value.
        assert_eq!(published["anthropic"]["key"], serde_json::json!("k"));
        assert_eq!(published["openai"]["access"], serde_json::json!("a"));
    }

    /// The one entry whose unmodelled fields are deliberately *not* carried: the entry
    /// being replaced. A device binding or a rotation stamp issued for the credential a
    /// login replaces is a claim about that credential, and re-attaching it to a freshly
    /// minted token would hand another build's authority claims to tokens it never saw.
    #[test]
    fn a_login_does_not_re_attach_the_replaced_credentials_unmodelled_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{ "openai": { "type": "api", "key": "old", "deviceBinding": { "device": "gone" } } }"#,
        )
        .expect("seed");

        let store = AuthStore::new(&path);
        store.set("openai", api()).expect("re-login");

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(!text.contains("deviceBinding"), "{text}");
        assert!(!text.contains("gone"), "{text}");
    }

    /// Every key a credential can put on disk has to be declared in
    /// [`Credential::MODELLED`], or the next write would treat this build's own field as
    /// somebody else's and resurrect a value the user just cleared.
    #[test]
    fn every_modelled_key_is_declared() {
        let mut written = std::collections::BTreeSet::new();
        for credential in [
            Credential::Oauth {
                refresh: Secret::new("r"),
                access: Secret::new("a"),
                expires: 1,
                account_id: Some("acct".to_owned()),
                enterprise_url: Some("https://enterprise.test".to_owned()),
            },
            Credential::Api {
                key: Secret::new("k"),
                metadata: Some(BTreeMap::from([("m".to_owned(), Secret::new("v"))])),
            },
            Credential::WellKnown {
                key: Secret::new("k"),
                token: Secret::new("t"),
            },
        ] {
            let value = serde_json::to_value(&credential).expect("encode");
            let object = value.as_object().expect("an entry is an object");
            written.extend(object.keys().cloned());
        }

        let declared: std::collections::BTreeSet<String> = Credential::MODELLED
            .keys
            .iter()
            .map(|key| (*key).to_owned())
            .collect();
        assert_eq!(
            written, declared,
            "Credential::MODELLED must name exactly the keys a credential writes"
        );
        assert!(
            Credential::MODELLED.within.is_empty(),
            "metadata's values are a free-form map in the model, so nothing below this level \
             needs a shape"
        );
    }

    /// The reviewer's measured input, verbatim: an entry this build cannot decode at
    /// all. It has to be reachable — reported, listable and removable — or preservation
    /// turns silent deletion into permanent, invisible accumulation.
    #[test]
    fn an_entry_this_build_cannot_decode_is_reachable_and_removable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
              "newer-zuno": { "type": "passkey", "attestation": "AAECAwQ", "counter": 7 },
              "anthropic": { "type": "api", "key": "k" }
            }"#,
        )
        .expect("seed");

        let store = AuthStore::new(&path);
        let loaded = store.all().expect("all");
        assert_eq!(loaded.entries.keys().collect::<Vec<_>>(), vec!["anthropic"]);
        assert_eq!(loaded.skipped, vec!["newer-zuno".to_owned()]);

        // The three accessors a client surface needs to name it.
        assert!(
            loaded.contains("newer-zuno"),
            "logout has to be able to resolve it"
        );
        assert!(loaded.is_preserved("newer-zuno"));
        assert!(!loaded.is_preserved("anthropic"));
        assert_eq!(
            loaded.names().collect::<Vec<_>>(),
            vec!["anthropic", "newer-zuno"],
            "a listing that omits it is how it becomes invisible"
        );

        // And the removal path, from inside this crate's API.
        store.remove("newer-zuno").expect("remove");
        let after = store.all().expect("all");
        assert!(after.skipped.is_empty());
        assert!(!after.contains("newer-zuno"));
        assert_eq!(after.entries.keys().collect::<Vec<_>>(), vec!["anthropic"]);
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(!text.contains("passkey"), "{text}");
        assert!(text.contains("\"k\""), "{text}");
    }

    /// Junk nobody can decode is still removable one key at a time, which is what stops
    /// a credential file growing monotonically with content the user cannot see.
    #[test]
    fn junk_entries_are_each_removable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"a":null,"b":[1,2],"c":"string","d":42}"#).expect("seed");

        let store = AuthStore::new(&path);
        assert_eq!(
            store.all().expect("all").skipped,
            vec![
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "d".to_owned()
            ]
        );
        for key in ["a", "b", "c", "d"] {
            store.remove(key).expect("remove");
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "{}",
            "every preserved entry has to be reachable by the name the read reported"
        );
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
    /// A mutation under an override publishes the override to the file, so an entry
    /// the override held and this build could not decode has to survive that
    /// publication for the same reason the file's own do.
    #[test]
    fn a_mutation_under_an_override_keeps_a_shape_it_could_not_decode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let overridden = AuthStore::with_env(
            &path,
            &env(r#"{"envgamma":{"type":"api","key":"sk-env-gamma"},
                    "newer-zuno":{"type":"passkey","counter":7}}"#),
        );

        overridden.set("openai", api()).expect("login");

        let published: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("store");
        assert_eq!(
            published.get("newer-zuno"),
            Some(&serde_json::json!({ "type": "passkey", "counter": 7 })),
            "the override's unreadable entry must reach the file too: {published:?}"
        );
        assert!(published.contains_key("envgamma") && published.contains_key("openai"));
    }

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

    /// The zero-byte `auth.json` the shipped 0.6.6 truncate window left behind is
    /// what a user upgrades into, and `zuno auth login` is the command that repairs
    /// it. It has to land. An empty file holds no entry the write could destroy, so
    /// refusing bought nothing and cost the repair.
    #[test]
    fn a_login_over_an_emptied_file_repairs_it_instead_of_being_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        std::fs::write(store.path(), b"").expect("what an interrupted write left");

        store
            .set("openai", api())
            .expect("the login that repairs the file must not be denied");

        let loaded = store.all().expect("read back");
        assert_eq!(loaded.entries.get("openai"), Some(&api()));
        assert_eq!(loaded.damage, None, "the repair write clears the damage");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(store.path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the repair is still a private publication");
        }
    }

    /// A file holding only whitespace is the same finding, and the same repair: the
    /// reviewer measured `" \t\r\n "` reading identically to a zero-byte file, so the
    /// write has to treat it identically too.
    #[test]
    fn a_whitespace_only_file_is_repaired_by_a_login_as_well() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        std::fs::write(store.path(), b" \t\r\n ").expect("what a truncation left");

        store.set("openai", api()).expect("the login must land");

        assert_eq!(store.all().expect("read back").entries.len(), 1);
    }

    /// `remove` is a read-modify-write too, and `zuno auth logout` against an emptied
    /// file publishes a real (empty) store over it rather than failing: there is no
    /// evidence in a file with no bytes to protect.
    #[test]
    fn a_logout_over_an_emptied_file_publishes_a_store_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        std::fs::write(store.path(), b"").expect("what an interrupted write left");

        store.remove("openai").expect("logout must not be denied");

        assert_eq!(
            std::fs::read_to_string(store.path()).expect("read back"),
            "{}"
        );
        assert_eq!(store.all().expect("read back").damage, None);
    }

    /// The other side of the split, and the reason it exists. The zero-byte
    /// `auth.json` the shipped truncate bug leaves is what a user upgrades into, and
    /// `zuno auth list` (providers.rs), `zuno models` (models.rs:25) and the run path
    /// (turn.rs:588 `auth_store.all()`) all reach the file through `all()` — including
    /// for a user whose model credential comes from a provider environment variable
    /// and who never needed this file. Refusing there would deny every one of those
    /// commands, and the login that repairs the file with them. The read carries on
    /// and reports the damage instead.
    #[test]
    fn a_zero_byte_file_still_reads_and_reports_the_damage_rather_than_locking_the_user_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        std::fs::write(store.path(), b"").expect("what an interrupted write left");

        let loaded = store.all().expect("a read must not be denied");

        assert!(loaded.is_empty(), "there is nothing left to report");
        let damage = loaded.damage.expect("the damage must travel with the read");
        assert_eq!(damage.path(), store.path());
        let rendered = damage.to_string();
        for expected in [
            "holds no store",
            "restore a backup",
            "log in again and the next write replaces it",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
        // `get`, which every provider request uses to find its credential, is the
        // same read: it must answer "no credential for this provider", not fail.
        assert_eq!(store.get("openai").expect("get must not be denied"), None);
        assert_eq!(
            std::fs::read(store.path()).expect("read back").len(),
            0,
            "a read must never repair or replace the damaged file"
        );
    }

    /// A healthy store must not acquire a damage report, or every surface that shows
    /// one would cry wolf on every read.
    #[test]
    fn a_healthy_store_reports_no_damage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("openai", api()).expect("set");

        assert_eq!(store.all().expect("all").damage, None);
        // A legitimately empty store — logged out of everything — is not damage
        // either.
        store.remove("openai").expect("remove");
        let loaded = store.all().expect("all");
        assert!(loaded.is_empty());
        assert_eq!(loaded.damage, None);
        assert_eq!(std::fs::read_to_string(store.path()).expect("read"), "{}");
    }

    /// With `ZUNO_AUTH_CONTENT` in effect the file is not the authority for any read,
    /// so a write over a damaged file ratifies nothing that was not already ignored.
    /// Pinned so the presence check cannot later be moved in front of the override and
    /// break a documented deployment.
    #[test]
    fn the_environment_override_still_writes_over_an_emptied_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(&path, b"").expect("what an interrupted write left");
        let store =
            AuthStore::with_env(&path, &env(r#"{"envgamma":{"type":"api","key":"sk-env"}}"#));

        store
            .set("openai", api())
            .expect("the override is the store");

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert!(written.get("envgamma").is_some(), "{written}");
        assert!(written.get("openai").is_some(), "{written}");
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
