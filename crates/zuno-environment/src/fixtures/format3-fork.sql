CREATE TABLE environment_fork(
  tenant TEXT NOT NULL,
  principal TEXT NOT NULL,
  environment_id TEXT NOT NULL,
  snapshot_id TEXT NOT NULL,
  request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
  data TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN('restoring','committed')),
  PRIMARY KEY(tenant,principal,environment_id),
  FOREIGN KEY(tenant,principal,snapshot_id) REFERENCES snapshot(tenant,principal,id)
);
