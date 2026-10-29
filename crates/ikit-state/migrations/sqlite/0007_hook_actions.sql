-- Shell and HTTP hooks. The full action of every hook is stored in spec_json; for SQL hooks
-- the 0006 columns (service, engine, database, source) are still filled so older rows, which
-- have spec_json = '{}', keep working. For other kinds those columns hold ''.
ALTER TABLE hooks ADD COLUMN spec_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE hooks ADD COLUMN fail_on_error INTEGER NOT NULL DEFAULT 1 CHECK (fail_on_error IN (0, 1));
