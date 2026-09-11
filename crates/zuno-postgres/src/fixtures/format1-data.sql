-- Format 1 schema comes from schema.sql, frozen at preview PR #182.
INSERT INTO zuno_enterprise_preview.workspace(tenant_id,principal_id,id,title)
VALUES('migration-fixture','owner','legacy-workspace','Preserved workspace');
INSERT INTO zuno_enterprise_preview.session(
  tenant_id,principal_id,id,workspace_id,title,agent,model,event_sequence,time_created,time_updated)
VALUES('migration-fixture','owner','legacy-session','legacy-workspace','Preserved research',
  'plan','{"providerId":"fixture","modelId":"model"}',0,1000,1001);
INSERT INTO zuno_enterprise_preview.input(
  tenant_id,principal_id,session_id,id,request_key,prompt,state,revision,admitted_sequence,time_created)
VALUES('migration-fixture','owner','legacy-session','legacy-input','legacy-request',
  '{"kind":"user","prompt":{"text":"Preserve this investigation","files":[],"agents":[]}}','queued',1,0,1001);
INSERT INTO zuno_enterprise_preview.event(tenant_id,principal_id,session_id,id,sequence,type,data)
VALUES('migration-fixture','owner','legacy-session','legacy-event',0,'session.input.admitted',
  '{"inputID":"legacy-input","state":"queued"}');
INSERT INTO zuno_enterprise_preview.request_receipt(
  tenant_id,principal_id,client_id,operation,request_id,request_digest,resource_id)
VALUES('migration-fixture','owner','old-web','queue-text:legacy-session','legacy-request',
  'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','legacy-input');
