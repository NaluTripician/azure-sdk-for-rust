// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

// Prometheus alert rules for the Cosmos DB Rust SDK observability soak.
//
// Deployed as an Azure Monitor managed Prometheus rule group, which evaluates
// against the same Azure Monitor workspace the dashboard reads from — so an
// alert and a panel can never disagree about what the data says.
//
// Deploy:
//   az deployment group create \
//     --resource-group "$RESOURCE_GROUP" \
//     --template-file alerts/regression-alerts.bicep \
//     --parameters azureMonitorWorkspaceId="$MONITOR_WORKSPACE_ID" \
//                  clusterName="$AKS_CLUSTER" \
//                  actionGroupId="$ACTION_GROUP_ID"
//
// Thresholds are deliberately parameters, not constants: the right p99 for a
// soak depends on the account's region and SKU, so they should be set from an
// observed baseline rather than guessed here. Start by watching the dashboard
// for a few days, then set each threshold ~30% above the steady-state value.

@description('Resource ID of the Azure Monitor workspace the soak writes to.')
param azureMonitorWorkspaceId string

@description('Name of the AKS cluster running the soak. Scopes rules to this cluster so several soaks can share a workspace.')
param clusterName string

@description('Location for the rule group. Must match the Azure Monitor workspace region.')
param location string = resourceGroup().location

@description('Optional action group resource ID to notify. Leave empty to record alerts without notifying anyone.')
param actionGroupId string = ''

@description('Container name used by the steady-state (baseline) workload.')
param steadyContainer string = 'items'

@description('p99 operation duration, in seconds, above which the steady-state workload is considered regressed.')
param p99LatencySecondsThreshold string = '0.5'

@description('Steady-state error rate percentage above which to alert.')
param errorRatePercentThreshold string = '1'

@description('p95 request charge, in RU, above which the steady-state workload is considered regressed.')
param requestChargeRuThreshold string = '10'

var actions = empty(actionGroupId) ? [] : [
  {
    actionGroupId: actionGroupId
  }
]

// Every rule filters on the steady-state container. The fault canary
// deliberately fails requests, so including it would make the error-rate and
// latency alerts fire on schedule and train everyone to ignore them.
var steadySelector = 'db_collection_name="${steadyContainer}", cluster="${clusterName}"'

resource ruleGroup 'Microsoft.AlertsManagement/prometheusRuleGroups@2023-03-01' = {
  name: 'cosmos-rust-sdk-soak-alerts'
  location: location
  properties: {
    description: 'Regression detection for the long-running Cosmos DB Rust SDK observability soak.'
    scopes: [
      azureMonitorWorkspaceId
    ]
    clusterName: clusterName
    interval: 'PT1M'
    rules: [
      // The most important rule in the file. A soak that has stopped renders as
      // a flat, uneventful dashboard — indistinguishable at a glance from a
      // healthy one — so silence has to be treated as a failure, not a pass.
      {
        alert: 'CosmosSoakWorkloadStopped'
        expression: 'sum(rate(db_client_operation_duration_seconds_count{${steadySelector}}[5m])) < 0.01 or absent(db_client_operation_duration_seconds_count{${steadySelector}})'
        for: 'PT10M'
        severity: 2
        labels: {
          workload: 'cosmos-rust-sdk-soak'
        }
        annotations: {
          summary: 'Cosmos Rust SDK soak has stopped producing traffic'
          description: 'No operations recorded for 10 minutes on cluster ${clusterName}. The dashboard is no longer tracking regressions until this is fixed.'
        }
        actions: actions
        resolveConfiguration: {
          autoResolved: true
          timeToResolve: 'PT10M'
        }
      }
      {
        alert: 'CosmosSoakErrorRateHigh'
        expression: '100 * sum(rate(db_client_operation_duration_seconds_count{${steadySelector}, error_type!=""}[10m])) / clamp_min(sum(rate(db_client_operation_duration_seconds_count{${steadySelector}}[10m])), 1e-9) > ${errorRatePercentThreshold}'
        for: 'PT15M'
        severity: 2
        labels: {
          workload: 'cosmos-rust-sdk-soak'
        }
        annotations: {
          summary: 'Cosmos Rust SDK soak error rate above ${errorRatePercentThreshold}%'
          description: 'The steady-state workload injects no faults, so any sustained error rate is a real SDK or service problem. Break down by error_type and db_response_status_code on the WS9 dashboard.'
        }
        actions: actions
        resolveConfiguration: {
          autoResolved: true
          timeToResolve: 'PT15M'
        }
      }
      {
        alert: 'CosmosSoakP99LatencyRegression'
        expression: 'histogram_quantile(0.99, sum by (le) (rate(db_client_operation_duration_seconds_bucket{${steadySelector}}[10m]))) > ${p99LatencySecondsThreshold}'
        for: 'PT30M'
        severity: 3
        labels: {
          workload: 'cosmos-rust-sdk-soak'
        }
        annotations: {
          summary: 'Cosmos Rust SDK soak p99 latency above ${p99LatencySecondsThreshold}s'
          description: 'Sustained for 30 minutes, which rules out a transient service blip. Compare against the same window on previous days before assuming an SDK change caused it.'
        }
        actions: actions
        resolveConfiguration: {
          autoResolved: true
          timeToResolve: 'PT30M'
        }
      }
      {
        // RU charge is the cheapest early warning for an SDK regression that
        // changes request shape (an extra round trip, a lost continuation, a
        // query that stopped using the index) without changing latency enough
        // to notice.
        alert: 'CosmosSoakRequestChargeRegression'
        expression: 'histogram_quantile(0.95, sum by (le) (rate(azure_cosmosdb_client_operation_request_charge_bucket{${steadySelector}}[10m]))) > ${requestChargeRuThreshold}'
        for: 'PT30M'
        severity: 3
        labels: {
          workload: 'cosmos-rust-sdk-soak'
        }
        annotations: {
          summary: 'Cosmos Rust SDK soak p95 request charge above ${requestChargeRuThreshold} RU'
          description: 'A rise in RU per operation with unchanged workload settings usually means the SDK started issuing different requests. Check the request-charge panels on the WS9 dashboard.'
        }
        actions: actions
        resolveConfiguration: {
          autoResolved: true
          timeToResolve: 'PT30M'
        }
      }
      {
        // Guards the guard: if the canary stops producing errors, the "rich on
        // error" diagnostics path is no longer being exercised and a regression
        // there would go unnoticed indefinitely.
        alert: 'CosmosSoakFaultCanarySilent'
        expression: 'sum(rate(db_client_operation_duration_seconds_count{cluster="${clusterName}", db_collection_name=~".*_canary", error_type!=""}[1h])) == 0'
        for: 'PT2H'
        severity: 4
        labels: {
          workload: 'cosmos-rust-sdk-soak'
        }
        annotations: {
          summary: 'Cosmos Rust SDK fault canary has produced no errors for two hours'
          description: 'Fault injection appears to be inactive, so the error-path diagnostics are untested. Check the cosmos-obs-soak-canary deployment and its --fault-* flags.'
        }
        actions: actions
        resolveConfiguration: {
          autoResolved: true
          timeToResolve: 'PT1H'
        }
      }
    ]
  }
}

output ruleGroupId string = ruleGroup.id
