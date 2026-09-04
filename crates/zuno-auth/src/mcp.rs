//! `mcp-auth.json` — per-MCP-server OAuth state.
//!
//! A port of `packages/opencode/src/mcp/auth.ts`. The file is a JSON object keyed
//! by MCP server name, each value an [`Entry`] whose every field is optional
//! because the OAuth dance fills them in over several steps:
//!
//! | field | oracle | what it is |
//! | --- | --- | --- |
//! | `tokens` | `mcp/auth.ts:9-14`, `:26` | access/refresh token pair, expiry, scope |
//! | `clientInfo` | `mcp/auth.ts:17-22`, `:27` | RFC 7591 dynamic client registration result |
//! | `codeVerifier` | `mcp/auth.ts:28` | the PKCE verifier, held between redirect and callback |
//! | `oauthState` | `mcp/auth.ts:29` | the CSRF `state` parameter, checked on callback |
//! | `serverUrl` | `mcp/auth.ts:30` | which URL these credentials were issued for |
//!
//! Written at `0600` — `mcp/auth.ts:80`.
//!
//! # Storage only
//!
//! This module holds the state; it does not perform the flow. Building
//! authorization URLs, running the loopback callback, exchanging a code, and
//! refreshing a token belong to task 46. Every method here is a read or a
//! read-modify-write of the file.
//!
//! # Two divergences, both deliberate
//!
//! **Per-entry decoding.** The oracle decodes the whole map in one go and falls
//! back to `{}` when any part of it fails — `mcp/auth.ts:67`. One malformed
//! entry therefore discards every server's credentials, and because
//! `mutate` writes the result back (`:76-82`), the next update makes that
//! permanent. This module decodes entry by entry, keeps the ones that are
//! intelligible, and reports the rest in [`McpCredentials::skipped`]. Nothing a
//! working oracle wrote is read differently; only the blast radius of damage
//! changes.
//!
//! An entry it could not decode is also republished verbatim by the next write —
//! [`McpCredentials::preserved`]. The oracle drops those, which turns one token
//! refresh into the permanent loss of a server's credential whose shape this build
//! merely does not model; a store that cannot read a value has no business deleting
//! it. Only [`McpAuthStore::remove`] deletes one, because the user asked for it.
//!
//! **No file lock.** The oracle wraps every read and write in an flock —
//! `mcp/auth.ts:73`, `:81`. No locking crate is pinned in this workspace's
//! `Cargo.toml`, and task 24 must not edit it, so two concurrent updates here can
//! lose one another. [`McpAuthStore::mutate`] is the single choke point through
//! which every write passes, so the lock has exactly one place to go. Recorded in
//! the project's engineering notes for task 46, which owns the MCP OAuth
//! flow.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zuno_paths::Layout;

use crate::error::AuthError;
use crate::secret::Secret;
use crate::store::{self, Modelled, PermissionWarning, StoreDamage, Unmodelled};

/// An OAuth token pair for one MCP server — `mcp/auth.ts:9-14`.
///
/// The timestamps are `Schema.Number` in the oracle rather than a non-negative
/// integer, so unlike `auth.json`'s `expires` they are modelled as [`i64`]. Every
/// producer computes them as whole milliseconds or seconds; a fractional value
/// would fail to decode and the entry would be skipped.
///
/// Deliberately not [`Default`]: `access_token` is the one field the oracle
/// requires, and a `Tokens` without it is not a thing that can exist. The same
/// goes for [`ClientInfo`] and its `client_id`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    /// The bearer token sent to the server.
    pub access_token: Secret,
    /// The token used to mint a new `access_token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<Secret>,
    /// Expiry as a Unix timestamp in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// The space-separated scopes the token was granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Tokens {
    /// Every key this build can write inside a `tokens` object, in its on-disk spelling.
    ///
    /// A token response is the likeliest place a newer Zuno adds a field — `tokenType`
    /// and `idToken` are both standard and neither is modelled here — so the keys inside
    /// `tokens` are carried through a write as well as the ones beside it. Adding a field
    /// means adding its spelling here; `every_modelled_key_is_declared` fails until it is.
    pub const MODELLED: &'static Modelled = &Modelled {
        keys: &["accessToken", "refreshToken", "expiresAt", "scope"],
        within: &[],
    };
}

/// A dynamically registered OAuth client — `mcp/auth.ts:17-22`.
///
/// `client_id` is deliberately **not** a [`Secret`]: an OAuth client ID is public
/// by design, it travels in query strings, and hiding it would make a log line
/// useless without protecting anything. `client_secret` is a secret.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// The public client identifier the server issued.
    pub client_id: String,
    /// The confidential half, for clients that get one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<Secret>,
    /// When the client ID was issued, as a Unix timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<i64>,
    /// When the client secret expires, as a Unix timestamp. `0` means never, per
    /// RFC 7591.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
}

/// One MCP server's stored OAuth state — `mcp/auth.ts:25-31`.
///
/// Every field is optional because the entry is built up across the flow: the
/// client registration lands first, then the verifier and state before the
/// redirect, then the tokens on callback.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// The token pair, once the exchange has happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// The registered client, once registration has happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
    /// The PKCE verifier, held only between the redirect and the callback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<Secret>,
    /// The CSRF `state`, held only between the redirect and the callback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_state: Option<Secret>,
    /// The server URL these credentials belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
}

impl ClientInfo {
    /// Every key this build can write inside a `clientInfo` object, in its on-disk
    /// spelling. RFC 7591 defines many more registration response fields than the four
    /// modelled here, so a server that returns them keeps them.
    pub const MODELLED: &'static Modelled = &Modelled {
        keys: &[
            "clientId",
            "clientSecret",
            "clientIdIssuedAt",
            "clientSecretExpiresAt",
        ],
        within: &[],
    };
}

impl Entry {
    /// Every key this build can write in one `mcp-auth.json` entry, and the modelled
    /// objects below it.
    ///
    /// This is what tells a write which keys belong to it. A key here is written from the
    /// typed value or not written at all, so [`McpAuthStore::clear_code_verifier`] really
    /// removes the verifier. A key that is not here belongs to whoever wrote the file and
    /// is carried through the write by [`store::unmodelled_fields`] — at this level and
    /// inside `tokens` and `clientInfo`.
    pub const MODELLED: &'static Modelled = &Modelled {
        keys: &[
            "tokens",
            "clientInfo",
            "codeVerifier",
            "oauthState",
            "serverUrl",
        ],
        within: &[
            ("tokens", Tokens::MODELLED),
            ("clientInfo", ClientInfo::MODELLED),
        ],
    };

    /// Whether this entry has nothing in it — the state `updateField` starts from
    /// when a server is seen for the first time.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Everything one read of `mcp-auth.json` produced.
///
/// `Eq` is deliberately absent: `preserved` holds arbitrary JSON, and
/// `serde_json::Value` is only `PartialEq`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpCredentials {
    /// The decodable entries, keyed by MCP server name.
    pub entries: BTreeMap<String, Entry>,
    /// Set when the file on disk was group- or world-accessible.
    pub permissions: Option<PermissionWarning>,
    /// Server names whose value did not decode. They are absent from `entries` and
    /// kept in `preserved`, so a read reports them and a write keeps them.
    pub skipped: Vec<String>,
    /// The values behind `skipped`, as the file held them (see [`store::Rewritten`]).
    /// A write
    /// republishes these untouched, so authorizing one server cannot delete another
    /// server's tokens because their shape is unrecognized.
    pub preserved: BTreeMap<String, serde_json::Value>,
    /// What each decoded entry held that [`Entry::MODELLED`] does not name, keyed by
    /// server name and nested one level into `tokens` and `clientInfo`. Absent for an
    /// entry that held nothing extra.
    ///
    /// The half `skipped`/`preserved` cannot see: every field of [`Entry`] is optional
    /// with `#[serde(default)]` and no `deny_unknown_fields`, so a field a newer Zuno
    /// added decodes successfully into nothing and the typed map written back would drop
    /// it — from entries a token refresh never touched as much as from the one it did.
    pub unmodelled: BTreeMap<String, Unmodelled>,
    /// Set when the file exists and held no store at all. `entries` is empty then
    /// because the store was destroyed, not because no server has been authorized.
    pub damage: Option<StoreDamage>,
}

impl McpCredentials {
    /// One server's entry.
    #[must_use]
    pub fn get(&self, server: &str) -> Option<&Entry> {
        self.entries.get(server)
    }

    /// How many entries were understood.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry was understood.
    ///
    /// Says nothing about `preserved`: a file holding only entries this build cannot
    /// decode is `is_empty()` and is not empty. See [`McpCredentials::names`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every server name the file holds, decoded or not, in one sorted sequence.
    ///
    /// The resolution a command that names a server needs. An entry this build could not
    /// decode is republished by every write, so refusing to resolve it makes it permanent
    /// and unreachable.
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

    /// Whether the file holds this server at all, decoded or not.
    #[must_use]
    pub fn contains(&self, server: &str) -> bool {
        self.entries.contains_key(server) || self.preserved.contains_key(server)
    }

    /// Whether this server is present but was not decoded, so a surface listing it can
    /// say the build does not understand it and [`McpAuthStore::remove`] is how it goes
    /// away.
    #[must_use]
    pub fn is_preserved(&self, server: &str) -> bool {
        self.preserved.contains_key(server)
    }
}

/// Reader and writer for `mcp-auth.json`.
#[derive(Clone, Debug)]
pub struct McpAuthStore {
    path: PathBuf,
}

impl McpAuthStore {
    /// A store over an explicit path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store for a resolved layout — `data()/mcp-auth.json`.
    ///
    /// There is no environment override for this file: `mcp/auth.ts` has no
    /// counterpart to `ZUNO_AUTH_CONTENT`.
    #[must_use]
    pub fn resolve(layout: &Layout) -> Self {
        Self::new(layout.mcp_auth_file())
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every entry — `mcp/auth.ts:65-74`.
    ///
    /// A file that exists and holds no store is reported in
    /// [`McpCredentials::damage`] rather than refused: this read backs
    /// `zuno mcp list` and every tool call that consults a server's token, and
    /// failing those would deny the login that repairs the file. The refusal belongs
    /// on the read that precedes a write — see [`McpAuthStore::mutate`].
    pub fn all(&self) -> Result<McpCredentials, AuthError> {
        Ok(self.decode(store::read_json(&self.path)?))
    }

    /// [`McpAuthStore::all`] for a caller that will write the result back.
    fn all_for_update(&self) -> Result<McpCredentials, AuthError> {
        Ok(self.decode(store::read_json_for_update(&self.path)?))
    }

    /// Turn one file read into entries, moving values this build does not recognize
    /// into [`McpCredentials::skipped`] and [`McpCredentials::preserved`].
    fn decode(&self, outcome: store::Read<BTreeMap<String, serde_json::Value>>) -> McpCredentials {
        let mut entries = BTreeMap::new();
        let mut skipped = Vec::new();
        let mut preserved = BTreeMap::new();
        let mut unmodelled = BTreeMap::new();
        for (server, value) in outcome.value {
            match serde_json::from_value::<Entry>(value.clone()) {
                Ok(entry) => {
                    let extra = store::unmodelled_fields(&value, Entry::MODELLED);
                    if !extra.is_empty() {
                        unmodelled.insert(server.clone(), extra);
                    }
                    entries.insert(server, entry);
                }
                Err(_) => {
                    tracing::warn!(
                        server = %server,
                        path = %self.path.display(),
                        "mcp auth entry is not a recognized shape; it is kept as it stands and \
                         reported, not decoded"
                    );
                    skipped.push(server.clone());
                    preserved.insert(server, value);
                }
            }
        }

        McpCredentials {
            entries,
            permissions: outcome.permissions,
            skipped,
            preserved,
            unmodelled,
            damage: outcome.damage,
        }
    }

    /// One server's entry — `mcp/auth.ts:84-87`.
    pub fn get(&self, server: &str) -> Result<Option<Entry>, AuthError> {
        Ok(self.all()?.entries.remove(server))
    }

    /// One server's entry, but only if it was issued for `server_url` —
    /// `mcp/auth.ts:89-95`.
    ///
    /// Returns `None` when the entry is absent, when it records no `serverUrl` at
    /// all, or when the recorded one differs. The middle case matters: an entry
    /// whose URL was never recorded is not assumed to match, because reusing a
    /// token against a different endpoint would hand it to the wrong server.
    pub fn get_for_url(&self, server: &str, server_url: &str) -> Result<Option<Entry>, AuthError> {
        let Some(entry) = self.get(server)? else {
            return Ok(None);
        };
        match &entry.server_url {
            Some(recorded) if recorded == server_url => Ok(Some(entry)),
            Some(_) | None => Ok(None),
        }
    }

    /// Read, apply `update`, and write back at `0600` — `mcp/auth.ts:76-82`.
    ///
    /// `update` returning `false` means "nothing changed", and then no write
    /// happens at all. That is the oracle's `if (!next) return`, and it is what
    /// makes clearing a field on an absent entry leave the file untouched rather
    /// than rewriting it identically.
    ///
    /// Every write in this module goes through here, which is where a file lock
    /// belongs once one is available.
    ///
    /// The read is [`store::read_json_for_update`]: a file that holds no store, or
    /// one whose absence could not be confirmed, stops the write instead of becoming
    /// a store holding only this server's tokens.
    ///
    /// `update` sees only the entries this build decoded. The ones it did not are
    /// republished verbatim around the update, so a token refresh for one server
    /// cannot delete an entry whose shape a newer Zuno wrote. A caller that must
    /// reach those too takes [`McpAuthStore::mutate_document`] — deleting an entry
    /// nobody can decode is the one operation a user has no other way to perform.
    fn mutate<F>(&self, update: F) -> Result<(), AuthError>
    where
        F: FnOnce(&mut BTreeMap<String, Entry>) -> bool,
    {
        self.mutate_document(|loaded| update(&mut loaded.entries))
    }

    /// [`McpAuthStore::mutate`] for an update that must also see the entries this
    /// build could not decode, or the fields of a decoded entry that it does not model.
    ///
    /// Every map in [`McpCredentials`] is republished as one document, so an entry left
    /// in `preserved` survives and an entry removed from it is gone.
    ///
    /// `unmodelled` is keyed by the decoded entries, and [`store::settle_unmodelled`] is
    /// where that invariant is restored against what `update` actually did: a carried key
    /// whose entry is gone goes with it, and a carried key that lived inside a modelled
    /// object this write rewrote is not re-attached to the value that replaced it. A
    /// `remove` therefore does not have to remember that a third map exists.
    fn mutate_document<F>(&self, update: F) -> Result<(), AuthError>
    where
        F: FnOnce(&mut McpCredentials) -> bool,
    {
        let mut loaded = self.all_for_update()?;
        let before = loaded.entries.clone();
        if !update(&mut loaded) {
            return Ok(());
        }
        let McpCredentials {
            entries,
            preserved,
            unmodelled,
            ..
        } = &mut loaded;
        store::settle_unmodelled(&before, entries, unmodelled);
        store::write_json(&self.path, &store::rewrite(entries, unmodelled, preserved))
    }

    /// Store `entry` under `server`, optionally stamping it with `server_url` —
    /// `mcp/auth.ts:97-102`.
    pub fn set(
        &self,
        server: &str,
        entry: Entry,
        server_url: Option<&str>,
    ) -> Result<(), AuthError> {
        let mut entry = entry;
        if let Some(url) = server_url {
            entry.server_url = Some(url.to_owned());
        }
        self.mutate_document(|loaded| {
            loaded.entries.insert(server.to_owned(), entry);
            // This replaces the server's entry outright. Keys a newer Zuno added to the
            // entry it replaces are claims about *that* registration — a bound device, a
            // proof-of-possession key — and re-attaching them to a freshly authorized
            // one would hand another build's authority claims to tokens it never saw.
            // A newer Zuno writing this entry again restores them.
            loaded.unmodelled.remove(server);
            true
        })
    }

    /// Remove `server`'s entry — `mcp/auth.ts:104-110`.
    ///
    /// Writes unconditionally, as the oracle does: its `remove` always returns a
    /// map, so `mutate` always writes even when nothing was there.
    pub fn remove(&self, server: &str) -> Result<(), AuthError> {
        self.mutate_document(|loaded| {
            loaded.entries.remove(server);
            loaded.preserved.remove(server);
            true
        })
    }

    /// Set `tokens`, creating the entry if this server is new —
    /// `mcp/auth.ts:112-120`, `:132`.
    pub fn update_tokens(
        &self,
        server: &str,
        tokens: Tokens,
        server_url: Option<&str>,
    ) -> Result<(), AuthError> {
        self.update_field(server, server_url, |entry| entry.tokens = Some(tokens))
    }

    /// Set `client_info`, creating the entry if this server is new —
    /// `mcp/auth.ts:112-120`, `:133`.
    pub fn update_client_info(
        &self,
        server: &str,
        client_info: ClientInfo,
        server_url: Option<&str>,
    ) -> Result<(), AuthError> {
        self.update_field(server, server_url, |entry| {
            entry.client_info = Some(client_info);
        })
    }

    /// Set the PKCE verifier — `mcp/auth.ts:134`.
    pub fn update_code_verifier(
        &self,
        server: &str,
        code_verifier: Secret,
    ) -> Result<(), AuthError> {
        self.update_field(server, None, |entry| {
            entry.code_verifier = Some(code_verifier);
        })
    }

    /// Drop the PKCE verifier once the exchange is done — `mcp/auth.ts:136`.
    ///
    /// A no-op, with no write, when the server has no entry.
    pub fn clear_code_verifier(&self, server: &str) -> Result<(), AuthError> {
        self.clear_field(server, |entry| entry.code_verifier = None)
    }

    /// Set the CSRF `state` — `mcp/auth.ts:135`.
    pub fn update_oauth_state(&self, server: &str, oauth_state: Secret) -> Result<(), AuthError> {
        self.update_field(server, None, |entry| {
            entry.oauth_state = Some(oauth_state);
        })
    }

    /// The CSRF `state` to compare a callback against — `mcp/auth.ts:139-142`.
    pub fn get_oauth_state(&self, server: &str) -> Result<Option<Secret>, AuthError> {
        Ok(self.get(server)?.and_then(|entry| entry.oauth_state))
    }

    /// Drop the CSRF `state` once the callback is handled — `mcp/auth.ts:137`.
    ///
    /// A no-op, with no write, when the server has no entry.
    pub fn clear_oauth_state(&self, server: &str) -> Result<(), AuthError> {
        self.clear_field(server, |entry| entry.oauth_state = None)
    }

    /// `updateField` — `mcp/auth.ts:112-120`. Creates a default entry when the
    /// server is unknown, and stamps `server_url` when one is given.
    fn update_field<F>(
        &self,
        server: &str,
        server_url: Option<&str>,
        apply: F,
    ) -> Result<(), AuthError>
    where
        F: FnOnce(&mut Entry),
    {
        self.mutate(|entries| {
            let entry = entries.entry(server.to_owned()).or_default();
            apply(entry);
            if let Some(url) = server_url {
                entry.server_url = Some(url.to_owned());
            }
            true
        })
    }

    /// `clearField` — `mcp/auth.ts:122-130`. Returns without writing when the
    /// server has no entry.
    fn clear_field<F>(&self, server: &str, apply: F) -> Result<(), AuthError>
    where
        F: FnOnce(&mut Entry),
    {
        self.mutate(|entries| match entries.get_mut(server) {
            Some(entry) => {
                apply(entry);
                true
            }
            None => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const SERVER: &str = "acme-mcp";
    const URL: &str = "https://mcp.example.test/sse";

    fn store_in(dir: &tempfile::TempDir) -> McpAuthStore {
        McpAuthStore::new(dir.path().join("mcp-auth.json"))
    }

    fn tokens() -> Tokens {
        Tokens {
            access_token: Secret::new("mcp-access-token-0001"),
            refresh_token: Some(Secret::new("mcp-refresh-token-0001")),
            expires_at: Some(1_893_456_000_000),
            scope: Some("read write".to_owned()),
        }
    }

    fn client_info() -> ClientInfo {
        ClientInfo {
            client_id: "client-public-id".to_owned(),
            client_secret: Some(Secret::new("mcp-client-secret-0002")),
            client_id_issued_at: Some(1_700_000_000),
            client_secret_expires_at: Some(0),
        }
    }

    fn full_entry() -> Entry {
        Entry {
            tokens: Some(tokens()),
            client_info: Some(client_info()),
            code_verifier: Some(Secret::new("pkce-verifier-0003")),
            oauth_state: Some(Secret::new("csrf-state-0004")),
            server_url: Some(URL.to_owned()),
        }
    }

    #[test]
    fn a_full_entry_round_trips_field_for_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set(SERVER, full_entry(), None).expect("set");
        assert_eq!(store.get(SERVER).expect("get"), Some(full_entry()));
    }

    #[test]
    fn the_serialized_field_names_are_the_oracle_spellings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set(SERVER, full_entry(), None).expect("set");

        let text = std::fs::read_to_string(store.path()).expect("read");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
        let entry = &json[SERVER];

        assert_eq!(entry["tokens"]["accessToken"], "mcp-access-token-0001");
        assert_eq!(entry["tokens"]["refreshToken"], "mcp-refresh-token-0001");
        assert_eq!(entry["tokens"]["expiresAt"], 1_893_456_000_000_i64);
        assert_eq!(entry["tokens"]["scope"], "read write");
        assert_eq!(entry["clientInfo"]["clientId"], "client-public-id");
        assert_eq!(
            entry["clientInfo"]["clientSecret"],
            "mcp-client-secret-0002"
        );
        assert_eq!(entry["clientInfo"]["clientIdIssuedAt"], 1_700_000_000);
        assert_eq!(entry["clientInfo"]["clientSecretExpiresAt"], 0);
        assert_eq!(entry["codeVerifier"], "pkce-verifier-0003");
        assert_eq!(entry["oauthState"], "csrf-state-0004");
        assert_eq!(entry["serverUrl"], URL);
    }

    #[test]
    fn absent_optional_fields_are_omitted_not_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store
            .set(
                SERVER,
                Entry {
                    tokens: Some(Tokens {
                        access_token: Secret::new("at"),
                        refresh_token: None,
                        expires_at: None,
                        scope: None,
                    }),
                    ..Entry::default()
                },
                None,
            )
            .expect("set");

        let text = std::fs::read_to_string(store.path()).expect("read");
        assert!(!text.contains("null"), "{text}");
        assert!(!text.contains("refreshToken"), "{text}");
        assert!(!text.contains("clientInfo"), "{text}");
        assert!(!text.contains("codeVerifier"), "{text}");
        assert_eq!(text.matches("accessToken").count(), 1, "{text}");
    }

    #[test]
    fn a_file_the_typescript_binary_would_write_decodes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-auth.json");
        std::fs::write(
            &path,
            r#"{
  "acme-mcp": {
    "tokens": { "accessToken": "at", "refreshToken": "rt", "expiresAt": 1893456000000, "scope": "read" },
    "clientInfo": { "clientId": "cid", "clientSecret": "cs", "clientIdIssuedAt": 1700000000, "clientSecretExpiresAt": 0 },
    "codeVerifier": "cv",
    "oauthState": "st",
    "serverUrl": "https://mcp.example.test/sse"
  },
  "bare-mcp": {}
}"#,
        )
        .expect("seed");

        let loaded = McpAuthStore::new(&path).all().expect("all");
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get("bare-mcp").expect("bare").is_empty());

        let entry = loaded.get("acme-mcp").expect("acme");
        let stored = entry.tokens.as_ref().expect("tokens");
        assert_eq!(stored.access_token.expose(), "at");
        assert_eq!(
            stored.refresh_token.as_ref().map(Secret::expose),
            Some("rt")
        );
        assert_eq!(stored.expires_at, Some(1_893_456_000_000));
        assert_eq!(entry.client_info.as_ref().expect("client").client_id, "cid");
        assert_eq!(entry.code_verifier.as_ref().map(Secret::expose), Some("cv"));
        assert_eq!(entry.oauth_state.as_ref().map(Secret::expose), Some("st"));
        assert_eq!(entry.server_url.as_deref(), Some(URL));
    }

    #[test]
    fn undecodable_entries_are_skipped_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-auth.json");
        std::fs::write(
            &path,
            r#"{
  "good": { "tokens": { "accessToken": "at" } },
  "notanobject": 42,
  "badtokens": { "tokens": "not-an-object" },
  "notokenfield": { "tokens": { "refreshToken": "rt" } }
}"#,
        )
        .expect("seed");

        let loaded = McpAuthStore::new(&path).all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("good").is_some());
        assert_eq!(
            loaded.skipped,
            vec![
                "badtokens".to_owned(),
                "notanobject".to_owned(),
                "notokenfield".to_owned()
            ]
        );
    }

    /// The reviewer's input in this store's shape: an `mcp-auth.json` holding an
    /// entry this build cannot decode, and one token refresh for a different server.
    ///
    /// The OAuth refresh path (`crates/zuno-mcp/src/oauth/token.rs`) calls
    /// `update_tokens` on every expiry, so before the write carried undecoded values
    /// this deleted the other server's entry without anyone running a command.
    #[test]
    fn a_token_refresh_keeps_an_entry_shape_this_build_cannot_decode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-auth.json");
        let unrecognized =
            r#"{ "tokens": { "accessToken": { "envelope": "v2", "cipher": "AAEC" } } }"#;
        std::fs::write(&path, format!(r#"{{ "newer-zuno": {unrecognized} }}"#)).expect("seed");

        let store = McpAuthStore::new(&path);
        store
            .update_tokens(SERVER, tokens(), Some(URL))
            .expect("refresh");

        let published: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("the file must still be a store");
        assert_eq!(
            published.get("newer-zuno"),
            Some(&serde_json::from_str::<serde_json::Value>(unrecognized).expect("value")),
            "a refresh for another server must not delete an entry it could not read"
        );
        assert!(published.contains_key(SERVER));
        assert_eq!(
            store.all().expect("all").skipped,
            vec!["newer-zuno".to_owned()]
        );
    }

    /// `zuno mcp logout` on a server whose entry cannot be decoded must still remove
    /// it: nothing else in the product can.
    /// The reviewer's measured input, verbatim. Every field of [`Entry`] is optional with
    /// `#[serde(default)]` and there is no `deny_unknown_fields`, so a field a newer Zuno
    /// added decodes successfully into nothing: `skipped` stays empty, the store believes
    /// nothing is at risk, and the typed map written back used to drop the field — from
    /// the entry the write touched and from the one it did not.
    #[test]
    fn a_field_a_newer_zuno_added_survives_a_token_refresh_on_both_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-auth.json");
        std::fs::write(
            &path,
            r#"{
              "srv": {
                "tokens": { "accessToken": "old" },
                "serverUrl": "https://srv.example.test/sse",
                "dpopKey": { "kty": "EC", "crv": "P-256" },
                "boundDevice": true
              },
              "other": {
                "clientInfo": { "clientId": "other-client" },
                "futureShape": [1]
              }
            }"#,
        )
        .expect("seed");

        let store = McpAuthStore::new(&path);
        let loaded = store.all().expect("all");
        assert_eq!(loaded.entries.len(), 2);
        assert!(
            loaded.skipped.is_empty() && loaded.preserved.is_empty(),
            "an added field does not make an entry undecodable, which is the whole gap"
        );

        store
            .update_tokens("srv", tokens(), None)
            .expect("one refresh");

        let published: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("json");
        assert_eq!(
            published["srv"]["dpopKey"]["kty"],
            serde_json::json!("EC"),
            "{published}"
        );
        assert_eq!(
            published["srv"]["dpopKey"]["crv"],
            serde_json::json!("P-256"),
            "{published}"
        );
        assert_eq!(
            published["srv"]["boundDevice"],
            serde_json::json!(true),
            "{published}"
        );
        assert_eq!(
            published["other"]["futureShape"],
            serde_json::json!([1]),
            "an entry the write never touched must not lose a field: {published}"
        );
        // The modelled half is still written from the typed value.
        assert_eq!(
            published["srv"]["tokens"]["accessToken"],
            serde_json::json!("mcp-access-token-0001"),
            "{published}"
        );
        assert_eq!(
            published["srv"]["serverUrl"],
            serde_json::json!("https://srv.example.test/sse"),
            "a refresh with no URL must not drop the recorded one: {published}"
        );
        assert_eq!(
            published["other"]["clientInfo"]["clientId"],
            serde_json::json!("other-client")
        );
    }

    /// A field inside a modelled object is carried too, but only while that object is the
    /// one that was read. `tokenType` beside an untouched `accessToken` survives an
    /// unrelated write; a rotation of the tokens themselves does **not** re-attach it,
    /// because an unknown key inside `tokens` is a claim about the tokens that were there
    /// and a rotated token never carried it.
    #[test]
    fn a_field_inside_a_modelled_object_is_carried_but_not_re_attached_to_a_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-auth.json");
        let seed = r#"{
          "srv": {
            "tokens": { "accessToken": "old", "tokenType": "DPoP" },
            "clientInfo": { "clientId": "c", "softwareStatement": "jwt" }
          }
        }"#;

        // An unrelated write leaves both nested carries in place.
        std::fs::write(&path, seed).expect("seed");
        let store = McpAuthStore::new(&path);
        store
            .update_tokens("elsewhere", tokens(), Some(URL))
            .expect("another server");
        let published: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("json");
        assert_eq!(
            published["srv"]["tokens"]["tokenType"],
            serde_json::json!("DPoP"),
            "{published}"
        );
        assert_eq!(
            published["srv"]["clientInfo"]["softwareStatement"],
            serde_json::json!("jwt"),
            "{published}"
        );

        // Rotating this server's tokens drops the carry that lived inside `tokens`, and
        // keeps the one inside the object the write did not rewrite.
        std::fs::write(&path, seed).expect("reseed");
        store
            .update_tokens("srv", tokens(), None)
            .expect("rotate this server");
        let published: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("json");
        assert!(
            published["srv"]["tokens"].get("tokenType").is_none(),
            "a rotated token must not inherit the replaced token's unknown claims: {published}"
        );
        assert_eq!(
            published["srv"]["clientInfo"]["softwareStatement"],
            serde_json::json!("jwt"),
            "the object this write did not rewrite keeps its carry: {published}"
        );
    }

    /// Clearing a field this build models really clears it. Without the declared shape a
    /// cleared `codeVerifier` would look "on disk, absent from the typed value" — exactly
    /// like an added field — and the next write would resurrect the verifier the exchange
    /// just consumed.
    #[test]
    fn clearing_a_modelled_field_is_not_undone_by_the_carry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set(SERVER, full_entry(), Some(URL)).expect("seed");
        // A field this build does not model, beside the ones it does.
        let text = std::fs::read_to_string(store.path()).expect("read");
        let mut document: serde_json::Value = serde_json::from_str(&text).expect("json");
        document[SERVER]["futureShape"] = serde_json::json!("kept");
        std::fs::write(store.path(), document.to_string()).expect("reseed");

        store.clear_code_verifier(SERVER).expect("clear");

        let published: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.path()).expect("read back"))
                .expect("json");
        assert!(
            published[SERVER].get("codeVerifier").is_none(),
            "{published}"
        );
        assert_eq!(published[SERVER]["futureShape"], serde_json::json!("kept"));
    }

    /// Every key this build can write has to be declared, at each level, or the write
    /// would treat its own field as somebody else's.
    #[test]
    fn every_modelled_key_is_declared() {
        for (label, value, shape) in [
            (
                "Entry",
                serde_json::to_value(full_entry()).expect("entry"),
                Entry::MODELLED,
            ),
            (
                "Tokens",
                serde_json::to_value(tokens()).expect("tokens"),
                Tokens::MODELLED,
            ),
            (
                "ClientInfo",
                serde_json::to_value(client_info()).expect("client info"),
                ClientInfo::MODELLED,
            ),
        ] {
            let written: std::collections::BTreeSet<&str> = value
                .as_object()
                .expect("an object")
                .keys()
                .map(String::as_str)
                .collect();
            let declared: std::collections::BTreeSet<&str> = shape.keys.iter().copied().collect();
            assert_eq!(written, declared, "{label}::MODELLED is out of date");
        }
        assert_eq!(
            Entry::MODELLED
                .within
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>(),
            vec!["tokens", "clientInfo"],
            "the two modelled objects below an entry need their own shapes"
        );
    }

    /// Junk nobody can decode is still removable one key at a time, so a preserved entry
    /// cannot accumulate forever out of reach.
    #[test]
    fn junk_entries_are_each_removable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        std::fs::write(store.path(), r#"{"a":null,"b":[1,2],"c":"string","d":42}"#).expect("seed");

        let loaded = store.all().expect("all");
        assert_eq!(
            loaded.skipped,
            vec![
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "d".to_owned()
            ]
        );
        assert!(loaded.contains("b") && loaded.is_preserved("b"));
        assert_eq!(
            loaded.names().collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"],
            "a listing that omits them is how they become invisible"
        );

        for key in ["a", "b", "c", "d"] {
            store.remove(key).expect("remove");
        }
        assert_eq!(
            std::fs::read_to_string(store.path()).expect("read back"),
            "{}"
        );
    }

    #[test]
    fn removing_a_server_deletes_an_entry_this_build_cannot_decode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-auth.json");
        std::fs::write(
            &path,
            r#"{
  "keep": { "tokens": { "accessToken": "at" } },
  "newer-zuno": { "tokens": 42 }
}"#,
        )
        .expect("seed");

        let store = McpAuthStore::new(&path);
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

    #[test]
    fn get_for_url_matches_only_the_recorded_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set(SERVER, full_entry(), None).expect("set");

        assert_eq!(
            store.get_for_url(SERVER, URL).expect("match"),
            Some(full_entry())
        );
        assert_eq!(
            store
                .get_for_url(SERVER, "https://other.example.test/sse")
                .expect("mismatch"),
            None
        );
        assert_eq!(store.get_for_url("nobody", URL).expect("absent"), None);
    }

    /// An entry that never recorded a URL must not be assumed to match one.
    #[test]
    fn get_for_url_rejects_an_entry_with_no_recorded_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store
            .set(
                SERVER,
                Entry {
                    tokens: Some(tokens()),
                    ..Entry::default()
                },
                None,
            )
            .expect("set");
        assert_eq!(store.get_for_url(SERVER, URL).expect("no url"), None);
    }

    #[test]
    fn set_stamps_the_server_url_when_given_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store
            .set(
                SERVER,
                Entry {
                    tokens: Some(tokens()),
                    ..Entry::default()
                },
                Some(URL),
            )
            .expect("set");
        assert_eq!(
            store
                .get(SERVER)
                .expect("get")
                .and_then(|entry| entry.server_url),
            Some(URL.to_owned())
        );
    }

    #[test]
    fn update_tokens_creates_the_entry_for_an_unknown_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store
            .update_tokens(SERVER, tokens(), Some(URL))
            .expect("update");

        let entry = store.get(SERVER).expect("get").expect("created");
        assert_eq!(entry.tokens, Some(tokens()));
        assert_eq!(entry.server_url.as_deref(), Some(URL));
    }

    #[test]
    fn each_update_leaves_the_other_fields_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);

        store
            .update_client_info(SERVER, client_info(), Some(URL))
            .expect("client info");
        store
            .update_code_verifier(SERVER, Secret::new("pkce-verifier-0003"))
            .expect("verifier");
        store
            .update_oauth_state(SERVER, Secret::new("csrf-state-0004"))
            .expect("state");
        store.update_tokens(SERVER, tokens(), None).expect("tokens");

        assert_eq!(store.get(SERVER).expect("get"), Some(full_entry()));
    }

    #[test]
    fn get_oauth_state_reads_the_stored_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        assert_eq!(store.get_oauth_state(SERVER).expect("absent"), None);

        store
            .update_oauth_state(SERVER, Secret::new("csrf-state-0004"))
            .expect("state");
        assert_eq!(
            store
                .get_oauth_state(SERVER)
                .expect("present")
                .map(Secret::into_inner),
            Some("csrf-state-0004".to_owned())
        );
    }

    #[test]
    fn clearing_removes_only_the_named_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set(SERVER, full_entry(), None).expect("set");

        store.clear_code_verifier(SERVER).expect("clear verifier");
        let entry = store.get(SERVER).expect("get").expect("entry");
        assert_eq!(entry.code_verifier, None);
        assert_eq!(entry.oauth_state, Some(Secret::new("csrf-state-0004")));
        assert_eq!(entry.tokens, Some(tokens()));

        store.clear_oauth_state(SERVER).expect("clear state");
        let entry = store.get(SERVER).expect("get").expect("entry");
        assert_eq!(entry.oauth_state, None);
        assert_eq!(entry.tokens, Some(tokens()));
        assert_eq!(entry.client_info, Some(client_info()));
    }

    /// `mcp/auth.ts:126` returns `undefined`, so `mutate` writes nothing. Proven
    /// by the file not existing afterwards.
    #[test]
    fn clearing_an_absent_entry_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.clear_code_verifier("never-existed").expect("clear");
        store.clear_oauth_state("never-existed").expect("clear");
        assert!(
            !store.path().exists(),
            "no write should have created {}",
            store.path().display()
        );
    }

    #[test]
    fn remove_leaves_the_other_servers_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set("keep", full_entry(), None).expect("set");
        store.set("drop", full_entry(), None).expect("set");
        store.remove("drop").expect("remove");

        let loaded = store.all().expect("all");
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("keep").is_some());
        assert_eq!(loaded.get("drop"), None);
    }

    #[test]
    fn an_absent_file_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = store_in(&dir).all().expect("all");
        assert!(loaded.is_empty());
        assert_eq!(loaded.permissions, None);
        assert!(loaded.skipped.is_empty());
    }

    #[test]
    fn every_write_lands_at_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        type Write = (&'static str, fn(&McpAuthStore));

        let writes: Vec<Write> = vec![
            ("set", |store| {
                store.set(SERVER, full_entry(), Some(URL)).expect("set");
            }),
            ("update_tokens", |store| {
                store
                    .update_tokens(SERVER, tokens(), None)
                    .expect("update_tokens");
            }),
            ("update_client_info", |store| {
                store
                    .update_client_info(SERVER, client_info(), None)
                    .expect("update_client_info");
            }),
            ("update_code_verifier", |store| {
                store
                    .update_code_verifier(SERVER, Secret::new("cv"))
                    .expect("update_code_verifier");
            }),
            ("update_oauth_state", |store| {
                store
                    .update_oauth_state(SERVER, Secret::new("st"))
                    .expect("update_oauth_state");
            }),
            ("clear_code_verifier", |store| {
                store
                    .clear_code_verifier(SERVER)
                    .expect("clear_code_verifier");
            }),
            ("clear_oauth_state", |store| {
                store.clear_oauth_state(SERVER).expect("clear_oauth_state");
            }),
            ("remove", |store| store.remove(SERVER).expect("remove")),
        ];

        for (_label, write) in writes {
            #[cfg(unix)]
            if store.path().exists() {
                std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o666))
                    .expect("loosen");
            }
            write(&store);
            #[cfg(unix)]
            {
                let mode = std::fs::metadata(store.path())
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "after {_label}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_permissive_file_warns_and_still_yields_its_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set(SERVER, full_entry(), None).expect("set");
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");

        let loaded = store.all().expect("read must still succeed");
        assert_eq!(loaded.get(SERVER), Some(&full_entry()));
        let warning = loaded.permissions.expect("warning");
        assert_eq!(warning.path, store.path());
        assert_eq!(warning.mode, 0o644);
    }

    #[test]
    fn resolve_points_at_the_layouts_mcp_auth_file() {
        let layout =
            Layout::resolve_with(&zuno_paths::Env::from_pairs([("HOME", "/config")]), None);
        assert_eq!(
            McpAuthStore::resolve(&layout).path(),
            layout.mcp_auth_file()
        );
    }

    #[test]
    fn debug_of_an_entry_hides_every_secret_but_not_the_client_id() {
        let entry = full_entry();
        for rendered in [format!("{entry:?}"), format!("{entry:#?}")] {
            for plaintext in [
                "mcp-access-token-0001",
                "mcp-refresh-token-0001",
                "mcp-client-secret-0002",
                "pkce-verifier-0003",
                "csrf-state-0004",
            ] {
                assert!(!rendered.contains(plaintext), "{plaintext} leaked");
            }
            assert!(rendered.contains("client-public-id"), "{rendered}");
            assert!(rendered.contains(URL), "{rendered}");
            assert!(rendered.contains("read write"), "{rendered}");
        }
    }

    /// An emptied `mcp-auth.json` holds no entry a write could destroy, and the write
    /// that follows is the OAuth exchange that repairs it. It lands, and the file that
    /// comes back is a real store.
    #[test]
    fn a_mutation_over_an_emptied_file_repairs_it_instead_of_being_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        std::fs::write(store.path(), b"").expect("what an interrupted write left");

        store
            .update_tokens(SERVER, tokens(), Some(URL))
            .expect("the token store that repairs the file must not be denied");

        let loaded = store.all().expect("read back");
        assert_eq!(
            loaded.get(SERVER).and_then(|entry| entry.tokens.clone()),
            Some(tokens())
        );
        assert_eq!(loaded.damage, None);
    }

    /// The read half of the same split. An emptied `mcp-auth.json` must not deny
    /// `zuno mcp list` or a tool call that consults a server's token, because the
    /// login that repairs the file is reached through the same surface. The damage is
    /// reported instead.
    #[test]
    fn a_zero_byte_file_still_reads_and_reports_the_damage_rather_than_locking_the_user_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        std::fs::write(store.path(), b"").expect("what an interrupted write left");

        let loaded = store.all().expect("a read must not be denied");

        assert!(loaded.is_empty());
        let damage = loaded.damage.expect("the damage must travel with the read");
        assert_eq!(damage.path(), store.path());
        assert!(damage.to_string().contains("log in again"), "{damage}");
        assert_eq!(store.get(SERVER).expect("get must not be denied"), None);
        assert_eq!(
            std::fs::read(store.path()).expect("read back").len(),
            0,
            "a read must never repair or replace the damaged file"
        );
    }

    /// A healthy store must not acquire a damage report.
    #[test]
    fn a_healthy_store_reports_no_damage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store
            .update_tokens(SERVER, tokens(), Some(URL))
            .expect("seed");
        assert_eq!(store.all().expect("all").damage, None);
    }

    #[test]
    fn debug_of_the_whole_loaded_set_hides_every_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        store.set(SERVER, full_entry(), None).expect("set");
        let loaded = store.all().expect("all");
        let rendered = format!("{loaded:#?}");
        for plaintext in [
            "mcp-access-token-0001",
            "mcp-client-secret-0002",
            "pkce-verifier-0003",
            "csrf-state-0004",
        ] {
            assert!(!rendered.contains(plaintext), "{plaintext} leaked");
        }
        assert!(rendered.contains(SERVER), "{rendered}");
    }
}
