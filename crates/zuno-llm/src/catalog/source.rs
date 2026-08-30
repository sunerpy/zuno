//! Where the catalog comes from: the three environment variables, the cache file,
//! and the fetch — in that order of authority.
//!
//! Ported from `packages/core/src/models-dev.ts:160-231`. The resolution ladder is
//! the part worth stating precisely, because each rung answers a different
//! question and they are routinely conflated:
//!
//! The `ZUNO_*` names below are the spellings **this crate** accepts. Upstream
//! reads the same three switches under `ZUNO_MODELS_URL`,
//! `ZUNO_MODELS_PATH` and `ZUNO_DISABLE_MODELS_FETCH`, so any sentence
//! reporting what the oracle did names those instead.
//!
//! | variable | question it answers | effect |
//! |---|---|---|
//! | `ZUNO_MODELS_URL` | *where* would a fetch go | changes the source **and** the cache filename |
//! | `ZUNO_MODELS_PATH` | read this file **instead** | bypasses the cache path entirely; never written to |
//! | `ZUNO_DISABLE_MODELS_FETCH` | may we go to the network | no fetch, ever — including at startup |
//!
//! Three details that a reimplementation gets wrong by default:
//!
//! 1. **The cache filename depends on the source.** `models.json` for the default
//!    source, `models-<sha1(source)>.json` for anything else
//!    (`models-dev.ts:161-164`), so pointing at a mirror cannot poison the
//!    default cache. `zuno-paths` already implements this; this module does not
//!    re-derive it.
//! 2. **`ZUNO_MODELS_URL` is read with JavaScript `||` semantics**, so
//!    `ZUNO_MODELS_URL=""` means *unset*, not "empty source"
//!    (`models-dev.ts:160`). `ZUNO_DISABLE_MODELS_FETCH` goes through
//!    `Flag.truthy` instead, where only `"1"` and a case-insensitive `"true"`
//!    count (`flag.ts:3-6`) — so `ZUNO_DISABLE_MODELS_FETCH=0`, `=no` and
//!    `=yes` all leave fetching **enabled**. Both are `zuno-paths::Env` methods
//!    already; this module uses them rather than parsing strings again.
//! 3. **An unreadable cache file is deleted; an unreadable explicit path is not**
//!    (`models-dev.ts:184-196`). A corrupt cache is this program's own mess to
//!    clean up. An explicit path is the user's instruction, and quietly falling
//!    back from it would hide a typo.
//!
//! # Fetching disabled with nothing on disk is not a failure
//!
//! With fetching disabled and no cache, the oracle has **three** fallbacks, not
//! one, and none of them is an error:
//!
//! 1. the cache file (`models-dev.ts:218`),
//! 2. a **catalog snapshot compiled into the binary** — the `ZUNO_MODELS_DEV`
//!    global at `models-dev.ts:198-200`, read at `:220-221`,
//! 3. `{}` (`:222`) — an *empty catalog*, returned as a success.
//!
//! Rung 3 is what makes upstream work for an air-gapped user with a
//! self-contained `provider.*` block: `provider.ts:1425-1520` merges config
//! providers *over* whatever the load returned, so an empty document plus a config
//! that names its own provider, model, cost and limits still resolves that model.
//! Measured on 1.18.12 under `env -i`, an empty `XDG_CACHE_HOME`,
//! `ZUNO_DISABLE_MODELS_FETCH=1` and no `ZUNO_MODELS_PATH` — the names
//! that binary reads: `opencode
//! models` exits 0 and prints `test/test-model` from config alone.
//!
//! So [`CatalogSource::load`] returns `Ok` with an empty [`CatalogDocument`] here,
//! tagged [`CatalogProvenance::FetchForbidden`]. Returning an error instead is the
//! defect todo 108 fixed: the binary refused to start for a user whose config had
//! nothing left to look up.
//!
//! What that tag buys is the other half. An empty catalog *is* indistinguishable
//! from "you have no providers configured" — so the provenance travels with the
//! document, and the moment a caller asks for a model the resolved catalog does not
//! contain, [`LoadedCatalog::unresolved_model`] turns it back into
//! [`CatalogError::FetchDisabled`], naming the model, the flag, the source and the
//! cache path. Fail-fast is kept for the case it was written for — a model nobody
//! defined — and dropped for the case where there was nothing to look up.
//!
//! # Rung 2 is deliberately absent
//!
//! This crate ships no compiled-in snapshot, and that is a decision rather than an
//! omission. The oracle's snapshot is not a copy of models.dev: measured, it holds
//! exactly the seven `opencode/*` models of that release's hosted gateway, all
//! `-free` preview names that rotate between releases. Baking a frozen copy in
//! would make this binary advertise models on someone else's gateway from a list it
//! can never refresh — precisely the air-gapped user who would be worst served by a
//! stale one. It would also need either a network fetch at build time, which this
//! workspace forbids, or a committed blob to keep current forever.
//!
//! The cost is a listing difference, not a functional one: with fetching disabled
//! and no cache — `ZUNO_DISABLE_MODELS_FETCH` upstream,
//! `ZUNO_DISABLE_MODELS_FETCH` here — upstream lists its seven gateway models and
//! this crate lists none. Anything the user's own config declares
//! resolves identically on both sides, which is what
//! `tests/catalog_differential.rs` asserts byte for byte.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use zuno_paths::Layout;
use zuno_paths::env::Env;

use crate::catalog::error::CatalogError;
use crate::catalog::models_dev::CatalogDocument;

/// `ZUNO_MODELS_PATH` — read this catalog file instead of the cache.
pub const ZUNO_MODELS_PATH: &str = "ZUNO_MODELS_PATH";
/// `ZUNO_DISABLE_MODELS_FETCH` — never go to the network for the catalog.
pub const ZUNO_DISABLE_MODELS_FETCH: &str = "ZUNO_DISABLE_MODELS_FETCH";

/// How long a cache file counts as fresh — `models-dev.ts:165`.
pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// The catalog request path appended to the source — `models-dev.ts:176`.
const API_PATH: &str = "/api.json";

/// The fetch timeout — `models-dev.ts:180`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts after the first — `models-dev.ts:152-156` (`times: 2`).
const FETCH_RETRIES: u32 = 2;

/// Base backoff between attempts — `models-dev.ts:155` (`exponential(200)`).
const FETCH_BACKOFF: Duration = Duration::from_millis(200);

/// Everything the three variables and the layout decide, resolved once.
///
/// Built by [`CatalogSource::resolve`] from an explicit [`Env`] rather than from
/// `std::env`, so a test states the environment it means instead of mutating the
/// process and racing every other test in the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSource {
    source: String,
    cache: PathBuf,
    explicit_path: Option<PathBuf>,
    fetch_disabled: bool,
}

impl CatalogSource {
    /// Resolve the source, the cache path and the fetch policy from one `Env`.
    #[must_use]
    pub fn resolve(env: &Env, layout: &Layout) -> Self {
        Self {
            // `models_source()` is `ZUNO_MODELS_URL` with `||` semantics and
            // the models.dev default; the cache filename hangs off it.
            source: layout.models_source().to_owned(),
            cache: layout.models_cache(),
            explicit_path: env.truthy_value(ZUNO_MODELS_PATH).map(PathBuf::from),
            fetch_disabled: env.flag(ZUNO_DISABLE_MODELS_FETCH),
        }
    }

    /// The base URL a fetch would target.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The cache file for this source.
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// The file `ZUNO_MODELS_PATH` named, if it named one.
    #[must_use]
    pub fn explicit_path(&self) -> Option<&Path> {
        self.explicit_path.as_deref()
    }

    /// True when policy forbids the network.
    #[must_use]
    pub const fn fetch_disabled(&self) -> bool {
        self.fetch_disabled
    }

    /// The URL a fetch would request.
    #[must_use]
    pub fn api_url(&self) -> String {
        format!("{}{API_PATH}", self.source.trim_end_matches('/'))
    }

    /// The file this source reads from: the explicit path, else the cache.
    #[must_use]
    pub fn read_path(&self) -> &Path {
        self.explicit_path.as_deref().unwrap_or(&self.cache)
    }

    /// True when the cache exists and is younger than [`CACHE_TTL`].
    ///
    /// `models-dev.ts:168-173`: a missing file is stale, and an unreadable mtime
    /// is treated as the epoch, which is also stale.
    #[must_use]
    pub fn cache_is_fresh(&self) -> bool {
        let Ok(metadata) = std::fs::metadata(&self.cache) else {
            return false;
        };
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age < CACHE_TTL)
    }

    /// Read the catalog from disk, without touching the network.
    ///
    /// `Ok(None)` means "nothing usable on disk" and is not an error — it is the
    /// signal to fetch. Distinguishing the two is the whole point:
    ///
    /// - explicit path missing or unreadable → [`CatalogError::ExplicitPathUnreadable`],
    ///   because the user named it;
    /// - cache missing → `Ok(None)`;
    /// - cache present but unparseable → the file is **removed** and `Ok(None)`,
    ///   matching `models-dev.ts:184-196`; a corrupt cache this program wrote is
    ///   this program's to discard;
    /// - explicit path present but unparseable → [`CatalogError::Malformed`],
    ///   never removed.
    pub fn load_from_disk(&self) -> Result<Option<CatalogDocument>, CatalogError> {
        if let Some(path) = self.explicit_path.as_deref() {
            let bytes =
                std::fs::read(path).map_err(|source| CatalogError::ExplicitPathUnreadable {
                    path: path.to_owned(),
                    source,
                })?;
            let document =
                serde_json::from_slice(&bytes).map_err(|source| CatalogError::Malformed {
                    path: path.to_owned(),
                    source,
                })?;
            return Ok(Some(document));
        }

        let Ok(bytes) = std::fs::read(&self.cache) else {
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(document) => Ok(Some(document)),
            Err(_) => {
                // Best-effort: a cache we cannot delete is still a cache we will
                // not use, and failing here would turn a recoverable state into
                // a hard error.
                let _ = std::fs::remove_file(&self.cache);
                Ok(None)
            }
        }
    }

    /// Load the catalog: disk first, then the network if policy allows.
    ///
    /// The fetch is only reached when disk yielded nothing. When it is *also*
    /// forbidden this still succeeds, with an empty document tagged
    /// [`CatalogProvenance::FetchForbidden`] — `models-dev.ts:222`. A config that
    /// fully specifies its own provider and model has nothing left to look up, and
    /// refusing to start it was the whole of todo 108's defect. The tag is what
    /// keeps the other half honest: see [`LoadedCatalog::unresolved_model`].
    pub async fn load(&self) -> Result<LoadedCatalog, CatalogError> {
        if let Some(document) = self.load_from_disk()? {
            let provenance = match self.explicit_path.as_deref() {
                Some(path) => CatalogProvenance::ExplicitPath(path.to_owned()),
                None => CatalogProvenance::Cache(self.cache.clone()),
            };
            return Ok(LoadedCatalog {
                document,
                provenance,
            });
        }
        if self.fetch_disabled {
            return Ok(LoadedCatalog {
                document: CatalogDocument::new(),
                provenance: CatalogProvenance::FetchForbidden {
                    origin: self.source.clone(),
                    cache: self.cache.clone(),
                },
            });
        }
        let text = self.fetch().await?;
        let document = serde_json::from_str(&text).map_err(|source| CatalogError::Malformed {
            path: self.cache.clone(),
            source,
        })?;
        // A cache write failure is reported, not swallowed: the next run would
        // silently re-fetch and the user would never learn why.
        self.write_cache(&text)?;
        Ok(LoadedCatalog {
            document,
            provenance: CatalogProvenance::Fetched,
        })
    }

    /// Refresh the cache from the network.
    ///
    /// `force` skips the [`CACHE_TTL`] check, which is what `models --refresh`
    /// does (`models.ts:28-31`, `models-dev.ts:237-244`). Returns `Ok(false)`
    /// when the cache was already fresh and nothing was fetched.
    ///
    /// Returns [`CatalogError::RefreshDisabled`] rather than silently doing nothing
    /// when policy forbids the network: a user who typed `--refresh` asked a
    /// direct question and deserves a direct answer. That is the one place a
    /// disabled fetch is still an error on its own, because refreshing *is* the
    /// request — unlike [`Self::load`], which has a config to fall back on.
    pub async fn refresh(&self, force: bool) -> Result<bool, CatalogError> {
        if self.fetch_disabled {
            return Err(CatalogError::RefreshDisabled {
                origin: self.source.clone(),
            });
        }
        if !force && self.cache_is_fresh() {
            return Ok(false);
        }
        let text = self.fetch().await?;
        self.write_cache(&text)?;
        Ok(true)
    }

    /// `GET {source}/api.json`, with the oracle's timeout and retry budget.
    async fn fetch(&self) -> Result<String, CatalogError> {
        let url = self.api_url();
        let client = zuno_network::client_builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|error| CatalogError::Fetch {
                origin: self.source.clone(),
                cause: Box::new(error),
            })?;

        let mut attempt = 0;
        loop {
            let outcome = client
                .get(&url)
                .header(reqwest::header::USER_AGENT, user_agent())
                .send()
                .await
                .and_then(reqwest::Response::error_for_status);
            match outcome {
                Ok(response) => match response.text().await {
                    Ok(text) => return Ok(text),
                    Err(error) if attempt < FETCH_RETRIES => {
                        backoff(attempt).await;
                        attempt += 1;
                        let _ = error;
                    }
                    Err(error) => {
                        return Err(CatalogError::Fetch {
                            origin: self.source.clone(),
                            cause: Box::new(error),
                        });
                    }
                },
                Err(error) if attempt < FETCH_RETRIES => {
                    backoff(attempt).await;
                    attempt += 1;
                    let _ = error;
                }
                Err(error) => {
                    return Err(CatalogError::Fetch {
                        origin: self.source.clone(),
                        cause: Box::new(error),
                    });
                }
            }
        }
    }

    /// Write the cache atomically: temp file, then rename.
    ///
    /// `models-dev.ts:202-215` names the temp file after the pid and the clock
    /// so two concurrent CLIs cannot collide, and renames because a reader must
    /// never observe a half-written catalog. Both are reproduced here.
    fn write_cache(&self, text: &str) -> Result<(), CatalogError> {
        let parent = self.cache.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| CatalogError::CacheWrite {
            path: self.cache.clone(),
            source,
        })?;
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let temp = self
            .cache
            .with_extension(format!("json.{}.{stamp}.tmp", std::process::id()));
        let write = std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, &self.cache));
        if let Err(source) = write {
            let _ = std::fs::remove_file(&temp);
            return Err(CatalogError::CacheWrite {
                path: self.cache.clone(),
                source,
            });
        }
        Ok(())
    }
}

/// Where a loaded catalog document came from.
///
/// Travels with the document because the *same* empty document means two opposite
/// things. Loaded from a cache that genuinely lists nothing, an absent model is a
/// wrong id. Left empty because `ZUNO_DISABLE_MODELS_FETCH` forbade the only
/// remaining way to fill it, an absent model is a policy problem with a named fix —
/// and a caller with no way to tell those apart must either fail on both, which
/// breaks the air-gapped user todo 108 exists for, or fail on neither, which is how
/// an unknown model degrades into a mysteriously empty model picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogProvenance {
    /// Read from the file `ZUNO_MODELS_PATH` named.
    ExplicitPath(PathBuf),
    /// Read from this source's cache file.
    Cache(PathBuf),
    /// Fetched from the source and written to the cache.
    Fetched,
    /// Empty because policy forbade the fetch and nothing was on disk.
    FetchForbidden {
        /// The source a fetch would have gone to.
        origin: String,
        /// The cache file that was looked for and not found.
        cache: PathBuf,
    },
}

impl CatalogProvenance {
    /// True when the document is empty because policy forbade filling it.
    #[must_use]
    pub const fn is_fetch_forbidden(&self) -> bool {
        matches!(self, Self::FetchForbidden { .. })
    }

    /// The failure to report when the resolved catalog has no `requested` model.
    ///
    /// `Some` only when the document was empty because policy forbade filling it:
    /// the user named a model, nothing in their config defines it, and nothing was
    /// allowed to be looked up — so the answer names the model *and* all three ways
    /// out. `None` when a catalog was actually loaded, leaving the caller its own
    /// "no such model" wording, which is the truthful answer in that case.
    ///
    /// Calling this *before* checking the resolved catalog would resurrect the defect
    /// it exists to avoid: a config that fully specifies the requested model has
    /// nothing to look up and must not see an error at all.
    #[must_use]
    pub fn unresolved_model(&self, requested: &str) -> Option<CatalogError> {
        match self {
            Self::FetchForbidden { origin, cache } => Some(CatalogError::FetchDisabled {
                requested: requested.to_owned(),
                origin: origin.clone(),
                cache: cache.clone(),
            }),
            Self::ExplicitPath(_) | Self::Cache(_) | Self::Fetched => None,
        }
    }
}

/// A catalog document together with how it was obtained.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedCatalog {
    document: CatalogDocument,
    provenance: CatalogProvenance,
}

impl LoadedCatalog {
    /// The document, ready for [`crate::catalog::Catalog::resolve`].
    #[must_use]
    pub const fn document(&self) -> &CatalogDocument {
        &self.document
    }

    /// How this document was obtained.
    #[must_use]
    pub const fn provenance(&self) -> &CatalogProvenance {
        &self.provenance
    }

    /// Take the document, discarding the provenance.
    #[must_use]
    pub fn into_document(self) -> CatalogDocument {
        self.document
    }

    /// [`CatalogProvenance::unresolved_model`], for a caller holding the load.
    #[must_use]
    pub fn unresolved_model(&self, requested: &str) -> Option<CatalogError> {
        self.provenance.unresolved_model(requested)
    }
}

/// Exponential backoff between fetch attempts.
async fn backoff(attempt: u32) {
    tokio::time::sleep(FETCH_BACKOFF * 2_u32.pow(attempt)).await;
}

/// The Zuno identity sent to the model catalog.
fn user_agent() -> String {
    format!(
        "zuno/{}/{}/cli",
        zuno_paths::installation_channel(),
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_paths::env::{HOME, XDG_CACHE_HOME, ZUNO_MODELS_URL};

    fn source_for(pairs: &[(&str, &str)]) -> CatalogSource {
        let env = Env::from_pairs(pairs.iter().copied());
        let layout = Layout::resolve_with(&env, None);
        CatalogSource::resolve(&env, &layout)
    }

    #[test]
    fn the_default_source_caches_at_models_json() {
        let source = source_for(&[(HOME, "/h"), (XDG_CACHE_HOME, "/c")]);
        assert_eq!(source.source(), "https://models.dev");
        assert_eq!(source.cache(), Path::new("/c/zuno/models.json"));
        assert_eq!(source.api_url(), "https://models.dev/api.json");
    }

    #[test]
    fn a_custom_source_gets_its_own_cache_file() {
        let source = source_for(&[
            (HOME, "/h"),
            (XDG_CACHE_HOME, "/c"),
            (ZUNO_MODELS_URL, "https://mirror.example.com"),
        ]);
        assert_eq!(source.source(), "https://mirror.example.com");
        // sha1("https://mirror.example.com"); the suffix keeps a mirror from
        // poisoning the default cache.
        let file = source.cache().file_name().expect("a file name");
        let file = file.to_string_lossy();
        assert!(
            file.starts_with("models-") && file.ends_with(".json") && file.len() == 7 + 40 + 5,
            "expected models-<40 hex>.json, got {file}"
        );
        assert_ne!(source.cache(), Path::new("/c/zuno/models.json"));
        assert_eq!(source.api_url(), "https://mirror.example.com/api.json");
    }

    #[test]
    fn an_empty_models_url_means_unset() {
        // JavaScript `||`: `models-dev.ts:160` treats "" as absent.
        let source = source_for(&[(HOME, "/h"), (XDG_CACHE_HOME, "/c"), (ZUNO_MODELS_URL, "")]);
        assert_eq!(source.source(), "https://models.dev");
        assert_eq!(source.cache(), Path::new("/c/zuno/models.json"));
    }

    #[test]
    fn catalog_requests_use_the_zuno_identity() {
        let user_agent = user_agent();
        assert!(user_agent.starts_with("zuno/"), "{user_agent}");
        assert!(!user_agent.starts_with("opencode/"), "{user_agent}");
    }

    #[test]
    fn only_one_and_true_disable_the_fetch() {
        // `Flag.truthy` — flag.ts:3-6. Everything else leaves fetching on, so
        // `=0` and `=no` must not silently disable the network.
        for value in ["1", "true", "TRUE", "True"] {
            let source = source_for(&[(HOME, "/h"), (ZUNO_DISABLE_MODELS_FETCH, value)]);
            assert!(source.fetch_disabled(), "{value} should disable the fetch");
        }
        for value in ["0", "false", "no", "yes", "", "2"] {
            let source = source_for(&[(HOME, "/h"), (ZUNO_DISABLE_MODELS_FETCH, value)]);
            assert!(
                !source.fetch_disabled(),
                "{value} must not disable the fetch"
            );
        }
    }

    #[test]
    fn an_explicit_path_replaces_the_cache_as_the_read_target() {
        let source = source_for(&[
            (HOME, "/h"),
            (XDG_CACHE_HOME, "/c"),
            (ZUNO_MODELS_PATH, "/pinned/fixture.json"),
        ]);
        assert_eq!(
            source.explicit_path(),
            Some(Path::new("/pinned/fixture.json"))
        );
        assert_eq!(source.read_path(), Path::new("/pinned/fixture.json"));
        // The cache path is still computed: refresh() writes there even when a
        // read comes from the explicit path.
        assert_eq!(source.cache(), Path::new("/c/zuno/models.json"));
    }
}
