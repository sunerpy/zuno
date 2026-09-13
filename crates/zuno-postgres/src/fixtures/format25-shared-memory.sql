CREATE TABLE zuno_enterprise_preview.shared_memory_space (
  tenant_id text NOT NULL,
  id text NOT NULL,
  workspace_id text NOT NULL,
  title text NOT NULL CHECK(length(title) BETWEEN 1 AND 256),
  enabled boolean NOT NULL,
  policy_revision bigint NOT NULL CHECK(policy_revision>0),
  document_revision bigint NOT NULL CHECK(document_revision>0),
  entries jsonb NOT NULL CHECK(jsonb_typeof(entries)='array'),
  content_digest text NOT NULL CHECK(length(content_digest)=64),
  character_limit integer NOT NULL CHECK(character_limit BETWEEN 1 AND 32768),
  PRIMARY KEY(tenant_id,id)
);
CREATE TABLE zuno_enterprise_preview.shared_memory_member (
  tenant_id text NOT NULL,
  space_id text NOT NULL,
  principal_id text NOT NULL,
  role text NOT NULL CHECK(role IN('reader','contributor','reviewer')),
  PRIMARY KEY(tenant_id,space_id,principal_id),
  FOREIGN KEY(tenant_id,space_id) REFERENCES zuno_enterprise_preview.shared_memory_space(tenant_id,id)
);
CREATE TABLE zuno_enterprise_preview.shared_memory_change (
  tenant_id text NOT NULL,
  space_id text NOT NULL,
  id text NOT NULL,
  author text NOT NULL,
  base_revision bigint NOT NULL CHECK(base_revision>0),
  policy_revision bigint NOT NULL CHECK(policy_revision>0),
  state text NOT NULL CHECK(state IN('pending','applied','rejected','undone','invalidated')),
  data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
  state_digest text NOT NULL CHECK(length(state_digest)=64),
  PRIMARY KEY(tenant_id,space_id,id),
  FOREIGN KEY(tenant_id,space_id) REFERENCES zuno_enterprise_preview.shared_memory_space(tenant_id,id)
);
CREATE TABLE zuno_enterprise_preview.shared_memory_revision (
  tenant_id text NOT NULL,
  space_id text NOT NULL,
  revision bigint NOT NULL CHECK(revision>0),
  entries jsonb NOT NULL CHECK(jsonb_typeof(entries)='array'),
  content_digest text NOT NULL CHECK(length(content_digest)=64),
  change_id text,
  actor text NOT NULL,
  operation text NOT NULL CHECK(operation IN('create','apply','undo')),
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,space_id,revision),
  FOREIGN KEY(tenant_id,space_id) REFERENCES zuno_enterprise_preview.shared_memory_space(tenant_id,id)
);
CREATE TABLE zuno_enterprise_preview.shared_memory_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  request_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  response jsonb NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,request_id)
);
CREATE TABLE zuno_enterprise_preview.shared_memory_audit (
  tenant_id text NOT NULL,
  space_id text NOT NULL,
  id text NOT NULL,
  actor jsonb NOT NULL,
  operation text NOT NULL,
  data jsonb NOT NULL,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,space_id,id),
  FOREIGN KEY(tenant_id,space_id) REFERENCES zuno_enterprise_preview.shared_memory_space(tenant_id,id)
);
