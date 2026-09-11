CREATE TABLE session_input_receipt (
  input_id text NOT NULL PRIMARY KEY REFERENCES session_input(id) ON DELETE CASCADE,
  state text NOT NULL CHECK (state IN ('admitted','recorded','applied','completed','failed','cancelled')),
  delivery text NOT NULL CHECK (delivery IN ('queue','steer')),
  turn_id text CHECK (turn_id IS NULL OR length(turn_id) BETWEEN 1 AND 256),
  applied_at integer,
  completed_at integer,
  stop_reason text CHECK (stop_reason IS NULL OR stop_reason IN ('end_turn','max_tokens','refusal','cancelled')),
  error text,
  time_updated integer NOT NULL,
  CHECK (state <> 'applied' OR (turn_id IS NOT NULL AND applied_at IS NOT NULL)),
  CHECK (state <> 'completed' OR (turn_id IS NOT NULL AND completed_at IS NOT NULL AND stop_reason IS NOT NULL)),
  CHECK (stop_reason IS NULL OR state IN ('completed','cancelled'))
);
CREATE INDEX session_input_receipt_turn_state_idx
  ON session_input_receipt(turn_id,state,input_id);
