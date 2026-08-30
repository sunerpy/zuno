//! Behavioural tests for instruction discovery and loading.
//!
//! The unit tests inside `zuno_config::instructions` cover the discovery rules on
//! synthetic trees. These tests cover the two things a unit test cannot: real
//! fixture trees on disk, and a real HTTP server that misbehaves.

mod instructions {

    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zuno_config::instructions::{
        INSTRUCTION_FILENAMES, InstructionOptions, InstructionPath, Instructions,
        LOCAL_CONCURRENCY, Origin, REMOTE_CONCURRENCY, REMOTE_TIMEOUT, UpwardClaims, WarningKind,
    };
    use zuno_paths::Env;
    use zuno_paths::env::{HOME, XDG_CONFIG_HOME};

    /// A fixture tree with a private `$HOME` and `$XDG_CONFIG_HOME`, so nothing on
    /// the developer's machine can leak into an assertion.
    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(root.path().join("home/.config")).expect("mkdir home");
            Self { root }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.root.path().join(relative)
        }

        fn write(&self, relative: &str, body: &str) -> PathBuf {
            let target = self.path(relative);
            std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
            std::fs::write(&target, body).expect("write");
            target
        }

        fn env(&self) -> Env {
            Env::empty()
                .with(HOME, self.path("home").to_string_lossy().into_owned())
                .with(
                    XDG_CONFIG_HOME,
                    self.path("home/.config").to_string_lossy().into_owned(),
                )
        }

        fn options(&self, directory: &str, instructions: Vec<String>) -> InstructionOptions {
            InstructionOptions::new(
                self.path(directory),
                Some(self.path("repo")),
                &self.env(),
                instructions,
            )
        }
    }

    fn paths_of(found: &Instructions, origin: Origin) -> Vec<PathBuf> {
        found
            .paths()
            .iter()
            .filter(|entry| entry.origin() == origin)
            .map(|entry| entry.path().to_path_buf())
            .collect()
    }

    /// A tree holding both product filenames loads only native AGENTS rules.
    #[test]
    fn a_tree_with_both_names_loads_only_agents_md() {
        let fixture = Fixture::new();
        let agents = fixture.write("repo/AGENTS.md", "# agents\nuse tabs\n");
        let claude = fixture.write("repo/CLAUDE.md", "# claude\nuse spaces\n");

        let found = Instructions::discover(&fixture.options("repo", Vec::new()));

        assert_eq!(paths_of(&found, Origin::Project), vec![agents.clone()]);
        assert!(
            !found.contains(&claude),
            "CLAUDE.md must not be loaded when AGENTS.md exists next to it"
        );
        assert_eq!(found.paths().len(), 1);
    }

    /// The AGENTS chain loads root-to-current so nearer rules win later.
    #[test]
    fn the_first_name_claims_every_level_and_the_second_name_none() {
        let fixture = Fixture::new();
        let root_agents = fixture.write("repo/AGENTS.md", "root");
        let sub_agents = fixture.write("repo/pkg/AGENTS.md", "sub");
        fixture.write("repo/CLAUDE.md", "root claude");
        fixture.write("repo/pkg/CLAUDE.md", "sub claude");

        let found = Instructions::discover(&fixture.options("repo/pkg", Vec::new()));

        assert_eq!(
            paths_of(&found, Origin::Project),
            vec![root_agents, sub_agents]
        );
        assert!(!found.contains(&fixture.path("repo/CLAUDE.md")));
        assert!(!found.contains(&fixture.path("repo/pkg/CLAUDE.md")));
    }

    /// `CONTEXT.md` is in the oracle's cascade and deliberately not in this one.
    #[test]
    fn context_md_is_never_loaded() {
        let fixture = Fixture::new();
        let context = fixture.write("repo/CONTEXT.md", "deprecated");

        let found = Instructions::discover(&fixture.options("repo", Vec::new()));

        assert!(found.paths().is_empty());
        assert!(!found.contains(&context));
        assert!(!INSTRUCTION_FILENAMES.contains(&"CONTEXT.md"));
    }

    #[test]
    fn the_global_file_precedes_the_project_chain() {
        let fixture = Fixture::new();
        let global = fixture.write("home/.config/zuno/AGENTS.md", "global");
        let project = fixture.write("repo/AGENTS.md", "project");

        let found = Instructions::discover(&fixture.options("repo", Vec::new()));

        assert_eq!(
            found
                .paths()
                .iter()
                .map(|entry| (entry.path().to_path_buf(), entry.origin()))
                .collect::<Vec<_>>(),
            vec![(global, Origin::Global), (project, Origin::Project)]
        );
    }

    #[test]
    fn claude_instruction_files_are_never_loaded_implicitly() {
        let fixture = Fixture::new();
        let claude_global = fixture.write("home/.claude/CLAUDE.md", "global claude");
        let project_claude = fixture.write("repo/CLAUDE.md", "project claude");

        let found = Instructions::discover(&fixture.options("repo", Vec::new()));
        assert!(!found.contains(&claude_global));
        assert!(!found.contains(&project_claude));
        assert!(found.paths().is_empty(), "{:?}", found.paths());
    }

    #[tokio::test]
    async fn configured_globs_tildes_and_urls_all_resolve() {
        let fixture = Fixture::new();
        fixture.write("repo/docs/style.md", "style");
        fixture.write("repo/docs/testing.md", "testing");
        fixture.write("repo/docs/nested/deep.md", "deep");
        fixture.write("home/personal.md", "personal");
        let absolute = fixture.write("elsewhere/absolute.md", "absolute");

        let options = fixture.options(
            "repo",
            vec![
                "docs/*.md".to_owned(),
                "~/personal.md".to_owned(),
                absolute.to_string_lossy().into_owned(),
                "https://example.invalid/remote.md".to_owned(),
            ],
        );
        let found = Instructions::discover(&options);

        assert_eq!(
            paths_of(&found, Origin::Configured),
            vec![
                fixture.path("repo/docs/style.md"),
                fixture.path("repo/docs/testing.md"),
                fixture.path("home/personal.md"),
                absolute,
            ],
            "`*` must not cross a separator, so docs/nested/deep.md is out"
        );
        assert_eq!(
            found.urls(),
            ["https://example.invalid/remote.md".to_owned()]
        );
    }

    #[tokio::test]
    async fn loading_renders_one_block_per_file_with_the_oracle_header() {
        let fixture = Fixture::new();
        fixture.write("repo/AGENTS.md", "root rules");
        fixture.write("repo/docs/extra.md", "extra rules");

        let options = fixture.options("repo", vec!["docs/extra.md".to_owned()]);
        let loaded = Instructions::discover(&options).load().await;

        assert_eq!(
            loaded.rendered(),
            vec![
                format!(
                    "Instructions from: {}\nroot rules",
                    fixture.path("repo/AGENTS.md").display()
                ),
                format!(
                    "Instructions from: {}\nextra rules",
                    fixture.path("repo/docs/extra.md").display()
                ),
            ]
        );
        assert!(loaded.warnings().is_empty());
    }

    /// Many files at once must all arrive, in order, with the concurrency bound in
    /// place. The bound itself is a constant asserted below; this proves the bounded
    /// stream does not drop or reorder work.
    #[tokio::test]
    async fn a_file_count_above_the_concurrency_bound_loads_completely_and_in_order() {
        let fixture = Fixture::new();
        let count = LOCAL_CONCURRENCY * 3 + 1;
        let mut expected = Vec::new();
        for index in 0..count {
            let name = format!("repo/docs/rule-{index:03}.md");
            fixture.write(&name, &format!("rule {index}"));
            expected.push(format!("rule {index}"));
        }

        let options = fixture.options("repo", vec!["docs/*.md".to_owned()]);
        let loaded = Instructions::discover(&options).load().await;

        assert_eq!(loaded.entries().len(), count);
        let bodies: Vec<&str> = loaded
            .entries()
            .iter()
            .map(zuno_config::InstructionText::content)
            .collect();
        assert_eq!(bodies, expected);
    }

    #[test]
    fn the_concurrency_bounds_and_timeout_are_the_oracle_numbers() {
        assert_eq!(LOCAL_CONCURRENCY, 8);
        assert_eq!(REMOTE_CONCURRENCY, 4);
        assert_eq!(REMOTE_TIMEOUT, Duration::from_secs(5));
    }

    /// The acceptance test for the timeout: a real server that never answers.
    ///
    /// The load must finish at roughly [`REMOTE_TIMEOUT`], keep the reachable
    /// instructions, drop the hanging one, and report it as a warning — never abort.
    #[tokio::test]
    async fn a_hanging_remote_instruction_is_abandoned_at_the_timeout() {
        let fixture = Fixture::new();
        fixture.write("repo/AGENTS.md", "local survives");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hang.md"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fast.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("remote rules"))
            .mount(&server)
            .await;

        let options = fixture.options(
            "repo",
            vec![
                format!("{}/hang.md", server.uri()),
                format!("{}/fast.md", server.uri()),
            ],
        );

        let started = Instant::now();
        let loaded = Instructions::discover(&options).load().await;
        let elapsed = started.elapsed();

        assert!(
            elapsed >= REMOTE_TIMEOUT,
            "abandoned early at {elapsed:?}; the 5s budget was not honoured"
        );
        assert!(
            elapsed < REMOTE_TIMEOUT + Duration::from_secs(4),
            "took {elapsed:?}; the hang was not bounded by the timeout"
        );

        let bodies: Vec<&str> = loaded
            .entries()
            .iter()
            .map(zuno_config::InstructionText::content)
            .collect();
        assert_eq!(bodies, vec!["local survives", "remote rules"]);

        assert_eq!(loaded.warnings().len(), 1, "{:?}", loaded.warnings());
        let warning = &loaded.warnings()[0];
        assert_eq!(warning.kind(), &WarningKind::RemoteTimeout);
        assert!(warning.source().ends_with("/hang.md"));
        assert!(
            warning
                .to_string()
                .contains("did not respond within 5s and was skipped"),
            "{warning}"
        );
    }

    /// Remote concurrency is exactly 4, observed rather than asserted: with every
    /// response delayed well past the sampling point, the number of requests the
    /// server has *received* mid-flight is the in-flight count.
    #[tokio::test]
    async fn remote_fetches_run_exactly_four_at_a_time() {
        let fixture = Fixture::new();
        let server = MockServer::start().await;
        let total = REMOTE_CONCURRENCY * 2;
        for index in 0..total {
            Mock::given(method("GET"))
                .and(path(format!("/rule-{index}.md")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string("rule")
                        .set_delay(Duration::from_secs(30)),
                )
                .mount(&server)
                .await;
        }

        let urls: Vec<String> = (0..total)
            .map(|index| format!("{}/rule-{index}.md", server.uri()))
            .collect();
        let options = fixture.options("repo", urls);
        let discovered = Instructions::discover(&options);

        let handle = tokio::spawn(async move { discovered.load().await });
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let in_flight = server
            .received_requests()
            .await
            .expect("recorded requests")
            .len();
        handle.abort();

        assert_eq!(
            in_flight, REMOTE_CONCURRENCY,
            "expected exactly {REMOTE_CONCURRENCY} in-flight fetches, saw {in_flight}"
        );
    }

    #[tokio::test]
    async fn a_non_success_status_is_skipped_with_a_warning() {
        let fixture = Fixture::new();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let options = fixture.options("repo", vec![format!("{}/missing.md", server.uri())]);
        let loaded = Instructions::discover(&options).load().await;

        assert!(loaded.entries().is_empty());
        assert_eq!(loaded.warnings()[0].kind(), &WarningKind::RemoteStatus(404));
    }

    /// The QA failure scenario: an unreachable HTTPS host must warn and be skipped,
    /// not abort the load. `.invalid` is reserved by RFC 2606 and never resolves.
    #[tokio::test]
    async fn an_unreachable_https_instruction_warns_and_is_skipped() {
        let fixture = Fixture::new();
        fixture.write("repo/AGENTS.md", "local survives");

        let options = fixture.options(
            "repo",
            vec!["https://instructions.invalid/rules.md".to_owned()],
        );
        let loaded = Instructions::discover(&options).load().await;

        assert_eq!(
            loaded
                .entries()
                .iter()
                .map(zuno_config::InstructionText::content)
                .collect::<Vec<_>>(),
            vec!["local survives"],
            "an unreachable URL must not cost the local instructions"
        );
        assert_eq!(loaded.warnings().len(), 1);
        assert!(matches!(
            loaded.warnings()[0].kind(),
            WarningKind::RemoteTransport(_) | WarningKind::RemoteTimeout
        ));
        println!("warning: {}", loaded.warnings()[0]);
    }

    /// The QA happy scenario: reading a nested file appends the parent `AGENTS.md`
    /// **exactly once**, counted rather than merely present.
    #[tokio::test]
    async fn a_nested_read_appends_the_parent_agents_md_exactly_once() {
        let fixture = Fixture::new();
        fixture.write("repo/AGENTS.md", "root rules");
        fixture.write("repo/pkg/AGENTS.md", "pkg rules");
        fixture.write("repo/pkg/src/main.rs", "fn main() {}");
        fixture.write("repo/pkg/src/lib.rs", "pub fn go() {}");

        let options = fixture.options("repo", Vec::new());
        let system = Instructions::discover(&options);
        assert_eq!(
            paths_of(&system, Origin::Project),
            vec![fixture.path("repo/AGENTS.md")],
            "the session is anchored at the repo root, so only the root level is system"
        );

        let already = HashSet::new();
        let mut claims = UpwardClaims::new();

        let first = system.nearby(
            &options,
            &fixture.path("repo/pkg/src/main.rs"),
            &already,
            &mut claims,
        );
        assert_eq!(
            first.iter().map(InstructionPath::path).collect::<Vec<_>>(),
            vec![fixture.path("repo/pkg/AGENTS.md").as_path()]
        );
        let first_count = first.len();

        let second = system.nearby(
            &options,
            &fixture.path("repo/pkg/src/lib.rs"),
            &already,
            &mut claims,
        );
        assert!(second.is_empty(), "second read re-attached: {second:?}");

        let loaded = Instructions::load_nearby(first).await;
        let attached: Vec<&str> = loaded
            .entries()
            .iter()
            .map(zuno_config::InstructionText::source)
            .collect();
        let target = fixture.path("repo/pkg/AGENTS.md");
        let target = target.to_string_lossy();
        let attachments = attached.iter().filter(|source| **source == target).count();
        println!("read 1 (pkg/src/main.rs) attached: {} file(s)", first_count);
        println!(
            "read 2 (pkg/src/lib.rs)  attached: {} file(s)",
            second.len()
        );
        println!("pkg/AGENTS.md appears {attachments} time(s) in the rendered instructions");
        println!("claims recorded for this message: {}", claims.len());
        assert_eq!(
            attachments, 1,
            "pkg/AGENTS.md was attached {attachments} times"
        );
        assert_eq!(claims.len(), 1);
    }

    /// A file already read this session is not attached again, even by a different
    /// message with a fresh claim set — otherwise the same tokens are paid twice.
    #[tokio::test]
    async fn a_previously_read_instruction_is_not_re_attached_by_a_new_message() {
        let fixture = Fixture::new();
        fixture.write("repo/pkg/AGENTS.md", "pkg rules");
        fixture.write("repo/pkg/src/main.rs", "fn main() {}");

        let options = fixture.options("repo", Vec::new());
        let system = Instructions::discover(&options);

        let mut first_message = UpwardClaims::new();
        let first = system.nearby(
            &options,
            &fixture.path("repo/pkg/src/main.rs"),
            &HashSet::new(),
            &mut first_message,
        );
        assert_eq!(first.len(), 1);

        let already: HashSet<PathBuf> = first
            .iter()
            .map(|entry| entry.path().to_path_buf())
            .collect();
        let mut second_message = UpwardClaims::new();
        let second = system.nearby(
            &options,
            &fixture.path("repo/pkg/src/main.rs"),
            &already,
            &mut second_message,
        );
        assert!(second.is_empty(), "{second:?}");
    }

    /// A symlink and its target are one file. Node's `path.resolve` is symlink-blind,
    /// so string de-duplication alone would charge for both.
    #[test]
    fn a_symlinked_spelling_of_one_file_is_de_duplicated() {
        let fixture = Fixture::new();
        let real = fixture.write("repo/AGENTS.md", "root rules");
        let link = fixture.path("repo/linked.md");
        if std::os::unix::fs::symlink(&real, &link).is_err() {
            return;
        }

        let options = fixture.options("repo", vec!["linked.md".to_owned()]);
        let found = Instructions::discover(&options);

        assert_eq!(found.paths().len(), 1, "{:?}", found.paths());
    }

    /// Every warning must reach the log, not only the return value: a caller that
    /// ignores `warnings()` still has to be able to explain a missing instruction.
    #[test]
    fn a_skipped_instruction_is_logged_at_warn_level() {
        use std::io::Write;
        use std::sync::Mutex;

        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);

        impl Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let fixture = Fixture::new();
        let doomed = fixture.write("repo/docs/vanishing.md", "here for now");
        let options = fixture.options("repo", vec!["docs/vanishing.md".to_owned()]);
        let discovered = Instructions::discover(&options);
        assert_eq!(discovered.paths().len(), 1);

        // Discovery saw the file; the read must not. This is the race a long-running
        // agent hits whenever a generated instruction file is rewritten mid-turn.
        std::fs::remove_file(&doomed).expect("remove");

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(Arc::clone(&buffer)))
            .with_ansi(false)
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let loaded = tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async { discovered.load().await })
        });

        assert!(loaded.entries().is_empty());
        assert_eq!(loaded.warnings().len(), 1);
        let logged = String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8");
        assert!(logged.contains("WARN"), "not logged at WARN: {logged}");
        assert!(logged.contains("could not be read"), "{logged}");
        assert!(logged.contains("vanishing.md"), "{logged}");
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// The loader is used from the turn loop (todo 32), which moves it across tasks.
    #[test]
    fn the_public_types_cross_threads() {
        assert_send_sync::<Instructions>();
        assert_send_sync::<InstructionOptions>();
        assert_send_sync::<UpwardClaims>();
        assert_send_sync::<InstructionPath>();
    }
}
