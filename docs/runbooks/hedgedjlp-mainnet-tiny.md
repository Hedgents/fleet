# Hedged-JLP daemon — mainnet tiny-position runbook

WARNING: THIS USES REAL MONEY ON SOLANA MAINNET. Every step in this runbook
is mandatory. Do not skip the pre-flight checklist.

## Goal

Deploy $200 USDC into a delta-hedged JLP basis trade via the
hedgedjlp-daemon. The position should appear on-chain (Solscan +
Jupiter Perps UI) and the daemon's telemetry log should record both
the JLP holding and the hedge notional.

This is **not an earnings test**. The $200 is sized to verify the
JLP-buy + perp-hedge round-trip works on mainnet, that the keeper
fills the open-position requests within reasonable latency, and that
the daemon's reported numbers match Solscan / Jupiter Perps state.

Earnings come over weeks/months at much larger size — see "Earning
expectations" below for the honest math.

## Pre-flight checklist

Before running ANY mainnet command, confirm each item below.

- [ ] Devnet smoke (M3) has been run end-to-end: `assign-hedged-jlp`
      produces a Report. On devnet the Report has `ok=false error_code=5`
      (placeholder pubkeys, no live Jupiter Perps program state) — the
      wiring is what's verified, not the chain submit.
- [ ] Manual-approval flow (M4) is verified on devnet: two-step
      Assign → queued → Approve → execution Report. Both the
      same-orchestrator (positive) and cross-orchestrator (REJECTED)
      paths have been exercised.
- [ ] Telemetry (M10) is verified on devnet: `hedgedjlp-pnl.jsonl`
      writes one line per beacon tick with `jlp_lamports`,
      `hedge_notional_usdc`, and (placeholder, zero) APR fields. v0
      telemetry's APR is a known-zero stub; that's fine.
- [ ] Withdrawal path (M11) is verified on devnet:
      `WithdrawHedgedJlp` envelope round-trips through Approve and
      emits a ReportHedgedJlp with non-zero `jlp_burned_lamports`.
- [ ] You have a Solana mainnet wallet keypair file with:
      - **At least 0.1 SOL** for transaction fees, ATA rent, and
        Jupiter Perps position-account init rent (each of SOL/ETH/BTC
        opens its own position account).
      - **At least $200 USDC** in the wallet's USDC ATA.
        Mainnet USDC mint: `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`.
- [ ] You have a private RPC URL. The hedged-JLP flow submits ~4
      transactions back-to-back (1 JLP buy + 3 perp open requests).
      Public mainnet RPC WILL rate-limit during this burst and may
      serve stale state mid-flow. Recommended: Helius free tier,
      QuickNode, Triton, Alchemy. **DO NOT use
      `https://api.mainnet-beta.solana.com` for live submits.**
- [ ] You understand the genesis-hash bail. The daemon checks the RPC's
      genesis hash on boot against `--network mainnet`'s expected hash.
      A mismatch (e.g., devnet RPC paired with `--network mainnet`)
      bails with a clear error before any envelope is processed.
- [ ] You have the **mainnet pubkeys** for:
      - JLP pool: `5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq`
      - JLP mint: `27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4`
      - USDC mint: `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`
      - Jupiter Perps program: `PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu`
      - Per-asset custody pubkeys (USDC, SOL, ETH, BTC) — these
        **must be looked up live** from the pool account or Jupiter
        Perps documentation. Placeholder pubkeys baked into the source
        will likely need replacement before live submits succeed.
      - Operator: verify each pubkey before deploy. As of 2026-05-06
        the canonical source is Jupiter's docs at
        https://station.jup.ag/docs/perpetual-exchange and the
        Jupiter Perps app at https://jup.ag/perpetuals.
      - Do NOT hardcode pubkeys taken from secondary sources without
        verification. Pubkey mistakes here = funds sent to the wrong
        place or transactions that fail at simulation.
- [ ] You have at least 60 minutes of focused time to monitor the boot,
      the sim-only dry run, the real submit, and the keeper-fill
      verification. Hedged JLP has more moving parts than stable-yield,
      and the keeper-fill window can stretch a few slots.

If any checklist item is unchecked, **do not proceed**. Address it first.

## Step 1 — Generate role keys + wire the mainnet wallet

```bash
mkdir -p ~/01fi-mainnet/{hedgedjlp,orch}

# 32-byte raw Ed25519 seed for the daemon's mesh identity:
openssl rand 32 > ~/01fi-mainnet/hedgedjlp/hedgedjlp-role.key
openssl rand 32 > ~/01fi-mainnet/orch/orchestrator-role.key
chmod 600 ~/01fi-mainnet/hedgedjlp/hedgedjlp-role.key
chmod 600 ~/01fi-mainnet/orch/orchestrator-role.key

# Place your existing funded mainnet wallet here:
cp /path/to/your/mainnet-wallet.json ~/01fi-mainnet/hedgedjlp/solana-wallet.json
chmod 600 ~/01fi-mainnet/hedgedjlp/solana-wallet.json

# Verify the wallet pubkey + balances:
WALLET=$(solana-keygen pubkey ~/01fi-mainnet/hedgedjlp/solana-wallet.json)
echo "wallet: $WALLET"
solana balance "$WALLET" --url <YOUR_MAINNET_RPC_URL>

# Verify the USDC ATA balance (USDC mint EPjF...t1v):
spl-token balance EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
    --owner "$WALLET" --url <YOUR_MAINNET_RPC_URL>
```

You should see at least 0.1 SOL and at least 200 USDC (200.000000 with
6-decimal display).

## Step 2 — Boot hedgedjlp-daemon in mainnet mode (sim-only first)

In a terminal you'll keep open for the entire session:

```bash
RUST_LOG=info,libp2p=warn,zerox1_node_enterprise=info \
cargo run --release -p hedgedjlp-daemon -- \
    --secrets-dir ~/01fi-mainnet/hedgedjlp \
    --wallet ~/01fi-mainnet/hedgedjlp/solana-wallet.json \
    --rpc-url <YOUR_MAINNET_RPC_URL> \
    --network mainnet \
    --i-understand-this-is-mainnet \
    --listen /ip4/0.0.0.0/tcp/19311 \
    --max-position-usdc-lamports 250000000 \
    --simulate-only true \
    --require-approval true \
    --rebalance-interval-secs 600 \
    --telemetry-log ~/01fi-mainnet/hedgedjlp-pnl.jsonl \
    --telemetry-interval-secs 60
```

Key flags:
- `--network mainnet` + `--i-understand-this-is-mainnet`: redundant
  acknowledgments. The daemon refuses mainnet without both.
- `--max-position-usdc-lamports 250000000`: $250 ceiling — gives a
  small amount of headroom over the $200 test. caps.rs enforces a
  compile-time hard ceiling; this CLI cap brings it down further.
- `--simulate-only true`: **first run is dry — no broadcast.** The
  daemon will build the JLP buy + 3 perp open-request ixns and run
  `build_sign_simulate` against the mainnet RPC, but will not submit.
  This verifies wiring against real mainnet state (real pool account,
  real custodies, real ATAs, real account layouts) without spending a
  cent of the position.
- `--require-approval true`: every Assign queues, awaiting an Approve
  envelope on the same conversation_id. The daemon defaults to true
  on mainnet, but pass it explicitly so the boot log confirms it.
- `--rebalance-interval-secs 600`: rebalancer wakes every 10 minutes
  to read live delta + borrow rates. Longer than devnet's tighter
  interval to reduce mesh + RPC chatter and avoid spamming hedge
  adjustments while observing.
- `--telemetry-log` + `--telemetry-interval-secs 60`: beacon tick
  fires every 60s, records a snapshot in `hedgedjlp-pnl.jsonl`.

Watch the boot log for:

```
INFO Loaded identity from ".../hedgedjlp/.runtime-keypair-hedgedjlp"
     peer_id=12D3KooW...  agent_id=<HEX>
INFO Genesis hash verified: mainnet
INFO Listening on /ip4/0.0.0.0/tcp/19311
INFO Rebalancer started  interval_secs=600
```

If you see `Genesis hash mismatch` — the RPC is wrong. Stop and fix.

Save the 64-character hex `agent_id` as `HEDGEDJLP_AGENT_ID` for
Step 3.

## Step 3 — Sim-only dry run: send a $200 Assign

In a separate terminal:

```bash
HEDGEDJLP_AGENT_ID=<paste from Step 2>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19311 \
    --recipient-agent-id "$HEDGEDJLP_AGENT_ID" \
    --timeout-secs 90 \
    assign-hedged-jlp \
        --usdc-lamports 200000000 \
        --target-delta-bps 0 \
        --max-borrow-rate-bps 5000
```

Field meanings:
- `--usdc-lamports 200000000` = $200 (USDC has 6 decimals).
- `--target-delta-bps 0` = full delta-neutral (hedge entire non-stable
  share of JLP). Allowed range ±10000 (±100%).
- `--max-borrow-rate-bps 5000` = 50% APR ceiling on Jupiter Perps
  borrow rate per custody. Above this, the rebalancer's borrow-rate
  watch will emit `EscalateRisk(Warning)` and pause further
  rebalances.

Expected: stub prints an ack `Report received` with `ok=true`,
`jlp_acquired_lamports=0`, no `tx_signatures`. That's the **queued**
state. The daemon log shows:

```
INFO AssignHedgedJlp received  usdc_lamports=200000000 target_delta_bps=0 max_borrow_rate_bps=5000
INFO Caps validated  cap=250000000 requested=200000000
INFO AssignHedgedJlp queued — awaiting Approve  conv=<HEX>
INFO NeedsApproval Escalate emitted  conv=<HEX>
INFO report sent ok=true jlp_acquired=0 conv=<HEX>
```

**Capture the `conv` hex from the daemon log.** You need it to approve.

Now send the Approve from the same orchestrator `--secrets-dir`:

```bash
CONV=<paste conv hex from daemon log>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19311 \
    --recipient-agent-id "$HEDGEDJLP_AGENT_ID" \
    --timeout-secs 120 \
    approve --conv-hex "$CONV"
```

Because `--simulate-only=true`, the daemon will:
1. Verify `env.sender` matches the original Assign sender.
2. Re-validate caps (audit-fix I2).
3. Build the JLP buy ixn via `add_liquidity_ix`.
4. Read live pool state via `read_pool_state` to compute current
   delta (typically ~65% non-stable assets).
5. Compute `hedge_short_usd ≈ $130` (65% of $200) split pro-rata
   across SOL / ETH / BTC by their relative pool weights.
6. Build `create_increase_position_request_ix` for each non-stable
   custody.
7. Run `build_sign_simulate` on each tx against the mainnet RPC (no
   broadcast).
8. Compose a Report with `jlp_acquired_lamports`,
   `hedge_notional_usdc≈130000000`, `current_delta_bps`, ~4
   `tx_signatures` (sim hashes, not chain hashes).
9. Send a final Report.

Expected daemon log:

```
INFO Approve received — executing queued AssignHedgedJlp  conv=<HEX>
INFO hedgedjlp::run_or_simulate  simulate_only=true usdc_lamports=200000000
INFO JLP buy simulation succeeded  units_consumed=<N>
INFO read_pool_state  total_value=<USD> non_stable_share=0.6500
INFO hedge sizing  hedge_notional_usd=130 sol=<%> eth=<%> btc=<%>
INFO perp open SOL simulation succeeded  units_consumed=<N>
INFO perp open ETH simulation succeeded  units_consumed=<N>
INFO perp open BTC simulation succeeded  units_consumed=<N>
INFO report sent ok=true jlp_acquired=<lam> hedge_notional_usdc=130000000 conv=<HEX>  (sim-only)
```

Stub log:

```
Report received: ReportHedgedJlp {
    header: ReportHeader { ok: true, error_code: 0, ... },
    jlp_acquired_lamports: <~1900000000>,    // ~1.9 JLP, 9 decimals
    hedge_notional_usdc: 130000000,          // ~$130
    current_delta_bps: <small residual>,
    tx_signatures: [<sim_sig_1>, <sim_sig_2>, <sim_sig_3>, <sim_sig_4>],
}
```

If `ok=false` here:
- `error_code=5`: simulation failed on chain — read full daemon log.
  Likely causes: wrong custody pubkey, pool capacity exhausted, oracle
  stale, account layout mismatch, slippage exceeded. **Diagnose
  before proceeding.**
- `error_code=6`: ixn-build failed before chain. Likely a bad pool /
  mint / custody pubkey. **Verify pubkeys against Jupiter's docs.**

**Do NOT flip `--simulate-only=false` until the sim run returns
`ok=true` with all 4 sub-tx sims green.**

## Step 4 — Real $200 hedged-JLP open

Once the sim run is green: Ctrl-C the daemon and restart it with
`--simulate-only false`.

```bash
RUST_LOG=info,libp2p=warn,zerox1_node_enterprise=info \
cargo run --release -p hedgedjlp-daemon -- \
    --secrets-dir ~/01fi-mainnet/hedgedjlp \
    --wallet ~/01fi-mainnet/hedgedjlp/solana-wallet.json \
    --rpc-url <YOUR_MAINNET_RPC_URL> \
    --network mainnet \
    --i-understand-this-is-mainnet \
    --listen /ip4/0.0.0.0/tcp/19311 \
    --max-position-usdc-lamports 250000000 \
    --simulate-only false \
    --require-approval true \
    --rebalance-interval-secs 600 \
    --telemetry-log ~/01fi-mainnet/hedgedjlp-pnl.jsonl \
    --telemetry-interval-secs 60
```

Send the Assign again (same command as Step 3):

```bash
RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19311 \
    --recipient-agent-id "$HEDGEDJLP_AGENT_ID" \
    --timeout-secs 90 \
    assign-hedged-jlp \
        --usdc-lamports 200000000 \
        --target-delta-bps 0 \
        --max-borrow-rate-bps 5000
```

Wait for the queued ack Report. Capture the new `conv` hex. Send Approve:

```bash
CONV=<new conv hex from daemon log>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19311 \
    --recipient-agent-id "$HEDGEDJLP_AGENT_ID" \
    --timeout-secs 180 \
    approve --conv-hex "$CONV"
```

Expected sequence on chain:
1. JLP buy lands first (single tx, ~$200 USDC → ~1.9 JLP).
2. Three perp open-position requests submitted next (one each for
   SOL / ETH / BTC).
3. Jupiter Perps keeper picks up the requests within 1-3 slots
   typical and creates the actual short positions.

Expected final Report:

```
ReportHedgedJlp {
    header: ReportHeader { ok: true, error_code: 0, ... },
    jlp_acquired_lamports: <~1900000000>,
    hedge_notional_usdc: 130000000,
    current_delta_bps: <small residual>,
    tx_signatures: [<jlp_buy_sig>, <sol_open_sig>, <eth_open_sig>, <btc_open_sig>],
}
```

Daemon log:
```
INFO Approve received — executing queued AssignHedgedJlp  conv=<HEX>
INFO hedgedjlp::run_or_simulate  simulate_only=false usdc_lamports=200000000
INFO JLP buy submitted  signature=<base58_jlp>
INFO JLP buy confirmed  signature=<base58_jlp>
INFO perp open SOL submitted  signature=<base58_sol>
INFO perp open ETH submitted  signature=<base58_eth>
INFO perp open BTC submitted  signature=<base58_btc>
INFO report sent ok=true jlp_acquired=<lam> hedge_notional=130000000 conv=<HEX>
```

Look up each `<base58_signature>` on https://solscan.io/ to confirm.

## Step 5 — Verify the position

Three independent ways to confirm the round-trip landed:

1. **Solscan tx pages**
   For each of the 4 signatures from the Report:
   - JLP buy tx: `https://solscan.io/tx/<jlp_buy_sig>`
     Instructions should show `Jupiter Perps: AddLiquidity`,
     SPL Token transfer of 200 USDC out of your wallet's USDC ATA,
     and JLP mint into the wallet (~1.9 JLP at 9-decimal display).
   - Each perp open tx (SOL/ETH/BTC): `https://solscan.io/tx/<sig>`
     Instruction list should show
     `Jupiter Perps: CreateIncreasePositionRequest`. The actual
     position-creation happens later when the keeper executes the
     request — you should see a follow-on tx within 1-3 slots
     showing `IncreasePosition` from a keeper signer.

2. **Jupiter Perps UI**
   `https://jup.ag/perpetuals`
   Connect the wallet from Step 1, navigate to Positions, and
   confirm:
   - Three open SHORT positions (SOL / ETH / BTC).
   - Notional sizes roughly proportional to each asset's pool weight,
     summing to ~$130.
   - Entry prices match Pyth oracle marks at the keeper-fill slot.
   - Funding / borrow rates displayed alongside each position.

3. **Telemetry log**
   ```bash
   tail -5 ~/01fi-mainnet/hedgedjlp-pnl.jsonl
   ```
   Each line is a JSON snapshot. After the open, the most recent
   line should show:
   - `jlp_lamports: ~1900000000`
   - `hedge_notional_usdc: 130000000`
   - `current_delta_bps: <small residual>`
   - `jlp_yield_apr_bps: 0`  ← v0 placeholder, known-zero.
   - `hedge_borrow_apr_bps: 0`  ← v0 placeholder, known-zero.
   - `net_apr_bps: 0`  ← v0 placeholder, known-zero.

   Real APR computation lands in a later milestone (decoder work
   not in v0). The presence of populated `jlp_lamports` and
   `hedge_notional_usdc` is the load-bearing signal here.

If any of the three diverge — e.g., Solscan shows the open requests
but the Jupiter Perps UI shows nothing after 5 minutes — the keeper
may have rejected the request. Investigate before doing anything
else. Position state on Jupiter Perps is the source of truth, not
the telemetry stub.

## Earning expectations (be honest)

JLP yield: typically **30-50% APR** (last-7d running average from
Jupiter's published metrics), driven by trader fees and funding paid
into the pool.

Jupiter Perps borrow rate: **2-15% APR** depending on custody
utilization (Gauntlet's jump-rate curve).

Net target: **~15-25% APR**.

On a $200 deployed position:

| Net APR | $/year | $/month | $/day |
|---------|--------|---------|-------|
| 15%     | $30    | $2.50   | $0.082 |
| 20%     | $40    | $3.33   | $0.110 |
| 25%     | $50    | $4.17   | $0.137 |

Roughly **$0.10-0.15/day**. **This runbook is not validating
earnings.** Real earnings come over weeks/months at much larger size.
The $200 exists solely to verify the round-trip works on mainnet
without risking meaningful capital.

## Step 6 — 24-72h operational watch

Leave the daemon running for at least 24 hours, ideally 72. During
that window:

- **Solscan**: re-load the Jupiter Perps UI every few hours,
  confirm the 3 short positions remain open, no liquidations.
- **Telemetry**: `tail -f ~/01fi-mainnet/hedgedjlp-pnl.jsonl` should
  show one new line every 60s. Gaps mean the daemon's beacon loop
  stalled — investigate logs.
- **Borrow rates**: check each custody's borrow rate in the Jupiter
  Perps UI. If any rises above your `--max-borrow-rate-bps` (50%
  APR), the rebalancer's borrow-rate watch will emit
  `EscalateRisk(Warning)` and **pause further rebalances**. M9 v0
  does NOT auto-unwind on this — you decide whether to wait it out
  or unwind manually.
- **Pyth oracle health**: if a price feed stales, JLP NAV reads may
  be off. Researcher's M5 price watcher signals this if running
  alongside.
- **Daemon logs**: scan for `Approve REJECTED — sender does not
  match`. Should be zero in a clean run. Non-zero means someone is
  probing the approval gate — investigate.
- **Liquidation risk**: short positions on Jupiter Perps can be
  liquidated if collateral falls below maintenance. With $200 sized
  to ~$130 notional (sub-1x leverage at the position level, but JLP
  itself moves with the assets), the buffer is wide. Still, watch
  for adverse moves.

## Step 7 — Rollback / withdrawal

To unwind the $200 hedged-JLP position:

1. Send a `WithdrawHedgedJlp` envelope from the orchestrator with
   `jlp_lamports=u64::MAX` for a full unwind including any accrued
   yield (or a partial value to keep some position open):

   ```bash
   RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
       --secrets-dir ~/01fi-mainnet/orch \
       --listen /ip4/127.0.0.1/tcp/19399 \
       --bootstrap /ip4/127.0.0.1/tcp/19311 \
       --recipient-agent-id "$HEDGEDJLP_AGENT_ID" \
       --timeout-secs 90 \
       withdraw-hedged-jlp --jlp-lamports 18446744073709551615
   ```

2. Approve from the same orchestrator `--secrets-dir` (capture the
   new `conv` hex from daemon log):

   ```bash
   RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
       --secrets-dir ~/01fi-mainnet/orch \
       --listen /ip4/127.0.0.1/tcp/19399 \
       --bootstrap /ip4/127.0.0.1/tcp/19311 \
       --recipient-agent-id "$HEDGEDJLP_AGENT_ID" \
       --timeout-secs 180 \
       approve --conv-hex "$CONV"
   ```

3. Daemon executes:
   - Builds 3 `create_decrease_position_request_ix` ixns (one per
     SOL/ETH/BTC short) and submits them.
   - Waits for keeper fills (1-3 slots typical).
   - Builds `remove_liquidity_ix` and submits.
   - Composes a Report with `jlp_burned_lamports` and 4
     `tx_signatures`.

4. Funds return to the wallet's USDC ATA (~$200 minus fees + any
   P&L from the brief hold).

5. Verify on Solscan: each tx page should show
   `Jupiter Perps: CreateDecreasePositionRequest` (×3) and
   `Jupiter Perps: RemoveLiquidity` (×1).

If the daemon's WithdrawHedgedJlp path fails for any reason, fall
back to the manual path: connect the same wallet at
https://jup.ag/perpetuals, close each short position manually, then
go to the JLP page and burn JLP for USDC.

**v0 caveat**: the unwind Report's `usdc_returned_lamports` is a
proxy from `jlp_to_burn`, not a real post-tx wallet balance read.
Trust Solscan + the wallet's actual USDC ATA balance for the truth.

## Failure-mode triage

| Symptom | Likely cause | Fix |
|---|---|---|
| Daemon bails at boot: `RPC URL ... returned genesis hash X but --network mainnet expects Y` | Wrong RPC URL (e.g., devnet RPC paired with `--network mainnet`) | Use a true mainnet RPC. |
| Daemon bails at boot: `--network=mainnet requires --i-understand-this-is-mainnet flag` | Missing ack flag | Add the flag — this is a deliberate guard against accidental mainnet runs. |
| Assign exits non-zero, daemon log shows `AssignHedgedJlp received` but no `queued` line | Cap rejected — `usdc_lamports` exceeded `--max-position-usdc-lamports`, OR `target_delta_bps` outside ±10000 | Lower the Assign or raise the CLI cap (still bounded by the compile-time MAX). Bring `target_delta_bps` into ±10000. |
| Approve exits non-zero, daemon log shows `Approve REJECTED — sender does not match` | Approving from a different orchestrator role-key than the one that enqueued the Assign | Use the **same** `--secrets-dir` for Approve as for Assign. The audit-fix C1 sender check is operating correctly. |
| Final Report `ok=false error_code=5` | Sim or submit failed on chain — could be insufficient SOL for gas/rent, JLP pool capacity exhausted, oracle stale, Jupiter Perps program issue, slippage exceeded, custody utilization at 100% | Read full daemon log for the chain-side error string. Verify wallet has SOL. Check Jupiter status page at https://jup.ag and the Jupiter Discord for ongoing issues. |
| Final Report `ok=false error_code=6` | Ixn-build failed before chain — almost always a bad pool / mint / custody pubkey, or a discriminator mismatch from a Jupiter Perps IDL update | Verify constants in source against current Jupiter Perps documentation. Custody pubkeys must be looked up live, not from the placeholder source values. |
| Hedge open requests submitted but positions don't appear in Jupiter Perps UI after 5min | Keeper backlog or rejection (insufficient pool liquidity, slippage breach, custody at borrow cap, oracle staleness) | Check the Jupiter Perps Discord for keeper status. Open-position requests can be cancelled via a separate `cancel_increase_position_request_ix` if they remain unfilled — manually via the Jupiter Perps UI's "pending orders" view. |
| Borrow-rate watch emits `EscalateRisk(Warning) — borrow rate above max` | Custody utilization spiked above your `--max-borrow-rate-bps` ceiling | Daemon won't auto-unwind in v0. Decide: wait it out (utilization normalizes), or send `WithdrawHedgedJlp` to close. |
| Telemetry log silent (no new lines) | Beacon loop stalled or RPC hiccup | Check daemon logs for telemetry tick errors; restart daemon if stalled. |
| JLP buy lands but one or more perp open requests fail | Pool ran out of capacity for that custody between sim and submit, or oracle staleness for that asset | The position is in a partial-hedge state. Close the partially-filled hedges via Jupiter Perps UI manually, then re-run the Assign with smaller size or wait for capacity. |

## Cleanup / teardown

After a successful $200 round-trip + 24-72h watch:

1. Issue the `WithdrawHedgedJlp` (Step 7) — funds return to the
   wallet USDC ATA.
2. Stop the daemon: Ctrl-C in the daemon's terminal. Clean shutdown
   runs through `tokio::signal`.
3. Archive the telemetry log:
   ```bash
   mv ~/01fi-mainnet/hedgedjlp-pnl.jsonl \
      ~/01fi-mainnet/logs/hedgedjlp-mainnet-$(date +%Y-%m-%d).jsonl
   ```
4. **Rotate the role key** if any logs were shared externally:
   ```bash
   openssl rand 32 > ~/01fi-mainnet/hedgedjlp/hedgedjlp-role.key
   ```
   On next boot the daemon picks up the new key — note that the
   `agent_id` changes too, so the orchestrator's
   `--recipient-agent-id` needs updating.

## Step 8 — Document the result

Append to this runbook (or create a sibling file
`docs/runbooks/hedgedjlp-mainnet-results.md`) with:

- Date and time of the open
- Solscan links for all 4 open txs (JLP buy + 3 perp opens)
- Solscan links for the 4 unwind txs (3 perp closes + JLP burn)
- Final Report payload from open and from withdraw
- Telemetry first/last snapshot for the watch window
- Final wallet USDC balance vs starting balance (the difference is
  the realized P&L — expect a few cents to a few dollars on $200
  over a 24-72h window, dominated by JLP yield minus borrow costs
  and round-trip fees)
- Any incidents (genesis bail, REJECTED approves, RPC hiccups,
  keeper delays, borrow-rate warnings)

This documentation is the **proof of mainnet round-trip** for the
hedgedjlp-daemon's v0 milestone tracking.

## Emergency contacts (operator's own)

- Solana mainnet status: https://status.solana.com/
- Jupiter Perps app (manual close fallback): https://jup.ag/perpetuals
- Jupiter docs: https://station.jup.ag/docs/perpetual-exchange
- Jupiter Discord: https://discord.gg/jup
- Solscan: https://solscan.io/

## Mainnet pubkeys to verify before deploy

These constants ship in the daemon source. Verify each against
Jupiter's docs at deploy time — they may have changed:

- JLP pool: `5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq`
- JLP mint: `27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4`
- USDC mint: `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`
- Jupiter Perps program: `PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu`
- USDC custody, SOL custody, ETH custody, BTC custody: must be looked
  up live from the pool account or Jupiter Perps docs. **Placeholder
  pubkeys baked into the source likely need replacement.**

## What's NOT in v0

These limitations are documented elsewhere in the plan. You should be
aware of them before proceeding:

- **APR computation is a placeholder**: `jlp_yield_apr_bps`,
  `hedge_borrow_apr_bps`, and `net_apr_bps` in the telemetry log are
  hardcoded to 0 in v0. Real APR derivation from Jupiter's published
  metrics + each custody's borrow-rate curve lands in a later
  milestone (decoder work).
- **Real `usdc_returned_lamports` post-tx balance read in unwind
  Reports**: v0 uses `jlp_to_burn` as a proxy. Trust the wallet's
  actual USDC ATA balance for ground truth.
- **Auto-unwind on borrow-rate exceedance**: v0 only pauses the
  rebalancer and emits a Warning. Operator-driven unwind via
  `WithdrawHedgedJlp` is the v0 response.
- **Per-custody position tracking**: `ActivePosition` uses synthetic
  PDA derivation; works correctly for sim but may need adjustment
  against live Jupiter Perps state in production. Verify the
  positions listed in the daemon log against the Jupiter Perps UI.
- **Live IDL verification**: discriminators for
  `create_increase_position_request_v2` and
  `create_decrease_position_request_v2` were sourced from Jupiter's
  current public IDL but are not verified against live program data
  in the runtime. An IDL update on Jupiter's side could break ixn
  build silently.
- **read_pool_state custody list is caller-supplied**: v0 requires
  the operator to pass the list of custody pubkeys (USDC, SOL, ETH,
  BTC). Source them from Jupiter Perps documentation. Auto-discovery
  from the pool account is a v1 plan.
- **Single position**: the daemon manages one hedged-JLP position
  per process. Multi-position scenarios are a v1 plan.
- **No mainnet auto-promotion**: this runbook is for the FIRST
  mainnet test only. Larger positions and removal of
  `--require-approval` require a successful 24-72h watch +
  post-mortem first.
