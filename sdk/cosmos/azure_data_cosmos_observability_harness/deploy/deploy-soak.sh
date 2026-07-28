#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. All rights reserved.
# Licensed under the MIT License.
#
# Builds the harness image, pushes it to ACR, and (re)starts the soak on AKS.
#
# The image is built *in ACR* from a `git archive` of the current commit, so:
#   - no local Docker daemon is required,
#   - the build context contains only committed files (no 10 GB `target/`),
#   - the image is reproducible from the SHA it is tagged with.
#
# Usage:
#   ./deploy-soak.sh                 build from HEAD and roll out
#   ./deploy-soak.sh --no-build      redeploy manifests with the current image
#   ./deploy-soak.sh --tag v1.2.3    build and tag explicitly
#   ./deploy-soak.sh --local         build with the local Docker daemon instead

# shellcheck source=common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

DO_BUILD=true
USE_LOCAL_DOCKER=false
while [[ $# -gt 0 ]]; do
    case "$1" in
    --no-build) DO_BUILD=false ;;
    --local) USE_LOCAL_DOCKER=true ;;
    --tag)
        IMAGE_TAG="${2:?--tag requires a value}"
        shift
        ;;
    -h | --help)
        sed -n '2,20p' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    *) die "unknown option: $1" ;;
    esac
    shift
done

require_cmd az kubectl envsubst git
require_var SUBSCRIPTION_ID
az_select_subscription

# --- Resolve values produced by provisioning ---------------------------------
#
# Looked up from Azure when not already set, so a fresh clone only needs
# SUBSCRIPTION_ID in soak.env.

: "${ACR_LOGIN_SERVER:=$(az acr show --name "${ACR_NAME}" --resource-group "${RESOURCE_GROUP}" \
    --query loginServer -o tsv 2>/dev/null || true)}"
[[ -n "${ACR_LOGIN_SERVER}" ]] || die "ACR ${ACR_NAME} not found; run ./provision-soak-infra.sh first"

: "${COSMOS_ENDPOINT:=$(az cosmosdb show --name "${COSMOS_ACCOUNT}" \
    --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" --query documentEndpoint -o tsv 2>/dev/null || true)}"
[[ -n "${COSMOS_ENDPOINT}" ]] || die "Cosmos account ${COSMOS_ACCOUNT} not found; run ./provision-soak-infra.sh first"

: "${COSMOS_REGION:=$(az cosmosdb show --name "${COSMOS_ACCOUNT}" \
    --resource-group "${COSMOS_ACCOUNT_RESOURCE_GROUP}" \
    --query "writeLocations[0].locationName" -o tsv 2>/dev/null || true)}"

: "${WORKLOAD_IDENTITY_CLIENT_ID:=$(az identity show --name "${MANAGED_IDENTITY}" \
    --resource-group "${RESOURCE_GROUP}" --query clientId -o tsv 2>/dev/null || true)}"
[[ -n "${WORKLOAD_IDENTITY_CLIENT_ID}" ]] ||
    die "managed identity ${MANAGED_IDENTITY} not found; run ./provision-soak-infra.sh first"

COMMIT_SHA="$(git -C "${REPO_ROOT}" rev-parse --short HEAD)"
if [[ -n "$(git -C "${REPO_ROOT}" status --porcelain)" ]]; then
    # The image is built from committed content only, so an uncommitted change
    # would silently not be in the image it is about to be blamed for.
    warn "working tree is dirty; the image is built from HEAD (${COMMIT_SHA}) and will not include uncommitted changes"
fi
[[ "${IMAGE_TAG}" == "latest" ]] && IMAGE_TAG="${COMMIT_SHA}"

# Traces only have somewhere to go when Application Insights is configured.
if [[ -n "${APPLICATIONINSIGHTS_CONNECTION_STRING}" ]]; then
    TRACE_EXPORTERS="azuremonitor"
else
    TRACE_EXPORTERS="nop"
fi

export ACR_LOGIN_SERVER COSMOS_ENDPOINT COSMOS_REGION WORKLOAD_IDENTITY_CLIENT_ID \
    COMMIT_SHA IMAGE_TAG TRACE_EXPORTERS

# --- Build -------------------------------------------------------------------

DOCKERFILE_REL="sdk/cosmos/azure_data_cosmos_observability_harness/deploy/Dockerfile"

if $DO_BUILD; then
    if $USE_LOCAL_DOCKER; then
        require_cmd docker
        log "Building ${IMAGE_REPOSITORY}:${IMAGE_TAG} locally"
        az acr login --name "${ACR_NAME}" --only-show-errors
        docker build \
            --file "${REPO_ROOT}/${DOCKERFILE_REL}" \
            --tag "${ACR_LOGIN_SERVER}/${IMAGE_REPOSITORY}:${IMAGE_TAG}" \
            "${REPO_ROOT}"
        docker push "${ACR_LOGIN_SERVER}/${IMAGE_REPOSITORY}:${IMAGE_TAG}"
    else
        CONTEXT_ARCHIVE="$(mktemp -t cosmos-soak-context-XXXXXX).tar.gz"
        # shellcheck disable=SC2064  # expand CONTEXT_ARCHIVE now, not at trap time
        trap "rm -f '${CONTEXT_ARCHIVE}'" EXIT
        log "Packing build context from ${COMMIT_SHA}"
        git -C "${REPO_ROOT}" archive --format=tar.gz -o "${CONTEXT_ARCHIVE}" HEAD

        log "Building ${IMAGE_REPOSITORY}:${IMAGE_TAG} in ACR ${ACR_NAME}"
        az acr build \
            --registry "${ACR_NAME}" \
            --resource-group "${RESOURCE_GROUP}" \
            --image "${IMAGE_REPOSITORY}:${IMAGE_TAG}" \
            --image "${IMAGE_REPOSITORY}:latest" \
            --file "${DOCKERFILE_REL}" \
            "${CONTEXT_ARCHIVE}"
    fi
else
    log "Skipping build; deploying ${IMAGE_REPOSITORY}:${IMAGE_TAG}"
fi

# --- Cluster credentials -----------------------------------------------------

log "Fetching credentials for ${AKS_CLUSTER}"
az aks get-credentials \
    --name "${AKS_CLUSTER}" \
    --resource-group "${RESOURCE_GROUP}" \
    --overwrite-existing \
    --only-show-errors

# --- Managed Prometheus scrape configuration ---------------------------------

log "Enabling managed Prometheus pod-annotation scraping for ${NAMESPACE}"
render_manifest "${SCRIPT_DIR}/ama-metrics-settings-configmap.yaml" | kubectl apply -f -
# The ama-metrics agent reads its ConfigMap at startup only, so a config change
# is inert until the agent restarts. Names differ across addon versions; a
# missing one is not fatal.
kubectl rollout restart deployment/ama-metrics -n kube-system 2>/dev/null || true
kubectl rollout restart daemonset/ama-metrics-node -n kube-system 2>/dev/null || true

# --- Collector + workload ----------------------------------------------------

log "Applying collector configuration"
kubectl create namespace "${NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f -
RENDERED_COLLECTOR="$(render_manifest "${SCRIPT_DIR}/otel-collector-config.yaml")"
printf '%s\n' "${RENDERED_COLLECTOR}" | kubectl apply -f -

# Pods reference this checksum so a config-only change still triggers a rollout.
COLLECTOR_CONFIG_CHECKSUM="$(printf '%s' "${RENDERED_COLLECTOR}" | sha256sum | cut -c1-16)"
export COLLECTOR_CONFIG_CHECKSUM

log "Deploying soak workloads (image tag ${IMAGE_TAG})"
render_manifest "${SCRIPT_DIR}/soak-deployment.yaml" | kubectl apply -f -

log "Waiting for rollout"
kubectl rollout status deployment/otel-collector -n "${NAMESPACE}" --timeout=5m
kubectl rollout status deployment/cosmos-obs-soak-steady -n "${NAMESPACE}" --timeout=5m
if [[ "${FAULT_CANARY_REPLICAS}" != "0" ]]; then
    kubectl rollout status deployment/cosmos-obs-soak-canary -n "${NAMESPACE}" --timeout=5m
fi

cat <<EOF

$(log "Soak is running")

  namespace   ${NAMESPACE}
  image       ${ACR_LOGIN_SERVER}/${IMAGE_REPOSITORY}:${IMAGE_TAG}
  account     ${COSMOS_ENDPOINT}
  region      ${COSMOS_REGION}

Verify:
  kubectl logs -n ${NAMESPACE} -l soak-role=steady --tail=50 -f
  kubectl exec -n ${NAMESPACE} deploy/otel-collector -- \\
      wget -qO- localhost:8889/metrics | grep db_client_operation_duration

Metrics reach Grafana a few minutes after the first scrape. Publish the
dashboard with ./upload-grafana-dashboard.sh if you have not already.
EOF
