-- Native Job/checkpoint data using the exact format-2 DDL from PR #184.
UPDATE zuno_enterprise_preview.input SET state='consumed' WHERE id='legacy-input';
INSERT INTO zuno_enterprise_preview.agent_job(
  tenant_id,principal_id,id,parent_session_id,subject_kind,subject_payload,status,report_delivery,created_seq,time_created,time_updated)
VALUES('migration-fixture','owner','legacy-job','legacy-session','root-turn',
  '{"kind":"rootTurn","turnID":"legacy-turn"}','running','quiet',1,1000,1003);
INSERT INTO zuno_enterprise_preview.runtime_job(
  tenant_id,principal_id,job_id,session_id,turn_id,input_id,request_digest,principal,configuration,
  phase,checkpoint,checkpoint_version,input_version,ready_at,time_created,time_updated)
VALUES('migration-fixture','owner','legacy-job','legacy-session','legacy-turn','legacy-input',
  'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
  '{"tenantId":"migration-fixture","principalId":"owner","kind":"user","clientId":"old-web","policyRevision":1}',
  '{"id":"legacy-definition","version":1,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}',
  'ready',
  '{"jobId":"legacy-job","sessionId":"legacy-session","turnId":"legacy-turn","driver":"default","schemaVersion":2,"reference":{"spentTokens":1234,"toolCalls":9,"elapsedMs":7500,"eventId":"legacy-checkpoint"}}',
  1,1,1003,1000,1003);
INSERT INTO zuno_enterprise_preview.runtime_attempt(tenant_id,principal_id,id,job_id,worker_id,lease_epoch,state,started_at,finished_at)
VALUES('migration-fixture','owner','legacy-attempt','legacy-job','retired-worker',7,'released',1001,1003);
UPDATE zuno_enterprise_preview.runtime_session SET current_job_id='legacy-job',lease_epoch=7
WHERE session_id='legacy-session';
INSERT INTO zuno_enterprise_preview.runtime_owner_schedule(tenant_id,principal_id,last_dispatch_sequence)
VALUES('migration-fixture','owner',12);
INSERT INTO zuno_enterprise_preview.event(tenant_id,principal_id,session_id,id,sequence,type,data)
VALUES('migration-fixture','owner','legacy-session','legacy-job-event',1,'agent.job.created',
  '{"jobID":"legacy-job","subject":{"kind":"rootTurn","turnID":"legacy-turn"}}');
INSERT INTO zuno_enterprise_preview.event(tenant_id,principal_id,session_id,id,sequence,type,data)
VALUES('migration-fixture','owner','legacy-session','legacy-checkpoint',2,'runtime.checkpoint.committed',
  '{"jobID":"legacy-job","checkpointVersion":1}');
UPDATE zuno_enterprise_preview.session SET event_sequence=2 WHERE id='legacy-session';
