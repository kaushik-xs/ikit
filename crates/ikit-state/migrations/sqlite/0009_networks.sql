-- ZeroTier networks ikit created or attached. The Central API token comes from the
-- referenced zerotier profile and the keychain.
CREATE TABLE networks (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    network_id TEXT NOT NULL UNIQUE,
    profile    TEXT NOT NULL,
    origin     TEXT NOT NULL CHECK (origin IN ('created', 'attached')),
    cidr       TEXT,
    created_at TEXT NOT NULL
) STRICT;

-- The network a workload is linked to: at most one per workload, shared by many.
CREATE TABLE workload_networks (
    workload_id INTEGER PRIMARY KEY REFERENCES workloads (id) ON DELETE CASCADE,
    network_id  INTEGER NOT NULL REFERENCES networks (id) ON DELETE RESTRICT
) STRICT;
