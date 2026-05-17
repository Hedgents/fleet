# Orchestrator execute mode — mainnet bring-up

Target release: `fleet-v0.4.0`. Prerequisite: devnet smoke
(`orchestrator-devnet-smoke.md`) passed with all checkboxes ticked.

Scope: promote the orchestrator from `--dry-run` observation (rc1) to
`--execute` autonomous capital movement on Solana mainnet.

The change at the systemd-unit level is small (two new flags); the
operational discipline around it is the whole point of this runbook.

## Pre-flight checks

Run all of these before flipping `--execute` on. Any "no" → don't
proceed.

| Check | How |
|-------|-----|
| Dry-run audit log is healthy | `jq -r '.action.action' /var/lib/hedgents/logs/orchestrator-audit.jsonl \| tail -n 1440 \| sort \| uniq -c` — expect mostly `no_action` with occasional `deposit`; no inverted-carry surprises |
| All 5 strategy daemons green | `curl -s http://127.0.0.1:7700/daemons \| jq -r '.[] \| "\(.role)\t\(.status)\t\(.last_heartbeat_ms_ago)"'` — all green, recent heartbeat |
| Riskwatcher escalates count = 0 | `curl -s http://127.0.0.1:9091/metrics \| grep riskwatcher_escalates_total` |
| Live target up | `systemctl is-active hedgents-live.target` |
| Audit log writable | `sudo -u hedgents touch /var/lib/hedgents/logs/orchestrator-audit.jsonl && echo OK` |

## Step 1 — generate mainnet targets

Get each strategy daemon's agent_id (hex) from its boot log:

```bash
for d in stable-yield multiply hedgedjlp; do
    echo -n "$d: "
    grep -oE 'agent_id_hex=[a-f0-9]{64}' /var/lib/hedgents/logs/${d}-live.log \
        | head -1 | cut -d= -f2
done
```

Get the Kamino mainnet main-market + USDC reserve b58 from the
stable-yield daemon's systemd unit (`hedgents-stable-yield-live.service`):

```bash
systemctl cat hedgents-stable-yield-live | grep -E "market|reserve"
```

Write the targets file:

```bash
sudo -u hedgents tee /etc/hedgents/orchestrator-targets-mainnet.json <<JSON
{
  "stable_yield": {
    "recipient_agent_id_hex": "<stable-yield-live-agent-id-hex>",
    "market_b58": "<kamino-mainnet-main-market>",
    "reserve_b58": "<kamino-mainnet-usdc-reserve>"
  },
  "multiply": {
    "recipient_agent_id_hex": "<multiply-live-agent-id-hex>"
  },
  "hedgedjlp": {
    "recipient_agent_id_hex": "<hedgedjlp-live-agent-id-hex>"
  }
}
JSON
sudo chmod 0640 /etc/hedgents/orchestrator-targets-mainnet.json
sudo chown root:hedgents /etc/hedgents/orchestrator-targets-mainnet.json
```

`0640` + `root:hedgents` so the daemon user can read it but a
compromised daemon process can't overwrite it.

## Step 2 — edit the systemd unit

Add the two flags to `ExecStart` in
`/etc/systemd/system/hedgents-orchestrator.service`:

```diff
 ExecStart=/opt/hedgents/bin/orchestrator-daemon \
     --secrets-dir=/var/lib/hedgents/secrets \
     --listen=/ip4/0.0.0.0/tcp/19317 \
     --bootstrap=/ip4/127.0.0.1/tcp/19302 \
     --beacon-interval-secs=30 \
     --api-base=http://127.0.0.1:7700 \
     --tick-interval-secs=60 \
-    --audit-log=/var/lib/hedgents/logs/orchestrator-audit.jsonl
+    --audit-log=/var/lib/hedgents/logs/orchestrator-audit.jsonl \
+    --execute \
+    --targets-json=/etc/hedgents/orchestrator-targets-mainnet.json \
+    --cooldown-secs=300 \
+    --max-action-fraction=0.10 \
+    --min-action-usd=10.0
```

**Note the conservative caps:**
- `--cooldown-secs=300` — 5 min between consecutive dispatches per
  strategy
- `--max-action-fraction=0.10` — no single tick moves more than 10% of
  AUM (rc1 default was 0.50; tighten for first mainnet promotion)
- `--min-action-usd=10.0` — skip dust actions

These can be loosened in later releases once the orchestrator has run
in execute mode for ≥7 days without surprise.

## Step 3 — first-action observation window

```bash
sudo systemctl daemon-reload
sudo systemctl restart hedgents-orchestrator
sudo journalctl -u hedgents-orchestrator -f
```

For the next **1 hour**:

1. Stay in the same SSH session. Don't multitask.
2. Tail both the orchestrator log and the audit JSONL.
3. The orchestrator must produce at least one of:
   - One real envelope (audit record with `"envelope_result": "sent"`)
   - Or one explicit "skipped" record explaining why no action was
     warranted
4. If you see `"failed:*"`, stop the daemon immediately
   (`systemctl stop hedgents-orchestrator`) and investigate the
   recipient strategy daemon's log.
5. If the orchestrator emits anything you wouldn't have manually
   approved, stop it and re-tune the hurdles or risk premiums.

## Step 4 — first-day observation

Over the next 24 hours, count outcomes:

```bash
jq -r 'select(.mode=="execute") | .envelope_result' \
    /var/lib/hedgents/logs/orchestrator-audit.jsonl \
    | tail -n 1440 \
    | sort | uniq -c
```

Expected on a steady-state mainnet fleet:
- Many empty strings (NoAction ticks; nothing to dispatch)
- A handful of `sent`
- Maybe some `skipped:cooldown_*` if a regime change forced
  back-to-back dispatches

**Hard fail conditions** (any of these → roll back to dry-run):

| Symptom | Investigation |
|---------|---------------|
| Any `failed:*` line | Recipient daemon not reachable or rejected the envelope. Check daemon log. |
| > 5% of ticks produce `sent` | The hurdles are too tight; the orchestrator is over-trading. Widen `risk_premium_*_bps` |
| Same strategy dispatched > 4 times in an hour | Hot-loop. Increase `cooldown_secs`. |
| `skipped:stale_snapshot_*` rate > 1% | Some other process is racing the orchestrator. Investigate. |

## Step 5 — promotion to standard caps

After **7 days** of clean execute-mode operation with no hard-fails,
loosen the caps to operational defaults:

```diff
-    --max-action-fraction=0.10 \
-    --min-action-usd=10.0
+    --max-action-fraction=0.50 \
+    --min-action-usd=5.0
```

Restart, observe for another 24h, confirm the behaviour is unchanged.

Tag `fleet-v0.4.0` officially at this point.

## Roll-back

```bash
# 1. Stop dispatch immediately
sudo systemctl stop hedgents-orchestrator

# 2. Either:
#    a) Revert unit to rc1 dry-run (drop --execute + --targets-json), or
#    b) Leave the orchestrator stopped while you investigate

sudo systemctl daemon-reload
sudo systemctl start hedgents-orchestrator   # back to dry-run if reverted
```

No on-chain state to clean up. The audit log is append-only forward
history.

## What's NOT covered by this runbook

- **First-time orchestrator setup** — see `orchestrator-bringup.md`
- **CCTP-driven funding flows** — see (forthcoming)
  `cctp-bridge-mainnet.md`
- **Hurdle re-tuning playbook** — when DeFi rates change regime, the
  risk premium bps may need to move. That's a separate operational
  doc; for now, edit `--risk-premium-*-bps` flags and restart.
