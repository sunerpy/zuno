CREATE TABLE zuno_enterprise_preview.gateway_operation (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  operation_id text NOT NULL,
  gateway_id text NOT NULL,
  job_id text NOT NULL,
  session_id text NOT NULL,
  invocation_id text NOT NULL,
  admission jsonb NOT NULL CHECK(jsonb_typeof(admission)='object'),
  admission_digest text NOT NULL CHECK(length(admission_digest)=64),
  completion jsonb CHECK(jsonb_typeof(completion)='object'),
  completion_digest text CHECK(length(completion_digest)=64),
  time_admitted bigint NOT NULL,
  time_completed bigint,
  PRIMARY KEY(tenant_id,principal_id,operation_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id),
  CHECK((completion IS NULL AND completion_digest IS NULL AND time_completed IS NULL)
     OR (completion IS NOT NULL AND completion_digest IS NOT NULL AND time_completed IS NOT NULL))
);
CREATE INDEX gateway_operation_invocation_idx
  ON zuno_enterprise_preview.gateway_operation(tenant_id,principal_id,job_id,invocation_id);

CREATE TABLE zuno_enterprise_preview.gateway_operation_attempt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  operation_id text NOT NULL,
  attempt_id text NOT NULL,
  worker_id text NOT NULL,
  epoch bigint NOT NULL CHECK(epoch>0),
  checkpoint_version bigint NOT NULL CHECK(checkpoint_version>=0),
  lease jsonb NOT NULL CHECK(jsonb_typeof(lease)='object'),
  time_admitted bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,operation_id,attempt_id,checkpoint_version),
  FOREIGN KEY(tenant_id,principal_id,operation_id)
    REFERENCES zuno_enterprise_preview.gateway_operation(tenant_id,principal_id,operation_id)
);
