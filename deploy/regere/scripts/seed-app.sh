#!/usr/bin/env bash
# ikit:
#   name: seed-app
#   description: Register the platform services in launchpad, and seed authrs tenants and the core tenant (safe to re-run)
#   runs_on: target
#   params:
#     TENANT: service.dwigital-shell.env.TENANT
#     TENANT_NAME: { value: "Siemens Energy" }
#     ADMIN_EMAIL: service.authrs.env.SEED_ADMIN_EMAIL
#     ADMIN_PASSWORD: service.authrs.env.SEED_ADMIN_PASSWORD
#     CORE_DB_USER: service.core.env.DB_USER
#     CORE_DB_PASSWORD: service.core.env.DB_PASSWORD
#     CORE_DB_NAME: service.core.env.DB_NAME
#     PG_HOST: service.postgres.host
#     LAUNCHPAD_PUBLIC_URL: service.launchpad.env.PUBLIC_URL
#     AUTHRS_URL: service.authrs.internal_url
#     CORE_URL: service.core.internal_url
#     WORKFLOW_URL: service.workflow-engine.internal_url
#     PLATFORM_GW_URL: service.launchpad.env.PLATFORM_GW_URL
#     DECISION_HUB_URL: service.decision-hub.internal_url
#     SOCKUDO_URL: { value: "" }
#   timeout: 600
#
# A post-apply hook on the app workload. The platform is a separate workload reached only
# through platform-gw (launchpad's PLATFORM_GW_URL, called with PLATFORM_GW_API_KEY), so
# its services are registered as <gateway>/<slug>, the gateway slugs seed-platform-gw
# creates. common-services keeps its /common-services path prefix (strip_prefix off in the
# gateway), so its row is the gateway root. Sockudo needs WebSockets, which the gateway
# doesn't carry: SOCKUDO_URL is its own address. decision-hub runs in the app workload.
#   ikit hook add seed --script seed-app --phase post-apply \
#     --param TENANT_NAME="Siemens Energy" --param SOCKUDO_URL=https://stg.regere.ai/rt
#
# Every seed upserts, so re-running it is safe. Launchpad's upsert also rewrites each
# service's base_url, which repoints a database restored from another stack.
set -euo pipefail
: "${SOCKUDO_URL:?pass --param SOCKUDO_URL=<sockudo address reachable from launchpad>}"
GW=${PLATFORM_GW_URL%/}
dc() { docker compose -p "$IKIT_WORKLOAD" -f compose.yaml -f secrets.compose.yaml "$@"; }

services=$(cat <<JSON
[
  {"slug":"authrs","name":"Auth","base_url":"$AUTHRS_URL","service_type":"other","docker_image":"kaushik91/authrs","tag":"latest"},
  {"slug":"","name":"Launchpad","base_url":"${LAUNCHPAD_PUBLIC_URL%/}","service_type":"other","docker_image":"kaushik91/launchpad","tag":"latest"},
  {"slug":"core","name":"Core","base_url":"$CORE_URL","service_type":"framework","docker_image":"kaushik91/service-kit","tag":"latest"},
  {"slug":"workflow","name":"Workflow Engine","base_url":"$WORKFLOW_URL","service_type":"other","docker_image":"kaushik91/workflow-engine","tag":"latest"},
  {"slug":"load-optimizer","name":"Load Optimizer","base_url":"$GW/load-optimizer","service_type":"platform","docker_image":"ghcr.io/regere-ai/load-optimizer","tag":"latest"},
  {"slug":"simulation","name":"Simulation","base_url":"$GW/simulation","service_type":"platform","docker_image":"ghcr.io/regere-ai/simulation-service-v2","tag":"latest"},
  {"slug":"yard-recom","name":"Yard-Recommendation","base_url":"$GW/yard-recom","service_type":"platform","docker_image":"ghcr.io/regere-ai/yard-recom","tag":"latest"},
  {"slug":"message-flow","name":"Message-Flow","base_url":"$GW/message-flow","service_type":"platform","docker_image":"ghcr.io/regere-ai/message-flow","tag":"latest"},
  {"slug":"decision-hub","name":"Decision-Hub","base_url":"$DECISION_HUB_URL","service_type":"platform","docker_image":"ghcr.io/regere-ai/decision-hub","tag":"latest"},
  {"slug":"socket","name":"Sockudo","base_url":"${SOCKUDO_URL%/}","service_type":"platform","docker_image":"godson326/sockudo","tag":"latest"},
  {"slug":"common-service","name":"Common Service","base_url":"$GW","service_type":"platform","docker_image":"ghcr.io/regere-ai/common-services","tag":"latest"}
]
JSON
)
echo "== launchpad: service registry"
SEED_SERVICES="$services" dc exec -T -e SEED_SERVICES launchpad /app/seed

seed_tenant() { # id name role
  echo "== authrs: tenant $1"
  SEED_TENANT_ID="$1" SEED_TENANT_NAME="$2" SEED_ROLE_NAME="$3" \
    SEED_ADMIN_EMAIL="$ADMIN_EMAIL" SEED_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
    dc exec -T -e SEED_TENANT_ID -e SEED_TENANT_NAME -e SEED_ROLE_NAME \
      -e SEED_ADMIN_EMAIL -e SEED_ADMIN_PASSWORD authrs /app/seed
}
seed_tenant builder Builder "Builder Developer Admin"
seed_tenant "$TENANT" "$TENANT_NAME" "Super Admin"

echo "== core: tenant $TENANT (rls)"
SEED_TENANT_ID="$TENANT" SEED_TENANT_STRATEGY=rls SEED_TENANT_COMMENT="$TENANT_NAME" \
  SEED_TENANT_DATABASE_URL="postgresql://$CORE_DB_USER:$CORE_DB_PASSWORD@$PG_HOST:5432/$CORE_DB_NAME" \
  dc exec -T -e SEED_TENANT_ID -e SEED_TENANT_STRATEGY -e SEED_TENANT_COMMENT \
    -e SEED_TENANT_DATABASE_URL core /app/seed
echo "seeded: launchpad services, tenants builder and $TENANT"
