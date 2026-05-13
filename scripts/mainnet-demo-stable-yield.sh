#!/usr/bin/env bash
# Hedgents mainnet demo — real on-chain $X USDC deposit to Kamino.
#
# What it does
#   1. Stops any paper-trading soak (paper-trade-loop + soak stable-yield).
#   2. Boots stable-yield-daemon against mainnet via Helius RPC in
#      simulate-only mode and sends an Assign for $AMOUNT USDC.
#   3. If the simulation succeeds, prompts for confirmation, restarts the
#      daemon in live mode, re-sends the Assign, captures the on-chain
#      transaction signature, and prints a Solscan link.
#
# Safety
#   - The Helius RPC URL is read from ~/01fi-soak/secrets/helius-rpc-url
#     (chmod 600). No API key is hard-coded in this script.
#   - The simulation phase is mandatory: live submit is blocked if sim
#     does not return ok=true.
#   - Operator confirmation prompt sits between sim and live.
#   - The script exits with the daemon still running in live mode. To
#     return to paper mode, run scripts/run-fleet-with-dashboard.sh
#     mainnet.
#
# Usage
#   scripts/mainnet-demo-stable-yield.sh                 # default $1 USDC
#   scripts/mainnet-demo-stable-yield.sh --amount-usdc 5 # custom amount
#
# Withdrawal
#   See docs/runbooks/stable-yield-withdraw.md.

set -euo pipefail

# ─── Config ─────────────────────────────────────────────────────────────────
WORKDIR="$HOME/01fi-soak"
SECRETS="$WORKDIR/secrets"
WALLET="$SECRETS/solana-wallet.json"
HELIUS_FILE="$SECRETS/helius-rpc-url"
LOGS="$WORKDIR/logs"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/release"

# Kamino main lending market + USDC reserve. Verified against the Kamino app
# during the soak; also referenced by paper-trade-loop.sh.
KAMINO_MAIN_MARKET="7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF"
KAMINO_USDC_RESERVE="D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59"

# Defaults
AMOUNT_USDC=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --amount-usdc) AMOUNT_USDC="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
done
AMOUNT_LAMPORTS=$((AMOUNT_USDC * 1000000))

# ─── Pretty printing ────────────────────────────────────────────────────────
say()  { printf "\n▸ %s\n" "$*"; }
ok()   { printf "  ✓ %s\n" "$*"; }
warn() { printf "  ! %s\n" "$*"; }
bail() { printf "\n✖ %s\n" "$*" >&2; exit 1; }
hr()   { printf "▔%.0s" {1..60}; printf "\n"; }

# ─── Pre-flight ─────────────────────────────────────────────────────────────
say "Pre-flight checks"
[[ -f "$HELIUS_FILE" ]] || bail "Helius RPC URL not found at $HELIUS_FILE — see header for setup."
[[ -f "$WALLET" ]]      || bail "Wallet not found at $WALLET"
[[ -x "$BIN/stable-yield-daemon" ]] || bail "stable-yield-daemon binary missing — run cargo build --release"
[[ -x "$BIN/fleet-pm-stub" ]]       || bail "fleet-pm-stub binary missing — run cargo build --release"

RPC_URL="$(cat "$HELIUS_FILE")"
ok "RPC: ${RPC_URL%%\?*}?api-key=…"
ok "Wallet: $WALLET"
ok "Amount: \$${AMOUNT_USDC} USDC (${AMOUNT_LAMPORTS} lamports)"

# Derive stable-yield agent id (32-byte hex) from its mesh role-key.
STABLE_YIELD_AGENT="$(python3 - <<PY
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization
seed = open("$SECRETS/stable-yield-role.key", "rb").read()
k = Ed25519PrivateKey.from_private_bytes(seed)
print(k.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw).hex())
PY
)"
ok "stable-yield agent: ${STABLE_YIELD_AGENT:0:16}…${STABLE_YIELD_AGENT: -8}"

# Audit-fix C1: orchestrator pubkey is mandatory on mainnet. Derived from the
# same orchestrator-role.key that fleet-pm-stub signs Assigns with, so the
# daemon's allowlist passes for these test deposits.
ORCHESTRATOR_AGENT="$(python3 - <<PY
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization
seed = open("$SECRETS/orchestrator-role.key", "rb").read()
k = Ed25519PrivateKey.from_private_bytes(seed)
print(k.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw).hex())
PY
)"
ok "orchestrator agent: ${ORCHESTRATOR_AGENT:0:16}…${ORCHESTRATOR_AGENT: -8}"

# Wallet balance sanity (USDC + SOL). The daemon will hard-check at sim
# time; this is just a friendlier early failure.
say "Wallet balance check"
WALLET_JSON="$(curl -s http://127.0.0.1:7700/wallet 2>/dev/null || echo "{}")"
if [[ -n "$WALLET_JSON" && "$WALLET_JSON" != "{}" ]]; then
    SOL_LAMPORTS="$(echo "$WALLET_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("sol_lamports",0))')"
    USDC_LAMPORTS="$(echo "$WALLET_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("usdc_lamports",0))')"
    printf "  • SOL:  %.4f\n"  "$(awk "BEGIN{print ${SOL_LAMPORTS}/1000000000}")"
    printf "  • USDC: %.2f\n"  "$(awk "BEGIN{print ${USDC_LAMPORTS}/1000000}")"
    (( SOL_LAMPORTS  >= 5000000 ))         || warn "SOL balance below recommended 0.005 — tx fees may fail"
    (( USDC_LAMPORTS >= AMOUNT_LAMPORTS )) || bail "Insufficient USDC: need ${AMOUNT_LAMPORTS} lamports, have ${USDC_LAMPORTS}"
else
    warn "dashboard not running on :7700 — skipping pre-flight balance check (chain check still runs)"
fi

# ─── Stop existing paper-trading processes ──────────────────────────────────
say "Stopping paper-trade-loop and soak stable-yield-daemon"
pkill -f "paper-trade-loop.sh"      2>/dev/null && ok "paper-trade-loop stopped" || warn "paper-trade-loop not running"
pkill -f "stable-yield-daemon"      2>/dev/null && ok "stable-yield-daemon stopped" || warn "stable-yield-daemon not running"
sleep 2

# ─── Phase 1: sim-only run ──────────────────────────────────────────────────
boot_daemon() {
    local mode="$1"  # true | false
    say "Booting stable-yield-daemon (simulate-only=${mode}, RPC=helius)"
    RUST_LOG_FORMAT=json RUST_LOG=info,libp2p=warn \
        "$BIN/stable-yield-daemon" \
            --secrets-dir "$SECRETS" \
            --wallet "$WALLET" \
            --rpc-url "$RPC_URL" \
            --network mainnet \
            --i-understand-this-is-mainnet \
            --orchestrator-agent-id "$ORCHESTRATOR_AGENT" \
            --listen /ip4/127.0.0.1/tcp/19310 \
            --bootstrap /ip4/127.0.0.1/tcp/19302 \
            --max-position-usdc-lamports 100000000 \
            --simulate-only "$mode" \
            --require-approval false \
            --beacon-interval-secs 5 \
            --telemetry-market "$KAMINO_MAIN_MARKET" \
            --telemetry-interval-secs 60 \
            --telemetry-log "$LOGS/stable-yield-pnl.jsonl" \
        >> "$LOGS/stable-yield.log" 2>&1 &
    DAEMON_PID=$!
    ok "daemon PID: $DAEMON_PID"

    # Wait for daemon to be ready (genesis verified + listening).
    local marker_seen=0
    for i in {1..40}; do
        if tail -200 "$LOGS/stable-yield.log" 2>/dev/null | grep -q '"Genesis hash verified\|Listening on /ip4'; then
            marker_seen=1
            break
        fi
        sleep 1
    done
    (( marker_seen == 1 )) || bail "daemon failed to boot within 40s. See $LOGS/stable-yield.log"
    sleep 3
    ok "daemon ready"
}

send_assign() {
    say "Sending Assign for \$${AMOUNT_USDC} USDC"
    "$BIN/fleet-pm-stub" \
        --secrets-dir "$SECRETS" \
        --bootstrap /ip4/127.0.0.1/tcp/19310 \
        --timeout-secs 90 \
        --recipient-agent-id "$STABLE_YIELD_AGENT" \
        assign-stable-lend \
            --market  "$KAMINO_MAIN_MARKET" \
            --reserve "$KAMINO_USDC_RESERVE" \
            --usdc-lamports "$AMOUNT_LAMPORTS"
}

cleanup() { [[ -n "${DAEMON_PID:-}" ]] && kill "$DAEMON_PID" 2>/dev/null || true; }
trap cleanup EXIT

boot_daemon true
SIM_OUTPUT="$(send_assign 2>&1 | tee /dev/stderr)"

if ! echo "$SIM_OUTPUT" | grep -q 'ok: true'; then
    bail "Simulation did not report ok=true. Aborting before any live submit. See $LOGS/stable-yield.log"
fi

echo
hr
echo "  SIMULATION SUCCEEDED."
echo "  About to deposit \$${AMOUNT_USDC} USDC to Kamino on MAINNET."
echo "  This is a real on-chain transaction with real funds."
hr
printf "  Proceed with live submit? [y/N] "
read -r CONFIRM
if [[ "$CONFIRM" != "y" && "$CONFIRM" != "Y" ]]; then
    echo "  Cancelled. Daemon will be stopped on exit."
    exit 0
fi

# ─── Phase 2: live submit ───────────────────────────────────────────────────
say "Restarting daemon in LIVE mode (simulate-only=false)"
kill "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
sleep 2

boot_daemon false
LIVE_OUTPUT="$(send_assign 2>&1 | tee /dev/stderr)"

# Pull tx_signature out of the Report payload printed by fleet-pm-stub.
TX_SIG="$(echo "$LIVE_OUTPUT" | grep -oE 'tx_signature: Some\("[^"]+' | sed 's/^tx_signature: Some(\"//')"
if [[ -z "$TX_SIG" ]]; then
    TX_SIG="$(echo "$LIVE_OUTPUT" | grep -oE '"tx_signature":"[^"]+' | sed 's/^"tx_signature":"//')"
fi

echo
hr
if [[ -n "$TX_SIG" ]]; then
    echo "  ✓ ON-CHAIN DEPOSIT CONFIRMED"
    echo "  Signature: $TX_SIG"
    echo "  Solscan:   https://solscan.io/tx/$TX_SIG"
    echo "  Wallet:    https://solscan.io/account/$(python3 -c "
import json
import base58
import sys
b = json.load(open('$WALLET'))
sk = bytes(b[:32])
import nacl.signing
print(base58.b58encode(nacl.signing.SigningKey(sk).verify_key.encode()).decode())
" 2>/dev/null || echo 'see /wallet endpoint')"
else
    warn "tx_signature not parsed from fleet-pm-stub output. The deposit may have submitted — check $LOGS/stable-yield.log for 'submitted signature='."
fi
hr

echo
echo "  Daemon continues running on :19310 in LIVE mode."
echo "  To withdraw the position: follow docs/runbooks/stable-yield-withdraw.md."
echo "  To return to paper-trading soak: kill the daemon, then run"
echo "    scripts/run-fleet-with-dashboard.sh mainnet"
echo

# Detach: we want the daemon to keep running for the demo.
trap - EXIT
disown "$DAEMON_PID" 2>/dev/null || true
