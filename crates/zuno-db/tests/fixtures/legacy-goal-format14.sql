-- Optional GoalStore tables frozen from v0.10.31 (5619205d60aef572e484646dd9ab0563e1ef8066):
-- git show v0.10.31:crates/zuno-goal/src/store.rs (SCHEMA and these AUXILIARY_SCHEMA tables).
-- The format-14 database does not require GoalStore to have attached.
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
        'uncertain_side_effect',
        'turn_budget',
        'no_progress'
    )),
    human_request_id TEXT,
    paused_at_ms INTEGER NOT NULL
);
INSERT INTO goal
  (session_id,goal_id,revision,objective,success_criteria,status,blocked_reason,
   token_budget,tokens_used,usage_known,time_used_seconds,created_at_ms,updated_at_ms)
VALUES ('ses_fixture_0001','goal-published',7,'Paused work — 保持暂停','[ "published criterion" ]',
  'paused','keep original reason',10000,731,1,42,1735690010000,1735690013000);
INSERT INTO goal_pending_failure_signal VALUES ('ses_fixture_0001','legacy:offline');
INSERT INTO goal_failure_streak VALUES ('ses_fixture_0001','legacy:offline',2);
INSERT INTO goal_pause VALUES ('ses_fixture_0001','goal-published','user_interruption',NULL,1735690013000);
