#!/usr/bin/env bash
# Build client-ui-next for one app workload, so the browser talks to that workload's
# launchpad instead of the URL baked into the published image.
#
#   deploy/regere/ui-local.sh --app wl-regere-dev-005                  # build, switch, apply
#   deploy/regere/ui-local.sh --app wl-regere-dev-005 --src ~/code/client-ui-next
#   deploy/regere/ui-local.sh --app wl-regere-dev-005 --undo           # back to the published image
#
# Why: Next.js compiles every NEXT_PUBLIC_* variable into the browser bundle at build time.
# The published ghcr.io/regere-ai/client-ui-next:staging-latest carries
# https://launchpad-headless-dev.regere.ai, so a local UI logs in to, reads and WRITES yotta
# dev whatever ikit sets at runtime. This is a workaround until client-ui-next reads its URLs
# at runtime; then one published image serves every workload and this script goes away.
#
# What it does:
#   1. Reads the workload's client-ui-next env from ikit (NEXT_PUBLIC_LAUNCHPAD_API_URL,
#      NEXT_PUBLIC_SIMULATION_API_BASE_URL, TENANT, NEXT_PUBLIC_COMPANY_LOGO; env-up.sh sets them).
#   2. Builds the client-ui-next checkout with those as build args, on the target's Docker,
#      tagged client-ui-next:local-<wl>-g<sha>. The other build args match the staging build.
#   3. Adds registry version 0.0.0-local.<wl>.g<sha> for that image. A pre-release, so
#      `latest` never resolves to it: no other workload picks it up.
#   4. Pins the workload's client-ui-next to it and applies. compose only pulls missing
#      images, so the local one is used. Older local versions for the workload are removed.
#
# The image exists only on the Docker engine that built it: run this where the workload's
# target is (`laptop` targets are the local engine).
set -euo pipefail

IKIT=${IKIT:-ikit}
HERE=$(cd "$(dirname "$0")" && pwd)
APP="" SRC="$HERE/../../../client-ui-next" UNDO=0 BANNER=""
SVC=client-ui-next

usage() { sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift ;;
    --src) SRC=$2; shift ;;
    --banner) BANNER=$2; shift ;;
    --undo) UNDO=1 ;;
    -h|--help) usage ;;
    *) echo "unknown option $1" >&2; usage 1 ;;
  esac
  shift
done
[ -n "$APP" ] || usage 1

ik() { echo "+ ikit $*" >&2; $IKIT --no-input "$@"; }
local_versions() {
  $IKIT --no-input registry show "$SVC" -o json |
    jq -r --arg p "0.0.0-local.$APP." '.versions[].version | select(startswith($p))'
}
drop_local_versions() { # all of this workload's local versions except $1
  for v in $(local_versions); do
    [ "$v" = "${1:-}" ] || ik registry version rm "$SVC" "$v" >/dev/null || true
  done
}

if [ "$UNDO" = 1 ]; then
  ik svc upgrade "$SVC" --wl "$APP" --to latest
  ik apply --wl "$APP" -y
  drop_local_versions
  echo "$APP's $SVC is back on the published image." >&2
  exit 0
fi

[ -f "$SRC/dockerfile" ] || { echo "no client-ui-next checkout at $SRC (use --src)" >&2; exit 1; }
SRC=$(cd "$SRC" && pwd)

svc_json=$($IKIT --no-input --wl "$APP" svc list -o json | jq --arg s "$SVC" '.[] | select(.name == $s)')
[ -n "$svc_json" ] || { echo "$APP has no $SVC service" >&2; exit 1; }
env_of() { jq -r --arg k "$1" '.env[$k] // empty' <<<"$svc_json"; }
BASE=$(jq -r '.version' <<<"$svc_json")
case "$BASE" in 0.0.0-local.*) BASE=$($IKIT --no-input registry show "$SVC" -o json |
  jq -r '[.versions[].version | select(contains("-") | not)][0]') ;; esac

LP_URL=$(env_of NEXT_PUBLIC_LAUNCHPAD_API_URL)
SIM_URL=$(env_of NEXT_PUBLIC_SIMULATION_API_BASE_URL)
[ -n "$LP_URL" ] || { echo "$APP's $SVC has no NEXT_PUBLIC_LAUNCHPAD_API_URL; run env-up.sh first" >&2; exit 1; }
[ -n "$SIM_URL" ] || SIM_URL="${LP_URL%/}/proxy/simulation"
TENANT=$(env_of TENANT)
LOGO=$(env_of NEXT_PUBLIC_COMPANY_LOGO)

sha=$(git -C "$SRC" rev-parse --short=10 HEAD)
git -C "$SRC" diff --quiet HEAD -- || sha="$sha-dirty$(date +%s)"
IMAGE="$SVC:local-$APP-g$sha"
VERSION="0.0.0-local.$APP.g$sha"

echo "Building $IMAGE from $SRC ($(git -C "$SRC" branch --show-current)) for $LP_URL" >&2
docker build -f "$SRC/dockerfile" -t "$IMAGE" \
  --build-arg APP_BASE_PATH=/ui/app1 \
  --build-arg "NEXT_PUBLIC_LAUNCHPAD_API_URL=$LP_URL" \
  --build-arg "NEXT_PUBLIC_SIMULATION_API_BASE_URL=$SIM_URL" \
  --build-arg LAUNCHPAD_API_URL=http://launchpad:3000/ \
  --build-arg NEXT_PUBLIC_CORE_PATH=/proxy/core \
  --build-arg NEXT_PUBLIC_COMMON_SERVICES_BASE_URL=/proxy/common-service/common-services \
  --build-arg LOG_LEVEL=debug \
  --build-arg "TENANT=$TENANT" \
  --build-arg "NEXT_PUBLIC_COMPANY_LOGO=$LOGO" \
  --build-arg NEXT_PUBLIC_EMAIL_PASSWORD_LOGIN_ENABLED=true \
  --build-arg NEXT_PUBLIC_USERNAME_PASSWORD_LOGIN_ENABLED=true \
  --build-arg NEXT_PUBLIC_MICROSOFT_LOGIN_ENABLED=true \
  --build-arg "NEXT_PUBLIC_ENV_BANNER_TEXT=${BANNER:-LOCAL $APP}" \
  "$SRC"

if ! local_versions | grep -qxF "$VERSION"; then
  ik registry version add "$SVC" "$VERSION" --image "$IMAGE" --from "$BASE" \
    --notes "Local build of client-ui-next@$sha for $APP (ui-local.sh); the image exists only on the engine that built it"
fi
ik svc upgrade "$SVC" --wl "$APP" --to "$VERSION"
ik apply --wl "$APP" -y
drop_local_versions "$VERSION"

cat >&2 <<EOT

$APP's $SVC now runs $IMAGE; the browser calls $LP_URL.
Back to the published image: deploy/regere/ui-local.sh --app $APP --undo
EOT
