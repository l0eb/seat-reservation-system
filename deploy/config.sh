# shellcheck shell=bash disable=SC2034
# Shared settings for the deploy scripts. Every resource is named from
# $PROJECT and tagged Project=$PROJECT, so down.sh can find all of it.
export AWS_PAGER=""
PROJECT=seat-reservation
REGION=${AWS_REGION:-$(aws configure get region || echo ap-south-1)}
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)

# Graviton (arm64): about 45% cheaper than the x86 equivalents for this.
INSTANCE_TYPE=${INSTANCE_TYPE:-c7g.large}
REPLICAS=3
DB_CLASS=${DB_CLASS:-db.t4g.small}
DB_STORAGE_GB=20

REPO=$PROJECT-api
IMAGE=$ACCOUNT.dkr.ecr.$REGION.amazonaws.com/$REPO
DB_ID=$PROJECT-db
DB_NAME=seats
DB_USER=seats_admin
ROLE=$PROJECT-ec2
EDGE_SG=$PROJECT-edge
API_SG=$PROJECT-api
DB_SG=$PROJECT-db
LOG_GROUP=/$PROJECT/api
PARAMS=/$PROJECT

TAG_SPEC="Key=Project,Value=$PROJECT"
log() { printf '\033[1m==> %s\033[0m\n' "$*" >&2; }

vpc_id() { aws ec2 describe-vpcs --region "$REGION" --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text; }
sg_id() { aws ec2 describe-security-groups --region "$REGION" --filters Name=group-name,Values="$1" Name=tag:Project,Values=$PROJECT --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null; }
# "id private-ip public-ip name" for each instance with the given Role, by
# name. Stopped ones count too: up.sh must not launch a second api-2
# because the first is stopped (push.sh then fails on it, by design).
instances() {
  aws ec2 describe-instances --region "$REGION" \
    --filters Name=tag:Project,Values=$PROJECT Name=tag:Role,Values="$1" \
      Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].[InstanceId,PrivateIpAddress,PublicIpAddress,Tags[?Key==`Name`]|[0].Value]' \
    --output text | sort -k4
}
eip_alloc() { aws ec2 describe-addresses --region "$REGION" --filters Name=tag:Project,Values=$PROJECT --query 'Addresses[0].AllocationId' --output text; }
eip_ip() { aws ec2 describe-addresses --region "$REGION" --filters Name=tag:Project,Values=$PROJECT --query 'Addresses[0].PublicIp' --output text; }
# sslip.io resolves 1-2-3-4.sslip.io to 1.2.3.4, which gives Caddy a real
# hostname to get a Let's Encrypt certificate for, without owning a domain.
site() { echo "$(eip_ip | tr . -).sslip.io"; }

# Run a script on an instance through SSM (no SSH) and wait for it. Files
# are shipped inline: run_on <instance> <script> [dest=localfile ...].
run_on() {
  local id=$1 script=$2; shift 2
  local params cmd status
  params=$(python3 - "$script" "$@" <<'PY'
import base64, json, sys
cmds = ["set -euo pipefail", "mkdir -p /opt/seat"]
for spec in sys.argv[2:]:
    dest, src = spec.split("=", 1)
    data = base64.b64encode(open(src, "rb").read()).decode()
    cmds.append(f"echo {data} | base64 -d > /opt/seat/{dest}")
cmds.append(f"bash /opt/seat/{sys.argv[1]}")
print(json.dumps({"commands": cmds, "executionTimeout": ["600"]}))
PY
)
  cmd=$(aws ssm send-command --region "$REGION" --instance-ids "$id" --document-name AWS-RunShellScript \
    --parameters "$params" --query Command.CommandId --output text)
  for _ in $(seq 1 120); do
    status=$(aws ssm get-command-invocation --region "$REGION" --command-id "$cmd" --instance-id "$id" \
      --query Status --output text 2>/dev/null || echo Pending)
    case $status in Pending|InProgress|Delayed) sleep 5 ;; *) break ;; esac
  done
  aws ssm get-command-invocation --region "$REGION" --command-id "$cmd" --instance-id "$id" \
    --query '[StandardOutputContent,StandardErrorContent]' --output text | sed '/^\s*$/d; s/^/    /'
  [ "$status" = Success ]
}
