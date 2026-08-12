//! The measured-minimum pre-`/api` surface, and accounting for everything else.
//!
//! # Why a pre-`/api` surface exists at all
//!
//! The published SDK does not prefix its requests. `InstanceHttpApi` composes its
//! route groups with no `.prefix("/api")`
//! (`packages/opencode/src/server/routes/instance/httpapi/api.ts:61-76`), and the
//! generated client asks for bare paths such as `/session/{id}/abort` and
//! `/tui/show-toast` (`packages/sdk/js/src/gen/sdk.gen.ts:437,1120`). Every
//! resident plugin talks through that client. Serving only `/api/*` would
//! therefore leave every plugin call unrouted.
//!
//! # Why the surface is measured rather than ported
//!
//! The oracle serves 111 pre-`/api` paths. Porting all of them would be months of
//! work for endpoints nothing here calls. Instead the *installed* plugins were
//! scanned for SDK callsites, and only the routes those callsites reach are
//! served. [`V1_SURFACE`] carries the resulting 20 entries **with their evidence
//! attached**, so the justification for a route cannot drift away from the route:
//! `docs/v1-surface-capture.md` and `tests/compat_v1.rs` both read this table.
//!
//! # Why the wrong measurement is designed for
//!
//! A measured minimum can be measured wrong, and the symptom of a missing route
//! is the worst kind: a plugin awaiting a response that will never come, with
//! nothing in the log. So an unmeasured v1 path is not merely absent — it is
//! *accounted for*. It answers 404 immediately, the body names the path and tells
//! the operator to re-run the capture, and [`UnknownRoutes`] counts it for
//! `GET /compat/v1/diagnostics`. A wrong measurement becomes a number an operator
//! can read instead of a hang they have to guess at.
//!
//! # Why the accounting is one fallback per nested prefix
//!
//! A single `Router::fallback` is global: it would answer for unmatched `/api/*`
//! paths too, claiming the API surface's misses as v1 gaps. A `{*rest}` wildcard
//! per prefix looks like the scoped alternative but does not compile at runtime —
//! `matchit` rejects `/auth/{*rest}` outright as *conflicting* with the already
//! registered `/auth/{providerID}` rather than ordering one above the other, so
//! that shape panics on the first prefix that has a parameterised route.
//!
//! What does work is `Router::nest` with a fallback on the *inner* router: axum
//! grafts that fallback into the outer router **at the nest prefix**
//! (`axum-0.8.9/src/routing/mod.rs:227-229`), so it answers for `/session/<any
//! unmatched>` and stays silent for `/api/...`, `/event`, `/health` and `/doc`.
//! Each prefix in [`V1_PREFIXES`] is nested this way, its measured routes mounted
//! inside with the prefix stripped. A nest at `/foo` also matches bare `/foo`, so
//! prefixes with no measured root route get an explicit bare route to keep the
//! bare path counted rather than falling through to the plain outer 404.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Json;
use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, patch, post, put};
use axum::{Router, routing::MethodRouter};
use serde::Deserialize;
use serde_json::{Value, json};

/// Diagnostics for the v1 surface. Outside [`V1_PREFIXES`], so it shadows nothing.
pub const V1_DIAGNOSTICS_PATH: &str = "/compat/v1/diagnostics";

/// Pre-`/api` top-level segments the accounting catch-all covers.
///
/// The oracle's pre-`/api` surface has 25 distinct top-level segments;
/// `event` is excluded because the SSE stream owns `/event` exactly, and mounting
/// a second route on that path would panic at assembly. A test derives this set
/// from `.omo/fixtures/oracle-openapi-1.18.12.json` and asserts equality, so the
/// list cannot silently drift from the document it claims to mirror.
pub const V1_PREFIXES: &[&str] = &[
    "agent",
    "auth",
    "command",
    "config",
    "experimental",
    "file",
    "find",
    "formatter",
    "global",
    "instance",
    "log",
    "lsp",
    "mcp",
    "path",
    "permission",
    "project",
    "provider",
    "pty",
    "question",
    "session",
    "skill",
    "sync",
    "tui",
    "vcs",
];

/// Distinct unknown paths retained for the per-path breakdown.
///
/// The total is always exact; only the breakdown is capped. A path is
/// caller-controlled, so an uncapped map is an unbounded allocation driven by
/// whoever can reach the port.
const UNKNOWN_PATH_CARDINALITY: usize = 64;

/// Retained prefix of a recorded unknown path.
const UNKNOWN_PATH_MAX_LEN: usize = 256;

/// Toasts retained while no TUI is attached.
const TOAST_RING_CAPACITY: usize = 64;

/// The HTTP verbs the measured surface uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum V1Method {
    Get,
    Post,
    Put,
    Patch,
}

impl V1Method {
    /// The uppercase spelling used by HTTP and by the capture document.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
        }
    }

    /// The OpenAPI spelling, for comparison against the committed fixture.
    #[must_use]
    pub const fn as_openapi_key(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
            Self::Put => "put",
            Self::Patch => "patch",
        }
    }
}

impl fmt::Display for V1Method {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What actually answers a measured v1 route in this build.
///
/// Declared per route rather than derived from the path, so the surface's real
/// coverage is a value a test can read. The `Local` variant names the backend it
/// means: there is no way to mark a route served without saying what serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V1Backing {
    /// Served locally by [`show_toast`], which records into the toast sink.
    LocalToastSink,
    /// Registered and answering a structured `501`. No local backend.
    NotImplemented,
}

impl V1Backing {
    /// The spelling used by diagnostics and by the frozen coverage inventory.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalToastSink => "local-toast-sink",
            Self::NotImplemented => "not-implemented",
        }
    }

    /// Whether a request to this route is answered by real local work.
    #[must_use]
    pub const fn is_served(self) -> bool {
        matches!(self, Self::LocalToastSink)
    }
}

impl fmt::Display for V1Backing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One measured route, carrying the evidence that justifies serving it.
///
/// `callsites` is not documentation. It is the acceptance criterion in data form:
/// a route with an empty `callsites` is scope creep, and a test fails on it.
#[derive(Debug, Clone, Copy)]
pub struct V1Route {
    /// HTTP verb the SDK issues.
    pub method: V1Method,
    /// Path exactly as the oracle declares it, using `axum` parameter syntax.
    pub path: &'static str,
    /// SDK method observed calling this route.
    pub sdk_method: &'static str,
    /// Installed plugins whose source contains a callsite.
    pub plugins: &'static [&'static str],
    /// `package → file:line` evidence for each callsite. Never empty.
    pub callsites: &'static [&'static str],
    /// What answers this route here. Read by the `501` body and by diagnostics.
    pub backing: V1Backing,
    /// The `/api` route a caller should use instead, when one is served here.
    ///
    /// `None` is not "unmeasured": it means this capability has no served `/api`
    /// spelling in this build, so a caller that needs it needs a v1 backend.
    /// Each value was taken from the oracle document, which serves both
    /// surfaces and so states the equivalence itself — `POST /session/{id}/abort`
    /// (`session.abort`) against `POST /api/session/{id}/interrupt`
    /// (`v2.session.interrupt`), and so on. The `501` body reads this field, so
    /// the alternative a plugin author is sent to cannot drift from the route.
    pub api_alternative: Option<&'static str>,
}

const AG: &str = "opencode-antigravity-auth@1.6.0";
const KIRO: &str = "@sunerpy/opencode-kiro-auth@0.20.6";
const OMO: &str = "@sunerpy/oh-my-openagent@4.21.0";

/// The 20 routes the installed plugins actually call.
///
/// Measured 2026-08-06 against the plugins named at
/// `/config/.config/opencode/opencode.json:87-92`. Full derivation, including the
/// difference from the plan's originally-recorded six methods, is in
/// `docs/v1-surface-capture.md`.
pub const V1_SURFACE: &[V1Route] = &[
    V1Route {
        method: V1Method::Put,
        path: "/auth/{providerID}",
        sdk_method: "client.auth.set",
        plugins: &[AG],
        callsites: &["opencode-antigravity-auth: dist/src/plugin.js:1400,2319,2337,2366"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Post,
        path: "/log",
        sdk_method: "client.app.log",
        plugins: &[AG],
        callsites: &["opencode-antigravity-auth: dist/src/plugin/logger.js:45-50"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Get,
        path: "/agent",
        sdk_method: "client.app.agents",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:135963"],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("GET /api/agent"),
    },
    V1Route {
        method: V1Method::Get,
        path: "/config",
        sdk_method: "client.config.get",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:136416,137080,171644"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Get,
        path: "/provider",
        sdk_method: "client.provider.list",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:26958,84674"],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("GET /api/provider"),
    },
    V1Route {
        method: V1Method::Post,
        path: "/provider/{providerID}/oauth/authorize",
        sdk_method: "client.provider.oauth.authorize",
        plugins: &[KIRO],
        callsites: &["opencode-kiro-auth: dist/core/request/request-handler.js:783-786"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Post,
        path: "/provider/{providerID}/oauth/callback",
        sdk_method: "client.provider.oauth.callback",
        plugins: &[KIRO],
        callsites: &["opencode-kiro-auth: dist/core/request/request-handler.js:787-790"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Get,
        path: "/session",
        sdk_method: "client.session.list",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:128645,128654"],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("GET /api/session"),
    },
    V1Route {
        method: V1Method::Post,
        path: "/session",
        sdk_method: "client.session.create",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:131233,132341,135030,143073"],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("POST /api/session"),
    },
    V1Route {
        method: V1Method::Get,
        path: "/session/status",
        sdk_method: "client.session.status",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:10581,123210,131119,132235,133654"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Get,
        path: "/session/{sessionID}",
        sdk_method: "client.session.get",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:90497,96292,96646,116276,131215"],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("GET /api/session/{sessionID}"),
    },
    V1Route {
        method: V1Method::Patch,
        path: "/session/{sessionID}",
        sdk_method: "client.session.update",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:138043"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Get,
        path: "/session/{sessionID}/children",
        sdk_method: "client.session.children",
        plugins: &[OMO],
        // The plugin-entry line numbers for this one call were not captured; the
        // citation is the same package's CLI bundle. Recorded as gap G1 in
        // `docs/v1-surface-capture.md` rather than presented as equal evidence.
        callsites: &[
            "oh-my-openagent: dist/cli/index.js:106371,106539 (plugin-entry line UNVERIFIED, gap G1)",
        ],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Get,
        path: "/session/{sessionID}/todo",
        sdk_method: "client.session.todo",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:89318,89712,90912,119229,143674"],
        backing: V1Backing::NotImplemented,
        api_alternative: None,
    },
    V1Route {
        method: V1Method::Post,
        path: "/session/{sessionID}/abort",
        sdk_method: "client.session.abort",
        plugins: &[AG, OMO],
        callsites: &[
            "opencode-antigravity-auth: dist/src/plugin/recovery.js:293",
            "oh-my-openagent: dist/index.js:106808,120119,131421",
        ],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("POST /api/session/{sessionID}/interrupt"),
    },
    V1Route {
        method: V1Method::Post,
        path: "/session/{sessionID}/summarize",
        sdk_method: "client.session.summarize",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:94259,119806,119913"],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("POST /api/session/{sessionID}/compact"),
    },
    V1Route {
        method: V1Method::Get,
        path: "/session/{sessionID}/message",
        sdk_method: "client.session.messages",
        plugins: &[AG, OMO],
        callsites: &[
            "opencode-antigravity-auth: dist/src/plugin/recovery.js:295",
            "oh-my-openagent: dist/index.js:28404,85143,87664",
        ],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("GET /api/session/{sessionID}/message"),
    },
    V1Route {
        method: V1Method::Post,
        path: "/session/{sessionID}/message",
        sdk_method: "client.session.prompt",
        plugins: &[AG],
        callsites: &[
            "opencode-antigravity-auth: dist/src/plugin/recovery.js:126,198",
            "opencode-antigravity-auth: dist/src/plugin.js:1077",
        ],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("POST /api/session/{sessionID}/prompt"),
    },
    V1Route {
        method: V1Method::Post,
        path: "/session/{sessionID}/prompt_async",
        sdk_method: "client.session.promptAsync",
        plugins: &[OMO],
        callsites: &["oh-my-openagent: dist/index.js:138443"],
        backing: V1Backing::NotImplemented,
        api_alternative: Some("POST /api/session/{sessionID}/prompt"),
    },
    V1Route {
        method: V1Method::Post,
        path: TOAST_PATH,
        sdk_method: "client.tui.showToast",
        plugins: &[AG, KIRO, OMO],
        callsites: &[
            "opencode-antigravity-auth: dist/src/plugin.js:1086,1183,1254,2476",
            "opencode-kiro-auth: dist/plugin.js:46-47",
            "oh-my-openagent: dist/index.js:89478,93846,94061",
        ],
        backing: V1Backing::LocalToastSink,
        api_alternative: None,
    },
];

/// The one route in [`V1_SURFACE`] with a real local backend.
pub const TOAST_PATH: &str = "/tui/show-toast";

impl V1Route {
    /// What to tell the plugin author who just received this route's `501`.
    ///
    /// Built from [`V1Route::api_alternative`] rather than written per route, so
    /// the advice cannot survive the fact it describes. The previous text pointed
    /// at "todos 57-62"; those closed, and a hint that cites finished work tells a
    /// caller nothing about what to do next.
    #[must_use]
    pub fn hint(&self) -> String {
        match self.api_alternative {
            Some(alternative) => format!(
                "this pre-/api route is registered but has no local backend; call `{alternative}` \
                 instead, which serves the same capability in this build",
            ),
            None => format!(
                "this pre-/api route is registered but has no local backend, and `{}` has no \
                 served /api equivalent here; there is no alternative call that works today",
                self.sdk_method,
            ),
        }
    }
}

/// What the v1 surface actually covers, counted from [`V1_SURFACE`].
///
/// A [`Copy`] summary rather than a constant: every field is derived by
/// [`v1_coverage`], so the numbers a `501` body and the compatibility matrix
/// publish are recomputed from the route table instead of transcribed beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V1Coverage {
    /// Routes the plugin capture measured, and this surface therefore registers.
    pub measured: usize,
    /// Routes answered by real local work.
    pub served: usize,
    /// Routes registered as structured `501` seams.
    pub unbacked: usize,
    /// Unbacked routes whose `501` can name a served `/api` alternative.
    pub redirected: usize,
}

impl V1Coverage {
    /// One line, for an error body or a log, naming the shape of the gap.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} of {} measured pre-/api routes have no local backend ({} of those can name a \
             served /api alternative); {} served locally",
            self.unbacked, self.measured, self.redirected, self.served,
        )
    }
}

/// Counts [`V1_SURFACE`]'s real coverage.
#[must_use]
pub fn v1_coverage() -> V1Coverage {
    let unbacked = V1_SURFACE
        .iter()
        .filter(|route| !route.backing.is_served())
        .collect::<Vec<_>>();
    V1Coverage {
        measured: V1_SURFACE.len(),
        served: V1_SURFACE.len() - unbacked.len(),
        unbacked: unbacked.len(),
        redirected: unbacked
            .iter()
            .filter(|route| route.api_alternative.is_some())
            .count(),
    }
}

/// A toast a plugin asked to display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Text the plugin wants shown. Required; there is nothing to show without it.
    pub message: String,
    /// `info`, `success`, `warning` or `error` upstream; recorded verbatim here.
    pub variant: String,
    /// Optional heading.
    pub title: Option<String>,
    /// Optional display duration in milliseconds.
    pub duration: Option<u64>,
}

/// A TUI that can display toasts once one is attached.
///
/// No server entry point registers one: `crates/oc-server/src/main.rs` and
/// `crates/oc-cli/src/cmd/serve.rs` both build a bare [`CompatV1State::new`], so
/// in every shipped server this route records and never displays. That is why
/// [`V1Backing::LocalToastSink`] names a sink rather than a TUI. Keeping the route
/// independent of a display is the point: attaching one must not change the HTTP
/// surface a plugin already talks to.
pub trait ToastForwarder: Send + Sync + fmt::Debug {
    /// Displays one toast. Called while the request is in flight, so it must not
    /// block for long; a real TUI enqueues and returns.
    fn show(&self, toast: &Toast);
}

/// Bounded record of toasts, plus the forward seam.
#[derive(Debug)]
struct ToastSink {
    state: Mutex<ToastRing>,
    forwarder: Option<Arc<dyn ToastForwarder>>,
}

#[derive(Debug, Default)]
struct ToastRing {
    retained: std::collections::VecDeque<Toast>,
    accepted: u64,
    dropped: u64,
}

impl ToastSink {
    fn record(&self, toast: Toast) {
        if let Some(forwarder) = self.forwarder.as_ref() {
            forwarder.show(&toast);
        }
        let mut state = self.lock();
        state.accepted += 1;
        if state.retained.len() >= TOAST_RING_CAPACITY {
            state.retained.pop_front();
            state.dropped += 1;
        }
        state.retained.push_back(toast);
    }

    fn lock(&self) -> MutexGuard<'_, ToastRing> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Counters behind the unknown-route 404s.
#[derive(Debug, Default)]
pub struct UnknownRoutes {
    total: AtomicU64,
    /// Bounded per-path breakdown; `paths.len()` is capped, `total` is not.
    paths: Mutex<BTreeMap<String, u64>>,
    /// Sightings of paths that arrived after the breakdown filled up.
    overflowed: AtomicU64,
}

impl UnknownRoutes {
    /// Records one sighting, returning `true` when this path is newly seen.
    ///
    /// The boolean drives the stderr line: an operator gets told once per distinct
    /// path, so a scanner walking a thousand URLs is fully counted but logs a
    /// bounded number of lines.
    fn record(&self, path: &str) -> bool {
        self.total.fetch_add(1, Ordering::Relaxed);
        let key = truncate(path, UNKNOWN_PATH_MAX_LEN);
        let mut paths = self.lock_paths();
        if let Some(count) = paths.get_mut(&key) {
            *count += 1;
            return false;
        }
        if paths.len() >= UNKNOWN_PATH_CARDINALITY {
            self.overflowed.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        paths.insert(key, 1);
        true
    }

    /// Every unknown-path request seen by this process.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Sightings dropped from the breakdown once it reached its cardinality cap.
    #[must_use]
    pub fn overflowed(&self) -> u64 {
        self.overflowed.load(Ordering::Relaxed)
    }

    /// Per-path counts, bounded by [`UNKNOWN_PATH_CARDINALITY`].
    #[must_use]
    pub fn breakdown(&self) -> BTreeMap<String, u64> {
        self.lock_paths().clone()
    }

    /// Requests seen for one exact path.
    #[must_use]
    pub fn count_for(&self, path: &str) -> u64 {
        self.lock_paths()
            .get(&truncate(path, UNKNOWN_PATH_MAX_LEN))
            .copied()
            .unwrap_or_default()
    }

    fn lock_paths(&self) -> MutexGuard<'_, BTreeMap<String, u64>> {
        self.paths.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// Shared state for the v1 compatibility surface.
#[derive(Clone, Debug)]
pub struct CompatV1State {
    toasts: Arc<ToastSink>,
    unknown: Arc<UnknownRoutes>,
}

impl CompatV1State {
    /// Creates the surface with no TUI attached.
    #[must_use]
    pub fn new() -> Self {
        Self {
            toasts: Arc::new(ToastSink {
                state: Mutex::new(ToastRing::default()),
                forwarder: None,
            }),
            unknown: Arc::new(UnknownRoutes::default()),
        }
    }

    /// Attaches a display for toasts. Recording continues either way, so
    /// diagnostics stay meaningful after a TUI connects.
    #[must_use]
    pub fn with_toast_forwarder(mut self, forwarder: Arc<dyn ToastForwarder>) -> Self {
        self.toasts = Arc::new(ToastSink {
            state: Mutex::new(ToastRing::default()),
            forwarder: Some(forwarder),
        });
        self
    }

    /// The unknown-route counters, for callers assembling their own diagnostics.
    #[must_use]
    pub fn unknown_routes(&self) -> &Arc<UnknownRoutes> {
        &self.unknown
    }

    /// Toasts still retained by the sink, oldest first.
    #[must_use]
    pub fn retained_toasts(&self) -> Vec<Toast> {
        self.toasts.lock().retained.iter().cloned().collect()
    }

    /// Toasts accepted since start, including any since dropped from the ring.
    #[must_use]
    pub fn accepted_toasts(&self) -> u64 {
        self.toasts.lock().accepted
    }

    /// Whether a TUI is attached. `false` means toasts are recorded and not shown.
    #[must_use]
    pub fn toast_display_attached(&self) -> bool {
        self.toasts.forwarder.is_some()
    }
}

impl Default for CompatV1State {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the measured v1 surface, its diagnostics, and the scoped 404 accounting.
///
/// One nest per prefix, each carrying its own fallback. Verbs sharing a path are
/// merged into a single [`MethodRouter`] first, because registering the same path
/// twice is a matcher conflict rather than an addition.
pub fn compat_v1_router(state: CompatV1State) -> Router {
    let mut grouped: BTreeMap<&str, BTreeMap<String, MethodRouter<CompatV1State>>> =
        BTreeMap::new();
    for route in V1_SURFACE {
        let (prefix, nested_path) = split_prefix(route.path);
        let handler: MethodRouter<CompatV1State> = if route.path == TOAST_PATH {
            post(show_toast)
        } else {
            seam_handler(route)
        };
        let paths = grouped.entry(prefix).or_default();
        match paths.remove(&nested_path) {
            Some(existing) => paths.insert(nested_path, existing.merge(handler)),
            None => paths.insert(nested_path, handler),
        };
    }

    let mut router: Router<CompatV1State> = Router::new();
    for prefix in V1_PREFIXES {
        let paths = grouped.remove(*prefix);
        let has_root = paths
            .as_ref()
            .is_some_and(|paths| paths.contains_key(NESTED_ROOT));
        let mut nested: Router<CompatV1State> = Router::new();
        for (path, handler) in paths.unwrap_or_default() {
            nested = nested.route(&path, handler);
        }
        // Must follow the routes it applies to: axum retrofits this onto the
        // `MethodRouter`s already registered, not ones added later.
        let nested = nested
            .method_not_allowed_fallback(unknown_v1_operation)
            .fallback(unknown_v1_route);
        router = router.nest(&format!("/{prefix}"), nested);
        if !has_root {
            router = router.route(&format!("/{prefix}"), any(unknown_v1_route));
        }
    }
    debug_assert!(
        grouped.is_empty(),
        "a measured route sits under a prefix the accounting does not cover"
    );

    router
        .route(V1_DIAGNOSTICS_PATH, get(diagnostics))
        .with_state(state)
}

const NESTED_ROOT: &str = "/";

fn split_prefix(path: &str) -> (&str, String) {
    let trimmed = path.trim_start_matches('/');
    match trimmed.split_once('/') {
        Some((prefix, rest)) => (prefix, format!("/{rest}")),
        None => (trimmed, NESTED_ROOT.to_owned()),
    }
}

fn seam_handler(route: &'static V1Route) -> MethodRouter<CompatV1State> {
    let respond = move || async move { CompatV1Error::NoBackend(route) };
    match route.method {
        V1Method::Get => get(respond),
        V1Method::Post => post(respond),
        V1Method::Put => put(respond),
        V1Method::Patch => patch(respond),
    }
}

/// Failures the v1 surface reports. No variant carries a credential.
///
/// `client.auth.set` posts a secret to `PUT /auth/{providerID}`; the seam for it
/// never reads or echoes the body, and this type has no field that could hold it.
#[derive(Debug, thiserror::Error)]
pub enum CompatV1Error {
    /// A measured route with no local backend yet.
    #[error("`{}` has no local backend in this build", .0.sdk_method)]
    NoBackend(&'static V1Route),
    /// A request that cannot be served as written.
    #[error("{0}")]
    InvalidRequest(&'static str),
}

impl IntoResponse for CompatV1Error {
    fn into_response(self) -> Response {
        let (status, code, message, detail) = match self {
            Self::NoBackend(route) => (
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                self.to_string(),
                json!({
                    "sdkMethod": route.sdk_method,
                    "route": format!("{} {}", route.method, route.path),
                    "callers": route.plugins,
                    "backing": route.backing.as_str(),
                    "apiAlternative": route.api_alternative,
                    "hint": route.hint(),
                    // Counted from V1_SURFACE on each response rather than
                    // restated as a literal, so it cannot outlive the truth.
                    "surfaceCoverage": v1_coverage().summary(),
                }),
            ),
            Self::InvalidRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                self.to_string(),
                Value::Null,
            ),
        };
        let mut body = json!({ "code": code, "message": message });
        if let (Some(body), Some(detail)) = (body.as_object_mut(), detail.as_object()) {
            for (key, value) in detail {
                body.insert(key.clone(), value.clone());
            }
        }
        (status, Json(json!({ "error": body }))).into_response()
    }
}

/// `POST /tui/show-toast` request body.
///
/// Deliberately more lenient than the oracle, which marks `variant` required and
/// forbids unknown fields. Three of three installed plugins call this route, so a
/// `400` over a cosmetic mismatch would break exactly the toasts this endpoint
/// exists to preserve. A missing `message` is still rejected: there is then
/// nothing to display.
#[derive(Debug, Deserialize)]
struct ToastBody {
    message: String,
    variant: Option<String>,
    title: Option<String>,
    duration: Option<u64>,
}

async fn show_toast(
    State(state): State<CompatV1State>,
    body: Result<Json<ToastBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<bool>, CompatV1Error> {
    let Json(body) = body.map_err(|_| {
        CompatV1Error::InvalidRequest("a toast requires a JSON body with a string `message`")
    })?;
    state.toasts.record(Toast {
        message: body.message,
        variant: body.variant.unwrap_or_else(|| "info".to_owned()),
        title: body.title,
        duration: body.duration,
    });
    // Upstream answers a bare `true`. It stays `true` with no TUI attached: the
    // toast was accepted and recorded, and an error here would fail a plugin over
    // a display that does not exist yet.
    Ok(Json(true))
}

async fn unknown_v1_route(
    State(state): State<CompatV1State>,
    // The nest strips its prefix before the fallback runs, so the request's own
    // URI names only the remainder. Reporting a path the caller never sent would
    // make the 404 useless for the operator it is written for.
    OriginalUri(uri): OriginalUri,
) -> Response {
    let path = uri.path().to_owned();
    let first_sighting = state.unknown.record(&path);
    if first_sighting {
        // The operator-facing half of the accounting. Printed once per distinct
        // path so a scanner cannot flood the log while still being counted.
        eprintln!(
            "oc-server: unimplemented v1 route `{path}`; re-run the capture in \
             docs/v1-surface-capture.md and extend V1_SURFACE (total unaccounted \
             requests: {})",
            state.unknown.total()
        );
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "code": "unimplemented_v1_route",
                "message": format!("`{path}` is not part of the measured pre-/api surface"),
                "path": path,
                "action": "re-run the plugin capture documented in docs/v1-surface-capture.md, \
                           add the route to V1_SURFACE with its callsite, then rebuild",
                "diagnostics": V1_DIAGNOSTICS_PATH,
                "unaccountedRequests": state.unknown.total(),
            }
        })),
    )
        .into_response()
}

/// A verb the capture never recorded, on a path it did.
///
/// `DELETE /auth/{providerID}` is the live example: the oracle serves it, no
/// installed plugin calls it, so only `PUT` is mounted. Left alone, axum answers a
/// bodiless 405 and the operator learns nothing. Routing it here keeps the status
/// honest — the path exists, the method does not — while still naming the gap and
/// counting it, so a mis-measured *operation* is as visible as a mis-measured path.
async fn unknown_v1_operation(
    State(state): State<CompatV1State>,
    method: axum::http::Method,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let path = uri.path().to_owned();
    let operation = format!("{method} {path}");
    if state.unknown.record(&operation) {
        eprintln!(
            "oc-server: unimplemented v1 operation `{operation}`; re-run the capture in \
             docs/v1-surface-capture.md and extend V1_SURFACE (total unaccounted \
             requests: {})",
            state.unknown.total()
        );
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({
            "error": {
                "code": "unimplemented_v1_operation",
                "message": format!("`{operation}` is not part of the measured pre-/api surface"),
                "path": path,
                "method": method.as_str(),
                "action": "re-run the plugin capture documented in docs/v1-surface-capture.md, \
                           add the route to V1_SURFACE with its callsite, then rebuild",
                "diagnostics": V1_DIAGNOSTICS_PATH,
                "unaccountedRequests": state.unknown.total(),
            }
        })),
    )
        .into_response()
}

async fn diagnostics(State(state): State<CompatV1State>) -> Json<Value> {
    let implemented = V1_SURFACE
        .iter()
        .map(|route| {
            json!({
                "route": format!("{} {}", route.method, route.path),
                "sdkMethod": route.sdk_method,
                "callers": route.plugins,
                "callsites": route.callsites,
                "backend": route.backing.as_str(),
                "apiAlternative": route.api_alternative,
            })
        })
        .collect::<Vec<_>>();
    let coverage = v1_coverage();
    let ring = state.toasts.lock();
    Json(json!({
        "v1Surface": {
            "registeredRoutes": implemented.len(),
            "servedRoutes": coverage.served,
            "unbackedRoutes": coverage.unbacked,
            "unbackedWithApiAlternative": coverage.redirected,
            "coverage": coverage.summary(),
            "routes": implemented,
            "accountedPrefixes": V1_PREFIXES,
        },
        "unknownRoutes": {
            "total": state.unknown.total(),
            "distinctPathsRetained": state.unknown.breakdown().len(),
            "distinctPathCap": UNKNOWN_PATH_CARDINALITY,
            "overflowedSightings": state.unknown.overflowed(),
            "paths": state.unknown.breakdown(),
            "action": "a non-zero total means the capture is incomplete; re-run it and extend V1_SURFACE",
        },
        "toasts": {
            "displayAttached": state.toasts.forwarder.is_some(),
            "accepted": ring.accepted,
            "retained": ring.retained.len(),
            "droppedFromRing": ring.dropped,
            "ringCapacity": TOAST_RING_CAPACITY,
            "latest": ring.retained.back().map(|toast| json!({
                "message": toast.message,
                "variant": toast.variant,
                "title": toast.title,
                "duration": toast.duration,
            })),
        },
    }))
}
