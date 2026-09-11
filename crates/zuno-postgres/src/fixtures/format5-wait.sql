-- A wait releases an execution slot while keeping logical session ownership.
ALTER TABLE zuno_enterprise_preview.runtime_job
  DROP CONSTRAINT runtime_job_phase_check,
  ADD CONSTRAINT runtime_job_phase_check
    CHECK(phase IN('ready','running','waiting','paused','completed','failed','cancelled','uncertain')),
  ADD CONSTRAINT runtime_job_wait_binding
    UNIQUE(tenant_id,principal_id,job_id,session_id,turn_id);

CREATE TABLE zuno_enterprise_preview.runtime_wait (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  job_id text NOT NULL,
  session_id text NOT NULL,
  turn_id text NOT NULL,
  invocation_id text NOT NULL,
  reference jsonb NOT NULL CHECK(jsonb_typeof(reference)='object'),
  state text NOT NULL CHECK(state IN('pending','ready','consumed','cancelled')),
  deadline_ms bigint CHECK(deadline_ms>0),
  completion_event_id text,
  consumed_event_id text,
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,job_id,session_id,turn_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id,turn_id),
  FOREIGN KEY(tenant_id,principal_id,completion_event_id)
    REFERENCES zuno_enterprise_preview.event(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,consumed_event_id)
    REFERENCES zuno_enterprise_preview.event(tenant_id,principal_id,id),
  CHECK((state IN('ready','consumed'))=(completion_event_id IS NOT NULL)),
  CHECK((state='consumed')=(consumed_event_id IS NOT NULL))
);
CREATE INDEX runtime_wait_job_idx
  ON zuno_enterprise_preview.runtime_wait(tenant_id,principal_id,job_id,state);
CREATE INDEX runtime_wait_timer_idx
  ON zuno_enterprise_preview.runtime_wait(tenant_id,principal_id,deadline_ms,id)
  WHERE state='pending' AND deadline_ms IS NOT NULL;
