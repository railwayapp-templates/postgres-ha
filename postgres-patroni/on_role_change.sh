#!/bin/bash
# on_role_change.sh - Patroni callback for role changes (failover detection)
#
# Called by Patroni with: $1=action $2=role $3=scope
# Sends telemetry to Railway backboard for monitoring/alerting
#
# Environment variables used:
#   RAILWAY_PROJECT_ID - Railway project ID
#   RAILWAY_ENVIRONMENT_ID - Railway environment ID
#   RAILWAY_SERVICE_ID - Railway service ID
#   PATRONI_NAME - Node name
#   RAILWAY_PRIVATE_DOMAIN - Node address

ACTION="$1"
ROLE="$2"
SCOPE="$3"

# Only proceed for role changes
if [ "$ACTION" != "on_role_change" ]; then
    exit 0
fi

NODE_NAME="${PATRONI_NAME:-unknown}"
NODE_ADDRESS="${RAILWAY_PRIVATE_DOMAIN:-unknown}"
PROJECT_ID="${RAILWAY_PROJECT_ID:-}"
ENVIRONMENT_ID="${RAILWAY_ENVIRONMENT_ID:-}"
SERVICE_ID="${RAILWAY_SERVICE_ID:-}"

TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

# Determine event type based on new role
case "$ROLE" in
    master|primary)
        # This node was just promoted to primary - this is the main failover event
        # Happens when: replica takes over after primary failure
        EVENT_TYPE="POSTGRES_HA_FAILOVER"
        MESSAGE="Node promoted to primary (failover completed)"
        ;;
    replica|standby)
        # This node became a replica - happens when:
        # 1. Old primary recovers and rejoins as replica
        # 2. Manual switchover/failback
        # Less urgent than promotion, but still worth logging
        EVENT_TYPE="POSTGRES_HA_REJOINED"
        MESSAGE="Node rejoined cluster as replica"
        ;;
    *)
        # Unknown role change
        EVENT_TYPE="POSTGRES_HA_ROLE_CHANGE"
        MESSAGE="Node role changed to ${ROLE}"
        ;;
esac

# Log locally for container logs
echo "[${TIMESTAMP}] ${EVENT_TYPE}: ${MESSAGE} (node=${NODE_NAME}, scope=${SCOPE}, service=${SERVICE_ID})"

METADATA="node=${NODE_NAME}, role=${ROLE}, scope=${SCOPE}, address=${NODE_ADDRESS}, serviceId=${SERVICE_ID}, projectId=${PROJECT_ID}, environmentId=${ENVIRONMENT_ID}"

GRAPHQL_ENDPOINT="${RAILWAY_GRAPHQL_ENDPOINT:-https://backboard.railway.app/graphql/internal}"

PAYLOAD=$(cat <<EOF
{
  "query": "mutation telemetrySend(\$input: TelemetrySendInput!) { telemetrySend(input: \$input) }",
  "variables": {
    "input": {
      "command": "${EVENT_TYPE}",
      "error": "${MESSAGE}",
      "stacktrace": "${METADATA}",
      "projectId": "${PROJECT_ID}",
      "environmentId": "${ENVIRONMENT_ID}",
      "version": "postgres-ha"
    }
  }
}
EOF
)

if command -v curl &> /dev/null; then
    curl -s -X POST \
        -H "Content-Type: application/json" \
        -d "${PAYLOAD}" \
        "${GRAPHQL_ENDPOINT}" \
        --max-time 5 \
        > /dev/null 2>&1 &
fi

# Always exit 0 to not block Patroni
exit 0
