# Shared settings for the deploy scripts. Every resource is named from
# $PROJECT and tagged Project=$PROJECT, so down.sh can find all of it.
export AWS_PAGER=""
PROJECT=seat-reservation
REGION=${AWS_REGION:-$(aws configure get region || echo ap-south-1)}
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)

INSTANCE_TYPE=${INSTANCE_TYPE:-c7i.large}
DB_CLASS=${DB_CLASS:-db.t4g.small}
DB_STORAGE_GB=20

REPO=$PROJECT-api
IMAGE=$ACCOUNT.dkr.ecr.$REGION.amazonaws.com/$REPO
DB_ID=$PROJECT-db
DB_NAME=seats
DB_USER=seats_admin
ROLE=$PROJECT-ec2
API_SG=$PROJECT-api
DB_SG=$PROJECT-db
LOG_GROUP=/$PROJECT/api
PARAMS=/$PROJECT

TAG_SPEC="Key=Project,Value=$PROJECT"
log() { printf '\033[1m==> %s\033[0m\n' "$*"; }

vpc_id() { aws ec2 describe-vpcs --region "$REGION" --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text; }
sg_id() { aws ec2 describe-security-groups --region "$REGION" --filters Name=group-name,Values="$1" Name=tag:Project,Values=$PROJECT --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null; }
instance_id() { aws ec2 describe-instances --region "$REGION" --filters Name=tag:Project,Values=$PROJECT Name=instance-state-name,Values=pending,running,stopping,stopped --query 'Reservations[0].Instances[0].InstanceId' --output text; }
eip_alloc() { aws ec2 describe-addresses --region "$REGION" --filters Name=tag:Project,Values=$PROJECT --query 'Addresses[0].AllocationId' --output text; }
eip_ip() { aws ec2 describe-addresses --region "$REGION" --filters Name=tag:Project,Values=$PROJECT --query 'Addresses[0].PublicIp' --output text; }
# sslip.io resolves 1-2-3-4.sslip.io to 1.2.3.4, which gives Caddy a real
# hostname to get a Let's Encrypt certificate for, without owning a domain.
host_name() { echo "$(eip_ip | tr . -).sslip.io"; }
