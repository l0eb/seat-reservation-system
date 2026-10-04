#!/usr/bin/env bash
# Build the api image, push it to ECR, and (if the host is up) redeploy it.
#   deploy/push.sh            build + push + redeploy
#   deploy/push.sh --no-deploy  build + push only (used by up.sh)
set -euo pipefail
cd "$(dirname "$0")/.."
. deploy/config.sh

log "building and pushing $IMAGE:latest"
aws ecr get-login-password --region "$REGION" | docker login --username AWS --password-stdin "${IMAGE%%/*}" >/dev/null
docker build --platform linux/amd64 -t "$IMAGE:latest" .
docker push --quiet "$IMAGE:latest"

[ "${1:-}" = --no-deploy ] && exit 0
ID=$(instance_id)
[ "$ID" = None ] && { echo "no instance yet; run deploy/up.sh"; exit 0; }

log "redeploying on $ID"
CMD=$(aws ssm send-command --region "$REGION" --instance-ids "$ID" \
  --document-name AWS-RunShellScript --parameters 'commands=["/opt/seat/deploy.sh"]' \
  --query Command.CommandId --output text)
aws ssm wait command-executed --region "$REGION" --command-id "$CMD" --instance-id "$ID" || true
aws ssm get-command-invocation --region "$REGION" --command-id "$CMD" --instance-id "$ID" \
  --query '[Status,StandardOutputContent,StandardErrorContent]' --output text | tail -5
