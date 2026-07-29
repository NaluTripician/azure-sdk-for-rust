#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. All rights reserved.
# Licensed under the MIT License.

# cspell:ignore mwarn merror soakpool

# Shared configuration and helpers for the Cosmos observability soak scripts.
#
# Every script in this directory sources this file, which in turn sources
# `soak.env` (git-ignored) for the account-specific values. Copy
# `soak.env.example` to `soak.env` and fill it in once; nothing here hardcodes a
# subscription, account, or secret.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
export SCRIPT_DIR REPO_ROOT

# --- Configuration -----------------------------------------------------------

# shellcheck source=/dev/null
[[ -f "${SCRIPT_DIR}/soak.env" ]] && source "${SCRIPT_DIR}/soak.env"

# Azure targets. No defaults for subscription: guessing which subscription to
# bill is never the right behavior.
: "${SUBSCRIPTION_ID:=}"
: "${RESOURCE_GROUP:=cosmos-rust-obs-soak-rg}"
: "${LOCATION:=westus2}"

# Resource names. ACR names must be globally unique and alphanumeric-only.
: "${ACR_NAME:=cosmosrustobssoakacr}"
: "${AKS_CLUSTER:=cosmos-rust-obs-soak-aks}"
: "${AKS_NODE_COUNT:=2}"
: "${AKS_NODE_SIZE:=Standard_D4s_v5}"
: "${MONITOR_WORKSPACE:=cosmos-rust-obs-soak-amw}"
: "${GRAFANA_NAME:=cosmos-rust-obs-soak-grafana}"
: "${COSMOS_ACCOUNT:=cosmos-rust-obs-soak}"
: "${MANAGED_IDENTITY:=cosmos-obs-soak-identity}"

# Set to point the soak at an existing Cosmos account in another resource group;
# provisioning then only assigns data-plane RBAC instead of creating an account.
: "${COSMOS_ACCOUNT_RESOURCE_GROUP:=${RESOURCE_GROUP}}"

# --- Sharing a cluster with the Cosmos perf harness ---------------------------
#
# `provision-soak-infra.sh --attach-to-perf` reuses the cluster, registry,
# Grafana workspace and managed identity that the Cosmos perf harness
# (`rust-perf/deploy/deploy-k8s-deployments.sh` in the cosmos-sdk-copilot-toolkit
# repo) already stands up, so a new tenant is one deployment rather than two
# parallel stacks. Only what the perf harness has no equivalent of gets created:
# an Azure Monitor workspace, the managed Prometheus addon, a dedicated node
# pool, and the soak's own Cosmos account.
#
# Set PERF_RESOURCE_GROUP; the rest is discovered from it when left empty.
: "${PERF_RESOURCE_GROUP:=}"
: "${PERF_AKS_CLUSTER:=}"
: "${PERF_ACR_NAME:=}"
: "${PERF_GRAFANA_NAME:=}"
: "${PERF_MANAGED_IDENTITY:=}"

# The label the perf harness puts on its pods. The soak steers away from nodes
# carrying it (see SOAK_NODE_POOL below).
: "${PERF_POD_LABEL:=cosmos-perf}"

# Node pool the soak runs on. The perf harness pins exactly one perf pod per node
# with a required pod anti-affinity, then a CronJob tunes each pod's concurrency
# until it sits at ~80% CPU. A soak pod sharing those nodes would take CPU from a
# deliberately CPU-saturated measurement: perf latency inflates, the tuner reacts
# by cutting concurrency, and the soak's own latency picks up the contention.
# Both datasets get corrupted, so the soak always gets its own pool.
#
# Named the same in both modes so the manifests need no conditionals: standalone
# provisioning names the cluster's initial pool this, and --attach-to-perf adds
# it to the perf cluster tainted so nothing else lands on it.
: "${SOAK_NODE_POOL:=soakpool}"
: "${SOAK_NODE_SIZE:=Standard_D2s_v5}"
: "${SOAK_NODE_COUNT:=1}"
: "${SOAK_NODE_TAINT_KEY:=workload}"
: "${SOAK_NODE_TAINT_VALUE:=soak}"

# Application region for proximity routing. The harness expects an Azure region
# *display* name ("West US 2"), not a slug, so `deploy-soak.sh` reads the
# account's own write-region name rather than guessing from LOCATION.
: "${COSMOS_REGION:=}"

# Azure Monitor workspaces and Managed Grafana are not available in every region
# AKS is. Defaults follow LOCATION but can be pinned independently.
: "${MONITOR_LOCATION:=${LOCATION}}"
: "${GRAFANA_LOCATION:=${LOCATION}}"

# Workload shape.
: "${NAMESPACE:=cosmos-observability-soak}"
: "${IMAGE_REPOSITORY:=cosmos-observability-harness}"
: "${IMAGE_TAG:=latest}"
# Pinned rather than `latest` so a collector release can never change what the
# dashboard sees without someone choosing it. Bump deliberately and re-verify
# the `prometheus` exporter still emits the same series.
: "${OTEL_COLLECTOR_VERSION:=0.157.0}"
: "${SOAK_ENVIRONMENT:=soak}"

: "${COSMOS_DATABASE:=observability_soak}"
: "${COSMOS_CONTAINER:=items}"
: "${COSMOS_THROUGHPUT:=400}"
: "${SEED_COUNT:=500}"
: "${CONCURRENCY:=8}"
# Rate-limited on purpose. An unthrottled soak saturates the container's RU
# budget, and 429-driven latency drowns out the SDK-level regressions this
# dashboard exists to catch.
: "${TARGET_RPS:=20}"
: "${READ_WEIGHT:=70}"
: "${WRITE_WEIGHT:=20}"
: "${QUERY_WEIGHT:=10}"
: "${METRIC_EXPORT_INTERVAL_SECS:=15}"

# Fault canary: a 15-minute cycle with a 2-minute fault window starting at 13
# minutes, so ~13% of canary traffic exercises the error path.
: "${FAULT_CANARY_REPLICAS:=1}"
: "${FAULT_CANARY_CONCURRENCY:=4}"
: "${FAULT_CANARY_RPS:=5}"
: "${FAULT_CYCLE_SECS:=900}"
: "${FAULT_START_SECS:=780}"
: "${FAULT_DURATION_SECS:=120}"
: "${FAULT_PROBABILITY:=0.25}"
: "${FAULT_ERROR:=service-unavailable}"

# Cluster infrastructure scrape targets. These are billed per ingested sample
# and, unlike the pod-annotation job, they are NOT namespace-scoped -- they
# scrape the whole cluster, so on a shared cluster they pull in every other
# team's pods too. Defaults are deliberately lean: keep what tells you the soak
# itself is healthy, drop what only describes the nodes underneath it.
#
#   cadvisor     container CPU/memory -> catches an SDK memory leak over weeks.
#                Highest cardinality of the four; the one to drop first if the
#                bill matters more than leak detection.
#   kubestate    pod restarts / deployment availability -> tells you the soak died.
#   collectorhealth  scrape-pipeline liveness. Tiny, and without it a broken
#                scrape looks identical to a healthy-but-idle workload.
#   kubelet      kubelet's own operational metrics. Not about our workload.
#   nodeexporter node OS metrics. Irrelevant to SDK regression tracking.
#
# See "Cost" in README.md for measured per-target estimates.
: "${SCRAPE_CADVISOR:=true}"
: "${SCRAPE_KUBESTATE:=true}"
: "${SCRAPE_COLLECTOR_HEALTH:=true}"
: "${SCRAPE_KUBELET:=false}"
: "${SCRAPE_NODEEXPORTER:=false}"

# Optional Application Insights connection string for the traces pipeline. When
# empty the collector's traces pipeline terminates in `nop`.
: "${APPLICATIONINSIGHTS_CONNECTION_STRING:=}"

# Entra group that gets read access to the dashboard. Resolved by
# `grant-team-access.sh`; can be a group object id or display name.
: "${TEAM_ENTRA_GROUP:=}"
: "${TEAM_GRAFANA_ROLE:=Grafana Viewer}"

export SUBSCRIPTION_ID RESOURCE_GROUP LOCATION ACR_NAME AKS_CLUSTER \
    AKS_NODE_COUNT AKS_NODE_SIZE MONITOR_WORKSPACE GRAFANA_NAME COSMOS_ACCOUNT \
    MANAGED_IDENTITY COSMOS_ACCOUNT_RESOURCE_GROUP COSMOS_REGION \
    MONITOR_LOCATION GRAFANA_LOCATION NAMESPACE \
    IMAGE_REPOSITORY IMAGE_TAG OTEL_COLLECTOR_VERSION SOAK_ENVIRONMENT \
    COSMOS_DATABASE COSMOS_CONTAINER COSMOS_THROUGHPUT SEED_COUNT CONCURRENCY \
    TARGET_RPS READ_WEIGHT WRITE_WEIGHT QUERY_WEIGHT METRIC_EXPORT_INTERVAL_SECS \
    FAULT_CANARY_REPLICAS FAULT_CANARY_CONCURRENCY FAULT_CANARY_RPS \
    FAULT_CYCLE_SECS FAULT_START_SECS FAULT_DURATION_SECS FAULT_PROBABILITY \
    FAULT_ERROR APPLICATIONINSIGHTS_CONNECTION_STRING TEAM_ENTRA_GROUP \
    TEAM_GRAFANA_ROLE SCRAPE_CADVISOR SCRAPE_KUBESTATE SCRAPE_COLLECTOR_HEALTH \
    SCRAPE_KUBELET SCRAPE_NODEEXPORTER \
    PERF_RESOURCE_GROUP PERF_AKS_CLUSTER PERF_ACR_NAME PERF_GRAFANA_NAME \
    PERF_MANAGED_IDENTITY PERF_POD_LABEL SOAK_NODE_POOL SOAK_NODE_SIZE \
    SOAK_NODE_COUNT SOAK_NODE_TAINT_KEY SOAK_NODE_TAINT_VALUE

# --- Helpers -----------------------------------------------------------------

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die() {
    printf '\033[1;31merror:\033[0m %s\n' "$*" >&2
    exit 1
}

require_cmd() {
    for cmd in "$@"; do
        command -v "$cmd" >/dev/null 2>&1 || die "'$cmd' is required but not on PATH"
    done
}

require_var() {
    for name in "$@"; do
        [[ -n "${!name:-}" ]] ||
            die "$name is not set. Copy soak.env.example to soak.env and fill it in."
    done
}

# Minimum Azure CLI version. `az monitor account` (Azure Monitor workspaces) and
# the AKS managed-Prometheus flags (--enable-azure-monitor-metrics,
# --azure-monitor-workspace-resource-id, --grafana-resource-id) do not exist in
# older builds, and the failure there is a bare "unrecognized arguments" that
# gives no hint the CLI is the problem.
AZ_MIN_VERSION="2.60.0"

require_az_version() {
    local current
    current="$(az version --output tsv --query '"azure-cli"' 2>/dev/null)" ||
        die "could not determine the Azure CLI version; is 'az' installed?"

    # Sort the two versions and check the minimum stayed first.
    if [[ "$(printf '%s\n%s\n' "${AZ_MIN_VERSION}" "${current}" |
        sort -V | head -n1)" != "${AZ_MIN_VERSION}" ]]; then
        die "Azure CLI ${AZ_MIN_VERSION}+ is required (found ${current}). Run 'az upgrade'."
    fi
}

# Selects the target subscription for every subsequent `az` call in this shell.
az_select_subscription() {
    require_var SUBSCRIPTION_ID
    az account set --subscription "${SUBSCRIPTION_ID}" ||
        die "could not select subscription ${SUBSCRIPTION_ID}; run 'az login' first"
}

# Renders a manifest's ${VAR} placeholders. Restricted to the variables this
# deployment actually defines so that shell syntax inside the templates (for
# example a collector `$${...}` escape) survives untouched.
render_manifest() {
    local file="$1"
    envsubst "$(printf '${%s} ' \
        NAMESPACE AKS_CLUSTER COMMIT_SHA SOAK_ENVIRONMENT TRACE_EXPORTERS \
        APPLICATIONINSIGHTS_CONNECTION_STRING WORKLOAD_IDENTITY_CLIENT_ID \
        ACR_LOGIN_SERVER IMAGE_REPOSITORY IMAGE_TAG OTEL_COLLECTOR_VERSION \
        COLLECTOR_CONFIG_CHECKSUM COSMOS_ENDPOINT COSMOS_REGION COSMOS_DATABASE \
        COSMOS_CONTAINER COSMOS_THROUGHPUT SEED_COUNT CONCURRENCY TARGET_RPS \
        READ_WEIGHT WRITE_WEIGHT QUERY_WEIGHT METRIC_EXPORT_INTERVAL_SECS \
        FAULT_CANARY_REPLICAS FAULT_CANARY_CONCURRENCY FAULT_CANARY_RPS \
        FAULT_CYCLE_SECS FAULT_START_SECS FAULT_DURATION_SECS \
        FAULT_PROBABILITY FAULT_ERROR SCRAPE_CADVISOR SCRAPE_KUBESTATE \
        SCRAPE_COLLECTOR_HEALTH SCRAPE_KUBELET SCRAPE_NODEEXPORTER \
        PERF_POD_LABEL SOAK_NODE_POOL SOAK_NODE_TAINT_KEY \
        SOAK_NODE_TAINT_VALUE)" <"$file"
}
