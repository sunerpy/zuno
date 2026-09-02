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
fn evidence_fixture() -> (Fixture, Goal) {
    let fixture = Fixture::in_memory();
    let created = fixture
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
    (fixture, created.goal)
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

#[test]
fn a_question_goal_completes_without_any_evidence() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "explain how the budget flip works",
            &["the answer names the statement that flips the status".to_owned()],
            None,
        )
        .expect("create goal with criteria");

    let completed = fixture
        .store
        .complete_checked(SESSION, created.goal.revision)
        .expect("a goal that changed nothing has nothing to verify")
        .expect("goal exists");

    assert_eq!(completed.status, GoalStatus::Complete);
    assert_eq!(
        fixture.store.kind(SESSION).expect("read kind"),
        GoalKind::Question,
        "a goal is presumed harmless until something reports a change"
    );
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

#[test]
fn a_goal_proposed_without_criteria_stays_a_question_until_its_first_write_and_then_cannot_complete()
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

#[test]
fn a_goal_proposed_without_criteria_completes_as_a_question_while_nothing_was_written() {
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
