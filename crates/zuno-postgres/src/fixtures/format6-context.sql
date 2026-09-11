CREATE TABLE zuno_enterprise_preview.context_usage (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  source text NOT NULL,
  revision bigint NOT NULL CHECK(revision>=0),
  context_epoch bigint NOT NULL CHECK(context_epoch>=0),
  state jsonb NOT NULL CHECK(jsonb_typeof(state)='object'),
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,session_id,source),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id) ON DELETE CASCADE
);

CREATE TABLE zuno_enterprise_preview.input_execution_receipt (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  input_id text NOT NULL,
  state text NOT NULL CHECK(state IN('admitted','recorded','applied','completed','failed','cancelled')),
  turn_id text,
  applied_at bigint,
  completed_at bigint,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,input_id),
  FOREIGN KEY(tenant_id,principal_id,session_id,input_id)
    REFERENCES zuno_enterprise_preview.input(tenant_id,principal_id,session_id,id) ON DELETE CASCADE
);
CREATE INDEX input_execution_turn_idx
  ON zuno_enterprise_preview.input_execution_receipt(tenant_id,principal_id,session_id,turn_id,state);

ALTER TABLE zuno_enterprise_preview.input NO FORCE ROW LEVEL SECURITY;
INSERT INTO zuno_enterprise_preview.input_execution_receipt(
  tenant_id,principal_id,session_id,input_id,state,time_updated)
SELECT tenant_id,principal_id,session_id,id,
  CASE state WHEN 'consumed' THEN 'recorded'
             WHEN 'failed' THEN 'failed'
             WHEN 'cancelled' THEN 'cancelled'
             ELSE 'admitted' END,
  0
FROM zuno_enterprise_preview.input;
ALTER TABLE zuno_enterprise_preview.input FORCE ROW LEVEL SECURITY;

CREATE FUNCTION zuno_enterprise_preview.create_input_execution_receipt() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog AS $$
BEGIN
  INSERT INTO zuno_enterprise_preview.input_execution_receipt(
    tenant_id,principal_id,session_id,input_id,state,time_updated)
    VALUES(NEW.tenant_id,NEW.principal_id,NEW.session_id,NEW.id,'admitted',0);
  RETURN NEW;
END $$;
CREATE TRIGGER create_input_execution_receipt AFTER INSERT ON zuno_enterprise_preview.input
FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.create_input_execution_receipt();
REVOKE ALL ON FUNCTION zuno_enterprise_preview.create_input_execution_receipt() FROM PUBLIC;
