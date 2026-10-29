-- SQL hooks run against a workload service after each apply. `source` is a local .sql file
-- or a folder of them; the scripts are read fresh on every run.
CREATE TABLE hooks (
    id          INTEGER PRIMARY KEY,
    workload_id INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    phase       TEXT    NOT NULL,
    service     TEXT    NOT NULL,
    engine      TEXT    NOT NULL,
    database    TEXT,
    source      TEXT    NOT NULL,
    ord         INTEGER NOT NULL,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL,
    UNIQUE (workload_id, name)
) STRICT;

-- Mirror of scripts executed on targets. The target database's own `_ikit_hook_ledger` is
-- the source of truth for what has run there (it survives restores; this mirror does not).
CREATE TABLE hook_runs (
    id       INTEGER PRIMARY KEY,
    hook_id  INTEGER NOT NULL REFERENCES hooks (id) ON DELETE CASCADE,
    run_id   INTEGER REFERENCES runs (id) ON DELETE SET NULL,
    script   TEXT    NOT NULL,
    checksum TEXT    NOT NULL,
    result   TEXT    NOT NULL,
    error    TEXT,
    ran_at   TEXT    NOT NULL
) STRICT;

CREATE INDEX hook_runs_hook ON hook_runs (hook_id, id);
