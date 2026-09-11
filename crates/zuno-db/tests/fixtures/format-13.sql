-- Zuno format 13 delta, frozen from v0.10.30 (fb875e92833a68c77daf3c05e7cda566db055c14).
-- Load after the released format-7 through format-12 fixture chain.
-- questions.sql SHA-256: c15bc8e71e662618646653915b15aa46dda371c5e3c4f5b435835a83d25353c4
-- BEGIN v0.10.30 questions.sql
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
-- END v0.10.30 questions.sql
-- scheduling.sql SHA-256: 2e0541aac322b475128873def272500a298b1b4c24c2b56af8556ad486e61808
-- BEGIN v0.10.30 scheduling.sql
-- Format 13 stores scheduling eligibility on the existing execution row.
-- NULL preserves legacy rows; the migration repairs only structured no-progress pauses.
ALTER TABLE session_execution_state ADD COLUMN scheduling text
  CHECK (scheduling IS NULL OR (json_valid(scheduling) AND json_type(scheduling) = 'object'));
-- END v0.10.30 scheduling.sql
INSERT INTO human_request
  (id,session_id,kind,state,payload,response,revision,time_created,time_updated)
VALUES ('req_format13','ses_fixture_0001','input','pending',
  '{"source":"question_async","questions":[{"question":"Any correction?","header":"Notes","options":[]}]}',
  '{"draftAnswers":{"q1":["keep this draft"]}}',2,1735690000000,1735690001000);
INSERT INTO question_interaction(request_id,purpose,mode,definition)
VALUES ('req_format13','clarification','deferred',
  '{"origin":{"sessionId":"ses_fixture_0001"},"questions":[{"id":"q1","question":"Any correction?","header":"Notes","options":[]}],"plan":null,"initialMode":"deferred","handoffCompleted":false}');
UPDATE zuno_schema SET format=13 WHERE singleton=1 AND format=12;
