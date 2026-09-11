-- Preview runtime overlays evolve independently from released core formats.
CREATE TABLE zuno_preview_schema (
  singleton integer PRIMARY KEY CHECK (singleton = 1),
  format integer NOT NULL,
  channel text NOT NULL CHECK (channel = 'enterprise-preview')
);
