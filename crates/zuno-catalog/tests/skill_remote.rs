//! `skills.urls[]` against real HTTP servers.
//!
//! The remote root is the only one that can hang, and the whole reason it is
//! bounded is that a flaky URL in a config file must not make the agent
//! unusable. So these tests do not stub the transport: they stand up servers that
//! misbehave in each specific way and assert the load still succeeds.
//!
//! An entry whose `files` omits `SKILL.md` never appears in the output, an entry
//! with a `version` gets a `.zuno-version` stamp next to its `SKILL.md`, and both
//! land under `$XDG_CACHE_HOME/zuno/skills/<name>/`.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zuno_catalog::skill::remote::{FILE_CONCURRENCY, REMOTE_TIMEOUT, VERSION_FILE};
use zuno_catalog::skill::{SkillOptions, SkillWarningKind, Skills, builtin, load};
use zuno_paths::Env;
use zuno_paths::env::{HOME, XDG_CACHE_HOME, XDG_CONFIG_HOME};

/// An address whose TCP connect is guaranteed to fail *fast*, with
/// `ECONNREFUSED`, so the load reports `IndexUnreachable`.
///
/// Port 1 is privileged: the test process does not run as root, so nothing in
/// this workspace — or anywhere else on the machine without root — can bind it
/// and answer. Nothing is listening, so the connect is refused immediately.
///
/// Do NOT go back to the bind-then-drop trick (bind `127.0.0.1:0`, learn the
/// ephemeral port, drop the listener). Once the listener is dropped the port
/// returns to the ephemeral pool, and `cargo test` runs targets concurrently:
/// a sibling test — including a `wiremock` server — can bind that exact port
/// before the request goes out. The "unreachable" URL then reaches a live
/// server and loads *its* index, so this test intermittently saw a foreign
/// skill in the result and the merge gate flaked.
///
/// A reserved unroutable address (e.g. `192.0.2.1`, RFC 5737) is also wrong
/// here: it *times out* instead of refusing, producing `IndexTimeout` and
/// making the test slow — that case is already covered separately by
/// `a_hanging_index_is_abandoned_at_the_timeout_without_failing_the_load`.
const REFUSED_ADDRESS: &str = "127.0.0.1:1";

struct Tree {
    dir: TempDir,
}

impl Tree {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("tempdir"),
        }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn cache_root(&self) -> PathBuf {
        self.home().join(".cache/zuno/skills")
    }

    fn local_skill(&self, name: &str) {
        let path = self
            .home()
            .join(".config/zuno/skill")
            .join(name)
            .join("SKILL.md");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: local\n---\nB\n"),
        )
        .expect("write");
    }

    fn options(&self, urls: Vec<String>) -> SkillOptions {
        let env = Env::empty()
            .with(HOME, self.home().to_string_lossy().as_ref())
            .with(
                XDG_CONFIG_HOME,
                self.home().join(".config").to_string_lossy().as_ref(),
            )
            .with(
                XDG_CACHE_HOME,
                self.home().join(".cache").to_string_lossy().as_ref(),
            );
        SkillOptions::new(
            self.dir.path().join("proj"),
            None::<PathBuf>,
            &env,
            Vec::new(),
            urls,
        )
    }
}

fn names(skills: &Skills) -> Vec<String> {
    skills.all().iter().map(|s| s.name.clone()).collect()
}

fn skill_body(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nRemote body.\n")
}

#[tokio::test]
async fn an_index_entry_without_skill_md_is_warned_about_and_never_downloaded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"skills":[
                 {"name":"usable","files":["SKILL.md","helper.md"]},
                 {"name":"unusable","files":["README.md"]}
               ]}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/usable/SKILL.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string(skill_body("usable", "remote")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/usable/helper.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string("helper\n"))
        .mount(&server)
        .await;

    let tree = Tree::new();
    let skills = load(&tree.options(vec![server.uri()])).await;

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "usable".to_string()]
    );
    assert_eq!(
        skills.get("usable").expect("present").location,
        tree.cache_root()
            .join("usable/SKILL.md")
            .to_string_lossy()
            .into_owned()
    );
    assert!(tree.cache_root().join("usable/helper.md").is_file());
    assert!(
        !tree.cache_root().join("unusable").exists(),
        "an unusable entry must not be fetched at all"
    );
    let warnings: Vec<&SkillWarningKind> = skills.warnings().iter().map(|w| w.kind()).collect();
    assert_eq!(
        warnings,
        vec![&SkillWarningKind::EntryMissingSkillMd {
            skill: "unusable".to_string()
        }]
    );
}

#[tokio::test]
async fn a_version_change_refreshes_the_cache_and_restamps_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"skills":[{"name":"versioned","files":["SKILL.md"],"version":"v2"}]}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/versioned/SKILL.md"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(skill_body("versioned", "the v2 copy")),
        )
        .mount(&server)
        .await;

    let tree = Tree::new();
    let cached = tree.cache_root().join("versioned");
    fs::create_dir_all(&cached).expect("mkdir");
    fs::write(
        cached.join("SKILL.md"),
        skill_body("versioned", "the v1 copy"),
    )
    .expect("write");
    fs::write(cached.join(VERSION_FILE), "v1").expect("write");

    let skills = load(&tree.options(vec![server.uri()])).await;

    assert_eq!(
        skills
            .get("versioned")
            .expect("present")
            .description
            .as_deref(),
        Some("the v2 copy")
    );
    assert_eq!(
        fs::read_to_string(cached.join(VERSION_FILE)).expect("stamp"),
        "v2"
    );
    assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
    // `Effect.ensuring` at `discovery.ts:123`: no staging directory survives.
    let leftovers: Vec<_> = fs::read_dir(tree.cache_root())
        .expect("readdir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-") || name.contains(".old-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn a_matching_version_leaves_the_cache_alone() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"skills":[{"name":"pinned","files":["SKILL.md"],"version":"v1"}]}"#,
        ))
        .mount(&server)
        .await;

    let tree = Tree::new();
    let cached = tree.cache_root().join("pinned");
    fs::create_dir_all(&cached).expect("mkdir");
    fs::write(
        cached.join("SKILL.md"),
        skill_body("pinned", "already here"),
    )
    .expect("write");
    fs::write(cached.join(VERSION_FILE), "v1").expect("write");

    // No mock for `/pinned/SKILL.md`: an in-place refresh must skip a file that
    // already exists (`discovery.ts:38`), so nothing is requested.
    let skills = load(&tree.options(vec![server.uri()])).await;

    assert_eq!(
        skills
            .get("pinned")
            .expect("present")
            .description
            .as_deref(),
        Some("already here")
    );
}

#[tokio::test]
async fn a_404_index_is_warned_about_and_the_load_continues() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let tree = Tree::new();
    tree.local_skill("survivor");
    let skills = load(&tree.options(vec![server.uri()])).await;

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "survivor".to_string()]
    );
    assert_eq!(
        skills
            .warnings()
            .iter()
            .map(|w| w.kind())
            .collect::<Vec<_>>(),
        vec![&SkillWarningKind::IndexStatus(404)]
    );
}

#[tokio::test]
async fn a_malformed_index_is_warned_about_and_the_load_continues() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"skills\":\"not an array\"}"))
        .mount(&server)
        .await;

    let tree = Tree::new();
    tree.local_skill("survivor");
    let skills = load(&tree.options(vec![server.uri()])).await;

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "survivor".to_string()]
    );
    assert!(matches!(
        skills.warnings()[0].kind(),
        SkillWarningKind::IndexMalformed(_)
    ));
}

#[tokio::test]
async fn an_unreachable_host_is_warned_about_and_the_load_continues() {
    let dead = REFUSED_ADDRESS;

    let tree = Tree::new();
    tree.local_skill("survivor");
    let skills = load(&tree.options(vec![format!("http://{dead}")])).await;

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "survivor".to_string()]
    );
    assert!(matches!(
        skills.warnings()[0].kind(),
        SkillWarningKind::IndexUnreachable(_)
    ));
}

/// A server that accepts the connection and then says nothing at all — the worst
/// case, because there is no error to react to, only silence.
#[tokio::test]
async fn a_hanging_index_is_abandoned_at_the_timeout_without_failing_the_load() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let server = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });

    let tree = Tree::new();
    tree.local_skill("survivor");
    let started = Instant::now();
    let skills = load(&tree.options(vec![format!("http://{address}")])).await;
    let elapsed = started.elapsed();
    server.abort();

    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the connection must actually have been made"
    );
    assert!(
        elapsed >= REMOTE_TIMEOUT,
        "gave up after {elapsed:?}, before the {REMOTE_TIMEOUT:?} budget"
    );
    assert!(
        elapsed < REMOTE_TIMEOUT * 3,
        "took {elapsed:?}, far past the {REMOTE_TIMEOUT:?} budget"
    );
    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "survivor".to_string()],
        "a hanging index must not cost the local skills"
    );
    assert_eq!(
        skills
            .warnings()
            .iter()
            .map(|w| w.kind())
            .collect::<Vec<_>>(),
        vec![&SkillWarningKind::IndexTimeout]
    );
}

/// Observe [`FILE_CONCURRENCY`] rather than trust it.
///
/// A counting TCP server records the peak number of connections open at once
/// while one skill's files download. `reqwest` needs a separate connection per
/// in-flight request, so the peak is a lower bound on in-flight requests: it
/// proves the downloads really are concurrent *and* that they never exceed the
/// declared bound.
#[tokio::test]
async fn file_downloads_are_concurrent_and_bounded() {
    let file_count = FILE_CONCURRENCY * 2;
    let files: Vec<String> = (0..file_count)
        .map(|index| {
            if index == 0 {
                "SKILL.md".to_string()
            } else {
                format!("part{index}.md")
            }
        })
        .collect();
    let index_body = format!(
        r#"{{"skills":[{{"name":"wide","files":{}}}]}}"#,
        serde_json::to_string(&files).expect("json")
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(AtomicUsize::new(0));
    let (live_t, peak_t, served_t) = (Arc::clone(&live), Arc::clone(&peak), Arc::clone(&served));

    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (live, peak, served) = (
                Arc::clone(&live_t),
                Arc::clone(&peak_t),
                Arc::clone(&served_t),
            );
            let index_body = index_body.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let target = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1).map(str::to_string))
                    .unwrap_or_default();

                let body = if target.ends_with("index.json") {
                    index_body
                } else if target.ends_with("SKILL.md") {
                    "---\nname: wide\ndescription: wide\n---\nB\n".to_string()
                } else {
                    "part\n".to_string()
                };

                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(120)).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
                live.fetch_sub(1, Ordering::SeqCst);
                served.fetch_add(1, Ordering::SeqCst);
            });
        }
    });

    let tree = Tree::new();
    let skills = load(&tree.options(vec![format!("http://{address}")])).await;
    server.abort();

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "wide".to_string()]
    );
    assert_eq!(
        served.load(Ordering::SeqCst),
        file_count + 1,
        "every file plus the index must have been served"
    );
    let observed = peak.load(Ordering::SeqCst);
    assert!(
        observed > 1,
        "downloads ran one at a time; the bound is supposed to allow {FILE_CONCURRENCY}"
    );
    assert!(
        observed <= FILE_CONCURRENCY,
        "{observed} downloads were in flight, above the {FILE_CONCURRENCY} bound"
    );
}

#[tokio::test]
async fn an_index_entry_pointing_outside_the_cache_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"skills":[{"name":"evil","files":["SKILL.md","../../escaped.md"]}]}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/evil/SKILL.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string(skill_body("evil", "remote")))
        .mount(&server)
        .await;

    let tree = Tree::new();
    let skills = load(&tree.options(vec![server.uri()])).await;

    assert!(skills.get("evil").is_some(), "the safe file still loads");
    assert!(
        !tree.home().join(".cache/escaped.md").exists()
            && !tree.cache_root().join("../escaped.md").exists()
    );
    assert_eq!(
        skills
            .warnings()
            .iter()
            .map(|w| w.kind())
            .collect::<Vec<_>>(),
        vec![&SkillWarningKind::UnsafeIndexPath {
            skill: "evil".to_string(),
            file: "../../escaped.md".to_string()
        }]
    );
}

#[tokio::test]
async fn a_url_with_a_path_prefix_resolves_the_index_beneath_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/registry/index.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"skills":[{"name":"nested","files":["SKILL.md"]}]}"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/registry/nested/SKILL.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string(skill_body("nested", "remote")))
        .mount(&server)
        .await;

    let tree = Tree::new();
    let skills = load(&tree.options(vec![format!("{}/registry", server.uri())])).await;

    assert!(skills.get("nested").is_some(), "{:?}", skills.warnings());
}

#[tokio::test]
async fn one_dead_url_does_not_stop_the_next_one() {
    let good = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"skills":[{"name":"second","files":["SKILL.md"]}]}"#),
        )
        .mount(&good)
        .await;
    Mock::given(method("GET"))
        .and(path("/second/SKILL.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string(skill_body("second", "remote")))
        .mount(&good)
        .await;

    let dead = REFUSED_ADDRESS;

    let tree = Tree::new();
    let skills = load(&tree.options(vec![format!("http://{dead}"), good.uri()])).await;

    assert_eq!(
        names(&skills),
        vec![builtin::NAME.to_string(), "second".to_string()]
    );
}
