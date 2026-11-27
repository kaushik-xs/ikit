#!/bin/sh
# ikit:
#   name: seed-platform-gw
#   description: Register the platform services in platform-gw and issue the apps' API key (safe to re-run)
#   runs_on: target
#   params:
#     PGUSER: { from: service.postgres.env.POSTGRES_USER, default: postgres }
#     GW_DB_NAME: service.platform-gw.env.DB_NAME
#     KEY_PEPPER: service.platform-gw.env.GW_SECURITY__KEY_PEPPER
#     KEY_SEED: service.platform-gw.env.APP_KEY_SEED
#     TENANT: { value: siemens_energy }
#     ALLOWED_IPS: { from: service.platform-gw.env.APP_ALLOWED_IPS, required: false }
#     MESSAGE_FLOW_URL: { from: service.message-flow.internal_url, required: false }
#     LOAD_OPTIMIZER_URL: { from: service.load-optimizer.internal_url, required: false }
#     SIMULATION_URL: { from: service.simulation-service.internal_url, required: false }
#     COMMON_SERVICES_URL: { from: service.common-services.internal_url, required: false }
#     YARD_URL: { from: service.yard-recom.internal_url, required: false }
#   timeout: 300
#
# A post-apply hook on the platform workload, which runs platform-gw:
#   ikit hook add gw-seed --script seed-platform-gw --phase post-apply --param TENANT=siemens_energy
# with platform-gw's APP_ALLOWED_IPS set to the app hosts' egress addresses.
#
# What it writes into platform-gw's database (the same rows siemens-uat-platform's console holds):
#   services          one row per platform service the workload runs, upstream = its internal
#                     URL in this workload (http://load-optimizer:3050, ...); a service the
#                     workload does not run is left alone. Slugs, timeouts and strip_prefix
#                     match siemens-uat-platform.
#   api_keys          one key for the apps, tenant TENANT, no expiry, usable only from
#                     ALLOWED_IPS (comma/space separated IPs or CIDRs; required, so the key
#                     never ends up usable from anywhere). It is derived from the
#                     generated secret APP_KEY_SEED (pgw_<hex 0-8>_<hex 8-48>), so every run
#                     yields the same key and nothing has to be stored or printed. The secret
#                     part is kept only as HMAC-SHA256 under GW_SECURITY__KEY_PEPPER, as the
#                     gateway itself does (crates/gw-auth/src/api_key.rs).
#   api_key_services  the key may call every seeded service, every method.
# Then it NOTIFYs gw_changed so a running gateway reloads without a restart.
#
# Rows added in the console (other services, keys, AI providers) are never touched. To read
# the key for an app: ikit svc secret show platform-gw APP_KEY_SEED --yes, then
# pgw_<first 8 chars>_<next 40 chars>; env-up.sh does this for the app workload.
set -eu
dc() { docker compose -p "$IKIT_WORKLOAD" -f compose.yaml -f secrets.compose.yaml "$@"; }

case "$KEY_SEED" in
  *[!0-9a-f]*|"") echo "APP_KEY_SEED must be hex (generate: hex32)" >&2; exit 1 ;;
esac
[ "${#KEY_SEED}" -ge 48 ] || { echo "APP_KEY_SEED is shorter than 48 hex chars" >&2; exit 1; }
PREFIX=$(printf %s "$KEY_SEED" | cut -c1-8)
SECRET=$(printf %s "$KEY_SEED" | cut -c9-48)
PEPPER_HEX=$(printf %s "$KEY_PEPPER" | base64 -d | od -An -v -tx1 | tr -d ' \n')
[ "${#PEPPER_HEX}" -eq 64 ] || { echo "GW_SECURITY__KEY_PEPPER is not 32 bytes of base64" >&2; exit 1; }
HASH_HEX=$(printf %s "$SECRET" | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$PEPPER_HEX" -r | cut -d' ' -f1)
[ "${#HASH_HEX}" -eq 64 ] || { echo "computing the key hash failed (is openssl installed on the target?)" >&2; exit 1; }
KEY_NAME="$IKIT_WORKLOAD apps - $TENANT"
# "10.0.0.0/8, 203.0.113.7" -> "{10.0.0.0/8,203.0.113.7}"; postgres's cidr type validates each.
IPS=$(printf %s "${ALLOWED_IPS:-}" | tr ', \t\n' '\n\n\n\n' | grep . | paste -sd, -) || true
[ -n "$IPS" ] || { echo "APP_ALLOWED_IPS is empty: set it on platform-gw to the app hosts' egress IPs" >&2; exit 1; }

# slug|name|url|timeout_ms|strip_prefix, as in siemens-uat-platform's platform_gw.services.
rows=""
add() { [ -n "$3" ] && rows="$rows$1|$2|${3%/}|$4|$5
"; return 0; }
add message-flow    "Message Flow"        "${MESSAGE_FLOW_URL:-}"    30000  true
add load-optimizer  "Load Optimizer"      "${LOAD_OPTIMIZER_URL:-}"  120000 true
add simulation      "Simulation Service"  "${SIMULATION_URL:-}"      30000  true
add common-services "Common Services"     "${COMMON_SERVICES_URL:-}" 120000 false
add yard-recom      "Yard Recommendation" "${YARD_URL:-}"            30000  true
[ -n "$rows" ] || { echo "no platform services found in $IKIT_WORKLOAD" >&2; exit 1; }

psql_gw() { dc exec -T -e PGUSER="$PGUSER" "$@" postgres psql -X -q -v ON_ERROR_STOP=1 -d "$GW_DB_NAME"; }

# platform-gw creates its tables on start; wait for them (post-apply runs once it is up).
i=0
until echo "SELECT 1 FROM api_key_services LIMIT 0" | psql_gw >/dev/null 2>&1; do
  i=$((i + 1)); [ "$i" -le 60 ] || { echo "platform-gw tables did not appear in $GW_DB_NAME" >&2; exit 1; }
  sleep 2
done

echo "SELECT allowed_ips FROM api_keys LIMIT 0" | psql_gw >/dev/null 2>&1 \
  || { echo "platform-gw in $IKIT_WORKLOAD has no IP allowlists yet (needs Regere-AI/platform-gw#5)" >&2; exit 1; }

echo "== platform-gw: services"
printf %s "$rows" | while IFS='|' read -r slug name url timeout strip; do
  [ -n "$slug" ] || continue
  echo "  $slug -> $url"
  S_SLUG="$slug" S_NAME="$name" S_URL="$url" S_TIMEOUT="$timeout" S_STRIP="$strip" \
    psql_gw -e S_SLUG -e S_NAME -e S_URL -e S_TIMEOUT -e S_STRIP <<'SQL'
\getenv slug S_SLUG
\getenv name S_NAME
\getenv url S_URL
\getenv timeout S_TIMEOUT
\getenv strip S_STRIP
INSERT INTO services (id, slug, name, upstream_url, timeout_ms, strip_prefix, enabled)
VALUES (gen_random_uuid(), :'slug', :'name', :'url', :'timeout'::int, :'strip'::boolean, true)
ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name, upstream_url = EXCLUDED.upstream_url,
  timeout_ms = EXCLUDED.timeout_ms, strip_prefix = EXCLUDED.strip_prefix, enabled = true,
  updated_at = now()
WHERE (services.name, services.upstream_url, services.timeout_ms, services.strip_prefix, services.enabled)
  IS DISTINCT FROM (EXCLUDED.name, EXCLUDED.upstream_url, EXCLUDED.timeout_ms, EXCLUDED.strip_prefix, true);
SQL
done

echo "== platform-gw: API key pgw_${PREFIX}_… for tenant $TENANT, from $IPS"
SLUGS=$(printf %s "$rows" | cut -d'|' -f1 | grep . | paste -sd, -)
K_PREFIX="$PREFIX" K_HASH="$HASH_HEX" K_TENANT="$TENANT" K_NAME="$KEY_NAME" K_SLUGS="$SLUGS" K_IPS="{$IPS}" \
  psql_gw -e K_PREFIX -e K_HASH -e K_TENANT -e K_NAME -e K_SLUGS -e K_IPS <<'SQL'
\getenv prefix K_PREFIX
\getenv ips K_IPS
\getenv hash K_HASH
\getenv tenant K_TENANT
\getenv name K_NAME
\getenv slugs K_SLUGS
BEGIN;
INSERT INTO api_keys (id, tenant_id, name, prefix, secret_hash, status, expires_at, revoked_at, allowed_ips)
VALUES (gen_random_uuid(), :'tenant', :'name', :'prefix', decode(:'hash', 'hex'), 'active', NULL, NULL, :'ips'::cidr[])
ON CONFLICT (prefix) DO UPDATE SET tenant_id = EXCLUDED.tenant_id, name = EXCLUDED.name,
  secret_hash = EXCLUDED.secret_hash, status = 'active', expires_at = NULL, revoked_at = NULL,
  allowed_ips = EXCLUDED.allowed_ips;
INSERT INTO api_key_services (key_id, service_id, allowed_methods)
SELECT k.id, s.id, '{}' FROM api_keys k, services s
WHERE k.prefix = :'prefix' AND s.slug = ANY (string_to_array(:'slugs', ','))
ON CONFLICT (key_id, service_id) DO NOTHING;
SELECT FROM pg_notify('gw_changed', 'services');
SELECT FROM pg_notify('gw_changed', 'key:' || :'prefix');
COMMIT;
SQL
echo "seeded: $(printf %s "$SLUGS" | tr ',' ' '); key pgw_${PREFIX}_… (tenant $TENANT, from $IPS)"
