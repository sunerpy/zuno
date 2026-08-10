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
//! | `api-operations` | the served document from [`oc_server::api::openapi`] set-differenced against the committed 1.18.12 oracle capture, then **probed route by route** for an explicit `503 backend_unavailable` gap; any `501` fails the gate |
//! | `known-gaps` | [`oc_testkit::compat_report::known_gaps`] — the same list the compatibility report writes, rendered with the API counts probed above |
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
use std::sync::Arc;

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

/// Spells a small count the way the README's prose does.
///
/// The README writes "the seventeen deliberate differences", not "the 17", so a
/// derived assertion has to match its register. Only the range the allow-list can
/// plausibly reach is covered; anything outside it fails loudly rather than
/// silently formatting a digit the prose would never contain.
fn spell(count: usize) -> &'static str {
    match count {
        13 => "thirteen",
        14 => "fourteen",
        15 => "fifteen",
        16 => "sixteen",
        17 => "seventeen",
        18 => "eighteen",
        19 => "nineteen",
        20 => "twenty",
        other => panic!(
            "the allow-list holds {other} entries, which this helper cannot spell; extend it and \
             update the README's prose in the same commit"
        ),
    }
}

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

    // The README names this count in prose, and nothing derived it until F1 and F4
    // both found it still saying "thirteen" while the allow-list held seventeen.
    // Spelling the number from the live list means the next entry cannot leave the
    // README stale, which is the whole reason the other README figures are probed
    // rather than retyped.
    contains_all(
        "README.md",
        &[&format!("the {} deliberate differences", spell(list.len()))],
    );

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

/// Which served operations explicitly report an unavailable backend.
///
/// This is the difference between "the route exists" and "the route does
/// something", and it is the difference the plan's "must not document a
/// capability that has no passing test" turns on. Measured by driving the real
/// router rather than by reading the registration code, so a gap that gains a
/// handler is reclassified automatically.
async fn probe_gaps(served: &BTreeSet<(String, String)>) -> BTreeSet<(String, String)> {
    let pool = Arc::new(
        oc_db::Pool::open(&oc_paths::DbLocation::Memory).expect("in-memory event database"),
    );
    let events = oc_server::EventService::new(pool, 8);
    let state = ApiState::memory("/repo")
        .expect("in-memory API state")
        .with_events(events.clone());
    let app: Router = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(api::router(state).merge(oc_server::events_router(events)))
        .router();

    let mut gaps = BTreeSet::new();
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
        assert_ne!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "{method} {path} is a registered 501 stub, not an implemented operation or explicit gap"
        );
        if response.status() == StatusCode::SERVICE_UNAVAILABLE {
            gaps.insert((path.clone(), method.clone()));
        }
    }
    gaps
}

fn api_block(
    upstream: &BTreeSet<(String, String)>,
    served: &BTreeSet<(String, String)>,
    gaps: &BTreeSet<(String, String)>,
) -> String {
    let mut rows: BTreeMap<(String, String), &'static str> = BTreeMap::new();
    for operation in upstream.union(served) {
        let state = match (
            upstream.contains(operation),
            served.contains(operation),
            gaps.contains(operation),
        ) {
            (_, true, true) => "explicit gap (503 backend unavailable)",
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
    gaps: &BTreeSet<(String, String)>,
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
            &format!("{} explicit 503 backend gaps", gaps.len()),
        ],
    );

    // The README restates this split, and once shipped it inverted: 23 backed
    // against the table's 35. Both halves now come from the same probe.
    let backed = upstream
        .iter()
        .filter(|operation| served.contains(*operation) && !gaps.contains(*operation))
        .count();
    let gapped = upstream
        .iter()
        .filter(|operation| gaps.contains(*operation))
        .count();
    assert_eq!(
        backed + gapped + missing,
        upstream.len(),
        "every upstream operation is backed, gapped, or unregistered"
    );
    contains_all(
        "README.md",
        &[
            &format!("all {} upstream operations", upstream.len()),
            &format!(
                "{backed} of the {} upstream operations have local backends",
                upstream.len()
            ),
            &format!("the remaining {gapped} return an"),
        ],
    );
}

/// Renders the compatibility report's `known_gaps` section as documentation.
///
/// The counts come from the same live probe [`api_block`] uses, not from constants
/// restated here, so an API gap closing rewrites this page's text without anyone
/// editing it. Before plan todo 140 this block did not exist and the gap list was
/// reachable only inside the uncommitted `target/compat/compat-report.json`, while
/// `docs/divergences.md` promised twice that a gap is listed on this page.
fn known_gaps_block(
    upstream: &BTreeSet<(String, String)>,
    gaps: &BTreeSet<(String, String)>,
) -> String {
    let mut out = String::new();
    for (index, gap) in oc_testkit::compat_report::known_gaps(gaps.len(), upstream.len())
        .iter()
        .enumerate()
    {
        if index > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "### {}\n", gap.id);
        let _ = writeln!(out, "**Surface.** {}\n", gap.surface);
        let _ = writeln!(out, "**What is missing.** {}", gap.detail);
    }
    out
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
    let gaps = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime")
        .block_on(probe_gaps(&served));
    check_block(
        PAGE,
        "api-operations",
        &api_block(&upstream, &served, &gaps),
    );

    check_block(PAGE, "known-gaps", &known_gaps_block(&upstream, &gaps));
    check_block(PAGE, "v1-routes", &v1_block());

    assert_cli_disposition_counts();
    assert_api_counts(&upstream, &served, &gaps);
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
// The resident-memory gates, G1 and G2
// ---------------------------------------------------------------------------
//
// The README publishes measured numbers. A table of numbers checked against a
// hand-typed expectation proves only that two hand-typed things agree, so
// nothing below restates a figure. Every published value is computed from two
// live artifacts:
//
//   * `benchmarks/ts-baseline.json`, via [`oc_testkit::perf::FrozenThresholds`],
//     supplies the ceilings; and
//   * the newest committed measurement artefact under `.omo/evidence` supplies
//     the five per-repetition Rust peaks.
//
// Median, margin, spread, ratio and verdict are then derived here and rendered
// into a generated README block. The artefact's own narrative is used only as a
// cross-check: where its prose disagrees with its own table or with the
// committed baseline, this target fails instead of republishing either.

/// Directory holding the committed measurement artefacts.
const EVIDENCE_DIR: &str = ".omo/evidence";

/// Prefix and suffix of an evidence artefact's filename, around its task number.
const EVIDENCE_PREFIX: &str = "task-";
const EVIDENCE_SUFFIX: &str = "-opencode-rust.txt";

/// Column width the README's hand-written prose wraps at.
const PROSE_WIDTH: usize = 79;

/// Parses `1,494,024` into `1494024`.
fn parse_grouped(token: &str) -> Option<u64> {
    let digits: String = token.chars().filter(|c| *c != ',').collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Renders `1494024` as `1,494,024`.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Greedy wrap so generated prose matches the README's hand-written width.
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut column = 0usize;
    for word in text.split_whitespace() {
        if column == 0 {
            out.push_str(word);
            column = word.chars().count();
        } else if column + 1 + word.chars().count() <= width {
            out.push(' ');
            out.push_str(word);
            column += 1 + word.chars().count();
        } else {
            out.push('\n');
            out.push_str(word);
            column = word.chars().count();
        }
    }
    out
}

/// One gate's figures. Only `peaks` and `committed_ts_median_kib` are read from
/// the artefact; every other quantity the README prints is derived below.
#[derive(Debug, Clone, Copy)]
struct GateFigures {
    /// The five per-repetition Rust peaks, ascending.
    peaks: [u64; 5],
    /// The TypeScript median the frozen baseline committed for this workload.
    committed_ts_median_kib: u64,
    /// `0.50 x` that median, taken from [`oc_testkit::perf::FrozenThresholds`].
    ceiling_kib: u64,
}

impl GateFigures {
    /// The reported value: the median of the five retained peaks.
    fn median_kib(self) -> u64 {
        self.peaks[2]
    }

    /// Widest pair of per-run peaks — the run-to-run variance the median hides.
    fn spread_kib(self) -> u64 {
        self.peaks[4] - self.peaks[0]
    }

    /// How far the median sits under the ceiling, `None` when it does not.
    fn margin_kib(self) -> Option<u64> {
        self.ceiling_kib.checked_sub(self.median_kib())
    }

    /// How far the median sits *over* the ceiling, `None` when it does not.
    fn excess_kib(self) -> Option<u64> {
        self.median_kib().checked_sub(self.ceiling_kib)
    }

    /// The frozen predicate itself: median at or under the ceiling.
    fn passes(self) -> bool {
        self.median_kib() <= self.ceiling_kib
    }

    /// The verdict the frozen predicate implies, independent of any prose.
    fn verdict(self) -> &'static str {
        if self.passes() { "PASS" } else { "FAIL" }
    }

    /// Rust median as a fraction of the committed TypeScript median.
    fn ratio(self) -> f64 {
        self.median_kib() as f64 / self.committed_ts_median_kib as f64
    }

    /// The margin as a percentage of the ceiling, the form the README quotes.
    fn margin_percent(self) -> Option<f64> {
        self.margin_kib()
            .map(|margin| margin as f64 * 100.0 / self.ceiling_kib as f64)
    }

    /// The per-run peaks that exceeded the ceiling. Empty is the strong claim.
    fn peaks_over_ceiling(self) -> Vec<u64> {
        self.peaks
            .into_iter()
            .filter(|peak| *peak > self.ceiling_kib)
            .collect()
    }
}

/// A whole G1 + G2 measurement, attributed to the artefact it came from.
#[derive(Debug, Clone)]
struct MemoryGateMeasurement {
    /// Repository-relative path of the artefact, as the README cites it.
    artefact: String,
    g1: GateFigures,
    g2: GateFigures,
}

/// Slices one `FROZEN GATE RESULTS` subsection, which is blank-line delimited.
fn gate_section<'text>(text: &'text str, header: &str) -> Option<&'text str> {
    let start = text.find(&format!("\n{header}\n"))? + 1;
    let rest = &text[start..];
    let end = rest.find("\n\n").unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Reads `  committed TypeScript median: 954,240 KiB` from a gate subsection.
fn section_kib(section: &str, key: &str) -> Option<u64> {
    section
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(key))
        .and_then(|value| parse_grouped(value.trim().trim_end_matches("KiB").trim()))
}

/// Reads the five per-repetition peaks and the restated median from a raw row.
fn raw_peaks(text: &str, label: &str) -> Option<([u64; 5], u64)> {
    let row = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(label))?;
    let values: Vec<u64> = row
        .split_whitespace()
        .map(parse_grouped)
        .collect::<Option<Vec<u64>>>()?;
    let [r1, r2, r3, r4, r5, median] = <[u64; 6]>::try_from(values).ok()?;
    Some(([r1, r2, r3, r4, r5], median))
}

/// Parses one artefact, or `None` when it is not a G1/G2 measurement at all.
///
/// Every derived quantity is cross-checked against the artefact's own narrative:
/// the median it restates, the ceiling it prints, and the verdict it records must
/// all agree with what the raw five peaks and the frozen baseline imply. An
/// artefact whose prose contradicts its own table fails here rather than being
/// republished, and the *derived* value is what the README then carries.
fn parse_gate_figures(
    text: &str,
    thresholds: &oc_testkit::perf::FrozenThresholds,
) -> Option<[GateFigures; 2]> {
    let mut parsed = Vec::with_capacity(2);
    for (label, header, ceiling_mib) in [
        ("Rust W-idle", "G1 / W-idle", thresholds.g1_max_mib),
        ("Rust W-real", "G2 / W-real", thresholds.g2_max_mib),
    ] {
        let (mut peaks, restated_median) = raw_peaks(text, label)?;
        let section = gate_section(text, header)?;
        let committed_ts_median_kib = section_kib(section, "committed TypeScript median:")?;
        let recorded_ceiling = section_kib(section, "frozen ceiling (0.50 x committed):")?;
        let recorded_median = section_kib(section, "Rust median:")?;
        let recorded_verdict = section
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("verdict under the frozen formula:"))?
            .trim()
            .to_owned();

        let ceiling_kib_exact = ceiling_mib * 1024.0;
        let ceiling_kib = ceiling_kib_exact.round() as u64;
        assert!(
            (ceiling_kib_exact - ceiling_kib as f64).abs() < 0.5,
            "the frozen {header} ceiling {ceiling_kib_exact} KiB is not a whole number of KiB"
        );
        assert_eq!(
            recorded_ceiling, ceiling_kib,
            "{header}: the artefact prints a {recorded_ceiling} KiB ceiling but the frozen \
             baseline and formula give {ceiling_kib} KiB — the artefact was measured against a \
             different baseline than the one this repository commits"
        );

        peaks.sort_unstable();
        let figures = GateFigures {
            peaks,
            committed_ts_median_kib,
            ceiling_kib,
        };
        assert_eq!(
            figures.median_kib(),
            restated_median,
            "{header}: the artefact's raw row restates a median that is not the median of its own \
             five peaks {peaks:?}"
        );
        assert_eq!(
            figures.median_kib(),
            recorded_median,
            "{header}: the artefact's gate section reports a median its raw row contradicts"
        );
        assert_eq!(
            recorded_verdict,
            figures.verdict(),
            "{header}: the artefact records `{recorded_verdict}` but its own median {} KiB \
             against the frozen {ceiling_kib} KiB ceiling is a {}",
            figures.median_kib(),
            figures.verdict()
        );
        parsed.push(figures);
    }
    <[GateFigures; 2]>::try_from(parsed).ok()
}

/// Every committed G1/G2 measurement, oldest task number first.
///
/// Discovered rather than named, so a later re-measurement becomes the source of
/// truth the moment it is committed and a README left behind fails this target.
fn memory_gate_measurements() -> Vec<MemoryGateMeasurement> {
    let thresholds = oc_testkit::perf::FrozenThresholds::from_baseline(
        &oc_testkit::perf::load_committed_baseline()
            .expect("the committed TypeScript baseline must load"),
    )
    .expect("the frozen thresholds must substitute from the committed baseline");
    assert!(
        thresholds.all_finite(),
        "a non-finite frozen threshold would make every figure below meaningless"
    );

    let dir = workspace_root().join(EVIDENCE_DIR);
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    let mut candidates: BTreeMap<u64, MemoryGateMeasurement> = BTreeMap::new();
    for entry in entries {
        let path = entry
            .expect("an evidence directory entry must be readable")
            .path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(task) = name
            .strip_prefix(EVIDENCE_PREFIX)
            .and_then(|rest| rest.strip_suffix(EVIDENCE_SUFFIX))
            .and_then(|number| number.parse::<u64>().ok())
        else {
            continue;
        };
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        if let Some([g1, g2]) = parse_gate_figures(&text, &thresholds) {
            candidates.insert(
                task,
                MemoryGateMeasurement {
                    artefact: format!("{EVIDENCE_DIR}/{name}"),
                    g1,
                    g2,
                },
            );
        }
    }
    assert!(
        !candidates.is_empty(),
        "no artefact in {} records a G1/G2 measurement; the README's memory figures would have \
         no source",
        dir.display()
    );
    candidates.into_values().collect()
}

/// The G2 paragraph: the five raw peaks, then the margin-versus-spread ordering.
///
/// The *judgement* is generated too, not just the digits. Whether every run
/// passed, and whether the margin beats the spread, are both decided from the
/// peaks here — so a weaker future measurement produces weaker prose instead of
/// an unchanged boast.
fn g2_robustness_prose(
    current: &MemoryGateMeasurement,
    superseded: Option<&MemoryGateMeasurement>,
) -> String {
    let g2 = current.g2;
    let peaks = g2
        .peaks
        .iter()
        .map(|peak| grouped(*peak))
        .collect::<Vec<_>>()
        .join(" · ");
    let over = g2.peaks_over_ceiling();
    let coverage = if over.is_empty() {
        "Every one of the five is under the ceiling".to_owned()
    } else {
        format!(
            "**{} of the five sit over the ceiling**, so the median passes only because the \
             distribution is skewed",
            over.len()
        )
    };
    let spread = g2.spread_kib();
    let ordering = match (g2.margin_kib(), g2.excess_kib()) {
        (Some(margin), _) if margin > spread => format!(
            "and the median's {} KiB margin — {:.2}% of the ceiling — is {} KiB wider than the \
             {} KiB five-run spread. That ordering is the claim worth checking: a margin \
             narrower than the spread is a coin flip that landed, not a pass.",
            grouped(margin),
            g2.margin_percent().unwrap_or_default(),
            grouped(margin - spread),
            grouped(spread),
        ),
        (Some(margin), _) => format!(
            "but the median's {} KiB margin — {:.2}% of the ceiling — is **narrower than** the \
             {} KiB five-run spread. Read that as a coin flip that landed rather than a pass.",
            grouped(margin),
            g2.margin_percent().unwrap_or_default(),
            grouped(spread),
        ),
        (None, Some(excess)) => format!(
            "and the median finishes {} KiB **over** the ceiling against a {} KiB five-run \
             spread: G2 does not pass.",
            grouped(excess),
            grouped(spread),
        ),
        (None, None) => unreachable!("a median is either under or over its ceiling"),
    };

    let history = match superseded {
        Some(previous) if !previous.g2.passes() => format!(
            " The superseded measurement in [`{path}`]({path}) is the shape being avoided: a {} \
             KiB spread around a median that finished {} KiB over the same ceiling — {}.",
            grouped(previous.g2.spread_kib()),
            grouped(previous.g2.excess_kib().unwrap_or_default()),
            previous.g2.verdict(),
            path = previous.artefact,
        ),
        Some(previous) => format!(
            " The superseded measurement in [`{path}`]({path}) recorded a {} KiB median against a \
             {} KiB spread — {}.",
            grouped(previous.g2.median_kib()),
            grouped(previous.g2.spread_kib()),
            previous.g2.verdict(),
            path = previous.artefact,
        ),
        None => String::new(),
    };

    wrap(
        &format!("G2's five `W-real` peaks were {peaks} KiB. {coverage}, {ordering}{history}"),
        PROSE_WIDTH,
    )
}

/// Renders the README's memory-gate block from a measurement.
fn memory_gate_block(
    current: &MemoryGateMeasurement,
    superseded: Option<&MemoryGateMeasurement>,
) -> String {
    let artefact = &current.artefact;
    let mut out = wrap(
        &format!(
            "Derived from the newest committed measurement artefact, \
             [`{artefact}`]({artefact}). The ceilings are not measured here: \
             [`benchmarks/ts-baseline.json`](benchmarks/ts-baseline.json) freezes each one at \
             half the TypeScript median for the same workload, and every other column below is \
             computed from the five per-repetition Rust peaks the artefact records."
        ),
        PROSE_WIDTH,
    );
    out.push_str(
        "\n\n| gate | workload | Rust median peak | frozen ceiling | margin | five-run spread \
         | Rust / TypeScript | verdict |\n|---|---|---:|---:|---:|---:|---:|---|\n",
    );
    for (gate, workload, figures) in [("G1", "W-idle", current.g1), ("G2", "W-real", current.g2)] {
        let margin = match (figures.margin_kib(), figures.excess_kib()) {
            (Some(margin), _) => format!("{} KiB", grouped(margin)),
            (None, Some(excess)) => format!("−{} KiB", grouped(excess)),
            (None, None) => unreachable!("a median is either under or over its ceiling"),
        };
        let _ = writeln!(
            out,
            "| {gate} | `{workload}` | {} KiB | {} KiB | {margin} | {} KiB | {:.4} | {} |",
            grouped(figures.median_kib()),
            grouped(figures.ceiling_kib),
            grouped(figures.spread_kib()),
            figures.ratio(),
            figures.verdict(),
        );
    }
    out.push('\n');
    out.push_str(&g2_robustness_prose(current, superseded));
    out.push('\n');
    out
}

#[test]
fn readme_publishes_the_newest_measured_memory_figures_not_a_remembered_one() {
    let mut measurements = memory_gate_measurements();
    let current = measurements
        .pop()
        .expect("memory_gate_measurements asserts the list is non-empty");
    let superseded = measurements.pop();

    // The block is derived end to end, so a stale digit in the README fails here.
    check_block(
        "README.md",
        "memory-gate-measurement",
        &memory_gate_block(&current, superseded.as_ref()),
    );

    // Both cited artefacts must actually be present, so the README cannot point a
    // reader at a measurement that was never committed.
    for measurement in std::iter::once(&current).chain(superseded.as_ref()) {
        let path = workspace_root().join(&measurement.artefact);
        assert!(
            path.is_file(),
            "{} is cited by the README's memory block but absent",
            path.display()
        );
    }
}

/// The derived side has to dominate the artefact's prose, not defer to it.
///
/// Without this, `parse_gate_figures` could quietly trust a narrative that says
/// `PASS` while its own peaks say otherwise — the exact failure mode of a test
/// whose expected and actual sides come from the same hand-written source.
#[test]
#[should_panic(expected = "records `PASS` but its own median")]
fn a_measurement_whose_prose_contradicts_its_own_peaks_is_rejected() {
    let thresholds = oc_testkit::perf::FrozenThresholds::from_baseline(
        &oc_testkit::perf::load_committed_baseline()
            .expect("the committed TypeScript baseline must load"),
    )
    .expect("the frozen thresholds must substitute from the committed baseline");
    let g1_ceiling = grouped((thresholds.g1_max_mib * 1024.0).round() as u64);
    let g2_ceiling = grouped((thresholds.g2_max_mib * 1024.0).round() as u64);

    // Five W-real peaks an order of magnitude over the ceiling, with prose that
    // nonetheless claims the gate passed.
    let forged = format!(
        "\nRAW PER-REPETITION PEAK RSS (KiB)\n\
         \n  Rust W-idle   20,060  20,356  20,380  20,444  20,504  20,380\n\
         \n  Rust W-real   90,000,000  90,000,000  90,000,000  90,000,000  90,000,000  \
         90,000,000\n\
         \nG1 / W-idle\n\
         \u{20}\u{20}Rust median: 20,380 KiB\n\
         \u{20}\u{20}committed TypeScript median: 954,240 KiB\n\
         \u{20}\u{20}frozen ceiling (0.50 x committed): {g1_ceiling} KiB\n\
         \u{20}\u{20}verdict under the frozen formula: PASS\n\
         \nG2 / W-real\n\
         \u{20}\u{20}Rust median: 90,000,000 KiB\n\
         \u{20}\u{20}committed TypeScript median: 3,026,992 KiB\n\
         \u{20}\u{20}frozen ceiling (0.50 x committed): {g2_ceiling} KiB\n\
         \u{20}\u{20}verdict under the frozen formula: PASS\n\n"
    );

    let _ = parse_gate_figures(&forged, &thresholds);
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
            // the four caveats, each as a claim a reader can check. The G1/G2
            // margin and spread are deliberately absent from this list: they are
            // measured values, so they are checked by
            // `readme_publishes_the_newest_measured_memory_figures_not_a_remembered_one`
            // against the artefact instead of pinned to a literal here.
            "does not mean G1-G6 pass",
            "ses_2bcaee257ffeFZNJrmtpi3ZglR",
            // G6's unexecuted Windows half, named by the file that would run it.
            "windows_containment.rs",
            "NOT EXECUTED",
        ],
    );
}

/// Success criterion 15's narrowing: G6's Windows half, disclosed in both places.
///
/// The narrowing accepts a Linux-only G6 execution *provided* the Windows half is
/// implemented in source behind a `cfg(windows)` test and the unexecuted state is
/// stated in `README.md` **and** in the evidence. That makes it a disclosure
/// requirement rather than a waiver, so all three halves are asserted here:
/// the source gate really is `#![cfg(windows)]` (a test that silently started
/// running everywhere would make the disclosure false), the README says so, and at
/// least one committed evidence artefact says so too. Deleting the sentence from
/// either document fails this test.
#[test]
fn criterion_15_states_the_windows_g6_half_is_not_executed_in_the_readme_and_the_evidence() {
    const WINDOWS_TEST: &str = "crates/oc-process/tests/windows_containment.rs";
    const LINUX_TEST: &str = "crates/oc-process/tests/containment.rs";
    const DISCLOSURE: &str = "NOT EXECUTED";

    let root = workspace_root();
    for (relative, gate) in [
        (WINDOWS_TEST, "#![cfg(windows)]"),
        (LINUX_TEST, "#![cfg(target_os = \"linux\")]"),
    ] {
        let source = std::fs::read_to_string(root.join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            source.starts_with(gate),
            "{relative} must open with `{gate}`. The narrowing's honesty rests on which host each \
             half can run on; a changed gate makes the README's disclosure a false statement."
        );
    }

    contains_all(
        "README.md",
        &[
            WINDOWS_TEST.rsplit('/').next().expect("the test file name"),
            DISCLOSURE,
            "PASS on Linux; Windows half unexecuted",
        ],
    );

    let dir = root.join(EVIDENCE_DIR);
    let mut disclosing = Vec::new();
    for entry in
        std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let path = entry.expect("an evidence entry must be readable").path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(EVIDENCE_PREFIX) || !name.ends_with(EVIDENCE_SUFFIX) {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        if text.contains("windows_containment.rs") && text.contains(DISCLOSURE) {
            disclosing.push(name.to_owned());
        }
    }
    assert!(
        !disclosing.is_empty(),
        "no artefact in {} states that {WINDOWS_TEST} is {DISCLOSURE}. Criterion 15's narrowing \
         requires the unexecuted state in the evidence as well as the README, so that nobody \
         reads a Linux-only G6 result as a cross-platform one.",
        dir.display()
    );
    eprintln!(
        "criterion 15: the Windows G6 half is disclosed as {DISCLOSURE} in README.md and in {} \
         evidence artefact(s): {disclosing:?}",
        disclosing.len()
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
