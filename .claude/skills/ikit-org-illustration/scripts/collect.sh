#!/usr/bin/env bash
# Print everything the illustration needs about one ikit org, read straight from its state.
# Usage: collect.sh <org>   (honours $IKIT_HOME, default ~/.ikit)
# Never prints secret values: only secret env *names* are stored in state.db.
set -euo pipefail

org="${1:?usage: collect.sh <org>}"
home="${IKIT_HOME:-$HOME/.ikit}"
dir="$home/orgs/$org"
db="$dir/state.db"
[ -f "$db" ] || { echo "no state.db for org '$org' at $db" >&2; exit 1; }

q() { sqlite3 -readonly -header -separator ' | ' "$db" "$1"; }
section() { printf '\n## %s\n' "$1"; }

section "config.yaml"
cat "$dir/config.yaml"

section "registry"
q "select s.name, s.category, group_concat(v.version, ', ') as versions
   from registry_services s left join registry_versions v on v.service_id = s.id
   group by s.id order by s.category, s.name"

section "registry logins"
q "select host, username from registry_logins"

section "targets"
q "select name, kind, origin, connection_json, status, last_checked_at, last_check_json from targets"

section "profiles"
q "select kind, name from profiles"

section "storages"
q "select name, provider, profile from storages"

section "networks (ZeroTier)"
q "select name, profile, origin, cidr from networks"

section "workloads"
q "select w.name, w.env, w.status, t.name as target, w.updated_at
   from workloads w join targets t on t.id = w.target_id order by w.name"

section "services"
q "select w.name as workload, g.kind as grp, s.name, rs.name as registry_service,
          s.version_constraint, rv.version as resolved, rv.image,
          s.publish_json as ports, s.secret_env_json as secret_names
   from services s
   join workloads w on w.id = s.workload_id
   join service_groups g on g.id = s.group_id
   join registry_services rs on rs.id = s.registry_service_id
   join registry_versions rv on rv.id = s.registry_version_id
   order by w.name, g.kind, s.name"

section "hooks"
q "select w.name as workload, h.name, h.kind, h.phase, h.service from hooks h
   join workloads w on w.id = h.workload_id order by w.name, h.ord"

section "workload networks"
q "select w.name as workload, n.name as zerotier_network, wn.sidecar from workload_networks wn
   join workloads w on w.id = wn.workload_id join networks n on n.id = wn.network_id"
q "select w.name as workload, d.network as external_docker_network, d.group_kind from workload_docker_networks d
   join workloads w on w.id = d.workload_id"

section "runs"
q "select w.name as workload, r.id, r.action, r.result, r.started_at from runs r
   join workloads w on w.id = r.workload_id order by w.name, r.id"

section "locks"
q "select * from locks"

section "backups"
q "select count(*) as backups from backups"

# Start order and Compose networks come from the generated compose file (local targets only).
for compose in "$dir"/deploy/*/compose.yaml; do
  [ -f "$compose" ] || continue
  section "compose: $(basename "$(dirname "$compose")")"
  awk '
    /^services:/ { in_s = 1; next }
    /^[a-z]/     { in_s = 0 }
    in_s && /^  [A-Za-z0-9_.-]+:$/ { svc = $1; sub(":", "", svc); next }
    in_s && /^    depends_on:/ { mode = "dep"; next }
    in_s && /^    networks:/   { mode = "net"; next }
    in_s && /^    ports:/      { mode = "port"; next }
    in_s && /^    [a-z_]+:/    { mode = "" }
    mode == "dep"  && /^      [A-Za-z0-9_.-]+:/ { d = $1; sub(":", "", d); print svc " depends_on " d }
    mode == "net"  && /^      [A-Za-z0-9_.-]+:/ { n = $1; sub(":", "", n); print svc " network " n }
    mode == "port" && /^    - /                 { print svc " port " $2 }
  ' "$compose"
done
