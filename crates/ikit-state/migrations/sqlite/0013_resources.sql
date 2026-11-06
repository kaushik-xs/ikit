-- Places that provide one resource API (s3, sqs): AWS itself, or a compatible server at
-- options.endpoint. Credentials come from the referenced aws profile.
CREATE TABLE resource_backends (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    api          TEXT NOT NULL CHECK (api IN ('s3', 'sqs')),
    provider     TEXT NOT NULL CHECK (provider IN ('aws')),
    profile      TEXT,
    options_json TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL
) STRICT;

-- Which backend serves an API for the workloads of an env.
CREATE TABLE resource_bindings (
    env        TEXT    NOT NULL,
    api        TEXT    NOT NULL,
    backend_id INTEGER NOT NULL REFERENCES resource_backends (id) ON DELETE RESTRICT,
    PRIMARY KEY (env, api)
) STRICT;

-- Resources of workload services. backend_id is NULL until `ikit res bind` or the first apply
-- picks one; once ready, the resource stays on that backend. A service removed from the
-- workload leaves its resources orphaned rather than deleting them.
CREATE TABLE workload_resources (
    id           INTEGER PRIMARY KEY,
    workload_id  INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    service      TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    api          TEXT    NOT NULL CHECK (api IN ('s3', 'sqs')),
    backend_id   INTEGER REFERENCES resource_backends (id) ON DELETE RESTRICT,
    external_id  TEXT    NOT NULL,
    outputs_json TEXT    NOT NULL DEFAULT '{}',
    origin       TEXT    NOT NULL CHECK (origin IN ('created', 'adopted')),
    status       TEXT    NOT NULL CHECK (status IN ('pending', 'ready')),
    orphaned     INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    UNIQUE (workload_id, service, name)
) STRICT;

CREATE INDEX workload_resources_backend ON workload_resources (backend_id);
