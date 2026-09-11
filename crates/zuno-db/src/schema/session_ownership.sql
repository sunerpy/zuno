-- Ownership is separate from user-editable session metadata and from the
-- application/policy identity captured by a particular request.
CREATE TABLE session_ownership (
  session_id text PRIMARY KEY NOT NULL,
  tenant_id text NOT NULL CHECK (length(tenant_id) BETWEEN 1 AND 128),
  principal_id text NOT NULL CHECK (length(principal_id) BETWEEN 1 AND 128),
  FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
);

-- Published local databases have one explicit local owner. Import into an
-- enterprise namespace is a separate, authorized operation.
INSERT INTO session_ownership (session_id, tenant_id, principal_id)
  SELECT id, 'local', 'local-user' FROM session;

-- Raw local insertions keep the same invariant as the public creation service.
-- A service creating an authenticated root replaces this initial binding in
-- the same creation transaction, before any observer can see it.
CREATE TRIGGER session_ownership_insert AFTER INSERT ON session
BEGIN
  INSERT INTO session_ownership (session_id, tenant_id, principal_id)
  VALUES (
    NEW.id,
    COALESCE((SELECT tenant_id FROM session_ownership WHERE session_id=NEW.parent_id), 'local'),
    COALESCE((SELECT principal_id FROM session_ownership WHERE session_id=NEW.parent_id), 'local-user')
  );
END;

CREATE INDEX session_ownership_principal_idx
  ON session_ownership(tenant_id, principal_id, session_id);
