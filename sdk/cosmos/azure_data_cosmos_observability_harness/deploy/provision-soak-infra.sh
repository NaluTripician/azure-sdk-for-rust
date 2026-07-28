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
#   --skip-cosmos     Do not create a Cosmos account (still assigns RBAC).
#   --dry-run         Print what would be created and exit.

# shellcheck source=common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

SKIP_COSMOS=false
DRY_RUN=false
while [[ $# -gt 0 ]]; do
    case "$1" in
    --skip-cosmos) SKIP_COSMOS=true ;;
    --dry-run) DRY_RUN=true ;;
    -h | --help)
        sed -n '2,30p' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    *) die "unknown option: $1" ;;
    esac
    shift
done

require_cmd az
require_az_version
require_var SUBSCRIPTION_ID

if $DRY_RUN; then
    cat <<EOF
Would provision into subscription ${SUBSCRIPTION_ID}:

  resource group        ${RESOURCE_GROUP} (${LOCATION})
  container registry    ${ACR_NAME}
  monitor workspace     ${MONITOR_WORKSPACE} (${MONITOR_LOCATION})
  managed grafana       ${GRAFANA_NAME} (${GRAFANA_LOCATION})
  aks cluster           ${AKS_CLUSTER} (${AKS_NODE_COUNT} x ${AKS_NODE_SIZE})
  cosmos account        ${COSMOS_ACCOUNT} $($SKIP_COSMOS && echo '(skipped)')
  managed identity      ${MANAGED_IDENTITY}
EOF
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

log "Resource group ${RESOURCE_GROUP}"
az group create \
    --name "${RESOURCE_GROUP}" \
    --location "${LOCATION}" \
    --tags purpose=cosmos-rust-sdk-observability-soak \
    --only-show-errors -o none

# --- Container registry ------------------------------------------------------

log "Container registry ${ACR_NAME}"
if ! az acr show --name "${ACR_NAME}" --resource-group "${RESOURCE_GROUP}" >/dev/null 2>&1; then
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
    # --enable-azure-monitor-metrics turns on the managed Prometheus addon and
    # wires the data source into Grafana in one step, which is why the workspace
    # and Grafana instance have to exist before the cluster.
    az aks create \
        --name "${AKS_CLUSTER}" \
        --resource-group "${RESOURCE_GROUP}" \
        --location "${LOCATION}" \
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
    # already on.
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
