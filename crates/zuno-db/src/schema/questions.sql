CREATE TABLE question_interaction (
  request_id text PRIMARY KEY REFERENCES human_request(id) ON DELETE CASCADE,
  purpose text NOT NULL CHECK (purpose IN ('clarification','required_input','plan_authorization')),
  mode text NOT NULL CHECK (mode IN ('blocking','deferred')),
  definition text NOT NULL CHECK (json_valid(definition) AND json_type(definition) = 'object'),
  decision text CHECK (decision IN ('approve','decline')),
  authorization text CHECK (authorization IN ('waiting_for_handoff','applied','invalidated')),
  risk_reason text,
  authorization_input_id text
);
CREATE TABLE question_action_receipt (
  request_id text NOT NULL REFERENCES human_request(id) ON DELETE CASCADE,
  command_id text NOT NULL,
  command_json text NOT NULL CHECK (json_valid(command_json)),
  receipt text NOT NULL CHECK (json_valid(receipt)),
  time_created integer NOT NULL,
  PRIMARY KEY (request_id, command_id)
);
CREATE INDEX question_interaction_purpose_authorization_idx
  ON question_interaction(purpose, authorization, request_id);
