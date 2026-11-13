-- The org's script library. The body (the whole file, header included) is stored here so it
-- travels with the state; importing the file again updates the row in place. spec_json holds
-- the declared params, CSV input, args and timeout (ikit_core::script::ScriptSpec).
CREATE TABLE scripts (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    description TEXT    NOT NULL DEFAULT '',
    interpreter TEXT    NOT NULL,
    runs_on     TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    checksum    TEXT    NOT NULL,
    spec_json   TEXT    NOT NULL DEFAULT '{}',
    source      TEXT,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
) STRICT;

-- Every execution, on demand or by a hook. Workloads are kept by name: a run can span two
-- workloads, and history should outlive them. params_json never holds secret values.
CREATE TABLE script_runs (
    id            INTEGER PRIMARY KEY,
    script_id     INTEGER NOT NULL REFERENCES scripts (id) ON DELETE CASCADE,
    run_id        INTEGER REFERENCES runs (id) ON DELETE SET NULL,
    workload      TEXT    NOT NULL,
    from_workload TEXT,
    hook          TEXT,
    checksum      TEXT    NOT NULL,
    params_json   TEXT    NOT NULL DEFAULT '{}',
    result        TEXT    NOT NULL,
    exit_code     INTEGER,
    error         TEXT,
    output_tail   TEXT,
    ran_at        TEXT    NOT NULL
) STRICT;

CREATE INDEX script_runs_script ON script_runs (script_id, id);
