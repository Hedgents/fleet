#!/usr/bin/env bash
# Hedgents fleet boot script.
#
# Boots: dashboard server + 5 daemons.
# All daemons emit JSON-formatted tracing → dashboard tails the dir.
#
# Usage:
#   ./scripts/run-fleet-with-dashboard.sh [devnet|mainnet]
#
# Env overrides:
#   SOAK_DIR=~/01fi-soak           — workspace dir (logs + secrets + db)
#   RPC_URL=https://...            — Solana RPC override
#   SKIP_HEDGEDJLP=1               — skip hedgedjlp-daemon
#   DASHBOARD_PORT=7700            — dashboard server port
#   SKIP_BUILD=1                   — skip cargo build (use existing target/release)
#
# All daemons default to --simulate-only=true. Set --simulate-only=false
# manually after dress rehearsal. The boot script only spawns processes;
# operator sends Assigns separately via fleet-pm-stub.

set -euo pipefail

NETWORK="${1:-devnet}"
if [[ "$NETWORK" != "devnet" && "$NETWORK" != "mainnet" ]]; then
    echo "Usage: $0 [devnet|mainnet]" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SOAK_DIR="${SOAK_DIR:-$HOME/01fi-soak}"
RPC_URL_DEFAULT_DEVNET="https://api.devnet.solana.com"
RPC_URL_DEFAULT_MAINNET="https://api.mainnet-beta.solana.com"
if [[ -n "${RPC_URL:-}" ]]; then
    :
elif [[ "$NETWORK" == "mainnet" ]]; then
    RPC_URL="$RPC_URL_DEFAULT_MAINNET"
else
    RPC_URL="$RPC_URL_DEFAULT_DEVNET"
fi
DASHBOARD_PORT="${DASHBOARD_PORT:-7700}"
LOGS="$SOAK_DIR/logs"
SECRETS="$SOAK_DIR/secrets"
DB="$SOAK_DIR/dashboard.sqlite"

mkdir -p "$LOGS" "$SECRETS"

# Generate role keys + wallet if missing.
for role in multiply stable-yield hedgedjlp riskwatcher researcher orchestrator; do
    KEY_FILE="$SECRETS/${role}-role.key"
    if [[ ! -f "$KEY_FILE" ]]; then
        openssl rand 32 > "$KEY_FILE"
        chmod 600 "$KEY_FILE"
        echo "generated $KEY_FILE"
    fi
done

WALLET_FILE="$SECRETS/solana-wallet.json"
if [[ ! -f "$WALLET_FILE" ]]; then
    if command -v solana-keygen &>/dev/null; then
        solana-keygen new --outfile "$WALLET_FILE" --no-bip39-passphrase --force >/dev/null
        echo "generated $WALLET_FILE"
        echo "(operator must fund this wallet before running mainnet — see README)"
    else
        echo "ERROR: solana-keygen not found; install Solana CLI or place a keypair at $WALLET_FILE" >&2
        exit 1
    fi
fi

# Build everything once (release).
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    echo "building workspace..."
    (cd "$REPO_ROOT" && cargo build --release --workspace --quiet 2>&1 | tail -5)
fi

# Derive 32-byte hex Ed25519 pubkey from a raw 32-byte role-key seed.
# Required by riskwatcher --orchestrator (and as recipient agent_ids when
# the operator wires fleet-pm-stub by hand).
derive_role_pubkey_hex() {
    local key_file="$1"
    python3 -c "
import sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization
seed = open(sys.argv[1], 'rb').read()
assert len(seed) == 32, f'seed len {len(seed)} != 32'
k = Ed25519PrivateKey.from_private_bytes(seed)
print(k.public_key().public_bytes(
    serialization.Encoding.Raw,
    serialization.PublicFormat.Raw,
).hex())
" "$key_file"
}

ORCHESTRATOR_PUBKEY_HEX="$(derive_role_pubkey_hex "$SECRETS/orchestrator-role.key")"
echo "orchestrator agent_id (riskwatcher --orchestrator): $ORCHESTRATOR_PUBKEY_HEX"

# Derive subscriber pubkeys for the researcher's MarketSignal broadcasts.
MULTIPLY_PUBKEY_HEX="$(derive_role_pubkey_hex "$SECRETS/multiply-role.key")"
STABLE_YIELD_PUBKEY_HEX="$(derive_role_pubkey_hex "$SECRETS/stable-yield-role.key")"
HEDGEDJLP_PUBKEY_HEX="$(derive_role_pubkey_hex "$SECRETS/hedgedjlp-role.key")"
RISKWATCHER_PUBKEY_HEX="$(derive_role_pubkey_hex "$SECRETS/riskwatcher-role.key")"

# Network ack flag for mainnet.
ACK_ARGS=""
if [[ "$NETWORK" == "mainnet" ]]; then
    ACK_ARGS="--i-understand-this-is-mainnet"
fi

# macOS bash 3.2 has no associative arrays — use plain parallel arrays.
PID_NAMES=()
PID_VALUES=()

start_daemon() {
    local name="$1"
    local cmd="$2"
    echo ">>> starting $name (logs → $LOGS/$name.log)"
    RUST_LOG_FORMAT=json RUST_LOG="${RUST_LOG:-info,libp2p=warn}" \
        bash -c "$cmd" \
        > "$LOGS/$name.log" 2>&1 &
    PID_NAMES+=("$name")
    PID_VALUES+=("$!")
}

cleanup() {
    echo ""
    echo ">>> shutting down..."
    local i
    for i in "${!PID_VALUES[@]}"; do
        kill "${PID_VALUES[$i]}" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    exit 0
}
trap cleanup INT TERM

cd "$REPO_ROOT"

# Dashboard server first — it must be up so it picks up logs from t=0.
# Note: dashboard is a long-running mesh-aware server; we run it WITHOUT
# RUST_LOG_FORMAT=json (text logs are fine for the operator to tail
# manually). Logs go to dashboard.log but the dashboard tails *.log itself,
# so we keep its own log out of the same dir. Use a sibling file.
echo ">>> starting dashboard (logs → $SOAK_DIR/dashboard-server.log)"
RUST_LOG="info,libp2p=warn" \
    "$REPO_ROOT/target/release/fleet-dashboard-server" \
        --log-dir "$LOGS" \
        --telemetry-dir "$LOGS" \
        --db-path "$DB" \
        --solana-wallet "$WALLET_FILE" \
        --rpc-url "$RPC_URL" \
        --listen "127.0.0.1:$DASHBOARD_PORT" \
    > "$SOAK_DIR/dashboard-server.log" 2>&1 &
PID_NAMES+=("dashboard")
PID_VALUES+=("$!")

# Daemons. Each binds its own libp2p listen port. multiply is the
# bootstrap target; the rest dial it.
start_daemon multiply "$REPO_ROOT/target/release/multiply-daemon run \
    --secrets-dir $SECRETS \
    --wallet $WALLET_FILE \
    --rpc-url $RPC_URL \
    --listen /ip4/127.0.0.1/tcp/19302 \
    --beacon-interval-secs 5 \
    --network $NETWORK \
    $ACK_ARGS \
    --simulate-only true \
    --pnl-log $LOGS/multiply-pnl.jsonl"

start_daemon stable-yield "$REPO_ROOT/target/release/stable-yield-daemon \
    --secrets-dir $SECRETS \
    --rpc-url $RPC_URL \
    --listen /ip4/127.0.0.1/tcp/19310 \
    --bootstrap /ip4/127.0.0.1/tcp/19302 \
    --network $NETWORK \
    $ACK_ARGS \
    --beacon-interval-secs 5 \
    --simulate-only true \
    --require-approval false \
    --telemetry-log $LOGS/stable-yield-pnl.jsonl"

if [[ "${SKIP_HEDGEDJLP:-0}" != "1" ]]; then
    start_daemon hedgedjlp "$REPO_ROOT/target/release/hedgedjlp-daemon \
        --secrets-dir $SECRETS \
        --rpc-url $RPC_URL \
        --listen /ip4/127.0.0.1/tcp/19311 \
        --bootstrap /ip4/127.0.0.1/tcp/19302 \
        --network $NETWORK \
        $ACK_ARGS \
        --beacon-interval-secs 5 \
        --simulate-only true \
        --rebalance-interval-secs 600 \
        --telemetry-log $LOGS/hedgedjlp-pnl.jsonl"
fi

start_daemon riskwatcher "$REPO_ROOT/target/release/riskwatcher-daemon \
    --secrets-dir $SECRETS \
    --rpc-url $RPC_URL \
    --listen /ip4/127.0.0.1/tcp/19303 \
    --bootstrap /ip4/127.0.0.1/tcp/19302 \
    --network $NETWORK \
    $ACK_ARGS \
    --beacon-interval-secs 5 \
    --orchestrator $ORCHESTRATOR_PUBKEY_HEX \
    --telemetry-log $LOGS/riskwatcher-pnl.jsonl \
    --metrics-listen 127.0.0.1:9091"

start_daemon researcher "$REPO_ROOT/target/release/researcher-daemon \
    --secrets-dir $SECRETS \
    --rpc-url $RPC_URL \
    --listen /ip4/127.0.0.1/tcp/19316 \
    --bootstrap /ip4/127.0.0.1/tcp/19302 \
    --network $NETWORK \
    $ACK_ARGS \
    --beacon-interval-secs 5 \
    --lending-poll-interval-secs 60 \
    --lending-reserve usdc:D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59:USDC \
    --price-poll-interval-secs 30 \
    --price-feed sol:7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE:SOL \
    --jlp-pool 5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq \
    --subscriber $MULTIPLY_PUBKEY_HEX \
    --subscriber $STABLE_YIELD_PUBKEY_HEX \
    --subscriber $HEDGEDJLP_PUBKEY_HEX \
    --subscriber $RISKWATCHER_PUBKEY_HEX \
    --telemetry-log $LOGS/researcher-signals.jsonl"

echo ""
echo "==== fleet running ===="
echo "  network:          $NETWORK"
echo "  rpc:              $RPC_URL"
echo "  workspace:        $SOAK_DIR"
echo "  dashboard:        http://127.0.0.1:$DASHBOARD_PORT/events"
echo "  health pills:     http://127.0.0.1:$DASHBOARD_PORT/daemons"
echo "  logs tailed:      $LOGS/*.log"
echo "  ctrl-c to stop"
echo ""

# Wait for any daemon to exit (or ctrl-c via trap).
wait
