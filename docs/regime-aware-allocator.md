# Regime-aware allocator (`fleet-pm-stub allocator`)

Introduced in **fleet-v0.2.0**. A capital-allocation decision layer that
sits above the three yield daemons (`stable-yield`, `multiply`,
`hedgedjlp`) and recommends — or executes — capital movements between
them based on live net-APR regime.

Today the fleet's daemons are independently dispatched. When Kamino SOL
borrow rates spike, `multiply` can drop below `stable-yield`'s
risk-free APR and an institutional allocator would unwind multiply and
park in stable-yield until the regime normalises. The allocator
automates that decision.

## Model

The decision function is **pure and deterministic**:

```
decide(strategies, total_aum_usd, idle_usd, cfg) -> AllocatorAction
```

Hurdle definition for each leveraged strategy `S`:

```
hurdle(S) = stable_yield_apr_bps + risk_premium_bps[S]
```

Decision tree:

1. If `stable_yield` isn't in the snapshot → `NoAction` (no anchor).
2. For each leveraged strategy that is **deployed** AND `nominal_apr < hurdle`,
   collect the gap; pick the worst (most negative) → `Withdraw`. The
   amount is `min(deployed_usd, max_action_fraction × total_aum_usd)`.
3. Else if `idle_usd >= min_action_usd`:
   * if some leveraged strategy is **above** hurdle, pick the largest
     positive gap → `Deposit` idle into it;
   * otherwise → `Deposit` idle into `stable_yield`.
4. Else → `NoAction`.

The amount is always capped by `max_action_fraction × total_aum_usd` so
no single tick can move more than the configured fraction of AUM.

## Hurdle defaults (and why)

| flag                          | default | rationale                                                                                            |
| ----------------------------- | ------- | ---------------------------------------------------------------------------------------------------- |
| `--risk-premium-multiply-bps`   | **200** | covers Kamino liquidation, SOL oracle volatility, and the borrow-rate jump risk on the JIT-SOL pair |
| `--risk-premium-hedgedjlp-bps`  | **300** | adds funding-rate risk and JLP-basis risk on top of the multiply-style risks                         |
| `--min-action-usd`              | **5**   | avoids dust rebalances; the smallest live position the soak ever ran was ~$50                        |
| `--max-action-fraction`         | **0.5** | never moves more than half of AUM in a single tick — keeps blast radius bounded                      |

These are conservative starting numbers. The right values come from
walk-forward analysis once the audit log has weeks of data. The CLI
makes them tunable per-call so operators can A/B them without
recompiling.

## CLI

Dry-run (default — prints recommendation, doesn't send envelopes):

```bash
fleet-pm-stub \
    --secrets-dir /var/lib/hedgents/secrets \
    allocator \
    --api-base https://api.hedgents.com \
    --risk-premium-multiply-bps 200 \
    --risk-premium-hedgedjlp-bps 300 \
    --min-action-usd 5 \
    --max-action-fraction 0.5
```

The dry-run path appends a `mode=dry-run` line to the audit log so the
operator has a single chronological JSONL to grep.

Execute mode (also sends the corresponding Assign/Withdraw envelope to
the relevant desk):

```bash
fleet-pm-stub --secrets-dir /var/lib/hedgents/secrets allocator \
    --api-base http://127.0.0.1:7700 \
    --execute \
    --targets-json /var/lib/hedgents/secrets/allocator-targets.json \
    --audit-log /var/lib/hedgents/logs/allocator-audit.jsonl
```

`--targets-json` schema:

```json
{
  "stable_yield": {
    "recipient_agent_id_hex": "deadbeef…",
    "market_b58":  "HubrvD2pCNvVPVnSAR5Y8j8GsBxnxn3VTpdT9KbW18bM",
    "reserve_b58": "9TD2TSv4pENb8VwfbVYg25jvym7HN6iuAR6pFNSrKjqQ"
  },
  "multiply":  { "recipient_agent_id_hex": "cafebabe…" },
  "hedgedjlp": { "recipient_agent_id_hex": "0badcafe…" }
}
```

Action → envelope translation (see `action_to_cmd` in `main.rs`):

| Action                      | Envelope                                                                       |
| --------------------------- | ------------------------------------------------------------------------------ |
| `Withdraw{stable_yield, $}` | `WithdrawStableLend{usdc_lamports = $ × 1e6}`                                  |
| `Withdraw{multiply, $}`     | `AssignMultiply{target_ltv_bps = 0}` — unwinds by setting LTV to 0 next cycle  |
| `Withdraw{hedgedjlp, $}`    | `WithdrawHedgedJlp{jlp_lamports = u64::MAX}` (full unwind — see limits below)  |
| `Deposit{stable_yield, $}`  | `AssignStableLend{usdc_lamports = $ × 1e6}`                                    |
| `Deposit{multiply, $}`      | **skipped** — multiply has no USD sizing parameter, transfer USDC manually     |
| `Deposit{hedgedjlp, $}`     | `AssignHedgedJlp{usdc_lamports = $ × 1e6, delta=0, max_borrow=5000 bps}`       |

Known limits:

* `Withdraw{hedgedjlp}` always fully unwinds because the allocator
  models USD, not JLP lamports.
* `Deposit{multiply}` is skipped — multiply scales itself off whatever
  the daemon wallet holds, so growing it is an off-chain wallet
  transfer plus a re-Assign, not a single envelope.

## Audit log

Every tick appends one JSONL record to `--audit-log`
(`/var/lib/hedgents/logs/allocator-audit.jsonl` by default):

```json
{
  "ts_unix": 1715638800,
  "mode": "execute",
  "snapshot": {
    "total_aum_usd": 1052.34,
    "idle_usd": 12.10,
    "strategies": [
      {"id":"stable_yield","deployed_usd":540.0,"nominal_apr_bps":701},
      {"id":"multiply","deployed_usd":500.0,"nominal_apr_bps":496},
      {"id":"hedgedjlp","deployed_usd":0.0,"nominal_apr_bps":1500}
    ]
  },
  "action": {
    "action": "withdraw",
    "strategy": "multiply",
    "amount_usd": 500.0,
    "reason": "carry inverted: multiply earning 4.96% < hurdle 9.01% (7.01% + 2.00% risk premium)"
  },
  "envelope_result": "sent"
}
```

## Suggested cron

Hourly dry-run + weekly review of executed actions:

```cron
# every hour, dry-run + audit-log only
0 * * * * /usr/local/bin/fleet-pm-stub \
    --secrets-dir /var/lib/hedgents/secrets allocator \
    --api-base http://127.0.0.1:7700 \
    >> /var/log/hedgents/allocator.log 2>&1
```

Once an operator has watched several days of dry-run recommendations
and is confident, flip a separate cron job to `--execute`. Recommended
sequence:

1. Run dry-run for at least 1 week. Confirm `NoAction` dominates and
   that the rare `Withdraw` / `Deposit` decisions match human
   judgement.
2. Promote to `--execute` once on, then review the JSONL every Monday.
3. Tune `--risk-premium-*-bps` based on observed false-positive
   unwinds.
