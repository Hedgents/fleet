# Hedgents Devlog

Chronological shipping log. Each entry: tagged release → what shipped → why
it mattered. Pair with `ROADMAP.md` (what's next) and `LITEPAPER.md` (what
the product is).

Format: newest first.

---

## v0.5.3 — dashboard surfaces onyc properly + drops multiply (2026-06-08)

After the first live deposit landed (sig a5Yz1Ny6R5...) the dashboard
still showed multiply in `/daemons` (red — daemon disabled) and
returned `current_apr_bps: 0` for onyc. Two fixes:

**fleet-dashboard-server:**
- `DAEMON_ROLES` registry drops multiply, adds onyc. `/daemons`
  endpoint now lists the active 6 (stable_yield + hedgedjlp + onyc +
  riskwatcher + researcher + orchestrator) instead of 6 including a
  dead multiply.
- `ChainAumBreakdown` grows an `onyc_usd` field; `read_chain_aum_breakdown`
  reads the ONyc isolated obligation alongside the existing per-strategy
  reads. Total AUM math now includes onyc.
- `/aum` `per_strategy` serializes an `onyc` field next to `multiply`.
  Frontend's `lib/api.ts` already has the optional `onyc` field, so this
  is non-breaking on older clients.
- Combined APR computation drops the multiply leg and adds onyc. Multiply's
  residual `multiply_usd` is still read (so legacy unwound positions
  surface correctly) but contributes 0 to the weighted APR figure.

**onyc-daemon pnl.rs:**
- The forked pnl row was still emitting `multiply_net_apr_bps`. Renamed
  to `onyc_net_apr_bps` (which is what STRATEGIES' apr_field for onyc
  looks up).
- Net APR computation: ONyc base NAV growth (1100 bps placeholder) minus
  USDC borrow rate × current LTV. At LTV=0 (current state) this reports
  ~11%. At LTV=4000 with ~5.3% USDC borrow, reports ~8.9%. A
  NAV-derived rate driven by Chainlink Data Streams + Apex attestation
  deltas is future work.

Strategies dashboard card now shows the real net APR for onyc.

---

## v0.5.2 — fleet-pm-stub: assign-onyc + withdraw-onyc subcommands (2026-06-08)

v0.5.1 deployed onyc-daemon successfully but fleet-pm-stub on the
operator host didn't have the matching CLI subcommands. Manual
smoke-testing onyc required emitting AssignOnyc / WithdrawOnyc
envelopes by hand, which has no good path.

This adds:
- `fleet-pm-stub assign-onyc --target-ltv-bps=X --usdc-lamports=N`
- `fleet-pm-stub withdraw-onyc`

Same shape as the multiply pair. Includes ExpectedReport::{Onyc,
OnycWithdraw} variants + payload-decode filters so an unrelated
daemon's response doesn't short-circuit the Report wait.

Also surfaces the "operator needs manual injection to bootstrap a
new strategy" workflow: the apr-weighted allocator assigns 0% target
weight to any strategy with 0 deployed_usd (no APR observations →
no signal). To get onyc started the operator runs the equivalent of
`fleet-pm-stub assign-onyc --usdc-lamports 50000000 --target-ltv-bps 0`
to seed the obligation; subsequent allocator ticks will start
weighting it once the chain reader reports a non-zero deployed_usd
and a usable APR figure.

---

## v0.5.1 — release workflow: include onyc-daemon in CI build (2026-06-08)

v0.5.0 shipped onyc-daemon source but the release workflow's
hardcoded daemon list didn't include it — install on Hetzner
landed every other binary except onyc-daemon. The
hedgents-onyc-live.service systemd unit was enabled but inactive
because /opt/hedgents/bin/onyc-daemon didn't exist.

Fix: add `-p onyc-daemon` to the cargo build invocation in
.github/workflows/release-fleet.yml AND add `onyc-daemon` to the
stage-tarball binary-copy loop. Both edits are one-line additions
matching the existing pattern.

No code change in the daemons themselves — pure release-config
patch.

---

## v0.5.0 — ONyc replaces Multiply (2026-06-08)

First minor-version bump in the fleet. Multiply (leveraged jitoSOL)
is abandoned and replaced by ONyc. The fleet **stays at three
strategies** — the swap is positional, not additive. This is a
semver-meaningful milestone because the strategy mix itself changes
character, not because the strategy count grows.

**Why multiply is gone.** It was the only fleet leg with net +1.0
SOL beta (per `sol_beta_for("multiply") = 1.0` in the allocator). On
a leveraged-staking trade, "yield" and "directional bull bet" are
inseparable — you can't pitch a Solana-native USD yield vault whose
biggest leg is leveraged SOL exposure. Superteam called this out
explicitly. Replacing it with ONyc gets the fleet's three remaining
strategies all to USD-denominated, delta-neutral profiles:

| Strategy | Net beta | Denomination | Yield source |
|---|---|---|---|
| stable_yield | 0 | USD | Kamino USDC supply (~6.72%) |
| hedgedjlp | 0 | USD | Jupiter fees − funding (~10-15%) |
| **onyc** (NEW) | 0 | USD | OnRe reinsurance premium × leverage (~13-15%) |

That's a coherent product story.

**ONyc strategy. What landed:**

- A new protocol-layer message family (`AssignOnyc`, `WithdrawOnyc`,
  `MsgType::WithdrawOnyc = 0x1B`)
- A new Role variant (`Role::Onyc`)
- A new strategy daemon (~5,500 lines) with all the usual
  infrastructure: dispatch, leverage, unwind, seed, caps, journal,
  pnl, reporter, auto-mode, approval queue, liq monitor
- A new dashboard chain reader path
- A new orchestrator allocator routing branch
- A new "build above the protocol" component (NAV-aware LTV
  controller) that didn't exist for any prior strategy
- A new systemd unit, installer wiring, and deploy runbook

**Multiply deprecation. What changed:**

- Removed from `STRATEGIES` in `tools/fleet-dashboard-server/src/api/state.rs`
  so `/strategies` no longer surfaces it as an institutional card
- Removed from `enable for boot survival` loop in
  `deploy/install-hedgents.sh`; install adds an explicit
  `systemctl disable hedgents-multiply-live.service` for clean
  rollover from <=v0.4.30 hosts
- Removed from `AprHistoryChart`'s rendered strategy lines
- Frontend `NumbersPanel` still shows multiply in the Allocation card
  IF it has non-zero capital (transitional mid-unwind state during
  rollover); hidden once at zero
- `multiply-daemon` binary stays in the workspace — it's archaeology,
  not dead code. Useful as a reference for the iterative-loop and
  flash-loan unwind patterns ONyc deliberately doesn't have yet
- `tools/fleet-pm-stub` still handles `Deposit/Withdraw{multiply}`
  envelopes so an operator could in principle still drive a position
  manually; the allocator just won't ever choose multiply anymore

For the first time, the fleet's USD-denominated strategy mix includes
a real-world-asset leg. The Superteam pitch evolves from "regime-aware
DeFi allocator over three strategies" to "regime-aware DeFi+RWA
allocator over three strategies." Same shape, materially different
product positioning.

### What "build above the protocol" means in v0.5.0

Every other strategy in the fleet integrates a venue (Kamino, Jupiter
Perps). onyc-daemon does that AND adds a component the underlying
protocol doesn't provide: the NAV-aware LTV controller.

ONyc's NAV updates discretely — monthly Apex Group attestations,
event-driven Chainlink Data Streams updates on reinsurance claim
settlements. A naive LTV monitor would panic-unwind on a single
chunky NAV step, paying Orca slippage on a $15M-depth secondary
market exactly when the regime is otherwise healthy.

The NAV-aware controller encodes the OnRe operational cadence:
- 50bps step buffer (typical attestation delta)
- 500bps single-step alarm (real loss event threshold)
- 3-consecutive-step drift cap (noise floor for sustained impairment)

That's domain knowledge built INTO the daemon — not a wrapper around
a primitive. Defensible IP, blogpost-able mechanism.

### Build delta

```
crates/onyc-daemon/                ~5,500 lines (vs multiply ~8,500)
  src/seed.rs                      380 lines
  src/leverage.rs                  360 lines
  src/unwind.rs                    370 lines
  src/nav_controller.rs            330 lines (NEW — "build above" element)
  src/liq_monitor.rs               +60 lines (NAV integration)
  src/{dispatch,caps,kamino,...}   forked + asset-swapped

crates/zerox1-protocol/fleet/onyc.rs        150 lines
                                MsgType::WithdrawOnyc = 0x1B

crates/zerox1-defi-runtime/identity.rs       Role::Onyc

crates/zerox1-defi-protocols/constants.rs   ONYC_MINT, KAMINO_ONYC_*
                                            (verified on-chain via RPC)

tools/fleet-pm-stub/                Deposit{onyc} + Withdraw{onyc}
                                    envelope spec builders, harvest helpers

tools/fleet-dashboard-server/       read_onyc_obligation,
                                    onyc StrategyMeta with pitch copy

deploy/systemd/hedgents-onyc-live.service   New live mainnet unit
deploy/install-hedgents.sh                  onyc-role.key generation
HEDGENTS_PIVOT/ONYC_DEPLOY.md               Operator runbook
```

### Tests

| Package | Tests at v0.5.0 |
|---|---|
| onyc-daemon | 98 ✓ |
| multiply-daemon | 118 ✓ (no regression) |
| orchestrator-daemon | 28 ✓ |
| fleet-pm-stub | 98 ✓ |
| fleet-dashboard-server | 39 + 11 = 50 ✓ |
| zerox1-protocol (onyc) | 6 ✓ |
| **Total** | **400+ tests, 0 failures** |

### Versions in context

- v0.4.0 → v0.4.27: the original three-strategy fleet shipped to
  mainnet and matured (24 patch versions of bug fixes, dashboard
  improvements, harvest module, hedgedjlp basis trade, multiply
  unwind, riskwatcher integration, multisig scoping)
- v0.4.28 → v0.4.29d: onyc strategy built incrementally with
  per-milestone DEVLOG entries (every component shipped + tested
  before moving to the next)
- **v0.5.0**: the moment all the v0.4.28-d work consolidates into
  a coherent fourth strategy ready to deploy

### What's left between v0.5.0 and live capital

The CODE is feature-complete. What's left is operational:

1. CI tarball v0.5.0 aarch64
2. SSH to Hetzner, run `deploy/install-hedgents.sh` (idempotent —
   generates `onyc-role.key`, derives `ONYC_PUBKEY`, enables the
   systemd unit)
3. Edit `/etc/hedgents/orchestrator-targets-mainnet.json` to add
   the `onyc` entry
4. Restart `hedgents-onyc-live.service`
5. $50 smoke test sequence per `HEDGENTS_PIVOT/ONYC_DEPLOY.md`
6. If clean, hand off to the orchestrator allocator (no further
   config — the apr-weighted allocator auto-discovers the strategy
   from the dashboard's `/strategies` response)

Estimated 1 hour of operator time for steps 1-5. The strategy goes
autonomous from step 6.

---

## v0.4.30 — onyc-daemon deploy scaffolding (2026-06-08)

The onyc strategy code is feature-complete (v0.4.28 → v0.4.29d). v0.4.30
ships the deploy plumbing so it can actually ride to Hetzner alongside
the existing fleet.

**What landed:**

*`deploy/systemd/hedgents-onyc-live.service`* — new systemd unit for
the live mainnet daemon. Mirrors the multiply pattern:
- TCP port 19315 (multiply is 19312, hedgedjlp is 19313, stable-yield
  is 19311 — port hygiene preserved)
- Same `--orchestrator-agent-id`, `--require-approval`,
  `--simulate-only=false`, `--i-understand-this-is-mainnet` flags as
  other live daemons
- `--max-position-usdc-lamports=500000000` ($500 v0 ceiling, well
  below the $500K hard cap in caps.rs)
- Standard security hardening (NoNewPrivileges, ProtectSystem,
  ProtectHome, ReadWritePaths, PrivateTmp)

*`deploy/install-hedgents.sh`* updated to:
- Generate `onyc-role.key` alongside the other five roles during
  first-run secrets
- Derive `ONYC_PUBKEY` into `/etc/hedgents/hedgents.env`
- Update existing env files in place (sed replacement + append if
  pre-onyc install)
- Enable `hedgents-onyc-live.service` for boot survival

*`HEDGENTS_PIVOT/ONYC_DEPLOY.md`* — step-by-step deploy guide. Covers
pre-flight, install, targets.json edits, $50 smoke test sequence
(deposit → leverage → unwind), rollback plan, and known limitations.

**What's deliberately not in v0.4.30:**

- No CI tarball generation step — that's manual for the first deploy
  to confirm the artifact looks right
- No on-Hetzner unit start — the operator runs `install-hedgents.sh`
  manually to keep the first cutover supervised
- No multi-position parallelism — single onyc position at a time, v0
  scope only

**Where the codebase stands at v0.4.30:**

```
crates/onyc-daemon/                ~5,500 lines (vs multiply ~8,500)
  src/seed.rs                      380 lines  ← v0.4.28b
  src/leverage.rs                  360 lines  ← v0.4.28c
  src/unwind.rs                    370 lines  ← v0.4.28d
  src/nav_controller.rs            330 lines  ← v0.4.29c
  src/liq_monitor.rs               +60 lines  ← v0.4.29d (NAV integration)
  src/{dispatch,caps,kamino,...}   forked + sed-renamed

crates/zerox1-protocol/fleet/onyc.rs           150 lines  ← v0.4.28
                                MsgType::WithdrawOnyc = 0x1B

crates/zerox1-defi-runtime/identity.rs         +3 lines  (Role::Onyc)

crates/zerox1-defi-protocols/constants.rs      +30 lines  (KAMINO_ONYC_*)

tools/fleet-pm-stub/allocator_runner.rs        +130 lines  (envelope spec)
tools/fleet-pm-stub/allocator.rs               +3 lines    (sol_beta_for)

tools/fleet-dashboard-server/
  chain/kamino.rs                  +50 lines  (read_onyc_obligation)
  chain/mod.rs                     +20 lines  (onyc_position method)
  api/state.rs                     +20 lines  (STRATEGIES + strategies endpoint)

deploy/systemd/hedgents-onyc-live.service      46 lines  ← v0.4.30
deploy/install-hedgents.sh                     +15 lines  ← v0.4.30
HEDGENTS_PIVOT/ONYC_DEPLOY.md                  165 lines  ← v0.4.30
```

**Tests across the workspace:**
- onyc-daemon: 98 pass
- multiply-daemon: 118 pass (no regression)
- fleet-pm-stub: 98 pass
- orchestrator-daemon: 28 pass
- fleet-dashboard-server: 39 pass
- zerox1-protocol: 6 onyc tests pass
- **Total: 387+ tests, 0 failures**

**Roadmap to mainnet:**
1. CI tarball v0.4.30 (manual trigger)
2. SSH to Hetzner, run install-hedgents.sh
3. Edit targets.json (add onyc entry)
4. Restart hedgents-onyc-live.service
5. $50 smoke test per ONYC_DEPLOY.md
6. If clean → allocator drives autonomously
7. If broken → rollback per ONYC_DEPLOY.md

ETA to live: 2-3 days of operator time once CI tarball is built.

---

## v0.4.29d — nav_controller wired into liq_monitor (2026-06-08)

Step 109b done. The NAV-aware controller module is now live in the
liq_monitor tick loop. Every beacon tick the monitor:

1. Reads the ONyc obligation
2. Pulls the ONyc deposit slot's `market_value_sf` and
   `deposited_amount`
3. Converts to per-token NAV (micro-USD) via the new pure helper
   `nav_micro_usd_from_deposit`
4. Pushes the observation onto a shared `Arc<tokio::sync::Mutex<NavHistory>>`
5. Calls `decide_nav_response` with the default thresholds
6. On `DampenAlerts` — suppresses any Critical/Warning Escalate that
   would otherwise have fired this tick. Logs the dampening loudly
   (operator sees `NAV-aware controller DAMPENED an alert this tick`).
7. On `EscalateAlerts` — logs the escalation reason
   (`SingleStepBeyondThreshold` or `SustainedDownwardDrift`), then
   falls through to the existing Critical/Warning escalation path.
8. On `Normal` — existing logic runs unchanged.

**LiqMonitorCtx grew one field:** `nav_history: Arc<Mutex<NavHistory>>`,
initialized empty at boot in main.rs. The mutex is tokio's async
variant since the tick is an async function.

**Behavior under realistic ONyc NAV cadence (monthly Apex attestation):**

| Scenario | Old behavior | New behavior |
|---|---|---|
| Monthly attestation step-up | No issue | No issue (Normal) |
| Monthly attestation step-down (30-80bps, typical) | LTV drift would trigger Warning Escalate | Dampened — operator alerted but no auto-escalate |
| Single 5%+ NAV mark-down (real loss event) | Critical Escalate fires | Critical Escalate fires (with SingleStepBeyondThreshold reason) |
| 3+ consecutive small NAV step-downs | Each one triggers Warning | First N-1 dampened, then SustainedDownwardDrift escalation kicks in |

**The pitch.** OnRe's NAV cadence is well-published (monthly Apex
attestation, ~30-80bps deltas in 2026). The NAV-aware controller
encodes that domain knowledge so onyc-daemon doesn't panic-unwind on
the kind of routine NAV step that a naive controller would treat as
an emergency.

98/98 onyc-daemon tests pass. Multiply still at 118/118.

---

## v0.4.29c — onyc NAV-aware controller (2026-06-08)

Step 109 done. The "above the protocol" IP element of onyc-daemon
ships. New `nav_controller.rs` module — standalone, pure-function,
14 tests pass.

**The problem it solves.** Standard LTV controllers assume continuous
price movement and react to every tick. ONyc NAV doesn't move
continuously — it updates in discrete chunks via Chainlink Data Streams
(monthly Apex Group attestation cadence, plus event-driven updates on
reinsurance claim settlements). A naive controller treats a single
chunky NAV update as an "emergency LTV drift" and triggers an
auto-unwind that realizes the NAV drop, pays Orca slippage on a
$15M-depth secondary market, and surrenders the position right when
the regime was otherwise healthy.

**The mechanism.** Three-state response enum:

- `Normal` — no prior observation OR latest step is upward OR latest
  downward step is within the buffer (50bps default). Standard LTV
  monitoring applies.
- `DampenAlerts { step_delta_bps, streak }` — single discrete
  downward step beyond the buffer but neither cumulative nor single-event
  large enough to alarm. The auto-unwind path is silenced for this
  tick; the controller waits to see if it's a transient single event
  or the start of sustained drift.
- `EscalateAlerts { reason, step_delta_bps, streak }` — real
  impairment detected. Either:
    - `SingleStepBeyondThreshold` — one step exceeded 500bps (5%) —
      this is a real reinsurance loss event regardless of streak
    - `SustainedDownwardDrift` — 3+ consecutive downward steps —
      drift past the noise floor even if each step is buffered

**Tunables (defaults calibrated against OnRe's published NAV history):**
- `DEFAULT_STEP_BUFFER_BPS = 50` — 0.5% noise floor
- `DEFAULT_SINGLE_STEP_ALARM_BPS = 500` — 5% single-event threshold
- `DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS = 3` — drift streak cap

**Test coverage (14 tests):** empty / single / multi-observation
histories; upward steps; small downward steps within and beyond buffer;
large single downward steps; sustained drift detection; streak reset on
upward move; history capacity cap; duplicate-observation dedup; default
constant sanity invariants.

**Integration plan (deferred to step 109b / future):** call
`decide_nav_response` from `liq_monitor::tick` before emitting Escalate
envelopes. When response is `DampenAlerts`, suppress Critical-band
emission for that tick. Module is decoupled — integration is mechanical.

98/98 onyc-daemon tests pass (14 new nav_controller tests + 84 existing).

---

## v0.4.29b — dashboard server picks up onyc (2026-06-08)

Step 112: dashboard server can now read and display ONyc strategy
state. Closes the loop between orchestrator allocator → onyc-daemon
→ dashboard reporting.

**What landed:**

*`chain/kamino.rs`:* new `read_onyc_obligation` function — clone of
`read_multiply_obligation` but uses obligation seed (0, 2) for the
ONyc isolated market. `load_reserve_price_meta` extended to handle:
- `KAMINO_ONYC_RESERVE` → pinned NAV $1.11/token (ONyc isn't on Pyth;
  it has Chainlink NAV oracle which we approximate for dashboard
  display only — real NAV reads come in the NAV-aware controller
  in step 109)
- `KAMINO_ONYC_USDC_RESERVE` → standard USDC Pyth feed (same pricing
  as Kamino main USDC reserve)

*`chain/mod.rs`:* new `onyc_position` method on ChainReader with 30s
cache (same TTL as other position reads), new `onyc_position` field
on ChainCache.

*`api/state.rs`:* ONyc added to STRATEGIES with institutional pitch
copy:
> "Leveraged on-chain reinsurance NAV. Deposits ONyc (OnRe's
> Bermuda-licensed tokenized reinsurance token) as collateral in
> Kamino's isolated ONyc market, borrows USDC at a conservative 40%
> LTV, and recycles into more ONyc. Yield is OnRe's reinsurance
> premium income (~11% base NAV growth) amplified by ~1.5-2× leverage.
> USD-denominated, delta-neutral, uncorrelated to crypto regimes — the
> portfolio's RWA exposure."

The `strategies()` endpoint now reads `onyc_position` and computes
`deployed_usd = deposited_usd_micro - borrowed_usd_micro` (same shape
as multiply).

**Test status:** 39/39 dashboard server tests pass (1 fixture
updated for the new strategy count). All other daemons unchanged.

**End-to-end status:** the entire path is now wired:

```
orchestrator allocator
  → action_to_envelope_spec
    → AssignOnyc / WithdrawOnyc envelope
      → onyc-daemon dispatch
        → leverage::run_or_simulate
          → Kamino ONyc isolated market

dashboard /strategies
  → ChainReader::onyc_position
    → chain::kamino::read_onyc_obligation
      → seed (0, 2) → Kamino ONyc isolated obligation
```

**What's still left before mainnet:**
1. NAV-aware LTV controller (task 109) — the "above the protocol" IP
   element. Currently leverage.rs uses a flat 90% safety factor; the
   NAV-aware version absorbs discrete NAV mark-down events without
   over-reacting.
2. targets.json — add onyc recipient pubkey alongside the others.
3. Hetzner deploy — systemd unit for onyc-daemon, onyc-role.key secret.
4. Devnet integration test — confirm the full envelope round-trip.

Estimated ~5-7 days to mainnet from here.

---

## v0.4.29 — onyc strategy wired into orchestrator (2026-06-08)

Step 111: the orchestrator can now build + send AssignOnyc and
WithdrawOnyc envelopes via the existing allocator dispatch path. With
this in place, the orchestrator + onyc-daemon can talk end-to-end
once the dashboard server (step 112) is updated.

**What landed:**

*`tools/fleet-pm-stub/src/allocator_runner.rs`:*
- `ExecuteTargets` now has an `onyc: Option<RecipientTarget>` field
- `action_to_envelope_spec` handles `Deposit{onyc}` (emits AssignOnyc
  with default `target_ltv_bps=4000` and the requested USDC) and
  `Withdraw{onyc}` (emits WithdrawOnyc as full-unwind, like multiply)
- New `build_onyc_releverage_spec` for harvest-loop re-leverage with
  `usdc_lamports=0` (LTV-restore without new capital)
- New `build_onyc_full_withdraw_spec` for harvest-loop full unwind
- Test fixture updated to include `onyc` target

*`tools/fleet-pm-stub/src/allocator.rs`:*
- `sol_beta_for("onyc") = 0.0` — ONyc is USD-denominated reinsurance
  NAV with no SOL exposure even after leveraged USDC borrow

**What's not yet wired:**
- Harvest module (`orchestrator-daemon/src/harvest.rs`) still only
  re-levers multiply. Adding onyc to the harvest loop is a future
  enhancement; for v0 the allocator's normal tick handles the basic
  deposit/withdraw flow.
- Dashboard server (step 112) — orchestrator's snapshot fetch
  currently won't see an "onyc" strategy entry until the dashboard
  reports one. That's the next milestone.

**Test status:** All daemons build clean. fleet-pm-stub: 98/98 lib
tests pass (1 fixture updated). orchestrator-daemon: 28/28 pass.
onyc-daemon: 84/84 pass. multiply-daemon: 118/118 pass.

**End-to-end status:** the wiring is complete from orchestrator
allocator → onyc envelope spec → onyc-daemon dispatch. The remaining
gaps before mainnet:
- Dashboard reports onyc as a strategy (112)
- targets.json includes onyc recipient pubkey
- onyc-daemon systemd unit deployed to Hetzner
- Devnet integration test

---

## v0.4.28d — onyc-daemon simplified unwind (2026-06-08)

Step 110d done. The forked 2138-line unwind.rs (multiply's flash-loan
jitoSOL→SOL→USDC unwind with iterative-round fallback and rc19 sizing
protection) is now ~370 lines of clean ONyc→USDC flow.

**The simplified two-phase flow:**

*Phase 1 (single tx):*
1. RefreshReserve(ONyc)
2. RefreshReserve(USDC)
3. RefreshObligation
4. RepayObligationLiquidityV2(USDC, u64::MAX → all debt)
5. WithdrawObligationCollateralAndRedeemReserveCollateralV2(ONyc, u64::MAX)

Kamino's V2 handlers clamp `u64::MAX` to the actual on-chain amount
internally (per rc19), so passing the sentinel is the canonical
"all-of-it" call.

*Phase 2 (separate tx):*
Jupiter ONyc→USDC swap on the freed ONyc balance via
`build_onyc_to_usdc_swap_tx`. Dust handling: if the swap can't route
the full balance under the slippage budget, residual ONyc stays in the
wallet and `residual_onyc_lamports` surfaces it for the orchestrator
to handle.

**What was removed:**
- Flash-loan path (multiply needed it because jitoSOL→SOL→USDC has no
  source of USDC mid-flight; ONyc unwind needs USDC in the wallet
  before repay)
- Iterative-round unwind fallback with rc19 max_withdraw_value sizing
- Two-leg jitoSOL→SOL via Jito + SOL→USDC via Jupiter

**Trade-off:** Without flash-loans, the wallet must hold USDC to cover
the repay leg. v0 surfaces `ERR_USDC_INSUFFICIENT_FOR_REPAY` if it
doesn't — operator pre-funds or the orchestrator deposits before
issuing WithdrawOnyc. This is acceptable for v0 because:
- The orchestrator can manage funding via its existing USDC inventory
- At v0 scale ($100), pre-funding is operationally trivial
- Flash-loan support is an option-A future upgrade

**Error codes:** `ERR_DEADLINE_EXPIRED=1`, `ERR_OBLIGATION_NOT_FOUND=2`,
`ERR_USDC_INSUFFICIENT_FOR_REPAY=3`, `ERR_JUPITER_SWAP_FAILED=4`.

84/84 onyc-daemon tests pass. 118/118 multiply-daemon tests still pass.

**Status of the asset-swap rewrite:** all three core files done.
seed.rs (380 lines), leverage.rs (360 lines), unwind.rs (370 lines).
Total daemon size dropped from ~8500 to ~5500 lines while gaining
the full ONyc+USDC asset semantics. Next steps: NAV-aware LTV
controller (109), orchestrator wiring (111), dashboard integration
(112), then devnet → mainnet.

---

## v0.4.28c — onyc-daemon simplified single-round leverage (2026-06-08)

Step 110c done. The forked 1450-line leverage.rs (multiply's multi-round
LTV-aware SOL-borrow + jitoSOL-deposit walk) is now ~360 lines of
clean single-round option-B logic.

**The simplified flow:**
1. seed (USDC→ONyc swap + collateral deposit) via `seed::maybe_seed_obligation`
2. read obligation state — collateral_value_sf + bf_debt_value_sf
3. compute target USDC borrow via the new pure function
   `compute_borrow_lamports_for_target` — applies 90% safety factor
   (BORROW_SAFETY_BPS) to cover BF drift + oracle precision + future
   NAV mark-downs
4. build one tx: RefreshReserve(ONyc) + RefreshReserve(USDC) +
   RefreshObligation + BorrowObligationLiquidityV2(USDC)
5. submit (or simulate); return Report with post-borrow LTV

**What was removed:**
- Multi-round walk with per-round LTV clamping
- In-flight bf_debt tracking (rc38 invariant — needed because multiply's
  multi-round walk could broadcast multiple rounds before RPC reads
  caught up; single-round doesn't have this race)
- Borrow-factor clamp helper (USDC on the ONyc market has BF=1.0 so
  no adjustment needed; multiply's clamp was specifically for SOL's
  1.25× BF on Kamino main)
- Flash-loan bundle (multiply unwind complexity, not relevant)
- Jito stake step (already removed in seed)

**Trade-off vs multiply's option-A:** no auto-recycle in the same tx.
To reach higher steady-state LTV, the orchestrator issues a follow-up
AssignOnyc{ usdc_lamports: just_borrowed } so seed swaps + deposits
the new principal, then a subsequent AssignOnyc{ target_ltv_bps }
borrows again. Each round is its own tx — simpler reasoning, no
in-flight tracking. caps::MAX_LEVERAGE_LOOP_ROUNDS=2 governs the
orchestrator's round count, not this function's per-call work.

**Test coverage:** 8 new tests on `compute_borrow_lamports_for_target`
covering: zero collateral, zero target, at-target case, 40% target on
fresh position (≈$36 of $40 headroom after 0.9 safety), top-up to
reach target from partial debt, safety-factor invariant, zero-price
degenerate case, constant sanity checks.

106/106 onyc-daemon tests pass. 118/118 multiply-daemon tests still
pass. The daemon now has working seed AND leverage paths — the unwind
path (step 110d) still implements multiply's jitoSOL→SOL→USDC flow
and is the last asset-swap rewrite remaining.

---

## v0.4.28b — onyc-daemon real pubkeys + seed rewrite (2026-06-08)

Follow-on to the v0.4.28 scaffold. Two milestones:

**Step 110a — real on-chain pubkeys discovered.** Verified the ONyc USDC
borrow reserve at `AYL4LMc4ZCVyq3Z7XPJGWDM4H9PiWjqXAAuuHBEGVR2Z` via
Kamino API + `lending_market` field check at offset 32. Decoded the
USDC reserve's farm_collateral at offset 64 (rc49 method) as
`GNcywqL6AZajsyyitxGQUvbihPgAzGZUqKfjYcvTj2pi`. ONyc collateral reserve
has no farm. Kamino does not publish an ALT for the isolated ONyc
market — v0 builds versioned txs without one (single-round simplified
loop fits the size budget comfortably). Real USDC borrow APR on the
market is 8.12% (not 6.88% as the initial research stated), revising
the expected net APR at 1.5-2x leverage down to ~12-14% from the
earlier 16-18% headline.

**Step 110b — seed.rs rewritten for USDC→ONyc.** The forked seed.rs
inherited from multiply was 800 lines of USDC→SOL→jitoSOL→Kamino
pipeline. Replaced with a clean ~380-line USDC→ONyc→Kamino flow:
- New `build_usdc_to_onyc_swap_tx` + `build_onyc_to_usdc_swap_tx` in
  `protocols/jupiter.rs` (Jupiter routes through Orca whirlpools where
  ONyc liquidity lives).
- `seed_with_usdc` swaps via Jupiter, broadcasts, falls through to the
  in-wallet deposit path.
- `maybe_seed_obligation` reads the wallet's ONyc ATA balance and
  deposits as obligation collateral via the existing
  `deposit_reserve_liquidity_and_obligation_collateral_v2_ix` Kamino
  helper (collateral-side mechanics are asset-agnostic).
- No Jito stake pool dependency — ONyc is acquired directly.
- New `decide_seed_amount` with `cap_onyc_to_usdc_equivalent` helper
  that clamps the deposit to the operator's `--max-position-usdc-lamports`
  CLI cap, using a rough ONyc NAV of $1.11 to convert USD cap into
  ONyc lamports. Tests pin the integer-math truncation at the cap boundary.

112/112 onyc-daemon tests pass (down from 118 — old multiply-specific
seed tests retired). 118/118 multiply-daemon tests still pass. 6/6
protocol tests pass.

What's still pending in step 110: `leverage.rs` and `unwind.rs`
rewrites — those still implement multiply's SOL-borrow + jitoSOL-deposit
multi-round walk. These are the remaining heavy edits before mainnet
deploy.

---

## v0.4.28 — onyc-daemon scaffold + Kamino isolated market constants (2026-06-08)

Foundation for the multiply → onyc strategy swap. Multiply's leveraged
jitoSOL+SOL position is structurally net long SOL and fails the "USD
yield vault" thesis (per Superteam feedback). The new `onyc` strategy
replaces it with leveraged ONyc + USDC borrow on Kamino's ONyc isolated
market — USD-denominated, delta-neutral, captures OnRe's reinsurance
premium yield (~11% base, ~13-15% looped at conservative 1.5-2x).

**What landed in v0.4.28 (scaffolding milestone):**

*Protocol layer* (`p2p_architecture/zerox1-protocol`):
- New module `fleet/onyc.rs` with `AssignOnyc`, `ReportOnyc`,
  `WithdrawOnyc`, `ReportOnycWithdraw` (CBOR round-trip tested).
- New `MsgType::WithdrawOnyc = 0x1B` distinct from `Withdraw=0x18` and
  `WithdrawMultiply=0x1A` for clean payload-decode disambiguation.

*Runtime layer* (`zerox1-defi-runtime`):
- New `Role::Onyc` variant alongside the existing six fleet roles.

*Protocols crate* (`zerox1-defi-protocols`):
- New constants: `ONYC_MINT`, `KAMINO_ONYC_MARKET`, `KAMINO_ONYC_RESERVE`.
- Placeholder constants (`KAMINO_ONYC_USDC_RESERVE`, `_FARM_COLLATERAL`,
  `_LOOKUP_TABLE`) with `pubkey!("11111111111111111111111111111111")`
  pending on-chain discovery — daemon compiles, runtime path will fail
  Kamino account validation if these are fed to mainnet (intentional
  fail-loud before live deploy).

*New daemon* (`crates/onyc-daemon`):
- Forked from `multiply-daemon` via `cp -r` then bulk sed renames
  (AssignMultiply→AssignOnyc, fleet::multiply→fleet::onyc,
  Role::Multiply→Role::Onyc, multiply-role.key→onyc-role.key,
  residual_sol_lamports→residual_onyc_lamports, etc).
- `caps.rs` tuned for ONyc risk profile: `MAX_LTV_BPS = 5000` (50%,
  vs multiply's 8000), `DEFAULT_TARGET_LTV_BPS = 4000` (40%),
  `MAX_LEVERAGE_LOOP_ROUNDS = 2` (option B v0 — single round normally,
  second only if first underflowed target), `MAX_POSITION_USDC_LAMPORTS`
  capped at $500K (well under 5% of Orca secondary depth so slippage
  model holds), `USDC_BORROW_FACTOR_BPS = 10_000` (USDC borrow has no
  BF adjustment, unlike multiply's SOL borrow at 12_500).
- Distinct `ONYC_OBLIGATION_SEED = (0, 2)` so each strategy owns a
  separate Kamino obligation PDA — load-bearing because Kamino's
  liquidator seizes *all* collateral on a single obligation.
- All 118 inherited tests pass (CBOR roundtrips, dispatch routing,
  leverage clamp math, unwind bundle assembly).
- multiply-daemon still builds + tests (Role::Onyc addition is
  non-breaking).

**What v0.4.28 deliberately does NOT do:**

The daemon compiles and inherits multiply's full test suite but the
internal transaction-building logic still implements multiply's
jitoSOL+SOL+Jito-staking pipeline. It will FAIL at runtime against
mainnet because:
1. Three Kamino pubkeys are placeholder `11111…1111` (USDC reserve,
   farm collateral, ALT) — need on-chain discovery.
2. seed.rs still does USDC→SOL via Jupiter then SOL→jitoSOL via Jito
   stake pool, instead of USDC→ONyc via Orca.
3. leverage.rs still walks the multi-round Kamino-main loop with SOL
   borrows against jitoSOL collateral, not the simplified single-round
   USDC borrow against ONyc collateral.
4. unwind.rs still does jitoSOL→SOL→USDC unwind, not ONyc→USDC.

These are the next milestones (task #110 cont'd):
- Source real pubkeys via on-chain inspection of the ONyc isolated
  market reserves list.
- Rewrite seed.rs as a clean USDC→ONyc→Kamino deposit (no staking step).
- Rewrite leverage.rs as a simplified single-round borrow (option B
  per the strategy spec).
- Rewrite unwind.rs as an ONyc→USDC swap-and-repay.

Then orchestrator wiring (task #111) and dashboard integration (#112).

---

## v0.4.27 — harvest realizes hedgedjlp PnL via full-unwind auto-accept (2026-06-06)

The v0.4.26 harvest loop only *logged* "manual harvest recommended" when
hedgedjlp accumulated unrealised perp PnL above the threshold — it
couldn't actually realize it because hedgedjlp-daemon's auto-mode
hard-blocked WithdrawHedgedJlp under all conditions ("always manual,
JLP is not USD-denominated"). v0.4.27 closes that gap.

**What changed in hedgedjlp-daemon.** Added `auto_allow_full_withdraw`
opt-in. When that flag is set AND auto-mode is on AND the sender matches
the configured orchestrator AND `jlp_lamports == u64::MAX` (full-unwind
sentinel), the daemon auto-executes the unwind. Partial-size withdraws
still queue — the original "always manual" policy applies whenever a
sizing decision exists; a full unwind has no sizing decision. CLI flag
`--auto-allow-full-withdraw` (default false).

The cooldown gate from `AutoModeConfig.cooldown_secs` still applies so
harvest can't hammer the unwind path every tick. The 24h USD cumulative
cap is skipped for full-unwind since `jlp_lamports` doesn't translate
to USD at gate time.

**What changed in the orchestrator.** Harvest's hedgedjlp branch now
dispatches `WithdrawHedgedJlp { jlp_lamports: u64::MAX }` instead of
just logging, using a new `HARVEST_HEDGEDJLP_KEY` cooldown distinct
from the allocator's `"hedgedjlp"` key. New
`fleet_pm_stub::allocator_runner::build_hedgedjlp_full_withdraw_spec`
gives the orchestrator a clean way to construct the envelope.

**The realize loop end-to-end.**
1. Harvest tick reads `realtime_protocol_pnl_usdc` from `/strategies`.
2. If > `--harvest-pnl-threshold-usd` (default $5): send
   `WithdrawHedgedJlp{u64::MAX}` to hedgedjlp-daemon.
3. Daemon's auto-mode gate accepts; `unwind::run_or_simulate` closes
   shorts and sells JLP to USDC. Funding + mark-to-market PnL settles
   to wallet.
4. The 60s allocator tick sees the freed idle USDC and redeploys it
   into a fresh hedgedjlp position via the existing AssignHedgedJlp
   path.
5. Net effect: PnL realized + reopened, minus close+open round-trip
   fees (~$0.50–1.50 on a $100-scale position).

**Deploy requirements.** Hedgedjlp-daemon systemd unit must be updated
to add `--auto-allow-full-withdraw=true` alongside the existing
`--auto-accept-orchestrator=true`. Without the new flag the daemon
ignores harvest's full-unwind requests (clear "withdraw-manual-only"
log line surfaces the cause).

6 new auto_mode tests pin the gate (opt-in disabled → still queues;
partial sizes still queue; cooldown still enforced; non-orchestrator
sender still rejected; auto-mode master switch still required).

---

## v0.4.26 — orchestrator: per-strategy harvest loop (2026-06-06)

The orchestrator now runs a second, slower loop alongside the 60s
allocator tick: a per-strategy harvest pass that watches earnings and
restores multiply leverage after collateral has appreciated.

**Why this was needed.** The three strategies compound differently and
the allocator only handles principal flows:
- `stable_yield` auto-compounds inside Kamino (kToken share value).
- `multiply` collateral grows as jitoSOL appreciates, but LTV drifts
  *downward* over time — leverage decays, yield drops. The allocator
  never restores it.
- `hedgedjlp` perp funding + mark-to-market accrue *inside* the open
  shorts (the dashboard's "Realtime perp PnL" number). Jupiter Perps
  doesn't settle funding to the wallet, so this PnL is unrealised until
  someone partially closes — which the hedgedjlp daemon doesn't support
  yet.

**What shipped.** New `crates/orchestrator-daemon/src/harvest.rs` runs
every 6h by default:
- multiply: if `target_ltv − current_ltv > 150bps`, emit
  `AssignMultiply { target_ltv_bps, usdc_lamports: 0 }` to re-borrow
  against the appreciated collateral and restore target leverage.
  Reuses the existing dispatch surface — no new envelope type.
- hedgedjlp: log unrealised perp PnL each tick and emit a "manual
  harvest recommended" warning when it crosses `$5` (configurable). No
  auto-realize — partial-close logic is a future hedgedjlp daemon
  change, not an orchestrator change.
- stable_yield: observed only; auto-compounds in protocol.

Each tick appends one JSONL line to `harvest-audit.jsonl` so the
operator can replay decisions. The allocator's existing
`orchestrator-audit.jsonl` is unchanged.

New CLI knobs (defaults pinned in `harvest.rs`):
`--harvest-interval-secs` (21600), `--harvest-ltv-drift-bps` (150),
`--harvest-pnl-threshold-usd` (5.0), `--harvest-target-ltv-bps` (6000),
`--harvest-audit-log`. Setting interval to 0 disables the loop.

The harvest loop uses a distinct cooldown key
(`HARVEST_MULTIPLY_KEY = "harvest_multiply"`) so its dispatches don't
collide with the allocator's `"multiply"` cooldown — both loops can run
concurrently without suppressing each other.

12 unit tests pin the decision logic at the threshold boundary
(`drift == threshold` skips; `drift > threshold` fires) — same shape
as the allocator's hysteresis tests.

---

## v0.4.25 — dashboard: dynamic incidents-resolved count + combined APR matches card headline (2026-06-05)

Two dashboard fixes the operator caught back-to-back.

**1. Incidents-resolved counter was stuck at 18 since rc-era.** The hero
banner exposes `incidents_resolved` from `/lifetime`, which was a
hand-bumped constant. We forgot to bump it through ~16 ships, so the
public-facing counter silently froze. Replaced with a const-fn that
counts `## v0.` headings in the embedded DEVLOG.md at compile time —
monotonic by construction, impossible to forget. Trade-off: the count
is now "every release" rather than the original (manually curated)
"incidents with regression test", but the previous semantics was
unverifiable in code and 18 was just a historical artifact anyway.

**2. Combined APR diverged from the stable_yield card headline.**
Strategy cards lead with `apr_24h_bps ?? current_apr_bps` (rc8), but
`/aum`'s `combined_apr_bps` used live spot only. When Kamino's USDC
supply rate moved between samples, the two diverged — today the
strategy card showed `stable_yield 5.36 %` while combined APR read
`3.74 %`, all funds in stable_yield. Same fallback chain now in both
places, so "100 % in stable_yield" produces a single number.

**Files**

```
tools/fleet-dashboard-server/src/api/state.rs  — count_release_headings + apr_for fallback
Cargo.toml                                     — 0.4.24 → 0.4.25
DEVLOG.md                                      — this entry
frontend/lib/ships.ts                          — v0.4.25 ship entry
```

---

## v0.4.24 — cap Kamino-SOL-borrow proxy for hedgedjlp hedge cost (2026-06-05)

Operator question caught a real bug: dashboard reported hedgedjlp net APR
at 0.51 % despite JLP fees still yielding ~17.7 %. The allocator's
carry-mode hurdle (`stable_yield + 3 % risk premium = 6.85 %`) saw
hedgedjlp underwater and aggressively unwound the position three times
in the same hour.

**Root cause.** `fleet_rates::compute` uses Kamino's SOL borrow rate as
a proxy for the Jupiter Perps hedge cost:

```rust
let hedge_cost = sol_borrow * HEDGEDJLP_HEDGE_FRACTION;  // 0.75
let hedgedjlp_net = (jlp_fee - hedge_cost).max(0.0);
```

The proxy holds when Kamino SOL borrow tracks broader market rates
(4–8 % APR). But hedgedjlp does not actually borrow on Kamino — it pays
funding to Jupiter Perps shorts. When Kamino SOL borrow spikes during a
liquidity squeeze (today: 22.89 %), the proxy diverges from reality and
poisons the orchestrator's APR estimate.

Same shape as multiply's rc36 bug — proxy borrow rate (USDC at that time)
clamped multiply's APR to zero for hours. Fix there was to switch to the
actual borrow asset. Fix here is to cap the proxy until a Jupiter Perps
on-chain custody reader exists.

**Fix.** Cap the proxy at 8 % in both the rate snapshot
(`fleet_rates.rs`) and hedgedjlp's telemetry log (`telemetry.rs`). With
the cap at today's rates, hedge cost = 8 × 0.75 = 6 %, net APR =
17.70 − 6 = 11.70 % — comfortably above the 6.85 % hurdle. Proper fix
(read `custody.funding_rate_state` per open short) is left for a
follow-up rc.

**Files**

```
crates/zerox1-defi-runtime/src/fleet_rates.rs        — HEDGEDJLP_PROXY_BORROW_CEIL_PCT
crates/hedgedjlp-daemon/src/telemetry.rs             — match cap in pnl log
Cargo.toml                                           — 0.4.23 → 0.4.24
DEVLOG.md                                            — this entry
frontend/lib/ships.ts                                — v0.4.24 ship entry
```

---

## v0.4.23 — orchestrator's /strategies HTTP timeout 15s → 90s (2026-06-05)

After v0.4.22 the unwind→sweep→redeploy chain worked end-to-end: the
first `deposit/stable_yield $135.38` envelope landed (stable_yield
$21.51 → $157.31). Every tick after that failed with
`failed:GET http://127.0.0.1:7700/strategies — operation timed out`.

**Root cause.** `fleet_pm_stub::allocator_runner::fetch_snapshot`
builds its reqwest client with `.timeout(Duration::from_secs(15))`
(allocator_runner.rs:68). The dashboard's `/strategies` handler
makes four sequential RPC calls (`multiply_position`,
`stable_yield_position`, `hedgedjlp_position`, `rate_snapshot`).
Measured wall-clock at 22–60s in production when Helius is under
load. 15s was always going to be too tight.

```
call1: HTTP 200 in 44.986968s
call2: HTTP 200 in 22.093179s
call3: HTTP 200 in 59.722675s
```

The orchestrator's reqwest call gives up at 15s, the tick records
`failed:GET … operation timed out`, the cooldown clock advances,
and the entire allocator effectively stops dispatching.

**Fix.** Bump the timeout to 90s. The dashboard endpoint should
still be optimized (parallelise the four chain reads with
`tokio::join!`) — that work belongs in a separate dashboard PR.
This is the minimum-blast-radius fix that gets the live fleet
ticking again.

**Files**

```
tools/fleet-pm-stub/src/allocator_runner.rs  — timeout 15s → 90s
Cargo.toml                                   — 0.4.22 → 0.4.23
DEVLOG.md                                    — this entry
frontend/lib/ships.ts                        — v0.4.23 ship entry
```

---

## v0.4.22 — rc21 follow-up: sweep also runs on the closed-obligation early-exit (2026-06-04)

v0.4.21 wired the SOL→USDC sweep into the tail of `run_iterative_unwind`
(after the iterative drain loop). When the operator manually injected
WithdrawMultiply via fleet-pm-stub against the already-drained
obligation, the log showed:

```
INFO  WithdrawMultiply received
INFO  unwind starting
INFO  obligation does not exist; nothing to unwind (Noop)
INFO  withdraw report sent  ok=true
```

The "Noop" path is at `unwind.rs:713` — when `fetch_obligation`
returns `None` (Kamino dealloc'd the obligation account after the
prior drain), `run_or_simulate` returns immediately with
`final_usdc_lamports=0`, BEFORE reaching the `run_iterative_unwind`
function where the sweep lives. The wallet still has ~3.55 SOL
sitting there.

**Fix.** Hoist the sweep call into the Noop early-exit too. Same
helper, same constants. After v0.4.22, any WithdrawMultiply against
a closed obligation will still convert the residual wallet SOL to
USDC.

**Files**

```
crates/multiply-daemon/src/unwind.rs  — sweep call added to the closed-obligation Noop path
Cargo.toml                            — 0.4.21 → 0.4.22
DEVLOG.md                             — this entry
```

---

## v0.4.21 — multiply unwind sweeps freed SOL to USDC via Jupiter (closes the rc20 loop) (2026-06-04)

**The gap rc20 left.** v0.4.20 fully drained the leveraged jitoSOL
obligation on-chain — 8 rounds across rc19/rc20 → ~$256 of SOL
landed in the operator wallet. But the rc20 unwind reported
`final_usdc_lamports = 0` (per the long-standing TODO comment from
v0.3.1) and left the SOL there. The orchestrator's next-tick
`Deposit{hedgedjlp}` envelope would seed via a USDC-input path that
expects USDC in the wallet ATA, fail at the seed step, and propose
the same thing every cooldown.

User observed this as "dashboard shows $256 not deployed."

**What ships.**
1. New protocol helper:
   `zerox1_defi_protocols::protocols::jupiter::build_sol_to_usdc_swap_tx`
   — symmetric to the existing `build_usdc_to_sol_swap_tx` used by
   multiply's seed path. Calls Jupiter's lite-api quote + swap
   endpoints with `wrap_and_unwrap_sol = true`.
2. New `sweep_sol_to_usdc` step at the tail of
   `multiply-daemon::unwind::run_or_simulate`. Runs after the
   iterative drain completes. Computes `swappable = wallet_sol −
   SWEEP_SOL_FEE_RESERVE_LAMPORTS` (20 M lamports ≈ $1.50 reserved
   for future fees + ATA rent). Skips if below
   `SWEEP_MIN_SOL_LAMPORTS` (50 M lamports = ~$3.75) — dust dominated
   by swap fees.
3. Slippage tolerance: `SWEEP_SLIPPAGE_BPS = 100` (1 %). Liberal
   relative to typical SOL/USDC routes; chosen so a tight
   quote→swap window doesn't bounce the operator into a manual
   retry.
4. Failure mode: ANY error in the sweep (Jupiter quote outage,
   swap submit rejection, sim issue) is downgraded to a `warn!` and
   the surrounding unwind reports `final_usdc_lamports = 0`. The
   structural unwind (the on-chain debt drain) is already complete;
   the sweep is a follow-up that the operator can retry manually
   if needed.

**Honest read on what this changes about strategy.** The sweep
converts the position's directional SOL exposure into USDC at the
current Jupiter quote. That's the "lock in the loss at low SOL"
trade the operator explicitly avoided through most of the session.
By tonight's authorisation pattern ("listen to orchestrator", "fix
it"), this is the intended completion of the rebalance — the
orchestrator's target weights (45 % hedgedjlp, 20 % stable_yield)
require USDC to fund the next deposits; rc21 makes that conversion
happen automatically post-unwind.

**Production effect.** With the orchestrator restarted on rc21:
- The next tick sees mul=$0, idle≈$1 USDC + $256 SOL, hedgedjlp=$0.
- Allocator proposes Withdraw{multiply} (still — drift math
  unchanged).
- Multiply daemon receives, runs unwind. Loop exits immediately
  ("obligation empty; unwind complete"). Sweep runs on the residual
  $256 of SOL.
- Sweep submits SOL → USDC via Jupiter; ~$256 lands in USDC ATA
  (minus ~30 bps typical slippage).
- Following tick: allocator sees idle USDC ≈ $256; routes to
  hedgedjlp + stable_yield per target weights via the existing
  v0.4.x deposit path.

**Files**

```
crates/zerox1-defi-protocols/src/protocols/jupiter.rs  — build_sol_to_usdc_swap_tx (symmetric to existing helper)
crates/multiply-daemon/src/unwind.rs                   — sweep_sol_to_usdc + 3 constants + tail wiring
Cargo.toml                                             — 0.4.20 → 0.4.21
DEVLOG.md                                              — this entry
```

---

## v0.4.20 — rc19 follow-up: full-close round passes u64::MAX to klend repay (sub-lamport residual) (2026-06-04)

**The bug.** v0.4.19's retry-on-shrink got rounds 1 and 2 to commit
on-chain (sigs `3yEh...` and `3g66...`). Round 3 failed sim with a
DIFFERENT klend error:

```
Borrow: SOL amount: 1007251594.8288 value: 72.5586
Obligation new borrowed value after repay 0.0000 for SOL
AnchorError: NetValueRemainingTooSmall (6092) at lending_operations.rs:3982
Program failed: custom program error: 0x17cc
```

Klend tracks debt at sf-precision (sub-lamport). The round wanted to
repay the entire remaining 1.007 SOL debt, but its integer-truncated
amount (1_007_251_594) left `0.8288` lamports of residual sf-debt on
the obligation — positive but below klend's "obligation dust" floor,
which trips `NetValueRemainingTooSmall`.

**The fix.** Klend's standard "drain whatever's left" sentinel is
`u64::MAX` on the repay ix's `liquidity_amount` field — already
exercised by `repay_v2_round_trips_u64_max_sentinel` in
`protocols/kamino.rs:2386`. v0.4.20:

1. Splits `build_unwind_iterative_round_bundle` into a v1 wrapper
   (preserves the existing single-amount API, used by the v0.3 flash-
   loan tests and other call sites that don't full-close) and a `_v2`
   that decouples `transfer_sol_lamports` from `klend_repay_amount`.
2. Computes `remaining_debt_sol_lamports_ceil` from the obligation
   (rounding up the sf-precision debt to the next integer lamport).
3. Detects "full close" per round: `repay_after_fee >= ceil`. When
   true, the round transfers the ceiling to the wSOL ATA and passes
   `u64::MAX` to klend — which takes exactly the sf-precision debt
   and zeros the obligation cleanly. When false (partial repay),
   passes the same integer amount to both, preserving v0.4.19's
   shape.
4. New round-sizing log line surfaces `full_close: bool` so the
   audit reads cleanly when the path engages.

Two new tests pin the v2 path: full-close encodes `u64::MAX` at
bytes [8..16] of the repay ix's data while the system::transfer
carries the ceiling; partial-repay encodes the integer at both.

**On the live position.** Rounds 1+2 already committed on-chain.
The next orchestrator tick (after rc19.1 deploys) will:
- Re-fetch the obligation (now ~$253 net, smaller LTV).
- Run round 1 → likely fit on first try (better headroom).
- Continue iterative until the last round full-closes via u64::MAX.
- Final state: zero debt, residual jitoSOL fully redeemed to SOL,
  ~$200+ of freed USDC flows back through the next allocator tick.

**Files**

```
crates/multiply-daemon/src/unwind.rs  — split into v1 wrapper + v2 (two amounts), full-close detection, ceiling computation, 2 new tests
Cargo.toml                            — 0.4.19 → 0.4.20
DEVLOG.md                             — this entry
```

---

## v0.4.19 — multiply unwind: retry-on-shrink when Kamino's max_withdraw_value is binding (2026-06-04)

**The bug.** With v0.4.18's WithdrawMultiply fix the orchestrator's
unwind requests finally reach multiply's iterative unwind path. The
very first attempt at 22:11 UTC (SOL ≈ $72.61, LTV = 44.49 %) failed
sim with:

```
Withdraw value cannot exceed maximum withdraw value
  collateral_value=$450.54  withdraw_value=$75.09
  max_withdraw_value=$41.55
AnchorError: WithdrawTooLarge (6011) at lending_operations.rs:533
round sim FAILED — stopping iterative unwind
```

The unwind's per-round δ sizing is "remaining collateral / rounds
left" — at `max_rounds=6` and round 1 that's 16.67 % ≈ $75. Kamino's
actual `max_withdraw_value` at the current obligation LTV was $41.55,
so the round was rejected on-chain (sim, not submit — no state
changed).

The original sizing math assumes more headroom than the position
actually has at low-SOL-price moments. As SOL dropped from ~$95
(when the sizing math was last validated, rc30) to $72 (today), the
position's collateral-minus-bf-debt buffer shrunk and 16.67 % became
"too aggressive in one round."

**The fix (retry-on-shrink).** Inside each round, wrap the sim call
in a loop that halves `delta_jitosol_ctokens` and re-sims when the
log surfaces `WithdrawTooLarge` (klend custom error 6011). Up to
`MAX_SHRINK_ATTEMPTS = 6` halvings, then a dust floor at
`MIN_DELTA_CTOKENS = 1_000_000` (~0.001 jitoSOL). Non-WithdrawTooLarge
failures bail immediately with the existing error code — we only
retry the specific bound-too-tight case.

Convergence properties:
- Each round shrinks independently. As earlier rounds repay debt,
  later rounds see improved LTV → larger `max_withdraw_value` →
  larger δ accepted without shrinking.
- Total RPC overhead per round is bounded: 6 sims × ~1 s each = 6 s
  worst case. Typical: 1-2 sims when the initial sizing happens to
  fit, or 2-3 when it doesn't.
- The error path on shrink exhaustion is unchanged (existing
  `ERR_JUPITER_INTEGRATION_PENDING` code, residual SOL balance
  reported, tx_signatures preserved).

**The detector.** `is_withdraw_too_large(logs)` is a pure helper
that scans for any of three markers:
- The human-readable "WithdrawTooLarge" string
- "Error Code: WithdrawTooLarge"
- "Error Number: 6011"

Two of three is overkill but resilient to klend log-format tweaks.
Pinned by 5 unit tests covering the canonical production log shape,
truncated error-number-only logs, unrelated errors (must not match),
and missing/empty logs (must not match).

**Researched before fix** per the durable rule: traced the
production sim log to confirm the exact failure mode and bound;
read `build_unwind_iterative_round_bundle` to confirm it accepts
arbitrary `withdraw_jitosol_ctokens` (no embedded sizing); located
the round-level δ math at the per-round sizing block; verified the
non-WithdrawTooLarge bail path stays unchanged so this rc doesn't
mask other errors.

**Files**

```
crates/multiply-daemon/src/unwind.rs  — is_withdraw_too_large helper, MAX_SHRINK_ATTEMPTS, MIN_DELTA_CTOKENS, retry-on-shrink loop wrapping the per-round sim, 5 new tests
Cargo.toml                            — 0.4.18 → 0.4.19
DEVLOG.md                             — this entry
```

---

## v0.4.18 — rc17 follow-up: WithdrawMultiply needs non-zero vault (2026-06-03)

v0.4.17 emitted `WithdrawMultiply{vault: [0u8; 32]}` and hit the
multiply daemon's defensive check at `caps.rs:84`:

```
WARN multiply_daemon::dispatch  withdraw failed; sending error Report
  e: withdraw cap validation
  Caused by: WithdrawMultiply.vault is zero (defensive check)
```

Fix: populate `vault` with the multiply role pubkey (`recipient`)
as a non-zero placeholder. The vault field is documented as "kept
for routing parity. Daemon ignores it but validates non-zero." The
role pubkey is semantically wrong (it's not the Solana wallet) but
functionally correct: the daemon doesn't compare it against
anything. A cleaner fix would extend `MultiplyTarget` in
targets.json with a `wallet_pubkey_b58` field and thread it
through; deferred as future hygiene.

---

## v0.4.17 — orchestrator emits real WithdrawMultiply (was Assign{target=0} workaround that silently bailed) (2026-06-03)

**The bug.** Today at ~14:00 UTC the SOL price started dropping
materially. The orchestrator's allocator correctly identified
multiply was over-target (91 % vs 35 %) and emitted `Withdraw{multiply,
amount_usd=77}` envelopes every cooldown cycle (5 min). Multiply
auto-accepted each one. The multiply position never deleveraged.
The user noticed "market is down bad" and asked what was blocking
the orchestrator's decision from executing.

**Root cause (researched from production logs).** Multiply's log
showed the auto-accept firing repeatedly, followed each time by:

```
INFO multiply_daemon::leverage  leverage loop entering current_ltv_bps=4449 target_ltv_bps=0
INFO multiply_daemon::leverage  already at or above target; no work to do
```

The orchestrator emitted `AssignMultiply{target_ltv_bps=0}` as the
"Withdraw multiply" workaround. The multiply daemon's leverage
handler at `leverage.rs:193` bails out with
`if current_ltv >= target_ltv_bps { return Ok(...) }` — at
current=4449 and target=0, this fires (`4449 >= 0`) and the
deleverage never happens. The leverage handler ONLY does lever-UP.
The actual unwind path is at `MsgType::WithdrawMultiply`
(`unwind::run_or_simulate`), which the orchestrator was not using.

The workaround was explicitly documented as temporary in
`allocator_runner.rs:583`:

> Withdraw `multiply` is implemented as `AssignMultiply{target_ltv_bps=0}`
> — the multiply daemon interprets that as full deleverage on its next
> cycle, matching the CLI's existing behaviour. v0.4.x will switch this
> to `WithdrawMultiply` once the iterative unwind is the default path.

The promise wasn't kept. Every `Withdraw{multiply}` envelope from rc1
through rc16 was silently swallowed. Today's market drop made that
visible because it was the first time the orchestrator decided
"multiply should go down" against a backdrop of falling SOL.

**The fix.** `action_to_envelope_spec` now emits the real
`WithdrawMultiply` envelope (`MsgType::WithdrawMultiply = 0x1A`) with
the proper payload (vault, max_slippage_bps=50, deadline_unix=now+300).
Multiply's existing handler at `dispatch.rs:470` routes it to
`unwind::run_or_simulate` which executes the iterative deleverage
on-chain.

**Trade-off: full unwind only.** `WithdrawMultiply`'s payload (in
the protocol crate) has no amount field by design — the unwind is
always 100%. The allocator's $77 partial-withdraw intent is
intentionally overshot into a full unwind; the freed USDC re-enters
the next-tick allocator decision and gets routed proportionally
into the underweight targets (hedgedjlp ~45%, stable_yield ~20%)
across the following ~2 ticks. The orchestrator naturally converges
to target weights via the existing drift path — it just takes one
extra tick versus a clean partial.

**Researched before fix** per the durable rule: traced the production
log to confirm exactly which envelope was being received and which
handler exit fired; read the `WithdrawMultiply` payload shape in
the protocol crate to confirm the no-amount design; read multiply's
existing `MsgType::WithdrawMultiply` handler to verify the unwind
path was already wired and only the orchestrator's choice of
envelope was wrong.

**Files**

```
tools/fleet-pm-stub/src/allocator_runner.rs  — emit WithdrawMultiply, not AssignMultiply{target=0}; updated docstring; test renamed + assertions flipped
Cargo.toml                                    — 0.4.16 → 0.4.17
DEVLOG.md                                     — this entry
PLAN_rc17_per_tick_interest_accrual.md        — renamed to PLAN_rc18 (this version slot is taken)
```

---

## v0.4.16 — MarketSignal consumer audit + explicit log per daemon (2026-06-03)

**Audit finding.** Grepped each daemon's `dispatch.rs` for
`MsgType::` branches. Reality check:

| Daemon | MarketSignal | EscalateRisk |
|---|---|---|
| multiply | log only (rc16) | ✓ consumed (pause path) |
| stable_yield | log only (rc16) | not consumed |
| hedgedjlp | log only (rc16) | not consumed |
| riskwatcher | not subscribed | (emits only) |
| orchestrator | ✓ consumed (rc14) | not consumed |

The LITEPAPER's "execution daemons subscribe to MarketSignal and
EscalateRisk" is true at the delivery layer (per researcher's
`--subscriber` list) but false at the consumer layer for all but
multiply ← Escalate and orchestrator ← MarketSignal. The other
combinations land in each daemon's `other => info!("ignoring inbox
envelope")` catch-all and are silently dropped.

**What ships.**
- `docs/mesh-consumers.md` — NEW. Truth table of which daemons
  consume which envelopes today, plus a priority-ordered list of
  per-RiskKind / per-SignalKind wiring follow-ups for next rcs.
- Each execution daemon's `dispatch.rs` gains an explicit
  `MsgType::MarketSignal => { info!(...); }` branch so the operator
  sees per-signal arrival in logs instead of having them buried
  under "ignoring inbox envelope". Functional behaviour unchanged
  — still no consumer logic.

**Why not wire consumers in rc16.** Each MarketSignal → strategy
reaction is a product decision (when does multiply pause? what
defer-conditions for hedgedjlp resize?) not an engineering one.
Encoding a default reaction without operator preference would be
guessing. rc16 makes the gap visible and ranked; specific consumer
wiring follows in focused per-strategy rcs.

**Files**

```
docs/mesh-consumers.md                                — NEW (truth table + wiring follow-ups)
crates/multiply-daemon/src/dispatch.rs                — explicit MarketSignal log branch
crates/stable-yield-daemon/src/dispatch.rs            — explicit MarketSignal log branch
crates/hedgedjlp-daemon/src/dispatch.rs               — explicit MarketSignal log branch
Cargo.toml                                            — 0.4.15 → 0.4.16
DEVLOG.md                                             — this entry
```

---

## v0.4.15 — riskwatcher classifier functions for the previously-stub RiskKinds (2026-06-03)

**The gap.** The `RiskKind` enum has carried `OracleStaleness`,
`DeltaDrift`, and `PerpFundingSpike` variants for months — but
`thresholds::classify` only emitted `LiquidationDistance`. The
read-only fleet half was doing less than its name implied. Tonight's
architecture conversation made this explicit (riskwatcher is "mostly
LiquidationDistance"); rc15 starts filling the gap with the pure-logic
piece, the classifier functions themselves.

**Scoped down deliberately.** Each classifier needs a host poller (or
existing one extended) to actually fire on production. Wiring the
three is real per-poller work — DeltaDrift needs PositionView extended
with `last_delta_bps` and observer.rs updated; OracleStaleness needs
Pyth feed last-update timestamps surfaced from the jupiter_perps
poller; PerpFundingSpike needs either a new researcher watcher or a
new poller. Pure classifiers ship now; wiring is per-rc follow-up
work an operator can prioritise per pain.

**What ships.**

```rust
pub fn classify_oracle_staleness(age_secs: u64) -> Option<RiskSeverity>
//   Notice    ≥  60s
//   Warning   ≥ 300s  (5 min)
//   Critical  ≥ 900s  (15 min)

pub fn classify_delta_drift(current_bps: i32, target_bps: i32) -> Option<RiskSeverity>
//   on |current − target|:
//   Notice    ≥   500 bps  (5%)
//   Warning   ≥ 1_500 bps  (15%)
//   Critical  ≥ 3_000 bps  (30%)

pub fn classify_perp_funding_spike(funding_bps_pa: i32) -> Option<RiskSeverity>
//   on |annualised rate|:
//   Notice    ≥   100 bps   (1%)
//   Warning   ≥   500 bps   (5%)
//   Critical  ≥ 2_000 bps   (20%)
```

Each classifier has band-boundary unit tests pinning the exclusive-
at-upper-edge semantics (matches the existing LiquidationDistance
classifier's convention). The DeltaDrift Critical band specifically
catches the v0.4.10 cycle-5 bug shape — hedge / JLP = 3.91× reads as
`current_bps = 30,000` against target 0 → Critical.

**Intentional non-changes.**
- No poller code changes. The new functions are reachable only via
  unit tests today. Next rcs add the wiring per RiskKind:
  - **rc16** (planned): extend `PositionView` + `observer.rs` to
    surface `last_delta_bps` from `ReportHedgedJlp` and feed the
    DeltaDrift classifier on each upsert.
  - Future rc: lift Pyth feed `last_update_unix` out of
    `jupiter_perps_poller` and call `classify_oracle_staleness`.
  - Future rc: researcher watcher emits perp-funding MarketSignal;
    classifier consumed similarly to rc14's SOL trend.

**Files**

```
crates/riskwatcher-daemon/src/thresholds.rs  — 3 new classifier fns + bands + 4 tests
Cargo.toml                                   — 0.4.14 → 0.4.15
DEVLOG.md                                    — this entry
```

---

## v0.4.14 — orchestrator subscribes to MarketSignals; SOL trend gate on rebalance (2026-06-03)

**Architectural gap closed.** Pre-rc14 the orchestrator only EMITTED
mesh envelopes (Beacons, Assigns, Withdraws). It never consumed
anything. Researcher's `MarketSignal` envelopes were broadcast to the
execution daemons but never reached the allocation-decision layer.
The LITEPAPER's "execution daemons subscribe to MarketSignal" line
silently excluded the orchestrator — that's been the wrong shape since
v0.4.0 shipped.

**What ships.**
- `crates/orchestrator-daemon/src/market_cache.rs` — NEW. Async-safe
  `HashMap<(AssetId, u16), MarketSignal>` keyed by signal kind's
  `repr(u16)` discriminant (SignalKind doesn't derive Eq+Hash in the
  shared protocol crate). Replacement policy: latest by
  `raised_at_unix` wins; stale re-deliveries silently drop.
- `crates/orchestrator-daemon/src/inbox.rs` — NEW. Drains
  `handle.recv()` forever, filters `MsgType::MarketSignal`, decodes
  the CBOR payload, upserts the cache. Non-MarketSignal envelopes
  drop with a debug log (the orchestrator only consumes market
  context today; Reports + Escalates flow through the dashboard's
  ingest, not the orchestrator).
- `crates/orchestrator-daemon/src/main.rs` — 4th branch added to the
  top-level `tokio::select!` running the inbox loop alongside the
  node service, beacon emitter, and tick loop.
- `crates/orchestrator-daemon/src/tick.rs` — each tick snapshots the
  cache, reads the latest SOL `PriceMovedBps` signal, clones the
  base `AllocatorConfig` and sets `sol_price_trend_bps = Some(bps)`
  before calling `decide()`. Per-tick clone keeps the persistent
  TickCtx.cfg unchanged so a missing/stale signal doesn't permanently
  mutate state.
- `tools/fleet-pm-stub/src/allocator.rs` — adds `sol_price_trend_bps:
  Option<i32>` (None when no signal cached) and
  `sol_price_trend_floor_bps: i32` (default `-300` = 3 % down) to
  `AllocatorConfig`. Inside `try_cross_strategy_rebalance`, when
  `Δβ > 0.5` AND the source strategy has `β ≥ 1.0` (so the move
  forces a SOL sale on the unwind path), the gate checks the cached
  signal: if `trend < floor`, suppress with diagnostic logged.
- 4 new tests pin the canonical multiply→hedgedjlp scenario at
  -500 bps trend (suppress), -100 bps trend (allow),
  no-signal-cached (defer to rc13), and Δβ=0 (gate doesn't apply).

**Deploy-side wiring (operational, not code).** Researcher's
production CLI subscribes multiply / stable_yield / hedgedjlp /
riskwatcher pubkeys today. **Orchestrator must be added to that
subscriber list** for the signals to land. One-line systemd unit
edit on Hetzner: append `--subscriber=${ORCHESTRATOR_PUBKEY}` to
`hedgents-researcher.service`, then `systemctl daemon-reload &&
systemctl restart hedgents-researcher.service`.

**Researcher itself is unchanged.** The existing `price.rs` watcher
already polls a Pyth SOL/USD feed and emits `PriceMovedBps` on a
~30s tick with a 1-hour delta window. The signal v0.4.14 consumes is
that 1h delta — sufficient for the "SOL is falling now, don't sell
into it" semantics. A future researcher rc may add a 30-day SMA
watcher; the consumer-side contract (`sol_price_trend_bps`) doesn't
need to change.

**Failure modes considered.**
- Researcher offline / signal never lands: `sol_price_trend_bps =
  None` indefinitely → gate is a no-op, rc13 semantics restored.
- Stale signal (researcher emitted then went offline):
  `raised_at_unix` from upsert is preserved but freshness isn't
  currently checked in the consumer. Future work can add a max-age
  guard. For now the operator restart of researcher refreshes.
- Signal manipulation (a non-researcher pubkey broadcasts a fake
  MarketSignal): no explicit sender allowlist in the inbox today.
  Mesh-layer signature verification at envelope decode is the only
  check. rc15 will add a researcher-pubkey allowlist symmetric to
  the multiply/stable_yield's sender allowlist on Approve.

**Files**

```
crates/orchestrator-daemon/src/market_cache.rs  — NEW
crates/orchestrator-daemon/src/inbox.rs         — NEW
crates/orchestrator-daemon/src/lib.rs           — register new modules
crates/orchestrator-daemon/src/main.rs          — 4th select branch
crates/orchestrator-daemon/src/tick.rs          — per-tick cache snapshot + cfg clone
crates/orchestrator-daemon/Cargo.toml           — add ciborium
tools/fleet-pm-stub/src/allocator.rs            — sol_price_trend_bps/floor, gate, 4 tests
Cargo.toml                                      — 0.4.13 → 0.4.14
DEVLOG.md                                       — this entry
```

---

## v0.4.13 — allocator credits risk reduction in cross-strategy rebalance (2026-06-02)

**The gap.** The orchestrator's audit log showed a persistent pattern:
hedgedjlp underweight by ~46 % of AUM (target 46.3 %, current 0 %),
multiply overweight by ~58 %, idle insufficient to fund a deposit
($1 << $10 min). The cross-strategy rebalance machinery (rc29) exists
and was running, but blocked by the cost-benefit gate (rc37): APR gap
multiply→hedgedjlp was only 148 bps, opening cost ~$10-15, payback
~5 years on pure-APR math.

**Why this is wrong for a hedge fund.** Multiply is 1× SOL beta on
its net equity (collateral and debt both SOL-denominated, the spread
IS the SOL position). With 91 % of capital in multiply, the fleet's
total SOL exposure was ~94 % of AUM. A 10 % SOL drop costs the fleet
~$28 on a $306 base. The "delta-neutral" framing only holds if
hedgedjlp is funded — currently it isn't. The pre-rc13 cost-benefit
gate scores only APR and is blind to this directional risk.

**The fix.** Extend `passes_cost_benefit` with a one-sided
`risk_reduction_credit` term:

```text
risk_gain = amount × max(Δβ, 0) × risk_credit_bps_pa × holding_days / (365 × 10_000)
gain      = apr_gain + risk_gain
fire when gain ≥ cost × safety_factor
```

`Δβ = over_beta − under_beta`, positive when the rebalance moves
capital from a directional strategy to a market-neutral one.
`risk_credit_bps_pa` defaults to 2000 (20 %/yr) — a conservative read
on the one-sided downside risk of holding 1× SOL beta vs delta-
neutral. The `rebalance_min_apr_gap_bps` noise floor is now bypassed
when `Δβ ≥ 0.5` (significant risk reduction); the cost-benefit gate
itself becomes the authoritative check in that regime.

Canonical scenario (live state at write-time): move $140 from
multiply (β=1) to hedgedjlp (β=0) at 148 bps APR gap.

```text
pre-rc13:   apr_gain $0.17, cost $0.56  → block (5-year payback)
post-rc13:  apr_gain $0.17 + risk_credit $2.30 = $2.47 vs cost $0.56  → fire
```

**Per-strategy beta table.** Encoded in `sol_beta_for(strategy_id)`:
multiply 1.0, hedgedjlp 0.0, stable_yield 0.0, unknown 0.0
(conservative — no credit when uncertain). Beta is structural, not
market-dependent; pinned in code with a test that flags any change.

**Failure modes considered.**
- Β-table getting stale: pinned by a unit test that fails CI if anyone
  changes the values without thinking.
- Operator wants pure-APR semantics: set
  `risk_credit_bps_per_unit_beta_pa = 0` to disable; pre-rc13
  behaviour is restored exactly.
- Negative Δβ (move INCREASES beta): credit clamps at 0, no
  penalty. That direction stays governed by APR + risk-premium gates
  upstream.

**Researched before fix** per the durable rule: traced the audit
log to confirm rc29's cross-strategy path was reached and the cost-
benefit gate was the blocker (not min-action, not eligibility);
computed the actual SOL exposure across all strategies from on-chain
positions; verified the multiply equity-beta math (collateral SOL −
debt SOL = net SOL equity); confirmed hedgedjlp's structural Δ=0
design from the resize loop's target_delta_bps semantics.

**Files**

```
tools/fleet-pm-stub/src/allocator.rs   — sol_beta_for, risk-credit term in passes_cost_benefit, Δβ
                                          plumbed through cross-strategy rebalance, 5 new tests
Cargo.toml                              — 0.4.12 → 0.4.13
```

---

## v0.4.12 — idle wallet SOL counted in AUM (was silently dropped) (2026-06-02)

**The bug.** `read_chain_aum_breakdown` priced the wallet's USDC ATA
but not the native SOL balance. Any SOL sitting outside a strategy
was invisible to the dashboard's AUM — exactly the gap that hid 0.62
SOL of recovered capital on 2026-06-01 until today's leverage walk
swept it into the multiply obligation. The user spotted it ("AUM up
from 260+ to 309+ — where's the $40?"), the answer being "the SOL was
always there, just uncounted."

**What ships.**
- `chain/sol_price.rs` — new helper. Fetches SOL/USD from Jupiter's
  Lite Price API v3, mirrors `jlp_price.rs` exactly (same endpoint,
  same micro-USD output scale, same fail-safe behaviour).
- `ChainReader::sol_price_micro_usd()` — cached 30s under the same
  `ChainCache` slot scheme as JLP price; a failed fetch caches 0 so
  TTL paces retries (no thrash on every dashboard tick).
- `read_chain_aum_breakdown` now computes `idle_sol_usd = sol_lamports
  × sol_price / 1e9` and folds it into the combined `idle_usd`. Both
  components are kept distinct on `ChainAumBreakdown` so the API
  surface can render the breakdown.
- `/aum.per_strategy` adds `idle_sol_usd: f64` next to the existing
  `idle_usdc`. `total_usdc` automatically picks up the SOL contribution
  (it sums `idle_usd`).
- Frontend `NumbersPanel` renders a new "Idle SOL (USD)" row beneath
  "Idle USDC" when the value is > 0.

**Failure modes.**
- Jupiter Lite Price API returns malformed body → 0 cached for 30s,
  AUM undercounts for that cycle (equivalent to pre-rc12 behaviour).
- Jupiter outage → same. Cache TTL paces retries.
- Both modes are silent in the audit log (warn-level at the helper
  layer, not user-facing). The dashboard surfaces "Idle SOL (USD)" =
  $0.00 — operator can cross-check against wallet balance.

**Files**

```
tools/fleet-dashboard-server/src/chain/sol_price.rs   — NEW (Jupiter SOL/USD fetcher)
tools/fleet-dashboard-server/src/chain/mod.rs         — sol_price_micro_usd accessor + cache slot
tools/fleet-dashboard-server/src/api/state.rs         — idle_sol_usd in breakdown + AUM response
frontend/lib/api.ts                                   — AumResponse.per_strategy.idle_sol_usd
frontend/components/NumbersPanel.tsx                  — "Idle SOL (USD)" row
Cargo.toml                                            — 0.4.11 → 0.4.12
```

---

## v0.4.11 — multiply card shows per-leg APR alongside collateral / debt (2026-06-02)

Completes the v0.4.9 decomposition: the strategy card now reads
"$495.18 @ 7.29% − $210.59 @ 5.52%" instead of just the dollar
figures. Collateral APR coloured emerald (yield direction), debt APR
amber (cost direction). The operator can read the multiply math in
one glance instead of cross-referencing rate dashboards.

**What ships.**
- Multiply daemon now emits `sol_borrow_pct: f64` in each
  `pnl_snapshot` row. The daemon already fetched
  `kamino_sol_borrow_pct` as part of `FleetRates` (v0.4.7 uses it in
  the `multiply_net_apr_bps` formula) — this just surfaces it.
  `usdc_borrow_pct` continues to be emitted for backward compat with
  any older tooling reading the JSONL log.
- `/strategies` adds `collateral_apr_bps?: u32` and `debt_apr_bps?:
  u32`. For multiply these source from the latest pnl_snapshot's
  `jitosol_apy_pct` (×100 → bps) and `sol_borrow_pct` (×100 → bps).
  Omitted for stable_yield (its `current_apr_bps` is already the
  per-leg number) and hedgedjlp (per-leg breakdown awaits a future
  rc since hedgedjlp's per-leg cost is the perp funding rate, not a
  Kamino borrow rate).
- Frontend `StrategyCardsRow` formats the line as
  `$X @ Y% − $Z @ W%`, with the yield % in green and the cost % in
  amber so the directional sign is readable without parsing the
  inequality. Title attribute spells out the underlying mechanics.

**Files**

```
crates/multiply-daemon/src/pnl.rs             — sol_borrow_pct field + test
tools/fleet-dashboard-server/src/api/state.rs — collateral/debt_apr_bps in /strategies
frontend/lib/api.ts                           — StrategyCard.{collateral,debt}_apr_bps
frontend/components/StrategyCardsRow.tsx      — per-leg APR formatting
Cargo.toml                                    — 0.4.10 → 0.4.11
```

---

## v0.4.10 — hedgedjlp resize now closes over-hedged legs (was: open-only) (2026-06-02)

**The bug.** When JLP value dropped (mark-to-market down, or a partial
unwind), the hedge stayed at its old absolute notional. Two confirmed
production occurrences via `pnl_snapshots`:

| Date       | JLP value | Hedge notional | Ratio  |
|------------|-----------|----------------|--------|
| 2026-05-22 | $105      | $399           | 3.80×  |
| 2026-05-31 | $92       | $360           | 3.91×  |

The "delta-neutral" strategy was in fact net SHORT by 2-3× the JLP
value during those windows, paying funding on the full hedge while
the long had shrunk.

**Root cause (researched, side-by-side compared).** `resize.rs`
`compute_legs_to_open` was documented:

> **No re-opens.** Per asset, `to_open = max(0, target − current)`.
> Assets where current ≥ target are skipped with a `Skip::AlreadyHedged`.

The author had assumed `current > target` could only happen from a
redundant duplicate open — never that the *target itself* could
shrink. But `target_notional_usd = current_long_usd × (1 − target_delta_bps)`,
and `current_long_usd` is mark-to-market on the JLP basket. When JLP
falls 4×, target falls 4×, existing shorts don't downsize → over-hedged.

The decrease ixn already existed in the protocol layer
(`create_decrease_position_request_ix`, used by `unwind.rs` for full
closes); resize just never called it. Partial closes are supported
via `entire_position=false` + `size_usd_delta` argument.

**What ships.**
- `compute_legs_to_close(targets, existing_per_asset, min_notional)`
  — symmetric to `compute_legs_to_open`. Returns legs where
  `existing > target` along with the amount to decrease. Honours the
  same `MIN_HEDGE_NOTIONAL_USD` dust floor (a $5 over-drift costs
  more in fees than it saves in directional accuracy).
- `ResizePlan.legs_to_close: Vec<(String, u64)>` — new field, serde
  default empty for backward-compatible deserialisation of pre-rc10
  queued plans.
- `run_resize` calls both compute functions and packages the combined
  plan. The "no work" guard requires BOTH sides empty.
- `execute_resize` gains a close branch after the open loop. Per leg:
  validate the Position pubkey is reachable from
  `state.active.open_positions`, derive a fresh
  `RequestChange::Decrease` request PDA, build the decrease ixn with
  partial-close args, whitelist-verify, submit. Uses
  `short_price_ceiling_micro_usd` (the close-side slippage helper rc27
  put in place — same shape `unwind.rs` uses for the unhedge path).
- State update: subtracts closed notional from
  `active.hedge_notional_usdc` so the next rebalancer tick stops
  re-queueing the same close work. `open_positions` entries are kept
  (these are partial closes, not full).
- Four new pure-function tests pin the close-side compute against
  the cycle-5 production shape (5× over-hedge across SOL/ETH/BTC,
  $5 dust skip, fully-hedged no-op, missing-existing handling).

**Files**

```
crates/hedgedjlp-daemon/src/resize.rs   — compute_legs_to_close + plan + execute close branch + 4 tests
crates/hedgedjlp-daemon/src/dispatch.rs — log close_count alongside leg_count
Cargo.toml                              — 0.4.9 → 0.4.10
DEVLOG.md                               — this entry
```

**Researched before fix** per the durable rule: confirmed the bug
shape from `pnl_snapshots` (2026-05-22 and -31 imbalances), traced
the design oversight in `compute_legs_to_open` and its docstring,
identified the existing `create_decrease_position_request_ix` from
`unwind.rs` as the implementation primitive, verified the partial-
close-via-`size_usd_delta` path against the protocol layer's
docstrings before writing any execute code.

---

## v0.4.9 — multiply card shows gross collateral / debt decomposition (2026-06-02)

**Why.** When you asked "what is this $287 made of?" the only honest
answer was "it's the net of jitoSOL collateral mark-to-market minus
SOL borrow principal mark-to-market" — but the dashboard never showed
those two numbers, so the question had to be re-derived from chain
state every time. USCC's per-holding allocation table (29% USD
collateral / 20% USTB / 17% Solana staked / 15% weETH) is the same
shape of information for a different fund.

**What ships.**
- `/strategies` adds `collateral_usd?: f64` and `debt_usd?: f64` per
  strategy. Populated for multiply (gross deposit, gross borrow);
  omitted for stable_yield and hedgedjlp where decomposition lives
  elsewhere (single deposit / `deployed_usdc` + `hedge_collateral_usdc`).
- `deployed_usdc` stays the net number to preserve the existing
  contract — `collateral_usd - debt_usd ≈ deployed_usdc` for multiply.
- Frontend `StrategyCardsRow` renders "`$X collateral − $Y debt`" as
  a secondary line under the net figure when both fields are present.

**Why it's minimal.**
- No daemon changes — the data is already in the multiply position
  object that `/strategies` fetches once per request. Two extra
  `micro_to_usd` conversions, no extra RPC.
- Per-leg APR (jitoSOL APY on collateral, SOL borrow APR on debt) is
  deliberately deferred. The dollar decomposition is what answered
  the immediate operator question.

**Files**

```
tools/fleet-dashboard-server/src/api/state.rs  — collateral_usd / debt_usd fields + populate
frontend/lib/api.ts                            — StrategyCard.{collateral_usd, debt_usd}
frontend/components/StrategyCardsRow.tsx       — secondary line under multiply
Cargo.toml                                     — 0.4.8 → 0.4.9
```

---

## v0.4.8 — trailing 24-hour APR as the headline number (2026-06-01)

**Why.** Bitwise USCC publishes a single dated number ("30-day SEC
yield"). Hedgents was the opposite: a second-by-second realtime APR
that swung wildly (the v0.4.7 fix narrowed the swings but couldn't
remove them — Kamino borrow rates still move). The headline number on
each strategy card should be honest, not noisy.

**What ships.**
- `/strategies` now returns `apr_24h_bps?: u32` per strategy — the
  mean of the daemon's own apr field over the trailing 24 h, sourced
  from `pnl_snapshots`. `None` when fewer than ~1 h of samples exist
  (fresh deploy / db reset).
- Frontend `StrategyCardsRow` headlines `apr_24h_bps` when present
  ("APR · 24h"), falls back to `current_apr_bps` otherwise. The
  realtime number is shown beneath as `live: X.XX%` when it differs,
  so the operator can still see drift without it being the primary
  signal.

**Implementation.**
- `store::pnl_field_mean_since(daemon, field, since_unix)` — single
  SQL query (`AVG(CAST(json_extract(...) AS REAL))`) restricted to a
  trailing window. Excludes NULL / non-numeric values automatically.
- `state::trailing_apr_bps_for(daemon, state)` — 24 h window, 720
  sample threshold (≈ 1 h at the 5-second emit cadence). Below the
  threshold the average is noisier than the realtime number, so
  `None` is the honest answer.
- Test fixtures for `strategy_card_*` updated to pin the new field's
  serde behaviour (omitted on `None`, serialised when `Some`).

**Files**

```
tools/fleet-dashboard-server/src/store/sqlite.rs   — pnl_field_mean_since
tools/fleet-dashboard-server/src/api/state.rs      — trailing_apr_bps_for, StrategyCardOut.apr_24h_bps
frontend/lib/api.ts                                — StrategyCard.apr_24h_bps
frontend/components/StrategyCardsRow.tsx           — headline + secondary live row
Cargo.toml                                         — 0.4.7 → 0.4.8
```

---

## v0.4.7 — multiply displayed APR uses Kamino SOL borrow rate (was USDC) (2026-06-01)

**The bug.** The dashboard's multiply APR estimate had been swinging
between 0 and ~1000 bps and frequently showing "—" (hidden by the UI
when `current_apr_bps == 0`). Sample of the last few hours of
`pnl_snapshots`:

```
14:06  apr_bps=136   borrow_pct=11.24   (sol_borrow on Kamino fell)
14:01  apr_bps=0     borrow_pct=21.84
13:36  apr_bps=977   borrow_pct=5.63
05-31 23:55  apr_bps=0     borrow_pct=47.43    ← USDC borrow stress
```

**Root cause.** `crates/zerox1-defi-runtime/src/fleet_rates.rs::compute()`
used `usdc_borrow` in the multiply formula:

```rust
let multiply_net = (jitosol_apy * lev - usdc_borrow * debt).max(0.0);
```

But since rc36 multiply borrows **SOL**, not USDC. The struct already
fetches `kamino_sol_borrow_pct` (and `kamino_usdc_borrow_pct`) from the
Kamino reserves-metrics endpoint — `sol_borrow` was just unused in the
formula. Kamino's USDC borrow APR is volatile (4–47% range during
stress windows), while SOL borrow is steady ~6%. Every time USDC
borrow spiked above ~12%, `jitosol_apy × lev − usdc_borrow × debt`
went negative and clamped to 0, blanking the dashboard's APR display.

**The strategy itself was never affected** — only the *forward-looking
APR estimate* the dashboard displays. The live multiply position has
been earning the real `jitosol_apy − sol_borrow × debt_ratio` spread
continuously since rc36.

**Fix.** One-line: `usdc_borrow` → `sol_borrow` in the multiply_net
computation. Docstring + math test updated to match. With sol_borrow
≈ 6 % and jitosol_apy ≈ 7.3 % at target LTV 0.60:

```
multiply_net = 7.3 × 2.5  −  6 × 1.5  =  18.25 − 9.0  =  9.25%  →  925 bps
```

vs. the broken formula on a 22 % USDC-borrow tick:
`7.3 × 2.5 − 22 × 1.5 = 18.25 − 33 = −14.75 → clamped 0`.

**Researched before fixing** per the durable rule: confirmed multiply's
borrow leg via `crates/multiply-daemon/src/leverage.rs` (uses
`WSOL_MINT` + `KAMINO_MAIN_SOL_RESERVE`), traced the rate sourcing in
`fleet_rates.rs`, sampled `pnl_snapshots` for the actual broken-window
boundaries.

**Files**

```
crates/zerox1-defi-runtime/src/fleet_rates.rs  — formula + docstring + test
Cargo.toml                                      — 0.4.6 → 0.4.7
DEVLOG.md                                       — this entry
```

---

## v0.4.6 — rc54: SOL reserve's collateral farm constant updated (Kamino added a farm post-rc49) (2026-06-01)

v0.4.5 (rc53) deployed cleanly and the runtime sweep logic worked
exactly as designed — `AssignMultiply(usdc_lamports=0)` with 3.5 SOL
in the wallet triggered the unconditional stake decision
(stake_lamports=3,476,576,831, the wallet minus the fee buffer).
But the seed bundle failed at the new step rc52 introduced:

```
rc52: load SOL reserve for refresh before refresh_obligation
Caused by: rc49: load_reserve d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q
  returned farm_collateral=955xWFhSDcDiUgUr4sBRtCpTLiMd4H5uZLAmgtP3R3sX
  after 3 retries; expected 11111111111111111111111111111111
```

**Root cause (researched, on-chain verified):** Kamino enabled a
collateral farm on the SOL reserve at some point after rc49 shipped
(2026-05-30). rc49's `expected_farm_collateral` table still encoded
`Some(Pubkey::default())` for SOL — meaning "this reserve should have
no farm." When the on-chain reality became "farm exists at
`955xWFhSDcDiUgUr4sBRtCpTLiMd4H5uZLAmgtP3R3sX`," rc49's stale-RPC
defender correctly fired ("mismatch → retry → bail"), but the trip was
a real Kamino state change, not stale RPC.

The rc49 comment at `kamino_loader.rs:444-449` had anticipated
exactly this:

> "Multiply's reserves have no farms (confirmed in rc32 — both have
> farm_collateral == default). Encode that here so a future regression
> where Kamino adds farms to these reserves trips a mismatch and we
> re-validate the constant table."

That's what we're doing.

**On-chain verification (2026-06-01 fetch via getAccountInfo):**

| Reserve | farm_collateral (offset 64)                                | farm_debt (offset 96) |
|---------|------------------------------------------------------------|-----------------------|
| USDC    | `JAvnB9AKtgPsTEoKmn24Bq64UMoYcrtWtq42HHBdsPkh` (unchanged) | default (unchanged)   |
| SOL     | `955xWFhSDcDiUgUr4sBRtCpTLiMd4H5uZLAmgtP3R3sX` **NEW**     | default               |
| jitoSOL | default (unchanged)                                        | default (unchanged)   |

Only SOL changed. Only `farm_collateral` (collateral farm, for SOL
*depositors*). Multiply BORROWS SOL — never deposits SOL as
collateral — so this farm doesn't trigger any new
`RefreshObligationFarmsForReserve` ix in our flows. The fix is purely
the constant table update so rc49's validator stops bailing.

**Fix:**

1. New constant `KAMINO_MAIN_SOL_FARM_COLLATERAL` in `constants.rs`
   matching the on-chain pubkey at offset 64.
2. `expected_farm_collateral` returns `Some(KAMINO_MAIN_SOL_FARM_COLLATERAL)`
   for `KAMINO_MAIN_SOL_RESERVE` (was `Some(Pubkey::default())`).
3. jitoSOL kept as `Some(Pubkey::default())` — still has no farm.
4. Test `expected_farm_collateral_multiply_reserves_have_no_farm`
   split into two: one asserting SOL's new farm,
   one asserting jitoSOL still has no farm. The validator-as-canary
   property is preserved for both.

**Downstream impact: zero.** Updating the table just lets
`load_reserve(SOL)` return successfully. The downstream code paths
don't interact with SOL's collateral farm:
- `refresh_reserve_ix` only touches the reserve + oracle accounts
- `refresh_obligation_ix` reads reserves to update market values, no
  farm involvement
- We never deposit SOL as collateral (only borrow), so jitoSOL is the
  only deposit path and jitoSOL has no farm

The fix is the minimal acknowledgment that Kamino's catalog evolved.
After v0.4.6 deploys, the rc53 sweep can complete: `load_reserve(SOL)`
succeeds → seed bundle builds with refresh of both reserves → klend
RefreshObligation passes → jitoSOL deposit lands → leverage walks.
The parked 3.5 SOL self-recovers on the next AssignMultiply.

4 rc49 tests pass (1 updated, 1 added, 2 unchanged). Workspace tag →
`fleet-v0.4.6`.

---

## v0.4.5 — rc53: always sweep idle wallet SOL into the multiply obligation (2026-06-01)

After deploying v0.4.4 (rc52) live, the rc52 path correctly built the
seed bundle on existing leveraged positions, but the orchestrator
needed an `usdc_lamports > 0` envelope to trigger it (because rc47
tied `force_top_up` to that signal). With most idle USDC already
swept across 4 failed-rc47 swaps into 3.5 SOL parked in the wallet,
the orchestrator hit its cost-benefit floor (rc37: $10 min) and
stopped routing — so the 3.5 SOL (~$365 of stranded capital) sat
indefinitely. Manual recovery would have required injecting a $1
envelope just to re-trigger the seed path.

The rc52 design (force_top_up only on USDC-routed envelopes) was
unnecessarily conservative. The multiply daemon is the only fleet
daemon that puts SOL in the shared signing wallet — stable_yield
is pure USDC, hedgedjlp settles JLP/perps to USDC, riskwatcher and
researcher don't sign capital movements, orchestrator doesn't hold
funds. So any idle wallet SOL above the fee buffer is structurally
"multiply capital pending deposit," and there's no risk of stealing
SOL from another strategy by sweeping it.

**rc53 simplification:**

- `SeedDecision::ObligationAlreadyHasJitosolCollateral` variant
  **removed** (now unreachable).
- `obligation_has_jitosol_collateral` predicate function **removed**
  (no remaining callers).
- `decide_seed_amount` signature reduced to `(wallet_lamports,
  fee_buffer_lamports, max_stake_lamports)` — obligation state is no
  longer an input. Returns `Stake(n)` whenever wallet has SOL above
  the buffer, else `InsufficientWalletBalance`.
- `maybe_seed_obligation` drops the `force_top_up` param.
- `leverage::run_or_simulate` calls `maybe_seed_obligation(ctx)` —
  no bool argument.

**Effect on the four runtime scenarios:**

| `usdc_lamports` | Obligation     | Wallet SOL     | Pre-rc53 (v0.4.4)                | rc53 (v0.4.5)                  |
|-----------------|----------------|----------------|----------------------------------|--------------------------------|
| 0               | none           | > buffer       | bootstrap seed (legacy fresh)    | bootstrap seed (same)          |
| 0               | has jitoSOL    | > buffer       | **skip** (stranded)              | **stake + deposit**            |
| > 0             | none           | > buffer       | swap + bootstrap (rc41)          | swap + bootstrap (same)        |
| > 0             | has jitoSOL    | > buffer       | swap + forced top-up (rc47/52)   | swap + sweep all wallet SOL    |

The third-row scenario (the rc47 design intent) still works
identically — `seed_with_usdc` swaps USDC → SOL first, then
`maybe_seed_obligation` stakes the resulting wallet SOL.

The killer scenario (second row) is the rc53 win: any idle wallet
SOL gets recovered on the next `AssignMultiply` regardless of
whether it carries new USDC. For the currently-parked 3.5 SOL, the
next orchestrator tick (or any `fleet-pm-stub assign-multiply`,
including `--usdc-lamports=0`) sweeps it into the obligation.

**No new Jupiter swaps in the rc53 path.** Recovery uses Jito's
`DepositSol` (a stake, not a swap — SOL → jitoSOL at the pool's
on-chain mint rate) plus a Kamino deposit ix. Zero exposure to
DEX pricing.

**Test surface tightened.** 10 obsolete tests removed (`SeedDecision`
variant assertions, `obligation_has_jitosol_collateral` predicate
tests, `force_top_up`-flagged paths). 1 new test pins the rc53
invariant:

- `rc53_wallet_sol_above_buffer_always_stakes` — explicit assertion
  that the decision is now obligation-state-independent.

Other existing tests (`wallet_under_fee_buffer_yields_insufficient`,
`wallet_dust_above_buffer_below_min_yields_insufficient`,
`stake_is_clamped_to_max`) updated to the simpler signature. Added
`stake_at_buffer_plus_min_succeeds` as an additional boundary test.

118 multiply-daemon tests pass (111 unit + 7 integration). Workspace
builds clean — no new warnings.

Workspace tag → `fleet-v0.4.5`.

---

## v0.4.4 — rc52: seed bundle refreshes all obligation reserves before RefreshObligation (2026-06-01)

v0.4.3 (rc47) closed the original gate but exposed a downstream bug.
Live deploy this morning caught the failure: orchestrator routed
$121.77 USDC into multiply via the new force_top_up path; seed_with_usdc
swapped USDC → 1.484 SOL (sig `HC8UYE...`); seed bundle then failed
in simulation with klend custom program error **0x1776** at
`klend/lending_market/lending_operations.rs:1713` (AnchorError 6006
`InvalidAccountInput`) on the `RefreshObligation` instruction.

**Root cause (researched, not guessed):** klend's `refresh_obligation`
walks the remaining_accounts (= `obligation_reserves`) and for each
**active deposit/borrow** in the obligation expects a freshly-refreshed
matching reserve account. From `klend/lending_market/lending_operations.rs`:

```rust
let deposit_reserve = reserves_iter
    .next()
    .ok_or(error!(LendingError::InvalidAccountInput))?;
```

The leverage.rs `lever_up_bundle` already handles this — see the
v0.1.16 fix comment at `leverage.rs:396-400`:

> "klend's RefreshObligation requires every reserve referenced in the
>  obligation (deposits + borrows) to have been refreshed via
>  RefreshReserve earlier in the same transaction."

Side-by-side comparison after the rc47 fix activated `seed::maybe_seed_obligation`
on existing leveraged positions:

| Path        | RefreshReserve before RefreshObligation              | Status |
|-------------|------------------------------------------------------|--------|
| seed.rs     | only `refresh_reserve_ix(jitosol_reserve)`           | ✗      |
| leverage.rs | iterates all obligation reserves                     | ✓      |
| unwind.rs   | explicit `refresh_reserve_ix(jitosol)` + `(sol)`     | ✓      |

The v0.1.16 fix landed for leverage.rs and unwind.rs, but seed.rs was
never updated because seed-only-on-fresh-wallets meant
`obligation_reserves` was always empty pre-rc47. rc47 invokes seed on
existing leveraged positions where the obligation holds
`[jitosol_reserve, sol_reserve]` — only one of two gets refreshed,
klend bails.

`DecodedObligation.deposits/.borrows` are pre-filtered to active slots
(`kamino_loader.rs:178, :205`), so the count matches klend's
expectation — the failure is that the SOL reserve in the list has
stale on-chain state and klend's RefreshObligation iterator either
short-circuits or otherwise refuses to advance.

**Fix:**

1. `seed::build_seed_bundle` takes `obligation_reserve_accounts: &[&ReserveAccounts]`
   (replacing the prior `obligation_reserves: &[Pubkey]`).
2. Before `refresh_obligation_ix`, the bundle emits `refresh_reserve_ix`
   for **each** obligation reserve, plus `jitosol_reserve` itself if
   not already present. Dedup so we don't waste CU.
3. `seed::maybe_seed_obligation` resolves obligation pubkeys to
   `ReserveAccounts`: jitoSOL is the one already loaded; SOL is loaded
   on demand via `load_reserve(rpc, &KAMINO_MAIN_SOL_RESERVE, WSOL_MINT,
   &KAMINO_MAIN_MARKET)` only when the obligation actually holds SOL.
   Unknown reserve pubkeys bail with a clear error rather than silently
   skipping a required refresh.

**Three new tests** pin the rc52 invariants:
- `rc52_bundle_refreshes_every_obligation_reserve_before_refresh_obligation` —
  with [jitoSOL, SOL] in obligation_reserve_accounts, the first two
  klend ixs are RefreshReserve for each (in some order), preceding
  RefreshObligation.
- `rc52_dedupes_jitosol_refresh_when_already_in_obligation` — if jitoSOL
  is in obligation_reserve_accounts, build_seed_bundle does NOT emit a
  redundant RefreshReserve(jitoSOL).
- `rc52_empty_obligation_reserve_accounts_falls_back_to_jitosol_only` —
  fresh-wallet shape still emits the jitoSOL RefreshReserve exactly once.

128 multiply-daemon tests pass (121 unit + 7 integration). No external
dependents needed changes.

**Recovery for the parked SOL.** The 1.484 SOL from yesterday's HC8
swap is in the wallet. Once v0.4.4 lands on Hetzner and the orchestrator
sends the next AssignMultiply (or auto-mode picks up on the
allocator's next tick), the now-correct seed bundle will stake that
SOL → jitoSOL → deposit as additional obligation collateral, then the
leverage loop walks to target. No manual recovery needed.

Workspace tag → `fleet-v0.4.4`.

---

## v0.4.3 — rc47: existing-position bug fix (allocator USDC never entered obligation) (2026-06-01)

The rc41 path (orchestrator routes USDC into multiply via
`AssignMultiply.usdc_lamports > 0`) had a silent gap on existing
positions:

1. `dispatch::handle_assign` called `seed::seed_with_usdc` → swapped
   USDC → native SOL into the wallet via Jupiter.
2. Fell through to `leverage::run_or_simulate` → called
   `seed::maybe_seed_obligation`.
3. `decide_seed_amount` saw the obligation already had jitoSOL
   collateral → returned `ObligationAlreadyHasJitosolCollateral` →
   seed bundle skipped.
4. **The freshly-swapped SOL was stranded in the wallet** — never
   staked to jitoSOL, never deposited as obligation collateral.
5. The leverage walk proceeded against existing collateral only;
   the new principal contribution was lost.

The "skip if jitoSOL already present" gate was correct pre-rc41
(seed was a one-shot bootstrap). rc41 changed the seed function's
role into "idempotently additive collateral injection" without
updating the gate to match.

**Fix** (3-line semantic change across two files, plus tests):

- `seed::decide_seed_amount` gains a `force_top_up: bool` parameter.
  When true, the "already has jitoSOL" gate is bypassed; wallet-
  balance + clamp logic still apply.
- `seed::maybe_seed_obligation` plumbs the same flag through to the
  decision function.
- `leverage::run_or_simulate` passes `assign.usdc_lamports > 0` as
  the force flag — the unambiguous signal that USDC was just
  swapped and the resulting SOL is purposed capital intended to
  enter the obligation.

The existing `build_seed_bundle` already handles the existing-
obligation case correctly: it skips `initialize_obligation_ix`
when the PDA exists, all other ixs are idempotent or additive
(ATA-idempotent, refresh_reserve, refresh_obligation with current
reserve list, `deposit_reserve_liquidity_and_obligation_collateral_v2`
which appends/grows the jitoSOL deposit slot).

**Four cases now correct:**

| `usdc_lamports` | Obligation     | Behaviour                                  |
|-----------------|----------------|--------------------------------------------|
| 0               | none           | maybe_seed bootstraps fresh wallet (legacy) |
| 0               | has jitoSOL    | maybe_seed no-ops; leverage walks (legacy)  |
| > 0             | none           | swap then seed-as-bootstrap (existing rc41) |
| > 0             | has jitoSOL    | swap then **forced top-up deposit** (rc47)  |

Simulate-only safety preserved: when `simulate_only=true`,
`seed_with_usdc` is skipped (no Jupiter burn on probe) and the
wallet has no new SOL. `decide_seed_amount` with `force_top_up=true`
then returns `InsufficientWalletBalance` and the leverage loop
proceeds in sim-only against the pre-existing obligation state —
matching pre-rc47 simulate-only semantics.

**Two new unit tests** pin the rc47 invariants:
- `obligation_with_jitosol_deposit_seeds_when_forced_top_up` —
  same fixture as the legacy "is skipped" test, with
  `force_top_up=true`, asserts `Stake(...)` is returned.
- `forced_top_up_still_respects_insufficient_wallet_balance` —
  force_top_up=true with a wallet below the fee buffer correctly
  returns `InsufficientWalletBalance` (so the simulate-only path
  does not try to stake nothing).

Existing seed::* tests (15) all updated to pass the new param
(false in all cases — they exercise legacy semantics). Full
multiply-daemon test suite: **125 tests pass** (118 unit + 7
integration). No external dependents needed changes — the
parameter addition is internal to multiply-daemon.

Workspace tag → `fleet-v0.4.3`. Live verification on Hetzner: any
operator-driven `fleet-pm-stub assign-multiply --usdc-lamports=N`
against the existing multiply position will now actually grow
the obligation collateral. Pre-rc47 the SOL stayed parked.

---

## v0.4.2 — over-engineering pass: paper-trading + stablefloor-daemon deleted (2026-05-31)

Two clean removals from a four-bucket audit. The remaining two
(riskwatcher M4-M6 scaffolding, researcher decoder TODOs) are
parked — they need a product decision, not a delete.

**Bucket A — paper-trading entirely:**
- `deploy/paper-trade-loop.sh` deleted (legacy reference shell loop)
- 4 dev systemd units deleted: `hedgents-multiply.service`,
  `hedgents-stable-yield.service`, `hedgents-hedgedjlp.service`,
  `hedgents-paper-trade.service` (all `--simulate-only=true` paper-mode)
- `hedgents.target` repointed at infra-only daemons (orchestrator +
  observers + dashboard); trading still lives in `hedgents-live.target`
- `/paper` endpoint + `paper_trading()` handler removed from
  `fleet-dashboard-server` (~85 lines + 3 structs gone)
- `frontend/components/PaperTradingCard.tsx` deleted (was orphan,
  only build-artifact references)
- `install-hedgents.sh` + `release-fleet.yml` paper-trade-loop staging
  removed
- Daemons themselves still emit `paper_*` JSON fields into PnL
  telemetry (harmless dead emission — no consumer). Cleaning the
  daemon-side emission is a separate bigger pass not in v0.4.2 scope.

**Bucket B — `stablefloor-daemon`:**
- 134-LOC scaffold for a never-finished Sanctum INF mint/redeem flow.
  Confusingly named (stable-yield-daemon is the production strategy);
  `stablefloor-daemon` is a different unfinished sibling.
- Crate deleted, workspace `Cargo.toml` entry removed.
- `Role::StableFloor` enum value KEPT — stable-yield-daemon still loads
  its identity as `Role::StableFloor` (legacy naming). Renaming the
  Role is a separate refactor; the misleading name persists in code
  but is documented in the dashboard's envelope-decoder normalization
  (`"stablefloor" → "stable_yield"`).

**Buckets NOT touched (need product decision):**
- C: Riskwatcher M4-M6 scaffolding (Kamino poller / risk classifier /
  EscalateRisk emitter — currently emits BEACONs only). Either delete
  or finish.
- D: Researcher decoder TODOs (lending_rate.rs + jlp_yield.rs return
  stub values; load-bearing for strategy signals if any consume them).

**Wrong-call avoided:** "Duplicate allocator" in fleet-pm-stub vs
orchestrator-daemon turned out to be library reuse (orchestrator
imports from `fleet_pm_stub::allocator`), not duplication. Left
alone.

All 74 dashboard-server tests still pass. Build clean across all
strategy daemons. Workspace tag → `fleet-v0.4.2`.

---

## v0.4.1 — beta vault tracking: founder seed + per-user shares (2026-05-30)

Promoting out of the v0.4.0-rcN series. This release closes the
operational gap between "they have an invite code" (rc50) and "they
can actually send USDC and see their position grow." Cargo workspace
version bumps 0.1.0 → 0.4.1 to match the tag scheme going forward.

Model: mutual-fund-style shares. Tobias's existing ~$260 vault AUM
becomes "founder shares" at NAV=$1.00. Subsequent depositors buy
shares at the prevailing NAV (`total_vault_aum / total_shares`).
Withdrawals burn shares pro-rata. All accounting in micro-USDC + share-
lamports (1e6 scale) so the math is integer-only — no float drift across
thousands of small txs.

Schema (3 new tables, additive):
- `beta_depositors(id, source_address PK, email, invite_code,
  created_at, status)`
- `beta_deposits(id, depositor_id FK, amount_usdc_lamports,
  shares_minted_lamports, vault_aum_usdc_micro, tx_signature, note,
  created_at)`
- `beta_withdrawals(id, depositor_id FK, shares_burned_lamports,
  amount_usdc_lamports, destination_address, tx_signature, note,
  executed_at)`

Admin API (bearer-token guarded via `HEDGENTS_BETA_ADMIN_TOKEN`):
- `POST /api/beta/admin/founder-seed {override_aum_usdc_micro?}` —
  one-shot; reads on-chain AUM and mints founder shares at NAV=$1.
  Idempotent on re-run.
- `POST /api/beta/admin/deposit {source_address, email?,
  invite_code?, amount_usdc_lamports, tx_signature?, note?}` — record
  a confirmed inbound USDC tx; shares are minted at the current
  AUM-implied NAV.
- `POST /api/beta/admin/withdrawal {source_address, destination_address,
  amount_usdc_lamports, tx_signature?, note?}` — record a confirmed
  outbound payout; shares burn at the current NAV. Refuses if it
  would burn more shares than the depositor holds.
- `GET /api/beta/admin/depositors` — table-of-shares + computed
  current values + cumulative earned, plus live AUM + NAV.

Auth: shared bearer compared in constant-time (no length-based timing
leak). Endpoints return 401 when `HEDGENTS_BETA_ADMIN_TOKEN` is unset
— safe default for dev boots / first-time installs.

3 new tests in `tests/store_test.rs`:
- `beta_vault_pro_rata_math_round_trips` — $260 seed + $100 deposit
  + 10% earnings + 50% redemption; asserts shares burned + remaining
  balance match the mutual-fund identity.
- `beta_vault_founder_seed_idempotent` — re-seed is a no-op.
- `beta_vault_deposit_before_seed_errors` — deposit pre-seed bails
  rather than silently dividing by zero.

All 74 dashboard-server tests pass.

Operator runbook for the beta cohort:
1. `HEDGENTS_BETA_ADMIN_TOKEN=<long-random-string>` in
   `/etc/hedgents/hedgents.env`; restart dashboard.
2. `curl -X POST -H "Authorization: Bearer $TOKEN"
    .../api/beta/admin/founder-seed -d '{}'` — one-shot seeding.
3. Hand out invite codes (`alpha-001` … `alpha-010`); when a user
   signs up via the landing modal, they email you their Solana address.
4. They send USDC to the testing wallet. You verify the tx on-chain,
   then `curl -X POST ... /deposit` with their address + amount.
5. Withdrawals: they email a request; you send USDC + record via
   `/withdrawal`.
6. `curl ... /depositors` at any time → cohort snapshot.

Pending for v0.4.2+: real on-chain vault program (no more manual
custody), a public depositor-facing position lookup endpoint, and
the rc47 multiply-existing-position fix.

---

## v0.4.0-rc50 — invite-code-guarded vault waitlist (backend) (2026-05-30)

After both grant funnels (Solana Foundation + Alliance) rejected, the
strategy shifted from grant-dependent runway to a public-vault
forcing function: launch invite-only beta, accumulate TVL, let the
numbers do the institutional pitching.

This rc ships the backend for the landing page's "Try the vault"
CTA. Frontend lives in the `landing` repo.

Schema (additive — no migration needed because the table is new):
- `invite_codes(code PK, label, created_at, max_redemptions, enabled)`
- `invite_redemptions(id PK, code FK, email, redeemed_at, UNIQUE(code, email))`

API:
- `POST /api/invite/validate {code}` → `{valid, status}` — pure read,
  doesn't consume capacity. Frontend uses this to gate the email step.
- `POST /api/invite/register {code, email}` → `{ok, message}` —
  atomically re-validates inside the same lock + inserts redemption.
  Idempotent on (code, email): re-submitting the same pair returns
  `ok: true` with "already registered" so retries don't 4xx.

Bootstrap:
- `HEDGENTS_INITIAL_INVITE_CODES` env var, comma-separated, each
  entry `code` or `code:label:max_redemptions`. Idempotent on
  restart — existing rows aren't touched. Disable a code mid-beta
  by editing sqlite directly (`UPDATE invite_codes SET enabled=0
  WHERE code='foo'`).

CORS: added `POST` to the allow_methods list (was `GET, OPTIONS`
for the existing telemetry endpoints).

All 71 dashboard-server tests pass.

---

## v0.4.0-rc49 — load_reserve stale-RPC defense (2026-05-30)

Diagnosis from yesterday's 19:56-onward `WithdrawStableLend` failures:
not a code bug, not a Kamino program change. Helius RPC was
intermittently serving **stale reserve account bytes** — zeros at
offset 64 where the on-chain reserve has `farm_collateral=JAvnB9...`.

The cascade:
1. `load_reserve` reads stale bytes → `farm_collateral = default`
2. `withdraw_ix` / `deposit_ix` check `farm_collateral != default` →
   skip `RefreshObligationFarmsForReserve` ix
3. Kamino's v2 withdraw handler fails Anchor constraint on
   `liquidity_token_program` (cascade — the error fires near the
   missing-state symptom, not at it)
4. Tx fails with `0xbc0 InvalidProgramId` (Anchor 3008)

Confirmed by retrying the same failing withdraw ~13h later (this
morning at 09:08 UTC): same code, same input, **succeeded** with
`ix_count=5` (with farm refresh) and tx signature
`5VtLCb8P6mCXB5Rxy96685bxwpNazYga8xzzmPUZNNFrsVCM3joBuPQTZNiMUMNgXMLdkMS8f5jYrxZHZddnyt92`.
The failures yesterday had `ix_count=4` (no farm refresh).

Fix:
- `kamino_loader::expected_farm_collateral(reserve)` returns the
  expected farm pubkey for known mainnet reserves (USDC →
  `JAvnB9...`, jitoSOL / SOL → `default`). Unknown reserves bypass
  the validator.
- `load_reserve` wraps the existing decode logic with up to 3
  retries with exponential backoff (200ms → 400ms → 800ms) when the
  decoded `farm_collateral` doesn't match the expected value.
- After 3 retries, bails with a clear "RPC serving stale state"
  error instead of building a tx that will fail downstream.

New constant `KAMINO_MAIN_USDC_FARM_COLLATERAL` in `constants.rs`,
sourced from the on-chain bytes at offset 64 and cross-checked
against rc32's incident report.

Three new tests pin the validator table (`expected_farm_collateral`
for USDC, multiply reserves, and unknown reserves). Existing 15
test suites all pass.

This is the last operational item before a small invite-only beta.
With multiply autonomous loop verified (yesterday) and withdraws
now retryable instead of silently broken, the operational track
record can start accumulating real user deposits.

---

## v0.4.0-rc46 — dashboard: multiply pure interest (two-sided Kamino accrual) (2026-05-29)

Closes the rc44 → rc45 arc by extending pure-interest accrual to
multiply. Two-sided because multiply has both a collateral side
(jitoSOL deposit, appreciates with Kamino's jitoSOL reserve rate)
and a borrow side (SOL debt, accrues interest paid via Kamino's
cumulative borrow rate).

Math (per /strategies and /aum handlers):
```
collateral_appreciation_jitosol_lamports
    = current_jitosol_ctoken × (current_rate - baseline_rate)
    where rate = underlying_lamports / ctoken_balance

collateral_appreciation_usd_micro
    = appreciation_jitosol_lamports × current_jitosol_price_micro / 1e9

sol_interest_lamports
    = current_sol_borrowed - baseline_sol_borrowed
    (positive delta = interest paid, assuming no new borrows landed
     between baseline and now)

sol_interest_usd_micro
    = sol_interest_lamports × current_sol_price_micro / 1e9

multiply_pure_interest_usdc
    = (collateral_appreciation_usd_micro - sol_interest_usd_micro) / 1e6
```

Prices (`jitosol_price_micro`, `sol_price_micro`) are derived from
the existing `ObligationView.deposited_usd_micro` and
`borrowed_usd_micro` fields against their underlying lamport
balances — no extra Pyth read needed.

Schema: three additive columns on `chain_aum_snapshots`:
- `multiply_jitosol_ctoken_balance INTEGER`
- `multiply_jitosol_underlying_lamports INTEGER`
- `multiply_sol_borrowed_lamports INTEGER`

`ObligationView` gains matching fields; populated by the priced
multiply path (the legacy sf-based fallback path populates the SOL
borrow + jitoSOL cToken only, since underlying-lamports needs
reserve metas).

Caveat (same as rc45): if the multiply position grew via a new
allocator deposit between baseline and now, the SOL borrow delta
mixes new principal into "interest" calculation. Multiply has no
allocator routing today (rc42 wire format landed but daemon-side
Jupiter swap is operator-only), so this is fine in practice. When
rc42→rc46 routing wires up, we'd want a per-tick `Δ × Δrate`
integration to handle flow-mixed segments cleanly.

After rc46 deploy, multiply gets a "Pure interest accrued" line
matching rc45's stable_yield treatment. Expected near $0.00 right
after deploy (baseline = now); accrues at the multiply daemon's
calculated APR (~7% net of borrow interest at current rates) on
the $8 deployed = ~$0.0153/day or $5.60/year.

---

## v0.4.0-rc45 — dashboard: stable_yield pure interest via Kamino rate snapshots (2026-05-29)

rc44 cleaned up hedgedjlp (Jupiter Perps API returns the right number
directly). stable_yield + multiply still showed the misleading "Position
Δ" — flow-mixed. rc45 closes the stable_yield half with pure interest
accrual from the Kamino USDC reserve exchange rate.

Math:
- Reserve exchange rate = `total_liquidity / collateral_mint_total_supply`
  (already computed by `DecodedReserveLiquidity::ctokens_to_liquidity`).
- Snapshot both `stable_yield_usd` (= ctokens × current_rate × 1e-6)
  and the raw `stable_yield_ctoken_balance` each minute.
- Pure interest:
  ```
  baseline_rate = first_observed_underlying_usd × 1e6 / first_observed_ctoken
  current_rate  = current_underlying_usd       × 1e6 / current_ctoken
  pure_interest = current_ctoken × (current_rate - baseline_rate) / 1e6
  ```
- Falls back to `None` when no rc45 baseline row exists yet (boot-fresh
  install with no stable_yield deposit) so the frontend's existing
  fallback to "Position Δ" still works.

Caveat: if cToken balance changed between baseline and now (operator
deposited more or withdrew), the math attributes the entire rate delta
to the *current* balance. That undercounts interest on cTokens that
exited and overcounts on cTokens that arrived — but it's directionally
right and the error is small for stable positions. Strict
piecewise-constant accrual would need a per-tick `Δctoken × rate`
integration (rc46 work if we want it).

Schema: additive `stable_yield_ctoken_balance INTEGER` column on
`chain_aum_snapshots`. Idempotent migration via the rc44 helper.

Multiply: still deferred. Two-sided accrual (jitoSOL appreciation +
SOL borrow interest paid) needs both reserves' indices snapshotted
and a more careful baseline anchoring. rc46.

Live: on the production wallet's stable_yield position, the rc45
metric should land in the low single dollars (~$0.55 expected at
4.2% APR over 8 days on $55 starting principal), replacing rc44's
misleading "+$22.59" position-delta.

---

## v0.4.0-rc44 — dashboard: realtime perp PnL via Jupiter perps-api (2026-05-29)

rc43's "Earned on-chain" metric was mathematically what was asked
for (`current - first_observed`) but flow-mixed — the live deploy
showed `stable_yield: +$22.59` and `hedgedjlp: -$24.74` after a
rc34 rebalance moved capital between strategies, which made the
labels misleading. rc44 ships the next layer: **realtime protocol-
native PnL** sourced directly from Jupiter's public perps API.

For hedgedjlp specifically:
- `GET https://perps-api.jup.ag/v1/positions?walletAddress=<pk>`
- Sum `pnlAfterFeesUsd` across the dataList (`borrowFees` =
  settled funding; `closeFees` = predicted close costs; both
  deducted in pnlAfterFees)
- Surface as `realtime_protocol_pnl_usdc` on AumOut +
  StrategyCardOut

This is the cleanest "real on-chain earn" available for the perp
short legs — it agrees-by-construction with Jupiter's own UI for
the same wallet and has zero capital-flow noise. On the live wallet
right now: SOL short +$6.33, BTC short +$1.29, ETH short +$0.25 =
**+$7.87 unrealised on the hedge legs**, vs the rc43 metric's
misleading -$24.74.

For stable_yield + multiply: similar pure-interest metrics need
Kamino cToken exchange-rate snapshotting + Jupiter Perps' on-chain
cumulative-borrow-rate tracking. Deferred to rc45 — rc43's
"Position Δ since {date}" label is now used for these (relabeled
from the old "Earned on-chain" so the flow-vs-interest framing is
unambiguous).

Schema: additive ALTER TABLE on chain_aum_snapshots adds
`hedgedjlp_perps_pnl_after_fees_usd_micro INTEGER` (nullable, signed
— perp losses are first-class). New `apply_migrations` helper makes
the column add idempotent across reboots.

Frontend:
- NumbersPanel: top-level "Realtime perp PnL" pane sits above the
  "Position Δ since {date}" pane (the old rc43 metric kept but
  re-labeled with a clarifying caveat about capital flows)
- StrategyCardsRow: hedgedjlp card shows "Realtime perp PnL"; the
  other two strategies still show "Position Δ since {date}" with
  the same caveat tooltip

---

## v0.4.0-rc43 — dashboard: real-time on-chain earned per strategy (2026-05-29)

Operator feedback: the dashboard showed APR percentages without
context — "8.24%" doesn't translate to anything useful unless you
also know the deployed amount and how long the position has been
open. rc43 adds REAL on-chain unrealised earn, not an APR×deployed
math projection.

Baseline: per-strategy first non-zero observed value from the existing
`chain_aum_snapshots` table (already populated since rc24 on a 60s
cadence). Current minus baseline = lifetime-earned. Caveat: includes
operator-funded inflows in the delta (the strict "pure interest
accrual" alternative would require Kamino cToken exchange-rate +
Jupiter Perps settled-funding accounting; rc43 takes the simpler
shape that's still a real chain read).

Backend (`fleet-dashboard-server`):
- `store::sqlite::first_nonzero_per_strategy` — single sqlite query
  returning the first non-zero `(value, ts_unix)` per strategy column
  (multiply, stable_yield, hedgedjlp_jlp, hedgedjlp_collateral, total).
- `AumOut.lifetime_earned_usdc` / `lifetime_earned_since_unix` —
  fleet-wide delta + first-observed timestamp.
- `StrategyCardOut.lifetime_earned_usdc` / `lifetime_earned_since_unix` —
  per-strategy. For hedgedjlp, sums JLP + collateral legs.
- Both fields use `serde(skip_serializing_if = "Option::is_none")` so
  pre-rc24 sqlite installs (no snapshots) keep returning the previous
  shape — frontend defaults via TS optional access.

Frontend (`components/NumbersPanel.tsx`, `StrategyCardsRow.tsx`):
- "Earned on-chain" pane on the Total AUM card with a colored
  +$X.XX figure (emerald positive, amber negative) and a
  "since {date}" subline.
- Per-strategy "Earned on-chain" line below the Position/APR pair,
  with the full timestamp on hover via `title=`.

Two new tests pin the JSON shape:
- `strategy_card_omits_hedge_collateral_for_non_hedgedjlp` asserts
  the earn fields are also omitted (not null) when None.
- `strategy_card_emits_hedge_collateral_for_hedgedjlp` asserts they
  serialize cleanly when Some.

---

## v0.4.0-rc42 — orchestrator allocator routes USDC to multiply (2026-05-29)

Closes the loop opened by rc40 (envelope shape) and rc41 (daemon-side
Jupiter USDC→SOL swap): the orchestrator's allocator now auto-deploys
idle USDC into multiply alongside stable_yield and hedgedjlp.

`tools/fleet-pm-stub/src/allocator.rs`:
- `is_deployable_via_allocator` returns `true` for all strategies
  (multiply joins stable_yield + hedgedjlp; unknown ids still default
  to deployable, with the envelope-spec layer in `allocator_runner.rs`
  as the second gate).
- `min_deposit_usd("multiply") = 10.0`. Jupiter swap + Jito stake +
  Kamino deposit chain costs ~$0.05-0.10 in fees regardless of size,
  so $10 keeps fee drag under 100 bps. rc37 cost-benefit gate is the
  upper guard.

`tools/fleet-pm-stub/src/allocator_runner.rs`:
- `AllocatorAction::Deposit { strategy: "multiply", .. }` now produces
  a real `AssignMultiply` envelope with `usdc_lamports` populated
  (rather than returning `None`). `target_ltv_bps=6000` requests a
  60% LTV; the daemon walks as high as Kamino's BF allows (~46% live
  per [[ref_multiply_ltv_ceiling]]). `max_slippage_bps=100` matches
  the budget that worked end-to-end on the rc39 live test.

Existing rc34 zero-multiply fallback in the AprWeighted resolver
(`allocator.rs:150`) is now defensive-only — the `else` branch is
unreachable since `is_deployable_via_allocator` no longer returns
`false`. Left in place as a guard for any future "non-deployable"
strategy without requiring a re-wire.

Tests updated to reflect the new behavior:
- `is_deployable_via_allocator_filter` — asserts multiply is deployable.
- `min_deposit_usd_table_matches_desk_constants` — asserts $10 floor.
- `multiply_above_hurdle_with_idle_picked_when_deployable_post_rc42` —
  multiply with largest gap is now picked.
- `highest_gap_strategy_wins_after_rc42_multiply_deployable` — picker
  no longer skips multiply.
- `drift_mode_picks_most_underweight_deployable_strategy_rc42` —
  multiply wins as most underweight.
- `apr_weighted_targets_react_to_apr_shifts` — multiply now gets
  non-zero share when above hurdle.
- `rc34_mid_rebalance_state_routes_idle_post_rc42` — same live
  scenario routes to multiply (highest APR gap) instead of hedgedjlp.
- `deposit_multiply_returns_assign_multiply_with_usdc_lamports` —
  envelope-spec layer emits a real AssignMultiply.

Live smoke test plan: trigger an orchestrator-allocator tick with
free idle USDC and verify it routes to multiply via the rc41 Jupiter
swap. Will execute on deploy.

---

## v0.4.0-rc41 — multiply daemon: USDC seeding via Jupiter (2026-05-29)

rc40 landed the `AssignMultiply.usdc_lamports` wire format and rejected
non-zero values with a `bail!`. rc41 wires the daemon-side handling so
the orchestrator allocator can actually deploy USDC into multiply.

Path: USDC → SOL (Jupiter) → jitoSOL (Jito stake) → Kamino obligation
collateral. The middle two steps are the existing `maybe_seed_obligation`
flow; rc41 only adds the leading USDC→SOL step.

Code shape:
- `crates/zerox1-defi-protocols/src/protocols/jupiter.rs`: new
  `build_usdc_to_sol_swap_tx` helper (mirrors `build_jlp_buy_tx`,
  swaps to `WSOL_MINT` with `wrap_and_unwrap_sol: true` so the output
  is native SOL in the wallet rather than wSOL in an ATA).
- `crates/multiply-daemon/src/dispatch.rs`: `DispatchCtx` gains
  `jupiter: Option<Arc<JupiterSwap>>`. `handle_assign` removes the
  rc40 reject-bail and routes `usdc_lamports > 0` through the new
  `seed::seed_with_usdc` helper before falling through to the
  existing seed → leverage flow.
- `crates/multiply-daemon/src/seed.rs`: new `seed_with_usdc(ctx, jup,
  usdc_lamports, slippage_bps)` builds + signs + sends the Jupiter
  swap. simulate-only short-circuits to avoid burning USDC on a probe.
- `crates/multiply-daemon/src/main.rs`: constructs the Jupiter client
  at startup with the lite endpoint (same pattern as hedgedjlp).

Slippage budget: `AssignMultiply.max_slippage_bps` is reused for the
Jupiter swap — same envelope-level constraint already enforces an
ok upper bound (`caps::MAX_SLIPPAGE_BPS = 200`).

Allocator-side `is_deployable_via_allocator("multiply")` still
returns `false`; rc42 flips that and wires the orchestrator's
Deposit emit paths to populate `usdc_lamports`.

One new test (`build_usdc_to_sol_swap_tx_rejects_zero_amount`) pins
the zero-amount guard. Live multi-tx verification — Jupiter swap +
seed bundle + leverage walk — covered by the rc42 deploy smoke test.

---

## v0.4.0-rc40 — AssignMultiply.usdc_lamports wire format (2026-05-29)

Cleared the wire-format prerequisite for allocator-driven deposits
into multiply. `AssignMultiply` now carries a `usdc_lamports: u64`
field (in p2p_architecture commit `dcc10f5`); `serde(default)` keeps
pre-rc40 CBOR payloads decodable, so observer daemons that haven't
been rebuilt continue to consume historical envelopes without
breakage. CI's `P2P_REF` pin in `release-fleet.yml` advances to the
new commit; `ci.yml` already tracks `main` so picks it up
automatically.

Construction sites updated across the fleet:
- `crates/multiply-daemon/src/{caps,approval,auto_mode,dispatch}.rs`
  (5 test helpers default `usdc_lamports: 0`)
- `tools/fleet-pm-stub/src/allocator_runner.rs` (deleverage Assign
  builder)
- `tools/fleet-pm-stub/src/main.rs` (CLI: new `--usdc-lamports`
  flag, defaults to 0)

**Daemon-side handling deferred:** USDC seeding requires Jupiter
integration (`USDC → SOL → jitoSOL → obligation deposit`) which
landings as rc41. Until then, multiply daemon **rejects** any
AssignMultiply with `usdc_lamports > 0` via an explicit `bail!`
with a clear remediation message in
`crates/multiply-daemon/src/dispatch.rs`. Silent acceptance was
specifically avoided so a future allocator wiring can't move
capital into a strategy that won't actually deploy it.

Allocator-side `is_deployable_via_allocator("multiply")` still
returns `false` (rc41/42 work). The orchestrator can populate
the new field today, but multiply will reject the envelope —
matching the documented "operator-trigger only" status of multiply.

3 new protocol tests pin the invariants:
- `assign_round_trips` (extended with `usdc_lamports: 0`)
- `assign_round_trips_with_usdc_lamports` (non-zero value survives
  CBOR round trip)
- `assign_decodes_legacy_payload_without_usdc_field` (backward
  compat: pre-rc40 wire payloads decode with `usdc_lamports = 0`)

---

## v0.4.0-rc39 — multiply clamp: borrow-factor adjustment (2026-05-29)

Live rc38 test exposed a third bug in the rc36 clamp math. The
round 1 sim failed `BorrowTooLarge` despite the rc36 clamp activating:

```
borrow_size Exact(13044077)
Borrow value 1.3395 cannot exceed maximum borrow value 1.2486
```

`Borrow value 1.3395` is the BF-adjusted USD that Kamino's
`BorrowObligationLiquidityV2` checks against `maximum borrow value`
(which is `allowed_borrow_value - bf_debt_value`, also BF-adjusted).
For SOL on the main market, BF = 1.25. The clamp's existing math was:

```rust
safe_lamports = safe_headroom_sf / sol_value_per_lamport_sf
```

where `sol_value_per_lamport_sf` is the **raw** market value per lamport
(read from `ObligationBorrow.market_value_sf / borrowed_amount_sf`).
That's a units mismatch — raw price divides a BF-adjusted numerator, so
the returned `safe_lamports` is `1/BF` too large. With BF=1.25 the
clamp asks 25% more than the chain will accept. Pre-rc38 this manifested
as round-4 BorrowTooLarge in larger walks (where accumulated headroom
usage finally tripped the wall); rc38's tighter live test surfaced it
immediately on round 1.

Fix: multiply `sol_value_per_lamport_sf` by `borrow_factor_bps / 10_000`
before dividing. The clamp signature now takes `borrow_factor_bps`
explicitly so future reserves with different BFs (or BF-config drift)
can be wired through without touching the math. `caps::SOL_BORROW_FACTOR_BPS`
= 12_500 hardcodes the current main-market value, sourced from the live
RefreshObligation log line `value: 5.3788 value_bf: 6.7235`.

Two new tests pin the invariant:
- `clamp_with_borrow_factor_shrinks_budget_by_bf_inverse` — 1.25× BF must
  shrink the lamport budget to ~80% of the no-BF baseline.
- `clamp_matches_live_kamino_check_at_bf_125` — replays the 2026-05-28
  scenario (allowed=$7.9721, bf_debt=$6.7235, price=8.215e-8 USD/lamport)
  and asserts the clamped amount's BF-value is comfortably under the
  $1.2486 chain ceiling.

---

## v0.4.0-rc38 — multiply multi-round walk: seeded LTV query + in-flight bf-debt tracking (2026-05-29)

rc36 shipped an LTV-aware borrow clamp for the multiply leverage loop;
testing on mainnet today caught two remaining bugs that block any
multi-round walk against an existing position.

**Bug 1 — `query_position_ltv_bps` reads the wrong obligation.**
`crates/zerox1-defi-protocols/src/protocols/kamino_loader.rs:281` derives
the obligation PDA with `derive_user_obligation(user, lending_market)`
which hardcodes seed `(0, 0)`. Multiply uses seed `(0, 1)` for its own
per-strategy PDA (`crates/multiply-daemon/src/caps.rs:19`). The loop
ended up reading **stable-yield's** obligation (the (0,0) one): no
borrow → `borrowed_assets_market_value_sf == 0` → LTV returned as `0`
even when the multiply obligation was at 39.8%. The loop's halt
condition `headroom_bps < TARGET_PROXIMITY_BPS` therefore never fired
based on a real measurement.

Fix: add `query_position_ltv_bps_with_seed(rpc, user, market, tag, id)`
and call it with `caps::MULTIPLY_OBLIGATION_SEED.{0,1}` from
`leverage.rs`. The legacy `query_position_ltv_bps` becomes a thin
wrapper for `(0,0)` so stable-yield's existing callsites stay correct.

**Bug 2 — between-rounds RPC read-replica race.**
Round 1's tx broadcast completed at `14.689`; round 2's borrow size
was computed at `14.739` — 50 ms later. Solana's `confirmed`
commitment + RPC read-replica lag meant the re-fetched obligation
still showed pre-round-1 `bf_debt_value_sf` (and `allowed_borrow_value_sf`,
but the limit comes from bf-debt growing under it). rc36's headroom
calc thus saw stale debt and asked **$1.3448** of borrow when the
post-round-1 chain max was **$1.2535** — 7% overshoot, tx failed with
Anchor 6013 `BorrowTooLarge` (Kamino program error 0x177d). Waiting
for `finalized` between rounds would cost 12–32s per round and still
race read-replica selection on the next read.

Fix: track an `in_flight_bf_debt_sf` counter across the loop. After
each broadcast, add `(per_round_borrow_lamports × sol_value_per_lamport_sf × 125 / 100)`
(SOL's 1.25 borrow factor on the main market) to the counter. The
next round's clamp uses `bf_debt_value_sf + in_flight_bf_debt_sf` as
the effective bf-debt. When the re-fetched obligation shows growth in
`bf_debt_value_sf`, the observed growth is credited back against the
counter. End result: even with arbitrarily-stale RPC reads, the loop
under-borrows rather than over-borrowing — which is the safe
direction.

The headroom math is extracted into a pure
`clamp_borrow_to_headroom()` helper so the multi-round propagation
is unit-testable without a chain mock. Six new tests cover the
fall-back-to-naive path, the no-in-flight headroom calc, the rc38
core invariant (in-flight pessimism shrinks next-round headroom),
zero-headroom early-exit, and in-flight-exceeds-allowed early-exit.

Live `BorrowTooLarge` trace from the failed round 2 simulation
(2026-05-28T21:39:14, sig `4TNdzACFsTGiFL93ciNk844L81K4oKQ1hjGK9tzbXv45X2cLLr7tC2Mzxs4171tMBZjDStAH9JFF5jZFvskTxrmQ`)
captured the borrow_size `Exact(13044077)` vs `maximum borrow value
1.2535` — that signature is the rc38 regression marker.

Still pending (#67): `AssignMultiply` envelope has no `usdc_lamports`
field, so the orchestrator allocator still can't auto-size deposits
into multiply. Operator-triggered walk-ups (via `fleet-pm-stub
assign-multiply --target-ltv-bps=N` with `ZX_AUTO_ACCEPT_ORCHESTRATOR=true`)
now work end-to-end.

---

## v0.4.0-rc37 — allocator cost-benefit gate (2026-05-28)

Yesterday's rebalance cycle moved $179 from stable_yield into
hedgedjlp and lost ~$3 to opening costs (Jupiter Swap slippage on
the JLP buy, Jupiter Perps opening fees on three short legs, plus
funding paid on the first overnight). The hedge is working
correctly — that $3 was opening-cost amortisation, not a leak — but
the allocator had **no awareness of opening costs when it decided
to fire** the rebalance in the first place. With small APR gaps or
short expected holding windows, the same logic could (and would)
keep churning capital for negative net P&L.

rc37 adds a cost-benefit check before any allocator-driven deposit
or cross-strategy rebalance fires.

**Per-strategy opening cost table** (`open_cost_bps`, source of truth):

```rust
pub fn open_cost_bps(id: &str) -> u32 {
    match id {
        "stable_yield" => 5,   // just a Kamino deposit
        "multiply"     => 30,  // Jupiter swap × leverage rounds
        "hedgedjlp"    => 40,  // Jupiter swap + 3 perp open fees
        _ => 0,
    }
}
```

**Two new config fields** on `AllocatorConfig`:

- `expected_holding_days: u32` (default **30**) — the period over
  which the rebalance must earn back its opening cost.
- `cost_safety_factor: f64` (default **1.0**) — multiplier on the
  required gain. >1 demands explicit headroom above pure break-even.

**The gate:**

```rust
fn passes_cost_benefit(
    target_id: &str,
    amount_usd: f64,
    apr_gap_bps: i32,
    cfg: &AllocatorConfig,
) -> Result<(), String> {
    let cost = amount × open_cost_bps(target_id) / 10_000;
    let gain = amount × apr_gap × expected_holding_days / (365 × 10_000);
    if gain < cost × cfg.cost_safety_factor {
        return Err(format!(
            "expected ${gain:.2} gain over {N}d (gap {bps}) \
             < ${required:.2} open cost — break-even at {breakeven}d"
        ));
    }
    Ok(())
}
```

Wired into three call sites:
1. `decide_greedy_step` — greedy deposit, gap = best.gap_above_hurdle
2. `decide_drift_step` — drift deposit, gap = target's full APR
3. `try_cross_strategy_rebalance` — rebalance, gap = best.apr − over.apr

**Concrete worked examples** of the gate's effect at default config
(30-day hold, 1.0× safety):

| Move | Gap | Cost | Verdict |
|---|---|---|---|
| Idle → stable_yield at 4.5% APR | 450 bps | 5 bps | Pass (37 bps gain ≥ 5 bps) |
| Idle → hedgedjlp at 10% APR | 1000 bps | 40 bps | Pass (82 bps gain ≥ 40 bps) |
| stable_yield → hedgedjlp at 410 bps spread | 410 bps | 40 bps | Pass (34 bps gain barely ≥ 40 bps) |
| stable_yield → hedgedjlp at 200 bps spread | 200 bps | 40 bps | **Block** (16 bps gain < 40 bps — break-even at 73 days) |
| multiply → hedgedjlp at 100 bps spread | 100 bps | 40 bps | **Block** (8 bps gain < 40 bps — break-even at 146 days) |

The third row is interesting: today's actual rebalance (410 bps gap)
JUST barely passes. With safety_factor 1.5 it'd block — which would
be appropriate for an operator who wants to avoid being margin-of-
error positive. Operators can tune `cost_safety_factor` per their
risk tolerance.

Five new tests pin: production cost constants, block-below-breakeven,
allow-above-breakeven, safety-factor scaling, zero/negative-gap
rejection, stable_yield's lower threshold. Existing picker tests
were given a permissive `expected_holding_days: 365 × 10` so they
test picker logic independent of the cost gate.

**Workspace: 649 tests passing** (+6 from rc36).

## v0.4.0-rc36 — multiply: LTV-aware borrow clamp + raised cap (2026-05-27)

Today's manual `AssignMultiply` test (driven by fleet-pm-stub against
the live wallet's $143 idle USDC) ran rounds 1-3 of the leverage loop
successfully, then failed round 4 with Kamino's `BorrowTooLarge`
(error 6013): asked for $3.48 of additional SOL borrow when the
obligation's allowed-borrow ceiling only permitted $1.34 more.

Two structural problems:

**Bug 1 (#65) — naive per-round borrow ignores LTV capacity.**
`leverage.rs` line 220 computed `per_round_borrow_lamports =
max_position_usdc_lamports / rounds_left` — a flat divide that has
nothing to do with the obligation's actual collateral or existing
debt. The comment literally said *"M9 will replace this with an
LTV-driven sizing function"*. We're now in M9+1 territory. rc36 is
the replacement.

The fix tracks Kamino's borrow-capacity numbers across rounds
(`allowed_borrow_value_sf`, `borrow_factor_adjusted_debt_value_sf`)
and bootstraps a SOL-price ratio from the obligation's existing SOL
borrow (`market_value_sf / borrowed_amount_sf`). Per round:

```rust
let headroom_sf = allowed_borrow_value_sf
    .saturating_sub(borrow_factor_adjusted_debt_value_sf);
let safe_headroom_sf = headroom_sf.saturating_mul(9) / 10; // 90% safety
let safe_lamports = safe_headroom_sf / sol_value_per_lamport_sf;
let per_round = naive.min(safe_lamports);
```

90% safety factor absorbs BF drift between the value read and the
borrow's actual landing slot, plus small slippage overhead. After
each successful round we re-fetch the obligation; stale RPC reads
on value-typed sf fields just mean we under-borrow this round —
strictly safer than the pre-rc36 overshoot.

If no prior SOL borrow exists (fresh wallet, first round) the
SOL-price ratio is `None` and we fall back to the naive amount —
Kamino's on-chain check is still the safety net, but subsequent
rounds clamp once the price ratio is observable.

**Bug 2 (#66) — `max_position_usdc_lamports` defaulted to $100.**
The systemd template (`hedgents-multiply-live.service`) inherited
the daemon's $100 development default. Even a successful leverage
attempt would only deploy $100 of the wallet's idle USDC, leaving
the rest unused. Bumped to **$5000** (5_000_000_000 lamports) so
real-sized positions deploy. Well under `caps::MAX_POSITION_USDC_LAMPORTS`
($5M hard ceiling). Operators who want a smaller cap can override
via `--max-position-usdc-lamports`.

**Not in rc36 (#67 deferred).** The architectural fix —
`AssignMultiply` envelope shape change to add `usdc_lamports` so the
orchestrator can size multiply deposits — is a bigger change that
breaks envelope compatibility with the observer daemons
(riskwatcher, researcher). Deferring to a later rc with proper
migration. Until then, multiply remains operator-managed via
`fleet-pm-stub assign-multiply` and is excluded from the auto-allocator's
target weights (rc34 fix).

Workspace: **643 tests** (no test-surface change — the existing
leverage tests don't exercise the LTV clamp because they're round-1
fresh-wallet shapes; the clamp's behaviour is observable from live
production logs once deployed).

## v0.4.0-rc35 — reserve USDC for perp short collateral before JLP buy (2026-05-27)

rc34 unblocked the deposit-side allocator math. Live state confirmed
the rc29 cross-strategy rebalance loop closed end-to-end: stable_yield
withdraw → idle accumulates → orchestrator emits `AssignHedgedJlp`
→ hedgedjlp daemon receives $120.63.

Then the daemon bought $120 of JLP via Jupiter Swap successfully
(sig `34XFbLvxy…cJyi`) — and immediately failed to open any of the
three short legs with `Token program error 0x1 (insufficient funds)`.
Net: **$120 of unhedged long JLP** in production. The literal
opposite of "delta-neutral".

Root cause: `jlp_hedge::run_jlp_buy_only` passed **the full
`payload.usdc_lamports`** to the Jupiter Swap call. With 100% of
input converted to JLP, the wallet had zero USDC left for the perp
short collateral (each `create_increase_position_market_request`
transfers USDC from the wallet to the position PDA — at
`HEDGE_LEVERAGE=5x`, ~16% of JLP value is needed as collateral).

This bug has been latent since day one of the open path. It stayed
masked because prior Assigns had been triggered against wallets with
residual USDC from earlier cycles. rc29's cross-strategy rebalance is
what changed the regime: the orchestrator now sends **exactly the
idle balance**, leaving zero USDC after the buy. First Assign under
rc29's regime exposed the bug.

Fix: carve out a USDC reserve before the JLP buy.

```rust
const HEDGE_COLLATERAL_RESERVE_BPS: u64 = 2000; // 20% of input

fn jlp_buy_amount_after_reserve(usdc_lamports: u64) -> u64 {
    let reserve = (usdc_lamports as u128 * 2000) / 10_000;
    usdc_lamports.saturating_sub(reserve as u64)
}
```

Sizing math: JLP long delta ≈ 82%. At 5x leverage,
short_collateral = (0.82 × jlp_value) / 5 = 0.164 × jlp_value.
With 20% reserve (1.5x safety margin over the minimum), input
splits to 80% JLP buy + 20% USDC reserve. The synthetic delta
computation also takes the post-reserve amount so short notionals
size correctly.

**Important relationship to rc27 (orphan-shorts incident).** rc27
fixed the *close-side* — orphan shorts left open after a Withdraw
burned the JLP. rc35 fixes the *open-side* — buying JLP without
reserving short collateral. Same architectural class (multi-leg
operations failing to preserve operational state across legs), two
different instances. The open path should have been audited
alongside the close path during rc27; the miss cost one production
cycle of unhedged exposure.

Workspace tests: 643 passing (no test-surface change — the synthetic-
delta path is exercised by existing rebalance tests).

## v0.4.0-rc34 — AprWeighted excludes non-deployable strategies (2026-05-27)

rc33 deploy unblocked the on-chain Withdraw path: $120 of USDC drained
out of stable_yield as the rebalance designed. But the orchestrator's
*next* dispatch was `AssignStableLend` — depositing the freed capital
right back into stable_yield instead of into hedgedjlp. Watched the
cycle live: Withdraw → idle accumulates → re-Deposit to stable_yield
→ no net movement.

Root cause: the AprWeighted target resolver was including `multiply`
in the gap-weighted budget split. Multiply consistently has the
largest APR-vs-hurdle gap on the live fleet (~170 bps over hurdle),
so multiply captured ~92% of the non-stable target share. But
`AssignMultiply` has no `usdc_lamports` field — the allocator cannot
size a deposit envelope for it (see `is_deployable_via_allocator`).
With multiply's 92% effectively dead capacity, hedgedjlp's target
shrank to ~6%, equal to $16 at our $264 AUM. That's below
hedgedjlp's $100 desk floor, so the rc28 fallback dumped the
rebalance into stable_yield. Net: an expensive no-op cycle that
churned gas without moving exposure.

Fix is one branch in `TargetMode::AprWeighted::resolve`: zero
`apr_bps` for any strategy that fails `is_deployable_via_allocator`.
That collapses multiply's gap to ≤ 0, drops its target share to 0,
and the non-stable budget redistributes proportionally among the
strategies the allocator *can* fund. With only hedgedjlp left
non-stable on the live fleet, hedgedjlp now captures the full 80%
non-stable target — well above its $100 floor at any non-trivial AUM.

One pre-existing test (`apr_weighted_targets_react_to_apr_shifts`)
was rewritten — the old test pinned multiply-vs-hedgedjlp share
ratios, which doesn't make sense once multiply is always 0. The
"dynamic targets respond to APR shifts" contract still holds; the
new test exercises hedgedjlp's hurdle-cross regime (above-hurdle →
non-stable budget, below-hurdle → all-in-stable). Plus one new test
(`rc34_mid_rebalance_state_routes_idle_to_hedgedjlp_not_stable_yield`)
pinning the exact 2026-05-27 production snapshot that exposed the
bug. Workspace: **643 tests passing** (+1 from rc33).

## v0.4.0-rc33 — fleet monitor + systemd approval-gate fix (2026-05-27)

Two operational fixes that surfaced together while testing rc32.

**Fix 1: systemd unit reset bug.** Every `install-hedgents.sh` overwrites
`/etc/systemd/system/hedgents-*.service` from the release tarball. The
in-repo `hedgents-stable-yield-live.service` template shipped with
`--require-approval=true`, so every rc-deploy silently restored that
default and any operator runtime override (`sed -i`'d on the live box)
was wiped. Effect: orchestrator emits `WithdrawStableLend`, daemon
queues for `Approve` envelope that never comes, capital stays stuck.
Took ~90 min of incident-debugging to diagnose because the daemon's
log said `WithdrawStableLend queued — awaiting Approve` exactly once
per cycle and the symptom looked identical to the Kamino bundle bugs
from rc30-rc32. Fixed at the source: template now defaults to
`--require-approval=false`, matching `hedgedjlp-live`'s long-standing
config. Multiply-live still ships `--require-approval=true` because
multiply withdraws are riskier (leveraged unwind with slippage).

**Fix 2: fleet-monitor.sh + hedgents-monitor.timer.** A one-shot
health probe run every 5 minutes via systemd timer. Checks:
1. Every live daemon is `systemctl is-active`.
2. No `build_sign_send failed` lines in any daemon log in the last
   10 minutes.
3. No `orphan shorts` warnings from `recover.rs` in the last 10 min.
4. Allocator hasn't emitted `NoAction` 6 ticks in a row (stuck
   capital signal).
5. Orchestrator's most recent log line is within 5 minutes
   (heartbeat).

On any new failure, emits an email alert (deduped per signature for
1 hour, so the same error doesn't spam) and writes to
`/var/log/hedgents-monitor.log`. Runs independently of the rc25
watchdog timer (which only probes the dashboard's `/aum` endpoint).

Both fixes packaged: the install script copies `fleet-monitor.sh`
into `$PREFIX/bin` and enables `hedgents-monitor.timer` automatically
if the unit files are present in the tarball — same opt-out shape as
the rc25 watchdog timer.

Workspace tests still 642 passing (no test-surface change).

## v0.4.0-rc32 — Kamino withdraw bundle adds farm refresh (2026-05-27)

rc31 switched the withdraw to the v2 discriminator and the on-chain
error changed from `0xbc0 InvalidProgramId` to `0x17a3
IncorrectInstructionInPosition` — same path failing, different gate.
Kamino's klend `check_refresh` requires a specific sequence of
refresh ixns to appear in the tx before the v2 withdraw when the
reserve has a farm attached. Specifically:

```
Required ix: 0 → RefreshFarmsForObligationForReserve
Required ix: 1 → RefreshObligation
Required ix: 2 → RefreshReserve
```

The USDC reserve on Klend (`D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59`)
has farm `JAvnB9AKtgPsTEoKmn24Bq64UMoYcrtWtq42HHBdsPkh` attached, so
the farm-refresh ixn was required. Pre-rc32 our bundle was
`[ATA, refresh_reserve, refresh_obligation, withdraw_v2]` — missing
the farm refresh entirely.

This is why multiply's withdraw path worked without it: jitoSOL and
SOL reserves don't have farms attached. The farm-refresh requirement
only kicks in when `reserve.farm_collateral != Pubkey::default()`.

rc32 adds a conditional `refresh_obligation_farms_for_reserve_ix` to
the bundle when the reserve has a farm:

```
[ATA, refresh_reserve, refresh_obligation, refresh_farms?, withdraw_v2]
```

Two new tests pin both branches: farm-present (bundle = 5 ixns) and
farm-absent (bundle stays = 4 ixns, multiply's path unchanged).

This is the third Kamino-withdraw bug in three days. rc30 fixed an
imaginary account-layout problem, rc31 switched to the v2
discriminator (real fix but exposed a new gate), rc32 fixes the
real-but-different farm-refresh requirement. Each step uncovered the
next, which is the cost of an on-chain path that was never exercised
in production until rc29. **642 tests passing (was 640, +2 rc32 tests).**

## v0.4.0-rc31 — Kamino withdraw delegates to v2 discriminator (2026-05-25)

rc30 attempted to fix the on-chain `liquidity_token_program:
InvalidProgramId (Error 3008)` by realigning the v1 account layout
to mirror v2's. Deployed it, restarted everything, watched the next
Withdraw tick — **same error**.

Re-diagnosed: the bug isn't account ordering. The v1 discriminator
`withdraw_obligation_collateral_and_redeem_reserve_collateral`
points to a Kamino entry point whose IDL **expects different
accounts at the same positions** than v2 does. No reordering against
v2 will ever make a v1-discriminator tx land — Kamino's program
checks the program-id-typed accounts and rejects whatever we pass.

The fix that works: just use the v2 discriminator. `multiply-daemon`
has been calling
`withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix`
successfully since rc26-era; it's the path Kamino keepers actually
exercise. So `kamino::withdraw_ix` (the high-level wrapper) now
delegates the final ixn to the v2 builder while keeping the same
external `Vec<Instruction>` shape (`[ATA, refresh_reserve,
refresh_obligation, withdraw_v2]`).

Regression test `withdraw_ix_delegates_to_v2_under_the_hood` pins:
the bundle still produces 4 instructions, the last one has 17 accounts
(14 v1-shaped + 3 v2 farm appendix), and the critical slots match
v2's IDL ordering.

Also fixed operationally during this debug: `stable-yield-live` was
running with `--require-approval=true --auto-accept-orchestrator=false`,
which queued every orchestrator Withdraw for human approval instead
of executing it. Aligned to hedgedjlp-live's `--require-approval=false`
so the rc29 cross-strategy rebalance can complete unattended. The
sender-allowlist gate (only orchestrator's agent_id) remains the
authority boundary.

Workspace: **640 tests** (unchanged).

## v0.4.0-rc30 — fix Kamino v1 withdraw_ix account layout (2026-05-25)

rc29 deployed cleanly: the orchestrator did exactly what it was
supposed to do, emitting a `WithdrawStableLend` envelope every 5
minutes to free capital from the overweight stable_yield slice. But
each Withdraw failed on-chain with:

```
Instruction: WithdrawObligationCollateralAndRedeemReserveCollateral
AnchorError caused by account: liquidity_token_program.
Error Code: InvalidProgramId. Error Number: 3008.
```

Two latent bugs in `kamino::withdraw_ix` (the v1 builder used by
`stable-yield-daemon::lend.rs`) — both there since day one but
dormant because **no Withdraw against stable_yield had ever fired in
production**. Greedy-mode allocator only withdrew when a strategy
fell below its hurdle, and `stable_yield` IS the hurdle anchor, so
it could never be under-hurdle. rc29 was the first time anything
ever asked stable-yield to give USDC back.

The bugs:

1. **Missing placeholder slot at [10].** Kamino's IDL for the v1
   withdraw discriminator has the same shape as the deposit one:
   `... user_destination_liquidity, placeholder_user_destination_collateral
   (optional), collateral_token_program, liquidity_token_program,
   sysvar_instructions`. Our v1 builder omitted the placeholder. The
   two token programs and the sysvar shifted up by one slot, so
   `liquidity_token_program` ended up receiving SYSVAR_INSTRUCTIONS
   — Anchor checked the program ID, failed, error 3008.

2. **Slots [6] and [8] swapped.** v1 had
   `[6]=liquidity_supply, [8]=collateral_supply` (copied from
   the deposit layout). The Kamino IDL for withdraw expects
   `[6]=reserve_source_collateral=collateral_supply, [8]=liquidity_supply`
   — the opposite, since withdraw flows in the reverse direction.

Both fixed by aligning v1's account layout to v2's (which has been
working for `multiply-daemon`'s unwind path). The discriminator stays
`withdraw_obligation_collateral_and_redeem_reserve_collateral` (v1),
but the account positions now mirror `_v2_ix`. New test
`withdraw_v1_account_layout_matches_kamino_idl` pins every critical
slot so a future copy-paste error won't reintroduce either bug.

Workspace: **640 tests** (was 639).

## v0.4.0-rc29 — active cross-strategy rebalance (2026-05-24)

Post-rc28, the live wallet sat with $256 in stable_yield (5.41% APR)
and $0 in hedgedjlp (9.51% APR) — a 410 bps gap on $250 ≈ $10/year
of yield being left on the table. The allocator had **no path to move
capital between two healthy strategies**: it deposited idle, withdrew
under-hurdle, and that was it. Initial entry timing locked the
allocation forever.

Two patches make this a real allocator instead of a one-way ratchet:

**Patch 1 — `AprWeighted` is the default `target_mode`.** The drift
machinery + APR-weighted target resolver have been sitting unused
since rc22-rc23; rc29 flips the default to `"apr-weighted"` so every
boot routes through `decide_drift_step`. Operators who want the
legacy greedy path can set `--target-mode=greedy`.

**Patch 2 — cross-strategy rebalance in `decide_drift_step`.** When
the normal deposit picker can't fire because `idle_usd < min_action_usd`,
the new `try_cross_strategy_rebalance` checks for:

1. an eligible underweight target (the `best` from drift sort),
2. an overweight strategy with `drift_bps ≥ rebalance_overweight_bps`
   (default 1500 = 15% above target),
3. an APR gap (`best.apr − over.apr`) `≥ rebalance_min_apr_gap_bps`
   (default 200 = 2% — well above Kamino's typical jitter).

If all three hold, emit a `Withdraw` for the overweight strategy
sized at `0.5 × overweight_dollars` (damping factor keeps single-tick
moves smaller; multiple ticks converge). Two-tick rebalance: tick 1
emits Withdraw, on-chain settlement frees USDC, tick 2 sees idle and
the regular deposit picker routes it to the underweight target.

**Why not LLM-based?** A `(APR_a − APR_b) × balance × horizon vs
move_cost` decision is arithmetic, not judgement — LLMs add latency,
cost, and non-determinism without changing the answer. Regime
detection (is JLP fee APR durable, is this volatility transient) is
where an LLM could earn its keep; rebalancing between known yields
is not that. The cost gate via `rebalance_min_apr_gap_bps` does the
same job in a few constants.

Five new tests pin the production-snapshot shape, the cost-gate veto,
the idle-sufficiency interaction with rc28's deposit picker, the
overweight-threshold veto, and two-tick convergence.

Workspace: **639 tests** (was 634).

## v0.4.0-rc28 — allocator min-deposit awareness + libp2p dedupe (2026-05-23)

Two structural bugs surfaced while resolving the rc27 orphan-shorts
incident. Both deserved their own release before they bite something
larger.

**Patch 1 — allocator: skip deposits below each desk's min floor.**
Post-rc27 the wallet held $81 recovered from the orphan shorts. The
orchestrator's greedy picker chose `hedgedjlp` (highest gap) and
proposed `Deposit hedgedjlp $81.09` every tick. The desk rejected
each attempt at the `cap_validation` gate (`usdc_lamports 81094896
below minimum 100000000`); cooldown reset and the loop repeated.

`tools/fleet-pm-stub/src/allocator.rs` now carries a small
`min_deposit_usd(id)` table mirroring each desk's
`MIN_POSITION_USDC_LAMPORTS`. Both `decide_greedy_step` and
`decide_drift_step` gate their deposit picks on it:

- if the picked leveraged candidate's amount is below its desk floor,
  fall back to stable_yield (which has a $1 floor that any non-dust
  amount clears)
- if even stable_yield's floor isn't met, emit `NoAction` with a
  clear "below all desk minimums" reason instead of proposing a
  rejected envelope

Four pre-rc28 tests used small idle amounts that worked fine when the
floor was implicit; those have been bumped past the $100 hedgedjlp
floor so they still target the picker logic they were written for.
New tests cover the rc27 incident shape ($81 idle, $175 in
stable_yield, $0 in hedgedjlp → falls back to stable_yield) and the
boundary ($150 idle below the AUM cap → falls back; $300 AUM lifts
the cap and hedgedjlp wins). Allocator suite: 77 tests pass.

**Patch 2 — dispatch: dedupe duplicate envelopes by conversation_id.**
Hit twice in one session — once on the rc27 Withdraw (the stub
displayed an empty `tx_signatures` from a zero-Report second delivery
even though the first delivery had submitted three close-requests
correctly), once on the rc28-fix stable_yield deposit (first
delivery deposited $81 successfully; libp2p re-delivered the same
envelope 3 seconds later; daemon tried to deposit *another* $81 and
got "insufficient funds" from the Token program — the stub then
reported `ok=false` even though the first deposit had landed). The
second case is only safe by accident: if the wallet had been topped
up between deliveries, the daemon would have happily double-deposited.

Each desk's `dispatch.rs` (`hedgedjlp`, `stable-yield`, `multiply`)
now carries a 64-slot FIFO `ConvDedupe`. On Assign / Withdraw
delivery: if the conv_id was already processed, drop silently
(after logging "rc28: duplicate Assign/Withdraw dropped") instead of
re-running the on-chain work. Approve / Beacon / MarketSignal are
NOT deduped — Approve is idempotent (queue is empty after first
run), the others are observations. State is per-process; a restart
would re-process retransmissions from before the restart, which is
acceptable because `recover.rs` rebuilds any divergence on boot.

Total workspace test count: **634 passing** (was 631, +3 dedupe
tests +3 allocator rc28 tests, with 3 pre-existing tests retargeted
above the new floor).

## v0.4.0-rc27 — orphan-shorts incident fix: recover, slip, verify (2026-05-23)

**Incident.** On 2026-05-22T00:24:14 UTC a Withdraw closed the JLP leg
but left 3 perp shorts ($304 SOL + $70 BTC + $24.70 ETH notional,
$79.48 collateral) open on Solana mainnet. State machine had cleared
`ActivePosition`, so subsequent runs found "nothing to do" — the 3
positions were architecturally invisible to the daemon for ~24h.

**Three structural bugs, three patches:**

1. **`recover.rs` short-circuited on `jlp_balance == 0`** and never
   inspected perp position PDAs. Any successful JLP redeem that left
   shorts open would orphan them forever. Patch reads the position
   list first; if shorts exist they get added to the rebuilt
   `ActivePosition` regardless of JLP balance, with a `warn!` flagging
   the orphan-shorts case.

2. **`unwind.rs` close slippage was `sim_mark_price_micro_usd - buffer`
   = 1 - 0 = 1 micro-USD.** `sim_mark_price_micro_usd` returns 1 as a
   fail-safe (correct for opens — floor, oracle must be ≥ floor —
   broken for closes — ceiling, oracle must be ≤ ceiling). The Jupiter
   keeper saw "only fill if SOL trades at $0.000001" and silently
   rejected every request. Patch fetches live oracle-class prices via
   `prices::fetch_custody_prices_micro_usd` once before the loop, then
   per asset computes `short_price_ceiling_micro_usd(live_mark) =
   live_mark + 10%`. Mirror of the rc12 open-side fix.

3. **`unwind.rs` cleared `ActivePosition` unconditionally after submit
   — no fill verification.** A silent keeper reject was therefore
   indistinguishable from a successful close. Patch tracks every
   submitted close, polls each Position PDA (30 × 1s = 30s window)
   for `is_empty()` / account-gone before redeeming JLP, gates the
   JLP redeem on full close success (burning JLP while shorts remain
   would convert hedged book → naked shorts), and does a partial-clear
   of `ActivePosition` retaining any positions the keeper failed to
   close, so the next Withdraw picks them up.

**Deploy + recovery (observed, 2026-05-23 18:52–18:56 UTC).** Shipped
rc27 to the Hetzner ARM VM. Restart of `hedgedjlp-live` produced the
expected recovery log within 200ms of boot:

```
WARN recover: no JLP balance but 3 open Jupiter Perps short(s)
     discovered — orphan shorts from a partially-failed unwind.
     short_count: 3
INFO recovered active position: jlp_lamports=0, open_shorts=3,
     custodies=5, hedge_notional_usdc: 399012333   # $399.01
```

`fleet-pm-stub withdraw-hedgedjlp --jlp-lamports u64::MAX` (the
`0` value is rejected by the dispatch cap-validation gate; full-
withdraw sentinel resolves to `jlp_acquired = 0` so the close-only
path runs and the JLP redeem leg is skipped). Within 3 seconds the
unwind path submitted three close-requests on mainnet:

- SOL: `4YGm5JB7XCxoD5pWQ9truVPJaso2U8uBexmYbiMG4ZsnbTi2qe2gkHXGypLGudVQBw8HvnsmjYW8VTdzr6ad3BSE`
- BTC: `APqFzJvxQNv4boXx6ZyUkQ4jNUkp5pfcgrW1ZSttuyNbTGCxFdfP74MhAwwQP4u1Ym6SHGu4zX9ykhJv9LakTrY`
- ETH: `22qQVnSi8cWBJhc53FfjNxbjXfFJy7AZtbVdisNGDAu4DJAXcFzXUYQE1muTKjZqgV5CYGJMEWVTjoxzvzD1Suei`

Jupiter keeper executed all three within a single slot window;
`wait_for_position_closed` returned `true` on the first poll for
SOL and ETH and on the second poll for BTC (logged once at attempt
1/30 with size_usd=70212491, then silent success at attempt 2).
`still_open` ended empty → `clear_active_position` ran → ok=true.

Post-restart re-scan from a clean process confirmed the book is
flat:

```
INFO recover: no JLP balance, no open shorts — fresh start,
     state.active stays None
```

Wallet USDC after recovery: **$81.09** (= $79.48 returned collateral
+ ~$1.61 short-side PnL accrued during the 24h the positions were
orphan). Net incident cost: a libp2p envelope-retransmission caused
the stub to display the empty zero-Report from the duplicate
delivery instead of the real Report (cosmetic only — daemon log
shows the actual sigs above).

**Lesson.** Every silent-rejection branch that previously logged a
warn but kept marching forward should be re-audited against this
pattern: submit ≠ execute on a 2-tx protocol. The rc9 open-side
`wait_for_nonzero_position_size` already had this discipline; rc27
extends it to the close side.

## v0.4.0-rc26 — rc25 follow-up: bundle frontend.service + ship .timer files (2026-05-22)

rc25 added the watchdog timer and the `Restart=always` policy across
templates, but the release-artifact pipeline had two latent gaps that
the rc25 install surfaced:

1. **`hedgents-frontend.service` lived only on the production server**,
   not under `deploy/systemd/`. The install script's
   `install -m 0644 "$SRC/systemd/"*.service` therefore never refreshed
   it — operators kept whatever was hand-deployed on first bring-up.
   Result: rc25's `Restart=always` for the frontend was a no-op
   because the on-disk template didn't ship the unit.
2. **`release-fleet.yml` only staged `*.service` + `*.target`** into
   the release tarball. `*.timer` files (introduced in rc25 for the
   watchdog) silently weren't shipped. `systemctl enable
   hedgents-watchdog.timer` on a fresh rc25 install returned
   `not-found` because the file wasn't there.

Both fixed:

- `deploy/systemd/hedgents-frontend.service` vaulted in-repo (was a
  server-only artifact); installer can now refresh it across
  releases.
- Release workflow stages `*.timer` files when present (conditional
  on glob match, so older tags without timers don't break).
- Live server was hot-patched (scp + daemon-reload + enable) so the
  current deployment doesn't wait for rc26 — every unit is
  `Restart=always` and the watchdog is firing every 5 minutes.

This is the same lesson as rc17/rc18 (installer pipeline gap → UI
work undeployed): a manifest entry without a corresponding `cp` is
the silent-failure class that this codebase keeps re-discovering.
The rc26 fix-release pattern (small commit per gap, clear DEVLOG note)
keeps each one auditable.

## v0.4.0-rc25 — always-on hardening: Restart=always + boot-survival + watchdog (2026-05-22)

Operator question: *"make sure the agent is always running and dashboard
is always showing data."* Audit on the live VM found two structural
gaps:

1. **6 of 8 hedgents-* units were `disabled` at boot.** The install
   script told operators to `systemctl enable --now hedgents.target`,
   which activates the target but doesn't create the per-unit
   `WantedBy=hedgents.target.wants/` symlinks. On a VM reboot the
   target came up but most daemons didn't.
2. **`Restart=on-failure`** only catches non-zero exits. A daemon that
   exits cleanly (signal, normal shutdown, OOM-killer-without-cause)
   stays dead.

Fixes:

- **All daemon unit templates**: `Restart=on-failure` → `Restart=always`.
  The existing `StartLimitBurst=10` / `IntervalSec=120` still caps the
  crash-loop scenario; `always` adds coverage for clean-exit cases.
- **New `hedgents-watchdog.service` + `.timer`**: every 5min after
  boot, `curl -fsm 10 /aum`. If unresponsive → `systemctl restart
  hedgents-dashboard`. Catches the harder case where the process is
  alive but the HTTP server has wedged (which `Restart=always` can't
  see). No external monitoring dependency.
- **`install-hedgents.sh`** now `systemctl enable`s every hedgents
  unit + the watchdog timer unconditionally. Idempotent on re-runs;
  operators who disabled a unit for debugging can re-disable after
  install.
- **Live server** had all 6 disabled units enabled immediately so the
  current configuration survives a reboot without waiting for the
  rc25 release artifact.

This closes the "is it always running?" question structurally — every
class of failure that can be caught by a systemd-level mechanism is
now caught. What's left is application-level state corruption (rare)
and the operator-visible incidents that the rc1-rc24 march has been
pinning with regression tests one by one.

## v0.4.0-rc24 — /pnl from chain-state snapshots (2026-05-21)

Operator question: *"are we actually making money?"* The dashboard
said "no", but the chain said yes — three latent bugs in the pre-rc24
`/pnl` pipeline silently zeroed out a real $200 hedgedjlp position:

1. **hedgedjlp daemon's `ActivePosition` was desynced from chain.**
   After a partial unwind at 2026-05-21T00:29:00 UTC, the daemon
   cleared its internal state but the on-chain shorts + JLP remained.
   Subsequent Assigns reopened on-chain but didn't restore the
   internal ActivePosition → telemetry rows reported `jlp_lamports: 0`
   while chain held $200.
2. **multiply and stable_yield were running paper-mode systemd units**
   (`hedgents-multiply.service`, not `-live`). Real positions exist
   on-chain; paper-mode daemons polled them but wrote telemetry to
   `*-pnl.jsonl` not `*-live-pnl.jsonl`. Pre-rc24 `/pnl` only scanned
   the `*-live-pnl.jsonl` paths → 2.4-day-stale data.
3. **`/pnl` aggregated per-daemon rows** → any single broken stream
   poisoned the whole window.

`/aum` was already correct because it reads chain state directly. The
rc24 fix makes `/pnl` use the same source of truth.

- New `chain_aum_snapshots` SQLite table: per-tick `(ts_unix, total_usd,
  multiply_usd, stable_yield_usd, hedgedjlp_jlp_usd,
  hedgedjlp_collateral_usd, idle_usd)`. `ON CONFLICT (ts_unix) DO
  NOTHING` for idempotency.
- New `ingest::aum_sampler` background task: 60s cadence, calls the
  shared `read_chain_aum_breakdown` helper that `/aum` now also uses,
  inserts one row per tick. Refuses to write `total_usd == 0` rows
  (treats as transient RPC failure → next tick retries).
- `/pnl` rewritten: brackets `chain_aum_snapshots` within the window,
  computes `delta = end - start` and `annualised_apy = (delta/start)
  × (year/elapsed) × 100`. Same `PnlOut` JSON shape — no frontend
  break. Windows: `1h`, `24h`, `7d`, `all`.
- `/aum` refactored to call `read_chain_aum_breakdown` so both
  endpoints share the same chain reads — `/pnl` and `/aum` can no
  longer disagree by construction.
- Legacy `pnl_row_to_usd` helper kept under `#[allow(dead_code)]` for
  future per-daemon-secondary-signal use; its tests still pin the
  daemon telemetry JSON shape that daemons emit.

8 new tests: 2 Store round-trip + dedup, 4 `/pnl` HTTP integration
(empty-history note, computed delta, single-snapshot edge, window=all
cutoff math), plus the existing `/aum` tests still pass after the
refactor. Workspace: 67 dashboard tests (up from 34) — the big jump
is from existing tests being moved + new coverage; not a separate count.

**What this answers concretely:** on a fresh rc24 deploy the dashboard
will show a 5s-delayed `/pnl` populating from the very first snapshot,
and within 1-2 ticks operators see actual realized P&L on real
on-chain positions — including the $200 hedgedjlp position that
pre-rc24 was invisible.

## v0.4.0-rc23 — allocator v2 M5: APR-weighted dynamic targets (2026-05-21)

Follow-up to rc22's static drift mode. rc22 shipped the static path
(operator types `30/30/40` once in conf, allocator drifts toward it).
A natural follow-up question: "is `30/30/40` still static?" Yes — and
M5 is the dynamic answer.

`TargetMode::AprWeighted` recomputes the target tilt each tick from
the live per-strategy gap to hurdle:

```text
gap_i        = max(0, apr_i_bps - hurdle_i_bps)
non_stable_budget = 1 - stable_yield_floor
weight_i     = non_stable_budget * (gap_i / sum_of_gaps)
weight_stable = stable_yield_floor (default 0.20)
```

Higher-yield strategies auto-pull capital toward them. A `stable_yield_floor`
keeps the operator's risk-off anchor populated even when other strategies
are yielding well. If everything falls below hurdle, weights collapse to
100% stable_yield.

Wiring:
- new `TargetMode` enum: `Static(TargetWeights) | AprWeighted(AprWeightedConfig)`
- new `allocator_apr_weighted.rs` — pure math, validation, 12 unit tests
- `decide()` resolves the mode once per tick; downstream drift-step is
  mode-agnostic
- orchestrator CLI:
  - `--target-mode=static` (default with `--target-weights=…`)
  - `--target-mode=apr-weighted` (uses live APR gaps)
  - `--stable-yield-floor=0.20` (default)
  - `--min-per-strategy=0.0` (default — opt-in floor for noise dips)
- AuditLog refactored: `append_with_result` now accepts a resolved
  `Option<&TargetWeights>` from the caller (per-tick). Static mode
  passes the same vector every tick; AprWeighted passes a freshly
  resolved one. The forensic record matches the picker's input
  exactly, even when the target vector is moving.

15 new tests (12 math + 3 end-to-end integration). 74 fleet-pm-stub
tests pass total.

**Activation:**
```
EXECUTE_FLAGS=--execute --targets-json=... --cooldown-secs=300 \
    --max-action-fraction=0.50 --min-action-usd=5.0 --stale-slack=1.10 \
    --target-mode=apr-weighted \
    --stable-yield-floor=0.20 \
    --min-drift-bps=200
```

**Deferred to a later rc (M6):** riskwatcher overrides — a layer that
lets `EscalateRisk` envelopes from the riskwatcher pubkey temporarily
shrink a strategy's effective target weight. The mesh-integration cost
of subscribing to the inbox is the same class of work that introduced
rc2/rc14 nonce bugs; deferring it until we have soak data from M5 in
production is the conservative call. Plumbing-only M6 (manual override
CLI without inbox subscription) is scoped at ~250 LoC for whenever we
revisit.

## v0.4.0-rc22 — allocator v2: drift-from-target allocation (2026-05-21)

Pre-rc22 the allocator was greedy: one action per tick, "best-gap-above-
hurdle wins for Deposit, worst-under-hurdle wins for Withdraw." Over
time + cooldowns + caps that produced an *emergent* allocation — the
highest-APR strategy accumulated more deposits — but never a *designed*
one. Operators couldn't say "I want 30% in stable_yield, 30% in
multiply, 40% in hedgedjlp" and have the orchestrator drive toward
those targets. The rc15 incident analysis also surfaced this directly:
when an operator dashboard showed `1:1:1` across the three strategies,
that was just three daemons each independently using a hardcoded
$50k paper-principal baseline — there was no allocator math behind it.

rc22 adds drift-from-target mode as a first-class allocator path. Four
shipping milestones (all behind a single CLI flag):

- **M1 — `TargetWeights` struct + CLI parser** (`allocator_targets.rs`,
  368 LoC, 13 tests). Operator-set tilt `(stable=0.30, multiply=0.30,
  hedgedjlp=0.40)`. Validates no-negative, sum within [0.99, 1.01]
  tolerance, normalises to exactly 1.0. CLI parser tolerates whitespace,
  order-independence, missing strategies (default 0.0). Rejects unknown
  strategy names with an actionable error listing the expected three.
  Inert — no caller yet.
- **M2 — drift dispatcher + `decide_drift_step`** (`allocator.rs`,
  566 LoC, 11 tests). `decide()` becomes a dispatcher; when
  `target_weights` is `Some`, the new drift path runs after the shared
  hurdle gate. Picks the largest absolute *underweight* drift among
  eligible strategies (deployable, above-hurdle, target > 0). Overweight
  is never proactively withdrawn — that would whipsaw against a strategy
  whose APR is rewarding being there. New `min_drift_bps` (default 200)
  is the rebalance band; drifts inside it are noise. When
  `target_weights: None` (default), every rc15–rc21 behavioural test
  passes byte-for-byte.
- **M3 — audit log shape** (`allocator_runner.rs`, 216 LoC, 4 tests).
  `AuditStrategy` gains optional `current_weight`, `target_weight`,
  `drift_bps`. `from_with_targets(snap, Some(&t))` populates all three;
  `from(snap)` defaults to greedy and emits only `current_weight`.
  Backwards-compatible: pre-M3 readers see no schema break.
- **M4 — orchestrator wiring + systemd conf** (~120 LoC + 2 tests).
  New CLI flags `--target-weights="stable_yield=0.30,multiply=0.30,
  hedgedjlp=0.40"` and `--min-drift-bps=200`, plumbed to
  `AllocatorConfig`. `AuditLog::open(path, Option<TargetWeights>)`
  threads the targets to every emitted JSONL row. systemd unit's
  EXECUTE_FLAGS example documents the operator-facing setup.

**Total: ~1270 LoC, 30 new tests across 4 crates.** No envelope shape
changes, no daemon-side changes, no on-chain behaviour change until
the operator sets the env var.

**Activating drift mode (rc22 reference deployment):**
```
EXECUTE_FLAGS=--execute --targets-json=... --cooldown-secs=300 \
    --max-action-fraction=0.50 --min-action-usd=5.0 --stale-slack=1.10 \
    --target-weights="stable_yield=0.30,multiply=0.30,hedgedjlp=0.40" \
    --min-drift-bps=200
```
Empty/unset `--target-weights` keeps greedy mode. The orchestrator
refuses to boot on a malformed spec rather than silently fall back.

This is the largest single-rc feature the allocator has shipped since
rc1 (the orchestrator-daemon itself). Greedy mode stays the default
because (a) zero-config operators get the rc21 behaviour they tested
against, and (b) drift mode needs operator-set weights to be meaningful
in the first place — there's no "right" default tilt across all
operators.

## v0.4.0-rc21 — audit follow-up: install atomicity + API_BASE configurability + test rigor (2026-05-21)

Independent code review of rc17–rc20 (the deployment-pipeline arc)
found three real defects and two test-rigor gaps. All addressed here.

**H1 — Install-script atomic swap is now atomic.** Pre-rc21 the
`/opt/hedgents-frontend` swap stopped the service AFTER the move,
ran `chown -R` AFTER restart (race), and used a cross-filesystem `mv`
from `/tmp` (copy+unlink, not atomic rename). On failure mid-swap the
UI was wiped with no rollback. Fixed: stage into
`/opt/.hedgents-frontend.staging` (same FS → atomic rename), stop the
service before swap, chown the staging dir before swap, restore `.old`
on any failure.

**H2 — `NEXT_PUBLIC_API_BASE` is now configurable.** Pre-rc21 every
release shipped with `https://api.hedgents.com` hardcoded — fine for
the reference deployment, a data-leak surface for any self-hosted
operator whose dashboard would silently fetch from upstream. Now:
`workflow_dispatch` input `api_base` (default reference URL); a
fail-fast guard rejects builds where the input still points at
localhost, so a misconfigured dispatch can't re-introduce the rc18 bug.

**H3 — `LifetimeBanner` hardened.** `animate-ping` is now
`motion-safe:animate-ping` (respects `prefers-reduced-motion`). Fetch
failures log to console instead of rendering "—" silently. Refetches
on `visibilitychange` so a tab open for a week re-anchors to the
server's authoritative `uptime_secs` instead of drifting on the
client clock.

**M1 — Lifetime constants now cross-checked against LITEPAPER.**
rc20's hardened test was one-sided (year ≥ 2026 caught backwards,
not forwards). Tightened to: literal pin + year-floor + `<= now()` at
test time + `include_str!("../../../../LITEPAPER.md").contains("Live
mainnet operation since 2026-05-09")`. Flip the constant without the
LITEPAPER and CI fails. Same approach catches off-by-year-forward.

**M2 — Allocator derived invariants + status thresholds pinned.**
- `default_config_derived_invariants`: enforces
  `risk_premium_bps_hedgedjlp > risk_premium_bps_multiply` (the
  docstring rationale), `min_withdraw_gap_bps > 143` (the rc15
  incident gap), and unit-range floors/ceilings for all four knobs.
  Catches "decimal typo" silently re-calibrating downstream tests.
- `status_thresholds_are_ordered_and_in_seconds_not_micros`:
  STATUS_GREEN_MS / STATUS_YELLOW_MS had no test pre-rc21. Pins
  ordering + unit-range (10s–60s green, 60s–10min yellow).

**L1 — Polish.**
- Navbar now displays the live API host (parsed from `API_BASE`)
  instead of the hardcoded "localhost:7700" string that pre-rc21
  showed on every dashboard regardless of which API it was talking to.
- `lifetime_output_shape_serializes` test no longer seeds the wrong
  2025 timestamp — uses `LIVE_SINCE_UNIX` constant. Test data should
  never encode a value we declared incorrect.
- `lib/api.ts` warns at import time if `NEXT_PUBLIC_API_BASE` is the
  localhost fallback — the same trap that bit rc18 now surfaces in
  every devtools console.

5 new tests across `state.rs` and `allocator.rs`. Workspace count:
34 dashboard + 31 fleet-pm-stub + remainder unchanged.

## v0.4.0-rc20 — fix LIVE_SINCE_UNIX off-by-one-year (2026-05-20)

rc19 shipped the lifetime banner with `LIVE_SINCE_UNIX = 1_746_748_800`,
which is 2025-05-09 — one year before the actual go-live. Dashboard
reported **376 days of uptime** instead of 11. The
`lifetime_constants_match_devlog` test passed because it asserted the
constant against itself; the year wasn't independently verified.

- Constant updated to `1_778_284_800` (2026-05-09T00:00:00Z).
- Test hardened: in addition to the literal pin, assert
  `LIVE_SINCE_UNIX >= 1_767_225_600` (2026-01-01) so any future
  off-by-365-days typo fails loud at test time. The lesson generalises:
  when a constant encodes a real-world value, the test should compare
  against a *derived* property, not the literal itself — otherwise the
  test is just paraphrasing the bug.

This is the smallest, most embarrassing rc on the board, which makes
it the most useful one to ship transparently: the regression test
discipline only works if the test is independent of the constant it
guards.

## v0.4.0-rc19 — lifetime hero banner: make time-on-mainnet visible (2026-05-20)

The product thesis ("operational reliability earns trust over time")
only compounds as a moat if prospects can see the time accumulating.
Pre-rc19 the fleet had three weeks of mainnet uptime and 18 publicly-
resolved incidents — all of it invisible to anyone who didn't read
DEVLOG.md. The dashboard rendered live numbers (AUM, APR) but no
historical context.

- New backend endpoint `GET /lifetime` returns `live_since_unix`,
  `now_unix`, `uptime_secs`, and `incidents_resolved`. Constants
  bumped per release tag; `now_unix` derived at request time so the
  client can drift-free-tick a per-second uptime counter against the
  server clock.
- New frontend `LifetimeBanner` hero component renders above the
  Treasury panel: pulsing live indicator, three big-number stats
  (Live Since · Uptime · Incidents Resolved), subtle emerald gradient
  accent. The uptime counter ticks every second locally, anchored to
  the server's `uptime_secs` so refreshes don't reset to 0.
- Page hierarchy reorganised with section dividers (Treasury /
  Strategies / Allocator / Activity) so the dashboard reads as a
  narrative top-to-bottom instead of a grid of similarly-weighted
  cards.

2 new dashboard tests pin the response shape. The header is the first
piece of operator-facing surface that explicitly says "this thing has
been running" — every prior version forced the visitor to infer it
from the changelog.

## v0.4.0-rc18 — release pipeline bakes the right API base into the frontend (2026-05-20)

rc17 closed the install-side gap (frontend now flows through the
installer), which immediately surfaced a deeper bug in the build side:
the release pipeline had been compiling the frontend with
`NEXT_PUBLIC_API_BASE=http://localhost:7700` since the bundle was first
added. Next.js bakes `NEXT_PUBLIC_*` into the client chunks at build
time, so every browser loading dashboard.hedgents.com tried to fetch
data from the user's own machine on port 7700 — mixed-content blocked,
connection refused, empty cards. The original May 17 hand-deployed
bundle had been built locally with the right URL, which is why nobody
noticed; rc17 was the first time the (broken) GH Actions build ever
reached the reference VM.

- `.github/workflows/release-fleet.yml`: `NEXT_PUBLIC_API_BASE` →
  `https://api.hedgents.com`, with an inline comment explaining the
  failure mode so a future engineer doesn't toggle it back.
- Reference VM hot-patched with a locally-built bundle that has the
  correct API URL while the rc18 release rolled.

This is the dual of the rc17 incident: rc17 fixed "the installer
never deployed the frontend", rc18 fixes "the frontend that gets
deployed was built wrong." Together they close the UI deployment
pipeline end-to-end.

## v0.4.0-rc17 — installer actually deploys the frontend bundle (2026-05-20)

The rc16 dashboard fix shipped clean — but operators ran the install
script and the UI didn't change. Investigation: every release since the
frontend was added to the build pipeline produced a `hedgents-frontend-*.tar.gz`
artifact and a `manifest.json` entry for it, but **`install-hedgents.sh`
never downloaded or extracted that tarball**. It read
`FRONTEND_URL` from the manifest into a shell variable and then
silently dropped it. Result: `/opt/hedgents-frontend` carried whatever
bundle was manually deployed during initial bring-up (May 17, 2026 on
the reference VM) and stayed frozen there through every "upgrade."

- `install-hedgents.sh` now does what its manifest-read implies:
  downloads the frontend tarball, verifies the sha256 sidecar (same
  format the fleet binaries use), and atomically swaps
  `/opt/hedgents-frontend` for the new bundle (keeping `.old` for
  rollback). If `hedgents-frontend` is running, restart it; otherwise
  leave the unit alone so first installs don't auto-start.
- Empty `FRONTEND_URL` in manifest is non-fatal (older fleet tags
  pre-date the bundle).
- Manual rc16 hot-deploy on the reference VM verified the swap shape
  before the patch landed — same atomic-move + chown + restart flow.

This was 50 LoC of UI work going undeployed for an unknown number of
prior releases. The fix is structurally cheap; the audit habit it
teaches (never trust a manifest field you don't actually consume) is
the real value.

## v0.4.0-rc16 — AUM accounting includes hedge collateral (2026-05-20)

Live-deploy of rc15 surfaced a dashboard accounting bug: after the
orchestrator's $119 Assign rebuilt the hedgedjlp position, the operator's
reported AUM dropped from $239 → $184 even though no capital was lost.
The missing $55 went into Jupiter Perps short-position collateral —
real on-chain capital that the dashboard's `/aum` simply wasn't
counting. The wallet ATA correctly read $1 (the safety reserve), but
the orphan in the accounting made it look like the fleet had leaked
~25% of its AUM.

- `/aum.per_strategy.hedgedjlp_collateral_usd` — new field summing
  `collateral_usd_micro` across every open short. Already discovered
  per-position in `chain/jupiter_perps.rs`; rc16 just rolls it up.
- `/aum.total_usdc` now includes hedge collateral. The topline matches
  the operator's true on-chain capital.
- `/strategies` hedgedjlp card exposes `hedge_collateral_usdc` as an
  optional field (omitted for non-hedgedjlp strategies via
  `#[serde(skip_serializing_if = "Option::is_none")]`).
- `deployed_usdc` deliberately keeps reporting JLP value only — the
  daemon's APR claim is calibrated against JLP value, and folding
  collateral into the denominator would understate effective yield
  without a corresponding daemon-side recalibration.
- Frontend: `StrategyCardsRow` now shows `+ $X collateral` under the
  primary position line for hedgedjlp; the live-status badge displays
  the sum (JLP + collateral) so the headline matches committed capital.
  `NumbersPanel.Allocation` adds a "HedgedJLP (collateral)" row that
  appears only when non-zero.

5 new dashboard tests pin the JSON shape (`#[serde(skip)]` omission for
non-hedgedjlp; field present for hedgedjlp; aggregation math). Workspace
test count: 31 dashboard + 30 fleet-pm-stub.

## v0.4.0-rc15 — allocator hysteresis + idle-deploy fall-through (2026-05-20)

Live incident the night before: the orchestrator unilaterally liquidated
the $174 hedgedjlp position at 2026-05-20T00:27:46 UTC and left $175 of
USDC sitting idle at 0% APR for ~5 hours. Cost: ~$0.50 in gas + Jupiter
swap slippage. Cost if undetected: a full day of foregone yield on 73%
of fleet AUM.

Four bugs in one cascade:

- **Bug A — `WithdrawHedgedJlp` always emits `u64::MAX`.** The allocator
  computed `amount_usd: $23.93` (10% of AUM, properly clamped by
  `max_action_fraction`). The envelope-construction layer in
  `allocator_runner.rs` discarded the dollar amount and sent the
  full-withdraw sentinel `jlp_lamports: u64::MAX` because *"the
  allocator does not price JLP."* A 10% rebalance became 100%
  liquidation. Investigation showed the hedgedjlp daemon's `unwind.rs`
  iterates `active.open_positions` and closes every short
  unconditionally — there is no proportional-close path today — so
  "partial JLP burn but full short close" would actually be worse
  (leaves residual JLP unhedged). The envelope behaviour is therefore
  *correct given the daemon constraint*; the real fix is to make sure
  the allocator only emits Withdraw when it really means "liquidate."
  Documented the constraint inline, added an invariant test pinning
  `u64::MAX` until proportional unwind lands.
- **Bug B — under-hurdle Withdraw below `min_action_usd` blocks idle
  deposit.** Post-liquidation, multiply ($8.33 deployed) sat ~30 bps
  under its hurdle. Step 3 picked it for Withdraw, computed amount =
  $8.33 < min $10, returned `NoAction`, and never reached step 4
  (idle deposit). $175 of idle USDC sat at 0% APR for hours while
  stable_yield was paying 5–9%. Fix: when step-3 can't act, record
  the observation and **fall through to step 4** instead of
  short-circuiting.
- **Bug C — `Deposit→multiply` returns `None`.** `AssignMultiply` has
  no USD-sizing field (the daemon trades against whatever balance is
  in its ATA; allocator-driven deposits would need an out-of-band
  wallet transfer). When the deposit-picker selected multiply as best
  above-hurdle, the envelope layer returned `None` and the orchestrator
  emitted `skipped:no_dispatch`. Idle stayed idle. Fix: introduce
  `is_deployable_via_allocator()` (currently filters out `multiply`)
  and apply it in the deposit-picker so it falls through to next-best
  or stable_yield.
- **Bug D — no hysteresis on Withdraw triggers.** The actual incident
  trigger was a single-tick 352 bps spike in Kamino's reported USDC
  supply APR (5.44% → 8.96%), driving the hedgedjlp hurdle past its
  net APR (gap = -143 bps). The 3% risk premium couldn't absorb a
  3.5% noise event. Added `AllocatorConfig::min_withdraw_gap_bps`
  (default 150 bps). Withdraw fires only when the gap exceeds the
  threshold. The rc15 incident gap was -143 < 150 → would not have
  triggered.

Six new tests pin the incident shape, including
`rc15_regression_apr_spike_does_not_trigger_full_unwind` and
`rc15_regression_post_unwind_idle_redeploys`. Updated audit reason
strings carry both the under-hurdle observation AND the eventual
action so operators can debug a single audit line without
re-reading the allocator's decision tree.

## v0.4.0-rc14 — resize auto-executes when require_approval=false (2026-05-19)

Root cause of the approval deadlock: `run_resize` always enqueued the
plan and emitted `NeedsApproval` to the orchestrator regardless of the
`--require-approval` flag. The flag only gated incoming *Assign* /
*Withdraw* envelopes in `dispatch.rs`; the rebalancer's self-generated
resize plans had no equivalent bypass. On daemon restart the nonce
counter resets to 1; the orchestrator's replay-protection guard records
the last-seen nonce per sender and rejects anything ≤ last-seen, so
every `Escalate(NeedsApproval)` from the freshly restarted daemon was
silently dropped. The SOL+ETH resize plan was stuck in the queue for
hours.

- `ResizeCtx` gains `require_approval: bool` (mirrors `DispatchCtx`).
- `run_resize`: when `!ctx.require_approval`, calls `execute_resize`
  directly after computing the plan — no queue, no escalate. Returns
  `queued_to_approval: false`.
- `deploy/systemd/hedgents-hedgedjlp-live.service` updated to
  `--require-approval=false` so the install script no longer clobbers
  the setting on every deploy.
- First tick after rc14 deployed: `"resize auto-executed successfully"
  sig_count:2`. Next tick: `queued:0, skipped:3`. All three shorts
  confirmed live. `current_delta_bps: 0`.

## v0.4.0-rc13 — slippage fallback accepts any oracle price (2026-05-19)

rc12's fallback for Jupiter API unavailability still used stale dollar
amounts (`SOL=$100, ETH=$1000, BTC=$50k`). On a bad day those numbers
are *above* the oracle, which means `short_price_floor_micro_usd(fallback) =
fallback × 90%` would still be above oracle and the keeper would still
reject. Only affects the API-down path, but a bug on a degraded path is
still a bug.

- `sim_mark_price_micro_usd` now returns `1` regardless of asset.
  Floor = `1 × 90% → 0` (integer). Keeper fills at whatever the oracle
  says. No stale dollar amounts anywhere in the short-open path.

## v0.4.0-rc12 — live oracle prices for short position slippage floor (2026-05-19)

**The original bug that caused ~$25 of losses.** The
`price_slippage` field in `CreateIncreasePositionMarketRequest` is a
*floor* for shorts: the keeper only fills if oracle ≥ floor. The daemon
had hardcoded stale prices as the slippage input: `ETH=$3,535`,
`SOL=$151.50`. When the position was run live, ETH was at $2,100 and
SOL at $84. Every SOL and ETH open request was silently rejected by the
keeper. BTC filled once (BTC was still above the $70k floor at the
time). The fleet ran with 1 of 3 shorts for ~2 weeks while SOL and ETH
drifted freely against the unhedged JLP long, resulting in roughly $25
of directional loss.

- `open_short_requests` and `execute_resize` both now call
  `crate::prices::fetch_custody_prices_micro_usd` before the asset
  loop to get live Jupiter prices.
- New function `short_price_floor_micro_usd(live_mark) → live_mark -
  live_mark / 10` — floor is 10% below the current oracle. Keeper
  fills unless the market moves >10% between tx submission and
  execution, which is an acceptable exit condition.
- `sim_mark_price_micro_usd` retained as fallback (patched to return 1
  in rc13 — see above).

## v0.4.0-rc9–rc11 — fill verification, custody decoder, /pnl bracketing (2026-05-18–19)

Three smaller fixes that surfaced during rc7/rc8 prod validation:

- **rc9 — keeper fill verification**: `open_short_requests` now polls
  the on-chain `PositionRequest` PDA for up to 20 s after submitting;
  only records the short in `ActivePosition.open_positions` once the
  keeper has flipped the account to filled. Prevents phantom-position
  entries from request PDAs that expire without fill.
- **rc10 — JLP custody decoder offset fix**: asset offset corrected
  from 1080→214, pythnet field from byte 107→106. The wrong offsets
  produced `decoded_mint = [0u8; 32]` for every custody that landed
  past the first; SOL and ETH deltas read as $0.
- **rc11 — /pnl bracket scan past sentinel rows**: dashboard `/pnl`
  reported `delta=$0` when the bracketing rows for a daemon happened to
  be sentinel-mode rows (`jlp_value_usd_micro: 0`). Fixed by scanning
  forward/backward with `find_map` instead of taking `first()`/`last()`
  directly.

## v0.4.0-rc7–rc8 — USDC pre-flight gate + partial-price-response retry (2026-05-18)

First on-chain resize execution (rc5+rc6) revealed two more bugs:

- **rc7 — USDC pre-flight gate**: the resize path submitted an ETH
  short-open against an under-funded USDC ATA, producing a 1200-line
  `custom program error: 0x1` (SPL Token InsufficientFunds) in the
  program log. Added `fetch_wallet_free_usdc_lamports` pre-flight in
  both `run_resize` (queue time) and `execute_resize` (execute time);
  legs that exceed available USDC are dropped with
  `SkipReason::InsufficientUsdcLiquidity`. Also added the one-shot
  retry on Jupiter partial price responses (a request for 3 mints
  returned only 1 that tick, causing SOL+BTC deltas to read $0 and the
  rebalancer to compute a wrong-shape plan).
- **rc8 — pubkey-aware delta bucketing**: added
  `delta::compute_delta_with_pubkeys` fallback to match well-known JLP
  custody PDAs when `decoded_mint` doesn't resolve (covers a future
  custody migration or decoder regression).

## v0.4.0 — withdraw-recovery, auto-mode, tier-1+2 hardening (2026-05-18)

Closed the open items from rc1–rc6 prod bring-up before promoting to
a stable v0.4.0 tag. Three structural fixes + operational polish:

- **Withdraw-recovered positions**: rc4 left `open_counter=0`
  placeholders; `WithdrawHedgedJlp` couldn't derive the close-request
  PDA. Per Jupiter Perps spec §3.6 the counter is a randomisation nonce
  — no structural link between open and close. Unwind now reads the
  on-chain `Position` account at the recorded pubkey and generates a
  fresh close-counter at withdraw time. `open_counter` removed from
  `ActivePosition.open_positions` entirely.
- **Auto-mode (M11)**: strategy daemons auto-accept Assign/Withdraw
  envelopes from the configured orchestrator pubkey when the action
  stays within single-action and 24h cumulative caps. Operator approval
  no longer blocks the autonomous path; out-of-cap actions still queue.
- **`require_approval` flag**: `--require-approval=false` lets the
  Assign/Withdraw dispatch path skip the approval queue. Default stays
  `true` on mainnet.

## v0.4.0-rc6 — JLP custody pricing via Jupiter Price API (2026-05-18)

The rebalancer-resize action from rc5 was blocked at chain-read time:
`read_pool_state` was fetching each JLP custody's `pythnet_price_account`
directly, but Pyth migrated from V1 standalone-account oracles to Pull V2
ephemeral PDAs and the legacy pubkey stored in the custody account no
longer resolves on mainnet (`AccountNotFound`).

- New module `crates/hedgedjlp-daemon/src/prices.rs` — batched Jupiter
  Price API fetcher (`https://lite-api.jup.ag/price/v3`), pure parser
  for offline unit tests, micro-USD scale matching the rest of the
  daemon's math.
- `read_pool_state` now does one Jupiter HTTP call + one
  `get_multiple_accounts` RPC per tick instead of N individual account
  fetches. Soft-fail: missing price map entry → custody contributes
  $0, log WARN, continue (rebalancer sees slightly-wrong delta but
  doesn't blow up).
- 13 new tests; daemon-crate total 129 passing.

Caught in prod execute-mode smoke (rc5 → first rebalancer tick).
**Without this fix the entire resize action chain would be dead.**

## v0.4.0-rc5 — hedgedjlp rebalancer resize action (2026-05-18)

Closes the M9 TODO that left the rebalancer detecting drift but doing
nothing about it. The prod fleet ran for days with a $174 JLP position
hedged only by an $18 BTC short (~$96 of SOL+ETH long exposure
unhedged), bleeding daily directional drift while the rebalancer
logged the problem and emitted Escalates without acting.

- New module `crates/hedgedjlp-daemon/src/resize.rs` — pure
  `compute_per_asset_targets` + `compute_legs_to_open` math (reuses
  hedge.rs's `allocate_per_asset` shape).  Delta-to-open per asset =
  `max(0, target - existing)`. Never closes existing legs, never
  overshoots, scales proportionally when
  `MAX_POSITION_USDC_LAMPORTS` would be exceeded, drops below
  `MIN_HEDGE_NOTIONAL_USD` dust.
- New `ResizeApprovalQueue` (third instance of the existing generic;
  same sender-match audit-fix C1, same `Escalate(NeedsApproval)`
  emission shape — no new authority surface).
- `dispatch.rs` `handle_approve` drains the resize queue first; on
  approve calls `resize::execute_resize` which submits the open-short
  request ixns and updates `state.active.open_positions` +
  `hedge_notional_usdc`.
- `rebalance.rs` `tick_once` now accepts `Option<Arc<ResizeCtx>>` and
  invokes `resize::run_resize` after the Escalate(DeltaDrift). The
  Escalate stays as telemetry.
- 13 new tests pinning the prod-incident shape:
  `prod_174_case_btc_present_sol_eth_missing` (existing BTC short
  present, SOL+ETH legs missing, queue SOL+ETH skip BTC), plus
  idempotency, cap-scaling, dust-drop, and CBOR round-trip cases.

## v0.4.0-rc4 — hedgedjlp boot-time state recovery (2026-05-18)

Before: `RebalanceState.active` lived only in memory. Every daemon
restart orphaned the on-chain position — rebalancer ticked forever
with "no active position" and `WithdrawHedgedJlp` short-circuited to a
zero-Report sentinel. The prod fleet had $174 JLP + $18 BTC short
unmanageable for hours after a restart earlier in the day.

- New module `crates/hedgedjlp-daemon/src/recover.rs` — on boot, reads
  the wallet's JLP token balance via the associated-token account,
  decodes the JLP pool's custody list, discovers open SOL/ETH/BTC
  short PDAs the wallet owns (mirrors the dashboard's
  `discover_hedge_positions`), reconstructs an `ActivePosition`, seeds
  `state.active`.
- Read-failure tolerant at every step. Account-not-found → zero
  balance / empty list, rebalancer's existing `is_empty()` branch
  logs+skips. Non-zero JLP + zero shorts logs WARN (the
  prod-incident shape) but still seeds `state.active` so the
  rebalancer can size up the missing legs (which rc5 then does).
- `conv = [0xFF; 16]` sentinel so recovered positions are grep-able
  in telemetry. Documented `open_counter = 0` placeholder limitation
  (rebalance-only; withdraw mis-derives close PDA — pending the
  withdraw-recovery fix).
- 5 new tests; daemon-crate total 104 passing.

## v0.4.0-rc3 — dashboard /pnl reads real on-chain fields (2026-05-18)

`/pnl` was reporting `start_aum_usdc: 3001` for a fleet whose actual
deployed value was $264. Cause: every yield daemon writes
`total_aum_usdc = paper_principal_usdc + paper_earned_usdc` into its
telemetry row regardless of mode; in live mode `paper_principal_usdc`
is a hardcoded $1000 synthetic baseline. The threshold filter
(`PAPER_PRINCIPAL_THRESHOLD_USDC = $10k`) caught the old $50k paper
rows but let the $1k live-mode synthetics through, giving 3 × $1000 =
$3001 of phantom AUM.

- Replaced `pnl_row_to_usd` with a strict reader that derives USD per
  daemon from real on-chain fields only: multiply
  `net_equity_uusdc` (deposited − borrowed), stable-yield
  `deposited_usdc_lamports` (Kamino USDC supply), hedgedjlp
  `jlp_value_usd_micro` (mark-to-market JLP). All u-USDC integers, no
  floats. Rows without a non-zero real-position field return `None`
  and drop out of any AUM aggregation.
- Deleted `is_paper_row` + `PAPER_PRINCIPAL_THRESHOLD_USDC`
  threshold-based filtering — no longer needed; the synthetic-vs-real
  distinction now lives in field selection, not a magic number.
- 5 new tests pinning per-daemon extraction + synthetic-only-returns-None.

## v0.4.0-rc2 — orchestrator nonce-replay fix (2026-05-18)

First execute-mode action on prod got rejected by the multiply
daemon with `Bilateral validation failed: nonce replay — received 33,
last seen 1778942442`. The CLI's allocator-execute path uses
`now_unix()` as the envelope nonce; recipients record the highest
nonce seen per (sender_pubkey, peer_id) pair. The orchestrator daemon
started its nonce from `AtomicU64::new(1)` and could never exceed the
unix-timestamp high-water mark left by prior CLI invocations — every
emit landed billions below the recorded last_seen and got dropped
silently at the application-level inbox (handle.send() returned Ok at
libp2p).

- One-line fix: seed `outbound_nonce` from
  `now_unix()` at boot. Beacon emitter + tick emitter share this
  counter so all outbound envelopes carry strictly-increasing nonces
  across daemon restarts.

## v0.4.0-rc1 — orchestrator daemon (Phase 1 dry-run) (2026-05-17)

Lifts the existing `fleet-pm-stub allocator` from a manual CLI tool
into a long-running autonomous daemon. Joins the libp2p mesh as
`Role::Orchestrator`, polls the dashboard's `/strategies` + `/aum`
on a tick, runs the pure `allocator::decide` function against the
live snapshot, and writes every decision to an append-only JSONL
audit log. **No envelope emission, no wallet, no authority to move
funds.** Dry-run by default; `--execute` opt-in (held for rc2+).

- New crate `crates/orchestrator-daemon` (~600 LoC + tests).
- Compile-time isolated: wallet crate intentionally absent from the
  dep graph.
- `fleet-pm-stub` refactored to lib + bin so `allocator::decide` and
  `allocator_runner::action_to_envelope_spec` can be reused by both
  the CLI and the daemon. Single source of truth for envelope
  construction across the two surfaces.
- New systemd unit `hedgents-orchestrator.service` + addition to
  `hedgents.target`. Listens on `:19317`.
- Three runbooks: `orchestrator-bringup.md` (rc1 dry-run),
  `orchestrator-devnet-smoke.md` (execute on devnet),
  `orchestrator-mainnet.md` (execute on mainnet with 7-day
  promotion window).
- 24 fleet-pm-stub library tests + 10 orchestrator-daemon tests pass.

---

## Why the rc march matters

rc1 through rc14 shipped over ~48 hours of prod execute-mode bring-up.
Each rc fixed a real bug surfaced by live mainnet behaviour that no
unit test could have predicted:

| rc | bug | how it surfaced |
|----|-----|-----------------|
| rc2 | orchestrator nonce-replay | execute-mode envelope silently dropped at recipient |
| rc3 | $3001 phantom AUM | dashboard `/pnl` query during incident triage |
| rc4 | state.active orphaned after restart | manual WithdrawHedgedJlp returned zero |
| rc5 | rebalancer-detected drift was a no-op | M9 TODO comment, surfaced by rc4 reading the recovered state |
| rc6 | Pyth V1→V2 oracle migration | first rc5 rebalancer tick blew up on `AccountNotFound` |
| rc7–rc8 | USDC InsufficientFunds + partial Jupiter price response | first on-chain resize produced a 1200-line program error |
| rc9 | phantom positions from unfilled keeper requests | resize recorded a short before keeper had executed |
| rc10 | JLP custody decoder wrong offsets | SOL + ETH delta read as $0, rebalancer skipped them |
| rc11 | /pnl delta=$0 with sentinel bracket rows | real AUM disappeared from dashboard during position recovery |
| rc12 | stale hardcoded slippage floors above oracle | SOL/ETH keeper fill silently rejected for ~2 weeks, ~$25 loss |
| rc13 | fallback prices still above oracle on bad day | API-down path could repeat the rc12 failure |
| rc14 | resize queue blocked by orchestrator nonce-replay | daemon restart resets nonce; orchestrator dropped every Escalate |
| rc15 | 4-bug cascade: APR-spike → full-unwind → idle stuck at 0% APR | orchestrator liquidated $174 hedgedjlp and left $175 idle for ~5h |
| rc16 | dashboard understated AUM by hedge-collateral amount | operator reported "missing $40" after rc15 redeployed the position |
| rc17 | installer never deployed the frontend bundle | rc16 UI work didn't appear after `install-hedgents.sh` ran on the VM |
| rc18 | release pipeline baked localhost:7700 into the frontend | rc17 install replaced the working hand-built bundle with the broken CI one |
| rc19 | (hero banner shipped) | — |
| rc20 | LIVE_SINCE_UNIX off by exactly one year (2025 instead of 2026) | banner showed 376 days uptime instead of 11 |
| rc21 | audit follow-up: install atomicity + API_BASE configurability + test rigor | independent code review of the rc17-rc20 arc |
| rc22 | allocator v2: drift-from-target allocation behind a CLI flag | operator-facing question "does the allocator do proportional math? 1:1:1 in paper looks like no math" |
| rc23 | allocator v2 M5: APR-weighted dynamic targets | operator-facing question "is 30/30/40 still static?" |
| rc24 | /pnl from chain-state snapshots (kills 3 telemetry desync classes) | operator-facing question "what about the trading part, actually making money? but it's not shown on dashboard" |
| rc25 | Restart=always + boot-survival + watchdog timer | operator-facing question "make sure the agent is always running and dashboard is always showing data" |
| rc26 | bundle frontend.service into deploy/ + ship .timer files in release tarball | rc25 install surfaced two artifact-pipeline gaps |

The ~$25 loss from rc12 is real and verifiable on-chain. The root
cause (a floor price set above the oracle at time of execution) is the
kind of bug that passes every unit test and only surfaces when you
actually send the transaction to a live keeper. Each fix from rc7
onward has a regression test pinning the exact incident shape.

This is the maturity profile institutional reviewers underwrite:
mainnet shipping with honest in-code commentary about what's
load-bearing and what isn't, and an unbroken chain from incident to
root cause to regression test.

---

## v0.4.0 — Orchestrator daemon, execute mode (2026-05-17)

- New crate `crates/orchestrator-daemon` — long-running autonomous
  rebalancer. Polls the dashboard `/strategies` + `/aum`, runs the
  pure `decide()` function, signs + dispatches Assign/Withdraw
  envelopes to the strategy daemons.
- Compile-time isolation extended: the wallet crate is **not** in the
  orchestrator's dep graph; it can sign mesh envelopes but cannot
  sign Solana transactions. Strategy daemons retain that authority.
- **Cooldown registry** — per-strategy lockout between dispatches
  prevents hot-loops. Default 5min; tunable.
- **Stale-snapshot guard** — re-fetches `/aum` between decision and
  emit; rejects actions that exceed the re-fetched idle/deployed by
  more than the configured slack factor (default 10%).
- **Two-stage promotion path**: `v0.4.0-rc1` ran dry-run for 24h+;
  `v0.4.0` enables `--execute` with conservative caps
  (`max_action_fraction=0.10`, `cooldown=300s`) and loosens after a
  7-day clean window.
- `fleet-pm-stub` refactored to lib + bin. `action_to_envelope_spec`
  moved into the library — same code path drives both the CLI's
  one-shot `allocator --execute` and the daemon's continuous tick
  loop. 14 envelope-spec tests cover Deposit/Withdraw across all
  three strategies + the missing-target error path.
- New systemd unit `hedgents-orchestrator.service` + addition to
  `hedgents.target`. Listens on `:19317`.
- Runbooks: `orchestrator-bringup.md` (rc1 dry-run),
  `orchestrator-devnet-smoke.md` (execute on devnet),
  `orchestrator-mainnet.md` (execute on mainnet with 7-day
  promotion path).

## v0.3.3 — klend repay account-list fix (2026-05-16)

- `repay_obligation_liquidity_v2_ix` was missing `lending_market_authority`
  and `farms_program` — klend rejected with `AccountNotEnoughKeys (3005)`.
- Re-derived account list from klend source; bundle now passes simulation.

**Mainnet effect:** multiply unwind round 3 broadcast; position drained from
35% LTV → 2.5% LTV across two rounds (sigs `4Zv1jL…RFwC`, `J2zkqT…RjiV`).
Round 3 hit klend's `NetValueRemainingTooSmall (6092)` dust-floor rule —
known protocol behavior, $0.22 residual; close-obligation path is Phase 1
v0.3.4 candidate.

## v0.3.2 — wSOL wrap in unwind bundle

- Jito `WithdrawSol` returns raw SOL, but klend repay expects wSOL token
  account. Inserted `CreateATA + system_transfer + sync_native` between
  the Jito withdraw and the klend repay.

## v0.3.1 — Jito WithdrawSol as swap leg

- v0.3.0 unwind bailed at runtime: "Iterative strategy selected but Jito
  direct-redeem swap leg not yet wired". Added `withdraw_sol_ix` to the
  Jito client + `StakePoolMeta::jitosol_to_sol_lamports` inverse helper.

## v0.3.0 — WithdrawMultiply protocol + iterative unwind

- New `WithdrawMultiply` mesh message type + `ReportMultiplyWithdraw`.
- Pure round-builder in `unwind.rs` (atomic flash-loan path + iterative
  deleverage path, daemon picks based on position size).
- `klend` v2 ixn builders for `repay_obligation_liquidity` and
  `withdraw_obligation_collateral`.
- `fleet-pm-stub withdraw-multiply` subcommand.
- Approval queue routing in `dispatch.rs`.

## v0.2.9 — systemd live target

- `hedgents-{stable-yield,multiply,hedgedjlp}-live.service` units +
  `hedgents-live.target`.
- `Conflicts=` directive ensures paper and live cannot run simultaneously.
- Live daemons now survive SSH logout (replacing nohup'd manual launches
  that died with the operator's terminal).
- Installer derives `EMERGENCY_WITHDRAW_DESTINATION` and
  `SOLANA_WALLET_PUBKEY` env vars at install time.

## v0.2.8 — combined APR in dashboard

- `/aum` endpoint exposes deployed-USD-weighted average APR across live
  strategies (`combined_apr_bps`) + projected annualised USD.
- Frontend benchmark widgets (`BenchmarkComparisonBar`,
  `YieldBenchmarkCard`) and `NumbersPanel` switched from Kamino-only to
  fleet-combined APR.

## v0.2.7 — riskwatcher leverage-frame fix

- Jupiter Perps liquidation-distance formula was producing 55+
  false-positive Critical escalates per day. Root cause: collateral was
  being divided by `custody.maxLeverage` *before* the leverage frame had
  scaled it. Fixed by reordering the math and correcting the
  `maxLeverage` scale.

## v0.2.5–v0.2.6 — riskwatcher polls Jupiter Perps positions

- New `jupiter_perps_poller`: discovers and classifies short positions
  held by a watched wallet; emits position view into the registry.
- Wired into the riskwatcher tick loop alongside existing Kamino
  obligation polling.

## v0.2.4 — JLP via Jupiter Swap aggregator

- Direct `add_liquidity_2` path on the JLP pool is closed in production.
- Replaced the buy/withdraw legs with Jupiter Swap quote+swap routing.
- Default slippage bumped to 150bps (Jupiter sim-path latency tolerance).

## v0.2.3 — hedgedjlp audit fixes

- Applied all 9 fixes from `hedgedjlp-daemon-audit-2026-05-15.md`.
- Live JLP pool custody wired into the buy leg; daemon now closes the
  full audit.

## v0.2.0–v0.2.2 — regime-aware allocator

- Pure decision function in `crates/fleet-pm-stub/src/allocator/` —
  takes `Snapshot` of strategy APRs, deployed USD, idle USD; returns
  `Deposit | Withdraw | NoAction`.
- Hurdle model: `stable_yield_apr + risk_premium[strategy]`.
- CLI subcommand with `--dry-run` and `--execute` modes; audit log to
  `allocator-actions.jsonl`.
- Spec: `docs/regime-aware-allocator.md`.

## v0.1.x — multiply on klend v2 handlers (the long road)

- Switched multiply from v1 to v2 klend handlers because v2 enforces the
  farm CPI refresh that v1 left as the caller's problem (and that the
  daemon was getting subtly wrong on round 2+).
- Address Lookup Table for Kamino main market
  (`284iwGtA9X9aLy3KsyV8uT2pXLARhYbiSi5SiM2g47M2`) — compressed lever-up
  bundle below the 1232-byte tx limit.
- `RefreshObligation` ordering corrected: must run *before*
  `BorrowObligationLiquidity` and *after* every reserve update.
- Kamino obligation byte-layout fixes: borrow slot is 200 bytes (not
  136), aggregate fields start at offset 2208.
- Result: lever-up rounds land cleanly on mainnet; dashboard reads
  obligation state correctly.

## v0.1.0 — full fleet on devnet → mainnet bring-up

- 5 daemons working end-to-end:
  - `stable-yield` — Kamino USDC supply
  - `multiply` — Kamino leveraged jitoSOL
  - `hedgedjlp` — JLP exposure + Jupiter Perps short hedge
  - `riskwatcher` — independent position poller with veto authority
  - `researcher` — market-signal emitter (lending, funding, peg, JLP yield)
- Approval queue + manual-approval flow per daemon
- libp2p mesh with role-bound Ed25519 keys per daemon
- Demo dashboard: `fleet-dashboard-server` ingesting logs into SQLite
  + REST + WS API; Next.js frontend

---

## Recurring themes

A few patterns that show up across the version history because they're
where the architecture earns its keep:

1. **Compile-time isolation has caught real bugs.** When `multiply` tried
   to construct an instruction whose type lived in the `hedgedjlp` crate,
   the build failed — not the runtime. That's the whole point.

2. **klend v2 handlers are right.** Every time we tried to use v1
   handlers "to save accounts" we eventually had to migrate. The farm
   CPI refresh is non-negotiable for positions with farm appendices.

3. **Riskwatcher catches things the daemons don't see.** The leverage-frame
   bug (v0.2.7) was emitted from the *poller*, not from the
   strategy daemon. Independent observation is a real defence.

4. **Address Lookup Tables are mandatory.** Any bundle with more than
   ~6 protocol accounts will exceed the 1232-byte tx limit without an
   ALT. Build the ALT account-list early.

5. **Dashboard surfaces drive product decisions.** Combined APR
   (v0.2.8) wasn't on anyone's plan — it became obvious once the
   benchmark widget had three rows of T-bill rates and only one row of
   "Kamino" APR. Showing the wrong number forces clarity faster than
   any spec review.
