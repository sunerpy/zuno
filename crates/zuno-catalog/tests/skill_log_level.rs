//! The level a duplicate-skill override logs at.
//!
//! # Why this is its own test binary
//!
//! `tracing` caches callsite interest **process-wide**. A sibling test that calls
//! [`zuno_catalog::skill::load`] with no subscriber installed caches
//! `Interest::never` for every callsite inside it, and a later thread-local
//! subscriber then observes nothing — measured: this assertion sees three events
//! when run alone and zero when run beside the fifteen tests in `tests/skill.rs`.
//!
//! So this file installs a **global** subscriber and holds exactly one test.
//! `set_global_default` may be called once per process, which makes that constraint
//! enforce itself: a second test added here would either share this subscriber or
//! fail loudly, never silently observe an empty capture.
//!
//! # Why the level is worth a test at all
//!
//! It is the whole of the fix. The warning was already collected into
//! [`zuno_catalog::skill::Skills::warnings`] before and still is, so an assertion
//! about *recording* would have passed against the storm as well. At `WARN` this one
//! variant emitted 189 lines per launch on a real install
//! (`~/.local/share/zuno/log/opencode.2026-08-20.log`: 202 lines, 189 of them this),
//! burying the twelve variants that are genuine faults.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tracing::Level;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;
use zuno_catalog::skill::{SkillOptions, SkillWarningKind, load};
use zuno_paths::Env;
use zuno_paths::env::{HOME, XDG_CACHE_HOME, XDG_CONFIG_HOME};

#[derive(Debug, Default)]
struct Captured {
    events: Mutex<Vec<(Level, String)>>,
}

impl Captured {
    fn at(&self, level: Level) -> Vec<String> {
        self.events
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|(seen, _)| *seen == level)
            .map(|(_, message)| message.clone())
            .collect()
    }
}

struct Collector(Arc<Captured>);

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Collector {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .events
            .lock()
            .expect("capture lock")
            .push((*event.metadata().level(), message.0));
    }
}

fn skill(dir: &Path, name: &str, description: &str) {
    fs::create_dir_all(dir).expect("skill dir");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
    )
    .expect("skill file");
}

fn options(root: &Path) -> SkillOptions {
    let home = root.join("home");
    let env = Env::from_pairs([
        (HOME, home.to_string_lossy().into_owned()),
        (
            XDG_CONFIG_HOME,
            home.join(".config").to_string_lossy().into_owned(),
        ),
        (
            XDG_CACHE_HOME,
            home.join(".cache").to_string_lossy().into_owned(),
        ),
    ]);
    let project: PathBuf = root.join("proj");
    fs::create_dir_all(&project).expect("project dir");
    SkillOptions::new(project.clone(), Some(project), &env, Vec::new(), Vec::new())
}

#[tokio::test]
async fn an_override_logs_below_warn_while_a_broken_skill_still_warns() {
    let tree = TempDir::new().expect("tempdir");
    let root = tree.path();
    skill(
        &root.join("home/.claude/skills/dupe"),
        "dupe",
        "from claude",
    );
    skill(
        &root.join("home/.agents/skills/dupe"),
        "dupe",
        "from agents",
    );
    let broken = root.join("home/.agents/skills/broken");
    fs::create_dir_all(&broken).expect("broken skill dir");
    fs::write(broken.join("SKILL.md"), "---\ndescription: 5\n---\nbody\n")
        .expect("broken skill file");

    let captured = Arc::new(Captured::default());
    tracing::subscriber::set_global_default(
        Registry::default().with(Collector(Arc::clone(&captured))),
    )
    .expect("this binary holds one test, so nothing else has set a subscriber");

    let skills = load(&options(root)).await;

    let warned = captured.at(Level::WARN);
    assert!(
        !warned
            .iter()
            .any(|line| line.contains("duplicate skill name")),
        "a deliberate precedence override must not reach WARN: {warned:?}"
    );
    assert!(
        warned.iter().any(|line| line.contains("broken")),
        "a skill that will not parse is a fault and must stay at WARN: {warned:?}"
    );
    assert!(
        captured
            .at(Level::DEBUG)
            .iter()
            .any(|line| line.contains("duplicate skill name `dupe`")),
        "the override must remain diagnosable at DEBUG: {:?}",
        captured.at(Level::DEBUG)
    );
    assert!(
        captured
            .at(Level::INFO)
            .iter()
            .any(|line| line.contains("replaced an earlier same-named skill")),
        "the total belongs in one INFO line: {:?}",
        captured.at(Level::INFO)
    );
    assert_eq!(
        skills
            .warnings()
            .iter()
            .filter(|warning| matches!(warning.kind(), SkillWarningKind::DuplicateName { .. }))
            .count(),
        1,
        "lowering the level must not stop the warning being recorded"
    );
}
