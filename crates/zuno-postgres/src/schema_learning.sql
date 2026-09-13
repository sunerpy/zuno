ALTER TABLE zuno_enterprise_preview.memory_policy
  ADD COLUMN automatic_private boolean NOT NULL DEFAULT false,
  ADD COLUMN automation_actor jsonb,
  ADD COLUMN automation_since bigint,
  ADD CONSTRAINT memory_automation_actor_object
    CHECK(automation_actor IS NULL OR jsonb_typeof(automation_actor)='object'),
  ADD CONSTRAINT memory_automation_actor_required
    CHECK(automatic_private=(automation_actor IS NOT NULL) AND automatic_private=(automation_since IS NOT NULL));
ALTER TABLE zuno_enterprise_preview.session_memory_policy
  ADD COLUMN automatic_private boolean NOT NULL DEFAULT false;

CREATE TABLE zuno_enterprise_preview.learning_execution (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  source_job_id text NOT NULL,
  phase text NOT NULL CHECK(phase IN('extraction','maintenance')),
  principal jsonb NOT NULL CHECK(jsonb_typeof(principal)='object'),
  configuration jsonb NOT NULL CHECK(jsonb_typeof(configuration)='object'),
  input jsonb NOT NULL CHECK(jsonb_typeof(input)='object'),
  input_digest text NOT NULL CHECK(length(input_digest)=64),
  limits jsonb NOT NULL CHECK(jsonb_typeof(limits)='object'),
  context jsonb NOT NULL CHECK(jsonb_typeof(context)='object'),
  attempt bigint NOT NULL DEFAULT 0 CHECK(attempt>=0),
  charged_tokens bigint NOT NULL DEFAULT 0 CHECK(charged_tokens>=0),
  reserved_tokens bigint NOT NULL DEFAULT 0 CHECK(reserved_tokens>=0),
  ready_at bigint NOT NULL,
  deadline_at bigint,
  created_at bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.learning_job(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,source_job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id)
);
CREATE INDEX learning_execution_due
  ON zuno_enterprise_preview.learning_execution(tenant_id,principal_id,ready_at,job_id);
CREATE TABLE zuno_enterprise_preview.learning_execution_attempt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  epoch bigint NOT NULL CHECK(epoch>0),
  worker_id text NOT NULL,
  lease_token text NOT NULL CHECK(length(lease_token)=64),
  started_at bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,job_id,epoch),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.learning_execution(tenant_id,principal_id,job_id)
);
CREATE TABLE zuno_enterprise_preview.learning_model_request (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  request_id text NOT NULL,
  epoch bigint NOT NULL,
  request jsonb NOT NULL CHECK(jsonb_typeof(request)='object'),
  request_digest text NOT NULL CHECK(length(request_digest)=64),
  reserved_tokens bigint NOT NULL CHECK(reserved_tokens>0),
  state text NOT NULL CHECK(state IN('prepared','completed','failed','unknown')),
  outcome jsonb CHECK(outcome IS NULL OR jsonb_typeof(outcome)='object'),
  outcome_digest text CHECK(outcome_digest IS NULL OR length(outcome_digest)=64),
  created_at bigint NOT NULL,
  completed_at bigint,
  PRIMARY KEY(tenant_id,principal_id,job_id,request_id),
  FOREIGN KEY(tenant_id,principal_id,job_id,epoch)
    REFERENCES zuno_enterprise_preview.learning_execution_attempt(tenant_id,principal_id,job_id,epoch),
  CHECK((outcome IS NULL)=(outcome_digest IS NULL))
);
CREATE TABLE zuno_enterprise_preview.learning_experience (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  id text NOT NULL,
  workspace_id text NOT NULL,
  job_id text NOT NULL,
  ordinal integer NOT NULL CHECK(ordinal>=0),
  data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
  evidence jsonb NOT NULL CHECK(jsonb_typeof(evidence)='array'),
  hints jsonb NOT NULL CHECK(jsonb_typeof(hints)='array'),
  created_at bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,job_id,ordinal),
  FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.learning_execution(tenant_id,principal_id,job_id)
);
CREATE INDEX learning_experience_workspace
  ON zuno_enterprise_preview.learning_experience(tenant_id,principal_id,workspace_id,created_at,id);
