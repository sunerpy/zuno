CREATE TABLE operation_delivery(
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  completion TEXT,
  completion_digest TEXT,
  acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN(0,1)),
  scan_order INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(tenant,principal,operation_id),
  FOREIGN KEY(tenant,principal,operation_id) REFERENCES operation(tenant,principal,id),
  CHECK((completion IS NULL AND completion_digest IS NULL AND acknowledged=0)
     OR (completion IS NOT NULL AND completion_digest IS NOT NULL))
);
CREATE INDEX operation_delivery_scan_idx
  ON operation_delivery(acknowledged,scan_order,tenant,principal,operation_id);
INSERT INTO operation_delivery(tenant,principal,operation_id)
SELECT tenant,principal,id FROM operation;
