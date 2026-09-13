ALTER TABLE zuno_enterprise_preview.learning_execution
  DROP CONSTRAINT learning_execution_phase_check,
  ADD CONSTRAINT learning_execution_phase_check CHECK(phase IN('extraction','maintenance','skill_evaluation'));
CREATE TABLE zuno_enterprise_preview.skill_candidate (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  source_job_id text NOT NULL,
  session_id text NOT NULL,
  workspace_id text NOT NULL,
  configuration jsonb NOT NULL,
  evaluation jsonb NOT NULL,
  input jsonb NOT NULL CHECK(jsonb_typeof(input)='object'),
  input_digest text NOT NULL CHECK(length(input_digest)=64),
  state text NOT NULL CHECK(state IN('pending_review','evaluating','passed','failed','cancelled')),
  evaluation_job_id text,
  reviewer jsonb,
  policy_revision bigint,
  report jsonb,
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,source_job_id,session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id),
  FOREIGN KEY(tenant_id,principal_id,evaluation_job_id)
    REFERENCES zuno_enterprise_preview.learning_job(tenant_id,principal_id,id),
  CHECK((evaluation_job_id IS NULL)=(reviewer IS NULL)),
  CHECK((reviewer IS NULL)=(policy_revision IS NULL))
);
CREATE TABLE zuno_enterprise_preview.skill_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  request_id text NOT NULL,
  request_digest text NOT NULL,
  response jsonb NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,request_id)
);
CREATE TABLE zuno_enterprise_preview.skill_audit (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  candidate_id text NOT NULL,
  actor jsonb NOT NULL,
  operation text NOT NULL,
  data jsonb NOT NULL,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,candidate_id)
    REFERENCES zuno_enterprise_preview.skill_candidate(tenant_id,principal_id,id)
);
