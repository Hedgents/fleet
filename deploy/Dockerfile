# Hedgents fleet builder + runtime image.
#
# Self-contained: clones p2p_architecture (the zerox1-protocol +
# zerox1-node-enterprise crates the fleet path-depends on) directly
# during the build. Judges / operators only need to clone the fleet
# repo.
#
# Build context is the fleet repo root (see compose).

# ── Stage 1: planner (cargo-chef recipe so dep compilation is cached) ──────
FROM rust:1.93-slim AS chef
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake build-essential ca-certificates \
        protobuf-compiler git \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /work

# Clone the path-dep workspace into a sibling directory the fleet
# Cargo.toml expects (`../p2p_architecture`).
ARG P2P_REPO=https://github.com/Hedgents/p2p_architecture.git
ARG P2P_REF=main
RUN git clone --depth 1 --branch ${P2P_REF} ${P2P_REPO} /work/p2p_architecture

# ── Stage 2: recipe ────────────────────────────────────────────────────────
FROM chef AS planner
COPY . /work/fleet
WORKDIR /work/fleet
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: build deps (cached layer), then build the workspace ───────────
FROM chef AS builder
COPY --from=planner /work/fleet/recipe.json /work/fleet/recipe.json
WORKDIR /work/fleet
RUN cargo chef cook --release --recipe-path recipe.json

COPY . /work/fleet
RUN cargo build --release \
    -p multiply-daemon \
    -p stable-yield-daemon \
    -p hedgedjlp-daemon \
    -p riskwatcher-daemon \
    -p researcher-daemon \
    -p fleet-dashboard-server \
    -p fleet-pm-stub

# ── Stage 4: minimal runtime image ─────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /work/fleet/target/release/multiply-daemon          /usr/local/bin/
COPY --from=builder /work/fleet/target/release/stable-yield-daemon      /usr/local/bin/
COPY --from=builder /work/fleet/target/release/hedgedjlp-daemon         /usr/local/bin/
COPY --from=builder /work/fleet/target/release/riskwatcher-daemon       /usr/local/bin/
COPY --from=builder /work/fleet/target/release/researcher-daemon        /usr/local/bin/
COPY --from=builder /work/fleet/target/release/fleet-dashboard-server   /usr/local/bin/
COPY --from=builder /work/fleet/target/release/fleet-pm-stub            /usr/local/bin/

COPY deploy/paper-trade-loop.sh /usr/local/bin/paper-trade-loop.sh
RUN chmod +x /usr/local/bin/paper-trade-loop.sh

WORKDIR /
