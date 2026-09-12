CREATE TABLE goal_turn_observation (
    session_id TEXT NOT NULL,
    goal_id TEXT NOT NULL,
    cycle_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    signal TEXT NOT NULL CHECK(length(trim(signal)) > 0),
    time_created INTEGER NOT NULL,
    PRIMARY KEY(session_id, goal_id, cycle_id, turn_id)
);
CREATE TABLE goal_turn_audit (
    session_id TEXT NOT NULL,
    goal_id TEXT NOT NULL,
    cycle_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    audit TEXT NOT NULL CHECK(json_valid(audit)),
    time_recorded INTEGER NOT NULL,
    PRIMARY KEY(session_id, goal_id, cycle_id, turn_id)
);
CREATE TABLE goal_cycle_failure (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    cycle_id TEXT NOT NULL,
    active_turn_id TEXT NOT NULL,
    signal TEXT,
    consecutive_turns INTEGER NOT NULL CHECK(consecutive_turns BETWEEN 0 AND 3),
    CHECK((signal IS NULL AND consecutive_turns = 0) OR
          (signal IS NOT NULL AND length(trim(signal)) > 0 AND consecutive_turns > 0))
);
