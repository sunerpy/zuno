ALTER TABLE zuno_enterprise_preview.session
  ADD COLUMN parent_id text,
  ADD COLUMN context_epoch bigint NOT NULL DEFAULT 0 CHECK(context_epoch>=0),
  ADD COLUMN cost double precision NOT NULL DEFAULT 0 CHECK(cost>=0),
  ADD COLUMN tokens_input bigint NOT NULL DEFAULT 0 CHECK(tokens_input>=0),
  ADD COLUMN tokens_output bigint NOT NULL DEFAULT 0 CHECK(tokens_output>=0),
  ADD COLUMN tokens_reasoning bigint NOT NULL DEFAULT 0 CHECK(tokens_reasoning>=0),
  ADD COLUMN tokens_cache_read bigint NOT NULL DEFAULT 0 CHECK(tokens_cache_read>=0),
  ADD COLUMN tokens_cache_write bigint NOT NULL DEFAULT 0 CHECK(tokens_cache_write>=0),
  ADD COLUMN tokens_last_prompt bigint,
  ADD COLUMN tokens_context_limit bigint,
  ADD COLUMN tokens_accounting text,
  ADD COLUMN tokens_known boolean NOT NULL DEFAULT false,
  ADD COLUMN tokens_estimated_pending_prompt bigint,
  ADD COLUMN tokens_last_confirmed_at bigint,
  ADD CONSTRAINT session_parent_scope FOREIGN KEY(tenant_id,principal_id,parent_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id);

CREATE TABLE zuno_enterprise_preview.message (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  id text NOT NULL,
  role text NOT NULL CHECK(role IN('user','assistant')),
  data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  UNIQUE(tenant_id,principal_id,session_id,id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id) ON DELETE CASCADE
);
CREATE INDEX message_session_order_idx
  ON zuno_enterprise_preview.message(tenant_id,principal_id,session_id,time_created,id);

CREATE TABLE zuno_enterprise_preview.part (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  message_id text NOT NULL,
  id text NOT NULL,
  kind text NOT NULL CHECK(kind IN(
    'text','reasoning','tool','step-start','step-finish','patch',
    'file','compaction','subtask','snapshot','agent','retry'
  )),
  data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
  time_created bigint NOT NULL,
  time_updated bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,id),
  FOREIGN KEY(tenant_id,principal_id,session_id,message_id)
    REFERENCES zuno_enterprise_preview.message(tenant_id,principal_id,session_id,id) ON DELETE CASCADE
);
CREATE INDEX part_message_order_idx
  ON zuno_enterprise_preview.part(tenant_id,principal_id,session_id,message_id,id);
CREATE INDEX part_session_kind_idx
  ON zuno_enterprise_preview.part(tenant_id,principal_id,session_id,kind);
CREATE INDEX event_type_tail_idx
  ON zuno_enterprise_preview.event(tenant_id,principal_id,session_id,type,sequence DESC);

CREATE TABLE zuno_enterprise_preview.provider_retry_backoff (
  tenant_id text NOT NULL,
  principal_id text NOT NULL,
  session_id text NOT NULL,
  request_id text NOT NULL,
  turn_id text NOT NULL,
  failed_attempt integer NOT NULL CHECK(failed_attempt>0),
  next_attempt integer NOT NULL CHECK(next_attempt>failed_attempt),
  max_attempts integer NOT NULL CHECK(max_attempts>=next_attempt),
  reason text NOT NULL,
  delay_ms bigint NOT NULL CHECK(delay_ms>0),
  retry_at_ms bigint NOT NULL,
  scheduled_at_ms bigint NOT NULL,
  PRIMARY KEY(tenant_id,principal_id,session_id),
  FOREIGN KEY(tenant_id,principal_id,session_id)
    REFERENCES zuno_enterprise_preview.session(tenant_id,principal_id,id) ON DELETE CASCADE
);
