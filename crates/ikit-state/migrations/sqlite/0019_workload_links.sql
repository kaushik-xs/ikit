-- A consumer workload reaches a provider workload's app and platform services by
-- `service:` references, over a Docker network the provider offers (`<provider>-link`).
CREATE TABLE workload_links (
    consumer_id INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    provider_id INTEGER NOT NULL REFERENCES workloads (id) ON DELETE CASCADE,
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (consumer_id, provider_id),
    CHECK (consumer_id <> provider_id)
) STRICT;

CREATE INDEX workload_links_provider ON workload_links (provider_id);
