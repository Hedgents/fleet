# Stable-Yield Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `stable-yield-daemon` — fleet's second earning role. Supplies USDC into Kamino lending (no leverage, no swap, no Jito) and reports passive lending yield. Sole purpose: prove the fleet has *diversified* strategies, not just one bot.

**Architecture:** Standalone crate `crates/stable-yield-daemon/` lifting ~80% of multiply-daemon's plumbing (NodeService embed, role identity, RpcContext, SigningWhitelist, caps pattern, approval queue, Report emission). The strategy itself is dramatically simpler — one Kamino `lendingMarketDeposit` ixn, no borrow, no loop. Caps reflect the simpler risk: USDC max position, no LTV, no slippage. Approval gate behaves identically to multiply's M8 implementation.

**Tech Stack:** Rust, libp2p 0.54 (via `zerox1-node-enterprise`), `zerox1-defi-runtime`, `zerox1-defi-protocols::protocols::kamino_loader`, `zerox1-defi-wallet` (yes — this daemon DOES sign transactions), `ciborium`, `tokio`, `tracing`.

---

## File Structure

```
crates/stable-yield-daemon/
├── Cargo.toml
├── src/
│   ├── main.rs                — CLI args, boot, NodeService embed
│   ├── caps.rs                — USDC max, no leverage, validate_assign for AssignStableLend
│   ├── dispatch.rs            — Inbox dispatch (lift from multiply, simplified)
│   ├── lend.rs                — Build kamino deposit ixn, sign, submit/simulate
│   ├── approval.rs            — Lift verbatim from multiply (with audit-fix C1 already applied)
│   ├── kamino.rs              — Whitelist program IDs (subset of multiply's; no Jito needed)
│   ├── telemetry.rs           — Position polling + APR estimate logging
│   └── lib.rs
└── tests/
    ├── caps_test.rs
    ├── approval_test.rs
    └── lend_test.rs
```

**Protocol additions (in `zerox1-protocol`):**

```rust
// crates/zerox1-protocol/src/fleet/stable_lend.rs
pub struct AssignStableLend {
    pub market: [u8; 32],            // Kamino lending market pubkey
    pub reserve: [u8; 32],           // USDC reserve pubkey
    pub usdc_lamports: u64,          // amount to deposit (6 decimals)
    pub deadline_unix: u64,
}

pub struct ReportStableLend {
    pub header: ReportHeader,
    pub deposited_usdc_lamports: u64,
    pub current_apr_bps: u16,        // optional — telemetry estimate
    pub tx_signature: Option<[u8; 64]>,
}
```

---

## Milestones

### M1: Protocol types + workspace wiring

**Files:**
- Create: `crates/zerox1-protocol/src/fleet/stable_lend.rs`
- Modify: `crates/zerox1-protocol/src/fleet/mod.rs` (re-export)
- Modify: `Cargo.toml` (workspace members — add `crates/stable-yield-daemon`)
- Create: `crates/stable-yield-daemon/Cargo.toml`

- [ ] **Step 1: Define `AssignStableLend` + `ReportStableLend` structs (Borsh + Serde derives matching AssignMultiply convention)**
- [ ] **Step 2: Re-export from fleet/mod.rs**
- [ ] **Step 3: Stable-yield Cargo.toml — copy multiply's deps, drop Jito-specific crates**
- [ ] **Step 4: `cargo build -p zerox1-protocol -p stable-yield-daemon` clean (empty main is OK at this stage)**
- [ ] **Step 5: Commit `stable-yield: M1 — protocol types + crate scaffold`**

### M2: Hard-coded safety caps

**Files:**
- Create: `crates/stable-yield-daemon/src/caps.rs`
- Create: `crates/stable-yield-daemon/tests/caps_test.rs`

- [ ] **Step 1: Compile-time constants:**
  ```
  pub const MAX_POSITION_USDC_LAMPORTS: u64 = 5_000_000_000_000;  // $5M USDC
  pub const MIN_POSITION_USDC_LAMPORTS: u64 = 1_000_000;          // $1 USDC (no dust)
  ```
- [ ] **Step 2: `validate_assign(a: &AssignStableLend) -> Result<()>` — enforces both bounds**
- [ ] **Step 3: Sanity test in caps_test.rs (lift pattern from multiply::caps tests)**
- [ ] **Step 4: Commit `stable-yield: M2 — hard-coded safety caps`**

### M3: CLI args + sim-only / mainnet gates

**Files:**
- Create: `crates/stable-yield-daemon/src/main.rs`

- [ ] **Step 1: clap Args — `--secrets-dir`, `--wallet`, `--rpc-url`, `--listen`, `--bootstrap`, `--network`, `--simulate-only`, `--require-approval`, `--max-position-usdc-lamports`, `--i-understand-this-is-mainnet`**
- [ ] **Step 2: Network/cap gates — copy verbatim from multiply main.rs (network whitelist, mainnet ack flag, position cap bound)**
- [ ] **Step 3: Genesis-hash cross-check — copy `verify_network_matches_rpc` from multiply (audit-fix I3)**
- [ ] **Step 4: Boot path: load role key + Solana wallet, build RpcContext, NodeService, BEACON loop with shared nonce**
- [ ] **Step 5: Devnet smoke — boot daemon, confirm BEACONs**
- [ ] **Step 6: Commit `stable-yield: M3 — CLI args + boot + network gates`**

### M4: Approval queue + dispatch loop

**Files:**
- Create: `crates/stable-yield-daemon/src/approval.rs`
- Create: `crates/stable-yield-daemon/src/dispatch.rs`

- [ ] **Step 1: Lift `ApprovalQueue` verbatim from multiply (already includes audit-fix C1 sender check)**
- [ ] **Step 2: Adapt to use `AssignStableLend` instead of `AssignMultiply`**
- [ ] **Step 3: Re-run the 4 approval unit tests on the adapted version (sender match, mismatch rejected, NotFound, cap)**
- [ ] **Step 4: Dispatch loop: handle `MsgType::Assign` (validate → queue if `require_approval`, else execute) and `MsgType::Approve` (sender-match check → re-validate caps → execute)**
- [ ] **Step 5: Approval `Escalate(Notice, NeedsApproval)` emit pattern — lift from multiply**
- [ ] **Step 6: Commit `stable-yield: M4 — dispatch + approval queue`**

### M5: Devnet smoke #1 (sim-only round-trip, stub)

**Files:**
- Create: `crates/fleet-pm-stub/src/bin/assign_stable_lend.rs` (or extend existing stub)

- [ ] **Step 1: Add `assign-stable-lend` subcommand to fleet-pm-stub mirroring `assign-multiply`**
- [ ] **Step 2: Boot stable-yield-daemon with `--simulate-only` + `--require-approval=false`**
- [ ] **Step 3: Stub sends BEACON, then AssignStableLend with a $10 USDC payload**
- [ ] **Step 4: Daemon validates caps, falls through to lend.rs (stubbed for now: returns ok=true with tx_signature=None)**
- [ ] **Step 5: Verify Report round-trip — stub exits 0 on receipt**
- [ ] **Step 6: Commit `stable-yield: M5 — devnet sim-only round-trip`**

### M6: Implement Kamino USDC supply ixn

**Files:**
- Create: `crates/stable-yield-daemon/src/lend.rs`
- Create: `crates/stable-yield-daemon/src/kamino.rs`

- [ ] **Step 1: Lift Kamino `init_obligation_if_missing` + `refresh_reserve` ixn builders from defi-protocols (or multiply::kamino)**
- [ ] **Step 2: Implement `build_lending_market_deposit_ixn(payer, market, reserve, amount)` — single-leg deposit, no borrow**
- [ ] **Step 3: `kamino::whitelist_program_ids` — subset: Kamino lend, SPL Token, ATA, system, compute-budget (no Jito stake-pool, no SPL stake-pool)**
- [ ] **Step 4: `lend::run_or_simulate(ctx, &payload, conv) -> Result<ReportStableLend>` — verify_ixns, sign, submit/simulate, return Report with deposited amount + obligation LTV (will be 0 for unleveraged supply)**
- [ ] **Step 5: Devnet smoke — boot with real wallet, send $10 AssignStableLend, observe a real devnet tx OR a sim-only success Report**
- [ ] **Step 6: Commit `stable-yield: M6 — Kamino USDC supply ixn`**

### M7: Position telemetry + APR estimate

**Files:**
- Create: `crates/stable-yield-daemon/src/telemetry.rs`

- [ ] **Step 1: Periodic poll (CLI flag `--telemetry-interval-secs`, default 60s) of own obligation**
- [ ] **Step 2: Decode reserve `liquidity` fields to estimate current supply APR (Kamino exposes `borrowFactor`, `optimalUtilization`, etc. — formula in `klend` SDK)**
- [ ] **Step 3: JSONL log line: `{ ts, deposited_usdc, supply_apr_bps, accrued_interest_estimate }`**
- [ ] **Step 4: Output path: `--telemetry-log`, default `stable-yield-pnl.jsonl` (gitignored)**
- [ ] **Step 5: Commit `stable-yield: M7 — position telemetry + APR estimate`**

### M8: Manual-approval flow end-to-end

**Files:**
- Modify: `crates/fleet-pm-stub/src/main.rs` (ensure `approve` subcommand handles stable-lend conv_ids — should already, since approve is generic)

- [ ] **Step 1: Boot daemon with `--require-approval=true`**
- [ ] **Step 2: Send AssignStableLend → daemon queues + emits NeedsApproval Escalate**
- [ ] **Step 3: Send Approve from same orchestrator → daemon executes → Report**
- [ ] **Step 4: Verify same-orchestrator round-trip green; verify cross-orchestrator Approve REJECTED with no Report (audit-fix C1 on stable-yield)**
- [ ] **Step 5: Commit `stable-yield: M8 — manual-approval flow verified`**

### M9: Mainnet runbook ($50 USDC)

**Files:**
- Create: `docs/runbooks/stable-yield-mainnet-tiny.md`

- [ ] **Step 1: Document mainnet boot — wallet provisioning, RPC endpoint, role key**
- [ ] **Step 2: Document the $50 USDC sanity test — single AssignStableLend, observe deposit on Solscan, verify Kamino UI shows the position**
- [ ] **Step 3: Document approval workflow — operator manually issues Approve from a separate machine**
- [ ] **Step 4: Document teardown — `lendingMarketRepay` is N/A here (no debt); withdrawal is `lendingMarketWithdraw`. Document the withdrawal command for unwinding the test position.**
- [ ] **Step 5: Document earning expectations — USDC supply on Kamino is currently ~5-8% APR; on $50 that's ~$0.20-0.32/month. This is a "does the round-trip work on mainnet" test, not an earnings test.**
- [ ] **Step 6: Commit `stable-yield: M9 — mainnet runbook`**

### M10: Withdrawal path (for unwinding $50 test)

**Files:**
- Modify: `crates/zerox1-protocol/src/fleet/stable_lend.rs` (add `WithdrawStableLend` msg type)
- Modify: `crates/stable-yield-daemon/src/dispatch.rs`
- Modify: `crates/stable-yield-daemon/src/lend.rs`

- [ ] **Step 1: Add `WithdrawStableLend { market, reserve, usdc_lamports_or_max, deadline_unix }` payload type**
- [ ] **Step 2: Dispatch arm — same caps + approval treatment as Assign**
- [ ] **Step 3: Build `lendingMarketWithdraw` ixn (lift from defi-protocols)**
- [ ] **Step 4: Devnet smoke — deposit then withdraw on the same conv chain**
- [ ] **Step 5: Document the operator command in the mainnet runbook**
- [ ] **Step 6: Commit `stable-yield: M10 — withdrawal path`**

---

## Verification gates

After all 10 milestones:
- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (target: existing 202 + ~12 new tests)
- [ ] `cargo tree -p stable-yield-daemon` shows `zerox1-defi-wallet` (this daemon SHOULD sign txs — it's not the read-only riskwatcher)
- [ ] Devnet sim-only round-trip green (M5)
- [ ] Devnet real-tx round-trip green (M6)
- [ ] Mainnet runbook reproducible
- [ ] Same-orchestrator approval flow works; cross-orchestrator Approve rejected (audit-fix C1 carryover)

## Self-review notes

- Stable-yield is intentionally simpler than multiply: one ixn, no leverage, no swap. The point is **strategy diversity** in the fleet, not novelty in this daemon.
- Caps are looser than multiply's — no LTV cap (no leverage), no slippage cap (no swap). The only cap is position size.
- Withdrawal added as M10 because $50 mainnet test should be reversible. Without it the test funds would be stuck in Kamino requiring out-of-band recovery.
- APR estimate (M7) is best-effort telemetry. The authoritative APR comes from on-chain reserve state; the daemon's estimate is an indicator, not an oracle.
- Riskwatcher (the parallel plan) will observe stable-yield Reports the same way it observes multiply's. No riskwatcher change required for this daemon — `subject = env.sender` already keys positions correctly.

## Parallelism with riskwatcher plan

These two plans CAN execute concurrently in separate worktrees provided:
- Both edit `crates/zerox1-protocol/src/fleet/mod.rs` only by ADDING new submodule re-exports (riskwatcher doesn't touch this; stable-yield adds `stable_lend`)
- Riskwatcher's M3 (observer) only needs to be able to decode `ReportMultiply` AND `ReportStableLend` — it currently only handles the former. Easy follow-up: when stable-yield M1 lands, add a small PR to riskwatcher's observer.rs to decode the second variant.
