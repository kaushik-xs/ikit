-- Docker networks that exist outside a workload (created by hand or by another Compose
-- project) and that one of its service groups joins as `external: true`.
CREATE TABLE workload_docker_networks (
    workload_id INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    network     TEXT    NOT NULL,
    group_kind  TEXT    NOT NULL CHECK (group_kind IN ('app', 'platform', 'data')),
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (workload_id, network, group_kind)
) STRICT;
