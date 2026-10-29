-- Blob storage locations (S3 and S3-compatible such as MinIO, Azure Blob, GCS, or a local
-- folder). Credentials come from the referenced profile and the keychain.
CREATE TABLE storages (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    provider     TEXT NOT NULL CHECK (provider IN ('s3', 'azure', 'gcs', 'local')),
    bucket       TEXT NOT NULL,
    prefix       TEXT NOT NULL DEFAULT '',
    profile      TEXT,
    options_json TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL
) STRICT;

-- Database backups. Workload and service are kept by name so the catalog outlives them.
CREATE TABLE backups (
    id         INTEGER PRIMARY KEY,
    workload   TEXT    NOT NULL,
    service    TEXT    NOT NULL,
    engine     TEXT    NOT NULL,
    storage    TEXT    NOT NULL,
    object_key TEXT    NOT NULL,
    size       INTEGER NOT NULL,
    sha256     TEXT    NOT NULL,
    created_at TEXT    NOT NULL
) STRICT;

CREATE INDEX backups_service ON backups (workload, service, id);
