CREATE TABLE zuno_enterprise_preview.learning_control_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  request_id text NOT NULL,
  job_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  response jsonb NOT NULL CHECK(jsonb_typeof(response)='object'),
  created_at bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,request_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.learning_execution(tenant_id,principal_id,job_id)
);
CREATE INDEX learning_execution_history
  ON zuno_enterprise_preview.learning_execution(tenant_id,principal_id,created_at DESC,job_id DESC);
CREATE INDEX learning_job_workspace
  ON zuno_enterprise_preview.learning_job(tenant_id,principal_id,workspace_id,id);
