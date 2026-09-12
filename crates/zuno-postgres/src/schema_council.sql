ALTER TABLE zuno_enterprise_preview.runtime_job ADD COLUMN deadline_at bigint CHECK(deadline_at>0);
ALTER TABLE zuno_enterprise_preview.runtime_workflow_node
  ADD CONSTRAINT council_node_run_binding UNIQUE(tenant_id,principal_id,run_id,node_run_id);

CREATE TABLE zuno_enterprise_preview.runtime_council (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  run_id text NOT NULL,
  state text NOT NULL CHECK(state IN('seats','stopping','synthesis','completed','failed','cancelled','uncertain')),
  started_at bigint NOT NULL,
  seat_deadline_at bigint NOT NULL,
  deadline_at bigint NOT NULL,
  stop_deadline_at bigint,
  synthesis_deadline_at bigint,
  PRIMARY KEY(tenant_id,principal_id,run_id),
  FOREIGN KEY(tenant_id,principal_id,run_id)
    REFERENCES zuno_enterprise_preview.runtime_workflow(tenant_id,principal_id,run_id),
  CHECK(started_at>0 AND started_at<seat_deadline_at AND seat_deadline_at<deadline_at),
  CHECK(stop_deadline_at IS NULL OR (stop_deadline_at>=seat_deadline_at AND stop_deadline_at<=deadline_at)),
  CHECK(synthesis_deadline_at IS NULL OR (synthesis_deadline_at>started_at AND synthesis_deadline_at<=deadline_at)),
  CHECK(state<>'stopping' OR stop_deadline_at IS NOT NULL),
  CHECK(state NOT IN('synthesis','completed') OR synthesis_deadline_at IS NOT NULL)
);
CREATE TABLE zuno_enterprise_preview.runtime_council_seat (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  run_id text NOT NULL,
  node_run_id text NOT NULL,
  seat_id text NOT NULL,
  status text NOT NULL CHECK(status IN('pending','running','waiting','retrying','completed','invalid','failed','timed_out','cancelled','uncertain')),
  attempts integer NOT NULL DEFAULT 0 CHECK(attempts>=0 AND attempts<=4),
  retry_after_at bigint,
  answer jsonb CHECK(answer IS NULL OR jsonb_typeof(answer)='object'),
  answer_digest text,
  error text,
  PRIMARY KEY(tenant_id,principal_id,node_run_id),
  UNIQUE(tenant_id,principal_id,run_id,seat_id),
  FOREIGN KEY(tenant_id,principal_id,run_id)
    REFERENCES zuno_enterprise_preview.runtime_council(tenant_id,principal_id,run_id),
  FOREIGN KEY(tenant_id,principal_id,run_id,node_run_id)
    REFERENCES zuno_enterprise_preview.runtime_workflow_node(tenant_id,principal_id,run_id,node_run_id),
  CHECK((answer IS NULL)=(answer_digest IS NULL)),
  CHECK(answer_digest IS NULL OR length(answer_digest)=64),
  CHECK((status='completed')=(answer IS NOT NULL))
);
CREATE TABLE zuno_enterprise_preview.runtime_council_attempt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  node_run_id text NOT NULL,
  attempt integer NOT NULL CHECK(attempt>=1 AND attempt<=4),
  child_job_id text NOT NULL,
  kind text NOT NULL CHECK(kind IN('seat','repair')),
  observed_digest text,
  PRIMARY KEY(tenant_id,principal_id,node_run_id,attempt),
  UNIQUE(tenant_id,principal_id,child_job_id),
  FOREIGN KEY(tenant_id,principal_id,node_run_id)
    REFERENCES zuno_enterprise_preview.runtime_council_seat(tenant_id,principal_id,node_run_id),
  FOREIGN KEY(tenant_id,principal_id,child_job_id)
    REFERENCES zuno_enterprise_preview.runtime_child(tenant_id,principal_id,job_id),
  CHECK(observed_digest IS NULL OR length(observed_digest)=64)
);
