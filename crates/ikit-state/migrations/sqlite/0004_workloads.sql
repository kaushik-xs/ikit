-- Per-env workload numbering for wl-{env}-{nnn}.
CREATE TABLE env_counters (
    env      TEXT PRIMARY KEY,
    next_seq INTEGER NOT NULL
) STRICT;

CREATE TABLE workloads (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    env        TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    target_id  INTEGER NOT NULL REFERENCES targets (id) ON DELETE RESTRICT,
    status     TEXT    NOT NULL DEFAULT 'draft',
    created_at TEXT    NOT NULL,
    updated_at TEXT    NOT NULL,
    UNIQUE (env, seq)
) STRICT;

CREATE TABLE service_groups (
    id          INTEGER PRIMARY KEY,
    workload_id INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL CHECK (kind IN ('app', 'platform', 'data')),
    created_at  TEXT    NOT NULL,
    UNIQUE (workload_id, kind)
) STRICT;

-- RESTRICT on the registry references keeps services and versions in use from being deleted.
CREATE TABLE services (
    id                  INTEGER PRIMARY KEY,
    workload_id         INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    group_id            INTEGER NOT NULL REFERENCES service_groups (id) ON DELETE CASCADE,
    name                TEXT    NOT NULL,
    registry_service_id INTEGER NOT NULL REFERENCES registry_services (id) ON DELETE RESTRICT,
    registry_version_id INTEGER NOT NULL REFERENCES registry_versions (id) ON DELETE RESTRICT,
    version_constraint  TEXT    NOT NULL,
    env_json            TEXT    NOT NULL DEFAULT '{}',
    secret_env_json     TEXT    NOT NULL DEFAULT '[]',
    created_at          TEXT    NOT NULL,
    updated_at          TEXT    NOT NULL,
    UNIQUE (workload_id, name)
) STRICT;

CREATE INDEX services_registry_service ON services (registry_service_id);
CREATE INDEX services_registry_version ON services (registry_version_id);
