#!/bin/bash
# Runs on the EC2 host (first boot, and on every redeploy via SSM).
# Pulls the latest api image and (re)starts api, Dragonfly and Caddy.
# Reads REGION, IMAGE, HOST, PARAMS, LOG_GROUP from /opt/seat/env.
set -euo pipefail
. /opt/seat/env

param() { aws ssm get-parameter --region "$REGION" --name "$PARAMS/$1" --with-decryption --query Parameter.Value --output text; }

aws ecr get-login-password --region "$REGION" | docker login --username AWS --password-stdin "${IMAGE%%/*}"
docker pull "$IMAGE:latest"
docker network inspect seat >/dev/null 2>&1 || docker network create seat

if ! docker inspect dragonfly >/dev/null 2>&1; then
  docker run -d --name dragonfly --network seat --restart unless-stopped --ulimit memlock=-1 \
    docker.dragonflydb.io/dragonflydb/dragonfly:latest \
    --proactor_threads=1 --maxmemory=512mb --cache_mode=true
fi

docker rm -f api >/dev/null 2>&1 || true
docker run -d --name api --network seat --restart unless-stopped \
  --ulimit nofile=65536:65536 \
  -e DATABASE_URL="$(param database-url)" \
  -e JWT_SECRET="$(param jwt-secret)" \
  -e CACHE_URL=redis://dragonfly:6379 \
  -e AUTH_TOKEN_ROUTE_ENABLED=true \
  -e PORT=8080 \
  --log-driver awslogs \
  --log-opt awslogs-region="$REGION" \
  --log-opt awslogs-group="$LOG_GROUP" \
  --log-opt awslogs-stream=api \
  --log-opt mode=non-blocking --log-opt max-buffer-size=8m \
  "$IMAGE:latest"

# Caddy terminates TLS (Let's Encrypt, kept in the caddy_data volume) and
# proxies to the api container. Only ports 80 and 443 are open. It also
# compresses responses: a 13,000-seat GET /shows/{id} is ~490 KB raw and
# ~38 KB gzipped, and outbound data is what AWS bills per GB.
cat > /opt/seat/Caddyfile <<CADDY
$HOST {
	encode zstd gzip
	reverse_proxy api:8080
}
CADDY
docker rm -f caddy >/dev/null 2>&1 || true
docker run -d --name caddy --network seat --restart unless-stopped \
  -p 80:80 -p 443:443 -v caddy_data:/data -v /opt/seat/Caddyfile:/etc/caddy/Caddyfile:ro \
  caddy:2

for i in $(seq 1 60); do
  [ "$(docker inspect -f '{{.State.Health.Status}}' api)" = healthy ] && { echo "api healthy"; exit 0; }
  sleep 2
done
echo "api did not become healthy" >&2; docker logs --tail 50 api >&2 || true; exit 1
