# HedgedJLP Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **2026-05-06 rewrite:** This plan was originally written against Drift Protocol as the perp-short venue. Drift was hacked April 2026 ($285M, DPRK-linked, durable-nonce social engineering); pre-relaunch, recovery facility hasn't activated. Fleet has pivoted to **Jupiter Perps** for the hedge leg. Key structural shifts versus the original plan are noted inline; the M1-M12 milestone numbering is preserved so cross-references in code stubs and historical commits still resolve.

**Goal:** Ship `hedgedjlp-daemon` — fleet's delta-neutral basis trader. Buys Jupiter Perps LP token (JLP) for its yield (~30-50% APR from perp fees), simultaneously shorts the directional component on **Jupiter Perps** itself to neutralize price exposure. Net = JLP yield - short borrow fee. Targets ~15-25% net APR.

**Architecture:** Standalone crate `crates/hedgedjlp-daemon/` modeled on multiply-daemon. Two distinct execution legs: (1) **JLP leg** — buy JLP via Jupiter swap; (2) **Hedge leg** — open short SOL/ETH/BTC perps on Jupiter Perps sized to match JLP's underlying exposure. Periodic rebalancer runs every N minutes: re-reads JLP composition, re-sizes shorts to maintain target delta. Daemon only writes; risk decisions still live in riskwatcher-daemon (paired peer).

**Structural elegance — single-denominator P&L.** Jupiter Perps' liquidity providers ARE the JLP holders. Counter-party to a Jupiter Perps short is JLP itself. So hedgedjlp longs JLP and shorts JLP-counter-party perps — net P&L is contained inside one fee economy. Perp shorters pay borrow fees → those flow into JLP yield. We pay them on the short leg, we collect them on the long leg. The basis trade is therefore self-hedging at the protocol layer in a way it never was on Drift, where the perp venue and the LP token were structurally separate ledgers.

**CRITICAL design correction (vs original Drift plan).** Jupiter Perps has **no funding rate** — only an hourly compounding **borrow fee** under a Gauntlet jump-rate model that scales with utilization. Implications throughout:

- The daemon's auto-unwind escape valve trips on **HIGH borrow rate** (eats yield), not the original "low or sign-flipped funding" condition.
- All `MAX_FUNDING_RATE_*` and `funding_floor` identifiers are renamed to `MAX_BORROW_RATE_*` and `borrow_ceiling`.
- The rebalancer's monitoring direction inverts: borrow spikes are bad (we're paying), so unwind on the upside, not the downside.

**2-tx execution model (vs Drift's direct fill).** Jupiter Perps does not directly fill on the user transaction. The daemon submits a *position-open request*; an off-chain Jupiter keeper executes within seconds (typically 1-3 slots). The daemon's M8 hedge-leg flow is therefore:

1. submit `create_increase_position_request` ixn → user tx confirms,
2. poll position-request account or position account state for execution,
3. on execution, the position appears in our account; on rejection or timeout, the request is closeable.

This is structurally different from the original Drift direct-fill model and changes M8 + M11 significantly.

**Tech Stack:** Rust, libp2p 0.54 (via `zerox1-node-enterprise`), `zerox1-defi-runtime`, `zerox1-defi-wallet`, `zerox1-defi-protocols::protocols::{jupiter, jupiter_perps}` (Jupiter swap for JLP buy; Jupiter Perps program for both LP composition reads and perp shorts), `ciborium`, `tokio`, `tracing`.

**Jupiter Perps integration paths.** No first-party Rust SDK. v0 paths:

- **Anchor CPI client** — `Garrett-Weber/jupiter-perpetuals-cpi` (MIT-licensed, mirrors the on-chain program interface).
- **IDL parsing reference** — `julianfssen/jupiter-perps-anchor-idl-parsing` (worked examples for decoding custody, position, and request accounts).
- **On-chain program ID** — `PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu`.

If the CPI client proves invasive, fall back to raw ixn building from the IDL — same trade-off the original plan envisaged for Drift, except Jupiter's IDL is publicly indexed and the program is verified.

---

## File Structure

```
crates/hedgedjlp-daemon/
├── Cargo.toml
├── src/
│   ├── main.rs              — CLI args, boot, NodeService embed
│   ├── caps.rs              — max position, max delta drift, borrow-rate ceiling
│   ├── dispatch.rs          — Inbox dispatch (Assign/Approve/Withdraw)
│   ├── approval.rs          — generic ApprovalQueue<P> (lift from stable-yield)
│   ├── kamino.rs            — whitelist program IDs (Jupiter v6, Jupiter Perps, SPL Token, etc.)
│   ├── jlp.rs               — JLP buy via Jupiter; reads JLP composition from on-chain state
│   ├── hedge.rs             — Jupiter Perps short open/close/resize via 2-tx request flow
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
// crates/zerox1-protocol/src/fleet/hedgedjlp.rs
pub struct AssignHedgedJlp {
    pub usdc_lamports: u64,           // total capital to deploy
    pub target_delta_bps: i16,        // 0 = perfectly neutral; +500 = 5% long bias
    pub max_borrow_rate_bps: u16,     // unwind hedge if 24h-avg borrow > this
    pub deadline_unix: u64,
}
pub struct ReportHedgedJlp {
    pub header: ReportHeader,
    pub jlp_acquired_lamports: u64,
    pub hedge_notional_usdc: u64,
    pub current_delta_bps: i16,
    pub tx_signatures: Vec<String>,   // multiple txs: 1 swap + N hedge open requests
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

- [ ] **Step 1: AssignHedgedJlp + ReportHedgedJlp + WithdrawHedgedJlp types in protocol crate (match house style: serde-only, tx_signature: Option<String>). `max_borrow_rate_bps`, NOT `max_funding_rate_bps`.**
- [ ] **Step 2: Add hedgedjlp-daemon to workspace members**
- [ ] **Step 3: Cargo.toml deps — copy stable-yield's; add `jupiter_perps` integration via either the CPI client or raw IDL builder (decide at implementation time).**
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
  pub const MAX_BORROW_RATE_BPS_HARDCAP: u16 = 5000;              // 50% APR borrow — orchestrator caps further
  pub const MAX_LEVERAGE_ON_HEDGE: u8 = 3;                        // Jupiter Perps leverage cap (program enforces ≤ ~50x; we cap much lower)
  ```
- [ ] **Step 2: validate_assign(a: &AssignHedgedJlp) — bounds-check usdc_lamports, target_delta_bps within ±MAX_DELTA_DRIFT_BPS, max_borrow_rate ≤ hardcap**
- [ ] **Step 3: 6 unit tests (within-bounds, above-max, below-min, delta out-of-range, borrow above cap, sensibility sanity)**
- [ ] **Step 4: Commit `hedgedjlp: M2 — caps`**

### M3: CLI + boot + network gates

**Files:**
- Modify: `crates/hedgedjlp-daemon/src/main.rs`

- [ ] **Step 1: Lift main.rs structure from stable-yield-daemon (clap Args, network gates, genesis-hash check, BEACON loop, NodeService embed) — adapted for hedgedjlp args**
- [ ] **Step 2: Add CLI flags: `--rebalance-interval-secs` (default 600 = 10 min), `--max-position-usdc-lamports`, `--auto-unwind-borrow-ceiling-bps` (default 5000)**
- [ ] **Step 3: Devnet smoke — boot daemon, observe BEACON + listening lines**
- [ ] **Step 4: Negative gates — mainnet without ack, cap above bound, genesis mismatch — all bail correctly**
- [ ] **Step 5: Commit `hedgedjlp: M3 — CLI + boot + gates`**

### M4: Approval queue + dispatch loop

**Files:**
- Create: `crates/hedgedjlp-daemon/src/approval.rs`, `src/dispatch.rs`

- [ ] **Step 1: Lift generic ApprovalQueue<P> from stable-yield (audit-fix C1 sender-match already baked in)**
- [ ] **Step 2: DispatchCtx with three queues: assign / withdraw / rebalance** (rebalance is internally-issued; orchestrator can also force one)
- [ ] **Step 3: dispatch::run handles MsgType::Assign, Approve, Withdraw, plus a new MsgType::Rebalance if needed (or reuse Assign with discriminator — match the M10 stable-yield-daemon choice)**
- [ ] **Step 4: jlp::run_or_simulate + hedge::run_or_simulate are M4 stubs returning ok=true Reports — real ixns land in M6/M8**
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

- [ ] **Step 1: Read JLP custody accounts (one per asset: SOL, ETH, BTC, USDC, USDT). Each has owned_amount + price_oracle. Decode struct documented in Jupiter Perps program (IDL in `julianfssen/jupiter-perps-anchor-idl-parsing`).**
- [ ] **Step 2: pure fn compute_delta(jlp_lamports: u64, custody_state: &[CustodyAccount]) -> PortfolioDelta { sol_lamports, eth_lamports, btc_lamports, total_usd }**
- [ ] **Step 3: Comprehensive unit tests — synthetic compositions with known expected hedge sizes; edge case: 100% USDC (zero delta), 100% SOL (full SOL hedge), realistic mix**
- [ ] **Step 4: Commit `hedgedjlp: M7 — JLP composition + delta math`**

### M8: Implement Jupiter Perps hedge leg (request-execute model)

**Files:**
- Create: `crates/hedgedjlp-daemon/src/hedge.rs`
- Modify: `crates/zerox1-defi-protocols/src/protocols/jupiter_perps.rs` (additive — perp open-request / close-request ixn builders)

The Jupiter Perps execution model differs from any other DEX integration in fleet:

1. The daemon does NOT directly open a position. It submits a *request* account.
2. An off-chain Jupiter keeper observes the request account, performs price discovery, and executes (or rejects) within seconds.
3. The daemon polls the request account → on execution, the position account exists; on rejection/timeout, the request can be closed and funds reclaimed.

This means M8's hedge::run is async-stateful — there is a window between "request submitted" and "position exists" where we hold neither. The daemon cannot assume the position is open just because the create-request tx confirmed.

- [ ] **Step 1: Jupiter Perps perp-market lookup — SOL_PERP, ETH_PERP, BTC_PERP custody pubkeys (each long/short side has its own custody)**
- [ ] **Step 2: hedge::open_short_request(payer, custody, size_usd, collateral_usdc) -> Vec<Instruction> — build `create_increase_position_request` ixn (open-short variant). Repeat for SOL/ETH/BTC sized to neutralize delta.**
- [ ] **Step 3: hedge::close_short_request(payer, position) -> Vec<Instruction> — build `create_decrease_position_request` ixn for full size. Repeat for each open short.**
- [ ] **Step 4: hedge::poll_request_executed(rpc, request_pubkey, timeout_secs) -> Result<ExecutionOutcome> — polls the request account; returns Executed{position_pubkey} | Rejected | TimedOut. Caller decides whether to close-stale-request on TimedOut.**
- [ ] **Step 5: hedge::run_or_simulate replaces M4 stub. For each of 3 perps: build open-request ixn, whitelist verify, submit/sim, then poll until executed (or timeout → emit Escalate(Warning, KeeperLatency)). Report carries hedge_notional_usdc + 3 request tx_signatures.**
- [ ] **Step 6: Devnet smoke — Jupiter Perps does not have a devnet pool, so M8 sim-mode runs against fixture custody data (NodeFixture pattern from multiply M5). Real submit deferred to mainnet M12.**
- [ ] **Step 7: Commit `hedgedjlp: M8 — Jupiter Perps hedge leg (request-execute)`**

### M9: Periodic rebalancer + borrow-rate watch

**Files:**
- Create: `crates/hedgedjlp-daemon/src/rebalance.rs`

- [ ] **Step 1: Async task running every --rebalance-interval-secs**
- [ ] **Step 2: Read JLP custody → compute current delta → compare to target → if |diff| > MAX_DELTA_DRIFT_BPS, build resize requests (close-decrease excess shorts or open-increase additional ones)**
- [ ] **Step 3: Emit Escalate(Notice, RebalanceTriggered) on action; no orchestrator approval required for rebalances WITHIN ±MAX_DELTA_DRIFT_BPS — those are routine. ONLY actions OUTSIDE that band require approval (suggests fundamental composition shift).**
- [ ] **Step 4: Borrow-rate watch (replaces the original funding-rate watch): also check 24h-avg SOL/ETH/BTC perp **borrow rate** on Jupiter Perps; if any exceeds AssignHedgedJlp.max_borrow_rate_bps, emit Escalate(Warning, BorrowRateExceeded) and PAUSE rebalances until manual reset. Direction is `> ceiling` — high borrow rates eat yield, so we trip on spikes UP.**
- [ ] **Step 5: Devnet smoke — synthetic delta drift triggers a rebalance attempt (simulation only). Verify Escalate emitted.**
- [ ] **Step 6: Commit `hedgedjlp: M9 — periodic rebalancer + borrow-rate watch`**

### M10: Telemetry + position reports

**Files:**
- Create: `crates/hedgedjlp-daemon/src/telemetry.rs`

- [ ] **Step 1: Periodic poll (default 60s): read JLP balance + Jupiter Perps position accounts + borrow rates, write JSONL line: `{ts, jlp_value_usd, jlp_yield_apr_bps, hedge_notional_usdc, hedge_borrow_apr_bps, net_apr_bps, current_delta_bps}`**
- [ ] **Step 2: Compute net_apr from JLP yield - hedge borrow cost. Both sides come from Jupiter Perps custody/position state — single source of truth, since JLP yield IS the borrow fees flowing back. (See Structural elegance note above.)**
- [ ] **Step 3: --telemetry-log + --telemetry-interval-secs CLI flags (default `hedgedjlp-pnl.jsonl`, gitignored)**
- [ ] **Step 4: Commit `hedgedjlp: M10 — telemetry`**

### M11: Withdrawal path (request-execute unwind)

**Files:**
- Modify: `crates/hedgedjlp-daemon/src/{dispatch.rs,jlp.rs,hedge.rs}`

The unwind sequence inverts the M8 flow and inherits its 2-tx asynchrony:

- [ ] **Step 1: WithdrawHedgedJlp dispatch arm — symmetric to AssignHedgedJlp**
- [ ] **Step 2: Unwind sequence:**
  1. for each of 3 open shorts: submit `create_decrease_position_request` (full size) → poll until keeper executes,
  2. once all 3 perps closed and collateral returned, swap JLP → USDC via Jupiter,
  3. emit ReportHedgedJlpWithdraw with usdc_returned_lamports + tx_signatures.
- [ ] **Step 3: Timeout handling — if any close-request times out, the daemon does NOT proceed to the JLP swap (would leave a delta exposure). Instead it emits Escalate(Warning, UnwindStalled) with the stuck request account, and surfaces the situation for manual intervention.**
- [ ] **Step 4: Devnet smoke — withdraw subcommand on stub, daemon sim'd unwind**
- [ ] **Step 5: Commit `hedgedjlp: M11 — withdrawal path`**

### M12: Mainnet runbook

**Files:**
- Create: `docs/runbooks/hedgedjlp-mainnet-tiny.md`

- [ ] **Step 1: $200 USDC test (smaller than stable-yield because hedgedjlp has fixed costs from Jupiter swap + Jupiter Perps margin)**
- [ ] **Step 2: Document expected: ~15-25% net APR on $200 = $0.08-0.14/day. Failure modes: borrow-rate spikes, JLP utilization caps, Jupiter Perps program pause. Add a Jupiter-keeper-latency note: position requests typically execute within 1-3 slots, but a stuck keeper can leave the daemon partially hedged for tens of seconds — operators should expect to see "request submitted, awaiting executor" log lines and not panic.**
- [ ] **Step 3: Two-phase: --simulate-only first, then real submit**
- [ ] **Step 4: 24h watch instructions emphasizing borrow-rate vigilance (was funding-rate)**
- [ ] **Step 5: Commit `hedgedjlp: M12 — mainnet runbook`**

---

## Verification gates

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (target: existing + ~25 new from caps + delta + approval + jlp + hedge tests)
- [ ] Devnet smoke: full round-trip in sim mode (Assign → both legs sim → Report)
- [ ] Devnet smoke: rebalance task fires within configured interval
- [ ] Mainnet runbook reviewed by operator before $200 test

## Self-review notes

- HedgedJLP is the most complex daemon — two legs, periodic rebalancer, two protocol integrations (Jupiter swap + Jupiter Perps). Estimate: 10-15 days at the per-task review pace, vs. stable-yield's 2-3 days.
- The Jupiter-Perps integration is the highest-risk dependency. v0 picks between the third-party CPI client (`Garrett-Weber/jupiter-perpetuals-cpi`) and raw ixn-from-IDL building. Both work; the CPI client is faster to ship, raw building is faster to audit.
- The borrow-rate watch (M9) is a soft kill switch. A truly hostile borrow spike (50%+ APR) would erase JLP yield; better to unwind and lose principal-flat than bleed borrow.
- The 2-tx execution model (request → keeper → position) is the most novel piece versus other fleet daemons. It is encapsulated in `hedge.rs::poll_request_executed` and propagated as `Escalate(Warning, KeeperLatency)` envelopes when latency exceeds expectations. The rest of the daemon reasons in terms of "open positions" — same as stable-yield reasons in terms of "supplied amount".
- Riskwatcher (parallel agent) will observe hedgedjlp's Reports + Escalates the same way it observes multiply's. No riskwatcher change required for this daemon.
- Net APR depends entirely on JLP yield - borrow cost. Both fluctuate; the daemon does not "earn a fixed rate" — it tracks a spread. The structural elegance note above is what makes this spread tractable: both legs sit on the same fee economy, so the daemon's P&L collapses to a single Jupiter-Perps utilization quantity.
