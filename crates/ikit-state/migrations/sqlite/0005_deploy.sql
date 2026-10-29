-- Host ports a service publishes on its target: [{bind?, host, container, udp?}].
ALTER TABLE services ADD COLUMN publish_json TEXT NOT NULL DEFAULT '[]';

-- Every apply and destroy, with the plan it carried out and the compose file it applied
-- (never secret values).
CREATE TABLE runs (
    id           INTEGER PRIMARY KEY,
    workload_id  INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    action       TEXT    NOT NULL,
    actor        TEXT    NOT NULL,
    plan_json    TEXT    NOT NULL DEFAULT '{}',
    compose_yaml TEXT,
    result       TEXT    NOT NULL DEFAULT 'running',
    error        TEXT,
    started_at   TEXT    NOT NULL,
    finished_at  TEXT
) STRICT;

CREATE INDEX runs_workload ON runs (workload_id, id);

-- One apply or destroy at a time per workload. Stale locks (crashed runs) can be taken over.
CREATE TABLE locks (
    workload_id INTEGER PRIMARY KEY REFERENCES workloads (id) ON DELETE CASCADE,
    holder      TEXT NOT NULL,
    acquired_at TEXT NOT NULL
) STRICT;

-- Image registry credentials: the password lives in the keychain.
CREATE TABLE registry_logins (
    host       TEXT PRIMARY KEY,
    username   TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;
