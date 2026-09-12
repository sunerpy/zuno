-- BFF-only authentication state is tenant-scoped before a user is identified.
-- No Worker or public client receives a database credential.
CREATE TABLE zuno_enterprise_preview.browser_login (
  tenant_id text NOT NULL,
  state_hash bytea NOT NULL CHECK(octet_length(state_hash)=32),
  browser_hash bytea NOT NULL CHECK(octet_length(browser_hash)=32),
  expires_at bigint NOT NULL CHECK(expires_at>0),
  encrypted jsonb NOT NULL CHECK(jsonb_typeof(encrypted)='object'),
  PRIMARY KEY(tenant_id,state_hash)
);
CREATE INDEX browser_login_expiry_idx
  ON zuno_enterprise_preview.browser_login(tenant_id,expires_at);

CREATE TABLE zuno_enterprise_preview.browser_session (
  tenant_id text NOT NULL,
  token_hash bytea NOT NULL CHECK(octet_length(token_hash)=32),
  principal_id text NOT NULL,
  expires_at bigint NOT NULL CHECK(expires_at>0),
  identity jsonb NOT NULL CHECK(jsonb_typeof(identity)='object'),
  PRIMARY KEY(tenant_id,token_hash)
);
CREATE INDEX browser_session_expiry_idx
  ON zuno_enterprise_preview.browser_session(tenant_id,expires_at);

CREATE TABLE zuno_enterprise_preview.authentication_audit (
  tenant_id text NOT NULL,
  id text NOT NULL,
  principal_id text NOT NULL,
  type text NOT NULL,
  time_created bigint NOT NULL,
  PRIMARY KEY(tenant_id,id)
);
