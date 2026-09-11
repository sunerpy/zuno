CREATE TABLE environment(
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  id TEXT NOT NULL,
  spec TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>0),
  state TEXT NOT NULL DEFAULT 'active' CHECK(state IN('active','releasing','released')),
  active_operation TEXT,
  PRIMARY KEY(tenant,principal,id)
);
CREATE TABLE operation(
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  id TEXT NOT NULL,
  environment_id TEXT NOT NULL,
  request_digest TEXT NOT NULL,
  data TEXT NOT NULL,
  PRIMARY KEY(tenant,principal,id),
  FOREIGN KEY(tenant,principal,environment_id) REFERENCES environment(tenant,principal,id)
);
CREATE TABLE gateway_format(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  version INTEGER NOT NULL,
  channel TEXT NOT NULL,
  source_digest TEXT NOT NULL,
  manifest TEXT NOT NULL
);
CREATE TABLE snapshot(
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  id TEXT NOT NULL,
  data TEXT NOT NULL,
  PRIMARY KEY(tenant,principal,id)
);
