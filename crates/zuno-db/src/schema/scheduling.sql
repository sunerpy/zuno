-- Format 13 stores scheduling eligibility on the existing execution row.
-- NULL preserves legacy rows; the migration repairs only structured no-progress pauses.
ALTER TABLE session_execution_state ADD COLUMN scheduling text
  CHECK (scheduling IS NULL OR (json_valid(scheduling) AND json_type(scheduling) = 'object'));
