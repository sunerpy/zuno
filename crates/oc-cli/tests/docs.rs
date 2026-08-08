//! The documentation gate: every table in `docs/` is generated from the code it
//! describes, and this target proves the committed prose still matches.
//!
//! # Why a test that reads Markdown is not the vacuous test it looks like
//!
//! The obvious way to satisfy "document every divergence" is to hand-write a
//! table and add a test that reads that same table. Such a test proves nothing:
//! both sides are the same artifact, so it passes for any content, including
//! content that contradicts the code.
//!
//! Every assertion here therefore derives its *expected* side from a live code
//! artifact and its *actual* side from the committed Markdown:
//!
//! | doc block | derived from |
//! |---|---|
//! | `divergence-index`, `divergence-detail` | [`oc_testkit::DivergenceList`] over `docs/divergences.toml`, cross-checked against [`oc_testkit::divergence::DECLARED_COUNT`] |
//! | `cli-disposition` | [`oc_cli::dispositions`] — the same table `oc-cli/tests/surface.rs` asserts against the registered `clap` tree |
//! | `api-operations` | the served document from [`oc_server::api::openapi`] set-differenced against the committed 1.18.12 oracle capture, then **probed route by route** for a `501` stub |
//! | `v1-routes` | [`oc_server::V1_SURFACE`] |
//! | `rejected-inputs` | messages *rendered by* [`oc_config::legacy`]'s detectors, so a reworded message fails |
//! | `plugin-hooks` | [`oc_plugin::HookName::ALL`] |
//! | `prune-tables` | [`oc_db::prune::PRUNE_TABLES`] and [`oc_db::prune::DELETE_ORDER`] |
//! | `migration-journal` | [`oc_db::migration::MIGRATION_IDS`] and [`oc_db::migration::CURRENT_VERSION`] |
//! | `cross-session-memory` | [`oc_config::schema::ResolvedMemoryConfig`], [`oc_memory::Scope`] and [`oc_agent::reflection::NEGATIVE_LEARNING_LIST`] |
//!
//! So adding a divergence entry, registering a command, renaming a rejection
//! message, serving a new `/api` operation, or adding a migration all fail here
//! until the documentation is updated.
//!
//! # Regeneration
//!
//! `OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs` rewrites each block
//! **from the code** and then re-asserts. That is the intended way to satisfy a
//! failure: the mechanical edit is applied by the tool that read the source of
//! truth, not typed from memory. Prose outside the markers is never touched.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use oc_cli::{Disposition, dispositions};
use oc_server::api::{self, ApiState};
use oc_server::{ServerBuilder, ServerConfig, V1_SURFACE};
use oc_testkit::{DivergenceList, divergence};
use tower::ServiceExt as _;

/// The committed capture of the 1.18.12 release's OpenAPI document.
///
/// The same fixture `crates/oc-testkit/tests/compat_suite.rs` compares against,
/// so the documentation and the compatibility gate cannot disagree about what
/// upstream declares.
const ORACLE_OPENAPI_FIXTURE: &str = ".omo/fixtures/oracle-openapi-1.18.12.json";

/// How many `/api` operations that capture declares. Restated so a replaced
/// fixture fails loudly here as well as in the compatibility suite.
const ORACLE_API_OPERATIONS: usize = 58;

/// Stand-in for a filesystem path inside a generated example message.
///
/// Rejection messages embed the offending file's absolute path, which is not a
/// documentable constant. The detector renders a real message against a known
/// location and that location is replaced by this token, so everything *except*
/// the path is still compared byte for byte.
const PATH_PLACEHOLDER: &str = "<file>";

// ---------------------------------------------------------------------------
// Block plumbing
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crates/oc-cli has a workspace root two levels up")
        .to_path_buf()
}

fn begin_marker(name: &str) -> String {
    format!("<!-- generated:BEGIN {name} -->")
}

fn end_marker(name: &str) -> String {
    format!("<!-- generated:END {name} -->")
}

fn regenerating() -> bool {
    matches!(
        std::env::var("OC_DOCS_REGENERATE").as_deref(),
        Ok("1" | "true")
    )
}

/// Asserts the named block in `relative` equals `expected`, byte for byte.
///
/// The failure message carries the whole generated block, because the only
/// correct response to a mismatch is to take the code's version verbatim.
fn check_block(relative: &str, name: &str, expected: &str) {
    let path = workspace_root().join(relative);
    let original = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    let begin = begin_marker(name);
    let end = end_marker(name);
    let start = original.find(&begin).unwrap_or_else(|| {
        panic!(
            "{} has no `{begin}` marker; the generated block must be delimited so prose and \
             generated content cannot be confused",
            path.display()
        )
    });
    let body_start = start + begin.len();
    let body_end = original[body_start..]
        .find(&end)
        .map(|offset| body_start + offset)
        .unwrap_or_else(|| panic!("{} has `{begin}` but no `{end}`", path.display()));

    let actual = original[body_start..body_end].trim_matches('\n');
    let expected = expected.trim_matches('\n');

    if actual == expected {
        return;
    }

    if regenerating() {
        let rewritten = format!(
            "{}\n{expected}\n{}",
            &original[..body_start],
            &original[body_end..]
        );
        std::fs::write(&path, rewritten)
            .unwrap_or_else(|error| panic!("rewrite {}: {error}", path.display()));
        eprintln!("regenerated `{name}` in {relative}");
        return;
    }

    panic!(
        "{} block `{name}` is stale.\n\
         The code changed and the documentation did not. Run\n\
         \n    OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs\n\
         \nto take the generated version, then review the diff. Expected:\n\
         ----- BEGIN GENERATED -----\n{expected}\n----- END GENERATED -----\n\
         Found:\n----- BEGIN COMMITTED -----\n{actual}\n----- END COMMITTED -----",
        path.display()
    );
}

/// Asserts every needle appears in the document.
///
/// Used only for prose claims whose *content* is checked elsewhere — a link
/// target, a section anchor, a flag name. A claim of behaviour is never asserted
/// this way; those come from [`check_block`].
fn contains_all(relative: &str, needles: &[&str]) {
    let path = workspace_root().join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    for needle in needles {
        assert!(
            text.contains(needle),
            "{} must mention {needle:?}",
            path.display()
        );
    }
}

/// Escapes a table cell so a value containing `|` cannot forge a column.
fn cell(value: &str) -> String {
    value.replace('|', "\\|")
}

// ---------------------------------------------------------------------------
// Divergences
// ---------------------------------------------------------------------------

fn allow_list() -> DivergenceList {
    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    assert_eq!(
        list.len(),
        divergence::DECLARED_COUNT,
        "the allow-list declares {} entries but oc_testkit::divergence::DECLARED_COUNT expects \
         {}; the documentation gate refuses to render a list the compatibility gate rejects",
        list.len(),
        divergence::DECLARED_COUNT
    );
    assert!(
        !list.is_empty(),
        "an empty allow-list would make every documentation assertion below vacuous"
    );
    list
}

fn divergence_index(list: &DivergenceList) -> String {
    let mut out = String::from("| # | id | surface |\n|---:|---|---|\n");
    for (index, entry) in list.entries().iter().enumerate() {
        let _ = writeln!(
            out,
            "| {} | [`{}`](divergences.md#{}) | {} |",
            index + 1,
            entry.id,
            entry.id,
            cell(&entry.surface)
        );
    }
    out
}

fn divergence_detail(list: &DivergenceList) -> String {
    let mut out = String::new();
    for (index, entry) in list.entries().iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "### {}\n", entry.id);
        let _ = writeln!(out, "**Surface.** {}\n", entry.surface);
        let _ = writeln!(out, "**Why.** {}", entry.reason);
    }
    out
}

fn cross_session_memory_block() -> String {
    use oc_agent::reflection::NEGATIVE_LEARNING_LIST;
    use oc_config::schema::ResolvedMemoryConfig;
    use oc_memory::Scope;

    let defaults = ResolvedMemoryConfig::default();
    assert_eq!(defaults.global_char_limit, Scope::Global.cap());
    assert_eq!(defaults.project_char_limit, Scope::Project.cap());
    let resident_budget = defaults.global_char_limit + defaults.project_char_limit;
    let mut out = format!(
        "Persistent memory is **enabled by default**. With both non-empty scopes, the default \
resident prompt budget is up to **{resident_budget} stored characters** \
(`{global}` global + `{project}` project), plus two rendered scope headers. The \
model-facing tool schema also adds request metadata while enabled. No embedding model, vector \
database, or external memory service is used.\n\n\
`memory: false` is the only supported strict-parity mode: resident files are not opened, the \
`memory` tool is not advertised, reflection cannot spawn, and the original system-prompt bytes \
are returned unchanged.\n\n\
| key | default | effect |\n|---|---:|---|\n\
| `memory` | `true` | master switch for all three surfaces |\n\
| `memory.resident` | `{resident}` | inject session-frozen global and project blocks |\n\
| `memory.tool` | `{tool}` | advertise the model-facing `memory` tool |\n\
| `memory.reflection` | `{reflection}` | permit post-response reflection tasks |\n\
| `memory.global_char_limit` | `{global}` | cap `$CONFIG/memory/MEMORY.md` in Unicode scalar values |\n\
| `memory.project_char_limit` | `{project}` | cap `<worktree>/.opencode/RULES.md` in Unicode scalar values |\n\
| `memory.nudge_interval` | `{interval}` | periodic reflection cadence in delivered turns; `0` disables only that trigger |\n\n\
Reflection must not learn any of these negative cases:\n",
        global = defaults.global_char_limit,
        project = defaults.project_char_limit,
        resident = defaults.resident,
        tool = defaults.tool,
        reflection = defaults.reflection,
        interval = defaults.nudge_interval,
    );
    for exclusion in NEGATIVE_LEARNING_LIST {
        let _ = writeln!(out, "- {exclusion}");
    }
    out
}

#[test]
fn docs_every_declared_divergence_is_documented_with_its_reason() {
    let list = allow_list();
    check_block(
        "docs/divergences.md",
        "divergence-detail",
        &divergence_detail(&list),
    );

    let page = std::fs::read_to_string(workspace_root().join("docs/divergences.md"))
        .expect("read the divergence page");
    for entry in list.entries() {
        assert!(
            page.contains(&format!("### {}", entry.id)),
            "divergence {} has no `###` section on docs/divergences.md",
            entry.id
        );
        assert!(
            page.contains(entry.reason.as_str()),
            "divergence {} appears on docs/divergences.md without the reason declared in {}",
            entry.id,
            list.path().display()
        );
    }
}

// ---------------------------------------------------------------------------
// CLI disposition
// ---------------------------------------------------------------------------

fn cli_disposition_block() -> String {
    let mut out =
        String::from("| upstream symbol | command | disposition | why |\n|---|---|---|---|\n");
    for row in dispositions() {
        let label = match row.disposition {
            Disposition::Implemented => "implemented",
            Disposition::Rejected => "rejected",
            Disposition::NotRegistered => "not-registered",
        };
        let _ = writeln!(
            out,
            "| `{}` | `{}` | {label} | {} |",
            row.upstream_symbol,
            row.command,
            cell(row.reason)
        );
    }
    out
}

fn assert_cli_disposition_counts() {
    let implemented = dispositions()
        .iter()
        .filter(|row| row.disposition == Disposition::Implemented)
        .count();
    let rejected = dispositions()
        .iter()
        .filter(|row| row.disposition == Disposition::Rejected)
        .count();
    let deferred = dispositions()
        .iter()
        .filter(|row| row.disposition == Disposition::NotRegistered)
        .count();
    contains_all(
        "docs/compatibility-matrix.md",
        &[
            &format!("{implemented} implemented"),
            &format!("{rejected} rejected"),
            &format!("{deferred} not-registered"),
        ],
    );
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

/// The `(path, method)` pairs an OpenAPI document declares under `/api/`.
fn api_operations(document: &serde_json::Value) -> BTreeSet<(String, String)> {
    let mut operations = BTreeSet::new();
    for (path, item) in document["paths"]
        .as_object()
        .expect("an OpenAPI document has a paths object")
    {
        if !path.starts_with("/api/") {
            continue;
        }
        for method in item.as_object().into_iter().flatten().map(|(name, _)| name) {
            if matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                operations.insert((path.clone(), method.clone()));
            }
        }
    }
    operations
}

fn oracle_api_operations() -> BTreeSet<(String, String)> {
    let path = workspace_root().join(ORACLE_OPENAPI_FIXTURE);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let document: serde_json::Value =
        serde_json::from_str(&text).expect("the oracle capture is JSON");
    let operations = api_operations(&document);
    assert_eq!(
        operations.len(),
        ORACLE_API_OPERATIONS,
        "the committed oracle capture no longer declares {ORACLE_API_OPERATIONS} /api operations"
    );
    operations
}

/// Replaces `{param}` and `{*param}` segments with a concrete value.
fn concrete_uri(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') || segment == "*" {
                "probe"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether each served operation answers `501 Not Implemented`.
///
/// This is the difference between "the route exists" and "the route does
/// something", and it is the difference the plan's "must not document a
/// capability that has no passing test" turns on. Measured by driving the real
/// router rather than by reading the registration code, so a stub that gains a
/// handler is reclassified automatically.
async fn probe_stubs(served: &BTreeSet<(String, String)>) -> BTreeSet<(String, String)> {
    let state = ApiState::memory("/repo").expect("in-memory API state");
    let app: Router = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(api::router(state))
        .router();

    let mut stubs = BTreeSet::new();
    for (path, method) in served {
        let verb = Method::from_bytes(method.to_uppercase().as_bytes()).expect("known HTTP verb");
        let request = Request::builder()
            .method(verb)
            .uri(concrete_uri(path))
            .body(Body::empty())
            .expect("probe request is valid");
        let response = app
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers every registered route");
        if response.status() == StatusCode::NOT_IMPLEMENTED {
            stubs.insert((path.clone(), method.clone()));
        }
    }
    stubs
}

fn api_block(
    upstream: &BTreeSet<(String, String)>,
    served: &BTreeSet<(String, String)>,
    stubs: &BTreeSet<(String, String)>,
) -> String {
    let mut rows: BTreeMap<(String, String), &'static str> = BTreeMap::new();
    for operation in upstream.union(served) {
        let state = match (
            upstream.contains(operation),
            served.contains(operation),
            stubs.contains(operation),
        ) {
            (_, true, true) => "registered (501 stub)",
            (true, true, false) => "implemented",
            (false, true, false) => "added",
            (true, false, _) => "not-registered",
            (false, false, _) => unreachable!("an operation is in at least one set"),
        };
        rows.insert(operation.clone(), state);
    }

    let mut out = String::from("| method | path | state |\n|---|---|---|\n");
    for ((path, method), state) in &rows {
        let _ = writeln!(out, "| {} | `{path}` | {state} |", method.to_uppercase());
    }
    out
}

fn assert_api_counts(
    upstream: &BTreeSet<(String, String)>,
    served: &BTreeSet<(String, String)>,
    stubs: &BTreeSet<(String, String)>,
) {
    let missing = upstream.difference(served).count();
    let added = served.difference(upstream).count();
    contains_all(
        "docs/compatibility-matrix.md",
        &[
            &format!(
                "{} of the {} upstream",
                upstream.len() - missing,
                upstream.len()
            ),
            &format!("{added} operation"),
            &format!("{} registered as a 501 stub", stubs.len()),
        ],
    );
}

fn v1_block() -> String {
    let mut out = String::from("| method | path | SDK method |\n|---|---|---|\n");
    for route in V1_SURFACE {
        let _ = writeln!(
            out,
            "| {} | `{}` | `{}` |",
            route.method.as_str(),
            route.path,
            route.sdk_method
        );
    }
    out
}

/// Every table on the matrix page, checked by one test.
///
/// One test per file is not a stylistic choice: [`check_block`] rewrites the
/// whole file under `OC_DOCS_REGENERATE`, so two tests regenerating two blocks of
/// the same page race and each silently discards the other's write.
#[test]
fn docs_compatibility_matrix_matches_every_code_table() {
    const PAGE: &str = "docs/compatibility-matrix.md";

    let list = allow_list();
    check_block(PAGE, "divergence-index", &divergence_index(&list));
    check_block(PAGE, "cross-session-memory", &cross_session_memory_block());
    check_block(PAGE, "cli-disposition", &cli_disposition_block());

    let upstream = oracle_api_operations();
    let served = api_operations(&oc_server::api::openapi());
    let stubs = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime")
        .block_on(probe_stubs(&served));
    check_block(
        PAGE,
        "api-operations",
        &api_block(&upstream, &served, &stubs),
    );

    check_block(PAGE, "v1-routes", &v1_block());

    assert_cli_disposition_counts();
    assert_api_counts(&upstream, &served, &stubs);
    contains_all(PAGE, &[&format!("{} v1 route", V1_SURFACE.len())]);
    contains_all(PAGE, &oc_agent::reflection::NEGATIVE_LEARNING_LIST);
}

// ---------------------------------------------------------------------------
// Rejected inputs
// ---------------------------------------------------------------------------

/// One documented rejection, with the message its detector actually renders.
struct Rejection {
    form: oc_config::DeprecatedForm,
    found: String,
    replacement: String,
    message: String,
}

/// Renders every deprecated form through the real detectors.
///
/// Nothing here is transcribed: the `found`, `replacement`, and `message` all
/// come off the [`oc_config::Deprecation`] the production code produced, so a
/// reworded replacement fails the documentation gate.
fn rejections() -> Vec<Rejection> {
    use oc_config::legacy::{
        inspect_agent_frontmatter, inspect_auth, inspect_config, inspect_global_directory,
    };

    let config = Path::new("/example/opencode.json");
    let auth = Path::new("/example/auth.json");
    let agent = Path::new("/example/agent/build.md");

    let mut found: Vec<oc_config::Deprecation> = inspect_config(
        config,
        &serde_json::json!({
            "mode": { "build": {} },
            "layout": "auto",
            "autoshare": true,
            "reference": {},
            "agent": { "build": { "tools": {}, "maxSteps": 1 } },
        }),
    );
    found.extend(inspect_agent_frontmatter(
        agent,
        &serde_json::json!({ "tools": {}, "maxSteps": 1 }),
    ));
    found.extend(inspect_auth(
        auth,
        &serde_json::json!({ "prompts": [{ "condition": true }] }),
    ));
    // `AuthPromptCondition` has two detectors with two different replacements.
    // Both are documented, because a plugin author reaching the descriptor path
    // sees the longer message and would otherwise not find it on the page.
    found.extend(oc_config::legacy::auth_prompt_deprecation(
        auth,
        vec!["methods".to_owned(), "0".to_owned()],
        ["condition"],
    ));

    // The file-shaped forms need real files. The scratch directory is replaced
    // by the placeholder afterwards so only the path varies.
    let dir = tempfile::tempdir().expect("scratch directory");
    std::fs::create_dir(dir.path().join("mode")).expect("create mode/");
    std::fs::write(dir.path().join("mode/plan.md"), "# plan").expect("write mode/plan.md");
    std::fs::write(dir.path().join("CONTEXT.md"), "legacy").expect("write CONTEXT.md");
    std::fs::write(dir.path().join("config"), "model = \"x\"\n").expect("write config");
    found.extend(inspect_global_directory(dir.path()));

    let scratch = dir.path().display().to_string();
    let mut rejections: Vec<Rejection> = found
        .into_iter()
        .map(|deprecation| Rejection {
            form: deprecation.form(),
            found: deprecation.found().to_owned(),
            replacement: deprecation.replacement().to_owned(),
            message: deprecation.message().replace(&scratch, PATH_PLACEHOLDER),
        })
        .collect();
    rejections.sort_by(|left, right| {
        format!("{:?}", left.form)
            .cmp(&format!("{:?}", right.form))
            .then_with(|| left.found.cmp(&right.found))
            .then_with(|| left.message.cmp(&right.message))
    });
    rejections
}

fn rejected_inputs_block(rejections: &[Rejection]) -> String {
    let mut out = String::new();
    let mut current = None;
    for rejection in rejections {
        let heading = format!("{:?}", rejection.form);
        if current.as_deref() != Some(heading.as_str()) {
            if current.is_some() {
                out.push('\n');
            }
            let _ = writeln!(out, "### {heading}\n");
            current = Some(heading);
        }
        let _ = writeln!(
            out,
            "- Rejected: `{}` — {}\n\n  ```text\n  {}\n  ```\n",
            rejection.found, rejection.replacement, rejection.message
        );
    }
    out
}

#[test]
fn docs_every_rejected_form_is_documented_with_the_message_the_code_renders() {
    let rejections = rejections();
    let forms: BTreeSet<String> = rejections
        .iter()
        .map(|rejection| format!("{:?}", rejection.form))
        .collect();
    assert_eq!(
        forms.len(),
        10,
        "all ten deprecated forms must be reachable for the page to be complete; got {forms:?}"
    );

    check_block(
        "docs/rejected-inputs.md",
        "rejected-inputs",
        &rejected_inputs_block(&rejections),
    );

    // The load-bearing claim of the page: the documented message is the message
    // the binary prints. Asserted independently of the block rendering so a
    // hand-edit inside the markers still fails.
    let page = std::fs::read_to_string(workspace_root().join("docs/rejected-inputs.md"))
        .expect("read the rejected-input page");
    for rejection in &rejections {
        assert!(
            page.contains(&rejection.message),
            "docs/rejected-inputs.md does not contain the exact message oc-config renders for \
             {:?}:\n  {}",
            rejection.form,
            rejection.message
        );
    }
}

// ---------------------------------------------------------------------------
// Plugin hooks
// ---------------------------------------------------------------------------

fn plugin_hook_block() -> String {
    let mut out = String::from("| hook | JavaScript / JSON-RPC name |\n|---:|---|\n");
    for (index, hook) in oc_plugin::HookName::ALL.iter().enumerate() {
        let _ = writeln!(out, "| {} | `{}` |", index + 1, hook.as_str());
    }
    out
}

#[test]
fn docs_plugin_guide_matches_the_hooks_and_the_example_the_host_ships() {
    check_block(
        "docs/plugin-authoring.md",
        "plugin-hooks",
        &plugin_hook_block(),
    );
    contains_all(
        "docs/plugin-authoring.md",
        &[&format!("{} hooks", oc_plugin::HookName::ALL.len())],
    );

    let example = workspace_root().join("examples/rust_plugin.rs");
    assert!(
        example.is_file(),
        "the guide points at {}, which must exist",
        example.display()
    );
    let source = std::fs::read_to_string(&example).expect("read the example");
    contains_all(
        "docs/plugin-authoring.md",
        &["examples/rust_plugin.rs", "oc_plugin_sdk"],
    );
    for symbol in ["Plugin::new", "ToolDefinition::new", "ConformanceSuite"] {
        assert!(
            source.contains(symbol),
            "examples/rust_plugin.rs no longer uses {symbol}, which the guide documents"
        );
    }
}

// ---------------------------------------------------------------------------
// Session retention (C8) and migrations
// ---------------------------------------------------------------------------

fn prune_table_block() -> String {
    let mut out = String::from("| order | table |\n|---:|---|\n");
    for (index, table) in oc_db::prune::DELETE_ORDER.iter().enumerate() {
        let _ = writeln!(out, "| {} | `{table}` |", index + 1);
    }
    out
}

#[test]
fn docs_retention_guide_lists_the_tables_delete_actually_touches() {
    assert_eq!(
        oc_db::prune::DELETE_ORDER.len(),
        oc_db::prune::PRUNE_TABLES.len(),
        "the documented delete order must cover every pruned table"
    );
    check_block(
        "docs/session-retention.md",
        "prune-tables",
        &prune_table_block(),
    );
    contains_all(
        "docs/session-retention.md",
        &[
            "--delete",
            "--archive",
            "irreversible",
            "reversible",
            "time_archived",
            &format!("{} tables", oc_db::prune::DELETE_ORDER.len()),
        ],
    );
}

fn migration_block() -> String {
    let mut out = String::from("| # | migration id |\n|---:|---|\n");
    for (index, id) in oc_db::migration::MIGRATION_IDS.iter().enumerate() {
        let _ = writeln!(out, "| {} | `{id}` |", index + 1);
    }
    out
}

#[test]
fn docs_migration_guide_lists_every_migration_in_execution_order() {
    assert_eq!(
        u32::try_from(oc_db::migration::MIGRATION_IDS.len()).expect("the id count fits in u32"),
        oc_db::migration::CURRENT_VERSION,
        "CURRENT_VERSION must equal the number of migration ids"
    );
    check_block("docs/migration.md", "migration-journal", &migration_block());
    contains_all(
        "docs/migration.md",
        &[
            oc_db::migration::DRIZZLE_JOURNAL_TABLE,
            &format!("{} migrations", oc_db::migration::MIGRATION_IDS.len()),
            "OPENCODE_DB",
            "OPENCODE_DISABLE_CHANNEL_DB",
            "opencode-local.db",
        ],
    );
}

// ---------------------------------------------------------------------------
// README
// ---------------------------------------------------------------------------

#[test]
fn readme_states_the_pinned_baseline_the_binary_actually_reports() {
    contains_all(
        "README.md",
        &[
            oc_cli::COMPATIBILITY_VERSION,
            &format!("--version` reports `{}`", oc_cli::COMPATIBILITY_VERSION),
        ],
    );
}

#[test]
fn readme_documents_the_four_gaps_a_side_by_side_user_hits() {
    contains_all(
        "README.md",
        &[
            // 1. the channel database filename
            "opencode-local.db",
            "OPENCODE_DISABLE_CHANNEL_DB",
            // 2. the absent event stream
            "/api/event",
            // 3. legacy databases
            oc_db::migration::DRIZZLE_JOURNAL_TABLE,
            // 4. provider coverage by wire family
            "provider-coverage-by-wire-family",
            // and the rollback the QA scenario requires
            "Rolling back",
        ],
    );
}

#[test]
fn readme_reports_every_non_functional_gate_with_its_opt_in_command() {
    contains_all(
        "README.md",
        &[
            "OC_MEMORY_GATE_MODE=run",
            "--ignored",
            "cargo test --workspace",
            // the three caveats, each as a claim a reader can check
            "1.27%",
            "does not mean G1-G6 pass",
            "ses_2bcaee257ffeFZNJrmtpi3ZglR",
        ],
    );
}

#[test]
fn readme_links_every_page_this_target_checks() {
    contains_all(
        "README.md",
        &[
            "docs/compatibility-matrix.md",
            "docs/divergences.md",
            "docs/rejected-inputs.md",
            "docs/plugin-authoring.md",
            "docs/session-retention.md",
            "docs/migration.md",
        ],
    );
    for relative in [
        "docs/compatibility-matrix.md",
        "docs/divergences.md",
        "docs/rejected-inputs.md",
        "docs/plugin-authoring.md",
        "docs/session-retention.md",
        "docs/migration.md",
    ] {
        let path = workspace_root().join(relative);
        assert!(path.is_file(), "{} is linked but absent", path.display());
    }
}
