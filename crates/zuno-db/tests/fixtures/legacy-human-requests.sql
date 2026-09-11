-- Representative requests in the human_request layout shared by formats 5-12.
-- The released TUI, server and ACP question adapters store source="question"
-- with positional answers. Whitespace, unknown payload fields, response bytes,
-- revisions and terminal states are historical data, not migration input to rewrite.
INSERT INTO human_request
  (id,session_id,goal_id,kind,state,payload,response,message_id,call_id,revision,
   time_created,time_updated,time_resolved)
VALUES
  ('que_fixture_pending','ses_fixture_0001',NULL,'input','pending',
   '{ "source": "question", "questions": [{"question":"Which database — 哪个数据库?","header":"Database","options":[{"label":"SQLite","description":"Local storage"},{"label":"Postgres","description":"Remote storage"}],"custom":false},{"question":"Any migration notes?","header":"Notes","options":[]}], "legacyExtra": {"keep": true} }',
   NULL,'msg_fixture_0001','call_question_pending',1,1735689780000,1735689780000,NULL),
  ('que_fixture_answered','ses_fixture_0001',NULL,'input','answered',
   '{"source":"question","questions":[{"question":"Which database?","header":"Database","options":[{"label":"SQLite","description":"Local storage"}],"custom":false}]}',
   '{ "answers" : [ [ "SQLite" ] ], "legacyExtra": "keep response bytes" }',
   'msg_fixture_0001','call_question_answered',4,1735689780001,1735689780010,1735689780010),
  ('que_fixture_cancelled','ses_fixture_0001',NULL,'input','cancelled',
   '{"source":"question","questions":[{"question":"Any notes?","header":"Notes","options":[]}]}',
   '{"outcome":"cancelled"}',NULL,NULL,2,1735689780002,1735689780011,1735689780011),
  ('que_fixture_expired','ses_fixture_0001',NULL,'input','expired',
   '{"source":"question","questions":[{"question":"Any notes?","header":"Notes","options":[]}]}',
   '{"outcome":"expired"}',NULL,NULL,2,1735689780003,1735689780012,1735689780012),
  ('que_fixture_failed','ses_fixture_0001',NULL,'input','failed',
   '{"source":"question","questions":[{"question":"Any notes?","header":"Notes","options":[]}]}',
   '{"outcome":"failed"}',NULL,NULL,2,1735689780004,1735689780013,1735689780013),
  ('que_fixture_approval_label','ses_fixture_0001',NULL,'input','answered',
   '{"source":"question","questions":[{"question":"Choose a label","header":"Label","options":[{"label":"Approve","description":"A legacy answer label"},{"label":"Decline","description":"Another answer label"}],"custom":false}]}',
   '{"answers":[["Approve"]]}',NULL,NULL,2,1735689780005,1735689780014,1735689780014),
  ('req_fixture_permission','ses_fixture_0001','gol_fixture_0001','permission','pending',
   '{"permission":"shell","patterns":["cargo test"],"metadata":{"preserve":true}}',
   NULL,'msg_fixture_0001','call_permission_pending',1,1735689780006,1735689780006,NULL),
  ('req_fixture_other_input','ses_fixture_0001','gol_fixture_0001','input','pending',
   '{"source":"external-input","message":"Keep this historical input — 保留原始输入"}',
   NULL,NULL,NULL,3,1735689780007,1735689780008,NULL);
