#!/usr/bin/env bash
# Create (or finish creating) the AWS deployment. Safe to re-run: every
# step checks for what already exists. Takes ~10 min the first time,
# mostly waiting for RDS.
#
#   deploy/up.sh                      # c7i.large + db.t4g.small in your default region
#   INSTANCE_TYPE=t3.small deploy/up.sh
#
# Creates: ECR repo, 2 security groups, RDS Postgres, IAM role + instance
# profile, SSM parameters (DB URL, JWT secret), a CloudWatch log group, an
# Elastic IP and one EC2 instance. All tagged Project=seat-reservation;
# deploy/down.sh deletes them.
set -euo pipefail
cd "$(dirname "$0")/.."
. deploy/config.sh
VPC=$(vpc_id)

log "region $REGION, account $ACCOUNT, vpc $VPC"

# --- ECR + image ------------------------------------------------------------
if ! aws ecr describe-repositories --region "$REGION" --repository-names "$REPO" >/dev/null 2>&1; then
  log "creating ECR repo $REPO"
  aws ecr create-repository --region "$REGION" --repository-name "$REPO" --tags "$TAG_SPEC" >/dev/null
  # Every push leaves the previous image untagged; keep only the last 5.
  aws ecr put-lifecycle-policy --region "$REGION" --repository-name "$REPO" --lifecycle-policy-text \
    '{"rules":[{"rulePriority":1,"description":"keep last 5","selection":{"tagStatus":"any","countType":"imageCountMoreThan","countNumber":5},"action":{"type":"expire"}}]}' >/dev/null
fi

# --- security groups --------------------------------------------------------
API_SG_ID=$(sg_id "$API_SG")
if [ "$API_SG_ID" = None ]; then
  log "creating security group $API_SG (80, 443 from anywhere)"
  API_SG_ID=$(aws ec2 create-security-group --region "$REGION" --vpc-id "$VPC" --group-name "$API_SG" \
    --description "seat-reservation api host: http/https only" \
    --tag-specifications "ResourceType=security-group,Tags=[{$TAG_SPEC}]" --query GroupId --output text)
  for port in 80 443; do
    aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$API_SG_ID" --protocol tcp --port $port --cidr 0.0.0.0/0 >/dev/null
  done
fi
DB_SG_ID=$(sg_id "$DB_SG")
if [ "$DB_SG_ID" = None ]; then
  log "creating security group $DB_SG (5432 from the api host only)"
  DB_SG_ID=$(aws ec2 create-security-group --region "$REGION" --vpc-id "$VPC" --group-name "$DB_SG" \
    --description "seat-reservation postgres: api host only" \
    --tag-specifications "ResourceType=security-group,Tags=[{$TAG_SPEC}]" --query GroupId --output text)
  aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$DB_SG_ID" --protocol tcp --port 5432 --source-group "$API_SG_ID" >/dev/null
fi

# --- RDS --------------------------------------------------------------------
if ! aws rds describe-db-instances --region "$REGION" --db-instance-identifier "$DB_ID" >/dev/null 2>&1; then
  PG_VERSION=$(aws rds describe-db-engine-versions --region "$REGION" --engine postgres \
    --query 'DBEngineVersions[?starts_with(EngineVersion,`16.`)].EngineVersion' --output text | tr '\t' '\n' | sort -V | tail -1)
  # Saved before the database is created, so a failure in between can't
  # leave a database nobody has the password for.
  if ! aws ssm get-parameter --region "$REGION" --name "$PARAMS/db-password" >/dev/null 2>&1; then
    aws ssm put-parameter --region "$REGION" --name "$PARAMS/db-password" --type SecureString \
      --value "$(openssl rand -hex 24)" --tags "$TAG_SPEC" >/dev/null
  fi
  DB_PASSWORD=$(aws ssm get-parameter --region "$REGION" --name "$PARAMS/db-password" --with-decryption --query Parameter.Value --output text)
  log "creating RDS $DB_ID (postgres $PG_VERSION, $DB_CLASS); no backups: the data is disposable"
  aws rds create-db-instance --region "$REGION" --db-instance-identifier "$DB_ID" \
    --engine postgres --engine-version "$PG_VERSION" --db-instance-class "$DB_CLASS" \
    --allocated-storage "$DB_STORAGE_GB" --storage-type gp3 \
    --db-name "$DB_NAME" --master-username "$DB_USER" --master-user-password "$DB_PASSWORD" \
    --vpc-security-group-ids "$DB_SG_ID" --no-publicly-accessible --no-multi-az \
    --backup-retention-period 0 --no-deletion-protection --no-auto-minor-version-upgrade \
    --tags "$TAG_SPEC" >/dev/null
fi

# --- push the image while RDS is creating ------------------------------------
deploy/push.sh --no-deploy

# --- IAM role for the host: pull from ECR, read our params, write logs, SSM --
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

# --- logs, secrets ------------------------------------------------------------
if ! aws logs describe-log-groups --region "$REGION" --log-group-name-prefix "$LOG_GROUP" --query 'logGroups[0]' --output text | grep -q "$LOG_GROUP"; then
  aws logs create-log-group --region "$REGION" --log-group-name "$LOG_GROUP" --tags Project=$PROJECT
  aws logs put-retention-policy --region "$REGION" --log-group-name "$LOG_GROUP" --retention-in-days 7
fi
if ! aws ssm get-parameter --region "$REGION" --name "$PARAMS/jwt-secret" >/dev/null 2>&1; then
  aws ssm put-parameter --region "$REGION" --name "$PARAMS/jwt-secret" --type SecureString \
    --value "$(openssl rand -hex 32)" --tags "$TAG_SPEC" >/dev/null
fi

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

# --- Elastic IP (stable URL across stop/start) + instance ----------------------
if [ "$(eip_alloc)" = None ]; then
  aws ec2 allocate-address --region "$REGION" --domain vpc \
    --tag-specifications "ResourceType=elastic-ip,Tags=[{$TAG_SPEC}]" >/dev/null
fi
HOST=$(host_name)

ID=$(instance_id)
if [ "$ID" = None ]; then
  log "launching $INSTANCE_TYPE for $HOST"
  AMI=$(aws ssm get-parameter --region "$REGION" --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 --query Parameter.Value --output text)
  USER_DATA=$(mktemp)
  {
    echo '#!/bin/bash'
    echo 'set -euxo pipefail'
    echo 'dnf install -y docker && systemctl enable --now docker'
    echo 'mkdir -p /opt/seat'
    printf 'cat > /opt/seat/env <<"ENV"\nREGION=%s\nIMAGE=%s\nHOST=%s\nPARAMS=%s\nLOG_GROUP=%s\nENV\n' "$REGION" "$IMAGE" "$HOST" "$PARAMS" "$LOG_GROUP"
    printf 'cat > /opt/seat/deploy.sh <<"SCRIPT"\n%s\nSCRIPT\n' "$(cat deploy/host/deploy.sh)"
    echo 'chmod +x /opt/seat/deploy.sh && /opt/seat/deploy.sh'
  } > "$USER_DATA"
  # A new instance profile can take a few more seconds to be usable, so
  # retry. The client token makes every retry the *same* request: if one
  # actually launched but the CLI still reported an error, EC2 returns
  # that instance instead of starting a second one.
  CLIENT_TOKEN=$PROJECT-$(date +%s)
  for attempt in 1 2 3 4 5 6; do
  ID=$(aws ec2 run-instances --region "$REGION" --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
    --iam-instance-profile Name="$ROLE" --security-group-ids "$API_SG_ID" \
    --metadata-options HttpTokens=required,HttpPutResponseHopLimit=2 \
    --block-device-mappings 'DeviceName=/dev/xvda,Ebs={VolumeSize=16,VolumeType=gp3,DeleteOnTermination=true}' \
    --user-data "file://$USER_DATA" --client-token "$CLIENT_TOKEN" \
    --tag-specifications "ResourceType=instance,Tags=[{$TAG_SPEC},{Key=Name,Value=$PROJECT-api}]" \
                         "ResourceType=volume,Tags=[{$TAG_SPEC}]" \
    --query 'Instances[0].InstanceId' --output text) && break
    sleep 10
  done
  rm -f "$USER_DATA"
  [ -n "${ID:-}" ] || { echo "run-instances failed 6 times" >&2; exit 1; }
  aws ec2 wait instance-running --region "$REGION" --instance-ids "$ID"
  aws ec2 associate-address --region "$REGION" --instance-id "$ID" --allocation-id "$(eip_alloc)" >/dev/null
fi

log "waiting for https://$HOST/readyz (docker install + image pull + certificate, ~2-3 min)"
for i in $(seq 1 90); do
  if curl -fsS --max-time 5 "https://$HOST/readyz" >/dev/null 2>&1; then
    log "live: https://$HOST"
    curl -s "https://$HOST/readyz"; echo
    exit 0
  fi
  sleep 5
done
echo "not healthy after 7.5 min; check: aws logs tail $LOG_GROUP --region $REGION, or the instance's console output" >&2
exit 1
