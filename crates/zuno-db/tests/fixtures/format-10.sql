-- Zuno database format 10 delta, exactly as v0.10.23 upgrades format 9.
-- DDL provenance: tag v0.10.23 (ad61db80), schema.rs EXECUTION_SCHEMA_SQL.
-- Load after format-7.sql, format-8.sql, and format-9.sql.
ALTER TABLE `session_input` ADD COLUMN `source_key` text;
ALTER TABLE `session_input` ADD COLUMN `trigger_kind` text NOT NULL DEFAULT 'legacy'
  CHECK (`trigger_kind` IN ('legacy','user','user_control','automatic','recovery'));
ALTER TABLE `session_input` ADD COLUMN `cycle_id` text;
CREATE TABLE `session_execution_state` (
  `session_id` text PRIMARY KEY,
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `mode` text NOT NULL CHECK (`mode` IN ('plan','work')),
  `work_identity` text CHECK (`work_identity` IS NULL OR json_valid(`work_identity`)),
  `authorized_plan_id` text,
  `authorized_plan_revision` integer CHECK (`authorized_plan_revision` IS NULL OR `authorized_plan_revision` >= 1),
  `handoff_plan_id` text,
  `handoff_plan_revision` integer CHECK (`handoff_plan_revision` IS NULL OR `handoff_plan_revision` >= 1),
  `draft_review_risk` text CHECK (`draft_review_risk` IS NULL OR json_valid(`draft_review_risk`)),
  `cycle_id` text,
  `phase` text NOT NULL CHECK (`phase` IN ('idle','planning','authorized','running','waiting','paused','completed','blocked')),
  `continuation` text CHECK (`continuation` IS NULL OR json_valid(`continuation`)),
  `time_created` integer NOT NULL CHECK (`time_created` >= 0),
  `time_updated` integer NOT NULL CHECK (`time_updated` >= `time_created`),
  CONSTRAINT `session_execution_authorized_plan_pair`
    CHECK ((`authorized_plan_id` IS NULL) = (`authorized_plan_revision` IS NULL)),
  CONSTRAINT `session_execution_handoff_plan_pair`
    CHECK ((`handoff_plan_id` IS NULL) = (`handoff_plan_revision` IS NULL)),
  CONSTRAINT `fk_session_execution_state_session_id_session_id_fk`
    FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `completion_delivery` (
  `source_key` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `source` text NOT NULL CHECK (`source` IN ('background_execution','agent_job','workflow','product_agent')),
  `terminal_revision` integer NOT NULL CHECK (`terminal_revision` >= 0),
  `cycle_id` text,
  `payload` text NOT NULL CHECK (json_valid(`payload`)),
  `owner` text CHECK (`owner` IS NULL OR `owner` IN ('callback','inline')),
  `input_id` text,
  `time_created` integer NOT NULL CHECK (`time_created` >= 0),
  `time_updated` integer NOT NULL CHECK (`time_updated` >= `time_created`),
  CONSTRAINT `completion_delivery_owner_input`
    CHECK ((`owner` = 'callback' AND `input_id` IS NOT NULL)
      OR (`owner` IS NULL AND `input_id` IS NULL)
      OR (`owner` = 'inline' AND `input_id` IS NULL)),
  CONSTRAINT `fk_completion_delivery_session_id_session_id_fk`
    FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_completion_delivery_input_id_session_input_id_fk`
    FOREIGN KEY (`input_id`) REFERENCES `session_input`(`id`) ON DELETE SET NULL
);
CREATE UNIQUE INDEX `session_input_session_source_key_idx`
  ON `session_input` (`session_id`,`source_key`) WHERE `source_key` IS NOT NULL;
CREATE INDEX `session_execution_state_mode_phase_updated_idx`
  ON `session_execution_state` (`mode`,`phase`,`time_updated`,`session_id`);
CREATE INDEX `completion_delivery_session_owner_updated_idx`
  ON `completion_delivery` (`session_id`,`owner`,`time_updated`,`source_key`);

UPDATE zuno_schema SET format = 10 WHERE singleton = 1 AND format = 9;
INSERT INTO session_execution_state
  (session_id, revision, mode, phase, time_created, time_updated)
VALUES ('ses_fixture_0001', 4, 'work', 'completed', 1735689791000, 1735689792000);
