#!/usr/bin/env bash
# Create (or finish creating) the AWS deployment, then deploy. Safe to
# re-run: every step checks for what already exists. Takes ~10-15 min the
# first time, mostly waiting for RDS.
#
#   deploy/up.sh
#   INSTANCE_TYPE=c7g.xlarge deploy/up.sh
#
# Creates, in the default VPC, all tagged Project=seat-reservation:
#   edge      1 x c7g.large on an Elastic IP: Caddy (TLS on <ip>.sslip.io,
#             least_conn over the replicas) + Dragonfly
#   api-1..3  3 x c7g.large, reachable only from the edge and each other
#   RDS       Postgres 16, db.t4g.small, reachable only from the replicas
#   plus an ECR repo, 4 security groups, an IAM role, SSM parameters (DB URL,
#   JWT secret) and a CloudWatch log group. deploy/down.sh deletes them.
# No SSH anywhere: instances are configured and redeployed through SSM.
set -euo pipefail
cd "$(dirname "$0")/.."
. deploy/config.sh
VPC=$(vpc_id)
log "region $REGION, account $ACCOUNT, vpc $VPC"

# --- ECR ----------------------------------------------------------------------
if ! aws ecr describe-repositories --region "$REGION" --repository-names "$REPO" >/dev/null 2>&1; then
  log "creating ECR repo $REPO"
  aws ecr create-repository --region "$REGION" --repository-name "$REPO" --tags "$TAG_SPEC" >/dev/null
  # Every push leaves the previous image untagged; keep only the last 5.
  aws ecr put-lifecycle-policy --region "$REGION" --repository-name "$REPO" --lifecycle-policy-text \
    '{"rules":[{"rulePriority":1,"description":"keep last 5","selection":{"tagStatus":"any","countType":"imageCountMoreThan","countNumber":5},"action":{"type":"expire"}}]}' >/dev/null
fi

# --- security groups -----------------------------------------------------------
new_sg() { # name description -> id
  local id
  id=$(sg_id "$1")
  if [ "$id" = None ]; then
    log "creating security group $1"
    id=$(aws ec2 create-security-group --region "$REGION" --vpc-id "$VPC" --group-name "$1" --description "$2" \
      --tag-specifications "ResourceType=security-group,Tags=[{$TAG_SPEC}]" --query GroupId --output text)
  fi
  echo "$id"
}
allow() { # group port (cidr|sg-id): idempotent
  local source err
  case $3 in sg-*) source="--source-group $3" ;; *) source="--cidr $3" ;; esac
  # shellcheck disable=SC2086
  if ! err=$(aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$1" --protocol tcp --port "$2" $source 2>&1 >/dev/null); then
    # Already there is fine (a re-run); anything else stops the deploy.
    grep -q InvalidPermission.Duplicate <<<"$err" || { echo "$err" >&2; exit 1; }
  fi
}
EDGE_SG_ID=$(new_sg "$EDGE_SG" "seat-reservation edge: https in, cache for the replicas")
API_SG_ID=$(new_sg "$API_SG" "seat-reservation api replicas: from the edge and each other only")
DB_SG_ID=$(new_sg "$DB_SG" "seat-reservation postgres: api replicas only")
allow "$EDGE_SG_ID" 80 0.0.0.0/0
allow "$EDGE_SG_ID" 443 0.0.0.0/0
allow "$EDGE_SG_ID" 6379 "$API_SG_ID"
allow "$API_SG_ID" 8080 "$EDGE_SG_ID"
allow "$API_SG_ID" 8080 "$API_SG_ID"   # /metrics adds up the peers' counters
allow "$DB_SG_ID" 5432 "$API_SG_ID"

# --- RDS: the slow part, so start it first ------------------------------------
if ! aws rds describe-db-instances --region "$REGION" --db-instance-identifier "$DB_ID" >/dev/null 2>&1; then
  # Saved before the database is created, so a failure in between can't
  # leave a database nobody has the password for.
  if ! aws ssm get-parameter --region "$REGION" --name "$PARAMS/db-password" >/dev/null 2>&1; then
    aws ssm put-parameter --region "$REGION" --name "$PARAMS/db-password" --type SecureString \
      --value "$(openssl rand -hex 24)" --tags "$TAG_SPEC" >/dev/null
  fi
  DB_PASSWORD=$(aws ssm get-parameter --region "$REGION" --name "$PARAMS/db-password" --with-decryption --query Parameter.Value --output text)
  PG_VERSION=$(aws rds describe-db-engine-versions --region "$REGION" --engine postgres \
    --query 'DBEngineVersions[?starts_with(EngineVersion,`16.`)].EngineVersion' --output text | tr '\t' '\n' | sort -V | tail -1)
  log "creating RDS $DB_ID (postgres $PG_VERSION, $DB_CLASS); no backups: the data is disposable"
  aws rds create-db-instance --region "$REGION" --db-instance-identifier "$DB_ID" \
    --engine postgres --engine-version "$PG_VERSION" --db-instance-class "$DB_CLASS" \
    --allocated-storage "$DB_STORAGE_GB" --storage-type gp3 \
    --db-name "$DB_NAME" --master-username "$DB_USER" --master-user-password "$DB_PASSWORD" \
    --vpc-security-group-ids "$DB_SG_ID" --no-publicly-accessible --no-multi-az \
    --backup-retention-period 0 --no-deletion-protection --no-auto-minor-version-upgrade \
    --tags "$TAG_SPEC" >/dev/null
fi

# --- image, while RDS is creating ---------------------------------------------
deploy/push.sh --build-only

# --- IAM role for the instances: pull from ECR, read our params, write logs, SSM
if ! aws iam get-role --role-name "$ROLE" >/dev/null 2>&1; then
  log "creating IAM role + instance profile $ROLE"
  aws iam create-role --role-name "$ROLE" --tags "$TAG_SPEC" \
    --assume-role-policy-document '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}' >/dev/null
  aws iam attach-role-policy --role-name "$ROLE" --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore
  aws iam attach-role-policy --role-name "$ROLE" --policy-arn arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly
  aws iam put-role-policy --role-name "$ROLE" --policy-name app --policy-document "{
    \"Version\":\"2012-10-17\",\"Statement\":[
      {\"Effect\":\"Allow\",\"Action\":[\"ssm:GetParameter\",\"ssm:GetParameters\"],
       \"Resource\":\"arn:aws:ssm:$REGION:$ACCOUNT:parameter$PARAMS/*\"},
      {\"Effect\":\"Allow\",\"Action\":[\"logs:CreateLogStream\",\"logs:PutLogEvents\",\"logs:DescribeLogStreams\"],
       \"Resource\":\"arn:aws:logs:$REGION:$ACCOUNT:log-group:$LOG_GROUP:*\"}]}"
  aws iam create-instance-profile --instance-profile-name "$ROLE" --tags "$TAG_SPEC" >/dev/null
  aws iam add-role-to-instance-profile --instance-profile-name "$ROLE" --role-name "$ROLE"
  sleep 10 # instance profiles take a moment to become usable by EC2
fi

# Grafana on the edge reads logs and metrics with the instance role: read
# only, and logs only from this project's log group. Re-applied every run.
aws iam put-role-policy --role-name "$ROLE" --policy-name observability-read --policy-document "{
  \"Version\":\"2012-10-17\",\"Statement\":[
    {\"Effect\":\"Allow\",\"Action\":[\"logs:StartQuery\",\"logs:GetLogEvents\",\"logs:FilterLogEvents\",\"logs:GetLogGroupFields\"],
     \"Resource\":\"arn:aws:logs:$REGION:$ACCOUNT:log-group:$LOG_GROUP:*\"},
    {\"Effect\":\"Allow\",\"Action\":[\"logs:DescribeLogGroups\",\"logs:GetQueryResults\",\"logs:StopQuery\",
       \"cloudwatch:GetMetricData\",\"cloudwatch:ListMetrics\",\"ec2:DescribeRegions\"],\"Resource\":\"*\"}]}"

# --- logs, secrets ---------------------------------------------------------------
if ! aws logs describe-log-groups --region "$REGION" --log-group-name-prefix "$LOG_GROUP" --query 'logGroups[0].logGroupName' --output text | grep -qx "$LOG_GROUP"; then
  aws logs create-log-group --region "$REGION" --log-group-name "$LOG_GROUP" --tags Project=$PROJECT
  aws logs put-retention-policy --region "$REGION" --log-group-name "$LOG_GROUP" --retention-in-days 7
fi
if ! aws ssm get-parameter --region "$REGION" --name "$PARAMS/jwt-secret" >/dev/null 2>&1; then
  aws ssm put-parameter --region "$REGION" --name "$PARAMS/jwt-secret" --type SecureString \
    --value "$(openssl rand -hex 32)" --tags "$TAG_SPEC" >/dev/null
fi

# --- instances: Docker only at boot; configured by push.sh through SSM ----------
AMI=$(aws ssm get-parameter --region "$REGION" --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 --query Parameter.Value --output text)
USER_DATA=$(mktemp)
cat > "$USER_DATA" <<'EOF'
#!/bin/bash
set -eux
dnf install -y docker
systemctl enable --now docker
mkdir -p /opt/seat
EOF
launch() { # role name security-group
  local existing token id
  existing=$(instances "$1" | awk -v n="$2" '$4==n {print $1}')
  [ -n "$existing" ] && return
  log "launching $2 ($INSTANCE_TYPE)"
  # Retries are the same request (client token): if one launched but the
  # CLI still reported an error, EC2 returns that instance, never a second.
  token=$2-$(date +%s)
  for _ in 1 2 3 4 5 6; do
    id=$(aws ec2 run-instances --region "$REGION" --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
      --iam-instance-profile Name="$ROLE" --security-group-ids "$3" \
      --metadata-options HttpTokens=required,HttpPutResponseHopLimit=2 \
      --block-device-mappings 'DeviceName=/dev/xvda,Ebs={VolumeSize=16,VolumeType=gp3,DeleteOnTermination=true}' \
      --user-data "file://$USER_DATA" --client-token "$token" \
      --tag-specifications "ResourceType=instance,Tags=[{$TAG_SPEC},{Key=Name,Value=$2},{Key=Role,Value=$1}]" \
                           "ResourceType=volume,Tags=[{$TAG_SPEC}]" \
      --query 'Instances[0].InstanceId' --output text) && break
    sleep 10 # a new instance profile can take a few more seconds
  done
  [ -n "${id:-}" ] || { echo "could not launch $2" >&2; exit 1; }
}
for i in $(seq 1 $REPLICAS); do launch api "$PROJECT-api-$i" "$API_SG_ID"; done
launch edge "$PROJECT-edge" "$EDGE_SG_ID"
rm -f "$USER_DATA"

# --- Elastic IP on the edge: a stable URL across stop/start --------------------
if [ "$(eip_alloc)" = None ]; then
  aws ec2 allocate-address --region "$REGION" --domain vpc \
    --tag-specifications "ResourceType=elastic-ip,Tags=[{$TAG_SPEC}]" >/dev/null
fi
EDGE_ID=$(instances edge | awk '{print $1}')
aws ec2 wait instance-running --region "$REGION" --instance-ids "$EDGE_ID"
aws ec2 associate-address --region "$REGION" --instance-id "$EDGE_ID" --allocation-id "$(eip_alloc)" >/dev/null

# --- RDS ready: store the connection string ---------------------------------------
log "waiting for RDS to be available (the slow part)"
aws rds wait db-instance-available --region "$REGION" --db-instance-identifier "$DB_ID"
DB_HOST=$(aws rds describe-db-instances --region "$REGION" --db-instance-identifier "$DB_ID" --query 'DBInstances[0].Endpoint.Address' --output text)
DB_PASSWORD=$(aws ssm get-parameter --region "$REGION" --name "$PARAMS/db-password" --with-decryption --query Parameter.Value --output text)
# RDS enforces TLS; sqlx encrypts with sslmode=require.
DATABASE_URL="postgres://$DB_USER:$DB_PASSWORD@$DB_HOST:5432/$DB_NAME?sslmode=require"
if aws ssm get-parameter --region "$REGION" --name "$PARAMS/database-url" >/dev/null 2>&1; then
  aws ssm put-parameter --region "$REGION" --name "$PARAMS/database-url" --type SecureString --overwrite --value "$DATABASE_URL" >/dev/null
else # --tags can't be combined with --overwrite
  aws ssm put-parameter --region "$REGION" --name "$PARAMS/database-url" --type SecureString --tags "$TAG_SPEC" --value "$DATABASE_URL" >/dev/null
fi

# --- instances ready: registered with SSM, Docker installed ----------------------
ALL_IDS=$( { instances api; instances edge; } | awk '{print $1}')
log "waiting for the instances to register with SSM"
for id in $ALL_IDS; do
  for _ in $(seq 1 60); do
    [ "$(aws ssm describe-instance-information --region "$REGION" --filters "Key=InstanceIds,Values=$id" \
        --query 'InstanceInformationList[0].PingStatus' --output text)" = Online ] && break
    sleep 5
  done
done
TMP=$(mktemp); echo 'cloud-init status --wait >/dev/null; docker info >/dev/null && echo docker ready' > "$TMP"
for id in $ALL_IDS; do run_on "$id" ready.sh ready.sh="$TMP"; done
rm -f "$TMP"

# --- deploy --------------------------------------------------------------------------
deploy/push.sh --edge --no-build

URL="https://$(site)"
log "waiting for $URL/readyz (certificate, first health checks)"
for _ in $(seq 1 60); do
  if curl -fsS --max-time 5 "$URL/readyz" >/dev/null 2>&1; then
    log "live: $URL"
    curl -s "$URL/metrics" | grep '^replica_up'
    exit 0
  fi
  sleep 5
done
echo "$URL not ready after 5 min; check: aws logs tail $LOG_GROUP --region $REGION" >&2
exit 1
