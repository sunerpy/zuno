-- Zuno database format 6, exactly as the v0.2.2 release created it.
--
-- DDL provenance (nothing below is derived from the current schema):
--   git show v0.2.2:crates/zuno-db/src/schema.rs        -> CORE_SCHEMA_SQL,LEARNING_SCHEMA_SQL
--   git show v0.2.2:crates/zuno-db/src/migration/mod.rs -> FORMAT_SQL and the marker insert
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
CREATE TABLE `message_feedback` (
  `message_id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `rating` integer NOT NULL CHECK (`rating` IN (-1,1)),
  `note` text,
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_message_feedback_message_id_message_id_fk` FOREIGN KEY (`message_id`) REFERENCES `message`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_message_feedback_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `learning_job` (
  `id` text PRIMARY KEY,
  `project_id` text,
  `session_id` text,
  `source_message_id` text,
  `kind` text NOT NULL CHECK (`kind` IN ('extraction','project_aggregation','global_aggregation','evaluation','skill_apply','skill_undo')),
  `extractor_version` text,
  `idempotency_key` text NOT NULL UNIQUE,
  `status` text NOT NULL CHECK (`status` IN ('queued','running','completed','skipped','failed','uncertain')),
  `attempt` integer NOT NULL DEFAULT 0 CHECK (`attempt` >= 0),
  `owner_id` text,
  `lease_expires` integer,
  `scheduled_at` integer NOT NULL,
  `payload` text CHECK (`payload` IS NULL OR json_valid(`payload`)),
  `result` text CHECK (`result` IS NULL OR json_valid(`result`)),
  `error` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_completed` integer,
  CONSTRAINT `learning_job_extraction_identity` CHECK (
    `kind` <> 'extraction' OR (
      `session_id` IS NOT NULL AND
      `source_message_id` IS NOT NULL AND
      `extractor_version` IS NOT NULL AND
      length(trim(`extractor_version`)) > 0
    )
  ),
  CONSTRAINT `learning_job_lease_pair` CHECK (
    (`owner_id` IS NULL AND `lease_expires` IS NULL) OR
    (`owner_id` IS NOT NULL AND `lease_expires` IS NOT NULL)
  ),
  CONSTRAINT `fk_learning_job_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_learning_job_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_learning_job_source_message_id_message_id_fk` FOREIGN KEY (`source_message_id`) REFERENCES `message`(`id`) ON DELETE CASCADE
);
CREATE TABLE `experience_record` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `session_id` text,
  `source_message_id` text,
  `extraction_job_id` text,
  `extraction_ordinal` integer,
  `kind` text NOT NULL CHECK (`kind` IN ('outcome','problem','unresolved_issue','user_correction','explicit_feedback','procedure')),
  `title` text NOT NULL CHECK (length(trim(`title`)) > 0),
  `summary` text NOT NULL CHECK (length(trim(`summary`)) > 0),
  `resolution` text,
  `confidence` integer NOT NULL CHECK (`confidence` BETWEEN 0 AND 10000),
  `fingerprint` text NOT NULL,
  `status` text NOT NULL CHECK (`status` IN ('active','promoted','forgotten')),
  `promoted_memory_candidate_id` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `experience_extraction_pair` CHECK (
    (`extraction_job_id` IS NULL AND `extraction_ordinal` IS NULL) OR
    (`extraction_job_id` IS NOT NULL AND `extraction_ordinal` IS NOT NULL AND `extraction_ordinal` >= 0)
  ),
  CONSTRAINT `experience_unresolved_not_promoted` CHECK (
    `kind` <> 'unresolved_issue' OR
    (`status` <> 'promoted' AND `promoted_memory_candidate_id` IS NULL)
  ),
  CONSTRAINT `fk_experience_record_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_experience_record_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE SET NULL,
  CONSTRAINT `fk_experience_record_source_message_id_message_id_fk` FOREIGN KEY (`source_message_id`) REFERENCES `message`(`id`) ON DELETE SET NULL,
  CONSTRAINT `fk_experience_record_extraction_job_id_learning_job_id_fk` FOREIGN KEY (`extraction_job_id`) REFERENCES `learning_job`(`id`) ON DELETE SET NULL,
  CONSTRAINT `fk_experience_record_memory_candidate_id_memory_candidate_id_fk` FOREIGN KEY (`promoted_memory_candidate_id`) REFERENCES `memory_candidate`(`id`) ON DELETE SET NULL
);
CREATE TABLE `experience_evidence` (
  `id` text PRIMARY KEY,
  `experience_id` text NOT NULL,
  `kind` text NOT NULL CHECK (`kind` IN ('message','tool','feedback','artifact','user')),
  `source_id` text,
  `excerpt` text NOT NULL,
  `digest` text NOT NULL,
  `time_created` integer NOT NULL,
  CONSTRAINT `fk_experience_evidence_experience_id_experience_record_id_fk` FOREIGN KEY (`experience_id`) REFERENCES `experience_record`(`id`) ON DELETE CASCADE
);
CREATE TABLE `learning_pattern` (
  `id` text PRIMARY KEY,
  `scope` text NOT NULL CHECK (`scope` IN ('project','global')),
  `project_id` text,
  `fingerprint` text NOT NULL,
  `title` text NOT NULL CHECK (length(trim(`title`)) > 0),
  `summary` text NOT NULL CHECK (length(trim(`summary`)) > 0),
  `learned_rules` text NOT NULL CHECK (json_valid(`learned_rules`) AND json_type(`learned_rules`) = 'array'),
  `evidence_ids` text NOT NULL CHECK (json_valid(`evidence_ids`) AND json_type(`evidence_ids`) = 'array'),
  `evidence_digest` text NOT NULL,
  `evidence_version` integer NOT NULL CHECK (`evidence_version` >= 1),
  `independent_sessions` integer NOT NULL CHECK (`independent_sessions` >= 0),
  `project_count` integer NOT NULL CHECK (`project_count` >= 1),
  `status` text NOT NULL CHECK (`status` IN ('pending','promoted','rejected','superseded')),
  `rejected_evidence_version` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `learning_pattern_scope_project` CHECK (
    (`scope` = 'project' AND `project_id` IS NOT NULL) OR
    (`scope` = 'global' AND `project_id` IS NULL)
  ),
  CONSTRAINT `learning_pattern_rejection_version` CHECK (
    (`status` = 'rejected' AND `rejected_evidence_version` IS NOT NULL) OR
    (`status` <> 'rejected')
  ),
  CONSTRAINT `fk_learning_pattern_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `evaluation_suite` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `name` text NOT NULL,
  `description` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_evaluation_suite_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `evaluation_case` (
  `id` text PRIMARY KEY,
  `suite_id` text NOT NULL,
  `name` text NOT NULL,
  `prompt` text NOT NULL,
  `expected` text NOT NULL,
  `tool_cassette` text NOT NULL CHECK (json_valid(`tool_cassette`)),
  `case_kind` text NOT NULL CHECK (`case_kind` IN ('failure','protection','general')),
  `weight` integer NOT NULL CHECK (`weight` > 0),
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_evaluation_case_suite_id_evaluation_suite_id_fk` FOREIGN KEY (`suite_id`) REFERENCES `evaluation_suite`(`id`) ON DELETE CASCADE
);
CREATE TABLE `evaluation_run` (
  `id` text PRIMARY KEY,
  `suite_id` text NOT NULL,
  `candidate_id` text NOT NULL,
  `model` text NOT NULL,
  `toolset_digest` text NOT NULL,
  `budget` text NOT NULL CHECK (json_valid(`budget`)),
  `attempt_snapshot` text NOT NULL CHECK (json_valid(`attempt_snapshot`)),
  `status` text NOT NULL CHECK (`status` IN ('running','passed','failed','uncertain')),
  `baseline_metric` integer,
  `candidate_metric` integer,
  `error` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_completed` integer,
  CONSTRAINT `fk_evaluation_run_suite_id_evaluation_suite_id_fk` FOREIGN KEY (`suite_id`) REFERENCES `evaluation_suite`(`id`) ON DELETE CASCADE
);
CREATE TABLE `evaluation_result` (
  `id` text PRIMARY KEY,
  `run_id` text NOT NULL,
  `case_id` text NOT NULL,
  `baseline_score` integer NOT NULL,
  `candidate_score` integer NOT NULL,
  `cited_failure_fixed` integer NOT NULL CHECK (`cited_failure_fixed` IN (0,1)),
  `critical_regression` integer NOT NULL CHECK (`critical_regression` IN (0,1)),
  `details` text NOT NULL CHECK (json_valid(`details`)),
  `time_created` integer NOT NULL,
  CONSTRAINT `fk_evaluation_result_run_id_evaluation_run_id_fk` FOREIGN KEY (`run_id`) REFERENCES `evaluation_run`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_evaluation_result_case_id_evaluation_case_id_fk` FOREIGN KEY (`case_id`) REFERENCES `evaluation_case`(`id`) ON DELETE CASCADE
);
CREATE TABLE `skill_candidate` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `pattern_id` text,
  `name` text NOT NULL CHECK (length(trim(`name`)) > 0),
  `target_source` text NOT NULL CHECK (length(trim(`target_source`)) > 0),
  `target_path` text,
  `target_writable` integer NOT NULL CHECK (`target_writable` IN (0,1)),
  `target_digest` text NOT NULL,
  `proposed_content` text NOT NULL,
  `proposed_digest` text NOT NULL,
  `diff` text NOT NULL,
  `evidence_ids` text NOT NULL CHECK (json_valid(`evidence_ids`) AND json_type(`evidence_ids`) = 'array'),
  `learned_rules` text NOT NULL CHECK (
    json_valid(`learned_rules`) AND
    json_type(`learned_rules`) = 'array' AND
    json_array_length(`learned_rules`) BETWEEN 1 AND 15
  ),
  `operation_kind` text NOT NULL CHECK (`operation_kind` IN ('apply','revoke')),
  `reverts_candidate_id` text UNIQUE,
  `status` text NOT NULL CHECK (`status` IN ('pending_review','evaluating','approved','applying','applied','rejected','stale','undoing','undone','failed','uncertain')),
  `evaluation_run_id` text,
  `before_content` text,
  `after_content` text,
  `companion_name` text,
  `apply_operation_id` text UNIQUE,
  `error` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_applied` integer,
  CONSTRAINT `skill_candidate_apply_snapshots` CHECK (
    `status` NOT IN ('applying','applied','undoing','undone','uncertain') OR
    (`before_content` IS NOT NULL AND `after_content` IS NOT NULL AND `apply_operation_id` IS NOT NULL)
  ),
  CONSTRAINT `skill_candidate_operation_target` CHECK (
    (`operation_kind` = 'apply' AND `reverts_candidate_id` IS NULL) OR
    (`operation_kind` = 'revoke' AND `reverts_candidate_id` IS NOT NULL)
  ),
  CONSTRAINT `fk_skill_candidate_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE,
  CONSTRAINT `fk_skill_candidate_pattern_id_learning_pattern_id_fk` FOREIGN KEY (`pattern_id`) REFERENCES `learning_pattern`(`id`) ON DELETE SET NULL,
  CONSTRAINT `fk_skill_candidate_evaluation_run_id_evaluation_run_id_fk` FOREIGN KEY (`evaluation_run_id`) REFERENCES `evaluation_run`(`id`) ON DELETE SET NULL,
  CONSTRAINT `fk_skill_candidate_reverts_candidate_id_skill_candidate_id_fk` FOREIGN KEY (`reverts_candidate_id`) REFERENCES `skill_candidate`(`id`) ON DELETE CASCADE
);
CREATE INDEX `message_feedback_session_updated_idx` ON `message_feedback` (`session_id`,`time_updated`,`message_id`);
CREATE INDEX `learning_job_status_scheduled_idx` ON `learning_job` (`status`,`scheduled_at`,`id`);
CREATE INDEX `learning_job_project_kind_status_idx` ON `learning_job` (`project_id`,`kind`,`status`,`scheduled_at`);
CREATE UNIQUE INDEX `learning_job_extraction_source_idx`
  ON `learning_job` (`session_id`,`source_message_id`,`extractor_version`)
  WHERE `kind` = 'extraction';
CREATE UNIQUE INDEX `experience_record_extraction_ordinal_idx`
  ON `experience_record` (`extraction_job_id`,`extraction_ordinal`)
  WHERE `extraction_job_id` IS NOT NULL;
CREATE INDEX `experience_record_project_status_time_idx`
  ON `experience_record` (`project_id`,`status`,`time_created`,`id`);
CREATE INDEX `experience_record_session_time_idx`
  ON `experience_record` (`session_id`,`time_created`,`id`);
CREATE INDEX `experience_record_fingerprint_idx`
  ON `experience_record` (`project_id`,`fingerprint`,`status`);
CREATE UNIQUE INDEX `experience_evidence_identity_idx`
  ON `experience_evidence` (`experience_id`,`kind`,`digest`,`source_id`);
CREATE INDEX `experience_evidence_experience_idx`
  ON `experience_evidence` (`experience_id`,`time_created`,`id`);
CREATE UNIQUE INDEX `learning_pattern_scope_fingerprint_idx`
  ON `learning_pattern` (`scope`,coalesce(`project_id`,''),`fingerprint`);
CREATE INDEX `learning_pattern_scope_status_updated_idx`
  ON `learning_pattern` (`scope`,`project_id`,`status`,`time_updated`,`id`);
CREATE UNIQUE INDEX `evaluation_case_suite_name_idx`
  ON `evaluation_case` (`suite_id`,`name`);
CREATE INDEX `evaluation_run_candidate_time_idx`
  ON `evaluation_run` (`candidate_id`,`time_created`,`id`);
CREATE UNIQUE INDEX `evaluation_result_run_case_idx`
  ON `evaluation_result` (`run_id`,`case_id`);
CREATE INDEX `skill_candidate_project_status_time_idx`
  ON `skill_candidate` (`project_id`,`status`,`time_created`,`id`);
CREATE INDEX `skill_candidate_pattern_idx`
  ON `skill_candidate` (`pattern_id`,`time_created`,`id`);
CREATE UNIQUE INDEX `skill_candidate_pattern_digest_unique_idx`
  ON `skill_candidate` (`pattern_id`,`proposed_digest`);
CREATE TABLE zuno_schema (
  singleton integer PRIMARY KEY CHECK (singleton = 1),
  format integer NOT NULL
);
INSERT INTO zuno_schema (singleton, format) VALUES (1, 6);

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
-- Learning-flywheel rows (durable memory in this format also lives in
-- `experience_record`, promoted into the `memory_candidate` above).
INSERT INTO `message_feedback` (`message_id`, `session_id`, `rating`, `note`, `revision`, `time_created`, `time_updated`)
VALUES ('msg_fixture_0001', 'ses_fixture_0001', 1, 'Exactly the right level of caution.', 1, 1735689770000, 1735689770000);
INSERT INTO `learning_job` (`id`, `project_id`, `session_id`, `source_message_id`, `kind`, `extractor_version`, `idempotency_key`, `status`, `attempt`, `owner_id`, `lease_expires`, `scheduled_at`, `payload`, `result`, `error`, `time_created`, `time_updated`, `time_completed`)
VALUES ('ljb_fixture_0001', 'prj_fixture_0001', 'ses_fixture_0001', 'msg_fixture_0001', 'extraction', 'extractor-v1', 'extraction:ses_fixture_0001:msg_fixture_0001:extractor-v1', 'completed', 1, NULL, NULL, 1735689771000, NULL, '{"records":1}', NULL, 1735689771000, 1735689772000, 1735689772000);
INSERT INTO `experience_record` (`id`, `project_id`, `session_id`, `source_message_id`, `extraction_job_id`, `extraction_ordinal`, `kind`, `title`, `summary`, `resolution`, `confidence`, `fingerprint`, `status`, `promoted_memory_candidate_id`, `time_created`, `time_updated`)
VALUES ('exp_fixture_0001', 'prj_fixture_0001', 'ses_fixture_0001', 'msg_fixture_0001', 'ljb_fixture_0001', 0, 'procedure', 'Test the database crate before releasing', 'The user wants `cargo test -p zuno-db` to run before every release.', NULL, 8800, 'sha256:fixture-experience-0001', 'promoted', 'mem_fixture_0001', 1735689772000, 1735689773000);
INSERT INTO `experience_evidence` (`id`, `experience_id`, `kind`, `source_id`, `excerpt`, `digest`, `time_created`)
VALUES ('evd_fixture_0001', 'exp_fixture_0001', 'message', 'msg_fixture_0001', 'Keep every row — 保留每一行 — and add nothing silently.', 'sha256:fixture-evidence-0001', 1735689772000);
-- The active Plan. This format has no Plan stack: `work_plan` carries neither
-- `parent_plan_id` nor `stack_depth`, and `work_plan_archive` does not exist.
INSERT INTO `work_plan` (`session_id`, `id`, `goal_id`, `revision`, `title`, `steps`, `time_created`, `time_updated`)
VALUES ('ses_fixture_0001', 'pln_fixture_0001', 'gol_fixture_0001', 3, 'Migrate the ledger', '[{"id":"inspect","title":"Inspect the old ledger","status":"completed"},{"id":"upgrade","title":"Upgrade in one transaction","status":"in_progress"}]', 1735689710000, 1735689780000);
