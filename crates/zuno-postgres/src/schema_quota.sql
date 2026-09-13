CREATE TABLE zuno_enterprise_preview.organization_quota (
  tenant_id text PRIMARY KEY,
  revision bigint NOT NULL CHECK(revision>0),
  limits jsonb NOT NULL CHECK(jsonb_typeof(limits)='object'),
  FOREIGN KEY(tenant_id) REFERENCES zuno_enterprise_preview.organization_policy(tenant_id)
);
CREATE TABLE zuno_enterprise_preview.organization_quota_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  request_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  result jsonb NOT NULL,
  actor jsonb NOT NULL,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,request_id)
);
CREATE FUNCTION zuno_enterprise_preview.create_organization_quota() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog AS $$
BEGIN
  INSERT INTO zuno_enterprise_preview.organization_quota(tenant_id,revision,limits) VALUES(
    NEW.tenant_id,1,'{"rootSessions":2048,"rootJobs":64,"childJobs":1024,"executions":8,"learningJobs":64,"learningExecutions":2}');
  RETURN NEW;
END $$;
CREATE TRIGGER create_organization_quota AFTER INSERT ON zuno_enterprise_preview.organization_policy
FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.create_organization_quota();
REVOKE ALL ON FUNCTION zuno_enterprise_preview.create_organization_quota() FROM PUBLIC;
ALTER TABLE zuno_enterprise_preview.runtime_owner_schedule
  ADD COLUMN learning_dispatch_sequence bigint NOT NULL DEFAULT 0 CHECK(learning_dispatch_sequence>=0);
CREATE FUNCTION zuno_enterprise_preview.learning_dispatch_owners(requested_tenant text)
RETURNS TABLE(principal_id text,dispatch_clock bigint)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog SET row_security=on AS $$
  SELECT q.principal_id,max(q.learning_dispatch_sequence) OVER()
  FROM zuno_enterprise_preview.runtime_owner_schedule q
  WHERE q.tenant_id=requested_tenant
  ORDER BY q.learning_dispatch_sequence,q.principal_id LIMIT 64
$$;
REVOKE ALL ON FUNCTION zuno_enterprise_preview.learning_dispatch_owners(text) FROM PUBLIC;
