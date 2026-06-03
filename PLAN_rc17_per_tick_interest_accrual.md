# rc17 plan — Per-tick Δ × Δrate integration for pure_interest_usd

## Why

The current `realtime_protocol_pnl_usdc` metric (rc44/45/46) computes:

```text
pure_interest = current_ctoken × (current_rate − baseline_rate) × current_price
              − (current_sol_borrowed − baseline_sol_borrowed) × current_sol_price
```

Baseline = first observed non-zero `(ctoken, rate, sol_borrowed)`
triple in `chain_aum_snapshots`. The metric has **flow-mixing bugs**:

1. **New deposits inflate the gain.** Operator deposits more SOL →
   `current_ctoken` grows from N to N+K. The formula then attributes
   `K × (current_rate − baseline_rate) × price` of gain to interest
   accrual — but those K cTokens weren't present at the baseline,
   they earned only from deposit time forward.

2. **Withdraws hide the gain.** Operator unwinds → `current_ctoken`
   shrinks from N to N−K. The withdrawn K's accrued interest is
   forgotten (it's no longer in `current_ctoken`).

3. **Borrow-side mirror.** Same shape on the SOL-debt side: a new
   borrow event makes the formula attribute "extra" borrowed
   principal as additional interest.

Earlier today the operator manually reset the baseline (NULL-ing
multiply rows in sqlite) to stop the metric showing −$208. That
fix was tactical. The math itself is still wrong; the next
non-trivial deposit/withdraw will drift again.

## The right shape

A **per-tick Δ × Δrate integration**: at each tick the metric
records the *incremental* accrual since the previous tick, using
the cToken balance and rate AS OF THAT TICK, NOT the baseline.

```text
Δaccrual_t = current_ctoken_t  × (rate_t − rate_{t-1})  × price_t
           − current_borrow_t  × (sol_borrow_rate_t)    × elapsed_t
total_accrual = Σ Δaccrual_t over all ticks since baseline
```

Properties:
- A deposit between tick t−1 and t increases `current_ctoken_t`. The
  (rate_t − rate_{t-1}) factor is the same. The new ctokens earn
  one tick's worth of accrual that tick — correct.
- A withdraw decreases `current_ctoken_t`. The earlier ticks already
  accrued the withdrawn ctokens' interest into the running sum,
  preserved. The withdraw moment itself: `current_ctoken_t` is
  post-withdraw, so the (rate_t − rate_{t-1}) only accrues to the
  surviving balance — correct.
- Rate jumps (Kamino changes the supply APR): the full Δrate
  accrues against `current_ctoken_t`, capturing the regime change
  for the actual capital present.

Monotonic by construction (assuming rates are monotonic non-
negative, which they are for supply APR; for debt the sign flips
but the integration shape is identical).

## Edge cases the integration handles correctly

- **First observed snapshot.** Δrate is undefined (no previous tick).
  Initialise the accumulator to 0; first non-zero accrual is at the
  second tick. Same as the baseline metric.
- **Snapshot gap (daemon down for 30 min).** Δrate accrues against
  the current ctoken balance; the missing-interval ctoken changes
  (deposits/withdraws that happened while down) are lost. Bound by
  daemon uptime, not a math issue.
- **Tick interval drift.** The 5-second sampler isn't perfectly
  paced; Δrate × ctoken doesn't require precise timing because
  rates accumulate continuously in the underlying protocol. The
  difference between tick timestamps doesn't appear in the formula.
- **Negative interest spike (rate decrease).** Possible in theory
  (Kamino bad-debt provisioning). The integration handles it
  signed: accumulator decreases, reflecting real loss.

## Implementation surfaces

### A. Storage

`chain_aum_snapshots` already records `(ts_unix, multiply_jitosol_
ctoken_balance, multiply_jitosol_underlying_lamports,
multiply_sol_borrowed_lamports, stable_yield_ctoken_balance,
stable_yield_usd, multiply_usd)` per tick. That's enough to compute
the integration in either of two ways:

**Option A1 — On-the-fly query.** Each `/strategies` call queries:
- `prev_row` = max(ts < now − 5s) row from chain_aum_snapshots
- `current_row` = max(ts) row
- Compute Δaccrual using the two rows + Σ over all historical rows

The Σ over history is expensive (1 row per 5s = 17,000 rows/day).
Don't recompute per /strategies call. Cache the cumulative.

**Option A2 — Persisted accumulator column.** Add two columns to
`chain_aum_snapshots`:
- `multiply_pure_interest_accrued_usd_micro`
- `stable_yield_pure_interest_accrued_usd_micro`

Each tick's sampler computes `Δaccrual` from the previous row and
the new row, adds to the previous row's accumulator, writes the
new accumulator. `/strategies` reads the latest row's accumulator
directly. **Recommended.**

### B. Sampler change

`tools/fleet-dashboard-server/src/ingest/aum_sampler.rs` is the
right host. It already writes one chain_aum_snapshots row per
tick. Add the computation:

```rust
// Before writing the new row:
let prev = store.latest_aum_snapshot().await.ok().flatten();
let prev_multiply_accrual = prev.as_ref()
    .and_then(|p| p.multiply_pure_interest_accrued_usd_micro)
    .unwrap_or(0);

let delta_multiply = compute_multiply_delta(prev.as_ref(), &current_breakdown);
let new_multiply_accrual = prev_multiply_accrual + delta_multiply;

// ...same for stable_yield.

// Now write new row with the updated accumulator columns.
```

`compute_multiply_delta` is pure logic:
- If prev or current is missing required fields → return 0
- Else: `delta_collateral_usd = ctoken_t × Δrate × jitosol_price_t`
  - rate_t = `m.jitosol_underlying_lamports / m.jitosol_ctoken_balance`
  - jitosol_price = m.deposited_usd_micro / m.jitosol_underlying_lamports × 1e9
- `delta_debt_usd = elapsed_secs × sol_borrowed_t × sol_borrow_rate / SECS_PER_YEAR`
  - sol_borrow_rate comes from the live rates_snapshot
  - sol_price = m.borrowed_usd_micro / m.sol_borrowed_lamports × 1e9
- Returns `delta_collateral_usd - delta_debt_usd` in micro-USD

### C. API surface

`/strategies.realtime_protocol_pnl_usdc` keeps the existing field
name but the VALUE is now the accumulated integration, not the
baseline-vs-current. Change is invisible to the frontend's typed
contract; only the docstring updates.

Optional: add a separate `pure_interest_method` enum field that
labels the metric source so the frontend can render a tooltip
("v0.4.17: Δ × Δrate integration"). Defer to a follow-up if not
needed for transparency.

### D. Migration

- The new accumulator columns start NULL on existing rows.
- The sampler's first post-rc17 tick computes Δaccrual using the
  PREVIOUS row's NULL accumulator → treats NULL as 0 → first
  accrual row writes the first delta as the accumulator.
- Existing rows keep NULL. The `/strategies` reader falls back to
  `realtime_protocol_pnl_usdc = NULL` for rows with NULL
  accumulator, then to the rc46 baseline-vs-current math for the
  first tick after rc17.

No on-chain migration needed. No backfill required (we lose the
~24h of pre-rc17 history but that history was wrong anyway).

### E. Tests

- `pure_logic_delta_zero_when_inputs_match` — Δrate = 0 → Δaccrual = 0.
- `pure_logic_handles_deposit_event` — ctoken jumps but rate doesn't;
  Δaccrual = 0 (no time elapsed in rate).
- `pure_logic_handles_withdraw_event` — ctoken shrinks; Δaccrual
  computed against the post-withdraw ctoken correctly (prior
  history already accumulated).
- `pure_logic_handles_rate_jump` — rate jumps 100bps; Δaccrual =
  ctoken × 0.01 × price.
- `pure_logic_borrow_side_signs` — debt accrual subtracts from
  collateral accrual.
- `pure_logic_handles_missing_inputs` — NULL ctoken or NULL rate
  → return 0, never panic.

## Time estimate

- Implementation: 3-4 hours
- Tests: 1-2 hours
- Migration + verification on production data: 1 hour
- Total: ~5-7 hours

## Why I'm NOT shipping this tonight

Real-money accounting. The session has been ~30 hours of focused
work and pure-interest math has subtle correctness conditions —
the kind of subtle that survives unit tests and shows up wrong on
the dashboard a week later. Fresh-eyes review is worth more here
than shipping at 2 AM.

The plan is complete and on disk; rc17 picks up tomorrow as a
focused engagement.
