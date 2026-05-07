# Researcher Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `researcher-daemon` — fleet's read-only signal publisher. Watches market state (lending rates, perp funding, prices, JLP, stable peg) via direct RPC reads and publishes `MarketSignal` envelopes when thresholds cross. Other daemons (multiply, hedgedjlp, speculator) subscribe and use signals to decide when to enter/exit. No chain authority — pure observe-and-broadcast.

**Architecture:** Standalone crate `crates/researcher-daemon/`. Like riskwatcher-daemon, **structurally read-only**: Cargo.toml MUST NOT depend on `zerox1-defi-wallet`. Five independent watchers (one per signal source) run as parallel async tasks; each emits typed `MarketSignal` envelopes via the mesh. Throttled de-dup prevents signal spam. Subscribers use signals as inputs to their own strategy logic — researcher does not command, only informs.

**Tech Stack:** Rust, libp2p 0.54, `zerox1-defi-runtime` (RpcContext for read-only RPC), `zerox1-defi-protocols::protocols::{kamino_loader, drift, pyth, jupiter}` (read-only helpers from each), `ciborium`, `tokio`, `tracing`. NO `zerox1-defi-wallet`.

---

## File Structure

```
crates/researcher-daemon/
├── Cargo.toml                      — NO zerox1-defi-wallet dep
├── src/
│   ├── main.rs                     — CLI args, boot, NodeService embed, spawn watchers
│   ├── signal.rs                   — MarketSignal envelope build + sign + send
│   ├── thresholds.rs               — compile-time constants per signal type
│   ├── dedup.rs                    — per-(signal_kind, asset) emission throttle
│   ├── watchers/
│   │   ├── mod.rs
│   │   ├── lending_rate.rs         — Kamino reserves: borrow + supply APR
│   │   ├── perp_funding.rs         — Drift: SOL/ETH/BTC perp funding rates
│   │   ├── price.rs                — Pyth oracle: SOL/ETH/BTC spot
│   │   ├── jlp_yield.rs            — Jupiter Perps: 7d JLP yield + composition shifts
│   │   └── stable_peg.rs           — USDC/USDT depeg detection
│   ├── telemetry.rs                — JSONL log of all emitted signals
│   └── lib.rs
└── tests/
    ├── thresholds_test.rs
    └── dedup_test.rs
```

**Protocol additions:**

```rust
// crates/zerox1-protocol/src/fleet/researcher.rs (likely already exists per memory)
pub struct MarketSignal {
    pub kind: SignalKind,
    pub asset: AssetId,        // SOL / ETH / BTC / USDC / JLP / etc.
    pub measurement_bps: i32,  // -bps to +bps; meaning depends on kind
    pub severity: SignalSeverity,  // Info / Notice / Important
    pub raised_at_unix: u64,
}
pub enum SignalKind {
    LendingBorrowRateAbove,
    LendingSupplyRateAbove,
    PerpFundingAbove,
    PerpFundingBelow,
    PriceMovedBps,            // 1h move
    JlpYieldChanged,
    JlpCompositionShifted,
    StableDepegBps,
}
pub enum AssetId { SOL, ETH, BTC, USDC, USDT, JLP, Other([u8; 32]) }
pub enum SignalSeverity { Info, Notice, Important }
```

**Read-only zone enforcement:** `Cargo.toml` MUST NOT include `zerox1-defi-wallet`. CI check: `cargo tree -p researcher-daemon | grep wallet` returns nothing. Same structural authority pattern as riskwatcher.

---

## Milestones

### M1: Crate scaffold + role identity (read-only structurally)

**Files:**
- Create: `crates/researcher-daemon/Cargo.toml`, `src/main.rs` (stub), `src/lib.rs`
- Modify: `crates/zerox1-protocol/src/fleet/researcher.rs`, `fleet/mod.rs`, root `Cargo.toml`

- [ ] **Step 1: Define MarketSignal + SignalKind + AssetId + SignalSeverity in protocol crate**
- [ ] **Step 2: Add researcher-daemon to workspace members**
- [ ] **Step 3: Cargo.toml — copy riskwatcher's deps; explicitly omit `zerox1-defi-wallet`. Include `zerox1-defi-runtime` (for read-only RpcContext) + `zerox1-defi-protocols`**
- [ ] **Step 4: Stub main.rs — CLI args (--secrets-dir, --rpc-url, --listen, --network, --bootstrap), load role key, NodeService embed, BEACON loop**
- [ ] **Step 5: Verify `cargo tree -p researcher-daemon | grep wallet` returns empty**
- [ ] **Step 6: Devnet smoke — boot, BEACON, exit cleanly**
- [ ] **Step 7: Commit `researcher: M1 — crate scaffold + structurally read-only`**

### M2: Signal envelope build + emit infrastructure

**Files:**
- Create: `crates/researcher-daemon/src/signal.rs`
- Create: `crates/researcher-daemon/src/dedup.rs`
- Create: `crates/researcher-daemon/src/thresholds.rs`

- [ ] **Step 1: signal::emit(handle, role, nonce, recipient, signal: MarketSignal) -> Result<()> — CBOR encode, wrap in Envelope with MsgType::MarketSignal (new variant in protocol crate, 0x0_ infrastructure class — these are broadcasts), sign, send**
- [ ] **Step 2: dedup::EmissionTracker — HashMap<(SignalKind, AssetId), Instant>; throttle: same (kind, asset) only re-emits if last emission > 60s OR severity escalated**
- [ ] **Step 3: thresholds.rs — compile-time bands. Examples:**
  ```
  pub const LENDING_RATE_INFO_DELTA_BPS: i16 = 50;       // 0.5% change → Info
  pub const LENDING_RATE_NOTICE_DELTA_BPS: i16 = 200;    // 2% change → Notice
  pub const FUNDING_RATE_INFO_THRESHOLD_BPS: i16 = 500;  // 5% APR funding → Info
  pub const FUNDING_RATE_NOTICE_THRESHOLD_BPS: i16 = 2000; // 20% APR → Notice
  pub const PRICE_1H_NOTICE_DELTA_BPS: i16 = 200;        // 2% 1h move → Notice
  pub const STABLE_DEPEG_NOTICE_BPS: i16 = 30;           // 0.3% off-peg → Notice
  pub const STABLE_DEPEG_IMPORTANT_BPS: i16 = 100;       // 1% off-peg → Important
  ```
- [ ] **Step 4: Unit tests for EmissionTracker (throttle, severity escalation override, multi-asset isolation)**
- [ ] **Step 5: Commit `researcher: M2 — signal emission + de-dup + thresholds`**

### M3: Lending rate watcher

**Files:**
- Create: `crates/researcher-daemon/src/watchers/mod.rs`, `src/watchers/lending_rate.rs`
- Modify: `crates/researcher-daemon/src/main.rs` (spawn watcher task)

- [ ] **Step 1: lending_rate::run(rpc, signal_tx, interval) — poll Kamino main reserves (USDC, SOL, JLP) every interval; compute supply + borrow APR**
- [ ] **Step 2: Track per-reserve last-known APR; emit signal when delta crosses thresholds**
- [ ] **Step 3: CLI flag `--lending-poll-interval-secs` (default 60)**
- [ ] **Step 4: Devnet smoke — boot daemon, observe periodic lending_rate poll attempts (will fail on devnet without USDC reserve — acceptable, log warn)**
- [ ] **Step 5: Commit `researcher: M3 — Kamino lending rate watcher`**

### M4: Perp funding watcher

**Files:**
- Create: `crates/researcher-daemon/src/watchers/perp_funding.rs`

- [ ] **Step 1: perp_funding::run reads Drift PerpMarket accounts for SOL_PERP, ETH_PERP, BTC_PERP every interval**
- [ ] **Step 2: Compute current funding rate (lifted from speculator-daemon's M6 helper if it exists; otherwise inline-derive)**
- [ ] **Step 3: Emit signal when funding crosses Info or Notice thresholds, OR flips sign (positive ↔ negative)**
- [ ] **Step 4: Devnet smoke — Drift devnet has SOL_PERP; verify a real read returns a number, signal emits when threshold crossed**
- [ ] **Step 5: Commit `researcher: M4 — Drift perp funding watcher`**

### M5: Price watcher (Pyth)

**Files:**
- Create: `crates/researcher-daemon/src/watchers/price.rs`

- [ ] **Step 1: price::run reads Pyth oracle accounts for SOL/USD, ETH/USD, BTC/USD every interval**
- [ ] **Step 2: Maintain rolling 1h price-buffer per asset (every poll appends, prunes >1h-old)**
- [ ] **Step 3: Emit signal when 1h % change exceeds Notice threshold, or 24h trend reverses**
- [ ] **Step 4: Devnet smoke — Pyth devnet feeds exist; verify a real price returned, ring buffer populates**
- [ ] **Step 5: Commit `researcher: M5 — Pyth price watcher`**

### M6: Stable peg watcher

**Files:**
- Create: `crates/researcher-daemon/src/watchers/stable_peg.rs`

- [ ] **Step 1: stable_peg::run reads USDC/USD and USDT/USD oracles every interval**
- [ ] **Step 2: Emit signal when |peg deviation| > thresholds (Notice 30bps, Important 100bps)**
- [ ] **Step 3: Important depeg signal is a critical fleet-wide event — multiply should pause leverage, stable-yield should consider withdraw, etc. (these reactions live in the consumer daemons; researcher only signals)**
- [ ] **Step 4: Devnet smoke — synthetic depeg via mocked oracle response**
- [ ] **Step 5: Commit `researcher: M6 — stablecoin peg watcher`**

### M7: JLP yield + composition watcher

**Files:**
- Create: `crates/researcher-daemon/src/watchers/jlp_yield.rs`

- [ ] **Step 1: Read Jupiter Perps custody state every interval; extract JLP supply, custody allocations, 7d yield**
- [ ] **Step 2: Emit JlpYieldChanged when 7d yield delta exceeds threshold; emit JlpCompositionShifted when any custody's % allocation moves >5%**
- [ ] **Step 3: hedgedjlp-daemon will subscribe to these signals**
- [ ] **Step 4: Devnet smoke — JLP doesn't exist on devnet; daemon should log warn and skip (don't fail the whole researcher)**
- [ ] **Step 5: Commit `researcher: M7 — JLP yield + composition watcher`**

M8: Removed — Bags.fm watcher was a category error; fleet doesn't trade memecoins. See cleanup commit.

### M9: Telemetry + signal aggregation log

**Files:**
- Create: `crates/researcher-daemon/src/telemetry.rs`

- [ ] **Step 1: Every emitted signal also writes a JSONL line: `{ts, kind, asset, measurement_bps, severity, recipient_count}`**
- [ ] **Step 2: --telemetry-log + --telemetry-interval-secs CLI flags (default `researcher-signals.jsonl`, gitignored)**
- [ ] **Step 3: 24h running-tally summary every hour (info-level log line): "researcher: emitted N signals across last hour, M of severity Notice+"**
- [ ] **Step 4: Commit `researcher: M9 — signal telemetry log`**

### M10: End-to-end devnet round-trip + runbook

**Files:**
- Create: `docs/runbooks/researcher-devnet.md`

- [ ] **Step 1: Boot researcher + multiply (or any consumer) on two localhost peers**
- [ ] **Step 2: Inject a synthetic threshold breach (mock an APR delta in lending_rate.rs's input or via test fixture); verify the signal envelope arrives at the consumer**
- [ ] **Step 3: Verify de-dup: same threshold breach 3× within 60s should emit ONCE**
- [ ] **Step 4: Verify severity escalation: a Notice signal followed by an Important signal on same (kind, asset) WITHIN dedup window should still emit (severity overrides throttle)**
- [ ] **Step 5: Document the end-to-end flow in the runbook + how to add a new watcher**
- [ ] **Step 6: Mainnet bring-up section in runbook (researcher works the same on mainnet — only difference is which oracles/programs respond)**
- [ ] **Step 7: Commit `researcher: M10 — devnet round-trip + runbook`**

---

## Verification gates

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (target: pre-existing + ~12 new from thresholds + dedup + per-watcher tests)
- [ ] `cargo tree -p researcher-daemon | grep wallet` returns empty (read-only authority enforced structurally)
- [ ] Devnet round-trip: synthetic threshold → signal emitted → consumer logged receipt
- [ ] De-dup verified (3× breach → 1 emission)
- [ ] Severity escalation overrides throttle

## Self-review notes

- Researcher is the simplest of the three remaining daemons — no chain writes, no approval gates, no caps for tx safety (just throttle/severity caps for emission).
- The watchers are mostly independent; they can ship in any order. Easiest path: M1+M2 (infra) → M5 (Pyth — most reliable feeds) → M3 (Kamino) → M4 (Drift) → M6/M7/M8 (subset depending on what consumers need first).
- Researcher is high-leverage low-risk: shipping it lets every other daemon make better decisions. But it's also the lowest *visible* impact — without consumers reacting to signals, it's just a logger.
- v0 consumers (which daemons listen to which signals):
  - **multiply**: LendingBorrowRateAbove (USDC/SOL — high borrow rate = unwind), StableDepegBps (Important = pause)
  - **hedgedjlp**: JlpYieldChanged, JlpCompositionShifted, PerpFundingAbove (>50% on SOL/ETH/BTC = unwind hedge)
  - **speculator**: PerpFundingAbove (entry trigger), PerpFundingBelow (unwind trigger), PriceMovedBps
  - **stable-yield**: LendingSupplyRateAbove (notify orchestrator of yield improvements), StableDepegBps (pause)
- v0 wires the broadcast plumbing; consumer daemons grow signal-handling logic over time. Each consumer-side wire-up is a follow-up commit on the consumer crate, NOT part of this plan.
- The structural read-only enforcement is non-negotiable. A compromised researcher should be able to spam misleading signals (annoying) but NEVER move funds (catastrophic). The Cargo.toml dep absence enforces this at compile time.
