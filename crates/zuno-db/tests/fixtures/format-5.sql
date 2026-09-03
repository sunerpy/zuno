-- Zuno database format 5, exactly as the v0.0.3 release created it.
--
-- DDL provenance (nothing below is derived from the current schema):
--   git show v0.0.3:crates/zuno-db/src/schema.rs        -> SCHEMA_SQL
--   git show v0.0.3:crates/zuno-db/src/migration/mod.rs -> FORMAT_SQL and the marker insert
-- Statements appear in the order `migration::create_current` ran them at that tag:
-- application schema first, then the `zuno_schema` marker table and its single row.
--
-- Deliberately absent: the opt-in objects that release could create later through
-- `IF NOT EXISTS` helpers (`fts::ensure`, `continuity::ensure_schema`,
-- `experience` FTS). They are not part of the format and the upgrade never touches them.
--
-- Representative rows follow the DDL so tests/migration_fixtures.rs can prove the
-- upgrade to the current format preserves them byte-for-byte.
CREATE TABLE `workspace` (
  `id` text PRIMARY KEY,
  `type` text NOT NULL,
  `name` text DEFAULT '' NOT NULL,
  `branch` text,
  `directory` text,
  `extra` text,
  `project_id` text NOT NULL,
  `time_used` integer NOT NULL,
  CONSTRAINT `fk_workspace_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `data_migration` (
  `name` text PRIMARY KEY,
  `time_completed` integer NOT NULL
);
CREATE TABLE `account_state` (
  `id` integer PRIMARY KEY,
  `active_account_id` text,
  `active_org_id` text,
  CONSTRAINT `fk_account_state_active_account_id_account_id_fk` FOREIGN KEY (`active_account_id`) REFERENCES `account`(`id`) ON DELETE SET NULL
);
CREATE TABLE `account` (
  `id` text PRIMARY KEY,
  `email` text NOT NULL,
  `url` text NOT NULL,
  `access_token` text NOT NULL,
  `refresh_token` text NOT NULL,
  `token_expiry` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `control_account` (
  `email` text NOT NULL,
  `url` text NOT NULL,
  `access_token` text NOT NULL,
  `refresh_token` text NOT NULL,
  `token_expiry` integer,
  `active` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `control_account_pk` PRIMARY KEY(`email`, `url`)
);
CREATE TABLE `credential` (
  `id` text PRIMARY KEY,
  `integration_id` text,
  `label` text NOT NULL,
  `value` text NOT NULL,
  `connector_id` text,
  `method_id` text,
  `active` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `event_sequence` (
  `aggregate_id` text PRIMARY KEY,
  `seq` integer NOT NULL,
  `owner_id` text
);
CREATE TABLE `event` (
  `id` text PRIMARY KEY,
  `aggregate_id` text NOT NULL,
  `seq` integer NOT NULL,
  `type` text NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_event_aggregate_id_event_sequence_aggregate_id_fk` FOREIGN KEY (`aggregate_id`) REFERENCES `event_sequence`(`aggregate_id`) ON DELETE CASCADE
);
CREATE TABLE `permission` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `action` text NOT NULL,
  `resource` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_permission_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `project_directory` (
  `project_id` text NOT NULL,
  `directory` text NOT NULL,
  `type` text,
  `strategy` text,
  `time_created` integer NOT NULL,
  CONSTRAINT `project_directory_pk` PRIMARY KEY(`project_id`, `directory`),
  CONSTRAINT `fk_project_directory_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `project` (
  `id` text PRIMARY KEY,
  `worktree` text NOT NULL,
  `vcs` text,
  `name` text,
  `icon_url` text,
  `icon_url_override` text,
  `icon_color` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_initialized` integer,
  `sandboxes` text NOT NULL,
  `commands` text
);
CREATE TABLE `message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `part` (
  `id` text PRIMARY KEY,
  `message_id` text NOT NULL,
  `session_id` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_part_message_id_message_id_fk` FOREIGN KEY (`message_id`) REFERENCES `message`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_context_epoch` (
  `session_id` text PRIMARY KEY,
  `baseline` text NOT NULL,
  `snapshot` text NOT NULL,
  `baseline_seq` integer NOT NULL,
  CONSTRAINT `fk_session_context_epoch_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_input` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `prompt` text NOT NULL,
  `delivery` text NOT NULL CHECK (`delivery` IN ('queue', 'steer')),
  `state` text NOT NULL CHECK (`state` IN ('queued', 'steering', 'promoted', 'consumed', 'cancelled', 'failed')),
  `revision` integer NOT NULL CHECK (`revision` > 0),
  `admitted_seq` integer NOT NULL,
  `promoted_seq` integer,
  `error` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_session_input_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `human_request` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `goal_id` text,
  `kind` text NOT NULL CHECK (`kind` IN ('input','permission')),
  `state` text NOT NULL CHECK (`state` IN ('pending','answered','cancelled','expired','failed')),
  `payload` text NOT NULL CHECK (json_valid(`payload`)),
  `response` text CHECK (`response` IS NULL OR json_valid(`response`)),
  `message_id` text,
  `call_id` text,
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_resolved` integer
);
CREATE TABLE `provider_retry_backoff` (
  `session_id` text PRIMARY KEY,
  `request_id` text NOT NULL,
  `turn_id` text NOT NULL,
  `failed_attempt` integer NOT NULL CHECK (`failed_attempt` >= 1),
  `next_attempt` integer NOT NULL CHECK (`next_attempt` > `failed_attempt`),
  `max_attempts` integer NOT NULL CHECK (`max_attempts` >= `next_attempt`),
  `reason` text NOT NULL,
  `delay_ms` integer NOT NULL CHECK (`delay_ms` > 0),
  `retry_at_ms` integer NOT NULL,
  `scheduled_at_ms` integer NOT NULL
);
CREATE TABLE `session_message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `type` text NOT NULL,
  `seq` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_session_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `workspace_id` text,
  `parent_id` text,
  `slug` text NOT NULL,
  `directory` text NOT NULL,
  `path` text,
  `title` text NOT NULL,
  `version` text NOT NULL,
  `share_url` text,
  `summary_additions` integer,
  `summary_deletions` integer,
  `summary_files` integer,
  `summary_diffs` text,
  `metadata` text,
  `cost` real DEFAULT 0 NOT NULL,
  `tokens_input` integer DEFAULT 0 NOT NULL,
  `tokens_output` integer DEFAULT 0 NOT NULL,
  `tokens_reasoning` integer DEFAULT 0 NOT NULL,
  `tokens_cache_read` integer DEFAULT 0 NOT NULL,
  `tokens_cache_write` integer DEFAULT 0 NOT NULL,
  `tokens_last_prompt` integer,
  `tokens_context_limit` integer,
  `tokens_accounting` text,
  `tokens_known` integer DEFAULT 0 NOT NULL,
  `tokens_estimated_pending_prompt` integer,
  `tokens_last_confirmed_at` integer,
  `failed_turns` integer DEFAULT 0 NOT NULL,
  `last_failed_at` integer,
  `revert` text,
  `permission` text,
  `agent` text,
  `model` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_compacting` integer,
  `time_archived` integer,
  CONSTRAINT `fk_session_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `agent_job` (
  `id` text PRIMARY KEY,
  `parent_session_id` text NOT NULL,
  `logical_key` text NOT NULL CHECK (length(trim(`logical_key`)) > 0),
  `subject_kind` text NOT NULL,
  `subject_payload` text NOT NULL,
  `orchestration_snapshot` text,
  `evidence_start_rowid` integer NOT NULL CHECK (`evidence_start_rowid` >= 0),
  `status` text NOT NULL,
  `report_delivery` text NOT NULL,
  `result` text,
  `error` text,
  `report_input_id` text,
  `created_seq` integer NOT NULL,
  `settled_seq` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_completed` integer,
  CONSTRAINT `agent_job_subject` CHECK (
    json_valid(`subject_payload`) AND (
      (`subject_kind` = 'child-session' AND
       json_extract(`subject_payload`, '$.kind') = 'childSession' AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.sessionID'))), 0) > 0 AND
       `parent_session_id` <> json_extract(`subject_payload`, '$.sessionID')) OR
      (`subject_kind` = 'product-agent' AND
       json_extract(`subject_payload`, '$.kind') = 'productAgent' AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.runID'))), 0) > 0 AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.product'))), 0) > 0 AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.instance'))), 0) > 0 AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.tool'))), 0) > 0) OR
      (`subject_kind` = 'workflow' AND
       json_extract(`subject_payload`, '$.kind') = 'workflow' AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.runID'))), 0) > 0 AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.workflow'))), 0) > 0)
    )
  ),
  CONSTRAINT `agent_job_orchestration_snapshot` CHECK (
    `orchestration_snapshot` IS NULL OR json_valid(`orchestration_snapshot`)
  ),
  CONSTRAINT `agent_job_status` CHECK (`status` IN ('queued','running','completed','failed','cancelled','uncertain')),
  CONSTRAINT `agent_job_report_delivery` CHECK (`report_delivery` IN ('next-step','quiet')),
  CONSTRAINT `fk_agent_job_parent_session_id_session_id_fk` FOREIGN KEY (`parent_session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_agent_job_report_input_id_session_input_id_fk` FOREIGN KEY (`report_input_id`) REFERENCES `session_input`(`id`) ON DELETE SET NULL
);
CREATE TABLE `work_plan` (
  `session_id` text PRIMARY KEY,
  `id` text NOT NULL UNIQUE,
  `goal_id` text,
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `title` text NOT NULL,
  `steps` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_work_plan_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `work_item` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `goal_id` text,
  `plan_step_id` text,
  `parent_id` text,
  `subject` text NOT NULL,
  `description` text NOT NULL,
  `active_form` text,
  `status` text NOT NULL CHECK (`status` IN ('pending','in_progress','completed','cancelled','blocked')),
  `priority` text NOT NULL CHECK (`priority` IN ('high','medium','low')),
  `dependencies` text NOT NULL,
  `owner` text,
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `tokens_used` integer NOT NULL DEFAULT 0,
  `usage_known` integer NOT NULL DEFAULT 0,
  `time_used_ms` integer NOT NULL DEFAULT 0,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_work_item_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `memory_reflection_delivery` (
  `session_id` text NOT NULL,
  `source_message_id` text NOT NULL,
  `ordinal` integer NOT NULL,
  `recovered` integer NOT NULL,
  `negative_learning` integer NOT NULL,
  `time_created` integer NOT NULL,
  CONSTRAINT `memory_reflection_delivery_pk` PRIMARY KEY (`session_id`,`source_message_id`),
  CONSTRAINT `memory_reflection_delivery_ordinal` UNIQUE (`session_id`,`ordinal`),
  CONSTRAINT `memory_reflection_delivery_positive_ordinal` CHECK (`ordinal` > 0),
  CONSTRAINT `memory_reflection_delivery_recovered` CHECK (`recovered` IN (0,1)),
  CONSTRAINT `memory_reflection_delivery_negative_learning` CHECK (`negative_learning` IN (0,1)),
  CONSTRAINT `fk_memory_reflection_delivery_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `memory_reflection_job` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `source_message_id` text NOT NULL,
  `trigger` text NOT NULL,
  `status` text NOT NULL,
  `owner_id` text NOT NULL,
  `lease_expires` integer NOT NULL,
  `error` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_completed` integer,
  CONSTRAINT `memory_reflection_job_source` UNIQUE (`session_id`,`source_message_id`),
  CONSTRAINT `memory_reflection_job_trigger` CHECK (`trigger` IN ('periodic','recovery','periodic-recovery')),
  CONSTRAINT `memory_reflection_job_status` CHECK (`status` IN ('running','completed','failed','uncertain')),
  CONSTRAINT `fk_memory_reflection_job_delivery_fk` FOREIGN KEY (`session_id`,`source_message_id`) REFERENCES `memory_reflection_delivery`(`session_id`,`source_message_id`) ON DELETE CASCADE
);
CREATE TABLE `memory_candidate` (
  `id` text PRIMARY KEY,
  `target` text NOT NULL,
  `target_path` text NOT NULL,
  `action` text NOT NULL,
  `content` text,
  `old_text` text,
  `reason` text NOT NULL,
  `confidence` integer NOT NULL,
  `source_kind` text NOT NULL,
  `source_session_id` text,
  `source_message_id` text,
  `fingerprint` text,
  `status` text NOT NULL,
  `before_entries` text,
  `after_entries` text,
  `error` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_applied` integer,
  CONSTRAINT `memory_candidate_target` CHECK (`target` IN ('global','project')),
  CONSTRAINT `memory_candidate_action` CHECK (`action` IN ('add','replace','remove')),
  CONSTRAINT `memory_candidate_confidence` CHECK (`confidence` BETWEEN 0 AND 10000),
  CONSTRAINT `memory_candidate_source` CHECK (`source_kind` IN ('reflection','tool','user')),
  CONSTRAINT `memory_candidate_status` CHECK (`status` IN ('pending','applying','undoing','applied','rejected','undone','failed','uncertain')),
  CONSTRAINT `fk_memory_candidate_session_id_session_id_fk` FOREIGN KEY (`source_session_id`) REFERENCES `session`(`id`) ON DELETE SET NULL
);
CREATE TABLE `session_share` (
  `session_id` text PRIMARY KEY,
  `id` text NOT NULL,
  `secret` text NOT NULL,
  `url` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_session_share_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE UNIQUE INDEX `event_aggregate_seq_idx` ON `event` (`aggregate_id`,`seq`);
CREATE INDEX `event_aggregate_type_seq_idx` ON `event` (`aggregate_id`,`type`,`seq`);
CREATE UNIQUE INDEX `permission_project_action_resource_idx` ON `permission` (`project_id`,`action`,`resource`);
CREATE INDEX `message_session_time_created_id_idx` ON `message` (`session_id`,`time_created`,`id`);
CREATE INDEX `part_message_id_id_idx` ON `part` (`message_id`,`id`);
CREATE INDEX `part_session_idx` ON `part` (`session_id`);
CREATE INDEX `session_input_session_pending_delivery_seq_idx` ON `session_input` (`session_id`,`state`,`delivery`,`admitted_seq`);
CREATE UNIQUE INDEX `session_input_session_admitted_seq_idx` ON `session_input` (`session_id`,`admitted_seq`);
CREATE UNIQUE INDEX `session_input_session_promoted_seq_idx` ON `session_input` (`session_id`,`promoted_seq`);
CREATE INDEX `human_request_session_state_created_idx` ON `human_request` (`session_id`,`state`,`time_created`,`id`);
CREATE INDEX `human_request_goal_state_created_idx` ON `human_request` (`goal_id`,`state`,`time_created`,`id`);
CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
CREATE INDEX `session_message_session_type_seq_idx` ON `session_message` (`session_id`,`type`,`seq`);
CREATE INDEX `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);
CREATE INDEX `session_message_time_created_idx` ON `session_message` (`time_created`);
CREATE UNIQUE INDEX `agent_job_child_running_idx`
  ON `agent_job` (json_extract(`subject_payload`, '$.sessionID'))
  WHERE `subject_kind` = 'child-session' AND `status` = 'running';
CREATE UNIQUE INDEX `agent_job_product_run_idx`
  ON `agent_job` (json_extract(`subject_payload`, '$.runID'))
  WHERE `subject_kind` = 'product-agent';
CREATE UNIQUE INDEX `agent_job_workflow_run_idx`
  ON `agent_job` (json_extract(`subject_payload`, '$.runID'))
  WHERE `subject_kind` = 'workflow';
CREATE INDEX `agent_job_parent_status_created_idx` ON `agent_job` (`parent_session_id`,`status`,`time_created`);
CREATE INDEX `agent_job_parent_logical_created_idx` ON `agent_job` (`parent_session_id`,`logical_key`,`time_created`);
CREATE INDEX `session_project_idx` ON `session` (`project_id`);
CREATE INDEX `session_workspace_idx` ON `session` (`workspace_id`);
CREATE INDEX `session_parent_idx` ON `session` (`parent_id`);
CREATE INDEX `work_plan_goal_idx` ON `work_plan` (`goal_id`);
CREATE INDEX `work_item_session_status_created_idx` ON `work_item` (`session_id`,`status`,`time_created`);
CREATE INDEX `work_item_goal_idx` ON `work_item` (`goal_id`);
CREATE INDEX `work_item_plan_step_idx` ON `work_item` (`plan_step_id`);
CREATE INDEX `memory_reflection_job_session_status_time_idx` ON `memory_reflection_job` (`session_id`,`status`,`time_created`);
CREATE INDEX `memory_candidate_path_status_time_idx` ON `memory_candidate` (`target_path`,`status`,`time_created`);
CREATE INDEX `memory_candidate_session_time_idx` ON `memory_candidate` (`source_session_id`,`time_created`);
CREATE UNIQUE INDEX `memory_candidate_reflection_source_fingerprint_idx`
  ON `memory_candidate` (`source_session_id`,`source_message_id`,`fingerprint`)
  WHERE `source_kind` = 'reflection' AND `fingerprint` IS NOT NULL;
CREATE TABLE zuno_schema (
  singleton integer PRIMARY KEY CHECK (singleton = 1),
  format integer NOT NULL
);
INSERT INTO zuno_schema (singleton, format) VALUES (1, 5);

-- Representative rows. Insert order respects the foreign keys every release enforced
-- with `PRAGMA foreign_keys = ON`: project -> session -> message -> part, then the
-- durable event, the durable-memory candidate with its reflection delivery, and the
-- session's Plan.
INSERT INTO `project` (`id`, `worktree`, `vcs`, `name`, `icon_url`, `icon_url_override`, `icon_color`, `time_created`, `time_updated`, `time_initialized`, `sandboxes`, `commands`)
VALUES ('prj_fixture_0001', '/home/dev/zuno', 'git', 'zuno', NULL, NULL, '#0a7', 1735689600000, 1735689600500, 1735689601000, '["workspace-write"]', NULL);
INSERT INTO `session` (`id`, `project_id`, `workspace_id`, `parent_id`, `slug`, `directory`, `path`, `title`, `version`, `share_url`, `summary_additions`, `summary_deletions`, `summary_files`, `summary_diffs`, `metadata`, `cost`, `tokens_input`, `tokens_output`, `tokens_reasoning`, `tokens_cache_read`, `tokens_cache_write`, `tokens_last_prompt`, `tokens_context_limit`, `tokens_accounting`, `tokens_known`, `tokens_estimated_pending_prompt`, `tokens_last_confirmed_at`, `failed_turns`, `last_failed_at`, `revert`, `permission`, `agent`, `model`, `time_created`, `time_updated`, `time_compacting`, `time_archived`)
VALUES ('ses_fixture_0001', 'prj_fixture_0001', NULL, NULL, 'quiet-harbor', '/home/dev/zuno', NULL, 'Migrate the ledger — 迁移账本', '1', NULL, 12, 3, 2, NULL, '{"origin":"fixture","tags":["release","db"]}', 0.125, 4321, 987, 65, 2048, 512, 5100, 200000, '{"provider":"reported"}', 1, NULL, 1735689789000, 0, NULL, NULL, '{"edit":"ask","bash":"allow"}', 'build', 'anthropic/claude-sonnet-4', 1735689700000, 1735689790000, NULL, NULL);
INSERT INTO `message` (`id`, `session_id`, `time_created`, `time_updated`, `data`)
VALUES ('msg_fixture_0001', 'ses_fixture_0001', 1735689700200, 1735689700200, '{"id":"msg_fixture_0001","role":"user","sessionID":"ses_fixture_0001","time":{"created":1735689700200}}');
INSERT INTO `part` (`id`, `message_id`, `session_id`, `time_created`, `time_updated`, `data`)
VALUES ('prt_fixture_0001', 'msg_fixture_0001', 'ses_fixture_0001', 1735689700200, 1735689700200, '{"id":"prt_fixture_0001","messageID":"msg_fixture_0001","sessionID":"ses_fixture_0001","type":"text","text":"Keep every row — 保留每一行 — and add nothing silently.\n\tTabs and \"quotes\" survive too."}');
INSERT INTO `session_message` (`id`, `session_id`, `type`, `seq`, `time_created`, `time_updated`, `data`)
VALUES ('sem_fixture_0001', 'ses_fixture_0001', 'prompt.admitted', 1, 1735689700100, 1735689700100, '{"kind":"prompt.admitted","inputID":"inp_fixture_0001","digest":"sha256:9f2c"}');
INSERT INTO `memory_candidate` (`id`, `target`, `target_path`, `action`, `content`, `old_text`, `reason`, `confidence`, `source_kind`, `source_session_id`, `source_message_id`, `fingerprint`, `status`, `before_entries`, `after_entries`, `error`, `time_created`, `time_updated`, `time_applied`)
VALUES ('mem_fixture_0001', 'project', '/home/dev/zuno/.zuno/MEMORY.md', 'add', 'Run `cargo test -p zuno-db` before every release.', NULL, 'Stated by the user in the fixture session.', 9200, 'reflection', 'ses_fixture_0001', 'msg_fixture_0001', 'sha256:fixture-memory-0001', 'applied', '[]', '["Run `cargo test -p zuno-db` before every release."]', NULL, 1735689750000, 1735689760000, 1735689760000);
INSERT INTO `memory_reflection_delivery` (`session_id`, `source_message_id`, `ordinal`, `recovered`, `negative_learning`, `time_created`)
VALUES ('ses_fixture_0001', 'msg_fixture_0001', 1, 0, 0, 1735689740000);
-- The active Plan. This format has no Plan stack: `work_plan` carries neither
-- `parent_plan_id` nor `stack_depth`, and `work_plan_archive` does not exist.
INSERT INTO `work_plan` (`session_id`, `id`, `goal_id`, `revision`, `title`, `steps`, `time_created`, `time_updated`)
VALUES ('ses_fixture_0001', 'pln_fixture_0001', 'gol_fixture_0001', 3, 'Migrate the ledger', '[{"id":"inspect","title":"Inspect the old ledger","status":"completed"},{"id":"upgrade","title":"Upgrade in one transaction","status":"in_progress"}]', 1735689710000, 1735689780000);
