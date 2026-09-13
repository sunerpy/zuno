CREATE TABLE session_work_cycle (
  session_id TEXT NOT NULL,
  cycle_id TEXT NOT NULL,
  anchor_message_id TEXT,
  data TEXT NOT NULL CHECK(json_valid(data)),
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  PRIMARY KEY(session_id,cycle_id),
  FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);
CREATE INDEX session_work_cycle_updated_idx ON session_work_cycle(session_id,time_updated,cycle_id);
