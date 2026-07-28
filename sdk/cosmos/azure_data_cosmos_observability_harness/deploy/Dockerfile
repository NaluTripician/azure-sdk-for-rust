# Copyright (c) Microsoft Corporation. All rights reserved.
# Licensed under the MIT License.

# Multi-stage build for the Cosmos DB observability soak harness.
#
# Mirrors the shape of the perf harness image (`rust-perf/Dockerfile` in the
# cosmos-sdk-copilot-toolkit repo) so the two workloads can share an ACR and the
# same AKS node pool conventions.
#
# The build context is the repository root, because the harness depends on the
# in-repo `azure_data_cosmos` crate via a path dependency:
#
#   docker build -f sdk/cosmos/azure_data_cosmos_observability_harness/deploy/Dockerfile .
#
# `deploy-soak.sh` does this for you.

FROM rust:1.88-bookworm AS builder
WORKDIR /src
COPY . .

# `otlp` is opt-in in the crate's feature set but mandatory in the cloud soak:
# the harness has to reach the in-cluster Collector over OTLP/gRPC. The default
# features (metrics, distributed_tracing, fault_injection) stay on so the image
# exercises the exact code path the SDK ships.
ARG HARNESS_FEATURES="otlp"
RUN cargo build --release \
    -p azure_data_cosmos_observability_harness \
    --features "$HARNESS_FEATURES"

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/azure_data_cosmos_observability_harness /usr/local/bin/

# Quiet at steady state, rich on error: keep the SDK's sampled-diagnostics lines
# and harness progress visible in `kubectl logs` without the dependency noise.
ENV RUST_LOG="warn,azure_data_cosmos_observability_harness=info,azure_data_cosmos::diagnostics=info"

ENTRYPOINT ["azure_data_cosmos_observability_harness"]
