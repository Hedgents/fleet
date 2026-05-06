# Stable-yield daemon — mainnet tiny-position runbook

WARNING: THIS USES REAL MONEY ON SOLANA MAINNET. Every step in this runbook
is mandatory. Do not skip the pre-flight checklist.

## Goal

Deposit $50 USDC into Kamino's main lending market via the
stable-yield-daemon. The position should appear on-chain (Solscan +
Kamino UI) and the daemon's telemetry log should record the deposit.

This is **not an earnings test**. The $50 is far too small to produce
meaningful yield. The point is to prove the wiring (Assign → Approve →
build → sign → submit → Report) works end-to-end against mainnet state
without putting meaningful capital at risk.

Earnings come over weeks/months at much larger size — see "Earning
expectations" below for the honest math.

## Pre-flight checklist

Before running ANY mainnet command, confirm each item below.

- [ ] Devnet smoke (M5) has been run end-to-end: `assign-stable-lend`
      produces a Report. On devnet the Report has `ok=false error_code=5`
      (no Kamino reserve at placeholder pubkey) — the wiring is what's
      verified, not the chain submit. See
      `docs/runbooks/stable-yield-approval-verified.md`.
- [ ] Manual-approval flow (M8) is verified on devnet: two-step
      Assign → queued → Approve → execution Report. Both the
      same-orchestrator (positive) and cross-orchestrator (REJECTED)
      paths have been exercised.
- [ ] Telemetry (M7) is verified on devnet: `stable-yield-pnl.jsonl`
      writes one line per beacon tick with `deposited_usdc_lamports` and
      a (placeholder, zero) `supply_apr_bps`. v0 telemetry's APR is a
      known-zero stub; that's fine.
- [ ] You have a Solana mainnet wallet keypair file with:
      - **At least 0.05 SOL** for transaction fees + ATA rent.
      - **At least $50 USDC** in the wallet's USDC ATA.
        Mainnet USDC mint: `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`.
- [ ] You have a private RPC URL (free public mainnet RPC is too rate-
      limited for live trading and may serve stale state). Recommended:
      Helius free tier, QuickNode, Triton, Alchemy. **DO NOT use
      `https://api.mainnet-beta.solana.com` for live submits.**
- [ ] You understand the genesis-hash bail. The daemon checks the RPC's
      genesis hash on boot against `--network mainnet`'s expected hash.
      A mismatch (e.g., devnet RPC paired with `--network mainnet`)
      bails with a clear error before any envelope is processed.
- [ ] You have the **Kamino main lending market pubkey** and the
      **USDC reserve pubkey** for that market.
      - Operator: look up before deploy. As of 2026-05-06, the canonical
        source is Kamino's docs at https://docs.kamino.finance/ and the
        Kamino app at https://app.kamino.finance/ (inspect the USDC
        market → "Reserve info").
      - Do NOT hardcode pubkeys taken from secondary sources without
        verification. Pubkey mistakes here = funds sent to the wrong
        place.
- [ ] You have at least 30 minutes of focused time to monitor the boot,
      the sim-only dry run, and the real submit.

If any checklist item is unchecked, **do not proceed**. Address it first.

## Step 1 — Generate role keys + wire the mainnet wallet

```bash
mkdir -p ~/01fi-mainnet/{stable-yield,orch}

# 32-byte raw Ed25519 seed for the daemon's mesh identity:
openssl rand 32 > ~/01fi-mainnet/stable-yield/stable-yield-role.key
openssl rand 32 > ~/01fi-mainnet/orch/orchestrator-role.key
chmod 600 ~/01fi-mainnet/stable-yield/stable-yield-role.key
chmod 600 ~/01fi-mainnet/orch/orchestrator-role.key

# Place your existing funded mainnet wallet here:
cp /path/to/your/mainnet-wallet.json ~/01fi-mainnet/stable-yield/solana-wallet.json
chmod 600 ~/01fi-mainnet/stable-yield/solana-wallet.json

# Verify the wallet pubkey + balances:
WALLET=$(solana-keygen pubkey ~/01fi-mainnet/stable-yield/solana-wallet.json)
echo "wallet: $WALLET"
solana balance "$WALLET" --url <YOUR_MAINNET_RPC_URL>

# Verify the USDC ATA balance (USDC mint EPjF...t1v):
spl-token balance EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
    --owner "$WALLET" --url <YOUR_MAINNET_RPC_URL>
```

You should see at least 0.05 SOL and at least 50 USDC (50.000000 with
6-decimal display).

## Step 2 — Boot stable-yield-daemon in mainnet mode (sim-only first)

In a terminal you'll keep open for the entire session:

```bash
RUST_LOG=info,libp2p=warn,zerox1_node_enterprise=info \
cargo run --release -p stable-yield-daemon -- \
    --secrets-dir ~/01fi-mainnet/stable-yield \
    --wallet ~/01fi-mainnet/stable-yield/solana-wallet.json \
    --rpc-url <YOUR_MAINNET_RPC_URL> \
    --network mainnet \
    --i-understand-this-is-mainnet \
    --listen /ip4/0.0.0.0/tcp/19310 \
    --max-position-usdc-lamports 100000000 \
    --simulate-only true \
    --require-approval true \
    --telemetry-market <KAMINO_MAIN_MARKET_PUBKEY_BASE58> \
    --telemetry-interval-secs 60 \
    --pnl-log ~/01fi-mainnet/stable-yield-pnl.jsonl
```

Key flags:
- `--network mainnet` + `--i-understand-this-is-mainnet`: redundant
  acknowledgments. The daemon refuses mainnet without both.
- `--max-position-usdc-lamports 100000000`: $100 ceiling — gives a small
  amount of headroom over the $50 test. caps.rs enforces a compile-time
  hard ceiling of $5M; this CLI cap brings it down further.
- `--simulate-only true`: **first run is dry — no broadcast.** The
  daemon will build the deposit ixn set and run `build_sign_simulate`
  against the mainnet RPC, but will not submit. This verifies wiring
  against real mainnet state (real reserve account, real ATAs, real
  account layouts) without spending a cent of the position.
- `--require-approval true`: every Assign queues, awaiting an Approve
  envelope on the same conversation_id. The daemon defaults to true on
  mainnet, but pass it explicitly so the boot log confirms it.
- `--telemetry-market <pubkey>` + `--telemetry-interval-secs 60`:
  beacon tick fires every 60s, records a snapshot in
  `stable-yield-pnl.jsonl`. Longer than devnet's tighter interval to
  reduce mesh + RPC chatter.

Watch the boot log for:

```
INFO Loaded identity from ".../stable-yield/.runtime-keypair-stable-yield"
     peer_id=12D3KooW...  agent_id=<HEX>
INFO Genesis hash verified: mainnet
INFO Listening on /ip4/0.0.0.0/tcp/19310
```

If you see `Genesis hash mismatch` — the RPC is wrong. Stop and fix.

Save the 64-character hex `agent_id` as `STABLE_YIELD_AGENT_ID` for
Step 3.

## Step 3 — Sim-only dry run: send a $50 Assign

In a separate terminal:

```bash
STABLE_YIELD_AGENT_ID=<paste from Step 2>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19310 \
    --recipient-agent-id "$STABLE_YIELD_AGENT_ID" \
    --timeout-secs 60 \
    assign-stable-lend \
        --market <KAMINO_MAIN_MARKET_PUBKEY_BASE58> \
        --reserve <USDC_RESERVE_PUBKEY_BASE58> \
        --usdc-lamports 50000000
```

`--usdc-lamports 50000000` = $50 (USDC has 6 decimals).

Expected: stub prints an ack `Report received` with `ok=true`,
`deposited_usdc_lamports=0`, no `tx_signature`. That's the **queued**
state. The daemon log shows:

```
INFO AssignStableLend received  market=<pk> reserve=<pk> lamports=50000000
INFO Caps validated  cap=100000000 requested=50000000
INFO AssignStableLend queued — awaiting Approve  conv=<HEX>
INFO NeedsApproval Escalate emitted  conv=<HEX>
INFO report sent ok=true deposited=0 conv=<HEX>
```

**Capture the `conv` hex from the daemon log.** You need it to approve.

Now send the Approve from the same orchestrator `--secrets-dir`:

```bash
CONV=<paste conv hex from daemon log>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19310 \
    --recipient-agent-id "$STABLE_YIELD_AGENT_ID" \
    --timeout-secs 90 \
    approve --conv-hex "$CONV"
```

Because `--simulate-only=true`, the daemon will:
1. Verify `env.sender` matches the original Assign sender.
2. Re-validate caps.
3. Build the Kamino `lendingMarketDeposit` ixn set.
4. Run `build_sign_simulate` against the mainnet RPC (no broadcast).
5. Send a final Report.

Expected daemon log:

```
INFO Approve received — executing queued AssignStableLend  conv=<HEX>
INFO lend::run_or_simulate  simulate_only=true market=<pk> reserve=<pk>
INFO simulation succeeded  units_consumed=<N>
INFO report sent ok=true deposited=50000000 conv=<HEX>  (sim-only, no tx_signature)
```

Stub log:

```
Report received: ReportStableLend {
    header: ReportHeader { ok: true, ... },
    deposited_usdc_lamports: 50000000,
    tx_signature: None,
}
```

If `ok=false` here:
- `error_code=5`: simulation failed on chain — read full daemon log.
  Likely causes: wrong reserve pubkey, reserve frozen/paused, account
  layout mismatch. **Diagnose before proceeding.**
- `error_code=6`: ixn-build failed before chain. Likely a bad market or
  reserve pubkey. **Verify pubkeys against Kamino's docs.**

**Do NOT flip `--simulate-only=false` until the sim run returns
`ok=true`.**

## Step 4 — Real $50 deposit

Once the sim run is green: Ctrl-C the daemon and restart it with
`--simulate-only false`.

```bash
RUST_LOG=info,libp2p=warn,zerox1_node_enterprise=info \
cargo run --release -p stable-yield-daemon -- \
    --secrets-dir ~/01fi-mainnet/stable-yield \
    --wallet ~/01fi-mainnet/stable-yield/solana-wallet.json \
    --rpc-url <YOUR_MAINNET_RPC_URL> \
    --network mainnet \
    --i-understand-this-is-mainnet \
    --listen /ip4/0.0.0.0/tcp/19310 \
    --max-position-usdc-lamports 100000000 \
    --simulate-only false \
    --require-approval true \
    --telemetry-market <KAMINO_MAIN_MARKET_PUBKEY_BASE58> \
    --telemetry-interval-secs 60 \
    --pnl-log ~/01fi-mainnet/stable-yield-pnl.jsonl
```

Send the Assign again (same command as Step 3):

```bash
RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19310 \
    --recipient-agent-id "$STABLE_YIELD_AGENT_ID" \
    --timeout-secs 60 \
    assign-stable-lend \
        --market <KAMINO_MAIN_MARKET_PUBKEY_BASE58> \
        --reserve <USDC_RESERVE_PUBKEY_BASE58> \
        --usdc-lamports 50000000
```

Wait for the queued ack Report. Capture the new `conv` hex. Send Approve:

```bash
CONV=<new conv hex from daemon log>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19310 \
    --recipient-agent-id "$STABLE_YIELD_AGENT_ID" \
    --timeout-secs 90 \
    approve --conv-hex "$CONV"
```

Expected final Report:

```
ReportStableLend {
    header: ReportHeader { ok: true, ... },
    deposited_usdc_lamports: 50000000,
    tx_signature: Some("<base58_signature>"),
}
```

Daemon log:
```
INFO Approve received — executing queued AssignStableLend  conv=<HEX>
INFO lend::run_or_simulate  simulate_only=false market=<pk> reserve=<pk>
INFO submitted  signature=<base58>
INFO confirmed  signature=<base58>
INFO report sent ok=true deposited=50000000 tx_signature=<base58> conv=<HEX>
```

Look up `<base58_signature>` on https://solscan.io/ to confirm.

## Step 5 — Verify the position

Three independent ways to confirm the deposit landed:

1. **Solscan tx page**
   `https://solscan.io/tx/<base58_signature>`
   The instruction list should show:
   - `Kamino: lendingMarketDeposit` (or equivalent IDL name) succeeded
   - SPL Token transfer of 50 USDC out of your wallet's USDC ATA
   - cToken / kUSDC mint into the wallet (the receipt token Kamino
     issues for the supply)

2. **Kamino UI**
   `https://app.kamino.finance/`
   Connect the wallet from Step 1, navigate to the main USDC market,
   and confirm:
   - Supply position of $50 USDC (give or take a fraction of a cent for
     interest accrual since the deposit slot)
   - Current supply APR is shown next to the position

3. **Telemetry log**
   ```bash
   tail -5 ~/01fi-mainnet/stable-yield-pnl.jsonl
   ```
   Each line is a JSON snapshot. After the deposit, the most recent
   line should show:
   - `deposited_usdc_lamports: 50000000`
   - `supply_apr_bps: 0`  ← v0 placeholder, known-zero. Real APR
     computation lands in a later milestone. The presence of the
     `deposited` figure is the load-bearing signal here.

If any of the three diverge — e.g., Solscan shows success but Kamino UI
shows nothing — investigate before doing anything else. The position
state is the source of truth, not the telemetry stub.

## Earning expectations (be honest)

Kamino's USDC supply APR floats roughly **5-8%** depending on market
utilization. On a $50 position:

| APR | $/year | $/month | $/day |
|-----|--------|---------|-------|
| 5%  | $2.50  | $0.21   | $0.007 |
| 8%  | $4.00  | $0.33   | $0.011 |

That's pennies per day. **This runbook is not validating earnings.**
Real earnings come over weeks/months and at much larger size. The $50
exists solely to verify the round-trip works on mainnet without
risking meaningful capital.

## Step 6 — 24-hour operational watch

Leave the daemon running for at least 24 hours. During that window:

- **Solscan**: re-load the tx page every few hours, confirm the
  position remains. (No re-submit from the daemon during steady-state —
  the deposit is a one-shot ixn.)
- **Telemetry**: `tail -f ~/01fi-mainnet/stable-yield-pnl.jsonl` should
  show one new line every 60s. Gaps mean the daemon's beacon loop
  stalled — investigate logs.
- **Kamino UI**: check utilization on the USDC reserve. If utilization
  approaches 100%, withdrawals may be temporarily blocked at the
  Kamino-protocol level (not a daemon bug). The deposit side is
  unaffected.
- **Daemon logs**: scan for `Approve REJECTED — sender does not match`.
  Should be zero in a clean run. Non-zero means someone is probing the
  approval gate — investigate.

## Step 7 — Rollback / withdrawal

To unwind the $50 position, see `docs/runbooks/stable-yield-withdraw.md`
(lands in M10). The withdrawal flow is:

1. Send a `WithdrawStableLend` envelope from the orchestrator with
   `usdc_lamports=50000000` (or `u64::MAX` for a full withdraw including
   any accrued interest).
2. Approve from the same orchestrator `--secrets-dir`.
3. Daemon builds a Kamino `lendingMarketWithdraw` ixn, signs, submits.
4. Funds return to the wallet's USDC ATA.

If M10 has not yet landed when you need to unwind, fall back to the
manual path: connect the same wallet at https://app.kamino.finance/,
find the USDC supply position, click Withdraw.

## Failure-mode triage

| Symptom | Likely cause | Fix |
|---|---|---|
| Daemon bails at boot: `RPC URL ... returned genesis hash X but --network mainnet expects Y` | Wrong RPC URL (e.g., devnet RPC paired with `--network mainnet`) | Use a true mainnet RPC. |
| Daemon bails at boot: `--network=mainnet requires --i-understand-this-is-mainnet flag` | Missing ack flag | Add the flag — this is a deliberate guard against accidental mainnet runs. |
| Assign exits non-zero, daemon log shows `AssignStableLend received` but no `queued` line | Cap rejected the Assign — `usdc_lamports` exceeded `--max-position-usdc-lamports` | Lower the Assign amount or raise the CLI cap (still bounded by the compile-time MAX of $5M). |
| Approve exits non-zero, daemon log shows `Approve REJECTED — sender does not match` | Approving from a different orchestrator role-key than the one that enqueued the Assign | Use the **same** `--secrets-dir` for Approve as for Assign. The audit-fix C1 sender check is operating correctly. |
| Final Report `ok=false error_code=5` | Sim or submit failed on chain — could be insufficient SOL for gas, reserve frozen, slippage, account layout issue, RPC stale state | Read full daemon log for the chain-side error string. Check Kamino UI for reserve status. Verify wallet has SOL. |
| Final Report `ok=false error_code=6` | Ixn-build failed before chain — almost always a bad market or reserve pubkey | Verify pubkeys against Kamino's docs and the Kamino app's reserve-info pane. |
| Telemetry log silent (no new lines) | Beacon loop stalled or telemetry market pubkey wrong | Check daemon logs for telemetry tick errors; restart daemon if stalled. |

## Cleanup / teardown

After a successful $50 round-trip + 24h watch:

1. Issue the `WithdrawStableLend` (M10) — funds return to the wallet
   USDC ATA.
2. Stop the daemon: Ctrl-C in the daemon's terminal. Clean shutdown
   runs through `tokio::signal`.
3. Archive the telemetry log:
   ```bash
   mv ~/01fi-mainnet/stable-yield-pnl.jsonl \
      ~/01fi-mainnet/logs/stable-yield-mainnet-$(date +%Y-%m-%d).jsonl
   ```
4. **Rotate the role key** if any logs were shared externally:
   ```bash
   openssl rand 32 > ~/01fi-mainnet/stable-yield/stable-yield-role.key
   ```
   On next boot the daemon picks up the new key — note that the
   `agent_id` changes too, so the orchestrator's `--recipient-agent-id`
   needs updating.

## Step 8 — Document the result

Append to this runbook (or create a sibling file
`docs/runbooks/stable-yield-mainnet-results.md`) with:

- Date and time of the deposit
- Solscan link for the deposit tx
- Final Report payload (full hex or decoded)
- Telemetry first/last snapshot for the watch window
- Withdrawal Solscan link (after M10)
- Final wallet USDC balance vs starting balance (the difference is
  the realized yield — expect a few cents on $50 over a 24h-week window)
- Any incidents (genesis bail, REJECTED approves, RPC hiccups)

This documentation is the **proof of mainnet round-trip** for the
broader 01fi project's milestone tracking.

## Emergency contacts (operator's own)

- Solana mainnet status: https://status.solana.com/
- Kamino app (manual withdraw fallback): https://app.kamino.finance/
- Kamino docs: https://docs.kamino.finance/
- Solscan: https://solscan.io/

## What's NOT in v0

These limitations are documented elsewhere in the plan. You should be
aware of them before proceeding:

- **APR computation is a placeholder**: `supply_apr_bps` in the
  telemetry log is hardcoded to 0 in v0. Real APR derivation from the
  reserve's interest-rate curve lands in a later milestone.
- **Single position**: the daemon manages one USDC supply position per
  process. Multi-reserve / multi-market scenarios are a v1 plan.
- **No automatic withdrawal triggers**: v0 only withdraws on operator
  Assign. Yield-aware rebalancing across multiple reserves is a future
  milestone.
- **No mainnet auto-promotion**: this runbook is for the FIRST mainnet
  test only. Larger positions and removal of `--require-approval`
  require a successful 24h watch + post-mortem first.
