-- Zuno database format 8 delta, exactly as v0.10.5 upgrades the checked-in
-- v0.6.7 format-7 fixture.
--
-- DDL provenance:
--   git show v0.10.5:crates/zuno-db/src/schema.rs
--     -> VERIFICATION_SCHEMA_SQL
--   git show v0.10.5:crates/zuno-db/src/migration/mod.rs
--     -> migrate_verification marker update
--
-- tests/migration_fixtures.rs concatenates this after format-7.sql. That models
-- a real database first created by v0.6.7 and then opened by v0.10.5: every
-- historical row remains, the verification ledger is appended, and the marker
-- advances from 7 to 8 only after the DDL succeeds.
CREATE TABLE `verification_receipt` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `turn_id` text,
  `tool_call_id` text NOT NULL,
  `tool_id` text NOT NULL,
  `summary` text NOT NULL,
  `workdir` text,
  `exit_code` integer,
  `exit_authority` text NOT NULL CHECK (`exit_authority` IN ('authoritative','derived','absent')),
  `outcome` text NOT NULL CHECK (`outcome` IN ('passed','failed','unknown')),
  `git_head` text,
  `output_digest` text,
  `detail` text,
  `time_created` integer NOT NULL
);
CREATE UNIQUE INDEX `verification_receipt_call_idx`
  ON `verification_receipt` (`session_id`,`tool_call_id`);
CREATE INDEX `verification_receipt_session_time_idx`
  ON `verification_receipt` (`session_id`,`time_created`,`id`);
UPDATE `zuno_schema` SET `format` = 8 WHERE `singleton` = 1 AND `format` = 7;

-- A v0.10.5 ledger row proves format-8-only user evidence survives format 9.
INSERT INTO `verification_receipt`
  (`id`, `session_id`, `turn_id`, `tool_call_id`, `tool_id`, `summary`, `workdir`,
   `exit_code`, `exit_authority`, `outcome`, `git_head`, `output_digest`, `detail`,
   `time_created`)
VALUES
  ('vrc_fixture_0001', 'ses_fixture_0001', 'turn_fixture_0001',
   'call_fixture_0001', 'shell', 'The format-8 verification receipt survives.',
   '/home/dev/zuno', 0, 'authoritative', 'passed', 'f8c5157',
   'sha256:fixture-verification-0001', 'v0.10.5 durable verification evidence',
   1735689785000);
