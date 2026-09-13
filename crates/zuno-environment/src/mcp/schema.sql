CREATE TABLE mcp_format (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  version INTEGER NOT NULL CHECK(version=1),
  channel TEXT NOT NULL CHECK(channel='enterprise-preview'),
  source_digest TEXT NOT NULL,
  manifest TEXT NOT NULL
);
CREATE TABLE mcp_operation (
  key TEXT PRIMARY KEY,
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  admission TEXT NOT NULL,
  admission_digest TEXT NOT NULL,
  receipt TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN('queued','running','succeeded','failed','cancelled','uncertain')),
  acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN(0,1)),
  scan_order INTEGER NOT NULL DEFAULT 0,
  retry_at INTEGER NOT NULL DEFAULT 0,
  retry_count INTEGER NOT NULL DEFAULT 0,
  UNIQUE(tenant,principal,operation_id)
);
CREATE INDEX mcp_delivery ON mcp_operation(acknowledged,retry_at,scan_order);
