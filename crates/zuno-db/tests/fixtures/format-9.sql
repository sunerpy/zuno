-- Zuno database format 9 delta, exactly as v0.10.21 upgrades the checked-in
-- format-8 fixture.
--
-- DDL provenance:
--   git show v0.10.21:crates/zuno-db/src/schema.rs
--     -> MEMORY_POLICY_SCHEMA_SQL
--   git show v0.10.21:crates/zuno-db/src/migration/mod.rs
--     -> migrate_memory_policy marker update
CREATE TABLE `session_memory_policy` (
  `session_id` text PRIMARY KEY,
  `use_memories` integer NOT NULL CHECK (`use_memories` IN (0,1)),
  `generation` text NOT NULL CHECK (`generation` IN ('enabled','disabled','excluded')),
  `reason` text NOT NULL CHECK (length(trim(`reason`)) > 0),
  `source` text NOT NULL CHECK (length(trim(`source`)) > 0),
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `time_created` integer NOT NULL CHECK (`time_created` >= 0),
  `time_updated` integer NOT NULL CHECK (`time_updated` >= `time_created`),
  CONSTRAINT `fk_session_memory_policy_session_id_session_id_fk`
    FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE INDEX `session_memory_policy_generation_updated_idx`
  ON `session_memory_policy` (`generation`,`time_updated`,`session_id`);
UPDATE `zuno_schema` SET `format` = 9 WHERE `singleton` = 1 AND `format` = 8;

INSERT INTO `session_memory_policy`
  (`session_id`, `use_memories`, `generation`, `reason`, `source`, `revision`,
   `time_created`, `time_updated`)
VALUES
  ('ses_fixture_0001', 1, 'enabled', 'format-9 policy survives',
   'fixture', 3, 1735689790000, 1735689791000);
