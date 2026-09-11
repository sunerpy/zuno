-- Zuno database format 12 delta, exactly as released in v0.10.29.
-- Load after format-7.sql, format-8.sql, format-9.sql, format-10.sql and format-11.sql.
-- Frozen DDL provenance:
--   git show d1212860dba6a6b420ce81d444ace58feaf0adb5:crates/zuno-db/src/schema/automatic_memory.sql
-- SHA-256 of the verbatim section below:
--   6732ae158d4be5bffc9dcf37bd78f5efd6cc2608cfe2d02b30defd66c6709026
-- Both published indexes are part of this fixture, not inferred from current DDL.
-- BEGIN v0.10.29 automatic_memory.sql
-- Ordinary memory changes are revision-bound, including queued manual-review
-- changes. NULL preserves the identity of pre-upgrade candidates without
-- pretending that their original proposal recorded a revision or evidence.
ALTER TABLE memory_candidate ADD COLUMN base_revision integer
  CHECK (base_revision IS NULL OR base_revision >= 1);
ALTER TABLE memory_candidate ADD COLUMN evidence text
  CHECK (evidence IS NULL OR (json_valid(evidence) AND json_type(evidence) = 'array'));

-- Only automatically derived entries have provenance here. A missing or changed
-- source can therefore hide its derived entry without deleting unrelated or
-- user-authored memory. References deliberately survive source deletion.
CREATE TABLE resident_memory_provenance (
  path text NOT NULL,
  content text NOT NULL CHECK (length(trim(content)) > 0),
  evidence text NOT NULL CHECK (json_valid(evidence) AND json_type(evidence) = 'array'),
  candidate_id text,
  time_updated integer NOT NULL,
  PRIMARY KEY (path, content),
  FOREIGN KEY (path) REFERENCES resident_memory_document(path) ON DELETE CASCADE,
  FOREIGN KEY (candidate_id) REFERENCES memory_candidate(id) ON DELETE SET NULL
);

-- The consumed input digest and output revisions are committed together with
-- memory changes. A successful no-op advances this watermark too, so a quiet
-- project does not run another paid consolidation on every poll.
CREATE TABLE memory_maintenance_state (
  project_id text NOT NULL,
  project_path text NOT NULL,
  input_digest text NOT NULL CHECK (length(input_digest) = 64),
  global_revision integer NOT NULL CHECK (global_revision >= 1),
  project_revision integer NOT NULL CHECK (project_revision >= 1),
  job_id text,
  time_updated integer NOT NULL,
  PRIMARY KEY (project_id, project_path),
  FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE,
  FOREIGN KEY (job_id) REFERENCES learning_job(id) ON DELETE SET NULL
);
CREATE INDEX memory_candidate_path_status_updated_idx
  ON memory_candidate(target_path, status, time_updated, id);
CREATE INDEX resident_memory_provenance_candidate_idx
  ON resident_memory_provenance(candidate_id);
-- END v0.10.29 automatic_memory.sql

UPDATE zuno_schema SET format = 12 WHERE singleton = 1 AND format = 11;

-- Representative published automatic-memory state. Format 13 must preserve
-- these values alongside the session/message and resident revisions above.
UPDATE memory_candidate
  SET base_revision = 1, evidence = '[]'
  WHERE id = 'mem_fixture_0001';
INSERT INTO resident_memory_provenance
  (path,content,evidence,candidate_id,time_updated)
VALUES ('C:/Users/0791/project/.zuno/RULES.md','Keep reviewed resident memory.',
  '[]','mem_fixture_0001',1735689795000);
INSERT INTO memory_maintenance_state
  (project_id,project_path,input_digest,global_revision,project_revision,job_id,time_updated)
VALUES ('prj_fixture_0001','C:/Users/0791/project/.zuno/RULES.md',
  '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef',
  1,2,'ljb_fixture_0001',1735689796000);
