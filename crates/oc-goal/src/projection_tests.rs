use super::*;
use crate::spill;
use crate::status::{ModelStatus, SystemStatus};
use crate::store::GoalStore;
use oc_engine::compaction::TranscriptEntry;
use oc_engine::status::SessionRunRegistry;
use oc_llm::event::RequestContentBlock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SESSION: &str = "ses_projection";

struct Fixture {
    store: Arc<GoalStore>,
    projection: GoalProjection,
    _spill: tempfile::TempDir,
    _worktree: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let spill = tempfile::tempdir().expect("create spill directory");
        let worktree = tempfile::tempdir().expect("create worktree");
        let store =
            Arc::new(GoalStore::open_memory(spill.path().to_owned()).expect("open goal store"));
        let projection = GoalProjection::new(Some(worktree.path()), SESSION)
            .expect("a plain session id resolves to a document path");
        Self {
            store,
            projection,
            _spill: spill,
            _worktree: worktree,
        }
    }

    fn create(&self, objective: &str, token_budget: Option<i64>) -> Goal {
        let goal = self
            .store
            .create_goal(SESSION, objective, token_budget)
            .expect("create goal");
        self.projection.write(&goal).expect("render projection");
        goal
    }

    fn goal(&self) -> Goal {
        self.store
            .goal(SESSION)
            .expect("read goal")
            .expect("the session has a goal")
    }

    fn read(&self) -> String {
        std::fs::read_to_string(self.projection.path()).expect("read document")
    }

    fn edit(&self, edit: impl Fn(String) -> String) {
        let edited = edit(self.read());
        std::fs::write(self.projection.path(), edited).expect("save the user's edit");
    }

    fn ingest(&self) -> Ingest {
        self.projection
            .ingest(&self.store)
            .expect("ingest document")
    }
}

fn replace_objective(document: &str, objective: &str) -> String {
    let start = document
        .find(OBJECTIVE_BEGIN)
        .expect("the render opens the objective region")
        + OBJECTIVE_BEGIN.len();
    let end = document
        .rfind(OBJECTIVE_END)
        .expect("the render closes the objective region");
    format!("{}\n{objective}\n{}", &document[..start], &document[end..])
}

fn replace_field(document: &str, field: Field, value: &str) -> String {
    let old = format!("- `{}`: {}", field.key(), field.value_line_of(document));
    let new = format!("- `{}`: {value}", field.key());
    assert!(
        document.contains(&old),
        "{old:?} must be present to be replaced"
    );
    document.replacen(&old, &new, 1)
}

impl Field {
    fn value_line_of(self, document: &str) -> String {
        parse(document)
            .expect("the document parses")
            .fields
            .get(self.key())
            .expect("every field is rendered")
            .clone()
    }

    fn a_different_value(self, goal: &Goal) -> String {
        match self {
            Self::Status => GoalStatus::Complete.as_str().to_owned(),
            Self::TokenBudget => "999999".to_owned(),
            Self::SessionId | Self::GoalId => format!("{}-tampered", self.value(goal)),
            Self::CreatedAtMs
            | Self::UpdatedAtMs
            | Self::TokensUsed
            | Self::TokensRemaining
            | Self::TimeUsedSeconds => "7777".to_owned(),
        }
    }
}

fn tick(document: &str, check: Check, state: bool) -> String {
    let old = format!("- {} `{}`: {}", box_of(!state), check.key(), check.prose());
    let new = format!("- {} `{}`: {}", box_of(state), check.key(), check.prose());
    assert!(document.contains(&old), "{old:?} must be present");
    document.replacen(&old, &new, 1)
}

fn text(entry: &TranscriptEntry) -> &str {
    match entry.message.content.as_slice() {
        [RequestContentBlock::Text { text }] => text,
        other => panic!("expected one text block, got {other:?}"),
    }
}

#[test]
fn the_document_renders_the_objective_status_budget_counters_and_checklist() {
    let fixture = Fixture::new();
    fixture.create("land the markdown projection", Some(100_000));
    let document = fixture.read();

    assert!(document.starts_with("# Goal\n"));
    assert!(document.contains("land the markdown projection"));
    for section in [
        "## Objective",
        "## State",
        "## Budget",
        "## Checklist",
        "## Rejected edits",
    ] {
        assert!(
            document.contains(section),
            "{section} missing from\n{document}"
        );
    }
    let parsed = parse(&document).expect("a freshly rendered document parses");
    assert_eq!(parsed.objective, "land the markdown projection");
    assert_eq!(
        parsed.fields.get("status").map(String::as_str),
        Some("active")
    );
    assert_eq!(
        parsed.fields.get("token_budget").map(String::as_str),
        Some("100000")
    );
    assert_eq!(
        parsed.fields.get("tokens_used").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        parsed.fields.get("tokens_remaining").map(String::as_str),
        Some("100000")
    );
    assert_eq!(
        parsed.fields.get("time_used_seconds").map(String::as_str),
        Some("0")
    );
    assert_eq!(parsed.checks.get("active"), Some(&true));
    assert_eq!(parsed.checks.get("within_budget"), Some(&true));
    assert_eq!(parsed.checks.get("complete"), Some(&false));
    assert!(document.contains("_Nothing has been rejected._"));
}

#[test]
fn an_unbounded_budget_renders_as_none_and_unbounded() {
    let fixture = Fixture::new();
    fixture.create("no ceiling", None);
    let parsed = parse(&fixture.read()).expect("parse");
    assert_eq!(
        parsed.fields.get("token_budget").map(String::as_str),
        Some("none")
    );
    assert_eq!(
        parsed.fields.get("tokens_remaining").map(String::as_str),
        Some("unbounded")
    );
}

#[test]
fn an_edited_objective_is_adopted_and_the_next_turns_injection_carries_it() {
    let fixture = Fixture::new();
    fixture.create("the original objective", Some(10_000));
    fixture.edit(|document| replace_objective(&document, "the objective the human rewrote"));

    let ingest = fixture.ingest();
    assert_eq!(
        ingest.adopted(),
        Some("the objective the human rewrote"),
        "the document is authoritative for objective text"
    );
    assert!(ingest.rejected().is_empty(), "{ingest:?}");
    assert_eq!(fixture.goal().objective, "the objective the human rewrote");

    let runs = SessionRunRegistry::new();
    let continuation = crate::GoalContinuation::new(Arc::clone(&fixture.store), runs);
    let entry = continuation
        .injection(SESSION)
        .expect("render injection")
        .expect("an active goal has an injection");
    assert!(
        text(&entry).contains("the objective the human rewrote"),
        "the next turn must be steered by the edited objective"
    );
    assert!(!text(&entry).contains("the original objective"));
}

#[test]
fn an_edited_status_is_rejected_and_the_rejection_is_written_into_the_document() {
    let fixture = Fixture::new();
    fixture.create("still working", Some(10_000));
    fixture.edit(|document| replace_field(&document, Field::Status, "complete"));

    let ingest = fixture.ingest();
    assert_eq!(
        fixture.goal().status,
        GoalStatus::Active,
        "a hand-edited status must not complete a goal"
    );
    let rejected = ingest.rejected();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(rejected[0].edited, Edited::Field(Field::Status));
    assert_eq!(rejected[0].attempted, "complete");
    assert_eq!(rejected[0].actual, "active");

    let document = fixture.read();
    assert_eq!(
        rejected[0].message(),
        "- `status` was edited to `complete`, but the status is the system's to set, \
         not the document's; the goal database still says `active`."
    );
    assert!(
        document.contains(&rejected[0].message()),
        "the rejection must be visible in the document itself:\n{document}"
    );
    assert!(document.contains("The last turn did not apply these edits."));
    assert!(!document.contains("_Nothing has been rejected._"));
}

#[test]
fn an_edited_counter_is_rejected_the_same_way_as_status() {
    let fixture = Fixture::new();
    fixture.create("counting", Some(10_000));
    fixture
        .store
        .record_usage(SESSION, 1_200, 30)
        .expect("record usage");
    let goal = fixture.goal();
    fixture.projection.write(&goal).expect("re-render");
    fixture.edit(|document| replace_field(&document, Field::TokensUsed, "0"));

    let ingest = fixture.ingest();
    assert_eq!(
        fixture.goal().tokens_used,
        1_200,
        "a counter edit must not land"
    );
    let rejected = ingest.rejected();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(
        rejected[0].message(),
        "- `tokens_used` was edited to `0`, but the counters are the system's to set, \
         not the document's; the goal database still says `1200`."
    );
    assert!(fixture.read().contains(&rejected[0].message()));
}

#[test]
fn a_ticked_completion_checkbox_does_not_complete_the_goal() {
    let fixture = Fixture::new();
    fixture.create("not done yet", Some(10_000));
    fixture.edit(|document| tick(&document, Check::Complete, true));

    let ingest = fixture.ingest();
    assert_eq!(fixture.goal().status, GoalStatus::Active);
    let rejected = ingest.rejected();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(rejected[0].edited, Edited::Check(Check::Complete));
    assert_eq!(
        rejected[0].message(),
        "- the `complete` checklist item was edited to `[x]`, but the checklist is the \
         system's to set, not the document's; the goal database still says `[ ]`."
    );
    assert!(fixture.read().contains(&rejected[0].message()));
}

#[test]
fn every_projected_field_and_checkbox_is_guarded() {
    let mut guarded = 0;
    for field in Field::ALL {
        let fixture = Fixture::new();
        let goal = fixture.create("guard the whole surface", Some(10_000));
        let replacement = field.a_different_value(&goal);
        fixture.edit(|document| replace_field(&document, field, &replacement));
        let ingest = fixture.ingest();
        assert_eq!(
            ingest.rejected().len(),
            1,
            "editing `{}` must be refused exactly once: {ingest:?}",
            field.key()
        );
        assert_eq!(ingest.rejected()[0].edited, Edited::Field(field));
        assert_eq!(ingest.adopted(), None);
        assert_eq!(field.value(&fixture.goal()), field.value(&goal));
        guarded += 1;
    }
    for check in Check::ALL {
        let fixture = Fixture::new();
        let goal = fixture.create("guard the checklist", Some(10_000));
        let state = check.state(&goal);
        fixture.edit(|document| tick(&document, check, !state));
        let ingest = fixture.ingest();
        assert_eq!(
            ingest.rejected().len(),
            1,
            "ticking `{}` must be refused: {ingest:?}",
            check.key()
        );
        assert_eq!(check.state(&fixture.goal()), state);
        guarded += 1;
    }
    assert_eq!(
        guarded,
        Field::ALL.len() + Check::ALL.len(),
        "the matrix must cover every projected field and checkbox"
    );
    assert!(
        guarded >= 12,
        "the matrix would pass vacuously below this floor"
    );
}

#[test]
fn an_objective_edit_and_a_status_edit_in_one_save_are_handled_independently() {
    let fixture = Fixture::new();
    fixture.create("do the thing", Some(10_000));
    fixture.edit(|document| {
        let document = replace_objective(&document, "do the bigger thing");
        replace_field(&document, Field::Status, "complete")
    });

    let ingest = fixture.ingest();
    assert_eq!(ingest.adopted(), Some("do the bigger thing"));
    assert_eq!(ingest.rejected().len(), 1);
    let goal = fixture.goal();
    assert_eq!(goal.objective, "do the bigger thing");
    assert_eq!(goal.status, GoalStatus::Active);
}

#[test]
fn a_render_does_not_trigger_a_re_ingest() {
    let fixture = Fixture::new();
    let goal = fixture.create("write then watch", Some(10_000));
    let before = fixture.read();

    let ingest = fixture.ingest();
    assert_eq!(
        ingest,
        Ingest::OwnRender,
        "our own render must not look like a user edit"
    );
    assert_eq!(fixture.read(), before, "the document must not be rewritten");
    assert_eq!(
        fixture.goal().updated_at_ms,
        goal.updated_at_ms,
        "no SQL write may follow from reading back our own render"
    );

    for _ in 0..5 {
        assert_eq!(
            fixture.ingest(),
            Ingest::OwnRender,
            "the suppression must not be one-shot"
        );
    }
}

#[test]
fn the_re_render_that_follows_an_adopted_edit_is_also_recognised_as_our_own() {
    let fixture = Fixture::new();
    fixture.create("first", Some(10_000));
    fixture.edit(|document| replace_objective(&document, "second"));
    assert_eq!(fixture.ingest().adopted(), Some("second"));
    assert_eq!(
        fixture.ingest(),
        Ingest::OwnRender,
        "the render written by the ingest must not be ingested in turn"
    );
}

#[test]
fn a_save_that_changed_nothing_is_not_an_edit() {
    let fixture = Fixture::new();
    fixture.create("unchanged", Some(10_000));
    let document = fixture.read();
    std::fs::write(fixture.projection.path(), &document).expect("re-save the same bytes");
    assert_eq!(fixture.ingest(), Ingest::OwnRender);
}

#[test]
fn a_hand_edited_objective_over_the_cap_spills_and_the_document_shows_the_pointer() {
    let fixture = Fixture::new();
    fixture.create("short for now", Some(10_000));
    let oversized = "q".repeat(spill::MAX_OBJECTIVE_CHARS + 2_000);
    fixture.edit(|document| replace_objective(&document, &oversized));

    let ingest = fixture.ingest();
    let adopted = ingest.adopted().expect("the objective is adopted");
    assert_ne!(adopted, oversized, "an oversized objective must spill");
    assert!(adopted.starts_with(spill::OBJECTIVE_POINTER_PREFIX));

    let file = fixture
        .store
        .objective_file(&fixture.goal().objective)
        .expect("the stored pointer resolves to its spill file");
    assert_eq!(
        std::fs::read_to_string(&file).expect("read spill"),
        oversized
    );

    let document = fixture.read();
    assert!(
        document.contains(spill::OBJECTIVE_POINTER_PREFIX),
        "the document must show the pointer sentence, not the raw text"
    );
    assert!(
        !document.contains(&oversized),
        "6,000 characters must not be pasted back into the document"
    );
    assert_eq!(
        fixture.ingest(),
        Ingest::OwnRender,
        "the pointer sentence must round trip without looking like a new edit"
    );
}

#[test]
fn an_objective_cleared_to_nothing_is_refused_and_the_document_says_why() {
    let fixture = Fixture::new();
    fixture.create("keep me", Some(10_000));
    fixture.edit(|document| replace_objective(&document, "   "));

    let ingest = fixture.ingest();
    assert_eq!(ingest.adopted(), None);
    let rejected = ingest.rejected();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(
        rejected[0].message(),
        "- `objective` was edited to ``, but a goal objective must not be empty; \
         the goal database still says `keep me`."
    );
    assert_eq!(fixture.goal().objective, "keep me");
    assert!(fixture.read().contains(&rejected[0].message()));
}

#[test]
fn an_untouched_field_is_not_reported_when_sql_moved_on_since_the_render() {
    let fixture = Fixture::new();
    fixture.create("edit me", Some(10_000));
    fixture.edit(|document| replace_objective(&document, "edited while SQL moved on"));
    // Simulates a turn finishing between the render and the save: `tokens_used`,
    // `updated_at_ms` and `tokens_remaining` all change without the user touching
    // them, and a naive comparison against current SQL would report three edits.
    fixture
        .store
        .record_usage(SESSION, 500, 5)
        .expect("record usage");

    let ingest = fixture.ingest();
    assert_eq!(ingest.adopted(), Some("edited while SQL moved on"));
    assert!(
        ingest.rejected().is_empty(),
        "fields the user never touched must not be reported: {:?}",
        ingest.rejected()
    );
    assert_eq!(fixture.goal().tokens_used, 500);
}

#[test]
fn a_deleted_document_is_restored_from_sql() {
    let fixture = Fixture::new();
    fixture.create("survive deletion", Some(10_000));
    std::fs::remove_file(fixture.projection.path()).expect("delete the document");

    assert_eq!(fixture.ingest(), Ingest::Restored);
    assert!(fixture.projection.path().is_file());
    assert_eq!(
        parse(&fixture.read())
            .expect("the restored document parses")
            .objective,
        "survive deletion"
    );
}

#[test]
fn an_unparsable_document_is_preserved_and_the_rebuild_says_where() {
    let fixture = Fixture::new();
    fixture.create("recoverable", Some(10_000));
    std::fs::write(
        fixture.projection.path(),
        "the user replaced this with prose and deleted every marker",
    )
    .expect("mangle the document");

    let ingest = fixture.ingest();
    let Ingest::Salvaged { backup } = &ingest else {
        panic!("expected a salvage, got {ingest:?}");
    };
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        "the user replaced this with prose and deleted every marker",
        "the user's bytes must not be lost"
    );
    let document = fixture.read();
    assert!(parse(&document).is_some(), "the rebuild must parse");
    assert!(
        document.contains(&format!("Your version was kept at `{}`.", backup.display())),
        "the document must say where the salvage went:\n{document}"
    );
    assert_eq!(fixture.goal().objective, "recoverable");
}

#[test]
fn a_session_without_a_goal_leaves_the_document_alone() {
    let fixture = Fixture::new();
    assert_eq!(fixture.ingest(), Ingest::NoGoal);
    assert!(
        !fixture.projection.path().exists(),
        "a session with no goal must not get a document"
    );
}

#[test]
fn every_watch_kind_for_this_document_ingests_and_other_paths_are_ignored() {
    let fixture = Fixture::new();
    fixture.create("route events", Some(10_000));

    for kind in [ChangeKind::Add, ChangeKind::Change] {
        fixture.edit(|document| replace_objective(&document, &format!("via {}", kind.as_str())));
        let ingest = fixture
            .projection
            .ingest_event(
                &fixture.store,
                &FileEvent::new(fixture.projection.path(), kind),
            )
            .expect("ingest event");
        assert_eq!(
            ingest.adopted(),
            Some(format!("via {}", kind.as_str()).as_str())
        );
    }

    std::fs::remove_file(fixture.projection.path()).expect("delete");
    let ingest = fixture
        .projection
        .ingest_event(
            &fixture.store,
            &FileEvent::new(fixture.projection.path(), ChangeKind::Unlink),
        )
        .expect("ingest unlink");
    assert_eq!(ingest, Ingest::Restored);

    let elsewhere = fixture.projection.path().with_file_name("other.md");
    assert_eq!(
        fixture
            .projection
            .ingest_event(
                &fixture.store,
                &FileEvent::new(elsewhere, ChangeKind::Change)
            )
            .expect("ingest foreign event"),
        Ingest::NoGoal,
        "an event for another path must not read this document"
    );
}

#[test]
fn an_objective_containing_the_closing_marker_round_trips() {
    let fixture = Fixture::new();
    fixture.create("plain", Some(10_000));
    let tricky = format!("keep going {OBJECTIVE_END} and then stop");
    fixture.edit(|document| replace_objective(&document, &tricky));

    assert_eq!(fixture.ingest().adopted(), Some(tricky.as_str()));
    assert_eq!(fixture.goal().objective, tricky);
    assert_eq!(
        fixture.ingest(),
        Ingest::OwnRender,
        "the marker inside the objective must not truncate the region on the way back"
    );
}

#[test]
fn a_system_status_change_is_projected_and_survives_a_round_trip() {
    let fixture = Fixture::new();
    fixture.create("pausable", Some(10_000));
    fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Paused)
        .expect("pause");
    let goal = fixture.goal();
    fixture.projection.write(&goal).expect("re-render");

    let parsed = parse(&fixture.read()).expect("parse");
    assert_eq!(
        parsed.fields.get("status").map(String::as_str),
        Some("paused")
    );
    assert_eq!(parsed.checks.get("active"), Some(&false));
    assert_eq!(fixture.ingest(), Ingest::OwnRender);
}

#[test]
fn a_completed_goal_projects_a_ticked_completion_box() {
    let fixture = Fixture::new();
    fixture.create("finishable", Some(10_000));
    fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect("complete");
    let goal = fixture.goal();
    fixture.projection.write(&goal).expect("re-render");

    let parsed = parse(&fixture.read()).expect("parse");
    assert_eq!(parsed.checks.get("complete"), Some(&true));
    assert_eq!(parsed.checks.get("active"), Some(&false));
}

#[test]
fn the_document_path_parallels_the_oracles_plans_convention() {
    let worktree = Path::new("/tmp/repo");
    assert_eq!(
        document_path(Some(worktree), "ses_abc").expect("resolve"),
        worktree.join(".zuno").join("goal").join("ses_abc.md")
    );
    let global = document_path(None, "ses_abc").expect("resolve");
    assert!(global.ends_with("goal/ses_abc.md"));
    assert!(!global.starts_with(worktree));
}

#[test]
fn a_session_id_that_would_escape_the_directory_is_refused() {
    for hostile in [
        "../../etc/passwd",
        "a/b",
        "..",
        ".",
        "",
        "with/slash",
        "/absolute",
    ] {
        assert_eq!(
            document_path(Some(Path::new("/tmp/repo")), hostile),
            None,
            "{hostile:?} must not resolve to a document path"
        );
        assert!(GoalProjection::new(Some(Path::new("/tmp/repo")), hostile).is_none());
    }
}

#[test]
fn the_gitignore_snippet_names_the_projection_directory() {
    assert!(GITIGNORE_SNIPPET.contains(".zuno/goal/"));
    assert!(
        GITIGNORE_SNIPPET.lines().any(|line| line == ".zuno/goal/"),
        "the pattern must be a line on its own so the snippet can be pasted verbatim"
    );
    assert!(
        GITIGNORE_SNIPPET
            .lines()
            .filter(|line| line.starts_with('#'))
            .count()
            >= 1,
        "the snippet must explain itself to whoever finds it in a .gitignore"
    );
    assert!(GITIGNORE_SNIPPET.ends_with('\n'));
}

#[test]
fn parse_refuses_a_document_missing_any_projected_key() {
    let fixture = Fixture::new();
    fixture.create("complete document", Some(10_000));
    let document = fixture.read();
    assert!(parse(&document).is_some());

    for field in Field::ALL {
        let line = format!("- `{}`: {}", field.key(), field.value_line_of(&document));
        let without = document.replacen(&line, "", 1);
        assert!(
            parse(&without).is_none(),
            "a document missing `{}` must not parse",
            field.key()
        );
    }
    for check in Check::ALL {
        let goal = fixture.goal();
        let line = format!(
            "- {} `{}`: {}",
            check.rendered(&goal),
            check.key(),
            check.prose()
        );
        let without = document.replacen(&line, "", 1);
        assert!(
            parse(&without).is_none(),
            "a document missing the `{}` checkbox must not parse",
            check.key()
        );
    }
    assert!(parse(&document.replacen(OBJECTIVE_BEGIN, "", 1)).is_none());
    assert!(parse(&document.replacen(OBJECTIVE_END, "", 1)).is_none());
    assert!(parse("").is_none());
}

#[test]
fn the_render_is_deterministic_so_two_renders_of_one_state_are_identical() {
    let fixture = Fixture::new();
    let goal = fixture.create("determinism", Some(10_000));
    assert_eq!(
        render(&goal, &Notes::default()),
        render(&goal, &Notes::default())
    );
    let notes = Notes {
        rejected: vec![RejectedEdit {
            edited: Edited::Field(Field::Status),
            attempted: "complete".to_owned(),
            actual: "active".to_owned(),
            refusal: Field::Status.owner(),
        }],
        salvaged: None,
    };
    assert_eq!(render(&goal, &notes), render(&goal, &notes));
    assert_ne!(render(&goal, &notes), render(&goal, &Notes::default()));
}

/// The acceptance criterion: a concurrent reader must never observe a partial
/// document across 1,000 renders.
///
/// The reader fully parses and validates on every read rather than checking the
/// file is non-empty, because a length check passes against a file that was
/// truncated to zero and then half written — which is exactly what a non-atomic
/// write looks like from here.
#[test]
fn the_render_is_atomic_under_a_concurrent_reader() {
    const RENDERS: i64 = 1_000;
    const DEADLINE: Duration = Duration::from_secs(60);

    let fixture = Fixture::new();
    let objective = "z".repeat(3_000);
    let goal = fixture.create(&objective, Some(10_000_000));
    let path = fixture.projection.path().to_path_buf();

    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let reader = {
        let done = Arc::clone(&done);
        let reads = Arc::clone(&reads);
        let path = path.clone();
        let objective = objective.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut failures = Vec::new();
            while !done.load(Ordering::Acquire) && started.elapsed() < DEADLINE {
                let raw = match std::fs::read_to_string(&path) {
                    Ok(raw) => raw,
                    Err(error) => {
                        failures.push(format!("read failed: {error}"));
                        continue;
                    }
                };
                reads.fetch_add(1, Ordering::Relaxed);
                let Some(document) = parse(&raw) else {
                    failures.push(format!(
                        "a partial document was observed: {} bytes, starts {:?}",
                        raw.len(),
                        raw.chars().take(40).collect::<String>()
                    ));
                    continue;
                };
                if document.objective != objective {
                    failures.push(format!(
                        "objective was {} characters, expected {}",
                        document.objective.chars().count(),
                        objective.chars().count()
                    ));
                }
                if document
                    .fields
                    .get("tokens_used")
                    .is_none_or(|used| used.parse::<i64>().is_err())
                {
                    failures.push(format!(
                        "tokens_used was {:?}",
                        document.fields.get("tokens_used")
                    ));
                }
            }
            failures
        })
    };

    for tokens_used in 0..RENDERS {
        let mut goal = goal.clone();
        goal.tokens_used = tokens_used;
        goal.updated_at_ms = goal.created_at_ms + tokens_used;
        fixture.projection.write(&goal).expect("render");
    }
    done.store(true, Ordering::Release);
    let failures = reader.join().expect("the reader thread must not panic");

    assert!(
        failures.is_empty(),
        "{} of {} reads observed a partial or wrong document; first three: {:?}",
        failures.len(),
        reads.load(Ordering::Relaxed),
        &failures[..failures.len().min(3)]
    );
    let observed = reads.load(Ordering::Relaxed);
    assert!(
        observed >= 50,
        "the reader only completed {observed} reads, so it cannot have covered \
         {RENDERS} renders and would have passed vacuously"
    );
    let stragglers: Vec<String> = path
        .parent()
        .expect("the document has a directory")
        .read_dir()
        .expect("read the goal directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(
        stragglers.is_empty(),
        "every temporary file must have been renamed away, found {stragglers:?}"
    );
}

fn write_legacy_document(fixture: &Fixture, body: &str) {
    let legacy = fixture
        .projection
        .path()
        .parent()
        .expect("the document has a directory")
        .parent()
        .expect("the goal directory sits inside the project directory")
        .parent()
        .expect("the project directory sits inside the worktree")
        .join(oc_paths::LEGACY_PROJECT_DIRECTORY)
        .join(GOAL_DIRECTORY);
    std::fs::create_dir_all(&legacy).expect("create the legacy goal directory");
    let name = fixture
        .projection
        .path()
        .file_name()
        .expect("the document has a file name")
        .to_owned();
    std::fs::write(legacy.join(name), body).expect("write the legacy document");
}

#[test]
fn a_goal_document_left_at_the_pre_rename_path_is_reported_rather_than_silently_restored() {
    let fixture = Fixture::new();
    fixture.create("ship the thing", None);
    std::fs::remove_file(fixture.projection.path()).expect("remove the migrated document");
    write_legacy_document(&fixture, "a human edit nobody would see\n");

    let error = fixture
        .projection
        .ingest(&fixture.store)
        .expect_err("the legacy document must be reported");
    let message = error.to_string();
    assert!(
        message.contains(oc_paths::LEGACY_PROJECT_DIRECTORY),
        "the diagnostic must name the legacy directory, got: {message}"
    );
}

#[test]
fn a_goal_document_with_no_legacy_counterpart_is_still_restored() {
    let fixture = Fixture::new();
    fixture.create("ship the thing", None);
    std::fs::remove_file(fixture.projection.path()).expect("remove the migrated document");
    assert!(matches!(fixture.ingest(), Ingest::Restored));
}

#[test]
fn a_goal_document_present_in_both_locations_ingests_the_new_one() {
    let fixture = Fixture::new();
    fixture.create("ship the thing", None);
    write_legacy_document(&fixture, "a stale pre-rename projection\n");
    assert!(matches!(fixture.ingest(), Ingest::OwnRender));
}
