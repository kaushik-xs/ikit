-- Provider profiles. Secret values live in the OS keychain; only their keys are recorded here.
CREATE TABLE profiles (
    id               INTEGER PRIMARY KEY,
    kind             TEXT    NOT NULL CHECK (kind IN ('aws', 'azure', 'gcp', 'zerotier', 'k8s')),
    name             TEXT    NOT NULL,
    config_json      TEXT    NOT NULL DEFAULT '{}',
    secret_keys_json TEXT    NOT NULL DEFAULT '[]',
    created_at       TEXT    NOT NULL,
    updated_at       TEXT    NOT NULL,
    UNIQUE (kind, name)
) STRICT;
