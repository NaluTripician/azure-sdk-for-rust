#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. All rights reserved.
# Licensed under the MIT License.
#
# Grants the Cosmos Rust SDK team read access to the soak dashboard.
#
# Azure Managed Grafana authenticates with Entra ID, so "accessible to anyone on
# the team" is an Azure RBAC assignment on the Grafana resource — no Grafana
# users, passwords, or invite links to manage. Assign to a *group* rather than
# individuals so joiners and leavers are handled by group membership.
#
# Roles:
#   Grafana Viewer  read dashboards (the default, and what most of the team wants)
#   Grafana Editor  create/modify dashboards
#   Grafana Admin   manage the workspace itself
#
# Usage:
#   ./grant-team-access.sh                          # uses TEAM_ENTRA_GROUP from soak.env
#   ./grant-team-access.sh --group "Cosmos SDK Team"
#   ./grant-team-access.sh --group <object-id> --role "Grafana Editor"
#   ./grant-team-access.sh --list                   # show current assignments

# shellcheck source=common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

LIST_ONLY=false
while [[ $# -gt 0 ]]; do
    case "$1" in
    --group)
        TEAM_ENTRA_GROUP="${2:?--group requires a value}"
        shift
        ;;
    --role)
        TEAM_GRAFANA_ROLE="${2:?--role requires a value}"
        shift
        ;;
    --list) LIST_ONLY=true ;;
    -h | --help)
        sed -n '2,22p' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    *) die "unknown option: $1" ;;
    esac
    shift
done

require_cmd az
require_var SUBSCRIPTION_ID
az_select_subscription

az extension show --name amg >/dev/null 2>&1 || az extension add --name amg --only-show-errors

GRAFANA_ID="$(az grafana show --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" --query id -o tsv 2>/dev/null || true)"
[[ -n "${GRAFANA_ID}" ]] ||
    die "Grafana workspace ${GRAFANA_NAME} not found; run ./provision-soak-infra.sh first"

if $LIST_ONLY; then
    log "Role assignments on ${GRAFANA_NAME}"
    az role assignment list \
        --scope "${GRAFANA_ID}" \
        --include-inherited \
        --query "[].{principal:principalName, type:principalType, role:roleDefinitionName}" \
        -o table
    exit 0
fi

require_var TEAM_ENTRA_GROUP

# Accept either an object id or a display name so nobody has to go hunting in
# the portal for a GUID.
if [[ "${TEAM_ENTRA_GROUP}" =~ ^[0-9a-fA-F-]{36}$ ]]; then
    GROUP_ID="${TEAM_ENTRA_GROUP}"
else
    log "Resolving Entra group '${TEAM_ENTRA_GROUP}'"
    GROUP_ID="$(az ad group show --group "${TEAM_ENTRA_GROUP}" --query id -o tsv 2>/dev/null || true)"
    [[ -n "${GROUP_ID}" ]] || die "could not resolve Entra group '${TEAM_ENTRA_GROUP}'"
fi

log "Assigning '${TEAM_GRAFANA_ROLE}' on ${GRAFANA_NAME} to group ${GROUP_ID}"
if az role assignment list \
    --scope "${GRAFANA_ID}" \
    --assignee "${GROUP_ID}" \
    --role "${TEAM_GRAFANA_ROLE}" \
    --query "[0]" -o tsv | grep -q .; then
    log "Assignment already exists; nothing to do"
else
    az role assignment create \
        --scope "${GRAFANA_ID}" \
        --assignee-object-id "${GROUP_ID}" \
        --assignee-principal-type Group \
        --role "${TEAM_GRAFANA_ROLE}" \
        --only-show-errors -o none
fi

# Reading a dashboard also requires querying the Azure Monitor workspace behind
# the Prometheus data source. Without this the dashboard loads but every panel
# returns a 403, which reads as "the soak is broken" rather than "you lack
# permission".
MONITOR_WORKSPACE_ID="$(az monitor account show --name "${MONITOR_WORKSPACE}" \
    --resource-group "${RESOURCE_GROUP}" --query id -o tsv 2>/dev/null || true)"
if [[ -n "${MONITOR_WORKSPACE_ID}" ]]; then
    log "Granting Monitoring Data Reader on ${MONITOR_WORKSPACE}"
    if ! az role assignment list \
        --scope "${MONITOR_WORKSPACE_ID}" \
        --assignee "${GROUP_ID}" \
        --role "Monitoring Data Reader" \
        --query "[0]" -o tsv | grep -q .; then
        az role assignment create \
            --scope "${MONITOR_WORKSPACE_ID}" \
            --assignee-object-id "${GROUP_ID}" \
            --assignee-principal-type Group \
            --role "Monitoring Data Reader" \
            --only-show-errors -o none
    fi
else
    warn "Azure Monitor workspace ${MONITOR_WORKSPACE} not found; skipping Monitoring Data Reader"
fi

GRAFANA_ENDPOINT="$(az grafana show --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" --query properties.endpoint -o tsv)"

cat <<EOF

$(log "Access granted")

Anyone in the group can now sign in with their Microsoft account at:

  ${GRAFANA_ENDPOINT}/d/cosmos-rust-ws9

Role assignments can take a few minutes to propagate.
EOF
