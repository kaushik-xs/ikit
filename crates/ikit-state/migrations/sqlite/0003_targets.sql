-- Targets: the VM or Kubernetes cluster a workload runs on.
-- Provisioning columns (provider, profile, tofu state) arrive with `target create`.
CREATE TABLE targets (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    kind            TEXT NOT NULL CHECK (kind IN ('vm', 'k8s')),
    origin          TEXT NOT NULL CHECK (origin IN ('created', 'attached')),
    connection_json TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'unknown',
    last_checked_at TEXT,
    last_check_json TEXT NOT NULL DEFAULT '[]',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
) STRICT;
