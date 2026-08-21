//! Catalog *source* behaviour on a real filesystem: the cache, the explicit path,
//! and the failure the disable flag is supposed to produce.
//!
//! These tests write real files into a `tempfile::TempDir` and point `XDG_CACHE_HOME`
//! at it through an explicit [`Env`], so the user's real `~/.cache/zuno/models.json`
//! is never read, written, or deleted. Nothing here touches the network: the one
//! test that reaches the fetch branch asserts it is *not* reached.

use std::path::Path;

use zuno_config::schema::Config;
use zuno_llm::catalog::source::{CatalogSource, ZUNO_DISABLE_MODELS_FETCH, ZUNO_MODELS_PATH};
use zuno_llm::catalog::{Catalog, CatalogError, CatalogProvenance, ResolveInput};
use zuno_paths::Layout;
use zuno_paths::env::{Env, HOME, XDG_CACHE_HOME, ZUNO_MODELS_URL};

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
// Fetch disabled, no cache: an empty catalog, and fail-fast only for a model
// nothing defines.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_disabled_with_no_cache_yields_an_empty_catalog_rather_than_an_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(ZUNO_DISABLE_MODELS_FETCH, "1")]);
    assert!(source.fetch_disabled());
    assert!(
        !source.cache().exists(),
        "the temp cache must start empty for this to mean anything"
    );

    // The load must return, not hang: a five-second budget is ~500x what a
    // filesystem miss needs and 0x what a forbidden network call would take.
    let loaded = tokio::time::timeout(std::time::Duration::from_secs(5), source.load())
        .await
        .expect("load must return promptly rather than attempting a forbidden fetch")
        .expect("`models-dev.ts:222` returns `{}`, not an error");

    assert!(
        loaded.document().is_empty(),
        "nothing was readable, so the document must be empty rather than invented"
    );
    assert_eq!(
        loaded.provenance(),
        &CatalogProvenance::FetchForbidden {
            origin: "https://models.dev".to_owned(),
            cache: source.cache().to_owned(),
        },
        "the reason must travel with the document or an unknown model cannot be \
         told apart from an unconfigured one"
    );
    assert!(loaded.provenance().is_fetch_forbidden());
}

/// The air-gapped user's whole scenario, from the source to a selectable model.
///
/// No cache, no network, no `ZUNO_MODELS_PATH` — only a config that names its
/// own provider, model, limits and base URL. Upstream runs this
/// (`provider.ts:1425-1520` merges config over the loaded document); measured on
/// 1.18.12, `opencode models` prints `test/test-model` and exits 0.
#[tokio::test]
async fn a_config_only_provider_resolves_with_no_cache_and_no_network() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(ZUNO_DISABLE_MODELS_FETCH, "1")]);
    let config: Config = serde_json::from_str(
        r#"{"provider":{"test":{"name":"Test","id":"test","env":[],
             "npm":"@ai-sdk/openai-compatible","api":"https://gateway.internal/v1",
             "models":{"test-model":{"id":"test-model","name":"Test Model",
               "tool_call":true,"limit":{"context":100000,"output":10000},
               "cost":{"input":0,"output":0}}},
             "options":{"apiKey":"k","baseURL":"https://gateway.internal/v1"}}}}"#,
    )
    .expect("config parses");

    let loaded = source
        .load()
        .await
        .expect("a forbidden fetch is not an error");
    let catalog = Catalog::resolve(loaded.document(), &ResolveInput::new().with_config(&config));

    assert_eq!(catalog.model_lines(), vec!["test/test-model"]);
    let model = catalog
        .model("test", "test-model")
        .expect("the config's own model resolves without a catalog");
    assert_eq!(model.api.url, "https://gateway.internal/v1");
    assert!(
        loaded.unresolved_model("test/test-model").is_some(),
        "the policy error is still constructible; what matters is that the caller \
         never reaches for it, because the lookup above already succeeded"
    );
}

#[tokio::test]
async fn a_model_nothing_defines_still_fails_immediately_and_actionably() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(ZUNO_DISABLE_MODELS_FETCH, "1")]);
    let loaded = source
        .load()
        .await
        .expect("an empty catalog is not an error");

    let error = loaded
        .unresolved_model("nobody/defines-this")
        .expect("a forbidden fetch is exactly when this must be reported");

    assert!(error.is_policy(), "classified as a policy failure");
    let rendered = error.to_string();
    // Each assertion is one thing the user needs in order to fix this.
    assert!(
        rendered.contains("nobody/defines-this"),
        "must name the model that was asked for: {rendered}"
    );
    assert!(
        rendered.contains("provider"),
        "must name the config block that would define it: {rendered}"
    );
    assert!(
        rendered.contains("ZUNO_DISABLE_MODELS_FETCH"),
        "must name the flag that caused it: {rendered}"
    );
    assert!(
        rendered.contains("https://models.dev"),
        "must name the source that was not contacted: {rendered}"
    );
    assert!(
        rendered.contains(&source.cache().to_string_lossy().into_owned()),
        "must name the cache path it looked for: {rendered}"
    );
    assert!(
        rendered.contains("ZUNO_MODELS_PATH"),
        "must name the alternative: {rendered}"
    );
    assert!(
        matches!(error, CatalogError::FetchDisabled { .. }),
        "a caller matches the variant, never the rendered text"
    );
}

/// A catalog that *was* loaded and simply lacks the model is not a policy failure.
///
/// Blaming the flag there would send the user to unset a variable that had nothing
/// to do with it, and would hide a plain typo in a model id.
#[tokio::test]
async fn a_loaded_catalog_does_not_blame_the_flag_for_an_absent_model() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(ZUNO_DISABLE_MODELS_FETCH, "1")]);
    std::fs::create_dir_all(source.cache().parent().expect("a parent")).expect("mkdir");
    std::fs::write(source.cache(), PINNED).expect("seed the cache");

    let loaded = source.load().await.expect("the cache satisfies the load");
    assert!(loaded.unresolved_model("groq/no-such-model").is_none());
}

#[tokio::test]
async fn fetch_disabled_with_a_cache_present_loads_from_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(ZUNO_DISABLE_MODELS_FETCH, "1")]);
    std::fs::create_dir_all(source.cache().parent().expect("a parent")).expect("mkdir");
    std::fs::write(source.cache(), PINNED).expect("seed the cache");

    let loaded = source.load().await.expect("the cache satisfies the load");
    assert_eq!(loaded.document().len(), 7);
    assert_eq!(
        loaded.provenance(),
        &CatalogProvenance::Cache(source.cache().to_owned())
    );
}

#[tokio::test]
async fn refresh_with_the_fetch_disabled_says_so_rather_than_doing_nothing() {
    // `models --refresh` is a direct question. Silently succeeding without
    // refreshing would be the worst possible answer.
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(temp.path(), &[(ZUNO_DISABLE_MODELS_FETCH, "1")]);
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
            (ZUNO_MODELS_PATH, &pinned.to_string_lossy()),
            (ZUNO_DISABLE_MODELS_FETCH, "1"),
        ],
    );
    // The cache does not exist; the explicit path still satisfies the load.
    assert!(!source.cache().exists());
    let loaded = source.load().await.expect("the explicit path is honoured");
    assert_eq!(loaded.document().len(), 7);
    assert_eq!(
        loaded.provenance(),
        &CatalogProvenance::ExplicitPath(pinned.clone())
    );
}

#[tokio::test]
async fn a_missing_explicit_path_is_an_error_not_a_fallback() {
    // The user named a file. Quietly looking elsewhere would hide a typo.
    let temp = tempfile::tempdir().expect("temp dir");
    let source = source_in(
        temp.path(),
        &[(ZUNO_MODELS_PATH, "/nonexistent/t26/catalog.json")],
    );
    let error = source
        .load()
        .await
        .expect_err("a named-but-absent file is a mistake worth reporting");
    assert!(matches!(error, CatalogError::ExplicitPathUnreadable { .. }));
    assert!(
        error.to_string().contains("ZUNO_MODELS_PATH"),
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
        &[(ZUNO_MODELS_PATH, &pinned.to_string_lossy())],
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
        &[(ZUNO_MODELS_URL, "https://mirror.example.com")],
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
            zuno_paths::sha1::hex(b"https://mirror.example.com")
        )
    );
}
