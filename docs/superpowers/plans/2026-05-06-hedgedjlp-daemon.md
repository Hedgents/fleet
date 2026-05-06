# HedgedJLP Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `hedgedjlp-daemon` — fleet's delta-neutral basis trader. Buys Jupiter Perps LP token (JLP) for its yield (~30-50% APR from perp fees), simultaneously shorts the directional component on Drift to neutralize price exposure. Net = JLP yield - short funding cost. Targets ~15-25% net APR.

**Architecture:** Standalone crate `crates/hedgedjlp-daemon/` modeled on multiply-daemon. Two distinct execution legs: (1) **JLP leg** — buy JLP via Jupiter swap; (2) **Hedge leg** — open short SOL/ETH/BTC perps on Drift sized to match JLP's underlying exposure. Periodic rebalancer runs every N minutes: re-reads JLP composition, re-sizes shorts to maintain target delta. Daemon only writes; risk decisions still live in riskwatcher-daemon (paired peer).

**Tech Stack:** Rust, libp2p 0.54 (via `zerox1-node-enterprise`), `zerox1-defi-runtime`, `zerox1-defi-wallet`, `zerox1-defi-protocols::protocols::{jupiter, drift}` (Jupiter for JLP buy; Drift for perp shorts), `ciborium`, `tokio`, `tracing`. Drift integration is the heaviest new dep — Drift's Rust SDK is non-trivial; v0 may use raw ixn building if SDK is too invasive.

---

## File Structure

```
crates/hedgedjlp-daemon/
├── Cargo.toml
├── src/
│   ├── main.rs              — CLI args, boot, NodeService embed
│   ├── caps.rs              — max position, max delta drift, funding-rate ceiling
│   ├── dispatch.rs          — Inbox dispatch (Assign/Approve/Withdraw)
│   ├── approval.rs          — generic ApprovalQueue<P> (lift from stable-yield)
│   ├── kamino.rs            — whitelist program IDs (Jupiter v6, Drift v2, SPL Token, etc.)
│   ├── jlp.rs               — JLP buy via Jupiter; reads JLP composition from on-chain state
│   ├── hedge.rs             — Drift perp short open/close/resize
│   ├── rebalance.rs         — periodic delta-rebalance task
│   ├── delta.rs             — pure math: compute portfolio delta from JLP composition
│   ├── telemetry.rs         — JSONL log: {ts, jlp_value_usd, hedge_value_usd, delta_pct, est_apr_bps}
│   └── lib.rs
└── tests/
    ├── caps_test.rs
    ├── delta_test.rs        — comprehensive: synthetic JLP composition → expected hedge sizes
    └── approval_test.rs
```

**Protocol additions (`zerox1-protocol`):**

```rust
// crates/zerox1-protocol/src/fleet/hedgedjlp.rs (likely already exists per memory)
pub struct AssignHedgedJlp {
    pub usdc_lamports: u64,           // total capital to deploy
    pub target_delta_bps: i16,        // 0 = perfectly neutral; +500 = 5% long bias
    pub max_funding_rate_bps: u16,    // unwind hedge if 24h-avg funding > this
    pub deadline_unix: u64,
}
pub struct ReportHedgedJlp {
    pub header: ReportHeader,
    pub jlp_acquired_lamports: u64,
    pub hedge_notional_usdc: u64,
    pub current_delta_bps: i16,
    pub tx_signatures: Vec<String>,   // multiple txs: 1 swap + N hedge opens
}
pub struct WithdrawHedgedJlp { ... }  // unwind both legs
```

---

## Milestones

### M1: Crate scaffold + protocol types

**Files:**
- Create: `crates/hedgedjlp-daemon/Cargo.toml`, `src/main.rs` (stub), `src/lib.rs`
- Create or modify: `crates/zerox1-protocol/src/fleet/hedgedjlp.rs`
- Modify: root `Cargo.toml`, `crates/zerox1-protocol/src/fleet/mod.rs`

- [ ] **Step 1: AssignHedgedJlp + ReportHedgedJlp + WithdrawHedgedJlp types in protocol crate (match house style: serde-only, tx_signature: Option<String>)**
- [ ] **Step 2: Add hedgedjlp-daemon to workspace members**
- [ ] **Step 3: Cargo.toml deps — copy stable-yield's, add Drift SDK if available; mark Jupiter integration as defi-protocols dep**
- [ ] **Step 4: Stub main.rs that boots, BEACONs, and exits (mirrors stable-yield M1 + M3 baseline)**
- [ ] **Step 5: `cargo build --workspace` clean**
- [ ] **Step 6: Commit `hedgedjlp: M1 — crate scaffold + protocol types`**

### M2: Hard-coded safety caps

**Files:**
- Create: `crates/hedgedjlp-daemon/src/caps.rs`

- [ ] **Step 1: Constants:**
  ```
  pub const MAX_POSITION_USDC_LAMPORTS: u64 = 5_000_000_000_000;  // $5M
  pub const MIN_POSITION_USDC_LAMPORTS: u64 = 100_000_000;        // $100 (JLP+hedge has fixed costs)
  pub const MAX_DELTA_DRIFT_BPS: u16 = 1000;                      // 10% — beyond this trigger emergency rebalance
  pub const MAX_FUNDING_RATE_BPS_HARDCAP: u16 = 5000;             // 50% APR funding — orchestrator caps further
  pub const MAX_LEVERAGE_ON_HEDGE: u8 = 3;                        // Drift leverage cap
  ```
- [ ] **Step 2: validate_assign(a: &AssignHedgedJlp) — bounds-check usdc_lamports, target_delta_bps within ±MAX_DELTA_DRIFT_BPS, max_funding_rate ≤ hardcap**
- [ ] **Step 3: 6 unit tests (within-bounds, above-max, below-min, delta out-of-range, funding above cap, sensibility sanity)**
- [ ] **Step 4: Commit `hedgedjlp: M2 — caps`**

### M3: CLI + boot + network gates

**Files:**
- Modify: `crates/hedgedjlp-daemon/src/main.rs`

- [ ] **Step 1: Lift main.rs structure from stable-yield-daemon (clap Args, network gates, genesis-hash check, BEACON loop, NodeService embed) — adapted for hedgedjlp args**
- [ ] **Step 2: Add CLI flags: `--rebalance-interval-secs` (default 600 = 10 min), `--drift-cluster` (mainnet/devnet), `--max-position-usdc-lamports`**
- [ ] **Step 3: Devnet smoke — boot daemon, observe BEACON + listening lines**
- [ ] **Step 4: Negative gates — mainnet without ack, cap above bound, genesis mismatch — all bail correctly**
- [ ] **Step 5: Commit `hedgedjlp: M3 — CLI + boot + gates`**

### M4: Approval queue + dispatch loop

**Files:**
- Create: `crates/hedgedjlp-daemon/src/approval.rs`, `src/dispatch.rs`

- [ ] **Step 1: Lift generic ApprovalQueue<P> from stable-yield (audit-fix C1 sender-match already baked in)**
- [ ] **Step 2: DispatchCtx with three queues: assign / withdraw / rebalance** (rebalance is internally-issued; orchestrator can also force one)
- [ ] **Step 3: dispatch::run handles MsgType::Assign, Approve, Withdraw, plus a new MsgType::Rebalance if needed (or reuse Assign with discriminator — match the M10 stable-yield-daemon choice)**
- [ ] **Step 4: jlp::run_or_simulate + hedge::run_or_simulate are M4 stubs returning ok=true Reports — real ixns land in M6/M7**
- [ ] **Step 5: 4+ approval tests passing (matching, mismatch, NotFound, cap)**
- [ ] **Step 6: Commit `hedgedjlp: M4 — approval + dispatch with stubs`**

### M5: Devnet sim-only round-trip via fleet-pm-stub

**Files:**
- Modify: `tools/fleet-pm-stub/src/main.rs` (add `assign-hedgedjlp` + `withdraw-hedgedjlp` subcommands)

- [ ] **Step 1: Subcommand mirrors assign-stable-lend: builds CBOR, signs envelope, awaits Report**
- [ ] **Step 2: Devnet smoke — boot daemon with --simulate-only=true --require-approval=false, send Assign, observe Report**
- [ ] **Step 3: ASSIGN exits 0 with stub Report (deposited=requested, no tx)**
- [ ] **Step 4: Commit `hedgedjlp: M5 — devnet sim-only round-trip`**

### M6: Implement JLP buy leg

**Files:**
- Create: `crates/hedgedjlp-daemon/src/jlp.rs`
- Possibly modify: `crates/zerox1-defi-protocols/src/protocols/jupiter.rs` (additive)

- [ ] **Step 1: Read existing Jupiter integration in defi-protocols. JLP is bought via a normal Jupiter swap from USDC to JLP mint pubkey** (`27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4` mainnet)
- [ ] **Step 2: build_jlp_buy_ixns(payer, usdc_amount, slippage_bps) -> Vec<Instruction> via Jupiter v6 quote + swap API or direct cpi**
- [ ] **Step 3: jlp::run_or_simulate replaces M4 stub: build ixns → whitelist verify → submit/sim → Report with jlp_acquired_lamports + tx_signature**
- [ ] **Step 4: Devnet smoke — sim mode against placeholder mints; expect error (devnet has no JLP); wiring confirmed by whitelist verify success**
- [ ] **Step 5: 2-3 unit tests for parameter packing in build_jlp_buy_ixns**
- [ ] **Step 6: Commit `hedgedjlp: M6 — JLP buy leg via Jupiter`**

### M7: JLP composition + delta math

**Files:**
- Create: `crates/hedgedjlp-daemon/src/delta.rs`
- Possibly: `crates/zerox1-defi-protocols/src/protocols/jupiter_perps.rs` (additive — JLP custody account decoder)

- [ ] **Step 1: Read JLP custody accounts (one per asset: SOL, ETH, BTC, USDC, USDT). Each has owned_amount + price_oracle. Decode struct documented in Jupiter Perps program**
- [ ] **Step 2: pure fn compute_delta(jlp_lamports: u64, custody_state: &[CustodyAccount]) -> PortfolioDelta { sol_lamports, eth_lamports, btc_lamports, total_usd }**
- [ ] **Step 3: Comprehensive unit tests — synthetic compositions with known expected hedge sizes; edge case: 100% USDC (zero delta), 100% SOL (full SOL hedge), realistic mix**
- [ ] **Step 4: Commit `hedgedjlp: M7 — JLP composition + delta math`**

### M8: Implement Drift hedge leg

**Files:**
- Create: `crates/hedgedjlp-daemon/src/hedge.rs`
- Modify: `crates/zerox1-defi-protocols/src/protocols/drift.rs` (additive — perp open/close ixn builders if not present)

- [ ] **Step 1: Drift perp index lookup — SOL_PERP, ETH_PERP, BTC_PERP market indices**
- [ ] **Step 2: hedge::open_shorts(payer, target_delta: PortfolioDelta) -> Vec<Instruction> — initialize_user_account if missing, deposit USDC margin, open 3 short positions sized to neutralize delta**
- [ ] **Step 3: hedge::close_shorts(payer) -> Vec<Instruction> — flatten all 3 perps, withdraw margin**
- [ ] **Step 4: hedge::run_or_simulate replaces M4 stub. Whitelist verify, submit/sim, Report with hedge_notional_usdc + 3 tx_signatures**
- [ ] **Step 5: Devnet smoke (Drift devnet exists) — boot daemon, send Assign with $100, observe both legs simulated. Real submit deferred to mainnet.**
- [ ] **Step 6: Commit `hedgedjlp: M8 — Drift hedge leg`**

### M9: Periodic rebalancer

**Files:**
- Create: `crates/hedgedjlp-daemon/src/rebalance.rs`

- [ ] **Step 1: Async task running every --rebalance-interval-secs**
- [ ] **Step 2: Read JLP custody → compute current delta → compare to target → if |diff| > MAX_DELTA_DRIFT_BPS, build resize ixns (close excess shorts or add new ones)**
- [ ] **Step 3: Emit Escalate(Notice, RebalanceTriggered) on action; no orchestrator approval required for rebalances WITHIN ±MAX_DELTA_DRIFT_BPS — those are routine. ONLY actions OUTSIDE that band require approval (suggests fundamental composition shift).**
- [ ] **Step 4: Funding-rate watch: also check 24h-avg SOL/ETH/BTC perp funding; if any exceeds AssignHedgedJlp.max_funding_rate_bps, emit Escalate(Warning, FundingRateExceeded) and PAUSE rebalances until manual reset**
- [ ] **Step 5: Devnet smoke — synthetic delta drift triggers a rebalance attempt (simulation only). Verify Escalate emitted.**
- [ ] **Step 6: Commit `hedgedjlp: M9 — periodic rebalancer + funding watch`**

### M10: Telemetry + position reports

**Files:**
- Create: `crates/hedgedjlp-daemon/src/telemetry.rs`

- [ ] **Step 1: Periodic poll (default 60s): read JLP balance + Drift account + funding rates, write JSONL line: `{ts, jlp_value_usd, jlp_yield_apr_bps, hedge_notional_usdc, hedge_funding_apr_bps, net_apr_bps, current_delta_bps}`**
- [ ] **Step 2: Compute net_apr from JLP yield - hedge funding cost. JLP yield is documented in Jupiter's program state as a 7-day moving average; funding from Drift's market state.**
- [ ] **Step 3: --telemetry-log + --telemetry-interval-secs CLI flags (default `hedgedjlp-pnl.jsonl`, gitignored)**
- [ ] **Step 4: Commit `hedgedjlp: M10 — telemetry`**

### M11: Withdrawal path

**Files:**
- Modify: `crates/hedgedjlp-daemon/src/{dispatch.rs,jlp.rs,hedge.rs}`

- [ ] **Step 1: WithdrawHedgedJlp dispatch arm — symmetric to AssignHedgedJlp**
- [ ] **Step 2: Unwind sequence: close all 3 Drift shorts → withdraw Drift margin → swap JLP → USDC → Report ReportHedgedJlpWithdraw with usdc_returned_lamports**
- [ ] **Step 3: Devnet smoke — withdraw subcommand on stub, daemon sim'd unwind**
- [ ] **Step 4: Commit `hedgedjlp: M11 — withdrawal path`**

### M12: Mainnet runbook

**Files:**
- Create: `docs/runbooks/hedgedjlp-mainnet-tiny.md`

- [ ] **Step 1: $200 USDC test (smaller than stable-yield because hedgedjlp has fixed costs from Jupiter swap + Drift margin)**
- [ ] **Step 2: Document expected: ~15-25% net APR on $200 = $0.08-0.14/day. Failure modes: funding rate spikes, JLP utilization caps, Drift insurance-fund pause**
- [ ] **Step 3: Two-phase: --simulate-only first, then real submit**
- [ ] **Step 4: 24h watch instructions emphasizing funding-rate vigilance**
- [ ] **Step 5: Commit `hedgedjlp: M12 — mainnet runbook`**

---

## Verification gates

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (target: existing 224 + ~25 new from caps + delta + approval + jlp + hedge tests)
- [ ] Devnet smoke: full round-trip in sim mode (Assign → both legs sim → Report)
- [ ] Devnet smoke: rebalance task fires within configured interval
- [ ] Mainnet runbook reviewed by operator before $200 test

## Self-review notes

- HedgedJLP is the most complex daemon — two legs, periodic rebalancer, two protocol integrations (Jupiter + Drift). Estimate: 10-15 days at the per-task review pace, vs. stable-yield's 2-3 days.
- Drift integration is the highest-risk dependency. If Drift Rust SDK is unstable or absent, fall back to raw ixn building from Drift IDL — slower but viable.
- The funding-rate watch (M9) is a soft kill switch. A truly hostile funding spike (50%+ APR) would erase JLP yield; better to unwind and lose principal-flat than bleed funding.
- Riskwatcher (parallel agent) will observe hedgedjlp's Reports + Escalates the same way it observes multiply's. No riskwatcher change required for this daemon.
- Net APR depends entirely on JLP yield - funding cost. Both fluctuate; the daemon does not "earn a fixed rate" — it tracks a spread.
