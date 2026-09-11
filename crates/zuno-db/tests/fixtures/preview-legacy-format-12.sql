-- Zuno database format 12 delta, exactly as v0.10.29 upgrades format 11.
-- DDL provenance: git show d1212860dba6a6b420ce81d444ace58feaf0adb5:crates/zuno-db/src/schema/automatic_memory.sql
-- Load after format-7.sql through format-11.sql.
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

-- Representative format-12 maintenance and provenance. The imported revisions
-- in format-11 have no automatic candidates, so the released provenance backfill
-- is a no-op for them; this row represents a subsequently verified extraction.
INSERT INTO resident_memory_provenance(path,content,evidence,candidate_id,time_updated)
VALUES('C:/Users/0791/project/.zuno/RULES.md','Keep reviewed resident memory.',
       '[{"sessionId":"ses_fixture_0001","messageId":"msg_fixture_0001","sourceDigest":"fixture"}]',NULL,1710000000001);
INSERT INTO memory_maintenance_state(project_id,project_path,input_digest,global_revision,project_revision,job_id,time_updated)
SELECT id,'C:/Users/0791/project',printf('%064d',0),1,2,NULL,1710000000002 FROM project LIMIT 1;
UPDATE zuno_schema SET format=12 WHERE singleton=1 AND format=11;
