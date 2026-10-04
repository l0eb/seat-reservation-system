#!/bin/bash
# Runs on the edge instance (via SSM, as root, from /opt/seat): Dragonfly
# for the replicas' cache, and Caddy in front of them with the repo's
# Caddyfile (TLS for $SITE, least_conn, /readyz health checks, gzip).
#   /opt/seat/host.env   PRIVATE_IP SITE UPSTREAMS
#   /opt/seat/Caddyfile  caddy/Caddyfile from the repo
set -euo pipefail
cd /opt/seat
. ./host.env

# Private network only: the replicas reach it at $PRIVATE_IP:6379.
if ! docker inspect dragonfly >/dev/null 2>&1; then
  docker pull --quiet docker.dragonflydb.io/dragonflydb/dragonfly:latest >/dev/null
  docker run -d --name dragonfly --restart unless-stopped --ulimit memlock=-1 \
    -p "$PRIVATE_IP:6379:6379" \
    docker.dragonflydb.io/dragonflydb/dragonfly:latest \
    --proactor_threads=1 --maxmemory=512mb --cache_mode=true >/dev/null
fi

# Recreated so a changed Caddyfile or replica list takes effect; the
# certificate survives in the caddy_data volume.
docker rm -f caddy >/dev/null 2>&1 || true
docker pull --quiet caddy:2 >/dev/null
docker run -d --name caddy --restart unless-stopped \
  -p 80:80 -p 443:443 \
  -e SITE="$SITE" -e UPSTREAMS="$UPSTREAMS" \
  -v /opt/seat/Caddyfile:/etc/caddy/Caddyfile:ro -v caddy_data:/data \
  caddy:2 >/dev/null
echo "edge serving https://$SITE -> $UPSTREAMS"
