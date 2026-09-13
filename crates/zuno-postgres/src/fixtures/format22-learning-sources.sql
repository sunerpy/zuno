CREATE TABLE zuno_enterprise_preview.learning_root_scan (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  root_job_id text NOT NULL,
  source_version bigint NOT NULL DEFAULT 1 CHECK(source_version>0),
  scanned_version bigint NOT NULL DEFAULT 0 CHECK(scanned_version>=0 AND scanned_version<=source_version),
  updated_at bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,root_job_id),
  FOREIGN KEY(tenant_id,principal_id,root_job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id)
);
CREATE INDEX learning_root_scan_pending
  ON zuno_enterprise_preview.learning_root_scan(tenant_id,principal_id,updated_at,root_job_id)
  WHERE source_version>scanned_version;

CREATE TABLE zuno_enterprise_preview.learning_source_claim (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  root_job_id text NOT NULL,
  origin jsonb NOT NULL CHECK(jsonb_typeof(origin)='object' AND octet_length(origin::text)<=1024),
  source_digest text CHECK(source_digest IS NULL OR length(source_digest)=64),
  disposition text NOT NULL CHECK(disposition IN('captured','omitted','unavailable')),
  learning_job_id text,
  created_at bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,root_job_id,origin),
  FOREIGN KEY(tenant_id,principal_id,root_job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,learning_job_id)
    REFERENCES zuno_enterprise_preview.learning_execution(tenant_id,principal_id,job_id),
  CHECK(disposition<>'captured' OR (learning_job_id IS NOT NULL AND source_digest IS NOT NULL))
);
CREATE INDEX learning_source_claim_execution
  ON zuno_enterprise_preview.learning_source_claim(tenant_id,principal_id,learning_job_id);
