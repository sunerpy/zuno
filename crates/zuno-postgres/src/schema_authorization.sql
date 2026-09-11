CREATE TABLE zuno_enterprise_preview.organization_policy (
  tenant_id text PRIMARY KEY,
  revision bigint NOT NULL CHECK(revision>0),
  allowed_apps jsonb NOT NULL CHECK(jsonb_typeof(allowed_apps)='array'),
  approval_apps jsonb NOT NULL CHECK(jsonb_typeof(approval_apps)='array'),
  auto_read_apps jsonb NOT NULL CHECK(jsonb_typeof(auto_read_apps)='array'),
  approval_lifetime_seconds integer NOT NULL CHECK(approval_lifetime_seconds BETWEEN 30 AND 3600)
);
CREATE TABLE zuno_enterprise_preview.organization_member (
  tenant_id text NOT NULL REFERENCES zuno_enterprise_preview.organization_policy(tenant_id),
  principal_id text NOT NULL,
  role text NOT NULL CHECK(role IN('member','approver','administrator','automation')),
  active boolean NOT NULL,
  PRIMARY KEY(tenant_id,principal_id)
);
CREATE TABLE zuno_enterprise_preview.organization_audit (
  tenant_id text NOT NULL REFERENCES zuno_enterprise_preview.organization_policy(tenant_id),
  id text NOT NULL,
  actor jsonb NOT NULL CHECK(jsonb_typeof(actor)='object'),
  type text NOT NULL,
  data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,id)
);
CREATE TABLE zuno_enterprise_preview.operation_approval (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  job_id text NOT NULL,
  session_id text NOT NULL,
  operation_id text NOT NULL,
  binding jsonb NOT NULL CHECK(jsonb_typeof(binding)='object'),
  requester jsonb NOT NULL CHECK(jsonb_typeof(requester)='object'),
  policy_revision bigint NOT NULL CHECK(policy_revision>0),
  audience text NOT NULL CHECK(audience IN('requester','designatedApprover')),
  state text NOT NULL CHECK(state IN('pending','automatic','approved','rejected','expired','invalidated')),
  presentation jsonb NOT NULL CHECK(jsonb_typeof(presentation)='object'),
  decided_by jsonb CHECK(decided_by IS NULL OR jsonb_typeof(decided_by)='object'),
  created_at bigint NOT NULL,
  expires_at bigint NOT NULL CHECK(expires_at>created_at),
  decided_at bigint,
  PRIMARY KEY(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,operation_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id),
  CHECK(binding ?& ARRAY['jobId','sessionId','turnId','invocationId','operationId','argumentsSha256','resourcesSha256','effect']),
  CHECK(binding->>'jobId'=job_id AND binding->>'sessionId'=session_id AND binding->>'operationId'=operation_id),
  CHECK((state='pending' AND decided_by IS NULL AND decided_at IS NULL)
     OR (state='automatic' AND decided_by IS NULL AND decided_at IS NOT NULL)
     OR (state IN('approved','rejected') AND decided_by IS NOT NULL AND decided_at IS NOT NULL)
     OR state IN('expired','invalidated'))
);
CREATE INDEX operation_approval_pending_idx ON zuno_enterprise_preview.operation_approval(tenant_id,expires_at,id) WHERE state='pending';
CREATE TABLE zuno_enterprise_preview.approval_answer_receipt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  client_id text NOT NULL,
  request_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  approval_owner text NOT NULL,
  approval_id text NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,client_id,request_id),
  FOREIGN KEY(tenant_id,approval_owner,approval_id)
    REFERENCES zuno_enterprise_preview.operation_approval(tenant_id,principal_id,id)
);

-- The data owner resolves an opaque approval ID before entering its private
-- scope. The helper exposes only routing coordinates, never the presentation
-- or action parameters. Public HTTP access still verifies the viewer's role.
CREATE POLICY approval_catalog_owner ON zuno_enterprise_preview.operation_approval
FOR SELECT USING(current_user=pg_get_userbyid(
  (SELECT nspowner FROM pg_namespace WHERE nspname='zuno_enterprise_preview')));
CREATE FUNCTION zuno_enterprise_preview.approval_coordinates(requested_tenant text,requested_id text)
RETURNS TABLE(principal_id text,session_id text)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog SET row_security=on AS $$
  SELECT a.principal_id,a.session_id FROM zuno_enterprise_preview.operation_approval a
  WHERE a.tenant_id=requested_tenant AND a.id=requested_id LIMIT 1
$$;
REVOKE ALL ON FUNCTION zuno_enterprise_preview.approval_coordinates(text,text) FROM PUBLIC;
