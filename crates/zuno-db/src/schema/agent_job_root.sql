-- Format 14 extends native jobs with root turns; existing rows keep their identities.
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
      (`subject_kind` = 'root-turn' AND
       json_extract(`subject_payload`, '$.kind') = 'rootTurn' AND
       coalesce(length(trim(json_extract(`subject_payload`, '$.turnID'))), 0) > 0 AND
       `report_delivery` = 'quiet') OR
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
