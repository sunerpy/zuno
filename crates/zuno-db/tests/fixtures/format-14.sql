-- Zuno format 14 delta, frozen from v0.10.31 (5619205d60aef572e484646dd9ab0563e1ef8066).
-- Load after the released format-7 through format-13 fixture chain.
-- Sources: git show v0.10.31:crates/zuno-db/src/schema/{goal_resume,input_receipt,context_usage}.sql
-- The receipt backfill is from v0.10.31:crates/zuno-db/src/schema.rs::up_runtime_consistency.
-- goal_resume.sql SHA-256: 3f43111a4cb52969baf99f0aeb6ceb985bc7a8b1c5ac02c2a8db5b78f0b82884
-- BEGIN v0.10.31 goal_resume.sql
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
-- END v0.10.31 goal_resume.sql
-- input_receipt.sql SHA-256: 089e57540e97618fdd337b76e7e7ea8bfde2a0ec71f9b0a6fe1d750ece5043e6
-- BEGIN v0.10.31 input_receipt.sql
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
-- END v0.10.31 input_receipt.sql
-- context_usage.sql SHA-256: f639d6495ddd5e84afef1ebf679820ab4e6ecbbde8a8f84e6e152de7de34bc96
-- BEGIN v0.10.31 context_usage.sql
CREATE TABLE `session_context_usage` (
  `session_id` text NOT NULL,
  `source` text NOT NULL CHECK (`source` IN ('main', 'child', 'learning', 'compaction', 'auxiliary')),
  `revision` integer NOT NULL CHECK (`revision` >= 0),
  `context_epoch` integer NOT NULL CHECK (`context_epoch` >= 0),
  `state_json` text NOT NULL CHECK (json_valid(`state_json`)),
  `time_updated` integer NOT NULL,
  PRIMARY KEY (`session_id`, `source`),
  CONSTRAINT `fk_session_context_usage_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE INDEX `session_context_usage_updated_idx` ON `session_context_usage` (`time_updated`, `session_id`, `source`);
-- END v0.10.31 context_usage.sql
INSERT INTO session_input_receipt
  (input_id,state,delivery,completed_at,error,time_updated)
SELECT id,
  CASE state WHEN 'consumed' THEN 'recorded'
             WHEN 'cancelled' THEN 'cancelled'
             WHEN 'failed' THEN 'failed' ELSE 'admitted' END,
  delivery,
  CASE WHEN state IN ('cancelled','failed') THEN time_updated ELSE NULL END,
  error,time_updated
FROM session_input;
UPDATE zuno_schema SET format=14 WHERE singleton=1 AND format=13;

-- Representative published-format rows. They are facts, not inferred migration state.
INSERT INTO session_input
  (id,session_id,prompt,delivery,state,revision,admitted_seq,promoted_seq,
   time_created,time_updated,source_key,trigger_kind,cycle_id)
VALUES ('inp_format14_completed','ses_fixture_0001','Keep the finished turn — 保留回合',
  'steer','consumed',3,200,201,1735690010000,1735690013000,
  'published-completed-input','user','published-cycle-14');
INSERT INTO session_input_receipt
  (input_id,state,delivery,turn_id,applied_at,completed_at,stop_reason,error,time_updated)
VALUES ('inp_format14_completed','completed','steer','published-turn-14',
  1735690011000,1735690013000,'end_turn',NULL,1735690013000);
INSERT INTO session_context_usage
  (session_id,source,revision,context_epoch,state_json,time_updated)
VALUES ('ses_fixture_0001','main',7,2,
  '{ "inputTokens" : 123, "provenance" : "published — 保留" }',1735690013000);
INSERT INTO question_action_receipt
  (request_id,command_id,command_json,receipt,time_created)
VALUES ('req_format13','published-command-14','{ "kind" : "draft" }',
  '{ "preserve" : "回执 bytes" }',1735690013000);
UPDATE session_execution_state SET phase='paused',
  scheduling='{ "readiness" : {"kind":"paused","reason":"user"}, "progressFingerprint":"sha256:published-14","unchangedProgressCount":2 }'
WHERE session_id='ses_fixture_0001';
