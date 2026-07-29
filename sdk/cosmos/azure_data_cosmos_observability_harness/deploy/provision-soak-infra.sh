#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. All rights reserved.
# Licensed under the MIT License.
#
# Provisions the Azure infrastructure for the long-running Cosmos DB Rust SDK
# observability soak:
#
#   Resource group
#   Azure Container Registry            image for the harness
#   Azure Monitor workspace             18-month Prometheus metric retention
#   Azure Managed Grafana               team-facing dashboard, Entra SSO
#   AKS (OIDC + workload identity)      runs harness + OTel Collector,
#                                       managed Prometheus addon scrapes them
#   Cosmos DB account                   the real account under test
#   User-assigned managed identity      federated to the pod service account,
#                                       holds Cosmos Data Contributor
#
# Idempotent: safe to re-run. Every step checks for an existing resource first,
# so a partially-failed run can simply be repeated.
#
# Usage:
#   cp soak.env.example soak.env    # fill in SUBSCRIPTION_ID at minimum
#   ./provision-soak-infra.sh
#
# Options:
#   --attach-to-perf  Share the Cosmos perf harness's cluster, registry, Grafana
#                     workspace and managed identity instead of standing up a
#                     second stack. Requires PERF_RESOURCE_GROUP. Creates only
#                     what the perf harness has no equivalent of: an Azure
#                     Monitor workspace, the managed Prometheus addon, a
#                     dedicated tainted node pool, and the soak's Cosmos account.
#   --skip-cosmos     Do not create a Cosmos account (still assigns RBAC).
#   --dry-run         Print what would be created and exit.

# cspell:ignore nodepool soakpool subshell

# shellcheck source=common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

ATTACH_TO_PERF=false
SKIP_COSMOS=false
DRY_RUN=false
while [[ $# -gt 0 ]]; do
    case "$1" in
    --attach-to-perf) ATTACH_TO_PERF=true ;;
    --skip-cosmos) SKIP_COSMOS=true ;;
    --dry-run) DRY_RUN=true ;;
    -h | --help)
        sed -n '2,33p' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    *) die "unknown option: $1" ;;
    esac
    shift
done

require_cmd az
require_az_version
require_var SUBSCRIPTION_ID

# --- Sharing the perf harness's stack ----------------------------------------

# Names one resource of a given type in the perf resource group. Ambiguity is
# only resolved automatically when exactly one candidate is named for perf;
# anything else is a question the operator has to answer, not a guess worth
# making against live infrastructure.
discover_perf_resource() {
    local label="$1" resource_type="$2" names count preferred

    names="$(az resource list \
        --resource-group "${PERF_RESOURCE_GROUP}" \
        --resource-type "${resource_type}" \
        --query "[].name" -o tsv 2>/dev/null || true)"
    count="$(printf '%s' "${names}" | grep -c . || true)"

    if [[ "${count}" -eq 0 ]]; then
        die "no ${label} in resource group ${PERF_RESOURCE_GROUP}. Is that the group the perf harness deployed into?"
    fi

    if [[ "${count}" -gt 1 ]]; then
        preferred="$(printf '%s\n' "${names}" | grep -i perf || true)"
        if [[ "$(printf '%s' "${preferred}" | grep -c . || true)" -eq 1 ]]; then
            warn "${count} ${label} resources in ${PERF_RESOURCE_GROUP}; choosing '${preferred}' by name"
            printf '%s' "${preferred}"
            return 0
        fi
        die "${count} ${label} resources in ${PERF_RESOURCE_GROUP} ($(printf '%s' "${names}" | tr '\n' ' ')). Set the matching PERF_* variable in soak.env to choose one."
    fi

    printf '%s' "${names}"
}

# `die` inside a command substitution only kills the subshell, so every resolved
# value is re-checked in this shell before it is used.
resolve_perf_resource() {
    local var="$1" label="$2" resource_type="$3" value
    if [[ -z "${!var}" ]]; then
        value="$(discover_perf_resource "${label}" "${resource_type}")"
        [[ -n "${value}" ]] || die "could not resolve the ${label} in ${PERF_RESOURCE_GROUP}"
        printf -v "${var}" '%s' "${value}"
    fi
    log "  ${label}: ${!var}"
}

if $ATTACH_TO_PERF; then
    require_var PERF_RESOURCE_GROUP
    az_select_subscription

    az group show --name "${PERF_RESOURCE_GROUP}" -o none 2>/dev/null ||
        die "resource group ${PERF_RESOURCE_GROUP} does not exist in subscription ${SUBSCRIPTION_ID}"

    log "Reusing the perf harness stack in ${PERF_RESOURCE_GROUP}"
    resolve_perf_resource PERF_AKS_CLUSTER "AKS cluster" \
        Microsoft.ContainerService/managedClusters
    resolve_perf_resource PERF_ACR_NAME "container registry" \
        Microsoft.ContainerRegistry/registries
    resolve_perf_resource PERF_GRAFANA_NAME "Grafana workspace" \
        Microsoft.Dashboard/grafana
    resolve_perf_resource PERF_MANAGED_IDENTITY "managed identity" \
        Microsoft.ManagedIdentity/userAssignedIdentities

    # Everything below this point is written against the standalone variable
    # names, so point those at the discovered resources rather than branching
    # every create. The standalone defaults are kept so that values which merely
    # followed them (and were not set explicitly) can follow the move too.
    STANDALONE_RESOURCE_GROUP="${RESOURCE_GROUP}"
    STANDALONE_LOCATION="${LOCATION}"

    RESOURCE_GROUP="${PERF_RESOURCE_GROUP}"
    AKS_CLUSTER="${PERF_AKS_CLUSTER}"
    ACR_NAME="${PERF_ACR_NAME}"
    GRAFANA_NAME="${PERF_GRAFANA_NAME}"
    MANAGED_IDENTITY="${PERF_MANAGED_IDENTITY}"
    LOCATION="$(az group show --name "${RESOURCE_GROUP}" --query location -o tsv)"

    if [[ "${COSMOS_ACCOUNT_RESOURCE_GROUP}" == "${STANDALONE_RESOURCE_GROUP}" ]]; then
        COSMOS_ACCOUNT_RESOURCE_GROUP="${RESOURCE_GROUP}"
    fi
    if [[ "${MONITOR_LOCATION}" == "${STANDALONE_LOCATION}" ]]; then
        MONITOR_LOCATION="${LOCATION}"
    fi
    if [[ "${GRAFANA_LOCATION}" == "${STANDALONE_LOCATION}" ]]; then
        GRAFANA_LOCATION="${LOCATION}"
    fi
fi

if $DRY_RUN; then
    if $ATTACH_TO_PERF; then
        cat <<EOF
Would attach to the perf harness in subscription ${SUBSCRIPTION_ID}:

  reusing (not created)
    resource group      ${RESOURCE_GROUP} (${LOCATION})
    container registry  ${ACR_NAME}
    aks cluster         ${AKS_CLUSTER}
    managed grafana     ${GRAFANA_NAME}
    managed identity    ${MANAGED_IDENTITY}

  creating
    monitor workspace   ${MONITOR_WORKSPACE} (${MONITOR_LOCATION})
    prometheus addon    on ${AKS_CLUSTER}, wired to ${GRAFANA_NAME}
    aks node pool       ${SOAK_NODE_POOL} (${SOAK_NODE_COUNT} x ${SOAK_NODE_SIZE}),
                        tainted ${SOAK_NODE_TAINT_KEY}=${SOAK_NODE_TAINT_VALUE}:NoSchedule
    cosmos account      ${COSMOS_ACCOUNT} $($SKIP_COSMOS && echo '(skipped)')
    federated cred      ${NAMESPACE}/cosmos-obs-soak on ${MANAGED_IDENTITY}
EOF
    else
        cat <<EOF
Would provision into subscription ${SUBSCRIPTION_ID}:

  resource group        ${RESOURCE_GROUP} (${LOCATION})
  container registry    ${ACR_NAME}
  monitor workspace     ${MONITOR_WORKSPACE} (${MONITOR_LOCATION})
  managed grafana       ${GRAFANA_NAME} (${GRAFANA_LOCATION})
  aks cluster           ${AKS_CLUSTER} (${AKS_NODE_COUNT} x ${AKS_NODE_SIZE})
  aks node pool         ${SOAK_NODE_POOL}
  cosmos account        ${COSMOS_ACCOUNT} $($SKIP_COSMOS && echo '(skipped)')
  managed identity      ${MANAGED_IDENTITY}
EOF
    fi
    exit 0
fi

az_select_subscription

# The Managed Grafana commands live in an extension. Installing it up front
# avoids failing three quarters of the way through provisioning.
if ! az extension show --name amg >/dev/null 2>&1; then
    log "Installing the 'amg' (Azure Managed Grafana) CLI extension"
    az extension add --name amg --only-show-errors
fi

# --- Resource group ----------------------------------------------------------

if ! $ATTACH_TO_PERF; then
    log "Resource group ${RESOURCE_GROUP}"
    az group create \
        --name "${RESOURCE_GROUP}" \
        --location "${LOCATION}" \
        --tags purpose=cosmos-rust-sdk-observability-soak \
        --only-show-errors -o none
fi

# --- Container registry ------------------------------------------------------

log "Container registry ${ACR_NAME}"
if ! az acr show --name "${ACR_NAME}" --resource-group "${RESOURCE_GROUP}" >/dev/null 2>&1; then
    # A bare `$ATTACH_TO_PERF && die ...` would return 1 in standalone mode and
    # `set -e` would take that as a failure, so the guard is spelled out.
    if $ATTACH_TO_PERF; then
        die "container registry ${ACR_NAME} not found in ${RESOURCE_GROUP}"
    fi
    # Admin user stays off: AKS pulls with its kubelet identity via --attach-acr,
    # so there is no registry password to leak or rotate.
    az acr create \
        --name "${ACR_NAME}" \
        --resource-group "${RESOURCE_GROUP}" \
        --location "${LOCATION}" \
        --sku Basic \
        --admin-enabled false \
        --only-show-errors -o none
fi
ACR_LOGIN_SERVER="$(az acr show --name "${ACR_NAME}" --resource-group "${RESOURCE_GROUP}" \
    --query loginServer -o tsv)"

# --- Azure Monitor workspace (managed Prometheus) ----------------------------

log "Azure Monitor workspace ${MONITOR_WORKSPACE}"
if ! az monitor account show --name "${MONITOR_WORKSPACE}" \
    --resource-group "${RESOURCE_GROUP}" >/dev/null 2>&1; then
    az monitor account create \
        --name "${MONITOR_WORKSPACE}" \
        --resource-group "${RESOURCE_GROUP}" \
        --location "${MONITOR_LOCATION}" \
        --only-show-errors -o none
fi
MONITOR_WORKSPACE_ID="$(az monitor account show --name "${MONITOR_WORKSPACE}" \
    --resource-group "${RESOURCE_GROUP}" --query id -o tsv)"

# --- Azure Managed Grafana ---------------------------------------------------

log "Azure Managed Grafana ${GRAFANA_NAME}"
if ! az grafana show --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" >/dev/null 2>&1; then
    if $ATTACH_TO_PERF; then
        die "Grafana workspace ${GRAFANA_NAME} not found in ${RESOURCE_GROUP}"
    fi
    az grafana create \
        --name "${GRAFANA_NAME}" \
        --resource-group "${RESOURCE_GROUP}" \
        --location "${GRAFANA_LOCATION}" \
        --only-show-errors -o none
fi
GRAFANA_ID="$(az grafana show --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" --query id -o tsv)"
GRAFANA_ENDPOINT="$(az grafana show --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" --query properties.endpoint -o tsv)"

# --- AKS ---------------------------------------------------------------------

log "AKS cluster ${AKS_CLUSTER}"
if ! az aks show --name "${AKS_CLUSTER}" --resource-group "${RESOURCE_GROUP}" >/dev/null 2>&1; then
    if $ATTACH_TO_PERF; then
        die "AKS cluster ${AKS_CLUSTER} not found in ${RESOURCE_GROUP}"
    fi
    # --enable-azure-monitor-metrics turns on the managed Prometheus addon and
    # wires the data source into Grafana in one step, which is why the workspace
    # and Grafana instance have to exist before the cluster.
    #
    # The initial pool is named SOAK_NODE_POOL so the manifests' nodeSelector is
    # the same expression in both standalone and shared-cluster deployments.
    az aks create \
        --name "${AKS_CLUSTER}" \
        --resource-group "${RESOURCE_GROUP}" \
        --location "${LOCATION}" \
        --nodepool-name "${SOAK_NODE_POOL}" \
        --node-count "${AKS_NODE_COUNT}" \
        --node-vm-size "${AKS_NODE_SIZE}" \
        --enable-managed-identity \
        --enable-oidc-issuer \
        --enable-workload-identity \
        --enable-azure-monitor-metrics \
        --azure-monitor-workspace-resource-id "${MONITOR_WORKSPACE_ID}" \
        --grafana-resource-id "${GRAFANA_ID}" \
        --attach-acr "${ACR_NAME}" \
        --generate-ssh-keys \
        --only-show-errors -o none
else
    # An existing cluster may predate any of these; enabling them is a no-op when
    # already on. On a shared perf cluster this is the step that matters: the
    # perf harness reports to ADX and has no Prometheus pipeline, so managed
    # Prometheus and its Grafana data source are what the soak actually adds.
    #
    # It is not free for the perf numbers: the addon runs an ama-metrics
    # DaemonSet on *every* node, including the perf nodes, so perf pods lose a
    # small slice of CPU from the moment it is enabled. The concurrency tuner
    # absorbs it within a cycle, but perf results either side of this change are
    # not strictly comparable. Enable it at a point where that is acceptable.
    az aks update \
        --name "${AKS_CLUSTER}" \
        --resource-group "${RESOURCE_GROUP}" \
        --enable-oidc-issuer \
        --enable-workload-identity \
        --only-show-errors -o none
    az aks update \
        --name "${AKS_CLUSTER}" \
        --resource-group "${RESOURCE_GROUP}" \
        --enable-azure-monitor-metrics \
        --azure-monitor-workspace-resource-id "${MONITOR_WORKSPACE_ID}" \
        --grafana-resource-id "${GRAFANA_ID}" \
        --only-show-errors -o none
    az aks update \
        --name "${AKS_CLUSTER}" \
        --resource-group "${RESOURCE_GROUP}" \
        --attach-acr "${ACR_NAME}" \
        --only-show-errors -o none
fi

# --- Soak node pool ----------------------------------------------------------

# The soak never shares a node with the perf harness. Perf pins one pod per node
# and tunes it to ~80% CPU, so a co-scheduled soak pod would both distort perf's
# measurement and pick up the contention as latency noise of its own. A small
# dedicated pool costs one node and keeps both datasets honest.
log "Node pool ${SOAK_NODE_POOL}"
if ! az aks nodepool show \
    --cluster-name "${AKS_CLUSTER}" \
    --resource-group "${RESOURCE_GROUP}" \
    --name "${SOAK_NODE_POOL}" >/dev/null 2>&1; then
    az aks nodepool add \
        --cluster-name "${AKS_CLUSTER}" \
        --resource-group "${RESOURCE_GROUP}" \
        --name "${SOAK_NODE_POOL}" \
        --mode User \
        --node-count "${SOAK_NODE_COUNT}" \
        --node-vm-size "${SOAK_NODE_SIZE}" \
        --node-taints "${SOAK_NODE_TAINT_KEY}=${SOAK_NODE_TAINT_VALUE}:NoSchedule" \
        --labels "workload=soak" \
        --only-show-errors -o none
fi

OIDC_ISSUER="$(az aks show --name "${AKS_CLUSTER}" --resource-group "${RESOURCE_GROUP}" \
    --query oidcIssuerProfile.issuerUrl -o tsv)"
[[ -n "${OIDC_ISSUER}" ]] || die "AKS OIDC issuer is empty; workload identity cannot be federated"

# --- Cosmos DB account -------------------------------------------------------

if ! $SKIP_COSMOS; then
    log "Cosmos DB account ${COSMOS_ACCOUNT}"
    if ! az cosmosdb show --name "${COSMOS_ACCOUNT}" \
        --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" >/dev/null 2>&1; then
        # Session consistency and a single region: the soak measures SDK
        # behavior, so the account should not introduce multi-region variance
        # that would be misread as an SDK regression.
        az cosmosdb create \
            --name "${COSMOS_ACCOUNT}" \
            --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" \
            --locations regionName="${LOCATION}" failoverPriority=0 isZoneRedundant=False \
            --default-consistency-level Session \
            --only-show-errors -o none
    fi
fi

COSMOS_ENDPOINT="$(az cosmosdb show --name "${COSMOS_ACCOUNT}" \
    --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" --query documentEndpoint -o tsv)"
COSMOS_REGION_RESOLVED="$(az cosmosdb show --name "${COSMOS_ACCOUNT}" \
    --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" \
    --query "writeLocations[0].locationName" -o tsv)"

# --- Managed identity + federation + RBAC ------------------------------------

log "Managed identity ${MANAGED_IDENTITY}"
if ! az identity show --name "${MANAGED_IDENTITY}" \
    --resource-group "${RESOURCE_GROUP}" >/dev/null 2>&1; then
    if $ATTACH_TO_PERF; then
        die "managed identity ${MANAGED_IDENTITY} not found in ${RESOURCE_GROUP}"
    fi
    az identity create \
        --name "${MANAGED_IDENTITY}" \
        --resource-group "${RESOURCE_GROUP}" \
        --location "${LOCATION}" \
        --only-show-errors -o none
fi
IDENTITY_CLIENT_ID="$(az identity show --name "${MANAGED_IDENTITY}" \
    --resource-group "${RESOURCE_GROUP}" --query clientId -o tsv)"
IDENTITY_PRINCIPAL_ID="$(az identity show --name "${MANAGED_IDENTITY}" \
    --resource-group "${RESOURCE_GROUP}" --query principalId -o tsv)"

log "Federating ${MANAGED_IDENTITY} with serviceaccount ${NAMESPACE}/cosmos-obs-soak"
# When sharing the perf harness's identity this is an *additional* federated
# credential alongside the perf one: a single identity can be federated with
# many service accounts, so the soak needs no identity of its own and the tenant
# keeps one Cosmos RBAC principal to manage instead of two.
FEDERATED_NAME="cosmos-obs-soak-federation"
if ! az identity federated-credential show \
    --name "${FEDERATED_NAME}" \
    --identity-name "${MANAGED_IDENTITY}" \
    --resource-group "${RESOURCE_GROUP}" >/dev/null 2>&1; then
    az identity federated-credential create \
        --name "${FEDERATED_NAME}" \
        --identity-name "${MANAGED_IDENTITY}" \
        --resource-group "${RESOURCE_GROUP}" \
        --issuer "${OIDC_ISSUER}" \
        --subject "system:serviceaccount:${NAMESPACE}:cosmos-obs-soak" \
        --audiences api://AzureADTokenExchange \
        --only-show-errors -o none
fi

# Cosmos DB Built-in Data Contributor. This is a *data-plane* role assignment
# (`az cosmosdb sql role assignment`), which is separate from Azure RBAC —
# granting a control-plane role such as Contributor does not give the identity
# permission to read or write documents.
log "Granting Cosmos DB Built-in Data Contributor to ${MANAGED_IDENTITY}"
COSMOS_DATA_CONTRIBUTOR="00000000-0000-0000-0000-000000000002"
COSMOS_SCOPE="$(az cosmosdb show --name "${COSMOS_ACCOUNT}" \
    --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" --query id -o tsv)"
if ! az cosmosdb sql role assignment list \
    --account-name "${COSMOS_ACCOUNT}" \
    --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" \
    --query "[?principalId=='${IDENTITY_PRINCIPAL_ID}'] | [0]" -o tsv | grep -q .; then
    az cosmosdb sql role assignment create \
        --account-name "${COSMOS_ACCOUNT}" \
        --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" \
        --role-definition-id "${COSMOS_DATA_CONTRIBUTOR}" \
        --principal-id "${IDENTITY_PRINCIPAL_ID}" \
        --scope "${COSMOS_SCOPE}" \
        --only-show-errors -o none
fi

# --- Summary -----------------------------------------------------------------

cat <<EOF

$(log "Provisioning complete")

Add these to soak.env (or export them) before running ./deploy-soak.sh:

  RESOURCE_GROUP="${RESOURCE_GROUP}"
  AKS_CLUSTER="${AKS_CLUSTER}"
  ACR_NAME="${ACR_NAME}"
  GRAFANA_NAME="${GRAFANA_NAME}"
  SOAK_NODE_POOL="${SOAK_NODE_POOL}"
  ACR_LOGIN_SERVER="${ACR_LOGIN_SERVER}"
  COSMOS_ENDPOINT="${COSMOS_ENDPOINT}"
  COSMOS_REGION="${COSMOS_REGION_RESOLVED}"
  WORKLOAD_IDENTITY_CLIENT_ID="${IDENTITY_CLIENT_ID}"

Grafana: ${GRAFANA_ENDPOINT}

Next:
  ./deploy-soak.sh              build + push the image and start the soak
  ./upload-grafana-dashboard.sh publish the WS9 dashboard
  ./grant-team-access.sh        give the team read access
EOF
