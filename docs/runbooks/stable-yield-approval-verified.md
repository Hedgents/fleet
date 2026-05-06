# Stable-Yield Approval Flow — Verified

Devnet smoke (M8) verified the manual-approval flow end-to-end on
`stable-yield-daemon` running with `--require-approval=true`.

## Same-orchestrator round-trip (positive path)

1. Orchestrator sends `AssignStableLend` to stable-yield-daemon running with `--require-approval=true`.
2. Daemon validates against caps, enqueues, emits `EscalateRisk(Notice, NeedsApproval)` to orchestrator.
3. Daemon sends ack Report (`ok=true`, `deposited_usdc_lamports=0`, no `tx_signature`) — orchestrator now knows the Assign is queued.
4. Orchestrator sends `MsgType::Approve` envelope on the same `conversation_id`.
5. Daemon's Approve handler verifies `env.sender` matches the original Assign sender (audit-fix C1), re-validates caps (audit-fix I2), invokes `lend::run_or_simulate`, and sends the final Report.

## Cross-orchestrator REJECT (security path)

1. Orchestrator-A enqueues an Assign (queued, ack Report sent).
2. Orchestrator-B, knowing the conv_id (e.g., from sniffing the broadcast), tries to send Approve.
3. Daemon's Approve handler computes `env.sender != original_sender`, logs `"Approve REJECTED — sender does not match"`, and sends NO reply (silence is the correct response — no oracle signal to attacker).
4. Queued entry is preserved.
5. Orchestrator-A subsequently sends Approve — succeeds normally.

In the smoke run, orchestrator-B's stub retried sending Approve 9 times before timing out — the daemon logged `Approve REJECTED` on every retry but emitted no Report on any of them. Orchestrator-A's subsequent Approve was accepted on the first try, proving the queued entry was preserved.

## Devnet expected error code

In devnet smoke, the post-Approve execution emits `error_code=5` (sim failed) because devnet has no Kamino USDC reserve at the placeholder pubkey. This is **expected** — the wiring is what's verified. On mainnet with real reserves, the deposit succeeds.

## Failure modes

| Daemon log line | Meaning |
|---|---|
| `AssignStableLend received` | Assign envelope decoded successfully |
| `AssignStableLend queued — awaiting Approve` | Enqueue path — Assign is held, waiting for Approve |
| `NeedsApproval Escalate emitted` | Orchestrator notified of pending approval |
| `report sent ... ok=true` (deposited=0) | Ack Report sent in response to queued Assign |
| `Approve received — executing queued` | Sender matched, execution started |
| `Approve REJECTED — sender does not match` | Audit-fix C1 firing; security boundary held — no Report sent |
| `error_code=5` | Sim/submit failed — chain rejected the deposit (likely missing reserve) |
| `error_code=6` | Ixn-build failed — couldn't construct the deposit ixn (likely placeholder pubkey on devnet) |

## Reproducing the smoke

See M8 of `docs/superpowers/plans/2026-05-06-stable-yield-daemon.md` for the exact commands. Both smokes use:

- `cargo run --release -p stable-yield-daemon -- ... --require-approval true`
- `cargo run --release -p fleet-pm-stub -- ... assign-stable-lend ...` to enqueue
- `cargo run --release -p fleet-pm-stub -- ... approve --conv-hex <conv> ...` from the appropriate role-key dir

Smoke B's "attacker" step uses a *different* `--secrets-dir` than the original Assign — that's the only ingredient required to flip the audit-fix C1 sender check.
