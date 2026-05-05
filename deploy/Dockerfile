# Build stage: compile all six daemon binaries.
# Build context is the parent dir (..) containing both worktrees.
FROM rust:1.83-slim AS build

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake build-essential ca-certificates protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

# Copy both workspaces into the build context. The 01fi workspace's
# Cargo.toml has path-deps on ../node-enterprise/... — this layout
# preserves that.
WORKDIR /work
COPY zerox1-01fi-fleet ./zerox1-01fi-fleet
COPY node-enterprise   ./node-enterprise

WORKDIR /work/zerox1-01fi-fleet
RUN cargo build --release --workspace --bins

# Runtime stage: tiny image with all six binaries on the PATH.
FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Copy each daemon binary into /usr/local/bin.
COPY --from=build /work/zerox1-01fi-fleet/target/release/riskwatcher-daemon  /usr/local/bin/
COPY --from=build /work/zerox1-01fi-fleet/target/release/multiply-daemon     /usr/local/bin/
COPY --from=build /work/zerox1-01fi-fleet/target/release/hedgedjlp-daemon    /usr/local/bin/
COPY --from=build /work/zerox1-01fi-fleet/target/release/stablefloor-daemon  /usr/local/bin/
COPY --from=build /work/zerox1-01fi-fleet/target/release/researcher-daemon   /usr/local/bin/
COPY --from=build /work/zerox1-01fi-fleet/target/release/speculator-daemon   /usr/local/bin/

# Each daemon picks its own command via docker-compose.
WORKDIR /
