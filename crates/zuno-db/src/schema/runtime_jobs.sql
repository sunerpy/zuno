-- One authority row per session. Input revisions and execution epochs are
-- independent; no worker-supplied clock decides a lease deadline.
CREATE TABLE runtime_session (
  session_id text PRIMARY KEY NOT NULL,
  input_version integer NOT NULL DEFAULT 0 CHECK (typeof(input_version)='integer' AND input_version>=0),
  lease_epoch integer NOT NULL DEFAULT 0 CHECK (typeof(lease_epoch)='integer' AND lease_epoch>=0),
  current_job_id text,
  lease_job_id text,
  lease_attempt_id text,
  lease_worker_id text,
  lease_expires integer,
  CHECK (
    (lease_job_id IS NULL AND lease_attempt_id IS NULL AND lease_worker_id IS NULL AND lease_expires IS NULL)
    OR
    (lease_job_id IS NOT NULL AND lease_attempt_id IS NOT NULL AND lease_worker_id IS NOT NULL AND lease_expires IS NOT NULL)
  ),
  FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);
INSERT INTO runtime_session(session_id,input_version)
  SELECT s.id,(SELECT count(*) FROM session_input i WHERE i.session_id=s.id) FROM session s;
CREATE TRIGGER runtime_session_insert AFTER INSERT ON session
BEGIN
  INSERT INTO runtime_session(session_id) VALUES(NEW.id);
END;
CREATE TRIGGER runtime_input_insert AFTER INSERT ON session_input
BEGIN
  UPDATE runtime_session SET input_version=input_version+1 WHERE session_id=NEW.session_id;
END;
CREATE TRIGGER runtime_input_update AFTER UPDATE OF prompt,delivery,state ON session_input
WHEN NEW.prompt<>OLD.prompt OR NEW.delivery<>OLD.delivery
  OR (NEW.state='cancelled' AND OLD.state<>'cancelled')
BEGIN
  UPDATE runtime_session SET input_version=input_version+1 WHERE session_id=NEW.session_id;
END;

-- Extends the existing native Job identity; it is not another task ledger.
CREATE TABLE runtime_job (
  job_id text PRIMARY KEY NOT NULL,
  session_id text NOT NULL,
  turn_id text NOT NULL,
  input_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  principal text NOT NULL CHECK(json_valid(principal)),
  configuration text NOT NULL CHECK(json_valid(configuration)),
  phase text NOT NULL CHECK(phase IN('ready','running','paused','completed','failed','cancelled','uncertain')),
  checkpoint text CHECK(checkpoint IS NULL OR json_valid(checkpoint)),
  checkpoint_version integer NOT NULL DEFAULT 0
    CHECK(typeof(checkpoint_version)='integer' AND checkpoint_version>=0),
  input_version integer NOT NULL CHECK(typeof(input_version)='integer' AND input_version>=0),
  active_attempt_id text,
  ready_at integer NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  UNIQUE(session_id,turn_id),
  FOREIGN KEY(job_id) REFERENCES agent_job(id) ON DELETE CASCADE,
  FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE,
  FOREIGN KEY(input_id) REFERENCES session_input(id)
);
CREATE TABLE runtime_attempt (
  id text PRIMARY KEY NOT NULL,
  job_id text NOT NULL,
  worker_id text NOT NULL,
  lease_epoch integer NOT NULL CHECK(typeof(lease_epoch)='integer' AND lease_epoch>0),
  state text NOT NULL CHECK(state IN('running','released','completed','lost')),
  started_at integer NOT NULL,
  finished_at integer,
  UNIQUE(job_id,lease_epoch),
  FOREIGN KEY(job_id) REFERENCES runtime_job(job_id) ON DELETE CASCADE
);
CREATE TABLE runtime_owner_schedule (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  last_dispatch_sequence integer NOT NULL DEFAULT 0
    CHECK(typeof(last_dispatch_sequence)='integer' AND last_dispatch_sequence>=0),
  PRIMARY KEY(tenant_id,principal_id)
);
CREATE INDEX runtime_session_lease_deadline_idx ON runtime_session(lease_expires,session_id);
CREATE INDEX runtime_job_ready_idx ON runtime_job(phase,ready_at,session_id,job_id);
CREATE INDEX runtime_attempt_worker_state_idx ON runtime_attempt(worker_id,state,started_at);
