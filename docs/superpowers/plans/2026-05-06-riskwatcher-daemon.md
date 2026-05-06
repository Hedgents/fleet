# Riskwatcher Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `riskwatcher-daemon` — an independent libp2p peer that observes multiply-daemon's positions, polls Kamino obligation state via read-only RPC, and emits `EscalateRisk` envelopes when liquidation distance breaches thresholds. Critical-severity escalations route to the orchestrator AND back to the multiply-daemon as a soft-veto signal.

**Architecture:** Standalone crate `crates/riskwatcher-daemon/` mirroring the multiply-daemon scaffold (NodeService embed, role-keyed identity, CBOR envelopes). Riskwatcher does NOT hold a Solana wallet — it has read-only RPC access only, enforced structurally by not depending on `zerox1-defi-wallet` in Cargo.toml. The role-key is its only signing material, used solely to sign envelopes. State: in-memory `HashMap<position_subject, PositionView>` populated from observed `ReportMultiply` envelopes + periodic Kamino obligation polls.

**Tech Stack:** Rust, libp2p 0.54 (via `zerox1-node-enterprise`), `zerox1-defi-runtime` (RpcContext, RoleIdentity), `zerox1-defi-protocols::protocols::kamino_loader` (read-only obligation decoding), `ciborium`, `tokio`, `tracing`.

---

## File Structure

```
crates/riskwatcher-daemon/
├── Cargo.toml                 — NO zerox1-defi-wallet dep (structural read-only enforcement)
├── src/
│   ├── main.rs                — CLI args, boot, NodeService embed, async run loop
│   ├── observer.rs            — Inbox dispatch: subscribes to ReportMultiply envelopes
│   ├── poller.rs              — Periodic Kamino obligation reload (every N seconds)
│   ├── state.rs               — PositionView struct + ObservedPositions registry
│   ├── thresholds.rs          — Distance bands (Notice / Warning / Critical) — compile-time
│   ├── escalate.rs            — Build + sign EscalateRisk envelopes
│   └── lib.rs                 — re-exports for tests
└── tests/
    ├── thresholds_test.rs
    └── state_test.rs
```

**Read-only zone enforcement:** `Cargo.toml` MUST NOT include `zerox1-defi-wallet` as a dep. CI `cargo tree -p riskwatcher-daemon | grep wallet` should return nothing. This is the structural authority boundary: a riskwatcher binary literally cannot link signing code, so even a compromised binary cannot move funds.

---

## Milestones

### M1: Crate scaffold + role identity + NodeService embed

**Files:**
- Create: `crates/riskwatcher-daemon/Cargo.toml`
- Create: `crates/riskwatcher-daemon/src/main.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Add crate to workspace, define deps (NO wallet)**
- [ ] **Step 2: CLI args — `--secrets-dir`, `--rpc-url`, `--listen`, `--bootstrap`, `--network` (devnet/mainnet)**
- [ ] **Step 3: Boot path — load `riskwatcher-role.key`, build RoleIdentity, embed NodeService, start listening**
- [ ] **Step 4: BEACON-emit loop with shared nonce counter (lift pattern from multiply)**
- [ ] **Step 5: Verify `cargo tree -p riskwatcher-daemon | grep wallet` returns empty**
- [ ] **Step 6: Smoke — boot two peers (riskwatcher + fleet-pm-stub), confirm BEACONs delivered both directions**
- [ ] **Step 7: Commit `riskwatcher: M1 — crate scaffold, role identity, NodeService embed`**

### M2: PositionView + ObservedPositions registry

**Files:**
- Create: `crates/riskwatcher-daemon/src/state.rs`
- Create: `crates/riskwatcher-daemon/tests/state_test.rs`

- [ ] **Step 1: `PositionView { subject: [u8; 32], obligation_pubkey: Pubkey, last_ltv_bps: u16, last_seen_unix: u64, source: Source::Report | Source::Poll }`**
- [ ] **Step 2: `ObservedPositions { inner: Mutex<HashMap<[u8; 32], PositionView>> }` — add/update/get/list**
- [ ] **Step 3: 32-entry cap; LRU-evict on overflow**
- [ ] **Step 4: Unit tests — insert, update preserves earliest seen, eviction works, concurrent insert safe**
- [ ] **Step 5: Commit `riskwatcher: M2 — observed-positions registry`**

### M3: Inbox observer — subscribe to ReportMultiply

**Files:**
- Create: `crates/riskwatcher-daemon/src/observer.rs`
- Modify: `crates/riskwatcher-daemon/src/main.rs` (wire observer task)

- [ ] **Step 1: `pub async fn run(handle: NodeHandle, state: Arc<ObservedPositions>) -> Result<()>` loop**
- [ ] **Step 2: Match `MsgType::Report` envelopes, decode CBOR as `ReportMultiply`**
- [ ] **Step 3: On `header.ok=true && resulting_ltv_bps > 0`, upsert PositionView (subject = `env.sender`)**
- [ ] **Step 4: On `header.ok=true && resulting_ltv_bps == 0` (queued ack), ignore**
- [ ] **Step 5: Log all observed Reports at INFO; ignore other msg_types at TRACE**
- [ ] **Step 6: Devnet smoke — boot multiply + riskwatcher + fleet-pm-stub; trigger AssignMultiply; confirm riskwatcher logs the resulting Report and registry contains a PositionView**
- [ ] **Step 7: Commit `riskwatcher: M3 — inbox Report observer`**

### M4: Kamino obligation poller

**Files:**
- Create: `crates/riskwatcher-daemon/src/poller.rs`

- [ ] **Step 1: `pub async fn run(rpc: Arc<RpcContext>, state: Arc<ObservedPositions>, interval: Duration)`**
- [ ] **Step 2: Every `interval`, iterate ObservedPositions snapshot; for each, call `kamino_loader::fetch_obligation` + `decode_obligation`**
- [ ] **Step 3: Compute current LTV via `query_position_ltv_bps`; update PositionView with `Source::Poll`**
- [ ] **Step 4: On RPC failure, log warn and skip — never panic**
- [ ] **Step 5: Default poll interval: 30s. CLI flag `--poll-interval-secs`**
- [ ] **Step 6: Devnet smoke — boot daemon with empty registry, verify zero RPC calls. Manually inject a PositionView, verify next tick polls Kamino and updates LTV**
- [ ] **Step 7: Commit `riskwatcher: M4 — periodic Kamino obligation poller`**

### M5: Liquidation-distance thresholds

**Files:**
- Create: `crates/riskwatcher-daemon/src/thresholds.rs`
- Create: `crates/riskwatcher-daemon/tests/thresholds_test.rs`

- [ ] **Step 1: Compile-time constants — lift from multiply caps where applicable, add three bands:**
  ```
  pub const DISTANCE_NOTICE_BPS: u16 = 500;     // 5% headroom — info only
  pub const DISTANCE_WARNING_BPS: u16 = 200;    // 2% headroom — escalate
  pub const DISTANCE_CRITICAL_BPS: u16 = 50;    // 0.5% headroom — soft-veto
  ```
- [ ] **Step 2: `pub fn classify(view: &PositionView, decoded: &DecodedObligation) -> Option<RiskSeverity>`**
- [ ] **Step 3: Distance = `(unhealthy_borrow_value_sf - borrowed_assets_market_value_sf) / unhealthy_borrow_value_sf` in bps**
- [ ] **Step 4: Unit tests — comfortable position returns None; warning band returns Warning; critical band returns Critical**
- [ ] **Step 5: Commit `riskwatcher: M5 — liquidation-distance thresholds`**

### M6: Escalate emission

**Files:**
- Create: `crates/riskwatcher-daemon/src/escalate.rs`
- Modify: `src/poller.rs` (call escalate after classify)

- [ ] **Step 1: `pub async fn emit(handle: &NodeHandle, role: &RoleIdentity, nonce: &AtomicU64, recipient: [u8; 32], severity: RiskSeverity, kind: RiskKind, subject: [u8; 32], measurement: u64) -> Result<()>`**
- [ ] **Step 2: Build `EscalateRisk` payload (CBOR), wrap in `Envelope` with `MsgType::Escalate`, sign with role key**
- [ ] **Step 3: Use shared nonce counter (lift pattern from multiply)**
- [ ] **Step 4: De-dup — only emit if `(subject, severity)` is new or last emission > 60s ago**
- [ ] **Step 5: Critical severity: emit to BOTH orchestrator pubkey (CLI arg `--orchestrator`) AND the position subject (multiply-daemon)**
- [ ] **Step 6: Devnet smoke — manually mutate ObservedPositions to a critical-band LTV, verify riskwatcher emits Escalate envelopes to both targets**
- [ ] **Step 7: Commit `riskwatcher: M6 — Escalate emission with de-dup`**

### M7: Soft-veto protocol — multiply respects Critical Escalate

**Files:**
- Modify: `crates/multiply-daemon/src/dispatch.rs`

- [ ] **Step 1: Add `MsgType::Escalate` arm to multiply's inbox dispatch**
- [ ] **Step 2: Decode as `EscalateRisk`; if `severity == Critical && kind == LiquidationImminent`, set `paused_until_unix = now + 300` in DispatchCtx (Mutex<Option<u64>>)**
- [ ] **Step 3: Verify Escalate sender is the configured riskwatcher pubkey (CLI arg `--riskwatcher` on multiply); reject otherwise**
- [ ] **Step 4: When paused, multiply rejects new AssignMultiply with error_code=4 ("paused by riskwatcher veto")**
- [ ] **Step 5: Pause auto-clears when `now >= paused_until_unix`**
- [ ] **Step 6: Devnet smoke — riskwatcher emits Critical, multiply receives + flips to paused; subsequent Assign returns error_code=4**
- [ ] **Step 7: Commit `riskwatcher: M7 — soft-veto protocol (multiply respects Critical Escalate)`**

### M8: End-to-end devnet round-trip

**Files:**
- Create: `docs/runbooks/riskwatcher-devnet.md`

- [ ] **Step 1: Boot multiply + riskwatcher + fleet-pm-stub on three localhost peers**
- [ ] **Step 2: AssignMultiply → multiply executes (devnet sim) → Report → riskwatcher observes → registry populated**
- [ ] **Step 3: Synthetic poller fixture (or devnet position) drives Critical band**
- [ ] **Step 4: riskwatcher emits Escalate → orchestrator receives Escalate, multiply pauses**
- [ ] **Step 5: Document the end-to-end sequence in the runbook**
- [ ] **Step 6: Commit `riskwatcher: M8 — devnet round-trip + runbook`**

### M9: Operational telemetry + structured logs

**Files:**
- Modify: `crates/riskwatcher-daemon/src/main.rs`
- Create: `crates/riskwatcher-daemon/src/telemetry.rs`

- [ ] **Step 1: Per-position JSONL log line on every poll: `{ ts, subject, ltv_bps, distance_bps, classification }`**
- [ ] **Step 2: Output path: `--telemetry-log` CLI flag, default `riskwatcher-pnl.jsonl` (gitignored)**
- [ ] **Step 3: Counter — total Escalates emitted by severity, exposed at `127.0.0.1:9091/metrics` (prometheus text format)**
- [ ] **Step 4: Commit `riskwatcher: M9 — telemetry log + metrics endpoint`**

### M10: Mainnet bring-up runbook

**Files:**
- Create: `docs/runbooks/riskwatcher-mainnet.md`

- [ ] **Step 1: Document mainnet config — RPC endpoint, role key generation, listen address**
- [ ] **Step 2: Document deployment co-location: riskwatcher should run on a DIFFERENT machine than multiply (independence)**
- [ ] **Step 3: Document the 24h watch protocol — what to look for in telemetry, how to verify Escalate de-dup is working**
- [ ] **Step 4: Document teardown — graceful shutdown, role-key rotation procedure**
- [ ] **Step 5: Commit `riskwatcher: M10 — mainnet runbook`**

---

## Verification gates

After all 10 milestones:
- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (target: existing 202 + ~15 new tests)
- [ ] `cargo tree -p riskwatcher-daemon | grep wallet` returns empty (read-only authority enforced)
- [ ] Devnet round-trip from M8 reproducible from runbook
- [ ] Critical Escalate de-dup window verified (no spam)
- [ ] Multiply-daemon respects pause from riskwatcher

## Self-review notes

- Riskwatcher pause is **soft-veto** (multiply must opt-in by configuring `--riskwatcher` pubkey). A truly hostile multiply could ignore it. That's acceptable for v0 — hard-veto requires on-chain coordination which is post-v1.
- Riskwatcher has no chain-write authority. Even compromised, blast radius = "spam Escalates" + "fail to detect liquidation" — neither moves funds.
- The 30s poll interval is a tradeoff: faster catches liquidations earlier but burns more RPC. 30s is ~10× per critical-band threshold transit on mainnet volatility.
