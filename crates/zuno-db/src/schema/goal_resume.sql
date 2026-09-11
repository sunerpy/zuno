-- Extend the released format-13 companion without rewriting its payloads.
CREATE TABLE question_interaction_v14 (
  request_id text PRIMARY KEY REFERENCES human_request(id) ON DELETE CASCADE,
  purpose text NOT NULL CHECK (purpose IN ('clarification','required_input','plan_authorization','goal_resume')),
  mode text NOT NULL CHECK (mode IN ('blocking','deferred')),
  definition text NOT NULL CHECK (json_valid(definition) AND json_type(definition) = 'object'),
  decision text CHECK (decision IN ('approve','decline')),
  authorization text CHECK (authorization IN ('waiting_for_handoff','applied','invalidated')),
  risk_reason text,
  authorization_input_id text
);
INSERT INTO question_interaction_v14
  (request_id,purpose,mode,definition,decision,authorization,risk_reason,authorization_input_id)
SELECT request_id,purpose,mode,definition,decision,authorization,risk_reason,authorization_input_id
FROM question_interaction;
DROP TABLE question_interaction;
ALTER TABLE question_interaction_v14 RENAME TO question_interaction;
CREATE INDEX question_interaction_purpose_authorization_idx
  ON question_interaction(purpose, authorization, request_id);
