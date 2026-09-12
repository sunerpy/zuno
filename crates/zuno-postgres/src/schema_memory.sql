CREATE TABLE zuno_enterprise_preview.memory_policy (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  revision bigint NOT NULL CHECK(revision > 0),
  use_memories boolean NOT NULL DEFAULT true,
  generate_private boolean NOT NULL DEFAULT false,
  PRIMARY KEY(tenant_id,principal_id)
);
CREATE TABLE zuno_enterprise_preview.session_memory_policy (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  revision bigint NOT NULL CHECK(revision > 0),
  use_memories boolean NOT NULL DEFAULT true,
  generate_private boolean NOT NULL DEFAULT false,
  PRIMARY KEY(tenant_id,principal_id,session_id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id) ON DELETE CASCADE
);
CREATE TABLE zuno_enterprise_preview.memory_document (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  key text NOT NULL,
  scope text NOT NULL CHECK(scope IN('global','project')),
  revision bigint NOT NULL CHECK(revision > 0),
  data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
  PRIMARY KEY(tenant_id,principal_id,key)
);
CREATE TABLE zuno_enterprise_preview.memory_revision (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  key text NOT NULL,
  revision bigint NOT NULL CHECK(revision > 0),
  entries jsonb NOT NULL CHECK(jsonb_typeof(entries)='array'),
  content_digest text NOT NULL CHECK(length(content_digest)=64),
  operation text NOT NULL CHECK(operation IN('adopt','apply','undo')),
  candidate_id text,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,key,revision),
  FOREIGN KEY(tenant_id,principal_id,key)
    REFERENCES zuno_enterprise_preview.memory_document(tenant_id,principal_id,key)
);
CREATE TABLE zuno_enterprise_preview.memory_candidate (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  key text NOT NULL,
  status text NOT NULL CHECK(status IN('pending','applied','rejected','failed','undone','uncertain','applying','undoing')),
  source_session_id text,
  source_message_id text,
  fingerprint text,
  data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,key)
    REFERENCES zuno_enterprise_preview.memory_document(tenant_id,principal_id,key),
  FOREIGN KEY(tenant_id,principal_id,source_session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,key,source_session_id,source_message_id,fingerprint)
);
CREATE INDEX memory_candidates_by_document ON zuno_enterprise_preview.memory_candidate(tenant_id,principal_id,key,status,id);
CREATE TABLE zuno_enterprise_preview.memory_evidence (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  workspace_id text NOT NULL,
  origin jsonb NOT NULL CHECK(jsonb_typeof(origin)='object'),
  excerpt text NOT NULL CHECK(length(excerpt) BETWEEN 1 AND 2048),
  digest text NOT NULL CHECK(length(digest)=64),
  source_digest text NOT NULL CHECK(length(source_digest)=64),
  forgotten boolean NOT NULL DEFAULT false,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,workspace_id)
    REFERENCES zuno_enterprise_preview.workspace(tenant_id,principal_id,id)
);
CREATE TABLE zuno_enterprise_preview.memory_provenance (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  key text NOT NULL,
  content text NOT NULL,
  evidence jsonb NOT NULL CHECK(jsonb_typeof(evidence)='array'),
  PRIMARY KEY(tenant_id,principal_id,key,content),
  FOREIGN KEY(tenant_id,principal_id,key)
    REFERENCES zuno_enterprise_preview.memory_document(tenant_id,principal_id,key)
);
CREATE TABLE zuno_enterprise_preview.memory_retired (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  key text NOT NULL,
  content_digest text NOT NULL CHECK(length(content_digest)=64),
  content text NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,key,content_digest),
  FOREIGN KEY(tenant_id,principal_id,key)
    REFERENCES zuno_enterprise_preview.memory_document(tenant_id,principal_id,key)
);
CREATE TABLE zuno_enterprise_preview.learning_job (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  workspace_id text NOT NULL,
  session_id text,
  kind text NOT NULL CHECK(kind IN('extraction','project_aggregation','global_aggregation','evaluation','skill_apply','skill_undo')),
  status text NOT NULL CHECK(status IN('queued','running','completed','skipped','failed','uncertain')),
  owner_id text,
  lease_token text,
  lease_expires bigint,
  payload jsonb NOT NULL CHECK(jsonb_typeof(payload)='object'),
  result jsonb,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,workspace_id)
    REFERENCES zuno_enterprise_preview.workspace(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id)
);
CREATE TABLE zuno_enterprise_preview.memory_maintenance_state (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  workspace_id text NOT NULL,
  key text NOT NULL,
  input_digest text NOT NULL CHECK(length(input_digest)=64),
  global_revision bigint NOT NULL,
  project_revision bigint NOT NULL,
  job_id text NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,workspace_id,key),
  FOREIGN KEY(tenant_id,principal_id,key)
    REFERENCES zuno_enterprise_preview.memory_document(tenant_id,principal_id,key),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.learning_job(tenant_id,principal_id,id)
);
CREATE TABLE zuno_enterprise_preview.memory_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  workspace_id text NOT NULL,
  request_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  response jsonb NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,workspace_id,request_id)
);
CREATE TABLE zuno_enterprise_preview.memory_audit (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  workspace_id text NOT NULL,
  actor jsonb NOT NULL,
  action text NOT NULL,
  data jsonb NOT NULL,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id)
);
