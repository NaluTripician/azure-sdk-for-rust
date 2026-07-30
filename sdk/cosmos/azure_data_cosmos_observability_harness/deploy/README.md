<!--
Copyright (c) Microsoft Corporation. All rights reserved.
Licensed under the MIT License.
-->
<!-- cSpell:ignore agentpool nodepool soakpool uids -->

# Long-running Cosmos DB Rust SDK observability soak

Runs the WS9 observability harness continuously in Azure against a real Cosmos DB
account and publishes the results to a persistent, team-accessible Grafana
dashboard, so latency, RU, and error-rate regressions show up as a trend line
instead of being discovered by a customer.

This is the cloud counterpart of the local `docker compose` stack in
[`../../azure_data_cosmos_benchmarks/observability`](../../azure_data_cosmos_benchmarks/observability).
Both render the exact same dashboard, so what you debug on a laptop and what the
team watches in Azure are never out of sync.

## Architecture

```text
AKS  nodepool/soakpool          dedicated, tainted — never shares a node with perf
 └─ ns/cosmos-observability-soak
     ├─ cosmos-obs-soak-steady    baseline: never stops, never injects faults
     ├─ cosmos-obs-soak-canary    recurring fault windows, separate container
     └─ otel-collector            OTLP in → Prometheus exporter :8889
                 ▲ scraped by
Azure Monitor Managed Prometheus (AKS addon)
 └─ Azure Monitor workspace       long metric retention → regression history
        └─ Azure Managed Grafana  Entra SSO, team access by security group
               └─ cosmos-observability.json (uid `cosmos-rust-ws9`)
```

Everything on the storage and presentation side is managed. There is no
Prometheus or Grafana instance to patch, back up, or wake up for at 2am — which
matters for something meant to run unattended for months.

The cluster, registry, Grafana workspace and identity can all be shared with the
Cosmos perf harness — see [Sharing the Cosmos perf harness's
cluster](#sharing-the-cosmos-perf-harnesss-cluster). The whole deployment is
designed to be stood up in a tenant that gets recycled every few months; see
[Moving to a new tenant](#moving-to-a-new-tenant).

### Why two workloads

`cosmos-obs-soak-steady` is the regression baseline. It runs with
`--duration-secs 0` and no fault injection, so any movement in its p99 or RU
charge is a real signal.

`cosmos-obs-soak-canary` runs bounded cycles with a fault window near the end of
each one. Because the pod restarts when the run finishes, a single
`--fault-start-secs` window becomes a recurring one. Without it the error-path
diagnostics would only ever be exercised by accident, and a regression in the
"rich on error" behavior could sit undetected behind a permanently-green
dashboard.

The canary writes to `<container>_canary` so its deliberately-degraded numbers
never contaminate the baseline. Every metric carries a `db_collection_name`
label, so the two are separable in any panel or alert.

## Prerequisites

- Azure CLI **2.60.0 or newer**, logged in (`az login`), with permission to
  create resources and assign roles in the target subscription. Older builds lack
  `az monitor account` and the AKS managed-Prometheus flags, and fail with a bare
  "unrecognized arguments" that gives no hint the CLI is the problem —
  `provision-soak-infra.sh` checks the version up front so that cannot happen.
- `kubectl`, `jq`, `envsubst`, `git`, and `bash`.
- No local Docker daemon — the image is built in ACR by default.

## One-time setup

```bash
cd sdk/cosmos/azure_data_cosmos_observability_harness/deploy
cp soak.env.example soak.env      # set SUBSCRIPTION_ID at minimum
./provision-soak-infra.sh --dry-run   # review what will be created
./provision-soak-infra.sh
```

`provision-soak-infra.sh` is idempotent, so a partially-failed run can just be
re-run. It creates the resource group, ACR, Azure Monitor workspace, Azure
Managed Grafana, an AKS cluster with OIDC and workload identity enabled and the
managed Prometheus addon linked, a Cosmos DB account, and a user-assigned managed
identity federated to the pod's service account and granted the **Cosmos DB
Built-in Data Contributor** data-plane role.

It prints a block of values at the end. Add them to `soak.env` (or skip it — the
deploy script looks them up from Azure when they are unset).

If this deployment is going into a tenant that gets recycled — which is the
expected case — decide *now* whether the metric history should live in its own
resource group, because moving an Azure Monitor workspace later is not a thing
you can do. See [What happens to the
history](#what-happens-to-the-history).

### Sharing the Cosmos perf harness's cluster

When the Cosmos DB team's internal Rust perf harness is already deployed — for
example into an ephemeral tenant that gets recreated periodically — the soak can
attach to it instead of standing up a parallel stack, so a new tenant is one
deployment rather than two:

```bash
PERF_RESOURCE_GROUP=<the perf harness's resource group> \
  ./provision-soak-infra.sh --attach-to-perf --dry-run
```

The cluster, container registry, Grafana workspace and managed identity are
discovered from that resource group and reused. Only what the perf harness has no
equivalent of gets created:

| Created | Why the perf harness has no equivalent |
| --- | --- |
| Azure Monitor workspace | Perf results go to ADX; there is no Prometheus store |
| Managed Prometheus addon | Same — and it is what puts a Prometheus datasource in the shared Grafana |
| `soakpool` node pool | Every perf node is deliberately CPU-saturated (see below) |
| Cosmos account | Perf's workload account is under continuous load |
| A second federated credential | One identity, two service accounts — no new principal |

Set `PERF_AKS_CLUSTER`, `PERF_ACR_NAME`, `PERF_GRAFANA_NAME` or
`PERF_MANAGED_IDENTITY` explicitly only if the group holds more than one
candidate; discovery reports the ambiguity rather than guessing.

The two dashboards stay separate — the soak's is PromQL, perf's is KQL over ADX —
but they live in the same Grafana workspace, so the team follows one link and one
set of permissions.

#### Two things to know before attaching

**The soak never shares a node with perf.** The perf harness pins exactly one pod
per node with a required anti-affinity, then a CronJob raises each pod's
concurrency until it sits at ~80% CPU. A soak pod on that node would take CPU
from a deliberately saturated measurement: perf latency inflates, the tuner cuts
concurrency in response, and the soak picks up the contention as latency noise of
its own — both datasets go bad at once. So the soak gets its own small tainted
pool (`SOAK_NODE_POOL`, one `Standard_D2s_v5` by default, ~$70/month) and the pod
specs carry a matching `nodeSelector` and toleration.

**Enabling managed Prometheus perturbs perf once.** The addon runs an
`ama-metrics` DaemonSet on *every* node, including perf's, so perf pods lose a
small slice of CPU from the moment it is turned on. The concurrency tuner absorbs
it within a cycle, but perf results either side of that change are not strictly
comparable. Enable it at a point where a baseline reset is acceptable.

### Using an existing Cosmos account

Set `COSMOS_ACCOUNT` and `COSMOS_ACCOUNT_RESOURCE_GROUP` in `soak.env` and pass
`--skip-cosmos`. RBAC is still assigned so the soak identity can reach it.

Give the soak its own account if you can. Sharing one with another workload means
that workload's traffic shows up as noise in every latency panel, and its RU
consumption can push the soak into 429s that look like an SDK regression.

## Deploy

```bash
./deploy-soak.sh
```

Builds the harness image in ACR from a `git archive` of `HEAD` (committed files
only, no local `target/`), tags it with the short SHA, applies the manifests, and
waits for the rollout. Re-run it any time to pick up a new SDK build; that SHA
becomes the `service_version` label on every metric, so a regression can be
attributed to a specific commit.

```bash
./deploy-soak.sh --no-build     # redeploy manifests only
./deploy-soak.sh --tag v1.2.3   # explicit tag
./deploy-soak.sh --local        # build with a local Docker daemon
```

One piece of what it applies is cluster-wide rather than namespaced:
`ama-metrics-settings-configmap` in `kube-system`, which is what opts the soak's
namespace into pod-annotation scraping. On a shared cluster the script refuses to
overwrite that ConfigMap if another workload owns it — it either finds the soak's
namespace already opted in and leaves it alone, or stops and prints the edit to
make. `--force-scrape-config` overrides, at the cost of changing scraping for
whatever deployed it.

## Publish the dashboard and grant access

```bash
./upload-grafana-dashboard.sh
./grant-team-access.sh --group "<your Entra security group>"
```

`upload-grafana-dashboard.sh` uses
[`cosmos-observability.json`](../../azure_data_cosmos_benchmarks/dashboards/cosmos-observability.json)
verbatim, resolves the managed Prometheus data source uid, and updates in place —
it preserves the live dashboard's id and version, so bookmarks keep working
instead of accumulating duplicate copies. In a Grafana workspace shared with the
perf harness it filters for the Prometheus data source specifically, so it never
binds the panels to perf's Azure Data Explorer source, and both dashboards
coexist under their own uids.

`grant-team-access.sh` assigns **Grafana Viewer** on the Grafana workspace *and*
**Monitoring Data Reader** on the Azure Monitor workspace. Both are needed: with
only the first, the dashboard loads but every panel returns 403, which looks
exactly like a broken soak.

Assign to a group, not to individuals, so joiners and leavers are handled by
group membership.

Grafana Viewer is a **workspace-level** role: on a shared workspace it also grants
the perf harness's dashboards. That is usually what you want for one team, but it
is worth knowing before adding a group that should only see the soak.

## Alerts

```bash
MONITOR_WORKSPACE_ID=$(az monitor account show \
    -n "$MONITOR_WORKSPACE" -g "$MONITOR_RESOURCE_GROUP" --query id -o tsv)
MONITOR_WORKSPACE_LOCATION=$(az monitor account show \
    -n "$MONITOR_WORKSPACE" -g "$MONITOR_RESOURCE_GROUP" --query location -o tsv)

# Deployed alongside the workspace, not the cluster: the rules query the
# workspace, must sit in its region, and should outlive any cluster rebuild.
# location is read from the workspace rather than defaulted from the resource
# group, which can be in a different region than the workspace it holds.
az deployment group create \
    --resource-group "$MONITOR_RESOURCE_GROUP" \
    --template-file alerts/regression-alerts.bicep \
    --parameters azureMonitorWorkspaceId="$MONITOR_WORKSPACE_ID" \
                 clusterName="$AKS_CLUSTER" \
                 location="$MONITOR_WORKSPACE_LOCATION"
```

Five rules ship: workload stopped, error rate high, p99 latency regression,
request-charge regression, and fault-canary-silent.

Thresholds are parameters rather than constants because the right p99 depends on
the account's region and SKU. Watch the dashboard for a few days first, then set
each threshold roughly 30% above the observed steady state. Pass `actionGroupId`
to route notifications somewhere.

## Verify

```bash
kubectl logs -n cosmos-observability-soak -l soak-role=steady --tail=50 -f

# Confirm the collector is exposing series for Prometheus to scrape.
kubectl exec -n cosmos-observability-soak deploy/otel-collector -- \
    wget -qO- localhost:8889/metrics | grep db_client_operation_duration
```

Metrics take a few minutes to appear in Grafana after the first scrape.

## Operate

```bash
# Pause without losing history.
kubectl scale -n cosmos-observability-soak deploy/cosmos-obs-soak-steady --replicas=0

# Change the workload shape: edit soak.env, then
./deploy-soak.sh --no-build

# Disable the fault canary.
FAULT_CANARY_REPLICAS=0 ./deploy-soak.sh --no-build
```

## Cost

Retail US prices, Central US, pulled from the Azure retail price API. Treat the
ingestion rows as estimates — series counts depend on how many pods share the
cluster — and true them up with the query at the end of this section.

| Item | Rate | Monthly |
| --- | --- | --- |
| Azure Managed Grafana, Standard | $0.04207/hr | **~$31** |
| Grafana **Viewer** seats | free, unlimited | **$0** |
| Grafana Editor/Admin seats | $6/user/mo | $6 × editors |
| Azure Monitor metric ingestion | $0.16 / 10M samples | see below |
| Azure Monitor Prometheus queries | $0.001 / 10M samples | ~$0 |
| AKS node, `Standard_D4s_v5` Linux | $0.217/hr | **~$158 each** |
| Cosmos, 400 RU/s provisioned | — | ~$23 |

Viewers being free and unlimited is the part that matters for "accessible to
anyone on the team": add the whole team as Viewers and the Grafana bill does not
move.

**Ingestion.** At a 15s scrape a single series costs ~172.8K samples/month, so
~$0.0028 per series per month. Rough per-target figures on a 4-node cluster:

| Target | Approx. series | Monthly |
| --- | --- | --- |
| The soak's own SDK metrics (both deployments + collector) | ~2,000 | ~$6 |
| `cadvisor` (cluster-wide, 30s) | ~20,000 | ~$28 |
| `kubestate` (cluster-wide, 30s) | ~3,000 | ~$4 |
| `nodeexporter` (30s) | ~4,000 | ~$6 |
| `kubelet` (30s) | ~2,000 | ~$3 |

The infra targets are **cluster-wide, not namespace-scoped** — on a shared
cluster they ingest every other workload's pods too, which is why they dominate
and why the defaults in `common.sh` disable `kubelet` and `nodeexporter`.
`cadvisor` is left on because `container_memory_working_set_bytes` is what
surfaces an SDK memory leak over weeks, which is exactly what a soak is for; set
`SCRAPE_CADVISOR=false` if that is not worth ~$28/month to you.

**Reusing an existing cluster is the single biggest lever.** Attaching to the perf
harness's cluster (`--attach-to-perf`) costs one small node for the soak's own
pool rather than a full dedicated cluster: ~$70/month against ~$317/month for a
dedicated two-node `D4s_v5` pool.

Measure actual ingestion after a week and adjust, rather than trusting the table
above — in Grafana's Explore, against the Prometheus datasource:

```promql
topk(20, count by (__name__) ({__name__!=""}))
```

The defaults keep the workload side small on purpose: `TARGET_RPS=20`, 400 RU/s,
a 15s export interval. The point is a *continuous* signal, not a load test —
raising the request rate mostly buys a larger Azure Monitor bill and 429s that
obscure the SDK behavior you are trying to measure.

## Tear down

```bash
az group delete --name "$RESOURCE_GROUP" --yes --no-wait
```

Deletes everything in that group, including the metric history if the Azure
Monitor workspace lives there. If `MONITOR_RESOURCE_GROUP` points somewhere else
— see [What happens to the history](#what-happens-to-the-history) — the history
and the dashboard survive, which is the point of splitting them out.

**When attached to the perf harness** `RESOURCE_GROUP` is the *perf* group, so
that command would take the perf harness with it. Remove just the soak instead:

```bash
kubectl delete namespace "$NAMESPACE"
az aks nodepool delete --cluster-name "$AKS_CLUSTER" \
  --resource-group "$RESOURCE_GROUP" --name "$SOAK_NODE_POOL"
az cosmosdb delete --name "$COSMOS_ACCOUNT" \
  --resource-group "$COSMOS_ACCOUNT_RESOURCE_GROUP" --yes
az monitor account delete --name "$MONITOR_WORKSPACE" \
  --resource-group "$MONITOR_RESOURCE_GROUP" --yes
```

The managed Prometheus addon is left on the cluster deliberately: disabling it is
a second CPU perturbation for the perf pods, and an addon with nothing annotated
to scrape costs almost nothing.

## Moving to a new tenant

The soak is expected to run in a tenant that gets recycled every few months, so
moving it is a routine operation rather than a disaster. Nothing about the
deployment is tenant-specific except the values in `soak.env`, which is
git-ignored — so a move is "fill in a new `soak.env`, re-run the same three
scripts", not "rebuild the deployment".

### What moves and what does not

| | Carries over | Because |
| --- | --- | --- |
| Manifests, scripts, Dockerfile, dashboard JSON, alert rules | Yes | All in this repo, all parameterized |
| Workload configuration (RPS, weights, fault schedule) | Yes | Defaults in `common.sh`; only overrides live in `soak.env` |
| Resource *names* | Yes | Defaults are tenant-independent |
| Container image | Rebuilt | One `az acr build`, from the same commit |
| Cosmos data | No, and it does not matter | The harness seeds its own container on startup |
| Entra group and role assignments | No | Groups are tenant-scoped; see step 6 |
| **Metric history** | **No** | See [What happens to the history](#what-happens-to-the-history) |
| Grafana URL | No | New workspace, new endpoint — the team's bookmark changes |

### Do it with an overlap, not a gap

Stand the new deployment up *before* tearing the old one down. The soak exists to
produce a continuous trend line, and the fastest way to lose confidence in it is
a fortnight-long hole while someone finds time to redeploy. Running both for a
few days also gives you the one thing a fresh deployment otherwise lacks: a
same-week comparison against known-good numbers, which is how you tell "the new
tenant's account is slower" from "the SDK regressed".

The overlap has one consequence. Two of the names are globally unique across all
of Azure — the container registry (`<name>.azurecr.io`) and the Cosmos account
(`<name>.documents.azure.com`) — so the second deployment cannot reuse them while
the first is alive. Hence `NAME_SUFFIX`:

```bash
NAME_SUFFIX="2"     # -> cosmosrustobssoakacr2, cosmos-rust-obs-soak2
```

Lowercase alphanumeric only; ACR names permit nothing else, and `common.sh`
rejects anything else up front rather than letting `az` fail three resources in.
Bump it each move, or drop it once the old deployment is gone and you want the
plain names back.

### Steps

1. **Get access.** `az login --tenant <new-tenant-id>`, then pick the
   subscription that will be billed and note its id.

2. **Write the new `soak.env`.** Start from `soak.env.example` again rather than
   editing the old file — that way a stale value cannot survive by being
   uncommented and forgotten:

   ```bash
   cp soak.env.example soak.env
   # SUBSCRIPTION_ID   the new subscription
   # NAME_SUFFIX       bump it if the old deployment is still up
   # PERF_RESOURCE_GROUP   only if attaching to the perf harness
   ```

   Carry over any deliberate workload overrides (`TARGET_RPS`, `COSMOS_THROUGHPUT`,
   the `SCRAPE_*` toggles). Changing the workload shape in the same move as the
   tenant makes any difference in the numbers unattributable.

3. **Provision.** Dry-run first — it prints every name it is about to use, which
   is where a forgotten `NAME_SUFFIX` shows up:

   ```bash
   ./provision-soak-infra.sh --dry-run
   ./provision-soak-infra.sh
   ```

   Add `--attach-to-perf` if the perf harness is already in the new tenant. It
   will not be there on day one of a fresh tenant; standalone now and attaching
   later is fine, and is just another run of this script.

4. **Deploy and publish.**

   ```bash
   ./deploy-soak.sh
   ./upload-grafana-dashboard.sh
   ```

5. **Re-apply the alert rules** — see [Alerts](#alerts). Reuse the thresholds you
   settled on in the old tenant only if the new Cosmos account is in the same
   region and SKU; otherwise re-baseline, because a threshold tuned elsewhere
   will either page constantly or never.

6. **Recreate team access.** Entra groups do not cross tenants, so an object id
   copied from the old `soak.env` will not resolve — and a role assignment made
   with a stale id fails loudly rather than silently granting nothing, which is
   the good case. Create or find the group in the new tenant, then:

   ```bash
   ./grant-team-access.sh --group "<group display name>"
   ```

7. **Give the team the new link.** The Grafana endpoint changes. Both
   `provision-soak-infra.sh` and `grant-team-access.sh` print it.

8. **Verify before you delete anything** — see [Verify](#verify). Specifically:
   pods `Running` and on the soak node pool, the collector exposing
   `db_client_operation_duration`, every dashboard panel populated (not just the
   first), and the canary's `_canary` container showing its fault window. Then
   leave it for at least one full fault cycle.

9. **Tear down the old deployment** — see [Tear down](#tear-down). Do this last
   and deliberately; it is what releases the globally-unique names.

### What happens to the history

An Azure Monitor workspace cannot be moved between tenants, and the managed
Prometheus addon can only write to a workspace in the cluster's own tenant. So
when the tenant goes, the metric history goes with it, and the new deployment
starts its trend line at zero. There is no configuration that avoids this.

What you can avoid is losing history to the *much* more frequent event — a
cluster rebuild. The perf harness redeploys far more often than the tenant is
recycled, and if the workspace shares a resource group with the cluster, each
rebuild costs you the trend as surely as a tenant move does. Put the workspace
and Grafana in their own group:

```bash
az group create --name cosmos-rust-obs-history-rg --location westus2

# in soak.env
MONITOR_RESOURCE_GROUP="cosmos-rust-obs-history-rg"
GRAFANA_RESOURCE_GROUP="cosmos-rust-obs-history-rg"
```

Provisioning requires that group to already exist and will not create it, so a
typo cannot quietly strand months of data somewhere nobody looks. Deleting the
cluster's group then leaves the history and the dashboard untouched; re-running
`provision-soak-infra.sh` re-links a new cluster to the same workspace and the
trend continues across the gap.

If continuity across *tenant* boundaries is worth real complexity later, the
collector — which we control, unlike the addon — can additionally remote-write to
a workspace in a stable tenant. That needs a service principal and a stored
credential, so it is a deliberate decision rather than a default, and it is not
implemented here.

Practically: treat the per-tenant history as the regression signal, and when a
move is coming, screenshot or export the panels that establish the current
baseline so the new deployment has something to be compared against.

## Troubleshooting

**Every panel says "No data".** The managed Prometheus agent only scrapes
annotated pods in namespaces listed in `ama-metrics-settings-configmap`.
`deploy-soak.sh` applies it and restarts the agent; confirm with
`kubectl get cm ama-metrics-settings-configmap -n kube-system -o yaml`.

**Request-charge and returned-rows panels are empty, others work.** Those
instruments are opt-in. The manifests pass `--extended-metrics`; check it is
still on the container args.

**Pods `CrashLoopBackOff` with an authentication error.** Workload identity
federation is per service account name and namespace. If `NAMESPACE` changed, the
federated credential still points at the old subject — re-run
`provision-soak-infra.sh`.

**`--auth workload-identity` fails locally.** It is only for in-cluster use. Use
`--auth aad` on a developer machine; that path uses the developer credential
chain, which cannot work inside a container.

**Pods stay `Pending` and never schedule.** The soak pods select
`agentpool: $SOAK_NODE_POOL` and tolerate `workload=soak:NoSchedule`, so they only
land on the soak's own pool. Check the pool exists and matches:
`az aks nodepool list --cluster-name "$AKS_CLUSTER" --resource-group "$RESOURCE_GROUP" -o table`.
If it was created under a different name, set `SOAK_NODE_POOL` in `soak.env` and
re-run `./deploy-soak.sh`; `kubectl describe pod` reports it as
`node(s) didn't match Pod's node affinity/selector`.

**Perf numbers moved when the soak was deployed.** Two candidates, in order:
enabling managed Prometheus put an `ama-metrics` DaemonSet on every node
(one-time, absorbed by the concurrency tuner, but the baseline shifts); or a soak
pod is on a perf node, which means the `nodeSelector` is not matching — check
with `kubectl get pods -n "$NAMESPACE" -o wide` and compare against
`kubectl get pods -n cosmos-perf -o wide`.

**Dashboard shows canary errors as if they were real.** Filter on
`db_collection_name` — the canary container is suffixed `_canary`.

**Provisioning fails on a name already in use.** The container registry and
Cosmos account names are globally unique across Azure, so the usual cause is the
previous deployment still holding them. Set `NAME_SUFFIX` — see [Do it with an
overlap, not a gap](#do-it-with-an-overlap-not-a-gap). The same error appears
briefly after a teardown while the deletion finishes.

**`az account set` says the subscription does not exist.** `soak.env` still has
the old tenant's `SUBSCRIPTION_ID`, or the session is logged into a different
tenant. `az login --tenant <new-tenant-id>` and check `az account show`.

**Everything provisions but the dashboard is empty in a tenant that worked
before.** Check the Grafana workspace the dashboard landed in is the one linked
to *this* cluster's Azure Monitor workspace. After a move, a stale
`GRAFANA_NAME`/`GRAFANA_RESOURCE_GROUP` in `soak.env` can point
`upload-grafana-dashboard.sh` at a workspace whose Prometheus datasource is fed
by a cluster that no longer exists.
