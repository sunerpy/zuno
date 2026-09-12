-- A workflow is a native parent/child Job with separately durable coordination.
-- Its logical Job is never claimed by an Agent Worker or sent to a model.
CREATE TABLE zuno_enterprise_preview.runtime_workflow (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  run_id text NOT NULL,
  job_id text NOT NULL,
  parent_job_id text NOT NULL,
  parent_session_id text NOT NULL,
  plan jsonb NOT NULL CHECK(jsonb_typeof(plan)='object'),
  plan_digest text NOT NULL CHECK(length(plan_digest)=64),
  state text NOT NULL CHECK(state IN('preparing','prepared','active','completed','failed','cancelled','uncertain')),
  revision bigint NOT NULL DEFAULT 1 CHECK(revision>0),
  last_coordinated_at bigint NOT NULL DEFAULT 0,
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,run_id),
  UNIQUE(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_child(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,parent_job_id,parent_session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id)
);
CREATE INDEX runtime_workflow_active
  ON zuno_enterprise_preview.runtime_workflow(tenant_id,principal_id,state,time_updated,run_id)
  WHERE state IN('preparing','prepared','active');

CREATE TABLE zuno_enterprise_preview.runtime_workflow_node (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  run_id text NOT NULL,
  node_run_id text NOT NULL,
  node_id text NOT NULL,
  position integer NOT NULL CHECK(position>=0 AND position<64),
  child_job_id text NOT NULL,
  state text NOT NULL CHECK(state IN('pending','running','waiting','completed','failed','cancelled','uncertain')),
  result jsonb,
  result_digest text,
  input_prompt text,
  input_digest text,
  input_sources jsonb,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,node_run_id),
  UNIQUE(tenant_id,principal_id,run_id,node_id),
  UNIQUE(tenant_id,principal_id,run_id,position),
  UNIQUE(tenant_id,principal_id,child_job_id),
  FOREIGN KEY(tenant_id,principal_id,run_id)
    REFERENCES zuno_enterprise_preview.runtime_workflow(tenant_id,principal_id,run_id),
  FOREIGN KEY(tenant_id,principal_id,child_job_id)
    REFERENCES zuno_enterprise_preview.runtime_child(tenant_id,principal_id,job_id),
  CHECK((result IS NULL)=(result_digest IS NULL)),
  CHECK(result_digest IS NULL OR length(result_digest)=64),
  CHECK((input_prompt IS NULL)=(input_digest IS NULL)),
  CHECK((input_prompt IS NULL)=(input_sources IS NULL)),
  CHECK(input_digest IS NULL OR length(input_digest)=64)
);
