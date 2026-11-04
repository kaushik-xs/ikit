-- This org's reverse proxy on a target's host: {kind, email?, ca, version?}. NULL until
-- `ikit target edge install` installs, adopts or joins one.
ALTER TABLE targets ADD COLUMN edge_json TEXT;

-- Domains (and path prefixes) routed through the target's proxy to a service port, which
-- is published on the host's loopback address at upstream_port. path is '' for the whole
-- host, so the unique key works.
CREATE TABLE routes (
    id            INTEGER PRIMARY KEY,
    workload_id   INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    service       TEXT    NOT NULL,
    port          INTEGER NOT NULL,
    host          TEXT    NOT NULL,
    path          TEXT    NOT NULL DEFAULT '',
    tls           TEXT    NOT NULL CHECK (tls IN ('acme', 'internal', 'off')),
    upstream_port INTEGER NOT NULL,
    origin        TEXT    NOT NULL CHECK (origin IN ('auto', 'manual')),
    created_at    TEXT    NOT NULL,
    UNIQUE (workload_id, host, path)
) STRICT;

CREATE INDEX routes_host ON routes (host);

-- Domains that redirect elsewhere, keeping the request path.
CREATE TABLE redirects (
    id          INTEGER PRIMARY KEY,
    workload_id INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    host        TEXT    NOT NULL,
    to_url      TEXT    NOT NULL,
    permanent   INTEGER NOT NULL DEFAULT 1,
    tls         TEXT    NOT NULL CHECK (tls IN ('acme', 'internal', 'off')),
    created_at  TEXT    NOT NULL,
    UNIQUE (workload_id, host)
) STRICT;

-- Workloads whose routes follow their services: {groups, port_name, excluded}.
CREATE TABLE auto_routes (
    workload_id INTEGER PRIMARY KEY REFERENCES workloads (id) ON DELETE CASCADE,
    config_json TEXT NOT NULL,
    updated_at  TEXT NOT NULL
) STRICT;
