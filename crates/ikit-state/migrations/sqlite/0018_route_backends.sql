-- What a route serves: NULL for a service port (upstream_port), else
-- {"kind": "static", "source": "/abs/dir", "spa": bool} or {"kind": "address", "url": "..."}.
-- Static and address routes have service '' and port and upstream_port 0.
ALTER TABLE routes ADD COLUMN backend_json TEXT;

-- Extra proxy directives written into the route's block as they are.
ALTER TABLE routes ADD COLUMN snippet TEXT;
