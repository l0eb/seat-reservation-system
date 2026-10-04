#!/usr/bin/env bash
# Build the arm64 api image, push it to ECR, and roll it out to the
# replicas one at a time: each old container drains (fails /readyz) before
# it stops, so Caddy routes around it and nothing is dropped.
#
#   deploy/push.sh              build, push, roll out the replicas
#   deploy/push.sh --edge       same, and (re)configure the edge first
#   deploy/push.sh --no-build   roll out the image already in ECR
#   deploy/push.sh --build-only build and push, don't roll out
set -euo pipefail
cd "$(dirname "$0")/.."
. deploy/config.sh

BUILD=1 EDGE=0 ROLL=1
for arg in "$@"; do
  case $arg in
    --no-build) BUILD=0 ;;
    --build-only) ROLL=0 ;;
    --edge) EDGE=1 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

if [ $BUILD = 1 ]; then
  log "building and pushing $IMAGE:latest (linux/arm64)"
  aws ecr get-login-password --region "$REGION" | docker login --username AWS --password-stdin "${IMAGE%%/*}" >/dev/null
  # One plain image per push: without --provenance=false buildx pushes an
  # index plus an attestation, three ECR entries that the keep-last-5
  # lifecycle rule counts separately and could expire from under :latest.
  docker buildx build --platform linux/arm64 --provenance=false --sbom=false \
    -t "$IMAGE:latest" --push --quiet . >/dev/null
fi
[ $ROLL = 1 ] || exit 0

mapfile -t APIS < <(instances api)
mapfile -t EDGES < <(instances edge)
if [ ${#APIS[@]} -ne $REPLICAS ] || [ ${#EDGES[@]} -ne 1 ]; then
  echo "expected 1 edge and $REPLICAS api instances, found ${#EDGES[@]} and ${#APIS[@]}: run deploy/up.sh" >&2
  exit 1
fi
STOPPED=$(aws ec2 describe-instances --region "$REGION" --filters Name=tag:Project,Values=$PROJECT \
  Name=instance-state-name,Values=stopping,stopped --query 'Reservations[].Instances[].Tags[?Key==`Name`]|[].Value' --output text)
[ -z "$STOPPED" ] || { echo "stopped, start them first: $STOPPED" >&2; exit 1; }
read -r EDGE_ID EDGE_PRIVATE _ _ <<<"${EDGES[0]}"
SITE=$(site)
PRIVATES=()
for line in "${APIS[@]}"; do read -r _ private _ _ <<<"$line"; PRIVATES+=("$private"); done

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if [ $EDGE = 1 ]; then
  log "edge: https://$SITE -> ${PRIVATES[*]}"
  cat > "$TMP/edge.env" <<EOF
PRIVATE_IP=$EDGE_PRIVATE
SITE=$SITE
UPSTREAMS="$(printf '%s:8080 ' "${PRIVATES[@]}" | sed 's/ $//')"
EOF
  run_on "$EDGE_ID" edge.sh host.env="$TMP/edge.env" Caddyfile=caddy/Caddyfile edge.sh=deploy/host/edge.sh
fi

for line in "${APIS[@]}"; do
  read -r id private _ name <<<"$line"
  replica=${name#"$PROJECT-"}
  peers=$(for p in "${PRIVATES[@]}"; do [ "$p" = "$private" ] || printf 'http://%s:8080,' "$p"; done | sed 's/,$//')
  log "$replica: rolling out ($private)"
  cat > "$TMP/$replica.env" <<EOF
REGION=$REGION
IMAGE=$IMAGE
PARAMS=$PARAMS
LOG_GROUP=$LOG_GROUP
REPLICA_ID=$replica
PRIVATE_IP=$private
PEERS=$peers
CACHE_URL=redis://$EDGE_PRIVATE:6379
EOF
  run_on "$id" api.sh host.env="$TMP/$replica.env" api.sh=deploy/host/api.sh
  # Let the edge's health check (every 2s) see this one back before the
  # next one goes down.
  sleep 4
done
log "rolled out to $REPLICAS replicas: https://$SITE"
