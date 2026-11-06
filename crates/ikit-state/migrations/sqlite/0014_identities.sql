-- IAM users ikit created for workload services on backends with per-service keys, one per
-- service and provisioning profile ('' for the environment's credentials). The secret key
-- lives in the org's secret store; covers_json lists the resources its policy allows.
CREATE TABLE service_identities (
    id               INTEGER PRIMARY KEY,
    workload_id      INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    service          TEXT    NOT NULL,
    profile          TEXT    NOT NULL,
    account          TEXT,
    user_name        TEXT    NOT NULL,
    user_arn         TEXT,
    access_key_id    TEXT,
    retiring_key_id  TEXT,
    covers_json      TEXT    NOT NULL DEFAULT '[]',
    status           TEXT    NOT NULL CHECK (status IN ('pending', 'ready')),
    orphaned         INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT    NOT NULL,
    rotated_at       TEXT,
    UNIQUE (workload_id, service, profile)
) STRICT;
