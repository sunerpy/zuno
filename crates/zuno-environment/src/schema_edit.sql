CREATE TABLE workspace_edit(
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  id TEXT NOT NULL,
  environment_id TEXT NOT NULL,
  request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
  admission TEXT NOT NULL,
  volume TEXT NOT NULL,
  nonce TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN('preparing','committed','cancelled','uncertain')),
  receipt TEXT,
  completion TEXT,
  completion_digest TEXT,
  acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN(0,1)),
  scan_order INTEGER NOT NULL DEFAULT 0,
  retry_at INTEGER NOT NULL DEFAULT 0,
  retry_count INTEGER NOT NULL DEFAULT 0 CHECK(retry_count>=0),
  PRIMARY KEY(tenant,principal,id),
  FOREIGN KEY(tenant,principal,environment_id) REFERENCES environment(tenant,principal,id),
  CHECK((receipt IS NOT NULL)=(state='committed')),
  CHECK((completion IS NULL)=(completion_digest IS NULL)),
  CHECK(completion_digest IS NULL OR length(completion_digest)=64),
  CHECK(acknowledged=0 OR completion IS NOT NULL)
);
ALTER TABLE environment_volume RENAME TO environment_volume_v4;
CREATE TABLE environment_volume(
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  environment_id TEXT NOT NULL,
  merge_id TEXT,
  edit_id TEXT,
  PRIMARY KEY(tenant,principal,environment_id),
  FOREIGN KEY(tenant,principal,environment_id) REFERENCES environment(tenant,principal,id),
  FOREIGN KEY(tenant,principal,merge_id) REFERENCES workspace_merge(tenant,principal,id),
  FOREIGN KEY(tenant,principal,edit_id) REFERENCES workspace_edit(tenant,principal,id),
  CHECK((merge_id IS NULL)<>(edit_id IS NULL))
);
INSERT INTO environment_volume(tenant,principal,environment_id,merge_id)
  SELECT tenant,principal,environment_id,merge_id FROM environment_volume_v4;
DROP TABLE environment_volume_v4;
