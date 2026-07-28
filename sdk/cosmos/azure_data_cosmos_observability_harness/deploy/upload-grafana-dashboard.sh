#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. All rights reserved.
# Licensed under the MIT License.
#
# Publishes the WS9 observability dashboard into Azure Managed Grafana, bound to
# the managed Prometheus data source that the AKS addon created.
#
# The dashboard JSON is used verbatim from
# `sdk/cosmos/azure_data_cosmos_benchmarks/dashboards/cosmos-observability.json`
# — the same file the local docker-compose stack provisions — so the cloud
# dashboard and a developer's laptop always show the same panels.
#
# Follows the pattern of `rust-perf/deploy/upload-grafana-dashboard.sh` in the
# cosmos-sdk-copilot-toolkit repo: resolve the data source uid, substitute it in,
# preserve the live dashboard's identity, and update in place.
#
# Usage:
#   ./upload-grafana-dashboard.sh
#   ./upload-grafana-dashboard.sh --dashboard /path/to/other.json

# shellcheck source=common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

DASHBOARD_JSON="${REPO_ROOT}/sdk/cosmos/azure_data_cosmos_benchmarks/dashboards/cosmos-observability.json"
while [[ $# -gt 0 ]]; do
    case "$1" in
    --dashboard)
        DASHBOARD_JSON="${2:?--dashboard requires a path}"
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

require_cmd az jq
require_var SUBSCRIPTION_ID
[[ -f "${DASHBOARD_JSON}" ]] || die "dashboard not found: ${DASHBOARD_JSON}"
az_select_subscription

az extension show --name amg >/dev/null 2>&1 || az extension add --name amg --only-show-errors

# --- Resolve the managed Prometheus data source ------------------------------

log "Resolving the Prometheus data source in ${GRAFANA_NAME}"
DS_UID="$(az grafana data-source list \
    --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" \
    --query "[?type=='prometheus'] | [0].uid" -o tsv 2>/dev/null || true)"

if [[ -z "${DS_UID}" || "${DS_UID}" == "null" ]]; then
    die "no Prometheus data source in ${GRAFANA_NAME}.

The AKS managed Prometheus addon creates it when the cluster is linked to
Grafana. Re-run ./provision-soak-infra.sh, or link manually:

  az aks update -n ${AKS_CLUSTER} -g ${RESOURCE_GROUP} \\
      --enable-azure-monitor-metrics \\
      --azure-monitor-workspace-resource-id <workspace-id> \\
      --grafana-resource-id <grafana-id>"
fi
log "Prometheus data source uid: ${DS_UID}"

# --- Prepare the payload -----------------------------------------------------

DASHBOARD_UID="$(jq -r '.uid // "cosmos-rust-ws9"' "${DASHBOARD_JSON}")"

# Reuse the live dashboard's numeric id and current version when it already
# exists. Posting without them creates a *second* copy instead of updating the
# one the team has bookmarked.
EXISTING="$(az grafana dashboard show \
    --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" \
    --dashboard "${DASHBOARD_UID}" -o json 2>/dev/null || echo '')"

EXISTING_ID=null
EXISTING_VERSION=null
if [[ -n "${EXISTING}" ]]; then
    EXISTING_ID="$(printf '%s' "${EXISTING}" | jq -r '.dashboard.id // "null"')"
    EXISTING_VERSION="$(printf '%s' "${EXISTING}" | jq -r '.dashboard.version // "null"')"
    log "Updating existing dashboard (id=${EXISTING_ID}, version=${EXISTING_VERSION})"
else
    log "Creating dashboard ${DASHBOARD_UID}"
fi

PAYLOAD="$(mktemp -t cosmos-ws9-dashboard-XXXXXX.json)"
trap 'rm -f "${PAYLOAD}"' EXIT

# The dashboard's `datasource` template variable is what every panel resolves
# through, so pinning its current value to the managed Prometheus uid is enough
# to make the whole dashboard render without editing individual panels.
jq \
    --arg dsUid "${DS_UID}" \
    --arg env "${SOAK_ENVIRONMENT}" \
    --argjson id "${EXISTING_ID}" \
    --argjson version "${EXISTING_VERSION}" \
    '
    .id = $id
    | .version = $version
    | .templating.list = (
        (.templating.list // [])
        | map(
            if (.type == "datasource") then
              .current = { selected: true, text: "Managed Prometheus", value: $dsUid }
            else . end
          )
      )
    | .tags = ((.tags // []) + ["cosmos", "rust-sdk", $env] | unique)
    ' "${DASHBOARD_JSON}" >"${PAYLOAD}"

log "Publishing to ${GRAFANA_NAME}"
az grafana dashboard update \
    --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" \
    --definition "@${PAYLOAD}" \
    --overwrite true \
    --only-show-errors -o none

GRAFANA_ENDPOINT="$(az grafana show --name "${GRAFANA_NAME}" \
    --resource-group "${RESOURCE_GROUP}" --query properties.endpoint -o tsv)"

cat <<EOF

$(log "Dashboard published")

  ${GRAFANA_ENDPOINT}/d/${DASHBOARD_UID}

Share that link with the team. Grant them access with ./grant-team-access.sh.
EOF
