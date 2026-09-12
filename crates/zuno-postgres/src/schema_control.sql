CREATE TABLE zuno_enterprise_preview.runtime_control_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  request_id text NOT NULL,
  job_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  receipt jsonb NOT NULL CHECK(jsonb_typeof(receipt)='object'),
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,request_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id)
);
CREATE TABLE zuno_enterprise_preview.runtime_stop (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  root_job_id text NOT NULL,
  time_requested bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,root_job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id)
);
CREATE TABLE zuno_enterprise_preview.runtime_continuation (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  parent_job_id text NOT NULL,
  source_child_id text NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,parent_job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,source_child_id)
    REFERENCES zuno_enterprise_preview.runtime_child(tenant_id,principal_id,job_id)
);
CREATE INDEX runtime_continuation_parent
  ON zuno_enterprise_preview.runtime_continuation(tenant_id,principal_id,parent_job_id);
ALTER TABLE zuno_enterprise_preview.runtime_job NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.runtime_child NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.input NO FORCE ROW LEVEL SECURITY;
INSERT INTO zuno_enterprise_preview.runtime_continuation(tenant_id,principal_id,job_id,parent_job_id,source_child_id)
SELECT j.tenant_id,j.principal_id,j.job_id,c.parent_job_id,c.job_id
FROM zuno_enterprise_preview.runtime_job j
JOIN zuno_enterprise_preview.input i
  ON i.tenant_id=j.tenant_id AND i.principal_id=j.principal_id AND i.id=j.input_id
JOIN zuno_enterprise_preview.runtime_child c
  ON c.tenant_id=i.tenant_id AND c.principal_id=i.principal_id
    AND c.job_id=i.prompt->'completion'->'payload'->>'jobId' AND c.parent_session_id=j.session_id
WHERE i.prompt->>'kind'='completion';
ALTER TABLE zuno_enterprise_preview.runtime_job FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.runtime_child FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.input FORCE ROW LEVEL SECURITY;
CREATE TABLE zuno_enterprise_preview.gateway_cancellation_delivery (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  operation_id text NOT NULL,
  time_polled bigint NOT NULL DEFAULT 0,
  PRIMARY KEY(tenant_id,principal_id,operation_id),
  FOREIGN KEY(tenant_id,principal_id,operation_id)
    REFERENCES zuno_enterprise_preview.gateway_operation(tenant_id,principal_id,operation_id)
);

-- This helper exposes only pending cancellation coordinates to an authenticated
-- gateway host. Operation contents are read later under the exact owner policy.
CREATE POLICY cancellation_catalog_owner ON zuno_enterprise_preview.runtime_stop
FOR SELECT USING(current_user=pg_get_userbyid(
  (SELECT nspowner FROM pg_namespace WHERE nspname='zuno_enterprise_preview')));
CREATE POLICY cancellation_catalog_owner ON zuno_enterprise_preview.gateway_operation
FOR SELECT USING(current_user=pg_get_userbyid(
  (SELECT nspowner FROM pg_namespace WHERE nspname='zuno_enterprise_preview')));
CREATE POLICY cancellation_catalog_owner ON zuno_enterprise_preview.gateway_cancellation_delivery
FOR SELECT USING(current_user=pg_get_userbyid(
  (SELECT nspowner FROM pg_namespace WHERE nspname='zuno_enterprise_preview')));
CREATE FUNCTION zuno_enterprise_preview.gateway_cancellations(requested_tenant text, requested_gateway text, maximum integer)
RETURNS TABLE(principal_id text,operation_id text)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog SET row_security=on AS $$
  SELECT o.principal_id,o.operation_id
  FROM zuno_enterprise_preview.gateway_operation o
  JOIN zuno_enterprise_preview.runtime_stop s
    ON s.tenant_id=o.tenant_id AND s.principal_id=o.principal_id AND s.job_id=o.job_id
  JOIN zuno_enterprise_preview.gateway_cancellation_delivery d
    ON d.tenant_id=o.tenant_id AND d.principal_id=o.principal_id AND d.operation_id=o.operation_id
  WHERE o.tenant_id=requested_tenant AND o.gateway_id=requested_gateway AND o.completion IS NULL
  ORDER BY d.time_polled,s.time_requested,o.principal_id,o.operation_id
  LIMIT greatest(1,least(maximum,64))
$$;
REVOKE ALL ON FUNCTION zuno_enterprise_preview.gateway_cancellations(text,text,integer) FROM PUBLIC;
