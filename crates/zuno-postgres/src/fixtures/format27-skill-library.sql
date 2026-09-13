CREATE TABLE zuno_enterprise_preview.skill_installation (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  workspace_id text NOT NULL,
  candidate_id text NOT NULL,
  name text NOT NULL,
  description text NOT NULL,
  revision bigint NOT NULL CHECK(revision>0),
  source text NOT NULL,
  content text NOT NULL,
  content_digest text NOT NULL CHECK(length(content_digest)=64),
  active boolean NOT NULL DEFAULT false,
  evaluation_job_id text NOT NULL,
  candidate_digest text NOT NULL CHECK(length(candidate_digest)=64),
  evaluation_report jsonb NOT NULL CHECK(jsonb_typeof(evaluation_report)='object'),
  PRIMARY KEY(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,workspace_id,name),
  FOREIGN KEY(tenant_id,principal_id,candidate_id)
    REFERENCES zuno_enterprise_preview.skill_candidate(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,evaluation_job_id)
    REFERENCES zuno_enterprise_preview.learning_job(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,workspace_id)
    REFERENCES zuno_enterprise_preview.workspace(tenant_id,principal_id,id)
);
CREATE TABLE zuno_enterprise_preview.skill_installation_revision (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  installation_id text NOT NULL,
  revision bigint NOT NULL CHECK(revision>0),
  data jsonb NOT NULL,
  actor jsonb NOT NULL,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,installation_id,revision),
  FOREIGN KEY(tenant_id,principal_id,installation_id)
    REFERENCES zuno_enterprise_preview.skill_installation(tenant_id,principal_id,id)
);
CREATE TABLE zuno_enterprise_preview.skill_installation_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  request_id text NOT NULL,
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  response jsonb NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,request_id)
);
