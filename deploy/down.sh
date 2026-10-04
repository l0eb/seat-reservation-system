#!/usr/bin/env bash
# Delete everything up.sh created, then list anything still tagged
# Project=seat-reservation. Safe to re-run.
set -uo pipefail
cd "$(dirname "$0")/.."
. deploy/config.sh

log "deleting the $PROJECT deployment in $REGION ($ACCOUNT)"
if [ "${1:-}" != --yes ]; then
  read -r -p "This deletes the instance, the database and all its data. Type 'delete' to continue: " ok
  [ "$ok" = delete ] || exit 1
fi

# Every tagged instance, not just the first: never leave one running.
IDS=$(aws ec2 describe-instances --region "$REGION" --filters Name=tag:Project,Values=$PROJECT \
  Name=instance-state-name,Values=pending,running,stopping,stopped \
  --query 'Reservations[].Instances[].InstanceId' --output text)
if [ -n "$IDS" ]; then
  log "terminating instance(s) $IDS"
  aws ec2 terminate-instances --region "$REGION" --instance-ids $IDS >/dev/null
fi
if aws rds describe-db-instances --region "$REGION" --db-instance-identifier "$DB_ID" >/dev/null 2>&1; then
  log "deleting RDS $DB_ID (no final snapshot)"
  aws rds delete-db-instance --region "$REGION" --db-instance-identifier "$DB_ID" \
    --skip-final-snapshot --delete-automated-backups >/dev/null
fi
[ -n "$IDS" ] && aws ec2 wait instance-terminated --region "$REGION" --instance-ids $IDS

ALLOC=$(eip_alloc)
[ "$ALLOC" != None ] && { log "releasing Elastic IP"; aws ec2 release-address --region "$REGION" --allocation-id "$ALLOC"; }

if aws iam get-role --role-name "$ROLE" >/dev/null 2>&1; then
  log "deleting IAM role $ROLE"
  aws iam remove-role-from-instance-profile --instance-profile-name "$ROLE" --role-name "$ROLE" 2>/dev/null
  aws iam delete-instance-profile --instance-profile-name "$ROLE" 2>/dev/null
  aws iam delete-role-policy --role-name "$ROLE" --policy-name app 2>/dev/null
  for arn in $(aws iam list-attached-role-policies --role-name "$ROLE" --query 'AttachedPolicies[].PolicyArn' --output text); do
    aws iam detach-role-policy --role-name "$ROLE" --policy-arn "$arn"
  done
  aws iam delete-role --role-name "$ROLE"
fi

log "deleting ECR repo, log group, SSM parameters"
aws ecr delete-repository --region "$REGION" --repository-name "$REPO" --force >/dev/null 2>&1
aws logs delete-log-group --region "$REGION" --log-group-name "$LOG_GROUP" 2>/dev/null
for p in db-password jwt-secret database-url; do
  aws ssm delete-parameter --region "$REGION" --name "$PARAMS/$p" 2>/dev/null
done

if aws rds describe-db-instances --region "$REGION" --db-instance-identifier "$DB_ID" >/dev/null 2>&1; then
  log "waiting for RDS to finish deleting (~5 min)"
  aws rds wait db-instance-deleted --region "$REGION" --db-instance-identifier "$DB_ID"
fi
for sg in "$API_SG" "$DB_SG"; do
  SG=$(sg_id "$sg")
  [ "$SG" != None ] && { log "deleting security group $sg"; aws ec2 delete-security-group --region "$REGION" --group-id "$SG"; }
done

log "anything still tagged Project=$PROJECT:"
LEFT=$(aws resourcegroupstaggingapi get-resources --region "$REGION" --tag-filters Key=Project,Values=$PROJECT \
  --query 'ResourceTagMappingList[].ResourceARN' --output text)
# The tagging index lags behind deletes by a few minutes; re-run to confirm.
[ -z "$LEFT" ] && echo "  nothing" || echo "$LEFT" | tr '\t' '\n' | sed 's/^/  /'
