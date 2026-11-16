#!/usr/bin/env bash
# Local stand-ins for S3 and SQS, so a mock run of env-up.sh creates its buckets and the
# simulation queue through ikit without touching AWS. RustFS answers S3, ElasticMQ answers
# SQS; both listen on the Docker bridge address, which the host (where ikit runs) and every
# container can reach.
#
#   deploy/regere/mock-backends.sh up     # start them, add backends mock-s3/mock-sqs, bind dev
#   deploy/regere/mock-backends.sh down   # stop them and bind dev back to aws-s3/aws-sqs
#
# Only for a throwaway ikit home (IKIT_HOME): it rebinds the env for every workload in it.
# ElasticMQ keeps queues in memory: after it restarts, `ikit apply` creates them again.
set -euo pipefail

IKIT=${IKIT:-ikit}
ENV_NAME=${ENV_NAME:-dev}
BRIDGE=$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}')
S3_PORT=${S3_PORT:-19000} SQS_PORT=${SQS_PORT:-19324}
KEY=mockadmin SECRET=mockadmin-secret

case "${1:-}" in
  up)
    docker run -d --name ikit-mock-s3 --restart unless-stopped -p "$BRIDGE:$S3_PORT:9000" \
      -e RUSTFS_ACCESS_KEY=$KEY -e RUSTFS_SECRET_KEY=$SECRET \
      rustfs/rustfs:1.0.0-beta.8 >/dev/null
    docker run -d --name ikit-mock-sqs --restart unless-stopped -p "$BRIDGE:$SQS_PORT:9324" \
      softwaremill/elasticmq-native:1.6.11 >/dev/null
    IKIT_SECRET_SECRET_ACCESS_KEY=$SECRET $IKIT --no-input profile add aws mock \
      --set region=us-east-1 --set access_key_id=$KEY
    $IKIT --no-input res backend add mock-s3 --api s3 --profile mock --endpoint "http://$BRIDGE:$S3_PORT"
    $IKIT --no-input res backend add mock-sqs --api sqs --profile mock --endpoint "http://$BRIDGE:$SQS_PORT"
    $IKIT --no-input res backend bind "$ENV_NAME" mock-s3 mock-sqs
    echo "S3 at http://$BRIDGE:$S3_PORT, SQS at http://$BRIDGE:$SQS_PORT; $ENV_NAME now uses them."
    ;;
  down)
    $IKIT --no-input res backend bind "$ENV_NAME" aws-s3 aws-sqs || true
    docker rm -f ikit-mock-s3 ikit-mock-sqs >/dev/null 2>&1 || true
    ;;
  *) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
