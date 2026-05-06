# stable-yield-daemon — withdrawal runbook

This is the unwind procedure for positions opened via the deposit
runbook (`stable-yield-mainnet-tiny.md`). Without this path the $50
mainnet test funds would be stuck in Kamino requiring out-of-band
recovery; M10 makes the test reversible.

## When to use this

- After a successful `$50` mainnet deposit, to pull funds back to the
  daemon's wallet.
- After a failed mainnet smoke (deposit landed but you want to roll
  back before the 24h watch kicks in).
- Any time an operator decides to retire a position.

## Pre-flight

You should already have:
- The `secrets-dir` containing `stable-yield-role.key` and
  `solana-wallet.json` (the same dir used by the running daemon).
- A separate `secrets-dir` for the orchestrator stub holding
  `orchestrator-role.key`.
- The `--market` and `--reserve` base58 pubkeys that were used in the
  original deposit. (Read them from
  `docs/runbooks/stable-yield-mainnet-tiny.md` — they're pinned there.)
- The daemon's `agent_id` (32-byte hex) — read from its boot log:
  `Loaded identity ... agent_id=<hex>`.

## Partial vs full withdrawal

Two amount conventions:

| `--usdc-lamports`    | Behavior                                                        |
| -------------------- | --------------------------------------------------------------- |
| `5000000`            | Withdraw exactly $5 of liquidity (caps don't gate withdrawals)  |
| `50000000`           | Withdraw $50 (the full original test deposit)                   |
| `18446744073709551615` (`u64::MAX`) | Withdraw all collateral klend reports for the obligation |

Pass `u64::MAX` when you want to drain the position completely
including any accrued interest. Pass an explicit amount when you want
a partial unwind.

## Procedure

### 1. Daemon must already be running

The withdraw flow uses the same `MsgType::Withdraw`-listening dispatch
loop as a normal Assign. If the daemon isn't running, restart it via
the deposit runbook's "Boot the daemon" section first.

### 2. Send WithdrawStableLend (sim path on mainnet)

ALWAYS run sim-only first. The daemon defaults to `--simulate-only=true`,
which means even after Approve it won't broadcast — it'll only run
`simulate_transaction` and return the layout. Confirm sim succeeds
before you flip to submit.

```bash
RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir /path/to/orchestrator/secrets \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19310 \
    --recipient-agent-id <DAEMON_AGENT_ID_HEX> \
    --timeout-secs 60 \
    withdraw-stable-lend \
        --market <KAMINO_MARKET_BASE58> \
        --reserve <USDC_RESERVE_BASE58> \
        --usdc-lamports 50000000
```

If the daemon was booted with `--require-approval=true` (the mainnet
default), the stub will receive a `ReportStableWithdraw{ ok=true,
withdrawn_usdc_lamports=0 }` immediately — that's the
"queued, awaiting Approve" ACK. Capture the conv_id from the stub log.

### 3. Approve

```bash
RUST_LOG=info,libp2p=warn cargo run --release -p fleet-pm-stub -- \
    --secrets-dir /path/to/orchestrator/secrets \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19310 \
    --recipient-agent-id <DAEMON_AGENT_ID_HEX> \
    --timeout-secs 60 \
    approve --conv-hex <CONV_ID_HEX>
```

The daemon dispatches the queued Withdraw, builds the 3-ixn bundle
(idempotent ATA-create + refresh_reserve +
withdraw_obligation_collateral_and_redeem_reserve_collateral), runs
it through the SigningWhitelist, and either simulates or submits per
`--simulate-only`.

Expected sim Report on a properly-set-up obligation:

```
Report payload (decoded as ReportStableWithdraw):
  header: ok=true ...
  withdrawn_usdc_lamports: 50000000
  tx_signature: None
```

`tx_signature: None` is correct on the sim path — it only fills on
submit.

### 4. Promote to submit

After sim is clean:

1. Stop the daemon.
2. Restart with `--simulate-only=false` (still on mainnet, still with
   `--i-understand-this-is-mainnet`).
3. Re-run the WithdrawStableLend + Approve pair from steps 2–3.

The submit-path Report carries a `tx_signature: Some(...)`. Verify on
solscan that the obligation's `deposited_amount_usdc` decreased by the
expected amount (or hit zero on `u64::MAX`).

### 5. Verify the wallet received the USDC

```bash
spl-token balance <USDC_MINT> --owner <DAEMON_WALLET_PUBKEY>
```

Should reflect the withdrawn amount minus the (negligible) tx fee.

## Failure modes

| Symptom                                         | Cause                                                 | Action                                              |
| ----------------------------------------------- | ----------------------------------------------------- | --------------------------------------------------- |
| `error_code=5` on sim                           | Reserve refresh stale or oracle missing               | Wait one slot; if persistent, check Kamino status   |
| `error_code=6` on sim                           | Reserve metadata couldn't be loaded (RPC issue)       | Check `--rpc-url`; retry                            |
| `error_code=3` on Approve                       | Cap re-validation rejected (zero amount)              | Re-issue with non-zero `--usdc-lamports`            |
| Approve silently dropped, no Report             | Sender mismatch in approval queue                     | Use the same orchestrator key as the original Withdraw |
| Submit returns `InsufficientCollateral`         | Asked for more than the obligation holds              | Lower `--usdc-lamports`, or pass `u64::MAX`         |

## Manual fallback (Kamino UI)

If the daemon is unreachable, you can withdraw directly via the
Kamino app: <https://app.kamino.finance/>. Connect the daemon's
wallet, navigate to your obligation, and click Withdraw. The on-chain
program is the same — only the wrapper differs.

## Notes

- Withdrawal has no MIN/MAX cap analogous to deposits. The protocol
  enforces the upper bound (you can't pull more than the obligation
  holds), and `caps::validate_withdraw` only rejects the zero case.
- The daemon's `Withdraw` MsgType (0x18) is symmetric to `Assign`
  (0x10). Both live in the collaboration class (0x1_).
- Approve uses the same parallel-queue protocol as Assign — there's
  no behavioral difference in the approval flow itself.
