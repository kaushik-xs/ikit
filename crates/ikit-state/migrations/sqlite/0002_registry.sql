-- Service registry: deployable services and their versions.
-- `source` is validated in Rust (not a CHECK) so new sources need no table rebuild.
CREATE TABLE registry_services (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    display_name TEXT,
    description  TEXT,
    category     TEXT NOT NULL CHECK (category IN ('app', 'platform', 'data')),
    tags_json    TEXT NOT NULL DEFAULT '[]',
    owner        TEXT,
    homepage     TEXT,
    source       TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
) STRICT;

-- spec_json holds ports, env schema, volumes, healthcheck, resources and depends_on.
CREATE TABLE registry_versions (
    id            INTEGER PRIMARY KEY,
    service_id    INTEGER NOT NULL REFERENCES registry_services (id) ON DELETE CASCADE,
    version       TEXT    NOT NULL,
    image         TEXT    NOT NULL,
    digest        TEXT,
    spec_json     TEXT    NOT NULL DEFAULT '{}',
    release_notes TEXT,
    deprecated    INTEGER NOT NULL DEFAULT 0 CHECK (deprecated IN (0, 1)),
    created_at    TEXT    NOT NULL,
    UNIQUE (service_id, version)
) STRICT;
