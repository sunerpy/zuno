CREATE TABLE zuno_enterprise_preview.workspace_snapshot_transfer (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  snapshot_id text NOT NULL,
  source_gateway_id text NOT NULL,
  target_gateway_id text NOT NULL,
  job_id text NOT NULL,
  admission jsonb NOT NULL CHECK(jsonb_typeof(admission)='object'),
  admission_digest text NOT NULL CHECK(length(admission_digest)=64),
  snapshot jsonb CHECK(snapshot IS NULL OR jsonb_typeof(snapshot)='object'),
  snapshot_digest text CHECK(snapshot_digest IS NULL OR length(snapshot_digest)=64),
  time_admitted bigint NOT NULL,
  time_completed bigint,
  PRIMARY KEY(tenant_id,principal_id,snapshot_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  CHECK((snapshot IS NULL)=(snapshot_digest IS NULL)),
  CHECK((snapshot IS NULL)=(time_completed IS NULL))
);
CREATE INDEX workspace_snapshot_transfer_job
  ON zuno_enterprise_preview.workspace_snapshot_transfer(tenant_id,principal_id,job_id);
