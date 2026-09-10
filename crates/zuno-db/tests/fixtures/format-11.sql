-- Zuno database format 11 delta, exactly as v0.10.28 upgrades format 10.
-- DDL provenance: git show 851722f2142591899238166915ac6fd86fdd56cc:crates/zuno-db/src/schema/memory_runtime.sql
-- Load after format-7.sql, format-8.sql, format-9.sql and format-10.sql.
CREATE TABLE `resident_memory_document` (
  `path` text PRIMARY KEY CHECK (length(trim(`path`)) > 0),
  `scope` text NOT NULL CHECK (`scope` IN ('global','project')),
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `entries` text NOT NULL CHECK (json_valid(`entries`) AND json_type(`entries`) = 'array'),
  `content_digest` text NOT NULL CHECK (length(`content_digest`) = 64),
  `projected_revision` integer NOT NULL DEFAULT 0
    CHECK (`projected_revision` >= 0 AND `projected_revision` <= `revision`),
  `projection_error` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `resident_memory_revision` (
  `path` text NOT NULL,
  `revision` integer NOT NULL CHECK (`revision` >= 1),
  `entries` text NOT NULL CHECK (json_valid(`entries`) AND json_type(`entries`) = 'array'),
  `content_digest` text NOT NULL CHECK (length(`content_digest`) = 64),
  `operation` text NOT NULL CHECK (`operation` IN ('import','apply','undo')),
  `candidate_id` text,
  `time_created` integer NOT NULL,
  PRIMARY KEY (`path`,`revision`),
  FOREIGN KEY (`path`) REFERENCES `resident_memory_document` (`path`) ON DELETE CASCADE,
  FOREIGN KEY (`candidate_id`) REFERENCES `memory_candidate` (`id`) ON DELETE SET NULL
);
CREATE TABLE `learning_retrieval_snapshot` (
  `session_id` text PRIMARY KEY,
  `query_digest` text NOT NULL,
  `selected_ids` text NOT NULL CHECK (json_valid(`selected_ids`) AND json_type(`selected_ids`) = 'array'),
  `candidate_count` integer NOT NULL CHECK (`candidate_count` >= 0),
  `estimated_tokens` integer NOT NULL CHECK (`estimated_tokens` >= 0),
  `reason` text,
  `time_updated` integer NOT NULL,
  FOREIGN KEY (`session_id`) REFERENCES `session` (`id`) ON DELETE CASCADE
);
ALTER TABLE `experience_record` ADD COLUMN `evidence_verified` integer NOT NULL DEFAULT 0
  CHECK (`evidence_verified` IN (0,1));
ALTER TABLE `experience_record` ADD COLUMN `last_used_at` integer;
ALTER TABLE `experience_record` ADD COLUMN `use_count` integer NOT NULL DEFAULT 0
  CHECK (`use_count` >= 0);
ALTER TABLE `experience_evidence` ADD COLUMN `source_digest` text;
ALTER TABLE `experience_evidence` ADD COLUMN `verified` integer NOT NULL DEFAULT 0
  CHECK (`verified` IN (0,1));
ALTER TABLE `experience_evidence` ADD COLUMN `promotion_eligible` integer NOT NULL DEFAULT 0
  CHECK (`promotion_eligible` IN (0,1));
UPDATE `experience_evidence`
  SET `verified` = 1, `promotion_eligible` = 1, `source_digest` = `digest`
  WHERE `kind` = 'user' AND `experience_id` IN (
    SELECT `id` FROM `experience_record` WHERE `extraction_job_id` IS NULL
  );
UPDATE `experience_record` SET `evidence_verified` = 1
  WHERE EXISTS (SELECT 1 FROM `experience_evidence` e WHERE e.experience_id = experience_record.id)
    AND NOT EXISTS (
      SELECT 1 FROM `experience_evidence` e
      WHERE e.experience_id = experience_record.id AND e.verified = 0
    );
ALTER TABLE `learning_job` ADD COLUMN `lease_token` text;
DROP INDEX `learning_job_extraction_source_idx`;
CREATE UNIQUE INDEX `learning_job_extraction_source_idx`
  ON `learning_job` (`session_id`,`source_message_id`,`extractor_version`)
  WHERE `kind` = 'extraction'
    AND COALESCE(json_extract(`payload`,'$.trigger'),'automatic_post_turn') <> 'manual';
CREATE INDEX `learning_job_extraction_lookup_idx`
  ON `learning_job` (`session_id`,`source_message_id`,`extractor_version`)
  WHERE `kind` = 'extraction';
ALTER TABLE `skill_candidate` ADD COLUMN `evaluation_lease_token` text;
ALTER TABLE `skill_candidate` ADD COLUMN `evaluation_lease_expires` integer
  CHECK ((`evaluation_lease_token` IS NULL) = (`evaluation_lease_expires` IS NULL));
UPDATE `learning_job` SET `lease_token` = lower(hex(randomblob(16)))
  WHERE `owner_id` IS NOT NULL AND `lease_expires` IS NOT NULL;

-- Released query-time indexes are derived data. Replace their possibly older
-- triggers in the same migration transaction, then rebuild from preserved rows.
DROP TRIGGER IF EXISTS experience_search_fts_insert;
DROP TRIGGER IF EXISTS experience_search_fts_delete;
DROP TRIGGER IF EXISTS experience_search_fts_update;
DROP TABLE IF EXISTS experience_search_fts;
CREATE VIRTUAL TABLE experience_search_fts USING fts5(
  title, summary, resolution,
  content='experience_record', content_rowid='rowid', tokenize='unicode61'
);
CREATE VIRTUAL TABLE experience_search_cjk_fts USING fts5(
  title, summary, resolution,
  content='experience_record', content_rowid='rowid', tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS experience_search_fts_insert AFTER INSERT ON experience_record BEGIN
  INSERT INTO experience_search_fts(rowid, title, summary, resolution)
  VALUES (new.rowid, new.title, new.summary, new.resolution);
END;
CREATE TRIGGER IF NOT EXISTS experience_search_fts_delete AFTER DELETE ON experience_record BEGIN
  INSERT INTO experience_search_fts(experience_search_fts, rowid, title, summary, resolution)
  VALUES ('delete', old.rowid, old.title, old.summary, old.resolution);
END;
CREATE TRIGGER IF NOT EXISTS experience_search_fts_update
AFTER UPDATE OF title, summary, resolution ON experience_record BEGIN
  INSERT INTO experience_search_fts(experience_search_fts, rowid, title, summary, resolution)
  VALUES ('delete', old.rowid, old.title, old.summary, old.resolution);
  INSERT INTO experience_search_fts(rowid, title, summary, resolution)
  VALUES (new.rowid, new.title, new.summary, new.resolution);
END;
CREATE TRIGGER experience_search_cjk_fts_insert AFTER INSERT ON experience_record BEGIN
  INSERT INTO experience_search_cjk_fts(rowid, title, summary, resolution)
  VALUES (new.rowid, new.title, new.summary, new.resolution);
END;
CREATE TRIGGER experience_search_cjk_fts_delete AFTER DELETE ON experience_record BEGIN
  INSERT INTO experience_search_cjk_fts(experience_search_cjk_fts, rowid, title, summary, resolution)
  VALUES ('delete', old.rowid, old.title, old.summary, old.resolution);
END;
CREATE TRIGGER experience_search_cjk_fts_update
AFTER UPDATE OF title, summary, resolution ON experience_record BEGIN
  INSERT INTO experience_search_cjk_fts(experience_search_cjk_fts, rowid, title, summary, resolution)
  VALUES ('delete', old.rowid, old.title, old.summary, old.resolution);
  INSERT INTO experience_search_cjk_fts(rowid, title, summary, resolution)
  VALUES (new.rowid, new.title, new.summary, new.resolution);
END;
INSERT INTO experience_search_fts(experience_search_fts) VALUES ('rebuild');
INSERT INTO experience_search_cjk_fts(experience_search_cjk_fts) VALUES ('rebuild');
CREATE INDEX `message_session_user_boundary_idx`
  ON `message` (`session_id`,`time_created` DESC,`id` DESC)
  WHERE json_extract(`data`, '$.role') = 'user';
CREATE INDEX `resident_memory_revision_candidate_idx`
  ON `resident_memory_revision` (`candidate_id`,`operation`,`revision`);
CREATE INDEX `experience_record_usage_idx`
  ON `experience_record` (`project_id`,`last_used_at`,`id`);

UPDATE zuno_schema SET format = 11 WHERE singleton = 1 AND format = 10;
-- Representative resident state, including its previously published revision.
INSERT INTO resident_memory_document
  (path,scope,revision,entries,content_digest,projected_revision,time_created,time_updated)
VALUES ('C:/Users/0791/project/.zuno/RULES.md','project',2,
  '["Keep reviewed resident memory."]','dd64b4090aa1cc93da4823ba644d57c0afb93c77cb87b2f0690fff27838305fa',
  2,1735689793000,1735689794000);
INSERT INTO resident_memory_revision (path,revision,entries,content_digest,operation,time_created)
VALUES ('C:/Users/0791/project/.zuno/RULES.md',1,'[]',
  '4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945','import',1735689793000),
  ('C:/Users/0791/project/.zuno/RULES.md',2,'["Keep reviewed resident memory."]',
  'dd64b4090aa1cc93da4823ba644d57c0afb93c77cb87b2f0690fff27838305fa','import',1735689794000);
