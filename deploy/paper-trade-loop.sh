#!/usr/bin/env bash
# Drive periodic Assigns from a synthetic orchestrator into the three
# execution daemons. Sends one tick per INTERVAL_SECS; each tick fires
# an Assign at each strategy. Daemons are sim-only by default so no
# funds move.

set -uo pipefail

# Default to the systemd-deploy path; Docker compose overrides via SECRETS env.
SECRETS="${SECRETS:-/var/lib/hedgents/secrets}"
INTERVAL_SECS="${INTERVAL_SECS:-300}"
PATH="/opt/hedgents/bin:${PATH}"

# Derive recipient agent_ids from role keys (the libp2p keypair IS the
# role key, so its public key IS the agent id).
agent_id_for() {
    python3 <<PY
from nacl.signing import SigningKey
seed = open("$SECRETS/$1-role.key", "rb").read()
print(bytes(SigningKey(seed).verify_key).hex())
PY
}

STABLE_YIELD_AGENT=$(agent_id_for stable-yield)
MULTIPLY_AGENT=$(agent_id_for multiply)
HEDGEDJLP_AGENT=$(agent_id_for hedgedjlp)

KAMINO_MAIN_MARKET=7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF
KAMINO_USDC_RESERVE=D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59

log() { echo "[$(date -u +%H:%M:%SZ)] $*"; }

log "paper-trade-loop started (interval=${INTERVAL_SECS}s, sim-only)"
log "stable-yield agent: ${STABLE_YIELD_AGENT:0:16}…"
log "multiply agent:     ${MULTIPLY_AGENT:0:16}…"
log "hedgedjlp agent:    ${HEDGEDJLP_AGENT:0:16}…"

# Wait for the mesh to come up — give multiply a head start.
sleep 15

while true; do
    log "=== tick $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="

    log "→ stable-yield"
    fleet-pm-stub \
        --secrets-dir "$SECRETS" \
        --bootstrap /dns/stable-yield/tcp/19310 \
        --timeout-secs 30 \
        --recipient-agent-id "$STABLE_YIELD_AGENT" \
        assign-stable-lend \
        --market "$KAMINO_MAIN_MARKET" \
        --reserve "$KAMINO_USDC_RESERVE" \
        --usdc-lamports 5000000 || true

    log "→ multiply"
    fleet-pm-stub \
        --secrets-dir "$SECRETS" \
        --bootstrap /dns/multiply/tcp/19302 \
        --timeout-secs 30 \
        --recipient-agent-id "$MULTIPLY_AGENT" \
        assign-multiply \
        --target-ltv-bps 6000 || true

    log "→ hedgedjlp"
    fleet-pm-stub \
        --secrets-dir "$SECRETS" \
        --bootstrap /dns/hedgedjlp/tcp/19311 \
        --timeout-secs 30 \
        --recipient-agent-id "$HEDGEDJLP_AGENT" \
        assign-hedgedjlp \
        --usdc-lamports 5000000 \
        --target-delta-bps 0 || true

    log "tick done — sleeping ${INTERVAL_SECS}s"
    sleep "$INTERVAL_SECS"
done
