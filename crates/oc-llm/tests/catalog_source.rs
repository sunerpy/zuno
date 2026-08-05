//! Catalog *source* behaviour on a real filesystem: the cache, the explicit path,
//! and the failure the disable flag is supposed to produce.
//!
//! These tests write real files into a `tempfile::TempDir` and point `XDG_CACHE_HOME`
//! at it through an explicit [`Env`], so the user's real `~/.cache/opencode/models.json`
//! is never read, written, or deleted. Nothing here touches the network: the one
//! test that reaches the fetch branch asserts it is *not* reached.

use std::path::Path;

use oc_llm::catalog::CatalogError;
use oc_llm::catalog::source::{CatalogSource, OPENCODE_DISABLE_MODELS_FETCH, OPENCODE_MODELS_PATH};
use oc_paths::Layout;
use oc_paths::env::{Env, HOME, OPENCODE_MODELS_URL, XDG_CACHE_HOME};

const PINNED: &str = include_str!("fixtures/models-dev-pinned.json");

/// A source rooted in a temp directory, with no process env involved.
fn source_in(root: &Path, extra: &[(&str, &str)]) -> CatalogSource {
    let mut env = Env::empty()
        .with(HOME, root.join("home").to_string_lossy().into_owned())
        .with(
            XDG_CACHE_HOME,
            root.join("cache").to_string_lossy().into_owned(),
        );
    for (key, value) in extra {
        env = env.with(*key, *value);
    }
    let layout = Layout::resolve_with(&env, None);
    CatalogSource::resolve(&env, &layout)
}

// ---------------------------------------------------------------------------
// The QA failure scenario: fetch disabled, no cache.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_disabled_with_no_cache_fails_immediately_and_actionably() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(OPENCODE_DISABLE_MODELS_FETCH, "1")]);
    assert!(source.fetch_disabled());
    assert!(
        !source.cache().exists(),
        "the temp cache must start empty for this to mean anything"
    );

    // The load must return, not hang: a five-second budget is ~500x what a
    // filesystem miss needs and 0x what a forbidden network call would take.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), source.load())
        .await
        .expect("load must return promptly rather than attempting a forbidden fetch");
    let error = outcome.expect_err("no catalog is available");

    assert!(error.is_policy(), "classified as a policy failure");
    let rendered = error.to_string();
    // Each assertion is one thing the user needs in order to fix this.
    assert!(
        rendered.contains("OPENCODE_DISABLE_MODELS_FETCH"),
        "must name the flag that caused it: {rendered}"
    );
    assert!(
        rendered.contains("https://models.opencode.ai"),
        "must name the source that was not contacted: {rendered}"
    );
    assert!(
        rendered.contains(&source.cache().to_string_lossy().into_owned()),
        "must name the cache path it looked for: {rendered}"
    );
    assert!(
        rendered.contains("OPENCODE_MODELS_PATH"),
        "must name the alternative: {rendered}"
    );
    assert!(
        matches!(error, CatalogError::FetchDisabled { .. }),
        "a caller matches the variant, never the rendered text"
    );
}

#[tokio::test]
async fn fetch_disabled_with_a_cache_present_loads_from_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(OPENCODE_DISABLE_MODELS_FETCH, "1")]);
    std::fs::create_dir_all(source.cache().parent().expect("a parent")).expect("mkdir");
    std::fs::write(source.cache(), PINNED).expect("seed the cache");

    let document = source.load().await.expect("the cache satisfies the load");
    assert_eq!(document.len(), 7);
}

#[tokio::test]
async fn refresh_with_the_fetch_disabled_says_so_rather_than_doing_nothing() {
    // `models --refresh` is a direct question. Silently succeeding without
    // refreshing would be the worst possible answer.
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(OPENCODE_DISABLE_MODELS_FETCH, "1")]);
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), source.refresh(true))
        .await
        .expect("refresh must return promptly")
        .expect_err("refresh cannot succeed with the network forbidden");
    assert!(error.is_policy());
}

// ---------------------------------------------------------------------------
// The explicit path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_path_is_read_instead_of_the_cache() {
    let temp = tempfile::tempdir().expect("temp dir");
    let pinned = temp.path().join("pinned.json");
    std::fs::write(&pinned, PINNED).expect("write the fixture");

    let source = source_in(
        temp.path(),
        &[
            (OPENCODE_MODELS_PATH, &pinned.to_string_lossy()),
            (OPENCODE_DISABLE_MODELS_FETCH, "1"),
        ],
    );
    // The cache does not exist; the explicit path still satisfies the load.
    assert!(!source.cache().exists());
    let document = source.load().await.expect("the explicit path is honoured");
    assert_eq!(document.len(), 7);
}

#[tokio::test]
async fn a_missing_explicit_path_is_an_error_not_a_fallback() {
    // The user named a file. Quietly looking elsewhere would hide a typo.
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(
        temp.path(),
        &[(OPENCODE_MODELS_PATH, "/nonexistent/t26/catalog.json")],
    );
    let error = source
        .load()
        .await
        .expect_err("a named-but-absent file is a mistake worth reporting");
    assert!(matches!(error, CatalogError::ExplicitPathUnreadable { .. }));
    assert!(
        error.to_string().contains("OPENCODE_MODELS_PATH"),
        "the message names the variable: {error}"
    );
}

#[tokio::test]
async fn a_malformed_explicit_path_is_reported_and_never_deleted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let pinned = temp.path().join("broken.json");
    std::fs::write(&pinned, "{ not json").expect("write");
    let source = source_in(
        temp.path(),
        &[(OPENCODE_MODELS_PATH, &pinned.to_string_lossy())],
    );
    let error = source.load().await.expect_err("malformed JSON is an error");
    assert!(matches!(error, CatalogError::Malformed { .. }));
    assert!(
        pinned.exists(),
        "a file the user named must survive being unparseable"
    );
}

// ---------------------------------------------------------------------------
// The cache, including the corrupt-cache cleanup.
// ---------------------------------------------------------------------------

#[test]
fn a_corrupt_cache_is_deleted_so_the_next_run_can_recover() {
    // `models-dev.ts:184-196`. A cache this program wrote is this program's to
    // discard; leaving it would wedge every subsequent run.
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[]);
    std::fs::create_dir_all(source.cache().parent().expect("a parent")).expect("mkdir");
    std::fs::write(source.cache(), "{ not json").expect("seed a corrupt cache");

    let loaded = source
        .load_from_disk()
        .expect("a corrupt cache is not an error, it is a cache miss");
    assert!(loaded.is_none());
    assert!(
        !source.cache().exists(),
        "the corrupt cache must be removed, not left to wedge the next run"
    );
}

#[test]
fn a_missing_cache_is_a_miss_rather_than_an_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[]);
    assert!(source.load_from_disk().expect("not an error").is_none());
}

#[test]
fn cache_freshness_follows_the_five_minute_ttl() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[]);
    assert!(!source.cache_is_fresh(), "a missing cache is stale");
    std::fs::create_dir_all(source.cache().parent().expect("a parent")).expect("mkdir");
    std::fs::write(source.cache(), PINNED).expect("seed");
    assert!(source.cache_is_fresh(), "a just-written cache is fresh");
}

#[test]
fn a_custom_source_writes_to_its_own_cache_file() {
    // Pointing at a mirror must not poison the default cache, which is the whole
    // reason for the sha1 suffix (`models-dev.ts:161-164`).
    let temp = tempfile::tempdir().expect("temp dir");
    let default = source_in(temp.path(), &[]);
    let mirror = source_in(
        temp.path(),
        &[(OPENCODE_MODELS_URL, "https://mirror.example.com")],
    );
    assert_ne!(default.cache(), mirror.cache());
    assert_eq!(default.cache().file_name().expect("a name"), "models.json");
    let mirror_name = mirror
        .cache()
        .file_name()
        .expect("a name")
        .to_string_lossy();
    assert!(mirror_name.starts_with("models-") && mirror_name.ends_with(".json"));
    // Independently computable: sha1 of the source URL.
    assert_eq!(
        mirror_name.as_ref(),
        format!(
            "models-{}.json",
            oc_paths::sha1::hex(b"https://mirror.example.com")
        )
    );
}
