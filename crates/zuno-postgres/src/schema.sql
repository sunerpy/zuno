CREATE TABLE zuno_enterprise_preview.workspace (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  title text NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id)
);
CREATE TABLE zuno_enterprise_preview.session (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  workspace_id text NOT NULL,
  title text NOT NULL,
  agent text,
  model jsonb,
  event_sequence bigint NOT NULL DEFAULT -1 CHECK(event_sequence>=-1),
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,workspace_id)
    REFERENCES zuno_enterprise_preview.workspace(tenant_id,principal_id,id)
);
CREATE INDEX session_owner_updated_idx
  ON zuno_enterprise_preview.session(tenant_id,principal_id,time_updated DESC,id DESC);
CREATE TABLE zuno_enterprise_preview.request_receipt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  client_id text NOT NULL,
  operation text NOT NULL,
  request_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  resource_id text NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,client_id,operation,request_id)
);
CREATE TABLE zuno_enterprise_preview.input (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  id text NOT NULL,
  request_key text NOT NULL,
  prompt jsonb NOT NULL,
  state text NOT NULL CHECK(state IN('queued','steering','promoted','consumed','cancelled','failed')),
  revision bigint NOT NULL DEFAULT 1 CHECK(revision>=1),
  admitted_sequence bigint NOT NULL CHECK(admitted_sequence>=0),
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,session_id,request_key),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id) ON DELETE CASCADE
);
CREATE TABLE zuno_enterprise_preview.event (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  id text NOT NULL,
  sequence bigint NOT NULL CHECK(sequence>=0),
  type text NOT NULL,
  version integer NOT NULL DEFAULT 1 CHECK(version>=1),
  data jsonb NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,session_id,sequence),
  UNIQUE(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id) ON DELETE CASCADE
);
