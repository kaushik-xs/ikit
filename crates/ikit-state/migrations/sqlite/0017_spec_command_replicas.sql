-- Registry specs (registry_versions.spec_json) may now carry `command` and `replicas`.
-- No table changes: this migration only raises the schema version, so an older ikit, whose
-- ServiceSpec rejects unknown fields, refuses a remote snapshot with "upgrade ikit" instead
-- of failing to read the registry.
UPDATE registry_versions SET spec_json = spec_json WHERE 0;
