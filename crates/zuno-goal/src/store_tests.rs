use super::*;
use crate::spill::{MAX_OBJECTIVE_CHARS, OBJECTIVE_FILE_NAME};
use tempfile::TempDir;

/// A store plus the temporary directory its spilled objectives live in.
///
/// The spill directory is held here, not dropped at the end of the helper, or
/// every spill test would be writing into a path that had already been removed.
struct Fixture {
    store: GoalStore,
    spill: TempDir,
    database: Option<TempDir>,
}

impl Fixture {
    /// An isolated in-memory store. `zuno-db`'s pool names each in-memory database
    /// uniquely, so these are independent and the suite can run in parallel.
    fn in_memory() -> Self {
        let spill = tempfile::tempdir().expect("create spill directory");
        let store =
            GoalStore::open_memory(spill.path().to_path_buf()).expect("open in-memory goal store");
        Self {
            store,
            spill,
            database: None,
        }
    }

    /// A store on a real file, so it can be closed and reopened.
    fn on_disk() -> Self {
        let spill = tempfile::tempdir().expect("create spill directory");
        let database = tempfile::tempdir().expect("create database directory");
        let store = GoalStore::open_at(
            &database.path().join("goal-test.db"),
            spill.path().to_owned(),
        )
        .expect("open goal store on disk");
        Self {
            store,
            spill,
            database: Some(database),
        }
    }

    fn database_path(&self) -> PathBuf {
        self.database
            .as_ref()
            .expect("an on-disk fixture has a database directory")
            .path()
            .join("goal-test.db")
    }

    /// Rewrite the stored `created_at_ms` of a session's goal.
    ///
    /// Goals stamp their creation from the wall clock while these tests record
    /// receipts at small synthetic times, so a fixture that needs "the check ran
    /// after this goal was proposed" has to move the goal rather than the clock. A
    /// real database holds exactly this shape: a goal created long before the
    /// receipts that close it.
    fn backdate_goal(&self, session_id: &str, created_at_ms: i64) {
        let connection = self.store.pool().get().expect("check out connection");
        let updated = connection
            .execute(
                "UPDATE goal SET created_at_ms = ?2 WHERE session_id = ?1",
                params![session_id, created_at_ms],
            )
            .expect("backdate the goal");
        assert_eq!(updated, 1, "the fixture has a goal to backdate");
    }

    fn materialize_session(&self, session_id: &str) {
        let connection = self.store.pool().get().expect("check out connection");
        connection
            .execute(
                "INSERT OR IGNORE INTO project \
                 (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
                  time_updated,time_initialized,sandboxes,commands) \
                 VALUES ('goal-fixture','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
                [],
            )
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO session \
                 (id,project_id,slug,directory,title,version,time_created,time_updated) \
                 VALUES (?1,'goal-fixture',?1,'/tmp',?1,'test',1,1)",
                params![session_id],
            )
            .expect("insert session");
    }

    /// Close every connection and open the same file again.
    fn restart(self) -> Self {
        let Self {
            store,
            spill,
            database,
        } = self;
        let path = database
            .as_ref()
            .expect("an on-disk fixture has a database directory")
            .path()
            .join("goal-test.db");
        drop(store);
        let store =
            GoalStore::open_at(&path, spill.path().to_owned()).expect("reopen goal store on disk");
        Self {
            store,
            spill,
            database,
        }
    }

    /// The stored status read with a statement of the test's own, so a claim about
    /// the column is not merely a claim about this crate's row mapping.
    fn raw_status(&self, session_id: &str) -> String {
        let connection = self.store.pool().get().expect("check out a connection");
        connection
            .query_row(
                "SELECT status FROM goal WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .expect("read the stored status")
    }

    fn raw_counters(&self, session_id: &str) -> (i64, i64) {
        let connection = self.store.pool().get().expect("check out a connection");
        connection
            .query_row(
                "SELECT tokens_used, time_used_seconds FROM goal WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read the stored counters")
    }

    fn status(&self, session_id: &str) -> GoalStatus {
        self.goal(session_id).status
    }

    fn goal(&self, session_id: &str) -> Goal {
        self.store
            .goal(session_id)
            .expect("read the goal")
            .expect("the session has a goal")
    }
}

/// Whether the seeded goal's counters are over its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Budget {
    /// No budget at all, so the budget guards cannot fire.
    Unset,
    /// A budget that `tokens_used` has already passed.
    Exceeded,
}

impl Budget {
    fn is_exceeded(self) -> bool {
        self == Self::Exceeded
    }
}

const SESSION: &str = "ses_goal_matrix";
const BUDGET: i64 = 100;
const OVERSPEND: i64 = 150;

/// Drive a fresh goal into `status`, with counters over budget when asked.
///
/// The order matters for [`Budget::Exceeded`]: the status is set *before* the
/// overspend, because the flip only applies to an `active` goal — which is also
/// why `active` is not a reachable seed under an exceeded budget.
fn seed(fixture: &Fixture, status: GoalStatus, budget: Budget) {
    let token_budget = match budget {
        Budget::Unset => None,
        Budget::Exceeded => Some(BUDGET),
    };
    fixture
        .store
        .replace_goal_as_system(SESSION, "seeded objective", token_budget)
        .expect("seed a goal");
    match status {
        GoalStatus::Active => {}
        GoalStatus::BudgetLimited if budget.is_exceeded() => {}
        GoalStatus::Blocked | GoalStatus::Complete => {
            let model = ModelStatus::from_status(status).expect("a model-owned status");
            fixture
                .store
                .update_status_as_model(SESSION, model)
                .expect("seed the model status");
        }
        GoalStatus::Paused
        | GoalStatus::UsageLimited
        | GoalStatus::BudgetLimited
        | GoalStatus::Cancelled => {
            let system = SystemStatus::from_status(status).expect("a system-owned status");
            fixture
                .store
                .set_status_as_system(SESSION, system)
                .expect("seed the system status");
        }
    }
    if budget.is_exceeded() {
        fixture
            .store
            .record_usage(SESSION, OVERSPEND, 1, true)
            .expect("push the counters over budget");
    }
    assert_eq!(
        fixture.status(SESSION),
        status,
        "seeding {status} under {budget:?} did not land"
    );
}

/// What the two SQL guards say a transition resolves to.
fn expected_status(from: GoalStatus, to: GoalStatus, over_budget: bool) -> GoalStatus {
    let cancelled_is_kept = from == GoalStatus::Cancelled;
    let budget_terminal_status_is_kept =
        from == GoalStatus::BudgetLimited && matches!(to, GoalStatus::Blocked | GoalStatus::Paused);
    let reactivation_is_still_over_budget = to == GoalStatus::Active && over_budget;
    if cancelled_is_kept {
        GoalStatus::Cancelled
    } else if budget_terminal_status_is_kept || reactivation_is_still_over_budget {
        GoalStatus::BudgetLimited
    } else {
        to
    }
}

/// One row of the transition matrix, for the evidence dump.
fn matrix(budget: Budget, seeds: &[GoalStatus]) -> Vec<String> {
    let mut rows = Vec::new();
    for from in seeds.iter().copied() {
        for to in GoalStatus::ALL {
            let model = Fixture::in_memory();
            seed(&model, from, budget);
            let row = match ModelStatus::parse(to.as_str()) {
                Err(error) => {
                    assert!(
                        matches!(&error, GoalError::StatusNotModelOwned { requested, .. }
                            if *requested == to),
                        "the model reached {to} from {from}: {error:?}"
                    );
                    assert!(error.to_string().contains("`blocked` or `complete`"));
                    assert_eq!(
                        model.status(SESSION),
                        from,
                        "a refused model write must not touch the row"
                    );
                    format!("{from:?} -> {to:?} | model  | REFUSED ({error})")
                }
                Ok(status) => {
                    let updated = model
                        .store
                        .update_status_as_model(SESSION, status)
                        .expect("the model write should reach SQL")
                        .expect("the session has a goal");
                    let expected = expected_status(from, to, budget.is_exceeded());
                    assert_eq!(updated.status, expected, "model {from:?} -> {to:?}");
                    assert_eq!(model.raw_status(SESSION), expected.as_str());
                    format!("{from:?} -> {to:?} | model  | allowed, stored {expected:?}")
                }
            };
            rows.push(row);

            let system = Fixture::in_memory();
            seed(&system, from, budget);
            let row = match SystemStatus::from_status(to) {
                None => {
                    assert!(
                        to.owner().is_model(),
                        "{to} is unreachable from both scopes"
                    );
                    format!("{from:?} -> {to:?} | system | REFUSED (model-owned status)")
                }
                Some(status) => {
                    let updated = system
                        .store
                        .set_status_as_system(SESSION, status)
                        .expect("the system write should reach SQL")
                        .expect("the session has a goal");
                    let expected = expected_status(from, to, budget.is_exceeded());
                    assert_eq!(updated.status, expected, "system {from:?} -> {to:?}");
                    assert_eq!(system.raw_status(SESSION), expected.as_str());
                    format!("{from:?} -> {to:?} | system | allowed, stored {expected:?}")
                }
            };
            rows.push(row);
        }
    }
    rows
}

#[test]
fn every_transition_from_both_scopes_obeys_the_ownership_split() {
    let rows = matrix(Budget::Unset, &GoalStatus::ALL);
    assert_eq!(
        rows.len(),
        GoalStatus::ALL.len() * GoalStatus::ALL.len() * 2
    );
    for row in &rows {
        println!("{row}");
    }
    let refused = rows.iter().filter(|row| row.contains("REFUSED")).count();
    assert_eq!(
        refused,
        GoalStatus::ALL.len() * 5 + GoalStatus::ALL.len() * 2,
        "each start state refuses five system-owned statuses to the model \
         and two model-owned statuses to the system"
    );
}

#[test]
fn the_same_matrix_over_a_spent_budget_never_lets_either_scope_clear_the_limit() {
    let seeds = [
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::UsageLimited,
        GoalStatus::BudgetLimited,
        GoalStatus::Complete,
        GoalStatus::Cancelled,
    ];
    let rows = matrix(Budget::Exceeded, &seeds);
    assert_eq!(rows.len(), seeds.len() * GoalStatus::ALL.len() * 2);
    for row in &rows {
        println!("{row}");
    }
    for row in &rows {
        if row.contains("-> Active") && !row.contains("REFUSED") {
            assert!(
                row.contains("stored BudgetLimited") || row.contains("stored Cancelled"),
                "an over-budget goal was reactivated: {row}"
            );
        }
    }
}

#[test]
fn the_model_attempting_paused_is_refused_by_name_and_the_goal_is_untouched() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal(SESSION, "keep going", Some(1_000))
        .expect("create the goal");

    let refusal = ModelStatus::parse("paused").expect_err("`paused` is the system's to set");
    assert_eq!(
        refusal.to_string(),
        "goal status `paused` is the system's to set, not the model's; \
         the model may set only `blocked` or `complete`"
    );
    assert!(refusal.is_model_refusal());
    assert_eq!(fixture.goal(SESSION), created);
    assert_eq!(fixture.raw_status(SESSION), "active");
}

#[test]
fn human_input_and_permission_survive_restart_and_resume_once() {
    const SESSION_ID: &str = "ses_human_restart";
    let fixture = Fixture::on_disk();
    fixture.materialize_session(SESSION_ID);
    let goal = fixture
        .store
        .create_goal(SESSION_ID, "finish after human decisions", None)
        .expect("create goal");
    let request = fixture
        .store
        .request_human_input_at(
            SESSION_ID,
            goal.revision,
            "que_restart".to_owned(),
            serde_json::json!({
                "source": "goal_request_input",
                "questions": [{
                    "question": "Which release channel?",
                    "header": "Channel",
                    "options": [],
                    "multiple": false,
                    "custom": true
                }]
            }),
            GoalHumanRequestOrigin {
                message_id: Some("msg_question".to_owned()),
                call_id: Some("call_question".to_owned()),
            },
            1_000,
        )
        .expect("persist question and pause");
    assert_eq!(request.goal_id.as_deref(), Some(goal.goal_id.as_str()));
    assert_eq!(
        fixture
            .store
            .pause_state(SESSION_ID)
            .expect("read pause")
            .expect("paused goal")
            .reason,
        GoalPauseReason::HumanInput
    );

    let fixture = fixture.restart();
    assert_eq!(
        fixture
            .store
            .human_requests()
            .get("que_restart")
            .expect("read request")
            .expect("request")
            .state,
        zuno_db::human_request::HumanRequestState::Pending
    );
    fixture
        .store
        .human_requests()
        .answer_with_input(
            "que_restart",
            serde_json::json!({"answers": [["canary"]]}),
            2_000,
        )
        .expect("answer question")
        .expect("pending question");
    let resumed = fixture
        .store
        .resume_for_work(SESSION_ID)
        .expect("resume work")
        .expect("goal");
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(
        fixture
            .store
            .resume_for_work(SESSION_ID)
            .expect("repeat resume")
            .expect("goal")
            .revision,
        resumed.revision,
        "Start Work must consume the pause only once"
    );

    let permission = fixture
        .store
        .request_permission(
            SESSION_ID,
            "per_restart".to_owned(),
            serde_json::json!({
                "id": "per_restart",
                "sessionId": SESSION_ID,
                "permission": "shell",
                "patterns": ["git push"],
                "metadata": {},
                "always": []
            }),
            Some("msg_permission".to_owned()),
            Some("call_permission".to_owned()),
        )
        .expect("persist permission")
        .expect("active goal pauses");
    assert_eq!(permission.goal_id.as_deref(), Some(goal.goal_id.as_str()));
    let fixture = fixture.restart();
    fixture
        .store
        .human_requests()
        .answer_with_input("per_restart", serde_json::json!({"reply": "reject"}), 3_000)
        .expect("answer permission")
        .expect("pending permission");
    let resumed = fixture
        .store
        .resume_for_work(SESSION_ID)
        .expect("resume after permission")
        .expect("goal");
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(
        fixture
            .store
            .human_requests()
            .pending(Some(SESSION_ID))
            .expect("list requests"),
        Vec::new()
    );
    assert_eq!(
        zuno_db::inbox::SessionInbox::new(Arc::clone(&fixture.store.pool))
            .pending(SESSION_ID)
            .expect("read admitted answers")
            .len(),
        2
    );
}

#[test]
fn plan_mode_survives_restart_and_start_work_is_idempotent() {
    const SESSION_ID: &str = "ses_plan_restart";
    let fixture = Fixture::on_disk();
    let goal = fixture
        .store
        .create_goal(SESSION_ID, "plan then implement", None)
        .expect("create goal");
    let paused = fixture
        .store
        .enter_plan_mode(SESSION_ID)
        .expect("enter plan")
        .expect("goal");
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(paused.revision, goal.revision + 1);
    assert_eq!(
        fixture
            .store
            .enter_plan_mode(SESSION_ID)
            .expect("repeat plan")
            .expect("goal")
            .revision,
        paused.revision
    );

    let fixture = fixture.restart();
    assert_eq!(
        fixture
            .store
            .pause_state(SESSION_ID)
            .expect("read pause")
            .expect("pause")
            .reason,
        GoalPauseReason::PlanMode
    );
    assert_eq!(
        fixture
            .store
            .enter_plan_mode(SESSION_ID)
            .expect("reopen plan")
            .expect("goal")
            .revision,
        paused.revision
    );
    let active = fixture
        .store
        .resume_for_work(SESSION_ID)
        .expect("start work")
        .expect("goal");
    assert_eq!(active.status, GoalStatus::Active);
    assert_eq!(active.revision, paused.revision + 1);
    assert_eq!(
        fixture
            .store
            .resume_for_work(SESSION_ID)
            .expect("repeat start work")
            .expect("goal")
            .revision,
        active.revision
    );
    assert_eq!(
        fixture.store.pause_state(SESSION_ID).expect("read pause"),
        None
    );
}

#[test]
fn a_five_thousand_character_objective_spills_and_the_column_holds_the_pointer() {
    let fixture = Fixture::in_memory();
    let objective = "north star ".repeat(500);
    assert_eq!(objective.chars().count(), 5_500);

    let goal = fixture
        .store
        .create_goal(SESSION, &objective, None)
        .expect("create the goal");
    assert!(goal.objective.chars().count() <= MAX_OBJECTIVE_CHARS);

    let path = fixture
        .store
        .objective_file(&goal.objective)
        .expect("the stored objective must be a resolvable pointer");
    assert_eq!(path.file_name().expect("a file name"), OBJECTIVE_FILE_NAME);
    assert_eq!(
        goal.objective,
        format!(
            "Read the full goal objective at {} before continuing.",
            path.display()
        )
    );
    println!(
        "submitted objective: {} characters",
        objective.chars().count()
    );
    println!(
        "stored objective:    {} characters",
        goal.objective.chars().count()
    );
    println!("stored objective:    {}", goal.objective);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the spilled objective"),
        objective.trim()
    );
    assert_eq!(fixture.goal(SESSION).objective, goal.objective);
    assert!(path.starts_with(fixture.spill.path()));
}

#[test]
fn exceeding_the_token_budget_flips_the_status_inside_the_recording_statement() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "stay within budget", Some(BUDGET))
        .expect("create the goal");
    assert_eq!(fixture.raw_status(SESSION), "active");

    let under = fixture
        .store
        .record_usage(SESSION, BUDGET - 1, 5, true)
        .expect("record usage")
        .expect("the session has a goal");
    assert_eq!(under.status, GoalStatus::Active);
    assert_eq!(fixture.raw_status(SESSION), "active");
    assert_eq!(under.tokens_remaining(), Some(1));

    let over = fixture
        .store
        .record_usage(SESSION, 1, 5, true)
        .expect("record usage")
        .expect("the session has a goal");
    assert_eq!(
        over.status,
        GoalStatus::BudgetLimited,
        "the statement that incremented the counter must have flipped the status"
    );
    assert_eq!(
        fixture.raw_status(SESSION),
        "budget_limited",
        "the flip has to be in the column, not in the row mapping"
    );
    assert_eq!(fixture.raw_counters(SESSION), (BUDGET, 10));
    assert!(over.is_over_budget());
    assert_eq!(over.tokens_remaining(), Some(0));
    assert!(over.status.is_terminal());
}

#[test]
fn a_budget_of_zero_is_never_observed_as_an_active_goal() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal(SESSION, "spend nothing", Some(0))
        .expect("create the goal");
    assert_eq!(goal.status, GoalStatus::BudgetLimited);
    assert_eq!(fixture.raw_status(SESSION), "budget_limited");
    assert_eq!(goal.tokens_used, 0);
}

#[test]
fn create_goal_over_an_active_goal_is_refused_and_changes_nothing() {
    let fixture = Fixture::in_memory();
    let original = fixture
        .store
        .create_goal(SESSION, "the first objective", Some(BUDGET))
        .expect("create the goal");

    let error = fixture
        .store
        .create_goal(SESSION, "the second objective", None)
        .expect_err("an active goal must not be replaced");
    assert!(
        matches!(&error, GoalError::GoalNotReplaceable { session_id, status }
            if session_id == SESSION && *status == GoalStatus::Active),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "session ses_goal_matrix already has a goal with status `active`; \
         create_goal may replace a goal only once it is `complete` or `cancelled`"
    );
    assert!(error.is_model_refusal());
    assert_eq!(fixture.goal(SESSION), original);
}

#[test]
fn create_goal_is_refused_over_every_status_except_complete_or_cancelled() {
    for status in GoalStatus::ALL {
        let fixture = Fixture::in_memory();
        seed(&fixture, status, Budget::Unset);
        let before = fixture.goal(SESSION);
        let outcome = fixture.store.create_goal(SESSION, "a new objective", None);
        if matches!(status, GoalStatus::Complete | GoalStatus::Cancelled) {
            let replaced = outcome.expect("a terminal goal may be replaced");
            assert_eq!(replaced.objective, "a new objective");
            assert_eq!(replaced.status, GoalStatus::Active);
            assert_ne!(replaced.goal_id, before.goal_id);
        } else {
            let error = outcome.expect_err("an unfinished goal must not be replaced");
            assert!(
                matches!(&error, GoalError::GoalNotReplaceable { status: blocking, .. }
                    if *blocking == status),
                "{status}: {error:?}"
            );
            assert_eq!(fixture.goal(SESSION), before);
        }
    }
}

#[test]
fn replacing_a_complete_goal_resets_both_counters_and_mints_a_new_goal_id() {
    let fixture = Fixture::in_memory();
    let first = fixture
        .store
        .create_goal(SESSION, "the first objective", Some(1_000))
        .expect("create the goal");
    fixture
        .store
        .record_usage(SESSION, 400, 90, true)
        .expect("record usage");
    fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect("complete the goal");

    let second = fixture
        .store
        .create_goal(SESSION, "the second objective", Some(50))
        .expect("a complete goal may be replaced");
    assert_ne!(second.goal_id, first.goal_id);
    assert_eq!(second.tokens_used, 0);
    assert_eq!(second.time_used_seconds, 0);
    assert_eq!(second.token_budget, Some(50));
    assert_eq!(second.status, GoalStatus::Active);
    assert_eq!(fixture.raw_counters(SESSION), (0, 0));
}

#[test]
fn a_goal_survives_a_process_restart_with_its_counters_intact() {
    let fixture = Fixture::on_disk();
    let created = fixture
        .store
        .create_goal(SESSION, "outlive the process", Some(1_000))
        .expect("create the goal");
    fixture
        .store
        .record_usage(SESSION, 250, 61, true)
        .expect("record usage");
    fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Paused)
        .expect("pause the goal");
    let before = fixture.goal(SESSION);
    let path = fixture.database_path();
    assert!(path.exists(), "the goal must be on disk to survive");

    let fixture = fixture.restart();

    let after = fixture.goal(SESSION);
    println!("before restart: {before:?}");
    println!("after restart:  {after:?}");
    assert_eq!(after, before);
    assert_eq!(after.goal_id, created.goal_id);
    assert_eq!(after.created_at_ms, created.created_at_ms);
    assert_eq!(after.tokens_used, 250);
    assert_eq!(after.time_used_seconds, 61);
    assert_eq!(after.status, GoalStatus::Paused);
    assert_eq!(fixture.raw_counters(SESSION), (250, 61));
}

#[test]
fn a_spilled_objective_still_resolves_after_a_restart() {
    let fixture = Fixture::on_disk();
    let objective = "carry the whole plan ".repeat(300);
    assert!(objective.chars().count() > MAX_OBJECTIVE_CHARS);
    let created = fixture
        .store
        .create_goal(SESSION, &objective, None)
        .expect("create the goal");

    let fixture = fixture.restart();

    let after = fixture.goal(SESSION);
    assert_eq!(after.objective, created.objective);
    let path = fixture
        .store
        .objective_file(&after.objective)
        .expect("the pointer must still resolve");
    assert_eq!(
        std::fs::read_to_string(path).expect("read the spilled objective"),
        objective.trim()
    );
}

#[test]
fn only_the_system_replacement_gets_past_a_blocked_goal() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "the stuck objective", None)
        .expect("create the goal");
    fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Blocked)
        .expect("block the goal");

    assert!(
        fixture
            .store
            .create_goal(SESSION, "try again", None)
            .is_err(),
        "the model must not be able to abandon its own blocked goal"
    );

    let replaced = fixture
        .store
        .replace_goal_as_system(SESSION, "the user's new objective", Some(10))
        .expect("the user may always replace a goal");
    assert_eq!(replaced.status, GoalStatus::Active);
    assert_eq!(replaced.objective, "the user's new objective");
    assert_eq!(replaced.tokens_used, 0);
}

#[test]
fn lowering_the_budget_below_what_is_spent_stops_the_goal_in_the_same_statement() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "stay within budget", Some(1_000))
        .expect("create the goal");
    let spent = fixture
        .store
        .record_usage(SESSION, 500, 30, true)
        .expect("record usage")
        .expect("the session has a goal");
    assert_eq!(spent.status, GoalStatus::Active);

    let lowered = fixture
        .store
        .set_token_budget(SESSION, Some(400))
        .expect("lower the budget")
        .expect("the session has a goal");
    assert_eq!(lowered.status, GoalStatus::BudgetLimited);
    assert_eq!(fixture.raw_status(SESSION), "budget_limited");
    assert_eq!(lowered.tokens_used, 500);

    let raised = fixture
        .store
        .set_token_budget(SESSION, Some(2_000))
        .expect("raise the budget")
        .expect("the session has a goal");
    assert_eq!(
        raised.status,
        GoalStatus::BudgetLimited,
        "raising the budget must not silently resume the goal"
    );
    let resumed = fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Active)
        .expect("resume the goal")
        .expect("the session has a goal");
    assert_eq!(
        resumed.status,
        GoalStatus::Active,
        "once the budget covers the spend, the system may resume"
    );
}

#[test]
fn an_active_goal_is_never_observed_over_its_budget() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "stay within budget", Some(BUDGET))
        .expect("create the goal");

    let by_usage = fixture
        .store
        .record_usage(SESSION, OVERSPEND, 1, true)
        .expect("record usage")
        .expect("the session has a goal");
    assert!(!(by_usage.status.is_active() && by_usage.is_over_budget()));

    fixture
        .store
        .replace_goal_as_system(SESSION, "again", None)
        .expect("replace the goal");
    fixture
        .store
        .record_usage(SESSION, OVERSPEND, 1, true)
        .expect("record usage");
    let by_budget = fixture
        .store
        .set_token_budget(SESSION, Some(BUDGET))
        .expect("set a budget the spend already exceeds")
        .expect("the session has a goal");
    assert!(!(by_budget.status.is_active() && by_budget.is_over_budget()));

    let by_reactivation = fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Active)
        .expect("try to reactivate")
        .expect("the session has a goal");
    assert!(!(by_reactivation.status.is_active() && by_reactivation.is_over_budget()));
}

#[test]
fn usage_accumulates_for_a_finished_goal_without_reviving_the_budget_flip() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "finish the report", Some(BUDGET))
        .expect("create the goal");
    fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect("complete the goal");

    let accounted = fixture
        .store
        .record_usage(SESSION, OVERSPEND, 30, true)
        .expect("record the completing turn's cost")
        .expect("the session has a goal");
    assert_eq!(accounted.tokens_used, OVERSPEND);
    assert_eq!(accounted.time_used_seconds, 30);
    assert_eq!(
        accounted.status,
        GoalStatus::Complete,
        "a finished goal must not be relabelled budget_limited by its own final cost"
    );
    assert_eq!(fixture.raw_counters(SESSION), (OVERSPEND, 30));
}

#[test]
fn negative_deltas_are_clamped_rather_than_persisted() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "count forwards only", None)
        .expect("create the goal");
    fixture
        .store
        .record_usage(SESSION, 100, 10, true)
        .expect("record usage");

    let clamped = fixture
        .store
        .record_usage(SESSION, -50, -5, true)
        .expect("record usage")
        .expect("the session has a goal");
    assert_eq!(clamped.tokens_used, 100);
    assert_eq!(clamped.time_used_seconds, 10);
}

#[test]
fn unknown_accounting_is_sticky_and_a_charge_is_not_a_revision() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "track trustworthy usage", Some(1_000))
        .expect("create the goal");

    let unknown = fixture
        .store
        .record_usage(SESSION, 100, 10, false)
        .expect("record unknown usage")
        .expect("the session has a goal");
    assert_eq!(unknown.tokens_used, 100);
    assert!(!unknown.usage_known);

    let later = fixture
        .store
        .record_usage(SESSION, 50, 5, true)
        .expect("record later confirmed usage")
        .expect("the session has a goal");
    assert_eq!(later.tokens_used, 150);
    assert_eq!(later.time_used_seconds, 15);
    assert!(
        !later.usage_known,
        "a later confirmed checkpoint must not erase an earlier accounting gap"
    );

    // The floor lives on the goal row, which is what every reader consults, and the
    // two charges appended no history because a charge is not a revision. Usage is
    // now recorded around every provider request, so a history that recorded charges
    // would be a token log with the revision trail buried in it.
    let history = fixture.store.history(SESSION).expect("read history");
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.goal.usage_known)
            .collect::<Vec<_>>(),
        [true],
        "only the creation is a revision here"
    );
    let current = fixture
        .store
        .goal(SESSION)
        .expect("read the goal")
        .expect("the session has a goal");
    assert_eq!(current.tokens_used, 150);
    assert!(!current.usage_known);
}

#[test]
fn rewriting_the_objective_keeps_the_goal_instance_and_its_counters() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal(SESSION, "draft the report", Some(1_000))
        .expect("create the goal");
    fixture
        .store
        .record_usage(SESSION, 120, 12, true)
        .expect("record usage");

    let updated = fixture
        .store
        .update_objective(SESSION, "draft the report clearly")
        .expect("rewrite the objective")
        .expect("the session has a goal");
    assert_eq!(updated.objective, "draft the report clearly");
    assert_eq!(updated.goal_id, created.goal_id);
    assert_eq!(updated.created_at_ms, created.created_at_ms);
    assert_eq!(updated.tokens_used, 120);
    assert_eq!(updated.time_used_seconds, 12);
}

#[test]
fn an_oversized_rewrite_spills_just_as_creation_does() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "short", None)
        .expect("create the goal");
    let objective = "a much longer plan ".repeat(300);
    assert!(objective.chars().count() > MAX_OBJECTIVE_CHARS);

    let updated = fixture
        .store
        .update_objective(SESSION, &objective)
        .expect("rewrite the objective")
        .expect("the session has a goal");
    assert!(updated.objective.chars().count() <= MAX_OBJECTIVE_CHARS);
    let path = fixture
        .store
        .objective_file(&updated.objective)
        .expect("the rewritten objective must be a resolvable pointer");
    assert_eq!(
        std::fs::read_to_string(path).expect("read the spilled objective"),
        objective.trim()
    );
}

#[test]
fn an_empty_objective_is_refused_on_creation_and_on_rewrite() {
    let fixture = Fixture::in_memory();
    assert!(matches!(
        fixture.store.create_goal(SESSION, "   ", None),
        Err(GoalError::EmptyObjective)
    ));
    assert_eq!(fixture.store.goal(SESSION).expect("read the goal"), None);

    fixture
        .store
        .create_goal(SESSION, "a real objective", None)
        .expect("create the goal");
    assert!(matches!(
        fixture.store.update_objective(SESSION, ""),
        Err(GoalError::EmptyObjective)
    ));
    assert_eq!(fixture.goal(SESSION).objective, "a real objective");
}

#[test]
fn a_session_with_no_goal_reads_none_and_every_write_reports_none() {
    let fixture = Fixture::in_memory();
    assert_eq!(fixture.store.goal("ses_absent").expect("read"), None);
    assert_eq!(
        fixture
            .store
            .update_status_as_model("ses_absent", ModelStatus::Complete)
            .expect("write"),
        None
    );
    assert_eq!(
        fixture
            .store
            .set_status_as_system("ses_absent", SystemStatus::Paused)
            .expect("write"),
        None
    );
    assert_eq!(
        fixture
            .store
            .set_token_budget("ses_absent", Some(10))
            .expect("write"),
        None
    );
    assert_eq!(
        fixture
            .store
            .record_usage("ses_absent", 10, 1, true)
            .expect("write"),
        None
    );
    assert_eq!(
        fixture
            .store
            .update_objective("ses_absent", "nothing to update")
            .expect("write"),
        None
    );
}

#[test]
fn two_sessions_keep_independent_goals() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal("ses_a", "objective a", Some(10))
        .expect("create a");
    fixture
        .store
        .create_goal("ses_b", "objective b", None)
        .expect("create b");
    fixture
        .store
        .record_usage("ses_a", 10, 1, true)
        .expect("spend a's budget");

    assert_eq!(fixture.status("ses_a"), GoalStatus::BudgetLimited);
    assert_eq!(fixture.status("ses_b"), GoalStatus::Active);
    assert_eq!(fixture.goal("ses_b").tokens_used, 0);
}

/// The guard has to be *in* the statement, not around it.
///
/// A test that only calls [`GoalStore::create_goal`] cannot tell a SQL `WHERE`
/// from a Rust `if` wrapped around an unguarded upsert — and the difference is
/// whether two concurrent calls can both replace one goal. So this runs the
/// statement text directly and asserts SQLite itself declined the row.
#[test]
fn the_replacement_guard_is_in_the_statement_and_not_around_it() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "the first objective", None)
        .expect("create the goal");
    let sql = format!("{UPSERT_BODY}\n{UPSERT_IF_COMPLETE}");
    assert!(
        sql.contains("WHERE goal.status IN ('complete', 'cancelled')"),
        "the guard must be part of the upsert: {sql}"
    );

    let connection = fixture.store.pool().get().expect("check out a connection");
    let mut statement = connection
        .prepare(&sql)
        .expect("prepare the guarded upsert");
    let refused = statement
        .query(params![
            SESSION,
            "goal-2",
            "the second objective",
            "[]",
            None::<i64>,
            1_i64
        ])
        .expect("run the guarded upsert")
        .next()
        .expect("step the guarded upsert")
        .is_none();
    assert!(
        refused,
        "SQLite must decline the row while the goal is not complete"
    );
    assert_eq!(fixture.goal(SESSION).objective, "the first objective");

    fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect("complete the goal");
    let accepted = statement
        .query(params![
            SESSION,
            "goal-2",
            "the second objective",
            "[]",
            None::<i64>,
            2_i64
        ])
        .expect("run the guarded upsert")
        .next()
        .expect("step the guarded upsert")
        .is_some();
    assert!(accepted, "the same statement must succeed once complete");
    assert_eq!(fixture.goal(SESSION).objective, "the second objective");
}

/// The flip has to be in the statement that moves the counter.
///
/// Same argument as the replacement guard: run the SQL directly, with no Rust of
/// this crate's in the path, and watch the column change.
#[test]
fn the_budget_flip_is_in_the_recording_statement_and_not_around_it() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "stay within budget", Some(BUDGET))
        .expect("create the goal");
    assert!(
        RECORD_USAGE.contains("THEN 'budget_limited'"),
        "the flip must be part of the update: {RECORD_USAGE}"
    );

    let connection = fixture.store.pool().get().expect("check out a connection");
    let flipped: String = connection
        .query_row(
            "UPDATE goal SET revision = revision + 1, tokens_used = tokens_used + ?1, \
             status = CASE WHEN status = 'active' AND token_budget IS NOT NULL \
             AND tokens_used + ?1 >= token_budget THEN 'budget_limited' ELSE status END \
             WHERE session_id = ?2 RETURNING status",
            params![OVERSPEND, SESSION],
            |row| row.get(0),
        )
        .expect("run the recording update");
    assert_eq!(flipped, "budget_limited");
    assert_eq!(fixture.raw_status(SESSION), "budget_limited");
    assert_eq!(fixture.status(SESSION), GoalStatus::BudgetLimited);
}

#[test]
fn the_table_declares_the_check_constraint_and_deliberately_no_foreign_key() {
    let fixture = Fixture::in_memory();
    let connection = fixture.store.pool().get().expect("check out a connection");
    let ddl: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![TABLE],
            |row| row.get(0),
        )
        .expect("read the table's DDL");
    for status in GoalStatus::ALL {
        assert!(
            ddl.contains(&format!("'{}'", status.as_str())),
            "the CHECK constraint omits {status}: {ddl}"
        );
    }
    assert!(ddl.contains("CHECK(status IN ("), "{ddl}");
    assert!(
        !ddl.to_ascii_uppercase().contains("FOREIGN KEY"),
        "a goal must not cascade away with unrelated state: {ddl}"
    );

    let foreign_keys: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_foreign_key_list('goal')",
            [],
            |row| row.get(0),
        )
        .expect("count the table's foreign keys");
    assert_eq!(foreign_keys, 0);

    let retry_ddl: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'goal_retry'",
            [],
            |row| row.get(0),
        )
        .expect("read the retry table's DDL");
    assert!(retry_ddl.contains("CHECK(attempt >= 1)"), "{retry_ddl}");
    for reason in [
        "rate_limited",
        "provider_transient",
        "provider_stream",
        "provider_retry_deadline",
        "database_busy",
        "step_limit",
        "empty_assistant_message",
        "context_limit",
        "context_compacted",
        "tool_transient",
    ] {
        assert!(
            retry_ddl.contains(&format!("'{reason}'")),
            "the retry CHECK constraint omits {reason}: {retry_ddl}"
        );
    }
    assert!(
        !retry_ddl.contains("'tool_uncertain'"),
        "uncertain side effects must be pauses, not retry reasons: {retry_ddl}"
    );
    assert!(
        !retry_ddl.to_ascii_uppercase().contains("FOREIGN KEY"),
        "retry state follows the goal's explicit ownership: {retry_ddl}"
    );

    let pause_ddl: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'goal_pause'",
            [],
            |row| row.get(0),
        )
        .expect("read the pause table's DDL");
    for reason in GoalPauseReason::ALL {
        assert!(
            pause_ddl.contains(&format!("'{}'", reason.as_str())),
            "the pause CHECK constraint omits {reason}: {pause_ddl}"
        );
    }
    assert!(
        pause_ddl.contains("'uncertain_side_effect'"),
        "uncertain side effects must require inspection before any replay: {pause_ddl}"
    );
}

#[test]
fn a_status_outside_the_check_constraint_cannot_be_written_at_all() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "guarded by the column", None)
        .expect("create the goal");
    let connection = fixture.store.pool().get().expect("check out a connection");
    let error = connection
        .execute(
            "UPDATE goal SET status = 'abandoned' WHERE session_id = ?1",
            params![SESSION],
        )
        .expect_err("the CHECK constraint must reject an unknown status");
    assert!(zuno_db::is_constraint_violation(&error), "{error:?}");
    assert_eq!(fixture.raw_status(SESSION), "active");
}

#[test]
fn reopening_the_same_file_does_not_disturb_the_stored_goal() {
    let fixture = Fixture::on_disk();
    fixture
        .store
        .create_goal(SESSION, "idempotent schema", Some(5))
        .expect("create the goal");
    let before = fixture.goal(SESSION);

    let path = fixture.database_path();
    let reopened = GoalStore::open_at(&path, fixture.spill.path().to_owned())
        .expect("a second store on the same file");
    assert_eq!(
        reopened.goal(SESSION).expect("read the goal"),
        Some(before),
        "opening the store must create the table only when it is absent"
    );
}

#[test]
fn goal_history_keeps_every_revision_across_cancel_and_replacement() {
    let fixture = Fixture::in_memory();
    let first = fixture
        .store
        .create_goal(SESSION, "first objective", None)
        .expect("create first goal");
    fixture
        .store
        .update_objective(SESSION, "revised objective")
        .expect("revise objective")
        .expect("goal exists");
    fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Cancelled)
        .expect("cancel goal")
        .expect("goal exists");
    let second = fixture
        .store
        .create_goal(SESSION, "replacement objective", None)
        .expect("replace cancelled goal");

    let history = fixture.store.history(SESSION).expect("read goal history");
    assert_eq!(history.len(), 4);
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.revision)
            .collect::<Vec<_>>(),
        [1, 2, 3, 1]
    );
    assert_eq!(history[0].goal.goal_id, first.goal_id);
    assert_eq!(history[2].goal.status, GoalStatus::Cancelled);
    assert_eq!(history[3].goal.goal_id, second.goal_id);
    assert!(
        history
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
}

/// The trigger's guard has to say the same thing as the repair that enforces it.
///
/// Two copies of one string in two files that are never read together: a guard reworded
/// in the schema and not here would leave every upgraded database being repaired on
/// every open, or none of them being repaired at all.
#[test]
fn the_history_trigger_carries_the_guard_the_repair_looks_for() {
    assert!(
        AUXILIARY_SCHEMA.contains(HISTORY_UPDATE_GUARD),
        "the update trigger must be declared with {HISTORY_UPDATE_GUARD}"
    );
}

/// Replacing a goal that is still at revision 1 must not lose the new goal's history.
///
/// The revision resets to 1 for a replacement, so a trigger guarded on the revision
/// alone compares 1 with 1 and appends nothing. The row that is lost is the first one:
/// what the new goal was created as, which is the only record of its objective before
/// anything edited it.
#[test]
fn replacing_a_first_revision_goal_records_the_new_goal_in_history() {
    let fixture = Fixture::in_memory();
    let first = fixture
        .store
        .create_goal(SESSION, "first objective", None)
        .expect("create the first goal");
    let second = fixture
        .store
        .replace_goal_as_system(SESSION, "replacement objective", None)
        .expect("replace the goal");
    assert_eq!(first.revision, second.revision, "both are at revision 1");
    assert_ne!(first.goal_id, second.goal_id);

    let history = fixture.store.history(SESSION).expect("read goal history");
    assert_eq!(
        history
            .iter()
            .map(|entry| (entry.goal.goal_id.as_str(), entry.revision))
            .collect::<Vec<_>>(),
        [(first.goal_id.as_str(), 1), (second.goal_id.as_str(), 1)]
    );
    assert_eq!(history[1].goal.objective, "replacement objective");
}

/// A database created with the revision-only guard is repaired the same way.
///
/// The trigger before this one recorded every revision change and nothing else, so it
/// is not broken in the way the unguarded trigger was - it silently omits one row
/// instead of failing a key. An install that never fails is exactly the one nobody
/// would think to check, so the repair is exercised here rather than assumed.
#[test]
fn a_revision_only_history_trigger_is_replaced_before_a_goal_is_replaced() {
    let spill = tempfile::tempdir().expect("create spill directory");
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    let mut connection = pool.open_connection().expect("open shared connection");
    zuno_db::migration::apply(&mut connection).expect("apply shared schema");
    connection
        .execute_batch(SCHEMA)
        .expect("create the goal table");
    connection
        .execute_batch(AUXILIARY_SCHEMA)
        .expect("create the goal history table and its triggers");
    connection
        .execute_batch(
            "DROP TRIGGER goal_history_after_update;
             CREATE TRIGGER goal_history_after_update
             AFTER UPDATE ON goal
             WHEN NEW.revision <> OLD.revision
             BEGIN
                 INSERT INTO goal_history (
                     session_id, goal_id, revision, objective, success_criteria, status,
                     blocked_reason, token_budget, tokens_used, usage_known,
                     time_used_seconds, created_at_ms, updated_at_ms
                 ) VALUES (
                     NEW.session_id, NEW.goal_id, NEW.revision, NEW.objective,
                     NEW.success_criteria, NEW.status, NEW.blocked_reason, NEW.token_budget,
                     NEW.tokens_used, NEW.usage_known, NEW.time_used_seconds,
                     NEW.created_at_ms, NEW.updated_at_ms
                 );
             END",
        )
        .expect("build the trigger this release replaced");
    drop(connection);

    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("attach goal store to the older database");
    store
        .create_goal(SESSION, "replace me on an upgraded database", None)
        .expect("create goal");
    let replacement = store
        .replace_goal_as_system(SESSION, "the replacement", None)
        .expect("replace the goal");

    let history = store.history(SESSION).expect("read history");
    assert_eq!(history.len(), 2, "the replacement has to be recorded");
    assert_eq!(history[1].goal.goal_id, replacement.goal_id);
}

/// A database created before the guard existed must not fail on its second charge.
///
/// The old trigger appended a history row for every `UPDATE goal`, and `goal_history` is
/// keyed `UNIQUE(goal_id, revision)`. Now that a charge leaves the revision alone, that
/// trigger fails the key on the second provider request of a session — on an upgraded
/// install only, and at the moment the run records what it spent. This test builds the
/// old trigger on purpose, so the repair is exercised rather than assumed.
#[test]
fn an_older_history_trigger_is_replaced_before_a_second_charge_lands() {
    let spill = tempfile::tempdir().expect("create spill directory");
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    let mut connection = pool.open_connection().expect("open shared connection");
    zuno_db::migration::apply(&mut connection).expect("apply shared schema");
    connection
        .execute_batch(SCHEMA)
        .expect("create the goal table");
    connection
        .execute_batch(AUXILIARY_SCHEMA)
        .expect("create the goal history table and its triggers");
    connection
        .execute_batch(
            "DROP TRIGGER goal_history_after_update;
             CREATE TRIGGER goal_history_after_update
             AFTER UPDATE ON goal
             BEGIN
                 INSERT INTO goal_history (
                     session_id, goal_id, revision, objective, success_criteria, status,
                     blocked_reason, token_budget, tokens_used, usage_known,
                     time_used_seconds, created_at_ms, updated_at_ms
                 ) VALUES (
                     NEW.session_id, NEW.goal_id, NEW.revision, NEW.objective,
                     NEW.success_criteria, NEW.status, NEW.blocked_reason, NEW.token_budget,
                     NEW.tokens_used, NEW.usage_known, NEW.time_used_seconds,
                     NEW.created_at_ms, NEW.updated_at_ms
                 );
             END",
        )
        .expect("build the trigger this release replaced");
    drop(connection);

    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("attach goal store to the older database");
    store
        .create_goal(
            SESSION,
            "charge twice on an upgraded database",
            Some(10_000),
        )
        .expect("create goal");

    for (request_id, at_ms) in [("turn-1:1", 1_000), ("turn-1:2", 1_100)] {
        store
            .record_request_usage(SESSION, request_id, 11, at_ms)
            .expect("charge a request on an upgraded database");
    }

    let goal = store
        .goal(SESSION)
        .expect("read the goal")
        .expect("the session has a goal");
    assert_eq!(goal.tokens_used, 22, "both charges must be recorded");
    assert_eq!(
        store.history(SESSION).expect("read history").len(),
        1,
        "the replaced trigger must not append a charge"
    );
}

/// A database created before `turn_budget` existed must still accept it.
///
/// The pause table's `CHECK` constraint names every reason, `CREATE TABLE IF NOT EXISTS`
/// cannot change it, and SQLite cannot `ALTER` it. Without the repair in `from_pool`, a
/// new reason works on a fresh install and fails on every upgraded one, at the moment it
/// records why a run stopped. This test builds the old table on purpose, so the repair is
/// exercised rather than assumed.
#[test]
fn an_older_pause_table_is_widened_without_losing_the_pauses_it_holds() {
    let spill = tempfile::tempdir().expect("create spill directory");
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    let mut connection = pool.open_connection().expect("open shared connection");
    zuno_db::migration::apply(&mut connection).expect("apply shared schema");
    connection
        .execute_batch(
            "CREATE TABLE goal_pause (
                 session_id TEXT PRIMARY KEY NOT NULL,
                 goal_id TEXT NOT NULL,
                 reason TEXT NOT NULL CHECK(reason IN (
                     'user_interruption',
                     'plan_mode',
                     'human_input',
                     'permission',
                     'authentication',
                     'uncertain_side_effect'
                 )),
                 human_request_id TEXT,
                 paused_at_ms INTEGER NOT NULL
             );
             INSERT INTO goal_pause (session_id, goal_id, reason, human_request_id, paused_at_ms)
             VALUES ('older-session', 'older-goal', 'human_input', 'req-1', 7)",
        )
        .expect("build the pause table this release replaced");
    assert!(
        connection
            .execute(
                "INSERT INTO goal_pause \
                 (session_id, goal_id, reason, human_request_id, paused_at_ms) \
                 VALUES ('other', 'goal', 'turn_budget', NULL, 8)",
                [],
            )
            .is_err(),
        "the old constraint must reject the new reason, or this test proves nothing"
    );
    drop(connection);

    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("attach goal store to the older database");

    let connection = pool.get().expect("check out a connection");
    let (goal_id, reason, request, paused_at): (String, String, Option<String>, i64) = connection
        .query_row(
            "SELECT goal_id, reason, human_request_id, paused_at_ms \
             FROM goal_pause WHERE session_id = 'older-session'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("the pause the older database held must survive the rebuild");
    assert_eq!(goal_id, "older-goal");
    assert_eq!(reason, "human_input");
    assert_eq!(request.as_deref(), Some("req-1"));
    assert_eq!(paused_at, 7);
    assert!(
        connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' \
                 AND name = 'goal_pause_superseded'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .expect("query for the temporary table")
            .is_none(),
        "the rebuild must not leave its scratch table behind"
    );
    drop(connection);

    let connection = pool.get().expect("check out a connection");
    connection
        .execute(
            "INSERT OR IGNORE INTO project \
             (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
              time_updated,time_initialized,sandboxes,commands) \
             VALUES ('goal-fixture','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
            [],
        )
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO session \
             (id,project_id,slug,directory,title,version,time_created,time_updated) \
             VALUES (?1,'goal-fixture',?1,'/tmp',?1,'test',1,1)",
            params![SESSION],
        )
        .expect("insert session");
    drop(connection);

    store
        .create_goal(SESSION, "ship safely", None)
        .expect("create goal");
    let paused = store
        .pause_with_reason(SESSION, GoalPauseReason::TurnBudget)
        .expect("record a turn-budget pause")
        .expect("the goal was active");
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(
        store
            .pause_state(SESSION)
            .expect("read the pause")
            .expect("a pause exists")
            .reason,
        GoalPauseReason::TurnBudget
    );
}

fn shared_completion_fixture() -> (TempDir, Arc<zuno_db::Pool>, GoalStore, Goal) {
    let spill = tempfile::tempdir().expect("create spill directory");
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open shared database"),
    );
    let mut connection = pool.open_connection().expect("open shared connection");
    zuno_db::migration::apply(&mut connection).expect("apply shared schema");
    connection
        .execute(
            "INSERT INTO project \
             (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
              time_updated,time_initialized,sandboxes,commands) \
             VALUES ('prj','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
            [],
        )
        .expect("insert project");
    connection
        .execute(
            "INSERT INTO session \
             (id,project_id,slug,directory,title,version,time_created,time_updated) \
             VALUES (?1,'prj','goal','/tmp','goal','test',1,1)",
            params![SESSION],
        )
        .expect("insert session");
    drop(connection);

    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("attach goal store");
    let goal = store
        .create_goal(SESSION, "ship safely", None)
        .expect("create goal");
    (spill, pool, store, goal)
}

fn completion_job(id: &str, delivery: zuno_db::job::ReportDelivery) -> zuno_db::job::NewAgentJob {
    completion_job_for(SESSION, id, delivery)
}

fn completion_job_for(
    parent_session_id: &str,
    id: &str,
    delivery: zuno_db::job::ReportDelivery,
) -> zuno_db::job::NewAgentJob {
    zuno_db::job::NewAgentJob::new(
        id,
        parent_session_id,
        zuno_db::job::JobSubject::product_agent(
            format!("run-{id}"),
            "codex",
            "release-review",
            "subagent_codex",
        ),
        delivery,
        1,
    )
}

fn completion_report(id: &str, job_id: &str) -> zuno_db::inbox::NewSessionInput {
    completion_report_for(SESSION, id, job_id)
}

fn completion_report_for(
    session_id: &str,
    id: &str,
    job_id: &str,
) -> zuno_db::inbox::NewSessionInput {
    zuno_db::inbox::NewSessionInput::new(
        id,
        session_id,
        serde_json::json!({
            "kind": "subagentReport",
            "jobID": job_id,
            "text": "durable report"
        }),
        zuno_db::inbox::InputDelivery::Queue,
        2,
    )
}

fn insert_child_session(pool: &zuno_db::Pool, session_id: &str, parent_id: &str) {
    let connection = pool.get().expect("check out connection");
    connection
        .execute(
            "INSERT INTO session \
             (id,project_id,parent_id,slug,directory,title,version,time_created,time_updated) \
             VALUES (?1,'prj',?2,?1,'/tmp',?1,'test',1,1)",
            params![session_id, parent_id],
        )
        .expect("insert child session");
}

fn insert_grandchild_sessions(pool: &zuno_db::Pool) {
    insert_child_session(pool, "child-session", SESSION);
    insert_child_session(pool, "grandchild-session", "child-session");
}

fn terminal_settlement(
    status: zuno_db::job::JobStatus,
    report: Option<zuno_db::inbox::NewSessionInput>,
) -> zuno_db::job::JobSettlement {
    match status {
        zuno_db::job::JobStatus::Completed => {
            zuno_db::job::JobSettlement::completed(serde_json::json!({"ok": true}), 2, report)
        }
        zuno_db::job::JobStatus::Failed => zuno_db::job::JobSettlement::failed("failed", 2, report),
        zuno_db::job::JobStatus::Cancelled => {
            zuno_db::job::JobSettlement::cancelled("cancelled", 2, report)
        }
        zuno_db::job::JobStatus::Uncertain => {
            zuno_db::job::JobSettlement::uncertain("uncertain", 2, report)
        }
        zuno_db::job::JobStatus::Queued | zuno_db::job::JobStatus::Running => {
            panic!("terminal settlement helper received an active status")
        }
    }
}

fn assert_job_completion_blocked(store: &GoalStore, goal: &Goal) {
    let blocked = store
        .complete_checked(SESSION, goal.revision)
        .expect_err("durable job state must block completion");
    assert!(matches!(
        blocked,
        GoalError::CompletionBlocked {
            plan_steps: 0,
            work_items: 0,
            jobs: 1,
            human_requests: 0,
        }
    ));
    assert_eq!(
        store.goal(SESSION).expect("read goal"),
        Some(goal.clone()),
        "a blocked completion must not advance the goal revision"
    );
}

#[test]
fn completion_waits_for_a_pending_goal_human_request() {
    let (_spill, _pool, store, goal) = shared_completion_fixture();
    store
        .request_human_input_at(
            SESSION,
            goal.revision,
            "que_completion".to_owned(),
            serde_json::json!({
                "source": "goal_request_input",
                "questions": [{
                    "question": "Confirm release?",
                    "header": "Release",
                    "options": [],
                    "multiple": false,
                    "custom": true
                }]
            }),
            GoalHumanRequestOrigin {
                message_id: Some("msg_completion".to_owned()),
                call_id: Some("call_completion".to_owned()),
            },
            2,
        )
        .expect("persist pending request");
    let paused = store.goal(SESSION).expect("read goal").expect("goal");
    assert!(matches!(
        store
            .complete_checked(SESSION, paused.revision)
            .expect_err("pending request must block completion"),
        GoalError::CompletionBlocked {
            plan_steps: 0,
            work_items: 0,
            jobs: 0,
            human_requests: 1,
        }
    ));

    store
        .human_requests()
        .answer_with_input(
            "que_completion",
            serde_json::json!({"answers": [["yes"]]}),
            3,
        )
        .expect("answer request")
        .expect("pending request");
    let active = store
        .resume_for_work(SESSION)
        .expect("resume goal")
        .expect("goal");
    assert_eq!(active.status, GoalStatus::Active);
    let completed = store
        .complete_checked(SESSION, active.revision)
        .expect("completion is released")
        .expect("goal");
    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn completion_waits_for_plan_work_items_and_jobs_in_one_transaction() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let steps = serde_json::json!([
        {"id":"scan","title":"Scan","status":"pending"},
        {"id":"ship","title":"Ship","status":"completed"}
    ]);
    let connection = pool.get().expect("check out connection");
    connection
        .execute(
            "INSERT INTO work_plan \
             (session_id,id,goal_id,revision,title,steps,time_created,time_updated) \
             VALUES (?1,'plan',?2,1,'release',?3,1,1)",
            params![SESSION, goal.goal_id, steps.to_string()],
        )
        .expect("insert plan");
    connection
        .execute(
            "INSERT INTO work_item \
             (id,session_id,goal_id,plan_step_id,parent_id,subject,description,active_form,\
              status,priority,dependencies,owner,revision,tokens_used,time_used_ms,\
              time_created,time_updated) \
             VALUES ('todo',?1,?2,'scan',NULL,'scan','scan repository',NULL,\
                     'in_progress','high','[]','researcher',1,0,0,1,1)",
            params![SESSION, goal.goal_id],
        )
        .expect("insert work item");
    drop(connection);

    let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
    jobs.create(completion_job("job", zuno_db::job::ReportDelivery::Quiet))
        .expect("insert running job");

    let blocked = store
        .complete_checked(SESSION, goal.revision)
        .expect_err("unfinished durable work must block completion");
    assert!(matches!(
        blocked,
        GoalError::CompletionBlocked {
            plan_steps: 1,
            work_items: 1,
            jobs: 1,
            human_requests: 0,
        }
    ));
    assert_eq!(store.goal(SESSION).expect("read goal"), Some(goal.clone()));

    let completed_steps = serde_json::json!([
        {"id":"scan","title":"Scan","status":"completed"},
        {"id":"ship","title":"Ship","status":"completed"}
    ]);
    let connection = pool.get().expect("check out connection");
    connection
        .execute(
            "UPDATE work_plan SET steps=?1 WHERE session_id=?2",
            params![completed_steps.to_string(), SESSION],
        )
        .expect("complete plan");
    connection
        .execute(
            "UPDATE work_item SET status='completed' WHERE session_id=?1",
            params![SESSION],
        )
        .expect("complete work item");
    drop(connection);
    jobs.settle(
        "job",
        zuno_db::job::JobSettlement::completed(serde_json::json!({"ok":true}), 2, None),
    )
    .expect("complete job");

    let completed = store
        .complete_checked(SESSION, goal.revision)
        .expect("complete goal")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

fn assert_job_completion_allowed(store: &GoalStore, goal: &Goal, reason: &str) {
    let completed = store
        .complete_checked(SESSION, goal.revision)
        .unwrap_or_else(|error| panic!("{reason}: {error}"))
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn completion_blocks_queued_and_running_jobs() {
    for status in [
        zuno_db::job::JobStatus::Queued,
        zuno_db::job::JobStatus::Running,
    ] {
        let (_spill, pool, store, goal) = shared_completion_fixture();
        let jobs = zuno_db::job::AgentJobStore::new(pool);
        let job = completion_job("active", zuno_db::job::ReportDelivery::Quiet);
        jobs.create(if status == zuno_db::job::JobStatus::Queued {
            job.queued()
        } else {
            job
        })
        .expect("create active job");
        assert_job_completion_blocked(&store, &goal);
    }
}

#[test]
fn completion_blocks_active_and_uncertain_jobs_in_grandchild_sessions_until_reconciled() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    insert_grandchild_sessions(&pool);
    let jobs = zuno_db::job::AgentJobStore::new(pool);
    let job_id = "grandchild-active";
    jobs.create(completion_job_for(
        "grandchild-session",
        job_id,
        zuno_db::job::ReportDelivery::Quiet,
    ))
    .expect("create grandchild job");

    assert_job_completion_blocked(&store, &goal);
    jobs.settle(
        job_id,
        terminal_settlement(zuno_db::job::JobStatus::Uncertain, None),
    )
    .expect("mark grandchild job uncertain");
    assert_job_completion_blocked(&store, &goal);

    jobs.reconcile_uncertain(
        job_id,
        zuno_db::job::JobReconciliation::completed(
            serde_json::json!({"finalText": "confirmed complete"}),
            "authoritative remote status",
            "grandchild operation completed exactly once",
            3,
            None,
        ),
    )
    .expect("reconcile grandchild job");

    assert_job_completion_allowed(
        &store,
        &goal,
        "an authoritatively reconciled grandchild job must release root completion",
    );
}

#[test]
fn completion_blocks_unconsumed_grandchild_reports_and_preserves_cancel_semantics() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    insert_grandchild_sessions(&pool);
    let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
    let inbox = zuno_db::inbox::SessionInbox::new(pool);
    let job_id = "grandchild-cancelled";
    let input_id = "grandchild-cancelled-report";
    jobs.create(completion_job_for(
        "grandchild-session",
        job_id,
        zuno_db::job::ReportDelivery::NextStep,
    ))
    .expect("create grandchild next-step job");
    jobs.settle(
        job_id,
        terminal_settlement(
            zuno_db::job::JobStatus::Cancelled,
            Some(completion_report_for(
                "grandchild-session",
                input_id,
                job_id,
            )),
        ),
    )
    .expect("cancel grandchild job with a report");

    assert_job_completion_blocked(&store, &goal);
    inbox
        .promote_id("grandchild-session", input_id)
        .expect("promote grandchild report")
        .expect("queued grandchild report");
    assert_job_completion_blocked(&store, &goal);
    inbox
        .mark_consumed("grandchild-session", input_id)
        .expect("consume grandchild report")
        .expect("promoted grandchild report");
    assert_job_completion_allowed(
        &store,
        &goal,
        "a cancelled grandchild job is reconciled after its next-step report is consumed",
    );

    let (_spill, pool, store, goal) = shared_completion_fixture();
    insert_grandchild_sessions(&pool);
    let jobs = zuno_db::job::AgentJobStore::new(pool);
    jobs.create(completion_job_for(
        "grandchild-session",
        "grandchild-quiet-cancelled",
        zuno_db::job::ReportDelivery::Quiet,
    ))
    .expect("create quiet grandchild job");
    jobs.settle(
        "grandchild-quiet-cancelled",
        terminal_settlement(zuno_db::job::JobStatus::Cancelled, None),
    )
    .expect("cancel quiet grandchild job");
    assert_job_completion_allowed(
        &store,
        &goal,
        "a quiet cancelled grandchild job must not block completion",
    );
}

#[test]
fn descendant_job_walk_terminates_when_session_parent_links_form_a_cycle() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    insert_grandchild_sessions(&pool);
    let connection = pool.get().expect("check out connection");
    connection
        .execute(
            "UPDATE session SET parent_id='grandchild-session' WHERE id=?1",
            params![SESSION],
        )
        .expect("close the session cycle");
    drop(connection);

    let jobs = zuno_db::job::AgentJobStore::new(pool);
    jobs.create(completion_job_for(
        "grandchild-session",
        "grandchild-cycle-active",
        zuno_db::job::ReportDelivery::Quiet,
    ))
    .expect("create a blocker inside the cycle");

    assert_job_completion_blocked(&store, &goal);
}

#[test]
fn completion_blocks_unconsumed_next_step_reports_then_releases_consumed_reports() {
    for status in [
        zuno_db::job::JobStatus::Completed,
        zuno_db::job::JobStatus::Failed,
        zuno_db::job::JobStatus::Cancelled,
    ] {
        let (_spill, pool, store, goal) = shared_completion_fixture();
        let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
        let inbox = zuno_db::inbox::SessionInbox::new(pool);
        let job_id = format!("reported-{}", status.as_str());
        let input_id = format!("input-{}", status.as_str());
        jobs.create(completion_job(
            &job_id,
            zuno_db::job::ReportDelivery::NextStep,
        ))
        .expect("create reported job");
        jobs.settle(
            &job_id,
            terminal_settlement(status, Some(completion_report(&input_id, &job_id))),
        )
        .expect("settle reported job");

        assert_job_completion_blocked(&store, &goal);
        inbox
            .promote_id(SESSION, &input_id)
            .expect("promote report")
            .expect("queued report");
        assert_job_completion_blocked(&store, &goal);
        inbox
            .mark_consumed(SESSION, &input_id)
            .expect("consume report")
            .expect("promoted report");

        assert_job_completion_allowed(
            &store,
            &goal,
            "a consumed next-step report must release a reconciled terminal job",
        );
    }
}

#[test]
fn completion_allows_quiet_completed_failed_and_cancelled_jobs() {
    for status in [
        zuno_db::job::JobStatus::Completed,
        zuno_db::job::JobStatus::Failed,
        zuno_db::job::JobStatus::Cancelled,
    ] {
        let (_spill, pool, store, goal) = shared_completion_fixture();
        let jobs = zuno_db::job::AgentJobStore::new(pool);
        let job_id = format!("quiet-{}", status.as_str());
        jobs.create(completion_job(&job_id, zuno_db::job::ReportDelivery::Quiet))
            .expect("create quiet job");
        jobs.settle(&job_id, terminal_settlement(status, None))
            .expect("settle quiet job");

        assert_job_completion_allowed(
            &store,
            &goal,
            "a quiet completed, failed, or cancelled job is terminal and reconciled",
        );
    }
}

#[test]
fn completion_blocks_uncertain_jobs_even_after_report_consumption_or_quiet_delivery() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
    let inbox = zuno_db::inbox::SessionInbox::new(pool);
    let job_id = "reported-uncertain";
    let input_id = "input-uncertain";
    jobs.create(completion_job(
        job_id,
        zuno_db::job::ReportDelivery::NextStep,
    ))
    .expect("create uncertain reported job");
    jobs.settle(
        job_id,
        terminal_settlement(
            zuno_db::job::JobStatus::Uncertain,
            Some(completion_report(input_id, job_id)),
        ),
    )
    .expect("settle uncertain reported job");

    assert_job_completion_blocked(&store, &goal);
    inbox
        .promote_id(SESSION, input_id)
        .expect("promote uncertain report")
        .expect("queued uncertain report");
    assert_job_completion_blocked(&store, &goal);
    inbox
        .mark_consumed(SESSION, input_id)
        .expect("consume uncertain report")
        .expect("promoted uncertain report");
    assert_job_completion_blocked(&store, &goal);

    let (_spill, pool, store, goal) = shared_completion_fixture();
    let jobs = zuno_db::job::AgentJobStore::new(pool);
    jobs.create(completion_job(
        "quiet-uncertain",
        zuno_db::job::ReportDelivery::Quiet,
    ))
    .expect("create quiet uncertain job");
    jobs.settle(
        "quiet-uncertain",
        terminal_settlement(zuno_db::job::JobStatus::Uncertain, None),
    )
    .expect("settle quiet uncertain job");
    assert_job_completion_blocked(&store, &goal);
}

#[test]
fn authoritative_quiet_reconciliation_releases_goal_completion() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let jobs = zuno_db::job::AgentJobStore::new(pool);
    jobs.create(completion_job(
        "quiet-reconciled",
        zuno_db::job::ReportDelivery::Quiet,
    ))
    .expect("create quiet uncertain job");
    jobs.settle(
        "quiet-reconciled",
        terminal_settlement(zuno_db::job::JobStatus::Uncertain, None),
    )
    .expect("settle quiet uncertain job");
    assert_job_completion_blocked(&store, &goal);

    jobs.reconcile_uncertain(
        "quiet-reconciled",
        zuno_db::job::JobReconciliation::completed(
            serde_json::json!({"finalText": "confirmed complete"}),
            "authoritative remote status",
            "remote operation op-42 completed exactly once",
            3,
            None,
        ),
    )
    .expect("reconcile quiet job");

    assert_job_completion_allowed(
        &store,
        &goal,
        "authoritatively reconciled quiet work must release completion",
    );
}

#[test]
fn next_step_reconciliation_blocks_until_the_replacement_report_is_consumed() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&pool));
    let inbox = zuno_db::inbox::SessionInbox::new(pool);
    jobs.create(completion_job(
        "reported-reconciled",
        zuno_db::job::ReportDelivery::NextStep,
    ))
    .expect("create reported uncertain job");
    jobs.settle(
        "reported-reconciled",
        terminal_settlement(
            zuno_db::job::JobStatus::Uncertain,
            Some(completion_report(
                "input-uncertain-original",
                "reported-reconciled",
            )),
        ),
    )
    .expect("settle reported uncertain job");

    jobs.reconcile_uncertain(
        "reported-reconciled",
        zuno_db::job::JobReconciliation::completed(
            serde_json::json!({"finalText": "confirmed complete"}),
            "authoritative remote status",
            "remote operation op-43 completed exactly once",
            3,
            Some(completion_report(
                "input-uncertain-replacement",
                "reported-reconciled",
            )),
        ),
    )
    .expect("reconcile reported job");

    assert_job_completion_blocked(&store, &goal);
    inbox
        .promote_id(SESSION, "input-uncertain-replacement")
        .expect("promote replacement report")
        .expect("queued replacement report");
    assert_job_completion_blocked(&store, &goal);
    inbox
        .mark_consumed(SESSION, "input-uncertain-replacement")
        .expect("consume replacement report")
        .expect("promoted replacement report");

    assert_job_completion_allowed(
        &store,
        &goal,
        "a reconciled next-step job must remain blocked only until its replacement report is consumed",
    );
}

/// Record a receipt the way the runtime's verifying tool path does.
///
/// Written straight through the pool rather than through a fake, because the point
/// of every test below is what the goal store does with a *real* stored receipt.
fn record_receipt(
    fixture: &Fixture,
    session_id: &str,
    id: &str,
    outcome: ReceiptOutcome,
    exit_authority: zuno_db::verification::ExitAuthority,
    time_created: i64,
) {
    let connection = fixture.store.pool().get().expect("check out connection");
    zuno_db::verification::record(
        &connection,
        &zuno_db::verification::NewVerificationReceipt {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            turn_id: Some("turn-evidence".to_owned()),
            tool_call_id: format!("call-{id}"),
            tool_id: "shell".to_owned(),
            summary: "cargo test -p zuno-goal".to_owned(),
            workdir: None,
            exit_code: Some(0),
            exit_authority,
            outcome,
            git_head: None,
            output_digest: None,
            detail: None,
            time_created,
        },
    )
    .expect("record verification receipt");
}

/// A change goal with two criteria and one authoritative passing receipt.
///
/// The goal is backdated to `1_000` so the receipt at `2_000` is a check that ran
/// after the goal was proposed, which is what the evidence gate requires.
fn evidence_fixture() -> (Fixture, Goal) {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "land the evidence gate",
            &[
                "the workspace gates pass".to_owned(),
                "the release artifact exists".to_owned(),
            ],
            None,
        )
        .expect("create goal with criteria");
    fixture.backdate_goal(SESSION, 1_000);
    fixture
        .store
        .escalate_to_change(SESSION, "edited crates/zuno-goal/src/store.rs", 1_000)
        .expect("escalate to a change goal");
    record_receipt(
        &fixture,
        SESSION,
        "rec_pass",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        2_000,
    );
    let goal = fixture
        .store
        .goal(SESSION)
        .expect("read goal")
        .expect("the fixture created a goal");
    (fixture, goal)
}

#[test]
fn success_criteria_are_assigned_short_ids_in_creation_order() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "keep the checklist addressable",
            &[
                "first check".to_owned(),
                "second check".to_owned(),
                "third check".to_owned(),
            ],
            None,
        )
        .expect("create goal with criteria");

    assert_eq!(
        created
            .criteria
            .iter()
            .map(|criterion| (
                criterion.criterion_id.as_str(),
                criterion.statement.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            ("c1", "first check"),
            ("c2", "second check"),
            ("c3", "third check")
        ],
        "ids are positional so a model can cite them without transcribing a uuid"
    );
    assert_eq!(
        fixture.store.criteria(SESSION).expect("read criteria"),
        created.criteria,
        "the stored order is the creation order"
    );
    assert_eq!(
        created.goal.success_criteria,
        ["first check", "second check", "third check"],
        "the JSON column stays the compatibility projection of the same statements"
    );
    assert!(
        created
            .criteria
            .iter()
            .all(|criterion| criterion.status == GoalCriterionStatus::Open),
        "nothing is proven at creation"
    );
}

#[test]
fn a_criterion_is_satisfied_by_an_authoritative_passing_receipt() {
    let (fixture, goal) = evidence_fixture();

    let outcome = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 3_000)
        .expect("an authoritative passing receipt proves a criterion");

    assert_eq!(outcome.criterion.status, GoalCriterionStatus::Satisfied);
    assert_eq!(outcome.criterion.receipt_id.as_deref(), Some("rec_pass"));
    assert_eq!(outcome.criterion.satisfied_at_ms, Some(3_000));
    assert_eq!(
        outcome.goal.revision,
        goal.revision + 1,
        "closing a criterion is a change to the goal, so the revision moves"
    );
}

#[test]
fn a_failed_receipt_cannot_satisfy_a_criterion() {
    let (fixture, goal) = evidence_fixture();
    record_receipt(
        &fixture,
        SESSION,
        "rec_fail",
        ReceiptOutcome::Failed,
        zuno_db::verification::ExitAuthority::Authoritative,
        2_500,
    );

    let refusal = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_fail", 3_000)
        .expect_err("a failed check proves the opposite of success");

    assert!(matches!(
        &refusal,
        GoalError::EvidenceUnproven {
            criterion_id,
            receipt_id,
            ..
        } if criterion_id == "c1" && receipt_id == "rec_fail"
    ));
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert_eq!(
        fixture.store.goal(SESSION).expect("read goal"),
        Some(goal),
        "a refused citation leaves the goal untouched"
    );
}

#[test]
fn a_derived_exit_status_cannot_satisfy_a_criterion() {
    let (fixture, goal) = evidence_fixture();
    record_receipt(
        &fixture,
        SESSION,
        "rec_derived",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Derived,
        2_500,
    );

    let refusal = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_derived", 3_000)
        .expect_err("a status inferred from the last stage of a pipeline is not evidence");

    assert!(
        refusal.to_string().contains("derived"),
        "the refusal names why the status cannot be trusted: {refusal}"
    );
    assert!(matches!(refusal, GoalError::EvidenceUnproven { .. }));
}

#[test]
fn a_receipt_from_another_session_cannot_satisfy_a_criterion() {
    let (fixture, goal) = evidence_fixture();
    record_receipt(
        &fixture,
        "ses_somebody_else",
        "rec_borrowed",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        2_500,
    );

    let refusal = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_borrowed", 3_000)
        .expect_err("a receipt is evidence about the run that produced it");

    assert!(matches!(refusal, GoalError::EvidenceUnproven { .. }));
}

#[test]
fn a_receipt_older_than_the_last_workspace_change_is_refused_as_stale() {
    let (fixture, goal) = evidence_fixture();
    let reopened = fixture
        .store
        .mark_mutation(SESSION, 4_000)
        .expect("record a workspace change");
    assert_eq!(reopened, 0, "nothing was satisfied yet");

    let refusal = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 5_000)
        .expect_err("evidence gathered before the last edit describes code that is gone");

    assert!(matches!(
        refusal,
        GoalError::EvidenceStale {
            marked_at_ms: 4_000,
            receipt_at_ms: 2_000,
            ..
        }
    ));
}

#[test]
fn a_criterion_satisfied_before_the_last_edit_reopens() {
    let (fixture, goal) = evidence_fixture();
    let satisfied = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 3_000)
        .expect("satisfy the first criterion");

    let reopened = fixture
        .store
        .mark_mutation(SESSION, 4_000)
        .expect("record a workspace change");

    assert_eq!(
        reopened, 1,
        "the one satisfied criterion is no longer proven"
    );
    let criteria = fixture.store.criteria(SESSION).expect("read criteria");
    let first = criteria
        .iter()
        .find(|criterion| criterion.criterion_id == "c1")
        .expect("the first criterion still exists");
    assert_eq!(first.status, GoalCriterionStatus::Open);
    assert_eq!(
        first.receipt_id, None,
        "the citation goes with the status, so nothing looks proven by a stale receipt"
    );
    assert_eq!(
        satisfied.criterion.receipt_id.as_deref(),
        Some("rec_pass"),
        "the earlier outcome still describes what was true when it was returned"
    );
}

#[test]
fn a_mutation_mark_never_moves_backwards() {
    let (fixture, goal) = evidence_fixture();
    fixture
        .store
        .mark_mutation(SESSION, 9_000)
        .expect("record the later change first");
    fixture
        .store
        .mark_mutation(SESSION, 1_500)
        .expect("record an out-of-order change");

    let refusal = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 10_000)
        .expect_err("a late report must not re-validate stale evidence");

    assert!(matches!(
        refusal,
        GoalError::EvidenceStale {
            marked_at_ms: 9_000,
            ..
        }
    ));
}

#[test]
fn a_waiver_without_a_reason_is_refused() {
    let (fixture, goal) = evidence_fixture();

    let refusal = fixture
        .store
        .waive_criterion(SESSION, goal.revision, "c1", "   ", 3_000)
        .expect_err("an unexplained waiver is indistinguishable from skipping the check");

    assert!(matches!(
        &refusal,
        GoalError::EmptyWaiverReason { criterion_id } if criterion_id == "c1"
    ));
    assert!(refusal.is_model_refusal(), "{refusal}");

    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            goal.revision,
            "c1",
            "  the artifact is produced by release tooling this run cannot invoke  ",
            3_000,
        )
        .expect("a reasoned waiver is recorded");
    assert_eq!(waived.criterion.status, GoalCriterionStatus::Waived);
    assert_eq!(
        waived.criterion.waiver_reason.as_deref(),
        Some("the artifact is produced by release tooling this run cannot invoke"),
        "the reason is stored trimmed and verbatim"
    );
    assert_eq!(
        fixture
            .store
            .mark_mutation(SESSION, 9_000)
            .expect("record a workspace change"),
        0,
        "a decision is not invalidated by a later edit the way a test result is"
    );
}

#[test]
fn an_unknown_criterion_id_is_refused_with_the_ids_that_do_exist() {
    let (fixture, goal) = evidence_fixture();

    let refusal = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c9", "rec_pass", 3_000)
        .expect_err("an id that was never assigned cannot be closed");

    assert!(matches!(
        &refusal,
        GoalError::UnknownCriterion { criterion_id, .. } if criterion_id == "c9"
    ));
    assert!(
        refusal.to_string().contains("c1, c2"),
        "the refusal saves the model a turn spent guessing: {refusal}"
    );
}

#[test]
fn a_change_goal_cannot_complete_while_a_criterion_is_open() {
    let (fixture, goal) = evidence_fixture();
    let satisfied = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 3_000)
        .expect("prove the first criterion");

    let refusal = fixture
        .store
        .complete_checked(SESSION, satisfied.goal.revision)
        .expect_err("a change goal is done when its criteria are, not when it says so");

    assert!(matches!(
        &refusal,
        GoalError::EvidenceMissing { unsatisfied } if unsatisfied == &["c2".to_owned()]
    ));
    assert!(
        refusal.to_string().contains("c2"),
        "the refusal names exactly what is left: {refusal}"
    );
    assert_eq!(
        fixture
            .store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal exists")
            .status,
        GoalStatus::Active,
        "a refused completion leaves the run going"
    );
}

#[test]
fn a_change_goal_with_no_criteria_cannot_complete_at_all() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal(SESSION, "rewrite the parser", None)
        .expect("create goal");
    fixture
        .store
        .escalate_to_change(SESSION, "wrote crates/zuno-goal/src/store.rs", 1_000)
        .expect("escalate to a change goal");

    let refusal = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect_err("an empty checklist is not a completed one");

    assert!(matches!(
        &refusal,
        GoalError::EvidenceMissing { unsatisfied } if unsatisfied.is_empty()
    ));
    assert!(
        refusal
            .to_string()
            .contains("cannot complete without success criteria"),
        "the refusal says what is missing rather than which id: {refusal}"
    );
}

#[test]
fn a_waived_criterion_lets_a_change_goal_complete() {
    let (fixture, goal) = evidence_fixture();
    let satisfied = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 3_000)
        .expect("prove the first criterion");
    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            satisfied.goal.revision,
            "c2",
            "the release artifact is built by tooling outside this workspace",
            3_100,
        )
        .expect("waive the second criterion");

    let completed = fixture
        .store
        .complete_checked(SESSION, waived.goal.revision)
        .expect("evidence plus a recorded decision settles every criterion")
        .expect("goal exists");

    assert_eq!(completed.status, GoalStatus::Complete);
}

/// The model's own creator refuses the shape [`audit_evidence`] cannot read, and
/// refuses it before the row exists, so a run never ends up holding an ungated goal it
/// could later complete by assertion. Criteria are immutable after creation, so this is
/// also the only moment the requirement can still be met.
#[test]
fn the_model_cannot_propose_a_goal_that_names_no_check() {
    let fixture = Fixture::in_memory();

    let refusal = fixture
        .store
        .create_goal_as_model(SESSION, "make the parser accept trailing commas", &[], None)
        .expect_err("a proposal with no checklist is one the audit cannot read");
    assert!(matches!(refusal, GoalError::MissingSuccessCriteria));
    assert_eq!(
        fixture.store.goal(SESSION).expect("read goal"),
        None,
        "the refusal is not a goal with a note attached: nothing was written"
    );

    let blank = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "make the parser accept trailing commas",
            &["   ".to_owned(), "\t\n".to_owned()],
            None,
        )
        .expect_err("entries that are blank after trimming are not criteria");
    assert!(matches!(blank, GoalError::MissingSuccessCriteria));
    assert_eq!(fixture.store.goal(SESSION).expect("read goal"), None);

    // A blank entry among real ones is dropped rather than fatal: the checklist still
    // says something, so there is something to audit.
    let created = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "make the parser accept trailing commas",
            &["  ".to_owned(), " the parser test suite passes ".to_owned()],
            None,
        )
        .expect("a checklist with one real entry is a checklist");
    assert_eq!(
        created
            .criteria
            .iter()
            .map(|criterion| (criterion.criterion_id.clone(), criterion.statement.clone()))
            .collect::<Vec<_>>(),
        [("c1".to_owned(), "the parser test suite passes".to_owned())],
        "the blank entry is dropped and the surviving statement is stored trimmed"
    );
}

/// The human escape hatch, exactly as `/goal complete` reaches it: read the revision,
/// then [`GoalStore::complete_checked`]. A goal the model proposed now always carries a
/// checklist, so the user's own completion runs into the same audit — and until the CLI
/// grows a criterion verb, that refusal is the only thing standing between the user and
/// a dead end. The oracle is therefore not "it refuses" but *what the refusal says*: the
/// criterion id, and the two ways out. A refusal that named neither would be the
/// regression the reporter found.
#[test]
fn the_user_completing_a_model_proposed_goal_is_refused_with_the_id_and_the_way_out() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "make the parser accept trailing commas",
            &["the parser test suite passes".to_owned()],
            None,
        )
        .expect("propose a goal with a checklist");

    let refusal = fixture
        .store
        .complete_checked(SESSION, created.goal.revision)
        .expect_err("`/goal complete` runs the same audit as the model's own completion");
    let message = refusal.to_string();
    assert!(
        matches!(&refusal, GoalError::EvidenceMissing { unsatisfied } if unsatisfied == &["c1"]),
        "{refusal:?}"
    );
    assert!(
        message.contains("c1"),
        "a user who cannot see the criterion id cannot act on the refusal: {message}"
    );
    assert!(
        message.contains("cite the receipt id"),
        "the refusal has to name the first way out: {message}"
    );
    assert!(
        message.contains("the reason it will not be verified"),
        "and the second, or a criterion nothing can verify traps the goal: {message}"
    );
    assert_eq!(
        fixture
            .store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal exists")
            .status,
        GoalStatus::Active,
        "the refused completion leaves the goal exactly where it was"
    );

    // And the way out works from the user's side of the store, which is what the CLI
    // seam needs: waive the criterion, then complete.
    fixture
        .store
        .waive_criterion(
            SESSION,
            created.goal.revision,
            "c1",
            "the user ended the run before the suite was written",
            1_000,
        )
        .expect("waive the criterion");
    let revision = fixture
        .store
        .goal(SESSION)
        .expect("read goal")
        .expect("goal exists")
        .revision;
    let completed = fixture
        .store
        .complete_checked(SESSION, revision)
        .expect("a waived criterion is a settled criterion")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

/// The one goal shape that is still not gated on evidence, and the only creator that
/// can produce it: [`GoalStore::create_goal`], behind the user's own `/goal create`.
/// The user is the authority for their own objective and may leave it unmeasured;
/// [`GoalStore::create_goal_as_model`] refuses the same shape, because a goal the model
/// proposes for itself does not get to be unmeasurable.
#[test]
fn a_user_created_goal_with_no_criteria_completes_without_any_evidence() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal(SESSION, "explain how the budget flip works", None)
        .expect("create goal");

    let completed = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect("a goal that changed nothing and promised no check has nothing to verify")
        .expect("goal exists");

    assert_eq!(completed.status, GoalStatus::Complete);
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Question,
        "a goal is presumed harmless until something reports a change"
    );
}

/// The residual this crate could not close alone, now closed from the other side of the
/// seam: a goal the *user* created with no criteria, a workspace edit made by running a
/// command, and a model-reported completion. `shell` reports the paths it observed
/// changing (`writtenPaths`), and the verification ledger drives the same two calls for
/// that report as for every other mutating tool — [`GoalStore::escalate_to_change`] with
/// the reason it renders, then [`GoalStore::mark_mutation`] at the same instant
/// (`crates/zuno-cli/src/cmd/verification_ledger.rs`). That makes this a change goal with
/// an empty checklist, and an empty checklist is not a completed one: the model is refused
/// with [`GoalError::EvidenceMissing`] naming no criterion, because there is none to name.
///
/// The report is a lower bound, and only the reported shape is covered here. A target
/// that is statically resolvable and in scope — `sed -i 's/foo/bar/' src/lib.rs` — is
/// reported; one the shell expands — `$OUT`, `*.rs`, `$(ls)`, a here-doc, a redirection,
/// the files `git apply` rewrites — is skipped rather than guessed, and a run that edits
/// only through those still stays a question goal.
#[test]
fn a_shell_reported_write_escalates_a_user_goal_so_the_model_cannot_complete_it_unmeasured() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal(SESSION, "make the parser accept trailing commas", None)
        .expect("the user states the objective without measuring it");
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Question,
        "nothing has been reported yet, so the goal is presumed harmless"
    );

    // `shell {"command": "sed -i 's/foo/bar/' crates/zuno-parser/src/lib.rs"}` runs
    // here. The target is a literal in-scope path token, so the call reports it, and
    // the ledger makes exactly these two calls, in this order, at one timestamp.
    let at_ms = goal.created_at_ms + 1;
    assert_eq!(
        fixture
            .store
            .escalate_to_change(
                SESSION,
                "`shell` wrote crates/zuno-parser/src/lib.rs",
                at_ms
            )
            .expect("record the reported write"),
        GoalKind::Change,
        "the edit is visible to the kind"
    );
    assert_eq!(
        fixture
            .store
            .mark_mutation(SESSION, at_ms)
            .expect("advance the freshness mark"),
        0,
        "there was no satisfied criterion to reopen"
    );
    let marks: i64 = fixture
        .store
        .pool()
        .get()
        .expect("check out connection")
        .query_row(
            "SELECT count(*) FROM goal_mutation_mark WHERE session_id = ?1",
            [SESSION],
            |row| row.get(0),
        )
        .expect("count mutation marks");
    assert_eq!(
        marks, 1,
        "and visible to freshness, so a receipt recorded before this edit would be stale"
    );

    let refusal = fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect_err("a change goal with no checklist has nothing that could prove it done");

    assert!(
        matches!(&refusal, GoalError::EvidenceMissing { unsatisfied } if unsatisfied.is_empty()),
        "an empty checklist is refused with no criterion id to name: {refusal}"
    );
    assert_eq!(
        refusal.to_string(),
        "goal cannot complete without recorded verification evidence: a goal that changes \
         the workspace cannot complete without success criteria; propose success criteria \
         with `goal_propose` before completing (an unfinished goal cannot be re-proposed, so \
         this one has to be cancelled by the user first)",
        "the refusal says what is missing and what the run can still do about it"
    );
    assert!(
        matches!(
            fixture.store.complete_as_model_checked(SESSION, goal.revision),
            Err(GoalError::EvidenceMissing { unsatisfied }) if unsatisfied.is_empty()
        ),
        "the revision-guarded model entry point refuses the same way"
    );
    assert_eq!(
        fixture.status(SESSION),
        GoalStatus::Active,
        "a refused completion leaves the run going"
    );
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Change,
        "and the escalation stands, so the goal stays gated until criteria are proposed"
    );
}

/// The checklist gates completion even when nothing escalated the goal, because
/// escalation needs a tool that reports the paths it wrote and that report is a lower
/// bound. `shell` reports a statically resolvable in-scope target — `sed -i 's/foo/bar/'
/// src/lib.rs` — but a shell-expanded one — `$OUT`, `*.rs`, a here-doc, a redirection,
/// the files `git apply` rewrites — is skipped rather than guessed, so a run that edits
/// only through those stays a question goal. Without this the whole audit is skipped for
/// such a run and the goal completes with every criterion still open.
#[test]
fn an_unescalated_goal_cannot_complete_while_its_criteria_are_open() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "fix the failing clippy lint",
            &[
                "the workspace clippy gate passes".to_owned(),
                "the workspace test gate passes".to_owned(),
            ],
            None,
        )
        .expect("create goal with criteria");

    let refusal = fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect_err("a recorded checklist is audited whatever the kind says");

    assert!(
        matches!(
            &refusal,
            GoalError::EvidenceMissing { unsatisfied } if unsatisfied == &["c1", "c2"]
        ),
        "the refusal names both open criteria: {refusal}"
    );
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Question,
        "no write was reported, which is the reporting gap the checklist covers"
    );
    assert_eq!(
        fixture
            .store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal exists")
            .status,
        GoalStatus::Active,
        "a refused completion leaves the run going"
    );
    assert!(
        matches!(
            fixture
                .store
                .complete_checked(SESSION, created.goal.revision),
            Err(GoalError::EvidenceMissing { .. })
        ),
        "the revision-guarded entry point refuses the same way"
    );
}

#[test]
fn an_unescalated_goal_completes_once_its_checklist_is_settled() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "fix the failing clippy lint",
            &[
                "the workspace clippy gate passes".to_owned(),
                "the release artifact exists".to_owned(),
            ],
            None,
        )
        .expect("create goal with criteria");
    fixture.backdate_goal(SESSION, 1_000);
    record_receipt(
        &fixture,
        SESSION,
        "rec_clippy",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        2_000,
    );
    let satisfied = fixture
        .store
        .satisfy_criterion(SESSION, created.goal.revision, "c1", "rec_clippy", 3_000)
        .expect("prove the first criterion");
    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            satisfied.goal.revision,
            "c2",
            "the release artifact is built by tooling outside this workspace",
            3_100,
        )
        .expect("waive the second criterion");

    let completed = fixture
        .store
        .complete_checked(SESSION, waived.goal.revision)
        .expect("the gate is a checklist to close, not a trap")
        .expect("goal exists");

    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn escalating_to_a_change_goal_keeps_the_first_reason_and_is_idempotent() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "port the store", None)
        .expect("create goal");

    assert_eq!(
        fixture
            .store
            .escalate_to_change(SESSION, "wrote store.rs", 1_000)
            .expect("escalate"),
        GoalKind::Change
    );
    assert_eq!(
        fixture
            .store
            .escalate_to_change(SESSION, "wrote tools.rs", 2_000)
            .expect("escalate again"),
        GoalKind::Change
    );
    let connection = fixture.store.pool().get().expect("check out connection");
    let (reason, escalated_at_ms): (String, i64) = connection
        .query_row(
            "SELECT reason, escalated_at_ms FROM goal_kind WHERE session_id = ?1",
            params![SESSION],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read the recorded escalation");
    assert_eq!(reason, "wrote store.rs");
    assert_eq!(escalated_at_ms, 1_000);
}

#[test]
fn a_session_without_a_goal_cannot_be_escalated_or_marked() {
    let fixture = Fixture::in_memory();

    assert_eq!(
        fixture
            .store
            .escalate_to_change("ses_nothing", "wrote a file", 1_000)
            .expect("escalate without a goal"),
        GoalKind::Question,
        "a stale escalation must not gate a goal created later"
    );
    assert_eq!(
        fixture
            .store
            .mark_mutation("ses_nothing", 1_000)
            .expect("mark without a goal"),
        0
    );
}

#[test]
fn a_replayed_request_id_is_charged_once() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "spend the budget once", Some(10_000))
        .expect("create goal");

    let first = fixture
        .store
        .record_request_usage(SESSION, "turn-1:2", 250, 1_000)
        .expect("record a request");
    let replay = fixture
        .store
        .record_request_usage(SESSION, "turn-1:2", 250, 1_100)
        .expect("record the same request again");

    assert!(first.accounted, "the first sighting is charged");
    assert!(!replay.accounted, "the replay is recognised, not charged");
    assert_eq!(
        replay.goal.as_ref().expect("goal exists").tokens_used,
        250,
        "a resumed turn must not spend the budget twice"
    );
    assert_eq!(
        fixture
            .store
            .record_request_usage(SESSION, "turn-1:3", 100, 1_200)
            .expect("record the next request")
            .goal
            .expect("goal exists")
            .tokens_used,
        350
    );
    assert_eq!(
        fixture
            .store
            .record_request_usage("ses_nothing", "turn-1:2", 100, 1_300)
            .expect("record against a session with no goal")
            .goal,
        None
    );
}

/// The exact loop this guards against: the fixture ACP client reads a revision,
/// asks the model to write with it, and the request that carries the question charges
/// the goal. When that charge took the revision, the write conflicted, the client
/// retried with the revision it was given, and the run issued thousands of provider
/// requests without ever completing the goal.
#[test]
fn charging_a_request_leaves_a_writer_holding_the_revision() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal(
            SESSION,
            "answer without spending the revision",
            Some(10_000),
        )
        .expect("create goal");
    let held = created.revision;

    let charged = fixture
        .store
        .record_request_usage(SESSION, "turn-1:1", 11, 1_000)
        .expect("charge the request that asked for the write")
        .goal
        .expect("goal exists");

    assert!(charged.tokens_used > 0, "the charge must still be recorded");
    assert_eq!(
        charged.revision, held,
        "accounting is not a change a writer needs to know about"
    );
    let completed = fixture
        .store
        .update_status_as_model_checked(SESSION, ModelStatus::Complete, held)
        .expect("complete with the revision the writer was given")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

/// A status flip is a real change, so it still takes the token. A writer that read the
/// goal while it was active and completes it afterwards is asserting something about a
/// goal that has since stopped, which is the case the revision exists to catch.
#[test]
fn a_charge_that_spends_the_budget_takes_the_revision() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal(SESSION, "spend the whole budget", Some(100))
        .expect("create goal");

    let limited = fixture
        .store
        .record_request_usage(SESSION, "turn-1:1", 100, 1_000)
        .expect("charge the request that spends the budget")
        .goal
        .expect("goal exists");

    assert_eq!(limited.status, GoalStatus::BudgetLimited);
    assert_eq!(limited.revision, created.revision + 1);
    assert!(
        matches!(
            fixture.store.update_status_as_model_checked(
                SESSION,
                ModelStatus::Complete,
                created.revision
            ),
            Err(GoalError::RevisionConflict { .. })
        ),
        "a status the writer never saw must not be overwritten"
    );
}

#[test]
fn replacing_a_goal_clears_its_criteria_and_its_evidence() {
    let (fixture, goal) = evidence_fixture();
    fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 3_000)
        .expect("prove a criterion of the goal being replaced");

    fixture
        .store
        .replace_goal_as_system(SESSION, "a different objective entirely", None)
        .expect("replace the goal");

    assert_eq!(
        fixture.store.criteria(SESSION).expect("read criteria"),
        Vec::new(),
        "ids belong to one goal, so a citation cannot survive into the next"
    );
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Question,
        "the new goal has not changed anything yet"
    );
}

/// The visible plan of the completed goal, with every step finished.
fn insert_finished_plan(pool: &zuno_db::Pool, goal_id: Option<&str>) {
    let steps = serde_json::json!([{"id":"ship","title":"Ship","status":"completed"}]);
    let connection = pool.get().expect("check out connection");
    connection
        .execute(
            "INSERT INTO work_plan \
             (session_id,id,goal_id,revision,title,steps,time_created,time_updated) \
             VALUES (?1,'plan',?2,1,'release',?3,1,1)",
            params![SESSION, goal_id, steps.to_string()],
        )
        .expect("insert plan");
}

#[test]
fn a_visible_plan_bound_to_an_earlier_goal_refuses_completion_even_when_every_step_is_done() {
    let (_spill, pool, store, first) = shared_completion_fixture();
    insert_finished_plan(&pool, Some(&first.goal_id));
    store
        .complete_checked(SESSION, first.revision)
        .expect("the plan belongs to the goal completing")
        .expect("goal exists");

    let second = store
        .create_goal(SESSION, "a new objective over the old plan", None)
        .expect("replace the completed goal");
    assert_ne!(second.goal_id, first.goal_id);

    let refusal = store
        .complete_checked(SESSION, second.revision)
        .expect_err("a finished plan for the previous goal describes none of this goal's work");
    assert!(matches!(
        &refusal,
        GoalError::PlanBelongsToAnotherGoal {
            session_id,
            plan_goal_id,
            goal_id,
        } if session_id == SESSION && plan_goal_id == &first.goal_id && goal_id == &second.goal_id
    ));
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert!(
        refusal.to_string().contains("`plan_update`"),
        "the refusal names the tool that rebinds the plan: {refusal}"
    );
    assert_eq!(
        store.goal(SESSION).expect("read goal"),
        Some(second.clone()),
        "a refused completion leaves the goal untouched"
    );

    let connection = pool.get().expect("check out connection");
    connection
        .execute(
            "UPDATE work_plan SET goal_id=?1 WHERE session_id=?2",
            params![second.goal_id, SESSION],
        )
        .expect("rebind the plan to the goal completing");
    drop(connection);
    let completed = store
        .complete_checked(SESSION, second.revision)
        .expect("a plan bound to this goal with every step done blocks nothing")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn a_stale_plan_is_refused_as_stale_rather_than_as_unfinished_work() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let pending = serde_json::json!([{"id":"scan","title":"Scan","status":"pending"}]);
    let connection = pool.get().expect("check out connection");
    connection
        .execute(
            "INSERT INTO work_plan \
             (session_id,id,goal_id,revision,title,steps,time_created,time_updated) \
             VALUES (?1,'plan','goal_previous',1,'release',?2,1,1)",
            params![SESSION, pending.to_string()],
        )
        .expect("insert a plan of some earlier goal");
    drop(connection);

    let refusal = store
        .complete_checked(SESSION, goal.revision)
        .expect_err("a plan of another goal is refused before its steps are counted");

    assert!(
        matches!(
            &refusal,
            GoalError::PlanBelongsToAnotherGoal { plan_goal_id, .. } if plan_goal_id == "goal_previous"
        ),
        "counting a stale plan's steps would describe another goal's work as this one's: {refusal}"
    );
}

#[test]
fn a_plan_that_predates_goal_binding_still_lets_the_goal_complete() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    insert_finished_plan(&pool, None);

    let completed = store
        .complete_checked(SESSION, goal.revision)
        .expect("a plan written before plans knew their goal is not a stale plan")
        .expect("goal exists");

    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn an_archived_plan_of_an_earlier_goal_does_not_block_completion() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let pending = serde_json::json!([{"id":"scan","title":"Scan","status":"pending"}]);
    let connection = pool.get().expect("check out connection");
    for (id, state) in [
        ("plan_suspended", "suspended"),
        ("plan_completed", "completed"),
        ("plan_superseded", "superseded"),
    ] {
        connection
            .execute(
                "INSERT INTO work_plan_archive \
                 (id,session_id,parent_plan_id,stack_depth,goal_id,revision,title,steps,state,\
                  time_created,time_updated,time_archived) \
                 VALUES (?1,?2,NULL,0,'goal_previous',1,'old',?3,?4,1,1,1)",
                params![id, SESSION, pending.to_string(), state],
            )
            .expect("insert archived plan");
    }
    drop(connection);

    let completed = store
        .complete_checked(SESSION, goal.revision)
        .expect("archived plans are history or dormant parents, not the visible plan")
        .expect("goal exists");

    assert_eq!(completed.status, GoalStatus::Complete);
}

/// The *user* path, which is the only path left that reaches a criteria-less goal:
/// [`GoalStore::create_goal_as_model`] refuses the shape outright, as
/// `the_model_cannot_propose_a_goal_that_names_no_check` pins. The name says which
/// actor, because that is the only place this surface records it.
#[test]
fn a_user_created_goal_without_criteria_stays_a_question_until_its_first_write_and_then_cannot_complete()
 {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_with_criteria(SESSION, "enable structured output", &[], None)
        .expect("a goal without criteria is accepted, because a question needs none");
    assert!(created.criteria.is_empty());
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Question,
        "an empty checklist does not make a goal a change goal; only a write does"
    );

    assert_eq!(
        fixture
            .store
            .escalate_to_change(SESSION, "`write` wrote zuno.toml", 1_000)
            .expect("the first write escalates"),
        GoalKind::Change
    );

    let refusal = fixture
        .store
        .complete_checked(SESSION, created.goal.revision)
        .expect_err(
            "a change goal with no criteria can only complete by assertion, which is refused",
        );
    assert!(matches!(
        &refusal,
        GoalError::EvidenceMissing { unsatisfied } if unsatisfied.is_empty()
    ));
    assert!(
        refusal
            .to_string()
            .contains("propose success criteria with `goal_propose` before completing"),
        "the refusal names the remedy instead of an id: {refusal}"
    );
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert_eq!(
        fixture.status(SESSION),
        GoalStatus::Active,
        "a refused completion leaves the run going"
    );
}

/// The other half of the user path above: a question nothing wrote for still ends.
#[test]
fn a_user_created_goal_without_criteria_completes_as_a_question_while_nothing_was_written() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal(SESSION, "explain how bedrock regions differ", None)
        .expect("create goal");

    let completed = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect("nothing was written, so there is nothing to verify")
        .expect("goal exists");

    assert_eq!(completed.status, GoalStatus::Complete);
}

/// Replay the tool call that produced `original_id`, the way the runtime's
/// `(session_id, tool_call_id)` upsert does: the row keeps the call and takes the
/// new id and outcome. With `new_id == original_id` the citation still resolves, to
/// whatever the replay recorded.
fn replay_receipt(
    fixture: &Fixture,
    original_id: &str,
    new_id: &str,
    outcome: ReceiptOutcome,
    time_created: i64,
) {
    let connection = fixture.store.pool().get().expect("check out connection");
    zuno_db::verification::record(
        &connection,
        &zuno_db::verification::NewVerificationReceipt {
            id: new_id.to_owned(),
            session_id: SESSION.to_owned(),
            turn_id: Some("turn-evidence".to_owned()),
            tool_call_id: format!("call-{original_id}"),
            tool_id: "shell".to_owned(),
            summary: "cargo test -p zuno-goal".to_owned(),
            workdir: None,
            exit_code: Some(101),
            exit_authority: zuno_db::verification::ExitAuthority::Authoritative,
            outcome,
            git_head: None,
            output_digest: None,
            detail: None,
            time_created,
        },
    )
    .expect("replay the receipt");
}

/// The evidence fixture with both criteria settled: `c1` by `rec_pass`, `c2` waived.
fn settled_evidence_fixture() -> (Fixture, Goal) {
    let (fixture, goal) = evidence_fixture();
    let satisfied = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 3_000)
        .expect("prove the first criterion");
    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            satisfied.goal.revision,
            "c2",
            "the artifact is built by release tooling outside this workspace",
            3_100,
        )
        .expect("waive the second criterion");
    (fixture, waived.goal)
}

#[test]
fn a_cited_receipt_rewritten_by_a_replayed_call_under_a_new_id_no_longer_completes_the_goal() {
    let (fixture, goal) = settled_evidence_fixture();
    replay_receipt(
        &fixture,
        "rec_pass",
        "rec_pass_retry",
        ReceiptOutcome::Failed,
        3_500,
    );

    let refusal = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect_err("the row says satisfied, but the receipt it cites is gone");

    assert!(matches!(
        &refusal,
        GoalError::EvidenceUnproven {
            criterion_id,
            receipt_id,
            reason,
        } if criterion_id == "c1"
            && receipt_id == "rec_pass"
            && reason.contains("no longer recorded")
    ));
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert_eq!(
        fixture.status(SESSION),
        GoalStatus::Active,
        "a refused completion leaves the run going"
    );
}

#[test]
fn a_cited_receipt_rewritten_into_a_failure_under_the_same_id_no_longer_completes_the_goal() {
    let (fixture, goal) = settled_evidence_fixture();
    replay_receipt(
        &fixture,
        "rec_pass",
        "rec_pass",
        ReceiptOutcome::Failed,
        3_500,
    );

    let refusal = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect_err("the citation resolves, to a run that failed");

    assert!(matches!(
        &refusal,
        GoalError::EvidenceUnproven {
            criterion_id,
            receipt_id,
            reason,
        } if criterion_id == "c1"
            && receipt_id == "rec_pass"
            && reason.contains("no longer proves success")
            && reason.contains("failed")
    ));
}

#[test]
fn a_cited_receipt_that_was_pruned_no_longer_completes_the_goal() {
    let (fixture, goal) = settled_evidence_fixture();
    {
        let connection = fixture.store.pool().get().expect("check out connection");
        connection
            .execute("DELETE FROM verification_receipt WHERE id = 'rec_pass'", [])
            .expect("prune the receipt");
    }

    let refusal = fixture
        .store
        .complete_checked(SESSION, goal.revision)
        .expect_err("a citation that resolves to nothing proves nothing");

    assert!(matches!(
        &refusal,
        GoalError::EvidenceUnproven { criterion_id, receipt_id, .. }
            if criterion_id == "c1" && receipt_id == "rec_pass"
    ));

    record_receipt(
        &fixture,
        SESSION,
        "rec_pass_again",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        3_600,
    );
    let reproven = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass_again", 3_700)
        .expect("cite the new receipt");
    let completed = fixture
        .store
        .complete_checked(SESSION, reproven.goal.revision)
        .expect("a receipt that still proves success completes the goal")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

#[test]
fn the_unchecked_model_completion_runs_the_same_audit_as_the_checked_one() {
    let (fixture, goal) = evidence_fixture();

    let refusal = fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect_err("a gate with an unguarded side door is not a gate");
    assert!(matches!(
        &refusal,
        GoalError::EvidenceMissing { unsatisfied }
            if unsatisfied == &["c1".to_owned(), "c2".to_owned()]
    ));
    assert_eq!(
        fixture.store.goal(SESSION).expect("read goal"),
        Some(goal.clone()),
        "the refused write leaves the goal untouched"
    );

    let stale = fixture
        .store
        .update_status_as_model_checked(SESSION, ModelStatus::Complete, goal.revision + 7)
        .expect_err("the checked writer still guards the revision");
    assert!(matches!(stale, GoalError::RevisionConflict { .. }));

    let (fixture, settled) = settled_evidence_fixture();
    let completed = fixture
        .store
        .update_status_as_model(SESSION, ModelStatus::Complete)
        .expect("settled criteria pass the audit from this entry point too")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
    assert_eq!(completed.revision, settled.revision + 1);
}

#[test]
fn a_satisfied_criterion_cannot_be_waived_over_its_evidence() {
    let (fixture, goal) = evidence_fixture();
    let satisfied = fixture
        .store
        .satisfy_criterion(SESSION, goal.revision, "c1", "rec_pass", 3_000)
        .expect("prove the first criterion");

    let refusal = fixture
        .store
        .waive_criterion(
            SESSION,
            satisfied.goal.revision,
            "c1",
            "we decided not to check this after all",
            3_100,
        )
        .expect_err("a waiver may excuse a check that was never made, never replace one that was");

    assert!(matches!(
        &refusal,
        GoalError::CriterionAlreadySatisfied { criterion_id, receipt_id }
            if criterion_id == "c1" && receipt_id == "rec_pass"
    ));
    assert!(refusal.is_model_refusal(), "{refusal}");
    let first = fixture
        .store
        .criteria(SESSION)
        .expect("read criteria")
        .into_iter()
        .find(|criterion| criterion.criterion_id == "c1")
        .expect("the first criterion still exists");
    assert_eq!(first.status, GoalCriterionStatus::Satisfied);
    assert_eq!(first.receipt_id.as_deref(), Some("rec_pass"));
    assert_eq!(
        fixture.goal(SESSION).revision,
        satisfied.goal.revision,
        "a refused waiver is not a change to the goal"
    );

    fixture
        .store
        .mark_mutation(SESSION, 4_000)
        .expect("a later edit reopens the criterion");
    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            satisfied.goal.revision,
            "c1",
            "the check cannot be re-run in this environment",
            4_100,
        )
        .expect("an open criterion may be waived");
    assert_eq!(waived.criterion.status, GoalCriterionStatus::Waived);
}

/// Spin until the wall clock has passed `after_ms`, so a goal created next is stamped
/// strictly later. Goals take their creation stamp from the clock, and these tests need
/// two goals whose creation stamps are distinguishable.
fn wait_for_clock_after(after_ms: i64) {
    while crate::store::now_ms().expect("read the clock") <= after_ms {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// A citation proves that a check ran, not that it ran for *this* goal. The reviewer's
/// sequence was: record a passing receipt, then propose the goal, then cite it — the
/// checklist closed and the goal completed, so the mandatory checklist proved citation
/// rather than verification.
#[test]
fn a_receipt_recorded_before_the_goal_existed_cannot_close_its_checklist() {
    let fixture = Fixture::in_memory();
    // The receipt exists first, exactly as a check run earlier in the session would.
    record_receipt(
        &fixture,
        SESSION,
        "rec_before",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        2_000,
    );
    let created = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &["the gates pass".to_owned()], None)
        .expect("a proposal that names a check is accepted");
    let goal = created.goal;
    assert!(
        goal.created_at_ms > 2_000,
        "the goal is stamped from the clock, so the receipt is genuinely older"
    );

    let refusal = fixture
        .store
        .satisfy_criterion(
            SESSION,
            goal.revision,
            "c1",
            "rec_before",
            goal.created_at_ms + 1,
        )
        .expect_err("a check that ran before the goal existed proves nothing about it");
    assert!(
        matches!(
            &refusal,
            GoalError::EvidencePredatesGoal { criterion_id, receipt_id, receipt_at_ms, .. }
                if criterion_id == "c1" && receipt_id == "rec_before" && *receipt_at_ms == 2_000
        ),
        "{refusal}"
    );
    assert!(
        refusal.to_string().contains("run the check again"),
        "the refusal names the remedy: {refusal}"
    );
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert_eq!(
        fixture.store.criteria(SESSION).expect("read criteria")[0].status,
        GoalCriterionStatus::Open,
        "the refused citation left the checklist open"
    );

    let completion = fixture
        .store
        .complete_as_model_checked(SESSION, goal.revision)
        .expect_err("and the goal cannot complete on a checklist nothing closed");
    assert!(
        matches!(
            &completion,
            GoalError::EvidenceMissing { unsatisfied } if unsatisfied == &["c1".to_owned()]
        ),
        "{completion}"
    );
    assert_eq!(fixture.status(SESSION), GoalStatus::Active);

    // The remedy the refusal names works: run the check again, cite the new receipt.
    record_receipt(
        &fixture,
        SESSION,
        "rec_after",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        goal.created_at_ms + 2,
    );
    let satisfied = fixture
        .store
        .satisfy_criterion(
            SESSION,
            goal.revision,
            "c1",
            "rec_after",
            goal.created_at_ms + 3,
        )
        .expect("a check that ran under this goal is evidence about it");
    assert_eq!(satisfied.criterion.status, GoalCriterionStatus::Satisfied);
    let completed = fixture
        .store
        .complete_as_model_checked(SESSION, satisfied.goal.revision)
        .expect("the gate is a checklist to close, not a trap")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

/// Replacement clears the mutation mark, so the receipt goal A was refused for came out
/// of the wash clean and closed goal B's identical criterion. `clear_auxiliary_state`
/// keeps `verification_receipt` on purpose; the bound against the *new* goal's creation
/// stamp is what re-establishes the invariant its own comment claims.
#[test]
fn a_receipt_the_previous_goal_was_refused_for_cannot_close_the_next_ones() {
    let fixture = Fixture::in_memory();
    let first = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "make the parser accept trailing commas",
            &["the parser test suite passes".to_owned()],
            None,
        )
        .expect("propose the first goal")
        .goal;
    record_receipt(
        &fixture,
        SESSION,
        "rec_first",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        first.created_at_ms,
    );
    let satisfied = fixture
        .store
        .satisfy_criterion(
            SESSION,
            first.revision,
            "c1",
            "rec_first",
            first.created_at_ms,
        )
        .expect("the receipt is evidence about the goal it ran under");
    assert_eq!(satisfied.criterion.status, GoalCriterionStatus::Satisfied);

    // A later edit invalidates it, and the first goal is correctly refused.
    let reopened = fixture
        .store
        .mark_mutation(SESSION, first.created_at_ms + 1)
        .expect("record a workspace change");
    assert_eq!(reopened, 1, "the satisfied criterion reopened");
    assert!(matches!(
        fixture
            .store
            .complete_as_model_checked(SESSION, fixture.goal(SESSION).revision),
        Err(GoalError::EvidenceMissing { .. })
    ));

    // The run gives up on that objective and proposes the next one, which wipes the
    // mutation mark along with the checklist.
    fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Cancelled)
        .expect("settle the first goal");
    wait_for_clock_after(first.created_at_ms);
    let second = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "make the parser accept trailing commas after all",
            &["the parser test suite passes".to_owned()],
            None,
        )
        .expect("propose the replacement")
        .goal;
    assert!(second.created_at_ms > first.created_at_ms);

    let refusal = fixture
        .store
        .satisfy_criterion(
            SESSION,
            second.revision,
            "c1",
            "rec_first",
            second.created_at_ms + 1,
        )
        .expect_err("the receipt the first goal could not use cannot close the second's");
    assert!(
        matches!(
            &refusal,
            GoalError::EvidencePredatesGoal { receipt_id, goal_created_at_ms, receipt_at_ms, .. }
                if receipt_id == "rec_first"
                    && *goal_created_at_ms == second.created_at_ms
                    && *receipt_at_ms == first.created_at_ms
        ),
        "{refusal}"
    );
    assert!(matches!(
        fixture
            .store
            .complete_as_model_checked(SESSION, second.revision),
        Err(GoalError::EvidenceMissing { .. })
    ));
    assert_eq!(fixture.status(SESSION), GoalStatus::Active);
}

/// A criterion whose every character renders as nothing is populated in the database and
/// empty on screen, which is the one divergence this audit surface exists to remove.
/// Each of these was accepted as the whole of criterion `c1`.
#[test]
fn a_criterion_that_renders_as_nothing_is_not_a_criterion() {
    for invisible in [
        "\u{200b}", // ZERO WIDTH SPACE
        "\u{feff}", // ZERO WIDTH NO-BREAK SPACE
        "\u{2060}", // WORD JOINER
        "\u{00ad}", // SOFT HYPHEN
        "\u{180e}", // MONGOLIAN VOWEL SEPARATOR
        "\u{200e}", // LEFT-TO-RIGHT MARK
        "\u{00a0}", // NO-BREAK SPACE, already refused as White_Space
        "\u{3000}", // IDEOGRAPHIC SPACE, likewise
        "\u{200b}\u{feff}\u{2060}",
    ] {
        let fixture = Fixture::in_memory();
        let refusal = fixture
            .store
            .create_goal_as_model(SESSION, "ship it", &[invisible.to_owned()], None)
            .expect_err("a checklist a human cannot read is not a checklist");
        assert!(
            matches!(refusal, GoalError::MissingSuccessCriteria),
            "{:?}: {refusal}",
            invisible.escape_unicode().to_string()
        );
        assert_eq!(
            fixture.store.goal(SESSION).expect("read goal"),
            None,
            "and nothing was written"
        );
    }

    // Visible but terse is still a criterion: it renders as itself, so the human sees
    // exactly what the database holds. Refusing it would be a judgement about wording.
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &[".".to_owned()], None)
        .expect("a visible statement is accepted however terse");
    assert_eq!(created.criteria[0].statement, ".");
}

/// The waiver side of the same predicate: a criterion could be closed with a reason
/// that renders as nothing, so the goal document showed a settled checklist and no
/// stated reason.
#[test]
fn a_waiver_reason_that_renders_as_nothing_is_refused() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &["the gates pass".to_owned()], None)
        .expect("propose a goal")
        .goal;

    for invisible in [
        "\u{200b}", "\u{feff}", "\u{2060}", "\u{00ad}", "\u{180e}", "\u{200e}",
    ] {
        let refusal = fixture
            .store
            .waive_criterion(
                SESSION,
                goal.revision,
                "c1",
                invisible,
                goal.created_at_ms + 1,
            )
            .expect_err("a reason nobody can read is not a reason");
        assert!(
            matches!(&refusal, GoalError::EmptyWaiverReason { criterion_id } if criterion_id == "c1"),
            "{:?}: {refusal}",
            invisible.escape_unicode().to_string()
        );
        assert_eq!(
            fixture.store.criteria(SESSION).expect("read criteria")[0].status,
            GoalCriterionStatus::Open,
            "and the criterion is still open"
        );
    }

    // An invisible character alongside real text is left alone: the reason renders.
    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            goal.revision,
            "c1",
            "\u{200b}the gates cannot run in this environment",
            goal.created_at_ms + 2,
        )
        .expect("a reason with visible text is a reason");
    assert_eq!(waived.criterion.status, GoalCriterionStatus::Waived);
}

/// The other half of the waiver rule. The emptiness predicate was shared with the
/// criterion statements and the length bound was not, so `waive_criterion` accepted a
/// 2 000 000-character reason, stored it, and rendered a 2 000 434-byte `goal_update`
/// result from it on every later read.
#[test]
fn a_waiver_reason_is_bounded_like_the_criterion_statement_it_excuses() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &["the gates pass".to_owned()], None)
        .expect("propose a goal")
        .goal;

    let refusal = fixture
        .store
        .waive_criterion(
            SESSION,
            goal.revision,
            "c1",
            &"w".repeat(2_000_000),
            goal.created_at_ms + 1,
        )
        .expect_err("a reason no result could render is not a reason");
    assert!(
        matches!(
            &refusal,
            GoalError::WaiverReasonTooLong { criterion_id, actual: 2_000_000, max }
                if criterion_id == "c1" && *max == MAX_WAIVER_REASON_CHARS
        ),
        "{refusal}"
    );
    assert!(
        refusal.is_model_refusal(),
        "an oversized reason is a corrected call, not a harness failure: {refusal}"
    );
    let criterion = &fixture.store.criteria(SESSION).expect("read criteria")[0];
    assert_eq!(
        criterion.status,
        GoalCriterionStatus::Open,
        "the bound is checked before the write, so nothing was closed"
    );
    assert_eq!(criterion.waiver_reason, None, "and nothing was stored");

    // The boundary is exact, and counted in characters like every other cap here.
    let refusal = fixture
        .store
        .waive_criterion(
            SESSION,
            goal.revision,
            "c1",
            &"w".repeat(MAX_WAIVER_REASON_CHARS + 1),
            goal.created_at_ms + 2,
        )
        .expect_err("one character past the cap is past the cap");
    assert!(
        matches!(&refusal, GoalError::WaiverReasonTooLong { actual, .. } if *actual == 501),
        "{refusal}"
    );
    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            goal.revision,
            "c1",
            &"漢".repeat(MAX_WAIVER_REASON_CHARS),
            goal.created_at_ms + 3,
        )
        .expect("a reason at the cap is accepted, and multi-byte text is not bytes");
    assert_eq!(waived.criterion.status, GoalCriterionStatus::Waived);
    assert_eq!(
        waived
            .criterion
            .waiver_reason
            .as_deref()
            .map(str::len)
            .expect("the reason is stored"),
        MAX_WAIVER_REASON_CHARS * 3,
        "500 characters of CJK is 1500 bytes, and the cap counted the characters"
    );
}

/// The bound is a write bound, not a read bound: a reason an earlier release stored
/// above the cap still reads and still lets the goal complete.
#[test]
fn a_stored_waiver_reason_longer_than_the_cap_still_reads_and_still_completes() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &["the gates pass".to_owned()], None)
        .expect("propose a goal")
        .goal;
    {
        let connection = fixture.store.pool().get().expect("check out connection");
        connection
            .execute(
                "UPDATE goal_criterion SET status = 'waived', waiver_reason = ?2 \
                 WHERE session_id = ?1 AND criterion_id = 'c1'",
                params![SESSION, "w".repeat(2_000_000)],
            )
            .expect("write the row a previous release accepted");
    }
    let criterion = &fixture.store.criteria(SESSION).expect("read criteria")[0];
    assert_eq!(criterion.status, GoalCriterionStatus::Waived);
    assert_eq!(
        criterion
            .waiver_reason
            .as_deref()
            .expect("the stored reason reads back")
            .chars()
            .count(),
        2_000_000,
        "the column keeps what it held: a write bound must not rewrite stored history"
    );
    let completed = fixture
        .store
        .complete_as_model_checked(SESSION, goal.revision)
        .expect("a waived criterion still settles the checklist")
        .expect("the session has a goal");
    assert_eq!(completed.status, GoalStatus::Complete);
}

/// The third criterion-id list on the same model-visible surface. `EvidenceMissing` and
/// the capability ledger were bounded and `UnknownCriterion`'s `known` list was not, so a
/// goal an earlier release stored with an unbounded checklist answered one mistyped id
/// with the whole 34 KB checklist — the defect the other two lists closed, on the sibling
/// the cap missed.
#[test]
fn an_unknown_criterion_id_is_answered_with_a_bounded_list_of_the_ids_that_exist() {
    let fixture = Fixture::in_memory();
    let goal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &["the gates pass".to_owned()], None)
        .expect("propose a goal")
        .goal;
    // `create_goal_as_model` refuses this many now, so the only database that holds this
    // shape is one an earlier release wrote; plant it the way an upgrade would find it.
    {
        let connection = fixture.store.pool().get().expect("check out connection");
        for ordinal in 2..=5_000 {
            connection
                .execute(
                    "INSERT INTO goal_criterion \
                     (session_id, criterion_id, ordinal, statement, status, created_at_ms, \
                      updated_at_ms) \
                     VALUES (?1, ?2, ?3, 'a check', 'open', 1, 1)",
                    params![SESSION, format!("c{ordinal}"), ordinal],
                )
                .expect("write the rows a previous release accepted");
        }
    }

    let refusal = fixture
        .store
        .waive_criterion(
            SESSION,
            goal.revision,
            "c99999",
            "not an id this goal assigned",
            goal.created_at_ms + 1,
        )
        .expect_err("an id this goal never assigned is refused");
    let message = refusal.to_string();
    assert!(
        matches!(
            &refusal,
            GoalError::UnknownCriterion { criterion_id, .. } if criterion_id == "c99999"
        ),
        "{refusal}"
    );
    assert!(
        message.len() < 400,
        "a refusal the model reads on every mistyped id is bounded, not 34 KB: {} bytes",
        message.len()
    );
    assert!(
        message.contains("known criteria: c1, c2, c3"),
        "the ids it should have cited are still named: {message}"
    );
    assert!(
        message.contains("and 4990 more"),
        "and the elision says how many were left out, so nothing reads as the whole list: \
         {message}"
    );
    assert!(
        !message.contains("c4999"),
        "the rest is genuinely gone, not merely wrapped: {message}"
    );
    assert!(
        refusal.is_model_refusal(),
        "a mistyped id is a corrected call, not a harness failure: {refusal}"
    );
}

/// The objective is the headline of the same audit surface, and it kept the `trim`
/// check. An objective of "\u{200b}\u{feff}" was stored on a live goal and printed a
/// blank objective line into the goal document on every later turn.
#[test]
fn an_objective_that_renders_as_nothing_is_refused_on_creation_and_on_rewrite() {
    for invisible in [
        "\u{200b}",
        "\u{feff}",
        "\u{2060}",
        "\u{00ad}",
        "\u{180e}",
        "\u{3164}",
        "\u{200b}\u{feff}",
    ] {
        let fixture = Fixture::in_memory();
        let refusal = fixture
            .store
            .create_goal_as_model(SESSION, invisible, &["the gates pass".to_owned()], None)
            .expect_err("a goal whose purpose renders as nothing states no purpose");
        assert!(
            matches!(refusal, GoalError::EmptyObjective),
            "{:?}: {refusal}",
            invisible.escape_unicode().to_string()
        );
        assert_eq!(
            fixture.store.goal(SESSION).expect("read the goal"),
            None,
            "and no goal row was written"
        );

        // The system path shares the one funnel, so it refuses the same value, and so
        // does the rewrite.
        assert!(matches!(
            fixture.store.create_goal(SESSION, invisible, None),
            Err(GoalError::EmptyObjective)
        ));
        fixture
            .store
            .create_goal(SESSION, "a real objective", None)
            .expect("create the goal");
        assert!(matches!(
            fixture.store.update_objective(SESSION, invisible),
            Err(GoalError::EmptyObjective)
        ));
        assert_eq!(fixture.goal(SESSION).objective, "a real objective");
    }

    // Invisible characters beside real text are left alone: the objective renders.
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "\u{200b}ship it",
            &["the gates pass".to_owned()],
            None,
        )
        .expect("an objective with visible text is an objective");
    assert_eq!(created.goal.objective, "\u{200b}ship it");
}

/// A refused position is a position in the list the model sent. Blank entries were
/// dropped before the length loop counted, so ["real", "\u{200b}", "", <501 chars>] was
/// refused as "success criterion 2" and sent the model to edit the wrong element.
#[test]
fn a_refused_criterion_is_named_by_its_position_in_the_list_that_was_sent() {
    let fixture = Fixture::in_memory();
    let submitted = vec![
        "the gates pass".to_owned(),
        "\u{200b}".to_owned(),
        String::new(),
        "x".repeat(MAX_CRITERION_STATEMENT_CHARS + 1),
    ];
    let refusal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &submitted, None)
        .expect_err("the fourth entry is too long");
    assert!(
        matches!(
            refusal,
            GoalError::SuccessCriterionTooLong {
                ordinal: 4,
                submitted: 4,
                actual: 501,
                max
            } if max == MAX_CRITERION_STATEMENT_CHARS
        ),
        "the ordinal is the position the model can edit, not a position in the filtered \
         list: {refusal}"
    );
    assert_eq!(fixture.store.goal(SESSION).expect("read goal"), None);

    // The count has the mirror-image rule: the cap is compared against the entries that
    // would record, and the refusal names both numbers because they differ.
    let mut submitted = (1..=MAX_SUCCESS_CRITERIA + 3)
        .map(|index| format!("check number {index}"))
        .collect::<Vec<_>>();
    submitted.extend(std::iter::repeat_n("\u{200b}".to_owned(), 5));
    let refusal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &submitted, None)
        .expect_err("35 of the 40 entries record, and 35 is past the cap");
    assert!(
        matches!(
            refusal,
            GoalError::TooManySuccessCriteria {
                submitted: 40,
                recorded: 35,
                max
            } if max == MAX_SUCCESS_CRITERIA
        ),
        "{refusal}"
    );
    let message = refusal.to_string();
    assert!(
        message.contains("sent 40 entries") && message.contains("35 of which record"),
        "a model told only one of the two numbers deletes entries it never had to touch: \
         {message}"
    );
    assert_eq!(fixture.store.goal(SESSION).expect("read goal"), None);
}

/// The checklist is mandatory, model-supplied, stored twice and re-rendered into every
/// later refusal, so it is bounded. 5000 criteria were accepted in 108 ms and made the
/// next refusal 34 213 bytes; a single 2 000 000-character statement was stored whole.
#[test]
fn a_proposed_checklist_is_bounded_in_count_and_in_statement_length() {
    let fixture = Fixture::in_memory();
    let flood: Vec<String> = (1..=5_000)
        .map(|index| format!("check number {index}"))
        .collect();
    let refusal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &flood, None)
        .expect_err("a checklist nobody could review is not a checklist");
    assert!(
        matches!(
            refusal,
            GoalError::TooManySuccessCriteria { submitted: 5_000, recorded: 5_000, max }
                if max == MAX_SUCCESS_CRITERIA
        ),
        "{refusal}"
    );
    assert!(refusal.is_model_refusal(), "{refusal}");
    assert_eq!(
        fixture.store.goal(SESSION).expect("read goal"),
        None,
        "the bound is checked before the write, so there is no partial checklist"
    );

    let long = "a".repeat(2_000_000);
    let refusal = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "ship it",
            &["the gates pass".to_owned(), long],
            None,
        )
        .expect_err("a statement no document could render is not a statement");
    assert!(
        matches!(
            refusal,
            GoalError::SuccessCriterionTooLong {
                ordinal: 2,
                submitted: 2,
                actual: 2_000_000,
                max
            } if max == MAX_CRITERION_STATEMENT_CHARS
        ),
        "{refusal}"
    );
    assert_eq!(fixture.store.goal(SESSION).expect("read goal"), None);

    // The bound is a creation bound, not a read bound: a goal an earlier release stored
    // with a longer checklist still reads and still completes.
    let legacy: Vec<String> = (1..=MAX_SUCCESS_CRITERIA + 8)
        .map(|index| format!("legacy check {index}"))
        .collect();
    let created = fixture
        .store
        .create_goal_with_criteria(SESSION, "an objective from 0.6.6", &legacy, None)
        .expect("the system path still accepts what an earlier release stored");
    assert_eq!(created.criteria.len(), MAX_SUCCESS_CRITERIA + 8);
    assert_eq!(
        fixture
            .store
            .criteria(SESSION)
            .expect("read criteria")
            .len(),
        MAX_SUCCESS_CRITERIA + 8,
        "and every row still reads"
    );
}

/// What a database written by the released store holds, and what happens to it now. The
/// shipped `satisfy_criterion` had only the mutation-mark check, so it accepted a receipt
/// recorded before the goal and wrote exactly this row. The row still reads as satisfied,
/// with its citation, because a bound at a decision must not change what a stored goal
/// looks like; completion is the only thing that changes, and it names the way out.
#[test]
fn a_stored_criterion_citing_a_pre_goal_receipt_still_reads_and_says_how_to_settle_it() {
    let fixture = Fixture::in_memory();
    record_receipt(
        &fixture,
        SESSION,
        "rec_before",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        2_000,
    );
    let goal = fixture
        .store
        .create_goal_as_model(SESSION, "ship it", &["the gates pass".to_owned()], None)
        .expect("propose a goal")
        .goal;
    {
        // The row the released store wrote for this input, verbatim.
        let connection = fixture.store.pool().get().expect("check out connection");
        connection
            .execute(
                "UPDATE goal_criterion \
                 SET status = 'satisfied', waiver_reason = NULL, receipt_id = 'rec_before', \
                     satisfied_at_ms = 2500, updated_at_ms = 2500 \
                 WHERE session_id = ?1 AND criterion_id = 'c1'",
                params![SESSION],
            )
            .expect("write the row an earlier release left behind");
    }

    let stored = fixture.store.criteria(SESSION).expect("read criteria");
    assert_eq!(stored[0].status, GoalCriterionStatus::Satisfied);
    assert_eq!(stored[0].receipt_id.as_deref(), Some("rec_before"));
    assert_eq!(stored[0].satisfied_at_ms, Some(2_500));
    assert_eq!(
        fixture
            .store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal exists")
            .success_criteria,
        ["the gates pass"],
        "and the goal itself is untouched by the new bound"
    );

    for refusal in [
        fixture
            .store
            .complete_as_model_checked(SESSION, goal.revision)
            .expect_err("the audit re-reads the citation, because a row is data"),
        fixture
            .store
            .complete_checked(SESSION, goal.revision)
            .expect_err("and the human is held to the same reading of the same row"),
    ] {
        assert!(
            matches!(
                &refusal,
                GoalError::EvidencePredatesGoal { criterion_id, receipt_id, .. }
                    if criterion_id == "c1" && receipt_id == "rec_before"
            ),
            "{refusal}"
        );
        assert!(
            refusal.to_string().contains("run the check again"),
            "a stored row that can no longer close the goal says what will: {refusal}"
        );
    }

    // Both remedies the refusal names work, on the row that was already there.
    record_receipt(
        &fixture,
        SESSION,
        "rec_now",
        ReceiptOutcome::Passed,
        zuno_db::verification::ExitAuthority::Authoritative,
        goal.created_at_ms + 1,
    );
    let resatisfied = fixture
        .store
        .satisfy_criterion(
            SESSION,
            goal.revision,
            "c1",
            "rec_now",
            goal.created_at_ms + 2,
        )
        .expect("re-citing a fresh receipt over a satisfied row is allowed");
    assert_eq!(resatisfied.criterion.receipt_id.as_deref(), Some("rec_now"));
    let completed = fixture
        .store
        .complete_checked(SESSION, resatisfied.goal.revision)
        .expect("and then it completes")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
}

// ---------------------------------------------------------------------------------
// Upgrading a `0.6.6` database: the checklist lived only in `goal.success_criteria`.
// ---------------------------------------------------------------------------------

/// `git show v0.6.6:crates/zuno-goal/src/store.rs`, `SCHEMA` verbatim.
///
/// The released `0.6.6` goal table. Identical to today's, which is the point: the row
/// this test inserts is exactly the row that release wrote, and nothing about the goal
/// itself needs to move.
const V0_6_6_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS goal (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 1),
    objective TEXT NOT NULL,
    success_criteria TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active',
        'paused',
        'blocked',
        'usage_limited',
        'budget_limited',
        'complete',
        'cancelled'
    )),
    blocked_reason TEXT,
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    usage_known INTEGER NOT NULL DEFAULT 1 CHECK(usage_known IN (0, 1)),
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
)";

/// `git show v0.6.6:crates/zuno-goal/src/store.rs`, `AUXILIARY_SCHEMA` verbatim.
///
/// No `goal_criterion`, no `goal_kind`, no `goal_mutation_mark`, no
/// `goal_request_usage`, no `goal_capability_claim`; a `goal_pause` `CHECK` without
/// `turn_budget`; and the unguarded history trigger. Every one of those is what
/// `from_pool` has to bring forward, in one transaction, without touching the rows.
const V0_6_6_AUXILIARY_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS goal_continuation_deferral (
    session_id TEXT PRIMARY KEY NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_pending_failure_signal (
    session_id TEXT PRIMARY KEY NOT NULL,
    signal TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_failure_streak (
    session_id TEXT PRIMARY KEY NOT NULL,
    signal TEXT NOT NULL,
    consecutive_turns INTEGER NOT NULL CHECK(consecutive_turns BETWEEN 1 AND 3)
);
CREATE TABLE IF NOT EXISTS goal_pause (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN (
        'user_interruption',
        'plan_mode',
        'human_input',
        'permission',
        'authentication',
        'uncertain_side_effect'
    )),
    human_request_id TEXT,
    paused_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_retry (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK(attempt >= 1),
    reason TEXT NOT NULL CHECK(reason IN (
        'rate_limited',
        'provider_transient',
        'provider_stream',
        'provider_retry_deadline',
        'database_busy',
        'step_limit',
        'empty_assistant_message',
        'context_limit',
        'context_compacted',
        'tool_transient'
    )),
    delay_ms INTEGER NOT NULL CHECK(delay_ms >= 0),
    retry_at_ms INTEGER NOT NULL,
    scheduled_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_history (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    goal_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 1),
    objective TEXT NOT NULL,
    success_criteria TEXT NOT NULL,
    status TEXT NOT NULL,
    blocked_reason TEXT,
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL,
    usage_known INTEGER NOT NULL CHECK(usage_known IN (0, 1)),
    time_used_seconds INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(goal_id, revision)
);
CREATE INDEX IF NOT EXISTS goal_history_session_sequence
    ON goal_history(session_id, sequence);
CREATE TRIGGER IF NOT EXISTS goal_history_after_insert
AFTER INSERT ON goal
BEGIN
    INSERT INTO goal_history (
        session_id, goal_id, revision, objective, success_criteria, status, blocked_reason,
        token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, updated_at_ms
    ) VALUES (
        NEW.session_id, NEW.goal_id, NEW.revision, NEW.objective,
        NEW.success_criteria, NEW.status, NEW.blocked_reason, NEW.token_budget,
        NEW.tokens_used, NEW.usage_known, NEW.time_used_seconds, NEW.created_at_ms,
        NEW.updated_at_ms
    );
END;
CREATE TRIGGER IF NOT EXISTS goal_history_after_update
AFTER UPDATE ON goal
BEGIN
    INSERT INTO goal_history (
        session_id, goal_id, revision, objective, success_criteria, status, blocked_reason,
        token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, updated_at_ms
    ) VALUES (
        NEW.session_id, NEW.goal_id, NEW.revision, NEW.objective,
        NEW.success_criteria, NEW.status, NEW.blocked_reason, NEW.token_budget,
        NEW.tokens_used, NEW.usage_known, NEW.time_used_seconds, NEW.created_at_ms,
        NEW.updated_at_ms
    );
END;";

/// When the `0.6.6` goals below were proposed, in Unix milliseconds.
const V0_6_6_CREATED_AT_MS: i64 = 1_700_000_000_000;

/// One row of `goal` or `goal_history` as `0.6.6` wrote it, read back raw.
///
/// Raw rather than through [`GoalStore`] so the comparison is on the bytes the database
/// holds and not on what this release's reader makes of them.
#[derive(Debug, PartialEq, Eq)]
struct RawGoalRow {
    session_id: String,
    goal_id: String,
    revision: i64,
    objective: String,
    success_criteria: String,
    status: String,
    blocked_reason: Option<String>,
    token_budget: Option<i64>,
    tokens_used: i64,
    usage_known: i64,
    time_used_seconds: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
}

/// A file database holding the goal tables exactly as `0.6.6` created them.
///
/// A file and not `:memory:` because the point of the second open below is that it is
/// a second *process* attaching to the same database. The shared tables beside the goal
/// ones are current: `zuno-db` owns their forward migration and pins it in its own
/// tests, and the receipt the closing step cites has to land somewhere.
fn v0_6_6_database() -> (TempDir, Arc<zuno_db::Pool>) {
    let directory = tempfile::tempdir().expect("create database directory");
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::File(
            directory.path().join("zuno-0.6.6.db"),
        ))
        .expect("open the file database"),
    );
    let mut connection = pool.open_connection().expect("open connection");
    zuno_db::migration::apply(&mut connection).expect("apply the shared schema");
    connection
        .execute_batch(V0_6_6_SCHEMA)
        .expect("create the 0.6.6 goal table");
    connection
        .execute_batch(V0_6_6_AUXILIARY_SCHEMA)
        .expect("create the 0.6.6 auxiliary tables");
    assert!(
        !table_exists_on(&connection, "goal_criterion"),
        "0.6.6 had no goal_criterion table, or this fixture proves nothing"
    );
    drop(connection);
    (directory, pool)
}

fn table_exists_on(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .expect("query sqlite_master")
        .is_some()
}

/// Insert the row `0.6.6`'s `goal_propose` wrote: the criteria in the JSON column only.
///
/// Goes through the `0.6.6` insert trigger, so `goal_history` gains the row that
/// release recorded for it too.
fn insert_v0_6_6_goal(
    connection: &Connection,
    session_id: &str,
    goal_id: &str,
    success_criteria: &str,
    status: &str,
) {
    connection
        .execute(
            "INSERT INTO goal (session_id, goal_id, revision, objective, success_criteria, \
             status, blocked_reason, token_budget, tokens_used, usage_known, \
             time_used_seconds, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, 1, ?3, ?4, ?5, NULL, 50000, 0, 1, 0, ?6, ?6)",
            params![
                session_id,
                goal_id,
                format!("objective of {session_id}"),
                success_criteria,
                status,
                V0_6_6_CREATED_AT_MS,
            ],
        )
        .expect("insert the 0.6.6 goal row");
}

fn raw_goal_rows(connection: &Connection, table: &str, session_id: &str) -> Vec<RawGoalRow> {
    let order = if table == "goal_history" {
        "sequence"
    } else {
        "session_id"
    };
    connection
        .prepare(&format!(
            "SELECT session_id, goal_id, revision, objective, success_criteria, status, \
             blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, \
             created_at_ms, updated_at_ms FROM {table} WHERE session_id = ?1 ORDER BY {order}"
        ))
        .expect("prepare raw read")
        .query_map(params![session_id], |row| {
            Ok(RawGoalRow {
                session_id: row.get(0)?,
                goal_id: row.get(1)?,
                revision: row.get(2)?,
                objective: row.get(3)?,
                success_criteria: row.get(4)?,
                status: row.get(5)?,
                blocked_reason: row.get(6)?,
                token_budget: row.get(7)?,
                tokens_used: row.get(8)?,
                usage_known: row.get(9)?,
                time_used_seconds: row.get(10)?,
                created_at_ms: row.get(11)?,
                updated_at_ms: row.get(12)?,
            })
        })
        .expect("read raw rows")
        .collect::<Result<_, _>>()
        .expect("collect raw rows")
}

/// `(criterion_id, created_at_ms, updated_at_ms)` for every criterion row of a session.
fn criterion_stamps(connection: &Connection, session_id: &str) -> Vec<(String, i64, i64)> {
    connection
        .prepare(
            "SELECT criterion_id, created_at_ms, updated_at_ms FROM goal_criterion \
             WHERE session_id = ?1 ORDER BY ordinal",
        )
        .expect("prepare stamp read")
        .query_map(params![session_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("read stamps")
        .collect::<Result<_, _>>()
        .expect("collect stamps")
}

fn criterion_row_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM goal_criterion", [], |row| row.get(0))
        .expect("count criterion rows")
}

/// A goal a `0.6.6` database holds only in its column recovers its checklist at open.
///
/// `0.6.6` had no `goal_criterion` table, and its `goal_propose` stored the model's
/// `success_criteria` in the `goal.success_criteria` column. Before this backfill an
/// upgraded database presented a goal whose column read `["the gates pass", "the docs
/// build"]` with zero criterion rows, and once the run's first reported write escalated
/// it, both completion paths refused with "cannot complete without success criteria" —
/// false about the row the database holds — while `goal_propose` was refused with
/// `GoalNotReplaceable`, so the only exit was cancelling the goal. Measured on this exact
/// fixture before the change: 0 rows, that message, on both paths.
///
/// Pinned against the column, not table presence: ids, statements, statuses, ordinals,
/// stamps and count; the goal and history rows byte-for-byte before and after; the
/// refusal naming the real ids; a second open changing nothing; and the exit the refusal
/// names actually working.
#[test]
fn a_goal_a_0_6_6_database_holds_only_in_its_column_recovers_its_checklist_at_open() {
    const UPGRADED: &str = "ses_0_6_6_upgraded";
    let (_directory, pool) = v0_6_6_database();
    let spill = tempfile::tempdir().expect("create spill directory");
    let connection = pool.open_connection().expect("open connection");
    insert_v0_6_6_goal(
        &connection,
        UPGRADED,
        "goal-0-6-6",
        r#"["the gates pass","the docs build"]"#,
        "active",
    );
    // A second revision through 0.6.6's own trigger, so the goal carries accounting and
    // the history holds more than the creation row.
    connection
        .execute(
            "UPDATE goal SET revision = 2, tokens_used = 1234, time_used_seconds = 77, \
             updated_at_ms = ?2 WHERE session_id = ?1",
            params![UPGRADED, V0_6_6_CREATED_AT_MS + 100_000],
        )
        .expect("record a 0.6.6 revision");
    let goal_before = raw_goal_rows(&connection, "goal", UPGRADED);
    let history_before = raw_goal_rows(&connection, "goal_history", UPGRADED);
    assert_eq!(
        history_before.len(),
        2,
        "the fixture must hold real history"
    );
    drop(connection);

    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("attach to the 0.6.6 database");
    let connection = pool.get().expect("check out a connection");

    // Recovered rows against the column, not against table presence.
    let goal = store
        .goal(UPGRADED)
        .expect("read the goal")
        .expect("the goal is there");
    assert_eq!(goal.success_criteria, ["the gates pass", "the docs build"]);
    let criteria = store.criteria(UPGRADED).expect("read criteria");
    assert_eq!(criteria.len(), goal.success_criteria.len());
    for (index, (criterion, statement)) in criteria.iter().zip(&goal.success_criteria).enumerate() {
        assert_eq!(criterion.criterion_id, format!("c{}", index + 1));
        assert_eq!(
            criterion.ordinal,
            i64::try_from(index).expect("small index")
        );
        assert_eq!(&criterion.statement, statement);
        assert_eq!(criterion.status, GoalCriterionStatus::Open);
        assert_eq!(criterion.waiver_reason, None);
        assert_eq!(criterion.receipt_id, None);
        assert_eq!(criterion.satisfied_at_ms, None);
    }
    assert_eq!(
        criterion_stamps(&connection, UPGRADED),
        [
            ("c1".to_owned(), V0_6_6_CREATED_AT_MS, V0_6_6_CREATED_AT_MS),
            ("c2".to_owned(), V0_6_6_CREATED_AT_MS, V0_6_6_CREATED_AT_MS),
        ],
        "stamped from the goal's own creation, not from the clock at open"
    );
    // The goal and its history are the rows 0.6.6 wrote, byte for byte.
    assert_eq!(raw_goal_rows(&connection, "goal", UPGRADED), goal_before);
    assert_eq!(
        raw_goal_rows(&connection, "goal_history", UPGRADED),
        history_before
    );

    // The path the reviewer measured: the first reported write escalates the goal, the
    // model then claims completion. The refusal now names the ids the run has to close.
    assert_eq!(
        store
            .escalate_to_change(UPGRADED, "wrote src/main.rs", V0_6_6_CREATED_AT_MS + 5)
            .expect("escalate"),
        GoalKind::Change
    );
    for refusal in [
        store
            .complete_as_model_checked(UPGRADED, goal.revision)
            .expect_err("the checklist is open"),
        store
            .complete_checked(UPGRADED, goal.revision)
            .expect_err("on the human's path too"),
    ] {
        assert!(
            matches!(
                &refusal,
                GoalError::EvidenceMissing { unsatisfied } if unsatisfied == &["c1", "c2"]
            ),
            "{refusal}"
        );
        let message = refusal.to_string();
        assert!(message.contains("c1, c2"), "{message}");
        assert!(
            !message.contains("without success criteria"),
            "the refusal must stop saying the goal has no criteria: {message}"
        );
    }

    // A second process attaching to the same file changes nothing.
    let stamps_before = criterion_stamps(&connection, UPGRADED);
    let history_before = raw_goal_rows(&connection, "goal_history", UPGRADED);
    drop(connection);
    drop(store);
    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("attach a second time");
    let connection = pool.get().expect("check out a connection");
    assert_eq!(store.criteria(UPGRADED).expect("read criteria"), criteria);
    assert_eq!(criterion_stamps(&connection, UPGRADED), stamps_before);
    assert_eq!(
        raw_goal_rows(&connection, "goal_history", UPGRADED),
        history_before
    );
    assert_eq!(criterion_row_count(&connection), 2);

    // And the exit the refusal names exists: settle each id, then complete.
    zuno_db::verification::record(
        &connection,
        &zuno_db::verification::NewVerificationReceipt {
            id: "rec_gates".to_owned(),
            session_id: UPGRADED.to_owned(),
            turn_id: Some("turn-upgrade".to_owned()),
            tool_call_id: "call-rec_gates".to_owned(),
            tool_id: "shell".to_owned(),
            summary: "cargo test -p zuno-goal".to_owned(),
            workdir: None,
            exit_code: Some(0),
            exit_authority: zuno_db::verification::ExitAuthority::Authoritative,
            outcome: ReceiptOutcome::Passed,
            git_head: None,
            output_digest: None,
            detail: None,
            time_created: V0_6_6_CREATED_AT_MS + 10,
        },
    )
    .expect("record the receipt");
    drop(connection);
    let revision = store
        .goal(UPGRADED)
        .expect("read the goal")
        .expect("goal exists")
        .revision;
    let satisfied = store
        .satisfy_criterion(
            UPGRADED,
            revision,
            "c1",
            "rec_gates",
            V0_6_6_CREATED_AT_MS + 11,
        )
        .expect("cite the receipt for c1");
    let waived = store
        .waive_criterion(
            UPGRADED,
            satisfied.goal.revision,
            "c2",
            "the docs are built by the release workflow, not this run",
            V0_6_6_CREATED_AT_MS + 12,
        )
        .expect("waive c2");
    let completed = store
        .complete_as_model_checked(UPGRADED, waived.goal.revision)
        .expect("the upgraded goal completes once its checklist is settled")
        .expect("goal exists");
    assert_eq!(completed.status, GoalStatus::Complete);
    assert_eq!(completed.goal_id, "goal-0-6-6", "the same goal instance");
    assert_eq!(
        completed.tokens_used, 1234,
        "with the accounting it carried"
    );
}

/// A goal a `0.6.6` database finished keeps reading as it did until it is live again.
///
/// The decision `checklist_dormant` documents, measured rather than asserted: for a
/// `complete` and a `cancelled` `0.6.6` row the rendered document and the result of a
/// repeated `complete` are byte-for-byte what they were before the backfill existed,
/// because nothing is minted for them at open. The moment the finished goal is written
/// back to a live status it gets the rows the column names, and a completion is then held
/// to them; a `cancelled` goal cannot be written back, so it stays as it is.
#[test]
fn a_goal_a_0_6_6_database_finished_reads_as_it_did_until_it_is_made_live_again() {
    const FINISHED: &str = "ses_0_6_6_complete";
    const ABANDONED: &str = "ses_0_6_6_cancelled";
    let (_directory, pool) = v0_6_6_database();
    let spill = tempfile::tempdir().expect("create spill directory");
    let connection = pool.open_connection().expect("open connection");
    insert_v0_6_6_goal(
        &connection,
        FINISHED,
        "goal-0-6-6-done",
        r#"["shipped"]"#,
        "complete",
    );
    insert_v0_6_6_goal(
        &connection,
        ABANDONED,
        "goal-0-6-6-gone",
        r#"["never mind"]"#,
        "cancelled",
    );
    drop(connection);

    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("attach to the 0.6.6 database");
    let connection = pool.get().expect("check out a connection");
    assert_eq!(
        criterion_row_count(&connection),
        0,
        "nothing is minted for a finished goal"
    );
    let notes = crate::projection::Notes::default();
    for (session_id, expected) in [
        (FINISHED, GoalStatus::Complete),
        (ABANDONED, GoalStatus::Cancelled),
    ] {
        let goal = store
            .goal(session_id)
            .expect("read the goal")
            .expect("goal exists");
        assert_eq!(goal.status, expected);
        assert_eq!(
            goal.success_criteria.len(),
            1,
            "the column still names the check"
        );
        let criteria = store.criteria(session_id).expect("read criteria");
        assert!(criteria.is_empty());
        // What the document showed for this row before the backfill existed was the
        // render of the goal with no criterion rows; it must still be that, byte for byte.
        let document = crate::projection::render(&goal, &criteria, &notes);
        assert_eq!(document, crate::projection::render(&goal, &[], &notes));
        assert!(document.contains("_This goal has no success criteria._"));
        // And a repeated `complete` is the same idempotent no-op it was.
        let repeated = store
            .complete_as_model_checked(session_id, goal.revision)
            .expect("a finished goal is not refused for evidence it was never asked for")
            .expect("goal exists");
        assert_eq!(repeated.status, expected);
    }

    // Written back to a live status — the system's `active` here; the model's own
    // `blocked` is the other write that leaves `complete`, through the same statement
    // path — the checklist the column names comes with it.
    let revision = store
        .goal(FINISHED)
        .expect("read the goal")
        .expect("goal exists")
        .revision;
    let revived = store
        .set_status_as_system_checked(FINISHED, SystemStatus::Active, revision)
        .expect("set active")
        .expect("goal exists");
    assert_eq!(revived.status, GoalStatus::Active);
    let criteria = store.criteria(FINISHED).expect("read criteria");
    assert_eq!(criteria.len(), 1);
    assert_eq!(criteria[0].criterion_id, "c1");
    assert_eq!(criteria[0].statement, "shipped");
    assert_eq!(criteria[0].status, GoalCriterionStatus::Open);
    assert_eq!(
        criterion_stamps(&connection, FINISHED),
        [("c1".to_owned(), V0_6_6_CREATED_AT_MS, V0_6_6_CREATED_AT_MS)]
    );
    let refusal = store
        .complete_as_model_checked(FINISHED, revived.revision)
        .expect_err("a revived goal is held to the checklist it recorded");
    assert!(
        matches!(&refusal, GoalError::EvidenceMissing { unsatisfied } if unsatisfied == &["c1"]),
        "{refusal}"
    );

    // A cancelled goal cannot be written back to a live status, so nothing is minted.
    let revision = store
        .goal(ABANDONED)
        .expect("read the goal")
        .expect("goal exists")
        .revision;
    let still_cancelled = store
        .set_status_as_system_checked(ABANDONED, SystemStatus::Active, revision)
        .expect("the write is accepted")
        .expect("goal exists");
    assert_eq!(still_cancelled.status, GoalStatus::Cancelled);
    assert!(store.criteria(ABANDONED).expect("read criteria").is_empty());
}

/// A `success_criteria` column that is not a JSON array of strings is left alone.
///
/// Corruption, not input: nothing this crate or `0.6.6` wrote produces it. The backfill
/// neither guesses at it nor fails the whole open over one session's row — that would
/// take every other goal in the database down with it — so the row keeps failing closed
/// exactly where it failed before, on the read of that session, while its neighbours are
/// brought forward.
#[test]
fn a_success_criteria_column_that_is_not_a_json_array_of_strings_is_left_alone_at_open() {
    const NOT_JSON: &str = "ses_0_6_6_not_json";
    const NOT_STRINGS: &str = "ses_0_6_6_not_strings";
    const SOUND: &str = "ses_0_6_6_sound";
    let (_directory, pool) = v0_6_6_database();
    let spill = tempfile::tempdir().expect("create spill directory");
    let connection = pool.open_connection().expect("open connection");
    insert_v0_6_6_goal(&connection, NOT_JSON, "goal-not-json", "not json", "active");
    insert_v0_6_6_goal(
        &connection,
        NOT_STRINGS,
        "goal-not-strings",
        "[1, 2]",
        "active",
    );
    insert_v0_6_6_goal(
        &connection,
        SOUND,
        "goal-sound",
        r#"["the gates pass"]"#,
        "active",
    );
    let corrupt_before = raw_goal_rows(&connection, "goal", NOT_JSON);
    drop(connection);

    let store = GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("one corrupt row must not fail the open for every session");
    let connection = pool.get().expect("check out a connection");
    assert!(store.criteria(NOT_JSON).expect("read criteria").is_empty());
    assert!(
        store
            .criteria(NOT_STRINGS)
            .expect("read criteria")
            .is_empty()
    );
    assert_eq!(
        store
            .criteria(SOUND)
            .expect("read criteria")
            .iter()
            .map(|criterion| criterion.statement.as_str())
            .collect::<Vec<_>>(),
        ["the gates pass"],
        "the neighbour is brought forward"
    );
    assert_eq!(criterion_row_count(&connection), 1);
    assert_eq!(
        raw_goal_rows(&connection, "goal", NOT_JSON),
        corrupt_before,
        "left exactly as it was"
    );
    // Reported where it was reported before: on the read of that session.
    assert!(matches!(store.goal(NOT_JSON), Err(GoalError::Db(_))));
    assert!(matches!(store.goal(NOT_STRINGS), Err(GoalError::Db(_))));
}
