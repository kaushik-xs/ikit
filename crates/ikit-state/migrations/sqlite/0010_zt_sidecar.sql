-- Whether a linked workload runs a ZeroTier sidecar container that joins the network.
ALTER TABLE workload_networks ADD COLUMN sidecar INTEGER NOT NULL DEFAULT 0 CHECK (sidecar IN (0, 1));
