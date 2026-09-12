CREATE TABLE zuno_enterprise_preview.gateway_merge_operation (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  operation_id text NOT NULL,
  gateway_id text NOT NULL,
  job_id text NOT NULL,
  session_id text NOT NULL,
  invocation_id text NOT NULL,
  child_job_id text NOT NULL,
  offer jsonb NOT NULL CHECK(jsonb_typeof(offer)='object'),
  offer_digest text NOT NULL CHECK(length(offer_digest)=64),
  admitted boolean NOT NULL DEFAULT false,
  completion jsonb CHECK(completion IS NULL OR jsonb_typeof(completion)='object'),
  completion_digest text CHECK(completion_digest IS NULL OR length(completion_digest)=64),
  time_created bigint NOT NULL,
  time_admitted bigint,
  time_completed bigint,
  PRIMARY KEY(tenant_id,principal_id,operation_id),
  UNIQUE(tenant_id,principal_id,job_id,invocation_id),
  FOREIGN KEY(tenant_id,principal_id,job_id,session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id),
  FOREIGN KEY(tenant_id,principal_id,child_job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  CHECK(admitted=(time_admitted IS NOT NULL)),
  CHECK((completion IS NULL)=(completion_digest IS NULL)),
  CHECK((completion IS NULL)=(time_completed IS NULL)),
  CHECK(completion IS NULL OR admitted)
);
CREATE TABLE zuno_enterprise_preview.gateway_merge_attempt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  operation_id text NOT NULL,
  attempt_id text NOT NULL,
  checkpoint_version bigint NOT NULL CHECK(checkpoint_version>=0),
  lease jsonb NOT NULL CHECK(jsonb_typeof(lease)='object'),
  PRIMARY KEY(tenant_id,principal_id,operation_id,attempt_id,checkpoint_version),
  FOREIGN KEY(tenant_id,principal_id,operation_id)
    REFERENCES zuno_enterprise_preview.gateway_merge_operation(tenant_id,principal_id,operation_id)
);
CREATE TABLE zuno_enterprise_preview.gateway_merge_cancellation (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  operation_id text NOT NULL,
  time_polled bigint NOT NULL DEFAULT 0,
  PRIMARY KEY(tenant_id,principal_id,operation_id),
  FOREIGN KEY(tenant_id,principal_id,operation_id)
    REFERENCES zuno_enterprise_preview.gateway_merge_operation(tenant_id,principal_id,operation_id)
);
CREATE POLICY cancellation_catalog_owner ON zuno_enterprise_preview.gateway_merge_operation
FOR SELECT USING(current_user=pg_get_userbyid(
  (SELECT nspowner FROM pg_namespace WHERE nspname='zuno_enterprise_preview')));
CREATE POLICY cancellation_catalog_owner ON zuno_enterprise_preview.gateway_merge_cancellation
FOR SELECT USING(current_user=pg_get_userbyid(
  (SELECT nspowner FROM pg_namespace WHERE nspname='zuno_enterprise_preview')));
CREATE FUNCTION zuno_enterprise_preview.gateway_merge_cancellations(requested_tenant text,requested_gateway text,maximum integer)
RETURNS TABLE(principal_id text,operation_id text)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog SET row_security=on AS $$
  SELECT o.principal_id,o.operation_id FROM zuno_enterprise_preview.gateway_merge_operation o
  JOIN zuno_enterprise_preview.runtime_stop s
    ON s.tenant_id=o.tenant_id AND s.principal_id=o.principal_id AND s.job_id=o.job_id
  JOIN zuno_enterprise_preview.gateway_merge_cancellation c
    ON c.tenant_id=o.tenant_id AND c.principal_id=o.principal_id AND c.operation_id=o.operation_id
  WHERE o.tenant_id=requested_tenant AND o.gateway_id=requested_gateway AND o.admitted AND o.completion IS NULL
  ORDER BY c.time_polled,s.time_requested,o.principal_id,o.operation_id
  LIMIT greatest(1,least(maximum,32))
$$;
REVOKE ALL ON FUNCTION zuno_enterprise_preview.gateway_merge_cancellations(text,text,integer) FROM PUBLIC;
