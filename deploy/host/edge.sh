#!/bin/bash
# Runs on the edge instance (via SSM, as root, from /opt/seat): Dragonfly
# for the replicas' cache; Prometheus and Grafana; and Caddy in front of
# the replicas and Grafana with the repo's Caddyfile (TLS for $SITE,
# least_conn, /readyz health checks, gzip).
#   /opt/seat/host.env           PRIVATE_IP SITE UPSTREAMS REGION PARAMS
#   /opt/seat/Caddyfile          caddy/Caddyfile from the repo
#   /opt/seat/observability.tgz  the repo's observability/ directory
#   /opt/seat/targets.json       the replicas, for Prometheus
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

# Caddy, Prometheus and Grafana share a private Docker network; only Caddy
# publishes ports, so Prometheus is never reachable from outside and
# Grafana only through Caddy at /grafana.
docker network inspect obs >/dev/null 2>&1 || docker network create obs >/dev/null
rm -rf /opt/seat/observability && mkdir -p /opt/seat/observability
tar -xzf /opt/seat/observability.tgz -C /opt/seat/observability

docker rm -f prometheus >/dev/null 2>&1 || true
docker pull --quiet prom/prometheus:v3.15.0 >/dev/null
docker run -d --name prometheus --network obs --restart unless-stopped \
  -v /opt/seat/observability/prometheus/prometheus.yml:/etc/prometheus/prometheus.yml:ro \
  -v /opt/seat/targets.json:/etc/prometheus/targets.json:ro \
  -v prometheus_data:/prometheus \
  prom/prometheus:v3.15.0 \
  --config.file=/etc/prometheus/prometheus.yml --storage.tsdb.retention.time=7d >/dev/null

# Anonymous visitors are read-only Viewers. The admin password lives in
# Parameter Store (it only takes effect when grafana_data is first created).
GRAFANA_PASSWORD=$(aws ssm get-parameter --region "$REGION" --name "$PARAMS/grafana-admin-password" \
  --with-decryption --query Parameter.Value --output text)
docker rm -f grafana >/dev/null 2>&1 || true
docker pull --quiet grafana/grafana:13.2.3 >/dev/null
docker run -d --name grafana --network obs --restart unless-stopped \
  -e GF_SERVER_ROOT_URL="https://$SITE/grafana/" \
  -e GF_SERVER_SERVE_FROM_SUB_PATH=true \
  -e GF_AUTH_ANONYMOUS_ENABLED=true \
  -e GF_AUTH_ANONYMOUS_ORG_ROLE=Viewer \
  -e GF_USERS_ALLOW_SIGN_UP=false \
  -e GF_SECURITY_ADMIN_PASSWORD="$GRAFANA_PASSWORD" \
  -e GF_DASHBOARDS_DEFAULT_HOME_DASHBOARD_PATH=/etc/grafana/dashboards/metrics.json \
  -e GF_ANALYTICS_REPORTING_ENABLED=false \
  -e GF_ANALYTICS_CHECK_FOR_UPDATES=false \
  -v /opt/seat/observability/grafana/provisioning:/etc/grafana/provisioning:ro \
  -v /opt/seat/observability/grafana/dashboards:/etc/grafana/dashboards:ro \
  -v grafana_data:/var/lib/grafana \
  grafana/grafana:13.2.3 >/dev/null

# Recreated so a changed Caddyfile or replica list takes effect; the
# certificate survives in the caddy_data volume.
docker rm -f caddy >/dev/null 2>&1 || true
docker pull --quiet caddy:2 >/dev/null
docker run -d --name caddy --network obs --restart unless-stopped \
  -p 80:80 -p 443:443 \
  -e SITE="$SITE" -e UPSTREAMS="$UPSTREAMS" \
  -v /opt/seat/Caddyfile:/etc/caddy/Caddyfile:ro -v caddy_data:/data \
  caddy:2 >/dev/null
echo "edge serving https://$SITE -> $UPSTREAMS, Grafana at https://$SITE/grafana/"
