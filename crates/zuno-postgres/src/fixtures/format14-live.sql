CREATE TABLE zuno_enterprise_preview.live_progress (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  job_id text NOT NULL,
  session_id text NOT NULL,
  attempt_id text NOT NULL,
  epoch bigint NOT NULL CHECK(epoch>0),
  generation text NOT NULL,
  message_id text,
  sequence bigint NOT NULL CHECK(sequence>0),
  body jsonb NOT NULL CHECK(jsonb_typeof(body)='object'),
  body_digest text NOT NULL CHECK(length(body_digest)=64),
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,job_id),
  FOREIGN KEY(tenant_id,principal_id,job_id,session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id) ON DELETE CASCADE
);
CREATE FUNCTION zuno_enterprise_preview.clear_live_progress() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog AS $$
BEGIN
  DELETE FROM zuno_enterprise_preview.live_progress
    WHERE tenant_id=NEW.tenant_id AND principal_id=NEW.principal_id AND job_id=NEW.job_id;
  RETURN NEW;
END $$;
CREATE TRIGGER clear_live_progress AFTER UPDATE OF phase,active_attempt_id ON zuno_enterprise_preview.runtime_job
FOR EACH ROW WHEN(OLD.phase IS DISTINCT FROM NEW.phase OR OLD.active_attempt_id IS DISTINCT FROM NEW.active_attempt_id)
EXECUTE FUNCTION zuno_enterprise_preview.clear_live_progress();
REVOKE ALL ON FUNCTION zuno_enterprise_preview.clear_live_progress() FROM PUBLIC;
