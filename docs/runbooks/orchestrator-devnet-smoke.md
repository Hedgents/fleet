# Orchestrator execute mode — devnet smoke

Target release: `fleet-v0.4.0-rc2` (execute-mode candidate, devnet
only). Prerequisite: `v0.4.0-rc1` dry-run runbook (`orchestrator-bringup.md`)
has been exercised at least once.

Scope: prove the orchestrator's execute path end-to-end on devnet —
the snapshot fetch, allocator decision, cooldown gate, stale-snapshot
guard, envelope build, recipient peer wait, signed dispatch, and audit
log all working together against paper-mode strategy daemons.

## Pre-flight

You need:

- All 5 paper daemons running on devnet + the dashboard up at
  `127.0.0.1:7700` (the standard `hedgents-paper-trade.service` flow)
- Each strategy daemon's `agent_id` (hex) — recorded in
  `/var/lib/hedgents/logs/<daemon>.log` at boot:
  `agent_id_hex=<64-char-hex>`
- Kamino devnet main-market + USDC reserve pubkeys for the stable_yield
  target (already in `paper-trade-loop.sh` env or the systemd unit's
  `EnvironmentFile`)

## Step 1 — write the devnet targets file

```bash
sudo -u hedgents tee /etc/hedgents/orchestrator-targets-devnet.json <<JSON
{
  "stable_yield": {
    "recipient_agent_id_hex": "<stable-yield-daemon-agent-id-hex>",
    "market_b58": "<kamino-devnet-main-market-b58>",
    "reserve_b58": "<kamino-devnet-usdc-reserve-b58>"
  },
  "multiply": {
    "recipient_agent_id_hex": "<multiply-daemon-agent-id-hex>"
  },
  "hedgedjlp": {
    "recipient_agent_id_hex": "<hedgedjlp-daemon-agent-id-hex>"
  }
}
JSON
sudo chmod 0644 /etc/hedgents/orchestrator-targets-devnet.json
```

## Step 2 — boot orchestrator in execute mode

Either edit the existing `hedgents-orchestrator.service` ExecStart in
place, or run a one-off:

```bash
sudo -u hedgents /opt/hedgents/bin/orchestrator-daemon \
    --secrets-dir=/var/lib/hedgents/secrets \
    --listen=/ip4/0.0.0.0/tcp/19317 \
    --bootstrap=/ip4/127.0.0.1/tcp/19302 \
    --api-base=http://127.0.0.1:7700 \
    --tick-interval-secs=30 \
    --audit-log=/var/lib/hedgents/logs/orchestrator-audit.jsonl \
    --execute \
    --targets-json=/etc/hedgents/orchestrator-targets-devnet.json \
    --cooldown-secs=60 \
    --wait-for-peer-secs=30 \
    --stale-slack=1.10
```

Expected log lines:

```
orchestrator starting fleet=… mode=execute
execute mode enabled — orchestrator will sign + dispatch envelopes
orchestrator tick loop starting interval_secs=30 mode=execute
── allocator snapshot ────────────────
…
── recommendation ────────────────────
  Deposit stable_yield $XX.XX
    reason: …
envelope built label=AssignStableLend conv=…
orchestrator envelope sent label=AssignStableLend nonce=… conv=…
```

Tail the audit log:

```bash
tail -f /var/lib/hedgents/logs/orchestrator-audit.jsonl | jq .
```

You should see records with `"mode": "execute"` and
`"envelope_result": "sent"`.

## Step 3 — provoke each AllocatorAction variant

The paper daemons publish their reported APRs from their telemetry
files. Edit those files (or temporarily override via env in the unit)
to force the allocator into each branch.

### 3a — `Deposit{stable_yield}` (idle-cash sweep)

Default: idle USDC > $5 with no leveraged strategy above hurdle.
Expected: `Deposit stable_yield $X` envelope dispatched within one
tick. Verify the stable-yield paper daemon's inbox.

```bash
tail -n 20 /var/lib/hedgents/logs/stable-yield.log | grep -E "Assign|inbox"
```

### 3b — `Withdraw{multiply}` (carry inverted)

Force the multiply paper APR below `stable_yield + 200 bps`. The
simplest hack: edit the paper telemetry to write a 400 bps APR for
multiply while stable_yield holds 700 bps.

Expected: one `Withdraw multiply $X` envelope (synthesised as
`AssignMultiply{target_ltv_bps=0}`) dispatched within one tick.

### 3c — `Deposit{hedgedjlp}` (carry favourable)

Push hedgedjlp paper APR to e.g. 2000 bps; ensure idle USDC ≥ $5.
Expected: `Deposit hedgedjlp $X` envelope dispatched.

### 3d — Cooldown gate (M2)

After step 3a fires, watch the next tick. The cooldown is 60s in this
runbook; for ~60 seconds the next ticks should log:

```
skipping dispatch — strategy in cooldown elapsed_secs=… cooldown_secs=60
```

…and the audit log records `envelope_result: "skipped:cooldown_XXs"`.

After the cooldown expires (or you bounce the daemon to reset the
in-memory state), the next tick should dispatch normally.

### 3e — Stale-snapshot gate (M5)

This is hardest to provoke deterministically. The guard fires when the
re-fetched snapshot has moved beyond `slack × (idle or deployed)`. To
test:

1. Set `--stale-slack=1.00` (zero tolerance) on a one-off boot
2. Wait for a `Deposit` recommendation
3. Manually edit the dashboard's `/aum` to drop `idle_usdc` to 1¢
4. The re-fetch inside `dispatch()` should see the new idle and
   produce: `envelope_result: "skipped:stale_snapshot_deposit_X_exceeds_idle_Y"`

In normal operation `--stale-slack=1.10` is generous enough that this
rarely triggers. The mechanism is mostly there for the regime where
multiple parties (operator + orchestrator) might race.

## Step 4 — verify reports come back

Each Assign/Withdraw envelope the orchestrator sends should produce a
corresponding `Report*` envelope from the recipient strategy daemon
within a few seconds (paper mode is fast).

```bash
curl -s 'http://127.0.0.1:7700/events?type=Report&limit=20' | jq -r '.[] | "\(.ts_ms)  \(.sender_role)  \(.msg_type)  \(.payload_summary)"'
```

Each Report should reference the orchestrator's `conv_id` (visible in
the audit log's `action.action` line).

## Step 5 — count outcomes over a 30-min window

```bash
jq -r '.envelope_result' /var/lib/hedgents/logs/orchestrator-audit.jsonl \
  | tail -n 60 | sort | uniq -c
```

Healthy steady-state on a paper devnet you've been hammering:
- Mostly `sent`
- Some empty strings (the NoAction ticks)
- A handful of `skipped:cooldown_XXs` if you forced rapid Deposit/Withdraw
- Zero `failed:*` lines

If you see `failed:*` lines, the recipient daemon was not reachable —
check the strategy daemon's beacon stream + the mesh connectivity in
`/var/lib/hedgents/logs/<daemon>.log`.

## Step 6 — roll back to dry-run

```bash
sudo systemctl stop hedgents-orchestrator
# revert the ExecStart (drop --execute and --targets-json) or simply
# run without those flags
sudo systemctl start hedgents-orchestrator
```

The audit log is forward-only — there is no "execute-mode rollback"
operation; you simply stop dispatching, and the orchestrator goes back
to writing dry-run records.

## What "smoke passed" means

After this runbook completes you should have:

- ✅ At least one `sent` audit record for each of: `AssignStableLend`,
  `AssignHedgedJlp`, `AssignMultiply(target_ltv_bps=0)`
- ✅ At least one `skipped:cooldown_*` audit record (M2 working)
- ✅ Zero unexpected `failed:*` records
- ✅ Each dispatched envelope produced a corresponding Report from
  the recipient daemon

If all of the above hold, you're ready for the mainnet bring-up
(`orchestrator-mainnet.md`).
