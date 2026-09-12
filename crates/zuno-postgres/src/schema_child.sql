-- A native Job belongs to its delegating parent, while its runtime execution
-- belongs to the distinct session driven by a Worker. Root Jobs keep both equal.
ALTER TABLE zuno_enterprise_preview.runtime_job
  DROP CONSTRAINT runtime_job_tenant_id_principal_id_job_id_session_id_fkey;
ALTER TABLE zuno_enterprise_preview.runtime_job
  ADD CONSTRAINT runtime_job_subject FOREIGN KEY(tenant_id,principal_id,job_id)
    REFERENCES zuno_enterprise_preview.agent_job(tenant_id,principal_id,id),
  ADD CONSTRAINT runtime_job_execution UNIQUE(tenant_id,principal_id,job_id,session_id);
-- PostgreSQL truncates generated constraint names. Match the exact historical
-- relation/column binding instead of guessing its truncated spelling.
DO $$
DECLARE old_binding text; matches integer;
BEGIN
  SELECT min(c.conname),count(*) INTO old_binding,matches
  FROM pg_constraint c
  WHERE c.contype='f'
    AND c.conrelid='zuno_enterprise_preview.runtime_session'::regclass
    AND c.confrelid='zuno_enterprise_preview.agent_job'::regclass
    AND (SELECT array_agg(a.attname::text ORDER BY k.ordinality)
      FROM unnest(c.conkey) WITH ORDINALITY k(attnum,ordinality)
      JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=k.attnum)
      = ARRAY['tenant_id','principal_id','current_job_id','session_id'];
  IF matches<>1 THEN RAISE EXCEPTION 'historical runtime session binding is missing or ambiguous'; END IF;
  EXECUTE format('ALTER TABLE zuno_enterprise_preview.runtime_session DROP CONSTRAINT %I',old_binding);
END $$;
ALTER TABLE zuno_enterprise_preview.runtime_session
  ADD CONSTRAINT runtime_session_execution FOREIGN KEY(tenant_id,principal_id,current_job_id,session_id)
    REFERENCES zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id);
