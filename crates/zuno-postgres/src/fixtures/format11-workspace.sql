ALTER TABLE zuno_enterprise_preview.runtime_child
  ADD COLUMN delegation_depth_limit integer NOT NULL DEFAULT 0 CHECK(delegation_depth_limit BETWEEN 0 AND 16),
  ADD COLUMN workspace_policy text NOT NULL DEFAULT 'model_only' CHECK(workspace_policy IN('model_only','fork_parent')),
  ADD COLUMN workspace_state text NOT NULL DEFAULT 'model_only' CHECK(workspace_state IN('model_only','pending','ready')),
  ADD CONSTRAINT child_workspace_policy_state CHECK(
    (workspace_policy='model_only' AND workspace_state='model_only')
    OR (workspace_policy='fork_parent' AND workspace_state IN('pending','ready')));

ALTER TABLE zuno_enterprise_preview.session
  ADD COLUMN delegation_depth_limit integer NOT NULL DEFAULT 16 CHECK(delegation_depth_limit BETWEEN 0 AND 16);
ALTER TABLE zuno_enterprise_preview.session NO FORCE ROW LEVEL SECURITY;
UPDATE zuno_enterprise_preview.session SET delegation_depth_limit=0 WHERE parent_id IS NOT NULL;
ALTER TABLE zuno_enterprise_preview.session FORCE ROW LEVEL SECURITY;

CREATE TABLE zuno_enterprise_preview.child_workspace_preparation (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  child_job_id text NOT NULL,
  gateway_id text NOT NULL,
  admission jsonb NOT NULL CHECK(jsonb_typeof(admission)='object'),
  admission_digest text NOT NULL CHECK(length(admission_digest)=64),
  receipt jsonb CHECK(receipt IS NULL OR jsonb_typeof(receipt)='object'),
  receipt_digest text CHECK(receipt_digest IS NULL OR length(receipt_digest)=64),
  time_admitted bigint NOT NULL,
  time_completed bigint,
  PRIMARY KEY(tenant_id,principal_id,child_job_id),
  FOREIGN KEY(tenant_id,principal_id,child_job_id)
    REFERENCES zuno_enterprise_preview.runtime_child(tenant_id,principal_id,job_id),
  CHECK((receipt IS NULL AND receipt_digest IS NULL AND time_completed IS NULL)
     OR (receipt IS NOT NULL AND receipt_digest IS NOT NULL AND time_completed IS NOT NULL))
);
