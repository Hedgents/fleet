# Multiply daemon — mainnet tiny-position runbook

WARNING: THIS USES REAL MONEY ON SOLANA MAINNET. Every step in this runbook is
mandatory. Do not skip the pre-flight checklist.

## Goal

Take a $50 leveraged-jitoSOL position on Kamino mainnet via the new
fleet-shape multiply-daemon. After 24 hours, run the `report` subcommand
to demonstrate **positive trailing APR** — the acceptance criterion for
the whole multiply-strategy plan.

## Pre-flight checklist

Before running ANY mainnet command, confirm each item below.

- [ ] Devnet smoke (M5) has been run end-to-end: `assign-multiply` produces
      a Report with `ok: true`. Boot logs show "AssignMultiply received",
      "leverage loop entering", and the Report comes back via the mesh.
- [ ] Manual-approval flow (M8) is verified on devnet: two-step
      Assign → queued → Approve → execution Report.
- [ ] Telemetry (M9) is verified: `multiply-daemon report --log <devnet log>
      --since-secs 60` prints a readout (with $0 numbers on devnet —
      that's expected).
- [ ] Liquidation monitor (M7) is wired: beacon-tick logs show
      "liq monitor: no obligation yet, skipping tick" at debug level.
- [ ] You have a Solana mainnet wallet keypair file with:
      - At least 0.1 SOL for transaction fees
      - Either ~$50 of jitoSOL OR ~$50 of SOL (the daemon will deposit
        whatever you fund it with as initial collateral; the leverage
        loop borrows SOL from Kamino against it)
- [ ] You have a private RPC URL (free public mainnet RPC is too rate-
      limited for live trading). Examples: Helius free tier, Triton,
      Alchemy. **DO NOT use https://api.mainnet-beta.solana.com for
      live positions.**
- [ ] You have at least 30 minutes of focused time to monitor the boot
      and the first leverage round commitments.
- [ ] You understand that the smoke test on devnet did NOT exercise the
      Kamino reserves themselves (the reserves only exist on mainnet);
      the leverage loop runs for the first time with real money.

If any checklist item is unchecked, **do not proceed**. Address it first.

## Step 1 — Generate role keys + wire the mainnet wallet

```bash
mkdir -p ~/01fi-mainnet/{multiply,orch}

# 32-byte raw Ed25519 seed for the daemon's mesh identity:
openssl rand 32 > ~/01fi-mainnet/multiply/multiply-role.key
openssl rand 32 > ~/01fi-mainnet/orch/orchestrator-role.key
chmod 600 ~/01fi-mainnet/multiply/multiply-role.key
chmod 600 ~/01fi-mainnet/orch/orchestrator-role.key

# Place your existing funded mainnet wallet here:
cp /path/to/your/mainnet-wallet.json ~/01fi-mainnet/multiply/solana-wallet.json
chmod 600 ~/01fi-mainnet/multiply/solana-wallet.json

# Verify the wallet pubkey + balance:
solana-keygen pubkey ~/01fi-mainnet/multiply/solana-wallet.json
solana balance "$(solana-keygen pubkey ~/01fi-mainnet/multiply/solana-wallet.json)" \
  --url <your-mainnet-rpc-url>
```

## Step 2 — Boot multiply-daemon in mainnet mode

In a terminal you'll keep open for the entire session:

```bash
RUST_LOG=info,libp2p=warn,zerox1_node_enterprise=info \
cargo run --release -p multiply-daemon -- run \
    --secrets-dir ~/01fi-mainnet/multiply \
    --wallet ~/01fi-mainnet/multiply/solana-wallet.json \
    --rpc-url <YOUR_MAINNET_RPC_URL> \
    --listen /ip4/127.0.0.1/tcp/9302 \
    --beacon-interval-secs 30 \
    --pnl-log ~/01fi-mainnet/multiply-pnl.jsonl \
    --network mainnet \
    --i-understand-this-is-mainnet \
    --max-position-usdc-lamports 50000000 \
    --no-simulate-only
```

Key flags:
- `--network mainnet` + `--i-understand-this-is-mainnet`: redundant
  acknowledgments. The daemon refuses mainnet without both.
- `--max-position-usdc-lamports 50000000`: $50 cap (USDC has 6 decimals).
  caps.rs enforces a hard ceiling of $5M; this is well below.
- `--no-simulate-only`: this commits real txs. **The daemon defaults to
  sim-only.** Without `--no-simulate-only`, the leverage loop will
  simulate but never submit.
- `--require-approval`: not specified, defaults to `true` on mainnet.
  Every Assign will be queued waiting for an Approve envelope.
- `--beacon-interval-secs 30`: longer than devnet (5s) to reduce mesh
  chatter. The pnl snapshot fires once per beacon, so 30s gives 2880
  snapshots/day.

Watch the boot log for the multiply daemon's `agent_id` — you'll need it
in Step 3:

```
INFO Loaded identity from ".../multiply/.runtime-keypair-multiply"
     peer_id=12D3KooW...  agent_id=<HEX>
```

Save that 64-character hex string as `MULTIPLY_AGENT_ID` for the next step.

## Step 3 — Send the Assign

In a separate terminal:

```bash
MULTIPLY_AGENT_ID=<paste from Step 2>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/9399 \
    --bootstrap /ip4/127.0.0.1/tcp/9302 \
    --recipient-agent-id "$MULTIPLY_AGENT_ID" \
    --timeout-secs 90 \
    assign-multiply --target-ltv-bps 5000
```

Key choices:
- `--target-ltv-bps 5000`: 50% LTV. **More conservative than the cap
  (8000=80%)** for the first mainnet test. Lower LTV = lower
  liquidation risk.
- `--timeout-secs 90`: budget for the gossipsub mesh to warm up
  (~10-20s) plus the daemon's reply round-trip.

Expected: the stub prints `Report received` with payload
`ReportMultiply { ok: true, resulting_ltv_bps: 0, tx_signature: None }`.
That `ok: true` with `resulting_ltv_bps: 0` is the **queued** state —
the Assign is now waiting in multiply's approval queue. The daemon log
shows:

```
INFO AssignMultiply queued — awaiting Approve conv=<HEX>
INFO NeedsApproval Escalate emitted conv=<HEX>
```

**Capture the conv hex from the daemon log.** You need it for Step 4.

## Step 4 — Inspect, then approve

Before approving, **read the daemon's log carefully** to confirm:

- The Assign was correctly decoded: `target_ltv_bps=5000` matches what
  you sent
- caps validation passed: no "exceeds hard cap" errors
- The "NeedsApproval Escalate emitted" line shows the routing back to
  the orchestrator's pubkey worked

If anything looks wrong, **kill the daemon (Ctrl-C) and don't approve**.
The Assign expires from the queue after 5 minutes anyway.

If everything looks right, send the Approve:

```bash
CONV=<paste conv hex from daemon log>

RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/9399 \
    --bootstrap /ip4/127.0.0.1/tcp/9302 \
    --recipient-agent-id "$MULTIPLY_AGENT_ID" \
    --timeout-secs 180 \
    approve --conv-hex "$CONV"
```

`--timeout-secs 180`: the leverage loop walks LTV from 0 to 5000 bps in
multiple rounds. Each round commits ~1-2 transactions on mainnet (deposit,
borrow, optional swap). With Solana's ~400ms slot time + retries, budget
3 minutes.

Expected daemon log:
```
INFO Approve received — executing queued AssignMultiply conv=<HEX>
INFO leverage loop starting simulate_only=false target_ltv_bps=5000
INFO leverage loop entering current_ltv_bps=0 target_ltv_bps=5000
INFO round 1 committed signature=<base58_sig>
INFO round done current_ltv_bps=<some_value>
INFO round 2 committed signature=<base58_sig>
... (likely 2-3 rounds total to reach 5000 bps)
INFO leverage loop done resulting_ltv_bps=<close to 5000>
INFO report sent ok=true conv=<HEX>
```

Stub log:
```
Report received: msg_type=Report ...
Report payload (decoded): ReportMultiply {
    header: ReportHeader { ok: true, ... },
    resulting_ltv_bps: <close to 5000>,
    tx_signature: Some("<last round's signature>"),
}
```

## Step 5 — Verify on Solana Explorer

For each `round committed signature=<sig>` line, paste the signature into
https://explorer.solana.com (toggle to "Mainnet Beta") and confirm:

- The transaction is `Success`
- The signer is your wallet pubkey from Step 1
- The instructions touch programs:
  - `KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD` (Kamino Lend)
  - `Stake11111111111111111111111111111111111111` or jito stake-pool
    (jitoSOL deposit)
  - SPL Token transfers (USDC, jitoSOL flows)

**If any tx is `Failed` on Explorer**, immediately Ctrl-C the daemon and
investigate before sending more Assigns.

## Step 6 — Watch the liquidation monitor

The daemon's beacon loop runs `liq_monitor::tick` every 30 seconds. After
the position is open, you should see:

```
INFO liq monitor: position healthy distance_bps=<N> obligation=<addr>
```

`distance_bps` should be **well above 200** (the warning threshold). At
5000 bps target LTV with a typical liquidation threshold around 9000 bps,
distance should start at ~4000 bps.

If you ever see `WARNING — position drift; emit Escalate` or
`CRITICAL — position approaching liquidation`, **act immediately**: run
the unwind path in Step 8.

## Step 7 — 24h earning verification

Leave the daemon running for 24 hours. After 24h:

```bash
cargo run --release -p multiply-daemon -- report \
    --log ~/01fi-mainnet/multiply-pnl.jsonl \
    --since-secs 86400
```

Expected output:

```
Multiply position report (window: 86400 s, 2880 snapshots)
  Initial net equity: $50.000000  (or close)
  Current net equity: $50.013000  (or whatever has accrued)
  Initial deposited:  $XX.XXXXXX  (≈ $100 at 50% LTV)
  Current deposited:  $XX.XXXXXX
  Initial borrowed:   $XX.XXXXXX  (≈ $50)
  Current borrowed:   $XX.XXXXXX
  PnL (window):       $+0.013000 (+0.026%)
  Annualized APR:     +9.49%
```

The exact numbers depend on:
- jitoSOL's 24h staking yield (typically ~7%)
- Kamino's USDC borrow rate (typically 3-5% on mainnet)
- The leverage multiplier (at 50% LTV, leverage ≈ 1.5x)
- Net APR ≈ (jitoSOL yield × leverage) − borrow rate × (leverage − 1)

A **positive Annualized APR** value is the acceptance criterion. Save
the full output as evidence.

## Step 8 — Unwind path (when you're done)

To close the position and recover collateral:

```bash
RUST_LOG=info cargo run --release -p fleet-pm-stub -- \
    --secrets-dir ~/01fi-mainnet/orch \
    --listen /ip4/127.0.0.1/tcp/9399 \
    --bootstrap /ip4/127.0.0.1/tcp/9302 \
    --recipient-agent-id "$MULTIPLY_AGENT_ID" \
    --timeout-secs 90 \
    assign-multiply --target-ltv-bps 0
```

This sends an Assign with `target_ltv_bps=0`, telling the daemon to
deleverage the position to zero LTV. The current daemon implementation
*does not yet have a deleverage path implemented* (M6 only implemented
the lever-up direction). For v0, you'll need to manually unwind via
Kamino's web UI at https://app.kamino.finance/ — connect your wallet,
find the obligation, and use the "Repay" + "Withdraw" flows.

A `lever_down` implementation is a v0.1 follow-up. The monolith's
`crates/zerox1-defi-daemon/src/handlers/multiply.rs` already has a working
`lever_down` body that can be lifted into the fleet daemon the same way
M6 lifted `lever_up`.

## Step 9 — Document the result

Append to this runbook (or create a sibling file `docs/runbooks/multiply-
mainnet-results.md`) with:

- Date and time of the position open
- Initial wallet balance (SOL, jitoSOL, USDC)
- Solana Explorer links for each round's tx
- The 24h `report` output
- Final position state at unwind
- Any incidents (warning escalates, mesh hiccups, RPC issues)

This documentation is the **proof of mainnet earning** for the broader
01fi project's milestone tracking.

## Emergency contacts (operator's own)

- Solana mainnet status: https://status.solana.com/
- Kamino app (manual unwind path): https://app.kamino.finance/
- Kamino Discord (for KLend support): documented at
  https://docs.kamino.finance/

## What's NOT in v0

These limitations are documented elsewhere in the plan. You should be
aware of them before proceeding:

- **No automatic unwind**: Step 8 requires the operator to manually
  unwind via Kamino's web UI. v0.1 implements `lever_down`.
- **No automatic liquidation defense**: the liq monitor warns but does
  not auto-deleverage on Critical. v0.1 wires `lever_down` to the
  Critical band.
- **Single position**: the daemon manages exactly one Kamino position
  per multiply-daemon process. Multi-position scenarios are a v1 plan.
- **No mainnet auto-promotion**: this runbook is for the FIRST mainnet
  test only. Larger positions and auto-mode (no manual approval)
  require M11's 24h watch + post-mortem first.
