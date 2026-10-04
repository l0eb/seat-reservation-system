#!/bin/bash
# Runs on an api instance (via SSM, as root, from /opt/seat): pull the
# latest image and replace the running container. The old one gets
# SIGTERM, fails /readyz while it drains (SHUTDOWN_DRAIN_SECS), and Caddy
# routes around it.
#   /opt/seat/host.env  REGION IMAGE PARAMS LOG_GROUP REPLICA_ID PRIVATE_IP PEERS CACHE_URL
set -euo pipefail
cd /opt/seat
. ./host.env

# Secrets come from Parameter Store here, so they never travel in an SSM
# command (which keeps a history) or sit on disk.
param() { aws ssm get-parameter --region "$REGION" --name "$PARAMS/$1" --with-decryption --query Parameter.Value --output text; }

# (docker login warns that it stores the token unencrypted; show its output
# only if it fails. The token is ECR's, valid for 12 hours.)
aws ecr get-login-password --region "$REGION" \
  | docker login --username AWS --password-stdin "${IMAGE%%/*}" >/tmp/login.out 2>&1 \
  || { cat /tmp/login.out >&2; exit 1; }
docker pull --quiet "$IMAGE:latest" >/dev/null
docker stop -t 20 api >/dev/null 2>&1 || true
docker rm api >/dev/null 2>&1 || true
# Listens on the private network only; the security group also allows 8080
# from the edge and the other replicas alone.
docker run -d --name api --restart unless-stopped \
  --ulimit nofile=65536:65536 \
  -p "$PRIVATE_IP:8080:8080" \
  -e DATABASE_URL="$(param database-url)" \
  -e JWT_SECRET="$(param jwt-secret)" \
  -e CACHE_URL="$CACHE_URL" \
  -e AUTH_TOKEN_ROUTE_ENABLED=true \
  -e PORT=8080 \
  -e DB_POOL_MAX_CONNECTIONS=14 \
  -e RESERVE_SEMAPHORE_PERMITS=10 \
  -e SHUTDOWN_DRAIN_SECS=4 \
  -e REPLICA_ID="$REPLICA_ID" \
  -e PEERS="$PEERS" \
  --log-driver awslogs \
  --log-opt awslogs-region="$REGION" \
  --log-opt awslogs-group="$LOG_GROUP" \
  --log-opt awslogs-stream="$REPLICA_ID" \
  --log-opt mode=non-blocking --log-opt max-buffer-size=8m \
  "$IMAGE:latest" >/dev/null

for _ in $(seq 1 60); do
  [ "$(docker inspect -f '{{.State.Health.Status}}' api)" = healthy ] && { echo "$REPLICA_ID healthy"; exit 0; }
  sleep 2
done
echo "$REPLICA_ID did not become healthy" >&2
docker logs --tail 30 api >&2 || true
exit 1
