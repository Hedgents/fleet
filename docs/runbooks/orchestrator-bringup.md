# Orchestrator daemon — bring-up runbook (Phase 1, dry-run)

Target release: `fleet-v0.4.0-rc1`.

Scope: stand the new `orchestrator-daemon` next to the existing five
live daemons on the operator's mainnet host. The daemon is observe-only
in this phase — it polls the dashboard, runs the pure
`fleet_pm_stub::allocator::decide` function, and writes every decision
to `/var/lib/hedgents/logs/orchestrator-audit.jsonl`. No envelopes, no
wallet. The next runbook (`orchestrator-execute.md`, v0.4.0 proper)
covers granting authority.

## Prerequisites

- The standard `install-hedgents.sh` has already been run at least once.
  It generates the orchestrator role key under
  `/var/lib/hedgents/secrets/orchestrator-role.key` and writes
  `ORCHESTRATOR_PUBKEY` into `/etc/hedgents/hedgents.env` — both already
  consumed by the riskwatcher unit (`--orchestrator=${ORCHESTRATOR_PUBKEY}`).
- The dashboard is running and reachable on `127.0.0.1:7700`. The
  orchestrator unit declares `Requires=hedgents-dashboard.service`, so
  systemd will start the dashboard first if it isn't already up.
- The five strategy / risk / research daemons are running (you can run
  the orchestrator without them — it will just see an empty fleet
  snapshot and emit `NoAction` every tick).

## Step 1 — install the new release

On the operator host:

```bash
sudo /opt/hedgents/install-hedgents.sh fleet-v0.4.0-rc1
```

This will:
- drop the new `orchestrator-daemon` binary into `/opt/hedgents/bin/`
  (the `install -m 0755 "$SRC/bin/"*-daemon` line already globs it in)
- write `hedgents-orchestrator.service` to
  `/etc/systemd/system/hedgents-orchestrator.service`
- update `hedgents.target` to include the new unit in its `Wants=` list

Sanity-check the binary and unit landed:

```bash
ls -l /opt/hedgents/bin/orchestrator-daemon
systemctl cat hedgents-orchestrator
```

## Step 2 — start the unit

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now hedgents-orchestrator
```

Tail the log to confirm clean boot. You should see, in order:

```
orchestrator starting (dry-run, observe-only) fleet=01fi-dev …
audit log open path=/var/lib/hedgents/logs/orchestrator-audit.jsonl
orchestrator tick loop starting interval_secs=60 mode=dry-run
── allocator snapshot ─────────────────────────────────
  total_aum_usd = N.NNNN    idle_usd = N.NNNN
  stable_yield   deployed=$N.NNNN   apr=N.NN%
  multiply       deployed=$N.NNNN   apr=N.NN%
  hedgedjlp      deployed=$N.NNNN   apr=N.NN%
── recommendation ─────────────────────────────────────
  NoAction|Deposit|Withdraw …
───────────────────────────────────────────────────────
```

```bash
tail -f /var/lib/hedgents/logs/orchestrator.log
```

## Step 3 — verify the audit log is being written

One JSONL line per tick (default 60s):

```bash
tail -f /var/lib/hedgents/logs/orchestrator-audit.jsonl | jq .
```

Expected record shape:

```json
{
  "ts_unix": 1750000000,
  "mode": "dry-run",
  "snapshot": {
    "total_aum_usd": 264.12,
    "idle_usd": 0.41,
    "strategies": [
      {"id": "stable_yield", "deployed_usd": 100.0, "nominal_apr_bps": 1078},
      {"id": "multiply", "deployed_usd": 152.7, "nominal_apr_bps": 1430},
      {"id": "hedgedjlp", "deployed_usd": 11.0, "nominal_apr_bps": 2240}
    ]
  },
  "action": {"action": "no_action", "reason": "all deployed strategies meet hurdle; idle $0.41 below min $5.00"},
  "envelope_result": ""
}
```

## Step 4 — 24h soak

Let it tick for at least 24h. The goal: validate that the pure
`decide` function makes recommendations that match what an operator
would manually approve. Things to look for:

- **Stability.** No oscillation between Deposit and Withdraw across
  consecutive ticks at steady APRs. If you see ping-pong, the risk
  premium hurdles are too tight relative to APR volatility — bump the
  appropriate `risk_premium_*_bps` flag.
- **No spurious withdraws.** A `Withdraw` recommendation should always
  be matched by an obviously bad regime in the snapshot (carry inverted,
  big APR collapse). If the daemon recommends a Withdraw and you would
  not have manually unwound, capture the audit line and revisit the
  hurdle math.
- **Idle handling.** If there is significant `idle_usdc` (>$5) and at
  least one strategy is above hurdle, every tick should recommend a
  Deposit. If the daemon emits `NoAction` despite idle, double-check
  `--max-action-fraction` isn't capping the action below
  `--min-action-usd`.

```bash
# count recommendations by action type over the last 24h
jq -r '.action.action' /var/lib/hedgents/logs/orchestrator-audit.jsonl \
  | tail -n 1440 | sort | uniq -c
```

A healthy steady-state mainnet fleet should produce mostly
`no_action`, with occasional `deposit` on idle-sweep ticks. A
significant fraction of `withdraw` recommendations is a regime signal
worth eyeballing.

## Step 5 — disable when promoting to v0.4.0 execute mode

When v0.4.0 proper lands and you want to grant the orchestrator
authority, stop the rc1 unit first so two instances don't compete on
the same role keypair / libp2p identity:

```bash
sudo systemctl stop hedgents-orchestrator
# … install v0.4.0 (the unit file gets the --execute flags added) …
sudo systemctl start hedgents-orchestrator
```

The audit log is append-only; v0.4.0 entries land with `"mode": "execute"`
in the same file so historical comparisons across the promotion are
trivial.

## Roll-back

```bash
sudo systemctl disable --now hedgents-orchestrator
```

That's it — no state to clean up; the audit log is the only thing the
daemon writes and it's safe to keep.

## Failure modes (Phase 1)

| Symptom                                           | Cause                                                                                     | Fix                                                                                                       |
| -------                                           | -----                                                                                     | ---                                                                                                       |
| Boot fails: "loading orchestrator role key"       | First-run secret gen never finished, or someone deleted `secrets/orchestrator-role.key`. | Re-run `install-hedgents.sh` to regenerate (it skips other keys if they're present).                       |
| Boot fails: "open orchestrator audit log"         | The logs directory is read-only or owned by another user.                                | `chown -R hedgents:hedgents /var/lib/hedgents/logs && chmod 0755 /var/lib/hedgents/logs`.                  |
| `tick failed — continuing` warnings on every tick | Dashboard unreachable (port 7700) or returning 500s.                                     | `systemctl status hedgents-dashboard`; check the dashboard log for crash; restart if needed.              |
| `NoAction` every tick despite obvious imbalance   | One strategy missing from `/strategies` (e.g. `hedgedjlp` daemon down).                  | Bring the missing daemon back; the orchestrator refuses to act on partial snapshots that have no anchor. |
