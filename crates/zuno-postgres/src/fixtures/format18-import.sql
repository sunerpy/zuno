CREATE TABLE zuno_enterprise_preview.workspace_import (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  session_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  assignment jsonb NOT NULL CHECK(jsonb_typeof(assignment)='object'),
  assignment_digest text NOT NULL CHECK(length(assignment_digest)=64),
  state text NOT NULL CHECK(state IN('uploading','initializing','ready','cancelled')),
  receipt jsonb CHECK(receipt IS NULL OR jsonb_typeof(receipt)='object'),
  receipt_digest text CHECK(receipt_digest IS NULL OR length(receipt_digest)=64),
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id),
  CHECK((receipt IS NULL)=(receipt_digest IS NULL)),
  CHECK((state='ready')=(receipt IS NOT NULL))
);
CREATE UNIQUE INDEX workspace_import_active ON zuno_enterprise_preview.workspace_import(tenant_id,principal_id,session_id)
  WHERE state<>'cancelled';
