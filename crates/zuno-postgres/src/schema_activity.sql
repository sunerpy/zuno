-- Message execution identity is owned by the state service. Provider call IDs
-- may repeat in another turn of the same session.
ALTER TABLE zuno_enterprise_preview.message ADD COLUMN execution_job_id text,
  ADD CONSTRAINT message_execution_job FOREIGN KEY(tenant_id,principal_id,execution_job_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id);
ALTER TABLE zuno_enterprise_preview.message NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.runtime_job NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.event NO FORCE ROW LEVEL SECURITY;
WITH candidates AS (
  SELECT m.tenant_id,m.principal_id,m.id,j.job_id
  FROM zuno_enterprise_preview.message m
  JOIN zuno_enterprise_preview.runtime_job j
    ON j.tenant_id=m.tenant_id AND j.principal_id=m.principal_id AND j.session_id=m.session_id AND j.input_id=m.id
  UNION
  SELECT m.tenant_id,m.principal_id,m.id,j.job_id
  FROM zuno_enterprise_preview.message m
  JOIN zuno_enterprise_preview.event e
    ON e.tenant_id=m.tenant_id AND e.principal_id=m.principal_id AND e.session_id=m.session_id
      AND e.type='session.provider.request' AND e.data->>'requestID'=m.data->>'requestID'
  JOIN zuno_enterprise_preview.runtime_job j
    ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.session_id=e.session_id AND j.turn_id=e.data->>'turnID'
), resolved AS (
  SELECT tenant_id,principal_id,id,min(job_id) AS job_id FROM candidates
  GROUP BY tenant_id,principal_id,id HAVING count(DISTINCT job_id)=1
)
UPDATE zuno_enterprise_preview.message m SET execution_job_id=r.job_id
FROM resolved r WHERE m.tenant_id=r.tenant_id AND m.principal_id=r.principal_id AND m.id=r.id;
ALTER TABLE zuno_enterprise_preview.message FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.runtime_job FORCE ROW LEVEL SECURITY;
ALTER TABLE zuno_enterprise_preview.event FORCE ROW LEVEL SECURITY;

CREATE TABLE zuno_enterprise_preview.activity_session (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  sequence bigint NOT NULL DEFAULT 0 CHECK(sequence>=0),
  PRIMARY KEY(tenant_id,principal_id,session_id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id) ON DELETE CASCADE
);
ALTER TABLE zuno_enterprise_preview.session NO FORCE ROW LEVEL SECURITY;
INSERT INTO zuno_enterprise_preview.activity_session(tenant_id,principal_id,session_id)
SELECT tenant_id,principal_id,id FROM zuno_enterprise_preview.session;
ALTER TABLE zuno_enterprise_preview.session FORCE ROW LEVEL SECURITY;
CREATE FUNCTION zuno_enterprise_preview.create_activity_session() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog AS $$
BEGIN
  INSERT INTO zuno_enterprise_preview.activity_session(tenant_id,principal_id,session_id)
    VALUES(NEW.tenant_id,NEW.principal_id,NEW.id);
  RETURN NEW;
END $$;
CREATE TRIGGER create_activity_session AFTER INSERT ON zuno_enterprise_preview.session
FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.create_activity_session();
REVOKE ALL ON FUNCTION zuno_enterprise_preview.create_activity_session() FROM PUBLIC;

CREATE TABLE zuno_enterprise_preview.activity_item (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  id text NOT NULL,
  position bigint NOT NULL CHECK(position>0),
  revision bigint NOT NULL CHECK(revision>=position),
  record jsonb NOT NULL CHECK(jsonb_typeof(record)='object'),
  PRIMARY KEY(tenant_id,principal_id,session_id,id),
  UNIQUE(tenant_id,principal_id,session_id,position),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.activity_session(tenant_id,principal_id,session_id) ON DELETE CASCADE
);
CREATE TABLE zuno_enterprise_preview.activity_frame (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  sequence bigint NOT NULL CHECK(sequence>0),
  item_id text NOT NULL,
  version integer NOT NULL CHECK(version=1),
  record jsonb NOT NULL CHECK(jsonb_typeof(record)='object'),
  PRIMARY KEY(tenant_id,principal_id,session_id,sequence),
  FOREIGN KEY(tenant_id,principal_id,session_id,item_id)
    REFERENCES zuno_enterprise_preview.activity_item(tenant_id,principal_id,session_id,id) ON DELETE CASCADE
);
CREATE INDEX activity_item_history
  ON zuno_enterprise_preview.activity_item(tenant_id,principal_id,session_id,position DESC);
CREATE INDEX activity_frame_item_history
  ON zuno_enterprise_preview.activity_frame(tenant_id,principal_id,session_id,item_id,sequence DESC);
