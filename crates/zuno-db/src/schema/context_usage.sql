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
