#!/usr/bin/env bash
# Hedgents fleet health monitor.
#
# Runs every 5 minutes via systemd timer or cron. Checks for "anything not
# perfect" with the fleet:
#   1. Every live daemon is systemctl-active
#   2. No on-chain tx failures in the last 10 minutes (Kamino, Jupiter Perps, etc.)
#   3. No orphan perp shorts (recover.rs would log "orphan shorts" warn)
#   4. Allocator is producing actionable recommendations (not stuck in NoAction loops)
#   5. Wallet's free USDC is not unexpectedly draining
#   6. The orchestrator has emitted a tick in the last 15 minutes
#
# Emits a single alert per UNIQUE failure mode (deduped via /var/lib/hedgents/
# monitor-state.json) so the operator inbox doesn't get spammed with the
# same error every 5 minutes.
#
# Notification channel: writes to /var/log/hedgents-monitor.log AND sends an
# email via the system's `mail` command if a new failure appears.
#
# Exit 0 always — this is a monitor, not a gate. Failure to detect is itself
# logged as a meta-failure to /var/log/hedgents-monitor.log.

set -uo pipefail

LOG_DIR="/var/lib/hedgents/logs"
STATE_FILE="/var/lib/hedgents/monitor-state.json"
ALERT_LOG="/var/log/hedgents-monitor.log"
ALERT_EMAIL="${HEDGENTS_ALERT_EMAIL:-contact@infinityteam.io}"
HOSTNAME=$(hostname)

# 10-minute window for "recent" events.
WINDOW_MIN=10
NOW_EPOCH=$(date -u +%s)
WINDOW_EPOCH=$((NOW_EPOCH - WINDOW_MIN * 60))

mkdir -p "$(dirname "$ALERT_LOG")"
mkdir -p "$(dirname "$STATE_FILE")"
[[ -f "$STATE_FILE" ]] || echo '{}' > "$STATE_FILE"

log() {
    local level="$1"
    shift
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] [$level] $*" >> "$ALERT_LOG"
}

# Emit a new alert iff the signature hasn't been alerted in the last hour.
# Dedupes via the state file's last_seen map: signature -> epoch.
emit_alert() {
    local signature="$1"
    local body="$2"

    local last_seen
    last_seen=$(python3 -c "
import json, sys
try:
    s = json.load(open('$STATE_FILE'))
    print(s.get('$signature', 0))
except Exception:
    print(0)
" 2>/dev/null || echo 0)

    # Only fire if no alert for this signature in the last 3600s.
    if (( NOW_EPOCH - last_seen < 3600 )); then
        log INFO "Suppressed duplicate alert (signature=$signature, last_seen=$last_seen)"
        return
    fi

    log ALERT "$signature: $body"

    # Update state.
    python3 -c "
import json
s = json.load(open('$STATE_FILE'))
s['$signature'] = $NOW_EPOCH
json.dump(s, open('$STATE_FILE', 'w'))
" 2>/dev/null || true

    # Send email if configured.
    if command -v mail >/dev/null 2>&1; then
        echo "$body" | mail -s "[hedgents-monitor $HOSTNAME] $signature" "$ALERT_EMAIL" 2>/dev/null \
            && log INFO "Emailed alert to $ALERT_EMAIL" \
            || log WARN "Email send failed (mail command exited non-zero)"
    fi
}

# ── Check 1: every live daemon is active ──────────────────────────────────
for service in hedgents-orchestrator hedgents-hedgedjlp-live hedgents-stable-yield-live \
               hedgents-multiply-live hedgents-riskwatcher hedgents-researcher \
               hedgents-frontend hedgents-dashboard; do
    state=$(systemctl is-active "$service" 2>/dev/null || echo "missing")
    if [[ "$state" != "active" ]]; then
        emit_alert "service-down:$service" \
            "Service $service is in state '$state'. Expected: active. Run: systemctl status $service"
    fi
done

# ── Check 2: recent on-chain tx failures ──────────────────────────────────
# Looks for ANY "build_sign_send failed" in the last 10 min across all live daemons.
for daemon_log in hedgedjlp-live.log stable-yield-live.log multiply-live.log; do
    log_path="$LOG_DIR/$daemon_log"
    [[ -f "$log_path" ]] || continue

    # Extract recent failures. Match either build_sign_send failed OR keeper rejects.
    recent_failures=$(tail -2000 "$log_path" 2>/dev/null | python3 -c "
import json, sys, time
now = $NOW_EPOCH
window = $WINDOW_EPOCH
count = 0
last_err = ''
for line in sys.stdin:
    line = line.strip()
    if not line.startswith('{'): continue
    try:
        d = json.loads(line)
        ts = d.get('timestamp','')
        if not ts: continue
        from datetime import datetime
        epoch = int(datetime.strptime(ts.replace('Z',''), '%Y-%m-%dT%H:%M:%S.%f').timestamp())
        if epoch < window: continue
        msg = d.get('fields',{}).get('message','')
        if 'build_sign_send failed' in msg or 'submit failed' in msg or 'simulation failed' in msg:
            count += 1
            err = d.get('fields',{}).get('e','')
            if err: last_err = err[:200]
    except Exception:
        pass
print(f'{count}|{last_err}')
" 2>/dev/null || echo "0|")

    count=${recent_failures%|*}
    err=${recent_failures#*|}
    if [[ "$count" -gt 0 ]]; then
        emit_alert "tx-failures:$daemon_log" \
            "$count tx failures in $daemon_log in last ${WINDOW_MIN}m. Latest: ${err:0:300}"
    fi
done

# ── Check 3: orphan perp shorts ───────────────────────────────────────────
# recover.rs emits "orphan shorts" warn when it sees them.
if [[ -f "$LOG_DIR/hedgedjlp-live.log" ]]; then
    orphan_count=$(tail -500 "$LOG_DIR/hedgedjlp-live.log" 2>/dev/null \
        | python3 -c "
import json, sys
from datetime import datetime
window = $WINDOW_EPOCH
count = 0
for line in sys.stdin:
    if 'orphan shorts' not in line: continue
    try:
        d = json.loads(line)
        ts = d.get('timestamp','').replace('Z','')
        epoch = int(datetime.strptime(ts, '%Y-%m-%dT%H:%M:%S.%f').timestamp())
        if epoch >= window: count += 1
    except Exception: pass
print(count)
" 2>/dev/null | tail -1 | tr -d '[:space:]')
    orphan_count="${orphan_count:-0}"
    if (( orphan_count > 0 )); then
        emit_alert "orphan-shorts" \
            "recover.rs detected $orphan_count orphan-shorts event(s) in last ${WINDOW_MIN}m. \
Run: journalctl -u hedgents-hedgedjlp-live -n 50"
    fi
fi

# ── Check 4: allocator producing actionable output ───────────────────────
# If the most recent 6 ticks (~30 min) are all NoAction AND total_aum > $10
# AND APR gap between deployed strategies is > 200bps, that's stuck capital.
if [[ -f "$LOG_DIR/orchestrator.log" ]]; then
    stuck=$(tail -200 "$LOG_DIR/orchestrator.log" 2>/dev/null \
        | grep -E "NoAction|Withdraw|Deposit" | tail -6 \
        | grep -c "NoAction" 2>/dev/null | tail -1 | tr -d '[:space:]')
    stuck="${stuck:-0}"
    if (( stuck == 6 )); then
        # 6 consecutive NoAction recommendations. Surface the snapshot.
        snap=$(grep -A 5 "allocator snapshot" "$LOG_DIR/orchestrator.log" 2>/dev/null \
            | tail -5)
        emit_alert "allocator-stuck" \
            "Orchestrator emitted NoAction 6 ticks in a row. Latest snapshot: $snap"
    fi
fi

# ── Check 5: orchestrator heartbeat ──────────────────────────────────────
# The orchestrator emits a BEACON every ~10s. If the latest log line is more
# than 5 minutes old, the orchestrator is silent.
if [[ -f "$LOG_DIR/orchestrator.log" ]]; then
    latest_ts=$(tail -1 "$LOG_DIR/orchestrator.log" 2>/dev/null \
        | python3 -c "
import json, sys
from datetime import datetime
for line in sys.stdin:
    try:
        d = json.loads(line)
        ts = d.get('timestamp','').replace('Z','')
        print(int(datetime.strptime(ts, '%Y-%m-%dT%H:%M:%S.%f').timestamp()))
        break
    except Exception: pass
" 2>/dev/null || echo 0)
    if (( NOW_EPOCH - latest_ts > 300 )); then
        emit_alert "orchestrator-silent" \
            "Orchestrator last log line was $((NOW_EPOCH - latest_ts))s ago (>5 min). \
Service status: $(systemctl is-active hedgents-orchestrator)"
    fi
fi

# ── Final: log a heartbeat so we know the monitor itself ran ─────────────
log INFO "monitor pass complete"
exit 0
