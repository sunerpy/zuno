//! The documentation gate: regions delimited by `generated:BEGIN` and
//! `generated:END` are rendered from the code they describe. This target also
//! derives focused assertions for a small set of load-bearing prose claims.
//!
//! # Why a test that reads Markdown is not the vacuous test it looks like
//!
//! The obvious way to satisfy "document every divergence" is to hand-write a
//! table and add a test that reads that same table. Such a test proves nothing:
//! both sides are the same artifact, so it passes for any content, including
//! content that contradicts the code.
//!
//! Every generated-region assertion therefore derives its *expected* side from a
//! live code artifact and its *actual* side from the committed Markdown:
//!
//! | doc block | derived from |
//! |---|---|
//! | `divergence-index`, `divergence-detail` | [`zuno_testkit::DivergenceList`] over `docs/divergences.toml`, cross-checked against [`zuno_testkit::divergence::DECLARED_COUNT`] |
//! | `cli-disposition` | [`zuno_cli::dispositions`] — the same table `zuno-cli/tests/surface.rs` asserts against the registered `clap` tree |
//! | `api-operations` | the served document from [`zuno_server::api::openapi`] set-differenced against the committed 1.18.18 oracle capture, then **probed route by route** for an explicit `503 backend_unavailable` gap; any `501` fails the gate |
//! | `known-gaps` | [`zuno_testkit::compat_report::known_gaps`] — the same list the compatibility report writes, rendered with the API counts probed above |
//! | `v1-routes` | [`zuno_server::V1_SURFACE`] |
//! | `rejected-inputs` | messages *rendered by* [`zuno_config::legacy`]'s detectors, so a reworded message fails |
//! | `plugin-hooks` | [`zuno_plugin::hook_support`] |
//! | `prune-tables` | [`zuno_db::prune::PRUNE_TABLES`] and [`zuno_db::prune::DELETE_ORDER`] |
//! | `migration-journal` | [`zuno_db::migration::MIGRATION_IDS`] and [`zuno_db::migration::CURRENT_VERSION`] |
//! | `cross-session-memory` | [`zuno_config::schema::ResolvedMemoryConfig`], [`zuno_memory::Scope`] and [`zuno_agent::reflection::NEGATIVE_LEARNING_LIST`] |
//!
//! So adding a divergence entry, registering a command, renaming a rejection
//! message, serving a new `/api` operation, or adding a migration all fail here
//! until the documentation is updated.
//!
//! # Regeneration
//!
//! `ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs` rewrites each block
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
use clap::CommandFactory as _;
use tower::ServiceExt as _;
use zuno_cli::{Cli, Disposition, dispositions};
use zuno_server::api::{self, ApiState};
use zuno_server::{ServerBuilder, ServerConfig, V1_SURFACE};
use zuno_testkit::{DivergenceList, divergence};

/// The committed capture of the pinned 1.18.18 release's OpenAPI document.
///
/// Also read by `crates/zuno-server/tests/compat_v1.rs`, so the documentation and
/// the compatibility assertions cannot disagree about what upstream declares.
const ORACLE_OPENAPI_FIXTURE: &str = ".omo/fixtures/oracle-openapi-1.18.18.json";

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
        .expect("crates/zuno-cli has a workspace root two levels up")
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
        std::env::var("ZUNO_DOCS_REGENERATE").as_deref(),
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
         \n    ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs\n\
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

/// Asserts no needle appears in the document, with the reason in the message.
///
/// The counterpart to [`contains_all`], and the only form that can catch a
/// *wrong* claim rather than a missing one. A positive needle alone cannot: it
/// still passes when the correct sentence is added next to the incorrect one it
/// was meant to replace, which is how an invocation the binary rejects survived
/// in both READMEs while the gate stayed green.
fn contains_none_in(text: &str, label: &str, forbidden: &[(String, String)]) {
    for (needle, reason) in forbidden {
        assert!(
            !text.contains(needle.as_str()),
            "{label} must not mention {needle:?}: {reason}"
        );
    }
}

/// The `zuno session <name>` spellings the documentation must never use, derived
/// from the registered `clap` tree rather than typed.
///
/// Both halves of the session round trip are top-level commands. Deriving that
/// here, instead of hard-coding the forbidden strings, means a future move of
/// `export`/`import` under `session` fails *this* function loudly rather than
/// silently inverting the documentation gate into forbidding the only invocation
/// that works.
fn rejected_round_trip_spellings() -> Vec<(String, String)> {
    let root = Cli::command();
    let session = root
        .get_subcommands()
        .find(|subcommand| subcommand.get_name() == "session")
        .expect("`session` is a registered command");
    let session_children: Vec<&str> = session
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect();

    ["export", "import"]
        .into_iter()
        .map(|name| {
            assert!(
                root.get_subcommands()
                    .any(|subcommand| subcommand.get_name() == name),
                "`zuno {name}` must be a top-level command, because both READMEs document it as \
                 one"
            );
            assert!(
                !session_children.contains(&name),
                "`zuno session {name}` is now registered, so forbidding it in the READMEs would \
                 forbid a working invocation; update the documentation first. Registered `session` \
                 subcommands: {session_children:?}"
            );
            (
                format!("zuno session {name}"),
                format!(
                    "`zuno session` carries only {session_children:?}, so this invocation exits \
                     with `unrecognized subcommand '{name}'`; the working command is the \
                     top-level `zuno {name}`"
                ),
            )
        })
        .collect()
}

/// Collapses every run of whitespace to one space.
///
/// These documents are hard-wrapped, so a prose needle longer than a few words
/// straddles a line break and would otherwise be pinned to one wrap position — a
/// reflow that changed no claim would fail the gate, and the failure would read
/// as a broken edit. Matching the collapsed form pins the sentence, not its
/// layout.
fn unwrapped(relative: &str) -> String {
    let path = workspace_root().join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn section(relative: &str, heading: &str) -> String {
    let path = workspace_root().join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let start = text
        .find(heading)
        .unwrap_or_else(|| panic!("{} must contain section {heading:?}", path.display()));
    let body_start = start + heading.len();
    let end = text[body_start..]
        .find("\n## ")
        .map_or(text.len(), |offset| body_start + offset);
    text[start..end].to_owned()
}

fn contains_all_in(text: &str, label: &str, needles: &[&str]) {
    for needle in needles {
        assert!(text.contains(needle), "{label} must mention {needle:?}");
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
        "the allow-list declares {} entries but zuno_testkit::divergence::DECLARED_COUNT expects \
         {}; the documentation gate refuses to render a list the compatibility gate rejects",
        list.len(),
        divergence::DECLARED_COUNT
    );
    assert!(
        !list.is_empty(),
        "an empty allow-list would make every documentation assertion below vacuous"
    );
    for (id, expected) in [
        (
            "tool-output-filename-carries-session",
            format!(
                "on-disk `$XDG_DATA_HOME/{}/tool-output/tool_<session>_<uuidv7>`",
                zuno_paths::APP
            ),
        ),
        (
            "malformed-auth-json-is-an-error",
            format!(
                "`$XDG_DATA_HOME/{}/auth.json` — reading the credential store",
                zuno_paths::APP
            ),
        ),
    ] {
        let surface = list
            .entries()
            .iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("the path-bearing divergence {id} must exist"));
        assert_eq!(
            surface.surface, expected,
            "the {id} documentation must derive Zuno's data root from zuno_paths::APP"
        );
    }
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
    use zuno_agent::reflection::NEGATIVE_LEARNING_LIST;
    use zuno_config::schema::ResolvedMemoryConfig;
    use zuno_memory::Scope;

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
| `memory.project_char_limit` | `{project}` | cap `<worktree>/{project_directory}/RULES.md` in Unicode scalar values |\n\
| `memory.nudge_interval` | `{interval}` | periodic reflection cadence in delivered turns; `0` disables only that trigger |\n\n\
Reflection must not learn any of these negative cases:\n",
        global = defaults.global_char_limit,
        project = defaults.project_char_limit,
        resident = defaults.resident,
        tool = defaults.tool,
        reflection = defaults.reflection,
        interval = defaults.nudge_interval,
        project_directory = zuno_paths::PROJECT_CONFIG_DIRECTORY,
    );
    for exclusion in NEGATIVE_LEARNING_LIST {
        let _ = writeln!(out, "- {exclusion}");
    }
    out
}

#[test]
fn docs_every_declared_divergence_is_documented_with_its_reason() {
    let list = allow_list();

    // The Chinese root README names this count in prose. Deriving the number from
    // the live list means the next entry cannot leave the README stale, which is
    // the whole reason the other README figures are probed rather than retyped.
    contains_all("README.md", &[&format!("{} 项有意差异", list.len())]);

    // The divergence page's own headline. It said "Thirteen" for two review waves
    // after the allow-list reached seventeen, on the one page that calls itself the
    // single declaration point -- so the count a reader sees first is derived here
    // too. Capitalised because it opens the sentence.
    let headline = spell(list.len());
    let capitalised = format!(
        "{}{} deliberate differences",
        headline[..1].to_uppercase(),
        &headline[1..]
    );
    contains_all("docs/divergences.md", &[&capitalised]);

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
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory event database"),
    );
    let events = zuno_server::EventService::new(pool, 8);
    let state = ApiState::memory("/repo")
        .expect("in-memory API state")
        .with_events(events.clone());
    let app: Router = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(api::router(state).merge(zuno_server::events_router(events)))
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
    let v1 = zuno_server::compat_v1::v1_coverage();
    let v1 =
        zuno_testkit::compat_report::V1SurfaceCoverage::new(v1.measured, v1.served, v1.redirected);
    let known_gaps = zuno_testkit::compat_report::known_gaps(
        gaps.len(),
        upstream.len(),
        v1,
        zuno_server::api::openapi_body_schema_gaps(),
    );
    let channel_gap = known_gaps
        .iter()
        .find(|gap| gap.id == "channel-dependent-database-filename")
        .expect("the channel-dependent database gap must remain documented");
    assert_eq!(
        channel_gap.surface,
        format!(
            "$XDG_DATA_HOME/{app}/{app}-<channel>.db",
            app = zuno_paths::APP
        ),
        "the channel database documentation must derive Zuno's data root from zuno_paths::APP"
    );
    for (index, gap) in known_gaps.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "### {}\n", gap.id);
        let _ = writeln!(out, "**Surface.** {}\n", gap.surface);
        let _ = writeln!(out, "**What is missing.** {}", gap.detail);
    }
    out
}

/// Every `/api` route a v1 `501` sends a caller to must actually work.
///
/// The point of the redirect is that the caller has somewhere to go. An alternative
/// that is unregistered, or that is itself an explicit `503` gap, would send a
/// plugin author from one dead end to another — a more expensive version of the
/// stale "lands in todos 57-62" hint this replaced. Checked against the probed
/// router, so an `/api` route losing its backend fails here too.
fn assert_v1_alternatives_are_really_served(
    served: &BTreeSet<(String, String)>,
    gaps: &BTreeSet<(String, String)>,
) {
    let coverage = zuno_server::v1_coverage();
    let v1_section = section(
        "docs/compatibility-matrix.md",
        "## v1 plugin compatibility routes",
    );
    contains_all_in(
        &v1_section,
        "docs/compatibility-matrix.md v1 plugin compatibility routes section",
        &[
            &format!(
                "{} of the {} answer `501 not_implemented`",
                coverage.unbacked, coverage.measured
            ),
            &format!(
                "{} of those {} name a served",
                coverage.redirected, coverage.unbacked
            ),
            &format!(
                "the other {} have no served",
                coverage.unbacked - coverage.redirected
            ),
        ],
    );
    for route in V1_SURFACE {
        if route.backing.is_served() {
            continue;
        }
        let Some(alternative) = route.api_alternative else {
            continue;
        };
        let (method, path) = alternative
            .split_once(' ')
            .unwrap_or_else(|| panic!("`{alternative}` is not a `VERB /path` pair"));
        let key = (path.to_owned(), method.to_lowercase());
        assert!(
            served.contains(&key),
            "{} {} points callers at `{alternative}`, which this server does not serve",
            route.method,
            route.path
        );
        assert!(
            !gaps.contains(&key),
            "{} {} points callers at `{alternative}`, which is itself an explicit 503 gap; a \
             redirect to a second dead end is worse than none",
            route.method,
            route.path
        );
    }
}

fn v1_summary_block() -> String {
    let coverage = zuno_server::v1_coverage();
    format!(
        "**{measured} v1 routes** are registered from measured installed-plugin callsites. \
A route with no recorded callsite is scope creep, and a test fails on it.\n\n\
Registering a route is not the same as backing it: **{served} of the {measured} do real local \
work**, while **{unbacked} of the {measured} answer `501 not_implemented`**. {redirected} of \
those {unbacked} name a served `/api` alternative; the other {without_alternative} have no served \
`/api` spelling here. The generated route table below names every backing. The installed auth \
plugins' `auth.set` and provider OAuth routes are served; the remaining gaps are non-authentication \
operations. These figures come from `zuno_server::v1_coverage()`, which counts the same route and \
backend tables the server mounts.",
        measured = coverage.measured,
        served = coverage.served,
        unbacked = coverage.unbacked,
        redirected = coverage.redirected,
        without_alternative = coverage.unbacked - coverage.redirected,
    )
}

fn v1_capture_coverage_block() -> String {
    let coverage = zuno_server::v1_coverage();
    let adapters = V1_SURFACE
        .iter()
        .filter(|route| matches!(route.backing, zuno_server::V1Backing::ApiAdapter(_)))
        .count();
    let auth = V1_SURFACE
        .iter()
        .filter(|route| {
            matches!(
                route.backing,
                zuno_server::V1Backing::LocalAuthStore | zuno_server::V1Backing::LocalProviderOAuth
            )
        })
        .count();
    let toast = V1_SURFACE
        .iter()
        .filter(|route| matches!(route.backing, zuno_server::V1Backing::LocalToastSink))
        .count();
    let unbacked_methods = V1_SURFACE
        .iter()
        .filter(|route| !route.backing.is_served())
        .map(|route| format!("`{}`", route.sdk_method))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(adapters + auth + toast, coverage.served);

    format!(
        "Current backend coverage is **{served} of {measured} measured routes served locally** and \
**{unbacked} of {measured} registered as structured `501 not_implemented` seams**. The served set \
contains {adapters} `/api` adapters, {auth} credential/OAuth routes, and {toast} toast recording \
sink.\n\n\
Of the {unbacked} unbacked routes, {redirected} name a served `/api` alternative and \
{without_alternative} do not. The unbacked SDK methods are {unbacked_methods}. These counts are \
generated from `zuno_server::v1_coverage()` and the same `V1_SURFACE` backing declarations the \
server mounts.\n\n\
The practical consequence: the installed auth plugins can authenticate because `auth.set` and \
both provider OAuth routes have credential backends. Toasts reach the bounded recording sink. A \
plugin that needs one of the unbacked methods receives a definitive `501` rather than fabricated \
success data.",
        measured = coverage.measured,
        served = coverage.served,
        unbacked = coverage.unbacked,
        adapters = adapters,
        auth = auth,
        toast = toast,
        redirected = coverage.redirected,
        without_alternative = coverage.unbacked - coverage.redirected,
    )
}

/// Renders the v1 route table with the status each route really has.
///
/// The `backing` and `/api alternative` columns come off [`V1_SURFACE`] itself,
/// which `crates/zuno-server/tests/compat_v1.rs` asserts against the routes the
/// server answers. Before the tenth review wave this table listed 20 routes with no
/// hint that unbacked routes return `501`, so a reader consulting the matrix could
/// not learn which calls were usable.
fn v1_block() -> String {
    let mut out = String::from(
        "| method | path | SDK method | backing | `/api` alternative |\n|---|---|---|---|---|\n",
    );
    for route in V1_SURFACE {
        let _ = writeln!(
            out,
            "| {} | `{}` | `{}` | {} | {} |",
            route.method.as_str(),
            route.path,
            route.sdk_method,
            route.backing.as_str(),
            route
                .api_alternative
                .map_or_else(|| "none served here".to_owned(), |path| format!("`{path}`")),
        );
    }
    out
}

/// Every table on the matrix page, checked by one test.
///
/// One test per file is not a stylistic choice: [`check_block`] rewrites the
/// whole file under `ZUNO_DOCS_REGENERATE`, so two tests regenerating two blocks of
/// the same page race and each silently discards the other's write.
#[test]
fn docs_compatibility_matrix_matches_every_code_table() {
    const PAGE: &str = "docs/compatibility-matrix.md";

    let list = allow_list();
    check_block(PAGE, "divergence-index", &divergence_index(&list));
    check_block(PAGE, "cross-session-memory", &cross_session_memory_block());
    check_block(PAGE, "cli-disposition", &cli_disposition_block());

    let upstream = oracle_api_operations();
    let served = api_operations(&zuno_server::api::openapi());
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
    check_block(PAGE, "v1-summary", &v1_summary_block());
    check_block(PAGE, "v1-routes", &v1_block());
    check_block(
        "docs/v1-surface-capture.md",
        "v1-capture-coverage",
        &v1_capture_coverage_block(),
    );

    assert_cli_disposition_counts();
    assert_api_counts(&upstream, &served, &gaps);
    assert_v1_alternatives_are_really_served(&served, &gaps);
    contains_all(PAGE, &[&format!("{} v1 route", V1_SURFACE.len())]);
    contains_all(PAGE, &zuno_agent::reflection::NEGATIVE_LEARNING_LIST);
}

// ---------------------------------------------------------------------------
// Rejected inputs
// ---------------------------------------------------------------------------

/// One documented rejection, with the message its detector actually renders.
struct Rejection {
    form: zuno_config::DeprecatedForm,
    found: String,
    replacement: String,
    message: String,
}

/// Renders every deprecated form through the real detectors.
///
/// Nothing here is transcribed: the `found`, `replacement`, and `message` all
/// come off the [`zuno_config::Deprecation`] the production code produced, so a
/// reworded replacement fails the documentation gate.
fn rejections() -> Vec<Rejection> {
    use zuno_config::legacy::{
        ConfigFileScope, inspect_agent_frontmatter, inspect_auth, inspect_config,
        inspect_config_filename, inspect_global_directory,
    };

    let config = Path::new("/example/zuno.json");
    let auth = Path::new("/example/auth.json");
    let agent = Path::new("/example/agent/build.md");

    let mut found: Vec<zuno_config::Deprecation> = inspect_config(
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
    found.extend(zuno_config::legacy::auth_prompt_deprecation(
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
    // Both scopes of the filename form: they carry different replacements, because
    // only the project walk can be fixed by switching project config off. A page
    // showing one would leave the other's message undocumented.
    std::fs::write(dir.path().join("opencode.json"), "{}").expect("write legacy config");
    std::fs::write(dir.path().join("opencode.jsonc"), "{}").expect("write legacy JSONC");
    for scope in [ConfigFileScope::Owned, ConfigFileScope::ProjectAncestor] {
        for name in ["opencode.json", "opencode.jsonc"] {
            found.extend(inspect_config_filename(&dir.path().join(name), scope));
        }
    }

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
        11,
        "all eleven deprecated forms must be reachable for the page to be complete; got {forms:?}"
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
            "docs/rejected-inputs.md does not contain the exact message zuno-config renders for \
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
    let support = zuno_plugin::hook_support();
    let mut out = String::from(
        "| hook | JavaScript / JSON-RPC name | production trigger |\n|---:|---|---|\n",
    );
    for (index, entry) in support.enumerate() {
        let _ = writeln!(
            out,
            "| {} | `{}` | {} |",
            index + 1,
            entry.hook.as_str(),
            cell(entry.production_trigger)
        );
    }
    out
}

fn plugin_config_paths_block() -> String {
    format!(
        "Beyond the config array, Zuno scans every configuration directory for \
`plugin/*.{{ts,js}}` and `plugins/*.{{ts,js}}`. The directory chain is \
`$XDG_CONFIG_HOME/{app}`, project `{project}` directories, `$HOME/{project}`, then \
`OPENCODE_CONFIG_DIR`; files are sorted within `plugin/` and then `plugins/`. \
`OPENCODE_CONFIG_DIR` deliberately keeps its upstream spelling because installed npm plugins \
consume it as one of the six retained plugin-ABI environment names. Provenance is retained \
(`zuno_plugin::PluginOrigin`), successful discovery is visible at `DEBUG`, and scan or load \
failures are warnings that name the affected directory or plugin.",
        app = zuno_paths::APP,
        project = zuno_paths::PROJECT_CONFIG_DIRECTORY,
    )
}

fn tool_source_precedence_block() -> String {
    use zuno_tools::registry::TOOL_SOURCE_PRECEDENCE;

    let mut out = String::from(
        "The registry assembles sources in increasing precedence order. If tool ids collide, the \
later source replaces the earlier implementation in its existing provider-visible position and \
emits a suppression diagnostic naming both sources.\n\n\
| order | source |\n|---:|---|\n",
    );
    for (index, source) in TOOL_SOURCE_PRECEDENCE.iter().enumerate() {
        let _ = writeln!(out, "| {} | `{source}` |", index + 1);
    }
    let winners = TOOL_SOURCE_PRECEDENCE
        .iter()
        .rev()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" > ");
    let _ = write!(out, "\nHighest-to-lowest winner precedence: `{winners}`.");
    out
}

#[test]
fn docs_plugin_guide_matches_the_hooks_and_the_example_the_host_ships() {
    check_block(
        "docs/plugin-authoring.md",
        "plugin-config-paths",
        &plugin_config_paths_block(),
    );
    check_block(
        "docs/plugin-authoring.md",
        "plugin-hooks",
        &plugin_hook_block(),
    );
    check_block(
        "docs/plugin-authoring.md",
        "tool-source-precedence",
        &tool_source_precedence_block(),
    );
    contains_all(
        "docs/plugin-authoring.md",
        &[&format!("{} hooks", zuno_plugin::hook_support().len())],
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
        &["examples/rust_plugin.rs", "zuno_plugin_sdk"],
    );
    for symbol in ["Plugin::new", "ToolDefinition::new", "ConformanceSuite"] {
        assert!(
            source.contains(symbol),
            "examples/rust_plugin.rs no longer uses {symbol}, which the guide documents"
        );
    }

    // Every example the guide offers must exist. A guide that names a file nobody
    // shipped is the same defect as an API nobody can reach, one layer up.
    for relative in ["examples/go_plugin/main.go", "examples/js_plugin"] {
        let path = workspace_root().join(relative);
        assert!(
            path.is_file(),
            "the guide points at {}, which must exist",
            path.display()
        );
        contains_all("docs/plugin-authoring.md", &[relative]);
    }

    // The guide must keep saying how a process plugin is installed, and must keep
    // refusing to promise the four capabilities this tier does not have. Both are
    // load-bearing: the first is the only way in, and the second is what stops an
    // author building against `auth` or an interactive prompt that cannot work.
    contains_all_in(
        &section(
            "docs/plugin-authoring.md",
            "## Tier 3 — Out-of-process, over JSON-RPC",
        ),
        "the out-of-process tier section",
        &[
            "plugin/",
            "executable bit",
            "\"process\": false",
            "--pure",
            "the `auth` hook",
            "the `provider` hook",
            "interactive flows",
            "sub-turn orchestration",
            "publish = false",
        ],
    );
}

// ---------------------------------------------------------------------------
// Session retention (C8) and migrations
// ---------------------------------------------------------------------------

fn prune_table_block() -> String {
    let mut out = String::from("| order | table |\n|---:|---|\n");
    for (index, table) in zuno_db::prune::DELETE_ORDER.iter().enumerate() {
        let _ = writeln!(out, "| {} | `{table}` |", index + 1);
    }
    out
}

#[test]
fn docs_retention_guide_lists_the_tables_delete_actually_touches() {
    assert_eq!(
        zuno_db::prune::DELETE_ORDER.len(),
        zuno_db::prune::PRUNE_TABLES.len(),
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
            &format!("{} tables", zuno_db::prune::DELETE_ORDER.len()),
        ],
    );
}

fn migration_block() -> String {
    let mut out = String::from("| # | migration id |\n|---:|---|\n");
    for (index, id) in zuno_db::migration::MIGRATION_IDS.iter().enumerate() {
        let _ = writeln!(out, "| {} | `{id}` |", index + 1);
    }
    out
}

#[test]
fn docs_migration_guide_lists_every_migration_in_execution_order() {
    assert_eq!(
        u32::try_from(zuno_db::migration::MIGRATION_IDS.len()).expect("the id count fits in u32"),
        zuno_db::migration::CURRENT_VERSION,
        "CURRENT_VERSION must equal the number of migration ids"
    );
    check_block("docs/migration.md", "migration-journal", &migration_block());
    contains_all(
        "docs/migration.md",
        &[
            zuno_db::migration::DRIZZLE_JOURNAL_TABLE,
            &format!("{} migrations", zuno_db::migration::MIGRATION_IDS.len()),
            "ZUNO_DB",
            "ZUNO_DISABLE_CHANNEL_DB",
            zuno_paths::DEFAULT_DB_FILE,
            &format!("{}-local.db", zuno_paths::APP),
            zuno_paths::LEGACY_DB_FILE,
            "does not import an opencode database",
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
//   * `benchmarks/ts-baseline.json`, via [`zuno_testkit::perf::FrozenThresholds`],
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

/// The page that owns the resource-gate measurements.
///
/// These figures used to sit in `README.md`. They are a measurement record, not a
/// project introduction, so the README links here instead. Every assertion below
/// that used to name the README names this constant, at the same strength.
const RESOURCE_GATES_PAGE: &str = "docs/resource-gates.md";

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
    /// `0.50 x` that median, taken from [`zuno_testkit::perf::FrozenThresholds`].
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
    thresholds: &zuno_testkit::perf::FrozenThresholds,
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
    let thresholds = zuno_testkit::perf::FrozenThresholds::from_baseline(
        &zuno_testkit::perf::load_committed_baseline()
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
    link_prefix: &str,
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
            " The superseded measurement in [`{path}`]({link_prefix}{path}) is the shape being \
             avoided: a {} \
             KiB spread around a median that finished {} KiB over the same ceiling — {}.",
            grouped(previous.g2.spread_kib()),
            grouped(previous.g2.excess_kib().unwrap_or_default()),
            previous.g2.verdict(),
            path = previous.artefact,
        ),
        Some(previous) => format!(
            " The superseded measurement in [`{path}`]({link_prefix}{path}) recorded a {} KiB \
             median against a \
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

/// Renders the memory-gate block from a measurement.
///
/// `link_prefix` is prepended to every repository-root-relative link the block
/// emits, so the block renders correctly from whichever depth the page that hosts
/// it sits at. It lives one directory down (`docs/resource-gates.md`), so the
/// prefix is `../`; a hard-coded root-relative link would resolve to nothing
/// there, and a broken link is exactly the failure this block's citations exist
/// to prevent.
fn memory_gate_block(
    current: &MemoryGateMeasurement,
    superseded: Option<&MemoryGateMeasurement>,
    link_prefix: &str,
) -> String {
    let artefact = &current.artefact;
    let mut out = wrap(
        &format!(
            "Derived from the newest committed measurement artefact, \
             [`{artefact}`]({link_prefix}{artefact}). The ceilings are not measured here: \
             [`benchmarks/ts-baseline.json`]({link_prefix}benchmarks/ts-baseline.json) freezes \
             each one at half the TypeScript median for the same workload, and every other \
             column below is computed from the five per-repetition Rust peaks the artefact \
             records."
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
    out.push_str(&g2_robustness_prose(current, superseded, link_prefix));
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

    // The block is derived end to end, so a stale digit on the page fails here.
    check_block(
        RESOURCE_GATES_PAGE,
        "memory-gate-measurement",
        &memory_gate_block(&current, superseded.as_ref(), "../"),
    );

    // Both cited artefacts must actually be present, so the page cannot point a
    // reader at a measurement that was never committed.
    for measurement in std::iter::once(&current).chain(superseded.as_ref()) {
        let path = workspace_root().join(&measurement.artefact);
        assert!(
            path.is_file(),
            "{} is cited by the memory block but absent",
            path.display()
        );
    }

    // Every link the block emits has to resolve from the page that hosts it. The
    // block is one directory deep, so its `../`-prefixed targets are checked
    // against the real filesystem here rather than trusted; that is the only way
    // a wrong prefix fails instead of shipping dead citations.
    let page = workspace_root().join(RESOURCE_GATES_PAGE);
    let base = page.parent().expect("the page sits inside docs/");
    let text = std::fs::read_to_string(&page).expect("read the resource-gates page");
    let mut checked = 0usize;
    for target in text
        .split("](")
        .skip(1)
        .filter_map(|rest| rest.split_once(')'))
        .map(|(target, _)| target)
        .filter(|target| target.starts_with("../"))
    {
        assert!(
            base.join(target).exists(),
            "{RESOURCE_GATES_PAGE} links to `{target}`, which does not resolve from {}",
            base.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 3,
        "the memory block cites two evidence artefacts and the frozen baseline, so at least three \
         `../` links must have been checked; found {checked}"
    );
}

/// The derived side has to dominate the artefact's prose, not defer to it.
///
/// Without this, `parse_gate_figures` could quietly trust a narrative that says
/// `PASS` while its own peaks say otherwise — the exact failure mode of a test
/// whose expected and actual sides come from the same hand-written source.
#[test]
#[should_panic(expected = "records `PASS` but its own median")]
fn a_measurement_whose_prose_contradicts_its_own_peaks_is_rejected() {
    let thresholds = zuno_testkit::perf::FrozenThresholds::from_baseline(
        &zuno_testkit::perf::load_committed_baseline()
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

/// The name the plugin SDK crate publishes under, read from its manifest.
///
/// Both READMEs steer plugin authors at this crate, so a rename has to fail the
/// documentation gate rather than leave two files naming a crate that no longer
/// exists. Parsed from the manifest instead of restated here for that reason.
fn plugin_sdk_crate_name() -> String {
    const MANIFEST: &str = "crates/zuno-plugin-sdk/Cargo.toml";
    let path = workspace_root().join(MANIFEST);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let name = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("name = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("{MANIFEST} must declare a package name"));
    assert!(
        !name.is_empty(),
        "{MANIFEST} declares an empty package name, which would make the README assertion vacuous"
    );
    name.to_owned()
}

/// Both READMEs must recommend the Rust SDK, and neither may explain a pinned
/// compatibility version.
///
/// This replaces an assertion that required the READMEs to publish
/// `COMPATIBILITY_VERSION` and explain why `zuno --version` reports it. That
/// version is frozen plugin ABI and stays in the code; the explanation is
/// implementation detail that told a reader to reason about opencode releases
/// instead of about Zuno. So the positive half now pins what the project actually
/// offers a plugin author — opencode plugins load, and the first-party Rust SDK is
/// the recommended path — and the negative half forbids the removed prose from
/// creeping back, with the forbidden string derived from the constant rather than
/// typed. It cannot pass with the old text restored: the old text contained
/// `COMPATIBILITY_VERSION`'s value, which the negative half now rejects.
#[test]
fn both_readmes_recommend_the_rust_plugin_sdk_without_explaining_a_pinned_version() {
    let sdk = plugin_sdk_crate_name();
    let forbidden = vec![(
        zuno_cli::COMPATIBILITY_VERSION.to_owned(),
        "the pinned plugin-compatibility version is frozen ABI in \
         `crates/zuno-cli/src/version.rs`, not a fact a reader of the introduction has to hold; \
         the READMEs state that opencode plugins are supported and recommend the Rust SDK instead"
            .to_owned(),
    )];

    for (relative, needles) in [
        (
            "README.md",
            [
                "支持 opencode 插件",
                "推荐使用 Rust",
                "docs/plugin-authoring.md",
            ],
        ),
        (
            "docs/readme/README.en.md",
            [
                "supports opencode plugins",
                "Rust plugins are the recommended",
                "plugin-authoring.md",
            ],
        ),
    ] {
        let prose = unwrapped(relative);
        contains_all_in(&prose, relative, &needles);
        contains_none_in(&prose, relative, &forbidden);
        contains_all(relative, &[sdk.as_str()]);
    }
}

#[test]
fn readmes_define_zuno_as_independent_while_retaining_the_plugin_abi() {
    let config_root = format!("$XDG_CONFIG_HOME/{}", zuno_paths::APP);
    let data_root = format!("$XDG_DATA_HOME/{}", zuno_paths::APP);
    // The directory claim held while the *filenames* were still opencode's, and
    // this gate stayed green throughout because nothing pinned a filename. Both
    // live spellings are therefore required, derived from `zuno-paths` so a future
    // rename fails here before it reaches a user.
    //
    // A blanket ban on the retired spellings is deliberately *not* the second
    // half: these READMEs name them on purpose, to say they are no longer read, and
    // a bare-substring ban would force that sentence out. The checkable property is
    // the conditional one — a document may name a retired filename only while also
    // carrying the migration claim, so prose that reverts to *teaching* the old
    // name (and therefore drops the claim) fails.
    let config_files = [
        format!("{}.jsonc", zuno_paths::CONFIG_FILE_STEM),
        format!("{}.json", zuno_paths::CONFIG_FILE_STEM),
    ];
    let retired_config_files: Vec<String> = zuno_paths::LEGACY_GLOBAL_CONFIG_NAMES
        .iter()
        .chain(zuno_paths::LEGACY_CONFIG_NAMES.iter())
        .map(|name| format!("`{name}`"))
        .collect();
    let plugin_abi = zuno_paths::env::PLUGIN_ABI_ENV_NAMES.to_vec();
    let rejected = rejected_round_trip_spellings();
    // Each document must carry all four claims, because any subset reads as a
    // different one. A bound without the command names is the stale wording that
    // let a reader conclude Zuno cannot import anything at all; the command names
    // without both bounds would advertise adopting an opencode session or a share
    // URL. And a positive needle alone cannot catch a *wrong* invocation, only a
    // missing one — `zuno session import` sat in both files, naming a subcommand
    // `zuno session` has never carried, while this gate stayed green. So the
    // spellings come from the registered `clap` tree and the rejected ones are
    // forbidden outright.
    for (relative, independence, migration_claim) in [
        (
            "README.md",
            [
                "不接受 opencode 会话",
                "也不接受 share URL",
                "`zuno export` 与 `zuno import` 构成",
                "`zuno import` 只读取 Zuno 自己 `zuno export` 出的文档",
            ],
            "都不再被读取",
        ),
        (
            "docs/readme/README.en.md",
            [
                "never an opencode session",
                "never a share URL",
                "`zuno export` and `zuno import` close Zuno's own round trip",
                "`zuno import` reads Zuno's own `zuno export` documents only",
            ],
            "no longer read",
        ),
    ] {
        let prose = unwrapped(relative);
        contains_all_in(&prose, relative, &independence);
        contains_none_in(&prose, relative, &rejected);
        contains_all(
            relative,
            &[
                &config_root,
                &data_root,
                zuno_paths::PROJECT_CONFIG_DIRECTORY,
            ],
        );
        let names: Vec<&str> = config_files.iter().map(String::as_str).collect();
        contains_all_in(&prose, relative, &names);
        for retired in &retired_config_files {
            if prose.contains(retired.as_str()) {
                assert!(
                    prose.contains(migration_claim),
                    "{relative} names the retired config filename {retired} without stating that \
                     it is no longer read; a reader would write that file and be rejected at \
                     startup with no idea the name changed"
                );
            }
        }
        contains_all(relative, &plugin_abi);
    }
}

#[test]
fn the_gate_page_reports_every_non_functional_gate_with_its_opt_in_command() {
    // The README must keep pointing readers here, or moving the measurements out
    // of it would have quietly deleted them from the documentation.
    contains_all("README.md", &[RESOURCE_GATES_PAGE]);
    contains_all(
        RESOURCE_GATES_PAGE,
        &[
            "ZUNO_MEMORY_GATE_MODE=run",
            "--ignored",
            "cargo test --workspace",
            // the four caveats, each as a claim a reader can check. The G1/G2
            // margin and spread are deliberately absent from this list: they are
            // measured values, so they are checked by
            // `readme_publishes_the_newest_measured_memory_figures_not_a_remembered_one`
            // against the artefact instead of pinned to a literal here.
            "通过不代表 G1-G6 通过",
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
/// stated on `docs/resource-gates.md` **and** in the evidence. That makes it a
/// disclosure requirement rather than a waiver, so all three halves are asserted
/// here: the source gate really is `#![cfg(windows)]` (a test that silently
/// started running everywhere would make the disclosure false), the gate page says
/// so, and at least one committed evidence artefact says so too. Deleting the
/// sentence from either document fails this test.
#[test]
fn criterion_15_states_the_windows_g6_half_is_not_executed_in_the_readme_and_the_evidence() {
    const WINDOWS_TEST: &str = "crates/zuno-process/tests/windows_containment.rs";
    const LINUX_TEST: &str = "crates/zuno-process/tests/containment.rs";
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
        RESOURCE_GATES_PAGE,
        &[
            WINDOWS_TEST.rsplit('/').next().expect("the test file name"),
            DISCLOSURE,
            "Linux 上 PASS；Windows 部分未执行",
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
         requires the unexecuted state in the evidence as well as on the gate page, so that \
         nobody \
         reads a Linux-only G6 result as a cross-platform one.",
        dir.display()
    );
    eprintln!(
        "criterion 15: the Windows G6 half is disclosed as {DISCLOSURE} on \
         {RESOURCE_GATES_PAGE} and in {} \
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
