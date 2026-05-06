# Speculator Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `speculator-daemon` — fleet's directional/basis trader. v0 strategy: **funding-rate harvest** — when SOL perp funding > threshold, short the perp (collect funding) and buy spot SOL (neutralize price exposure). Earnings = perp funding paid by longs. Net APR scales with funding rate.

**Architecture:** Standalone crate `crates/speculator-daemon/`. Two execution legs: (1) **Spot leg** — buy SOL with USDC via Jupiter; (2) **Perp leg** — short SOL on Drift sized to match spot delta. Periodic monitor checks funding rate; if it falls below the entry threshold (or goes negative), unwind. v0 trades ONLY SOL — multi-asset basis trading is post-v1.

**Tech Stack:** Rust, libp2p 0.54, `zerox1-defi-runtime`, `zerox1-defi-wallet`, `zerox1-defi-protocols::protocols::{jupiter, drift, pyth}` (Pyth for spot price oracle reads). Reuses ~40% of hedgedjlp's Drift integration (less than ideal — refactoring shared Drift helpers into defi-protocols would benefit both daemons).

---

## File Structure

```
crates/speculator-daemon/
├── Cargo.toml
├── src/
│   ├── main.rs              — CLI args, boot
│   ├── caps.rs              — max position, min funding threshold, max slippage
│   ├── dispatch.rs          — Inbox dispatch
│   ├── approval.rs          — generic ApprovalQueue<P>
│   ├── kamino.rs            — whitelist program IDs (Jupiter + Drift + Pyth + SPL)
│   ├── basis.rs             — open + close basis position (spot buy + perp short)
│   ├── monitor.rs           — periodic funding rate check + auto-unwind
│   ├── telemetry.rs         — JSONL log
│   └── lib.rs
└── tests/
    ├── caps_test.rs
    ├── basis_test.rs
    └── approval_test.rs
```

**Protocol additions:**

```rust
// crates/zerox1-protocol/src/fleet/speculator.rs (likely already exists per memory)
pub struct AssignBasisTrade {
    pub usdc_lamports: u64,                     // capital to deploy
    pub asset_perp_index: u8,                   // Drift market index — v0 only SOL_PERP
    pub min_funding_rate_entry_bps: u16,        // only enter if funding ≥ this
    pub auto_unwind_funding_floor_bps: i16,     // unwind if funding falls below this (can be negative)
    pub max_slippage_bps: u16,
    pub deadline_unix: u64,
}
pub struct ReportBasisTrade {
    pub header: ReportHeader,
    pub spot_acquired_lamports: u64,
    pub perp_short_size: u64,
    pub entry_funding_bps: i16,
    pub tx_signatures: Vec<String>,
}
```

---

## Milestones

### M1: Crate scaffold + protocol types

**Files:**
- Create: `crates/speculator-daemon/Cargo.toml`, `src/main.rs` (stub), `src/lib.rs`
- Modify: `crates/zerox1-protocol/src/fleet/speculator.rs` (define or extend), `fleet/mod.rs`, root `Cargo.toml`

- [ ] **Step 1: AssignBasisTrade + ReportBasisTrade + WithdrawBasisTrade types in protocol crate**
- [ ] **Step 2: speculator-daemon Cargo.toml — share deps with hedgedjlp (Jupiter + Drift)**
- [ ] **Step 3: Stub main.rs (lift stable-yield M1 baseline)**
- [ ] **Step 4: `cargo build --workspace` clean**
- [ ] **Step 5: Commit `speculator: M1 — crate scaffold + protocol types`**

### M2: Hard-coded safety caps

**Files:**
- Create: `crates/speculator-daemon/src/caps.rs`

- [ ] **Step 1: Constants:**
  ```
  pub const MAX_POSITION_USDC_LAMPORTS: u64 = 5_000_000_000_000;  // $5M
  pub const MIN_POSITION_USDC_LAMPORTS: u64 = 100_000_000;        // $100 (Drift margin floor)
  pub const MIN_FUNDING_RATE_ENTRY_HARDCAP_BPS: u16 = 100;        // require ≥1% APR funding to enter — below this, basis is noise
  pub const MAX_SLIPPAGE_BPS: u16 = 100;
  pub const MAX_PERP_LEVERAGE: u8 = 2;                            // delta-neutral basis = 1× nominal; cap 2× for safety
  ```
- [ ] **Step 2: validate_assign(a: &AssignBasisTrade) bounds-checks all fields, requires asset_perp_index == 0 (SOL_PERP) for v0**
- [ ] **Step 3: 5+ unit tests**
- [ ] **Step 4: Commit `speculator: M2 — caps`**

### M3: CLI + boot + network gates

**Files:**
- Modify: `crates/speculator-daemon/src/main.rs`

- [ ] **Step 1: clap Args (lift stable-yield M3 pattern)**
- [ ] **Step 2: Add `--monitor-interval-secs` (default 300 = 5 min — funding updates hourly on Drift but check more often)**
- [ ] **Step 3: Genesis-hash check, mainnet ack flag, cap validation**
- [ ] **Step 4: Devnet smoke — boot, BEACON, exit cleanly**
- [ ] **Step 5: 3 negative-gate smokes pass**
- [ ] **Step 6: Commit `speculator: M3 — CLI + boot`**

### M4: Approval queue + dispatch loop

**Files:**
- Create: `crates/speculator-daemon/src/approval.rs`, `src/dispatch.rs`

- [ ] **Step 1: Lift generic ApprovalQueue<P> from stable-yield-daemon (audit-fix C1 baked in)**
- [ ] **Step 2: DispatchCtx with assign + withdraw queues**
- [ ] **Step 3: dispatch::run handles MsgType::Assign/Approve/Withdraw**
- [ ] **Step 4: basis::run_or_simulate is M4 stub returning ok=true Report**
- [ ] **Step 5: 4 approval tests passing**
- [ ] **Step 6: Commit `speculator: M4 — approval + dispatch with stubs`**

### M5: Devnet sim-only round-trip

**Files:**
- Modify: `tools/fleet-pm-stub/src/main.rs` (add `assign-basis` subcommand)

- [ ] **Step 1: Subcommand mirrors assign-stable-lend pattern**
- [ ] **Step 2: Devnet smoke — sim-only Assign → daemon stub → Report → stub exits 0**
- [ ] **Step 3: Commit `speculator: M5 — devnet sim-only round-trip`**

### M6: Funding rate read

**Files:**
- Possibly: `crates/zerox1-defi-protocols/src/protocols/drift.rs` (additive — funding rate getter)

- [ ] **Step 1: read_funding_rate(market_index) -> Result<i32_bps> — reads Drift's PerpMarket account, computes current funding rate from `last_funding_rate` + `last_funding_rate_ts`**
- [ ] **Step 2: Funding rate convention: positive = longs pay shorts (good for our short-perp basis trade); negative = shorts pay longs (bad)**
- [ ] **Step 3: Unit tests with synthetic Drift state**
- [ ] **Step 4: Commit `speculator: M6 — Drift funding rate read`**

### M7: Open basis position

**Files:**
- Create: `crates/speculator-daemon/src/basis.rs`

- [ ] **Step 1: basis::open(payer, usdc_lamports, max_slippage_bps) -> Vec<Instruction>**
- [ ] **Step 2: Sequence: (a) read SOL/USDC price from Pyth; (b) split USDC: ~50% for spot SOL, ~50% for Drift margin; (c) build Jupiter swap USDC→SOL ixn; (d) build Drift initialize_user_account_if_missing; (e) build Drift deposit_collateral; (f) build Drift open_perp_short_position with size matching the spot SOL exposure**
- [ ] **Step 3: All ixns into one or two transactions (split if compute-budget exceeds 1.4M)**
- [ ] **Step 4: basis::run_or_simulate replaces M4 stub. Whitelist verify, submit/sim, build Report**
- [ ] **Step 5: Devnet smoke — sim mode, observe both legs ixn-built, whitelist verified**
- [ ] **Step 6: 3 unit tests for splitting/sizing math**
- [ ] **Step 7: Commit `speculator: M7 — open basis position`**

### M8: Funding-rate guard at entry

**Files:**
- Modify: `crates/speculator-daemon/src/dispatch.rs`, `src/basis.rs`

- [ ] **Step 1: BEFORE building basis-open ixns, query current funding rate via M6 helper**
- [ ] **Step 2: If funding < AssignBasisTrade.min_funding_rate_entry_bps → reject with error_code=7 ("funding below entry threshold"); send Report ok=false; do NOT submit any tx**
- [ ] **Step 3: Devnet smoke with synthetic low-funding fixture — confirm rejection path works**
- [ ] **Step 4: Commit `speculator: M8 — funding-rate guard at entry`**

### M9: Periodic monitor + auto-unwind

**Files:**
- Create: `crates/speculator-daemon/src/monitor.rs`

- [ ] **Step 1: Async task every --monitor-interval-secs**
- [ ] **Step 2: For each open position (state stored in `pending_positions: Mutex<Vec<OpenPosition>>` populated by basis::open Reports), check current funding rate**
- [ ] **Step 3: If funding < auto_unwind_funding_floor_bps OR funding has been below entry threshold for >24h → emit Escalate(Warning, BasisDecayed) AND queue an internal unwind**
- [ ] **Step 4: Unwind: close perp short → swap SOL → USDC → emit ReportBasisUnwind**
- [ ] **Step 5: Devnet smoke with synthetic funding decay fixture — confirm auto-unwind triggers**
- [ ] **Step 6: Commit `speculator: M9 — funding monitor + auto-unwind`**

### M10: Telemetry + mainnet runbook

**Files:**
- Create: `crates/speculator-daemon/src/telemetry.rs`
- Create: `docs/runbooks/speculator-mainnet-tiny.md`

- [ ] **Step 1: Telemetry JSONL: `{ts, position_count, total_notional_usdc, current_funding_bps, accrued_funding_usdc, est_apr_bps}`**
- [ ] **Step 2: Mainnet runbook for $300 test: $150 spot + $150 Drift margin = ~$150 short notional with 1× leverage. Funding rate = 10% APR → ~$0.04/day on $300**
- [ ] **Step 3: Document failure modes: funding rate flipping negative mid-position (auto-unwind triggers), Drift insurance fund halt, Jupiter swap slippage**
- [ ] **Step 4: Two-phase test: sim-only first, then real submit only after sim Report ok=true**
- [ ] **Step 5: Commit `speculator: M10 — telemetry + mainnet runbook`**

---

## Verification gates

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (target: pre-existing + ~18 new from caps/basis/approval/funding-read tests)
- [ ] Devnet round-trip green (sim-only, both legs)
- [ ] Funding-rate guard rejects sub-threshold entries
- [ ] Auto-unwind fires on synthetic funding decay
- [ ] Mainnet runbook reviewed before $300 test

## Self-review notes

- v0 trades ONLY SOL_PERP. Multi-asset (ETH, BTC) is a post-v1 feature — code is structured to allow asset_perp_index parameterization but caps reject anything but 0 in v0.
- The funding-rate threshold approach is conservative: requires ≥1% APR to enter. Real-world SOL funding is typically 5-50% APR during volatile periods, so this isn't a high bar.
- Auto-unwind on negative funding is critical. If funding flips negative, the basis trade reverses: shorts pay longs. Holding through that costs principal.
- ~40% of code (Drift account init, deposit, perp open/close) overlaps with hedgedjlp-daemon. Refactor opportunity: extract shared Drift ixn helpers into `defi-protocols::protocols::drift` — defer to a v0.2 cleanup pass.
- Net APR fluctuates with funding. Telemetry tracks accrued_funding_usdc as the truth; APR estimate is a 24h moving average.
- Riskwatcher observes speculator's Reports + Escalates the same way as multiply/stable-yield. No riskwatcher changes required.
