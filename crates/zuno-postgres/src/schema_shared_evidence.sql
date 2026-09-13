CREATE TABLE zuno_enterprise_preview.shared_memory_evidence (
  tenant_id text NOT NULL,
  space_id text NOT NULL,
  id text NOT NULL,
  principal_id text NOT NULL,
  workspace_id text NOT NULL,
  evidence_id text NOT NULL,
  evidence_digest text NOT NULL CHECK(length(evidence_digest)=64),
  excerpt text NOT NULL CHECK(octet_length(excerpt) BETWEEN 1 AND 2048),
  source_kind text NOT NULL CHECK(source_kind IN('user_statement','successful_operation')),
  actor jsonb NOT NULL CHECK(jsonb_typeof(actor)='object'),
  revision bigint NOT NULL CHECK(revision>0),
  active boolean NOT NULL,
  PRIMARY KEY(tenant_id,space_id,id),
  FOREIGN KEY(tenant_id,space_id) REFERENCES zuno_enterprise_preview.shared_memory_space(tenant_id,id),
  FOREIGN KEY(tenant_id,principal_id,evidence_id) REFERENCES zuno_enterprise_preview.memory_evidence(tenant_id,principal_id,id)
);
CREATE INDEX shared_memory_evidence_owner ON zuno_enterprise_preview.shared_memory_evidence(tenant_id,principal_id,space_id,id);
CREATE TABLE zuno_enterprise_preview.shared_memory_evidence_audit (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  request_id text NOT NULL,
  space_id text NOT NULL,
  operation text NOT NULL,
  data jsonb NOT NULL,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,request_id)
);
CREATE TABLE zuno_enterprise_preview.shared_memory_support (
  tenant_id text NOT NULL,
  space_id text NOT NULL,
  content_digest text NOT NULL CHECK(length(content_digest)=64),
  content text NOT NULL,
  grants jsonb NOT NULL CHECK(jsonb_typeof(grants)='array' AND jsonb_array_length(grants) BETWEEN 1 AND 16),
  PRIMARY KEY(tenant_id,space_id,content_digest),
  FOREIGN KEY(tenant_id,space_id) REFERENCES zuno_enterprise_preview.shared_memory_space(tenant_id,id)
);
